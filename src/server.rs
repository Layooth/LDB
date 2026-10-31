//! LDBP/1 TCP 服务端。
//!
//! 每个连接一个线程，共享同一个 `Engine`（RwLock 读写分离）。

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::codec::Value;
use crate::engine::Engine;
use crate::error::{self, DbError, Result};
use crate::json::Json;
use crate::protocol::{encode_frame, FrameDecoder, PROTOCOL_VERSION};
use crate::sql::executor::ConnSession;

const SERVER_NAME: &str = "LittleDB";

#[derive(Clone)]
pub struct ServerOptions {
    pub host: String,
    pub port: u16,
    pub http_port: Option<u16>,
    pub token: Option<String>,
    pub max_connections: usize,
    pub slow_ms: f64,
    pub verbose: bool,
    pub web_dir: std::path::PathBuf,
    pub open_browser: bool,
    /// true = 每次提交 fsync（full，默认）；false = normal 模式
    pub durable_commits: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            host: "127.0.0.1".to_string(),
            port: 7801,
            http_port: Some(7802),
            token: None,
            max_connections: 64,
            slow_ms: 0.0,
            verbose: false,
            web_dir: crate::http::find_web_dir(),
            open_browser: true,
            durable_commits: true,
        }
    }
}

pub struct Server {
    engine: Arc<RwLock<Engine>>,
    options: ServerOptions,
    active: Arc<AtomicUsize>,
    pub connections: Arc<AtomicUsize>,
    pub queries: Arc<AtomicUsize>,
    pub slow_queries: Arc<AtomicUsize>,
    pub rejected: Arc<AtomicUsize>,
}

impl Server {
    pub fn new(mut engine: Engine, options: ServerOptions) -> Server {
        engine.store.set_sync_mode(options.durable_commits);
        Server {
            engine: Arc::new(RwLock::new(engine)),
            options,
            active: Arc::new(AtomicUsize::new(0)),
            connections: Arc::new(AtomicUsize::new(0)),
            queries: Arc::new(AtomicUsize::new(0)),
            slow_queries: Arc::new(AtomicUsize::new(0)),
            rejected: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 启动 HTTP 服务（后台线程）并阻塞运行 TCP 协议服务
    pub fn run(&self) -> Result<()> {
        if let Some(http_port) = self.options.http_port {
            let state = crate::http::HttpState::new(
                Arc::clone(&self.engine),
                self.options.token.clone(),
                self.options.web_dir.clone(),
                Arc::clone(&self.connections),
                self.options.max_connections,
                Arc::clone(&self.queries),
                Arc::clone(&self.slow_queries),
                Arc::clone(&self.rejected),
            );
            let host = self.options.host.clone();
            thread::spawn(move || {
                if let Err(err) = crate::http::serve(state, &host, http_port) {
                    eprintln!("[LittleDB] HTTP 服务启动失败：{}", err);
                }
            });

            if self.options.open_browser {
                // 监听 0.0.0.0 时浏览器要用回环地址访问
                let browser_host = if self.options.host == "0.0.0.0" || self.options.host == "::" {
                    "127.0.0.1".to_string()
                } else {
                    self.options.host.clone()
                };
                let url = format!("http://{}:{}", browser_host, http_port);
                thread::spawn(move || {
                    // 等 HTTP 线程真正 bind 完成
                    thread::sleep(std::time::Duration::from_millis(400));
                    open_browser(&url);
                });
            }
        }
        self.run_tcp()
    }

    fn run_tcp(&self) -> Result<()> {
        let address = format!("{}:{}", self.options.host, self.options.port);
        let listener = TcpListener::bind(&address)
            .map_err(|err| DbError::internal(format!("无法监听 {}：{}", address, err)))?;

        println!(
            "[LittleDB] 协议服务 (LDBP/{}) 已监听 tcp://{}",
            PROTOCOL_VERSION,
            listener
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or(address)
        );

        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    let mut stream = stream;
                    let engine = Arc::clone(&self.engine);
                    let options = self.options.clone();
                    let active = Arc::clone(&self.active);
                    let queries = Arc::clone(&self.queries);
                    let slow = Arc::clone(&self.slow_queries);
                    let rejected = Arc::clone(&self.rejected);
                    let connections = Arc::clone(&self.connections);

                    // 先 CAS 预约名额再 spawn（线程内自增会被并发 accept 超卖）
                    let mut current = active.load(Ordering::SeqCst);
                    let mut reserved = false;
                    while current < options.max_connections {
                        match active.compare_exchange(
                            current,
                            current + 1,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        ) {
                            Ok(_) => {
                                reserved = true;
                                break;
                            }
                            Err(actual) => current = actual,
                        }
                    }
                    if !reserved {
                        rejected.fetch_add(1, Ordering::SeqCst);
                        let payload = error_frame(
                            Json::Null,
                            error::LIMIT,
                            &format!(
                                "连接数已达上限（{}），请稍后重试",
                                options.max_connections
                            ),
                        );
                        send(&mut stream, &payload);
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }

                    // 栈按解析器闸门给：MAX_EXPR_DEPTH=64、MAX_TOKENS=2万，2MB 足够
                    let slot = Arc::clone(&active);
                    let spawn = thread::Builder::new()
                        .stack_size(2 * 1024 * 1024)
                        .spawn(move || {
                            handle_connection(
                                stream, engine, options, queries, slow, connections,
                            );
                            slot.fetch_sub(1, Ordering::SeqCst);
                        });
                    if spawn.is_err() {
                        active.fetch_sub(1, Ordering::SeqCst);
                        rejected.fetch_add(1, Ordering::SeqCst);
                        eprintln!("[LittleDB] 接受连接失败：线程创建失败");
                    }
                }
                Err(err) => {
                    eprintln!("[LittleDB] 接受连接失败：{}", err);
                }
            }
        }
        Ok(())
    }
}

/// 连接名额已在 accept 循环里用 CAS 预约，这里只负责会话本身。
#[allow(clippy::too_many_arguments)]
fn handle_connection(
    mut stream: TcpStream,
    engine: Arc<RwLock<Engine>>,
    options: ServerOptions,
    queries: Arc<AtomicUsize>,
    slow_queries: Arc<AtomicUsize>,
    connections: Arc<AtomicUsize>,
) {
    connections.fetch_add(1, Ordering::SeqCst);
    let _ = stream.set_nodelay(true);
    // 协议连接的读超时：客户端 5 分钟没有任何字节就断开。
    // LDBP 允许长连接，但完全不限时的 idle 连接会占死 max_connections 名额。
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));

    let mut session = ConnSession::new(engine);
    let mut authed = options.token.is_none();
    let mut decoder = FrameDecoder::new();

    send(&mut stream, &welcome_frame(authed));

    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let frames = match decoder.push(&buf[..n]) {
                    Ok(list) => list,
                    Err(err) => {
                        let payload = error_frame(Json::Null, err.code, &err.message);
                        send(&mut stream, &payload);
                        break;
                    }
                };

                let mut keep_going = true;
                for frame in frames {
                    if !handle_message(
                        &mut stream,
                        &mut session,
                        &mut authed,
                        &options,
                        &queries,
                        &slow_queries,
                        &frame,
                    ) {
                        keep_going = false;
                        break;
                    }
                }
                if !keep_going {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    session.close();
    let _ = stream.shutdown(Shutdown::Both);
}

fn welcome_frame(authenticated: bool) -> Json {
    Json::Obj(vec![
        ("id".to_string(), Json::Null),
        ("ok".to_string(), Json::Bool(true)),
        (
            "result".to_string(),
            Json::Obj(vec![
                ("event".to_string(), Json::str("welcome")),
                ("server".to_string(), Json::str(SERVER_NAME)),
                ("version".to_string(), Json::str(env!("CARGO_PKG_VERSION"))),
                ("protocol".to_string(), Json::Num(PROTOCOL_VERSION as f64)),
                ("authenticated".to_string(), Json::Bool(authenticated)),
            ]),
        ),
    ])
}

/// 返回 false 表示应当关闭连接
#[allow(clippy::too_many_arguments)]
fn handle_message(
    stream: &mut TcpStream,
    session: &mut ConnSession,
    authed: &mut bool,
    options: &ServerOptions,
    queries: &Arc<AtomicUsize>,
    slow_queries: &Arc<AtomicUsize>,
    message: &Json,
) -> bool {
    let id = message.get("id").cloned().unwrap_or(Json::Null);
    let kind = message
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if kind == "hello" {
        if let Some(expected) = &options.token {
            let given = message
                .get("token")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !crate::protocol::secret_eq(given, expected) {
                let payload = error_frame(id, error::AUTH, "认证失败：token 不正确");
                send(stream, &payload);
                return true;
            }
        }
        *authed = true;
        let payload = Json::Obj(vec![
            ("id".to_string(), id),
            ("ok".to_string(), Json::Bool(true)),
            (
                "result".to_string(),
                Json::Obj(vec![
                    ("event".to_string(), Json::str("ready")),
                    ("server".to_string(), Json::str(SERVER_NAME)),
                    ("version".to_string(), Json::str(env!("CARGO_PKG_VERSION"))),
                ]),
            ),
        ]);
        send(stream, &payload);
        return true;
    }

    if !*authed {
        let payload = error_frame(id, error::AUTH, "请先发送 hello 完成认证");
        send(stream, &payload);
        return true;
    }

    if kind == "ping" {
        let payload = Json::Obj(vec![
            ("id".to_string(), id),
            ("ok".to_string(), Json::Bool(true)),
            (
                "result".to_string(),
                Json::Obj(vec![
                    ("pong".to_string(), Json::Bool(true)),
                    ("time".to_string(), Json::Num(now_millis() as f64)),
                ]),
            ),
        ]);
        send(stream, &payload);
        return true;
    }

    if kind == "query" {
        let sql = message.get("sql").and_then(|v| v.as_str()).unwrap_or("");
        let params = match message.get("params") {
            Some(Json::Arr(items)) => items.iter().map(json_to_value).collect::<Vec<Value>>(),
            _ => Vec::new(),
        };

        let started = Instant::now();
        match session.execute(sql, &params) {
            Ok(result) => {
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                queries.fetch_add(1, Ordering::SeqCst);
                if options.slow_ms > 0.0 && elapsed >= options.slow_ms {
                    slow_queries.fetch_add(1, Ordering::SeqCst);
                    println!("[LittleDB] 慢查询 {:.1}ms: {}", elapsed, one_line(sql));
                } else if options.verbose {
                    println!("[LittleDB] {:.1}ms: {}", elapsed, one_line(sql));
                }

                let payload = Json::Obj(vec![
                    ("id".to_string(), id),
                    ("ok".to_string(), Json::Bool(true)),
                    ("result".to_string(), result.to_wire_json()),
                    ("elapsedMs".to_string(), Json::Num(elapsed)),
                ]);
                send(stream, &payload);
            }
            Err(err) => {
                let payload = error_frame(id, err.code, &err.message);
                send(stream, &payload);
            }
        }
        return true;
    }

    if kind == "close" {
        return false;
    }

    let payload = error_frame(
        id,
        error::PROTOCOL,
        &format!("未知的报文类型 \"{}\"", kind),
    );
    send(stream, &payload);
    true
}

fn json_to_value(json: &Json) -> Value {
    match json {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Int(n) => Value::Int(*n),
        Json::Num(n) => {
            if n.fract() == 0.0 && n.abs() < 9007199254740992.0 {
                Value::Int(*n as i64)
            } else {
                Value::Real(*n)
            }
        }
        Json::Str(s) => Value::Text(s.clone()),
        other => Value::Text(other.to_string()),
    }
}

fn error_frame(id: Json, code: &str, message: &str) -> Json {
    Json::Obj(vec![
        ("id".to_string(), id),
        ("ok".to_string(), Json::Bool(false)),
        (
            "error".to_string(),
            Json::Obj(vec![
                ("code".to_string(), Json::str(code)),
                ("message".to_string(), Json::str(message)),
            ]),
        ),
    ])
}

fn send(stream: &mut TcpStream, payload: &Json) {
    if let Ok(frame) = encode_frame(payload) {
        if stream.write_all(&frame).is_ok() {
            let _ = stream.flush();
        }
    }
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(_) => 0,
    }
}

/// 用系统默认浏览器打开控制台；失败也不影响服务运行
fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        // start 是 cmd 内建命令，第一个空参数是窗口标题占位
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    println!("[LittleDB] 已在浏览器打开 {}", url);
}

fn one_line(sql: &str) -> String {
    let collapsed: Vec<&str> = sql.split_whitespace().collect();
    let text = collapsed.join(" ");
    if text.chars().count() > 200 {
        text.chars().take(200).collect()
    } else {
        text
    }
}
