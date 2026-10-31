//! HTTP API + Web 控制台静态文件服务。
//!
//! HTTP 路由与响应结构：
//!   GET  /api/health
//!   GET  /api/tables
//!   GET  /api/schema
//!   POST /api/sql
//!   GET  /api/tables/:name?limit&offset
//!   DELETE /api/tables/:name
//!   POST /api/tables/:name/alter
//!
//! 只实现 HTTP/1.1 的最小可用子集：每个请求处理完即关闭连接（Connection: close），
//! 不支持 keep-alive、chunked、HTTPS。

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::auth::AuthLimiter;
use crate::codec::Value;
use crate::engine::Engine;
use crate::error::{self, DbError, Result};
use crate::json::{self, Json};
use crate::sql::executor::ConnSession;

const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_HEADER: usize = 64 * 1024;
/// 单次 socket 读的超时：客户端必须在这个时间内继续发送数据。
/// 防止声明了 Content-Length 却不发送的连接永久占住线程（slowloris）。
const READ_TIMEOUT_SECS: u64 = 30;
const SESSION_TTL_SECS: u64 = 30 * 60;
/// 事务中的会话空闲多久后自动回滚。
/// 被抛弃的 BEGIN 会一直握着全局写锁（其它会话全部 DATABASE_LOCKED），
/// 等 30 分钟才释放是不可接受的，所以这里用短得多的超时。
const TXN_SESSION_TTL_SECS: u64 = 120;
/// 同时在册的会话上限：每个会话都持有 Arc<Engine> 与未提交状态，
/// 无上限就是一条内存增长型 DoS 通道。超限时淘汰最久未使用的空闲会话。
const MAX_SESSIONS: usize = 512;
/// 认证失败限速的默认阈值（与 ServerOptions 的默认值保持一致）
const DEFAULT_AUTH_FAIL_LIMIT: u32 = 5;
/// 同一会话的并发请求最多等待多久（超过返回 503 SESSION_BUSY）
const DEFAULT_SESSION_WAIT_SECS: u64 = 30;

/// 一个 HTTP 会话。
///
/// 用 `Arc<SessionEntry>` + 内部 `Mutex` 而不是把 `ConnSession` 直接存在
/// map 里：旧实现在执行期间把会话从 map 取出、结束时插回，同一 sid 的两个
/// 并发请求会各自新建一个 `ConnSession` 并互相覆盖（前一个的开放事务被
/// Drop 静默回滚）。现在同一会话的请求在入口处串行，不同会话仍然并行。
struct SessionEntry {
    session: Mutex<ConnSession>,
    last_used: Mutex<Instant>,
}

impl SessionEntry {
    fn new(engine: Arc<RwLock<Engine>>) -> SessionEntry {
        SessionEntry {
            session: Mutex::new(ConnSession::new(engine)),
            last_used: Mutex::new(Instant::now()),
        }
    }

    /// 是否在事务中；None = 会话正被请求占用（拿不到锁）
    fn in_transaction(&self) -> Option<bool> {
        match self.session.try_lock() {
            Ok(guard) => Some(guard.in_transaction()),
            Err(_) => None,
        }
    }

    fn touch(&self) {
        if let Ok(mut last) = self.last_used.lock() {
            *last = Instant::now();
        }
    }

    fn idle_secs(&self) -> u64 {
        self.last_used
            .lock()
            .map(|last| last.elapsed().as_secs())
            .unwrap_or(0)
    }

    /// 没有请求正在使用它：可以安全移除（移除后 Drop 会回滚未提交事务）
    fn is_idle(&self) -> bool {
        self.session.try_lock().is_ok()
    }
}

#[derive(Clone)]
pub struct HttpState {
    pub engine: Arc<RwLock<Engine>>,
    sessions: Arc<Mutex<HashMap<String, Arc<SessionEntry>>>>,
    started: Instant,
    pub token: Option<String>,
    pub web_dir: PathBuf,
    pub connections: Arc<AtomicUsize>,
    pub max_connections: usize,
    pub queries: Arc<AtomicUsize>,
    pub slow_queries: Arc<AtomicUsize>,
    pub rejected: Arc<AtomicUsize>,
    /// 允许的跨源来源，逗号分隔；`*` 不限制。None = 只允许同源（默认）
    cors_origin: Option<String>,
    /// 认证失败限速（与 LDBP 端口共用同一份计数）
    auth: Arc<AuthLimiter>,
    /// 同一会话被占用时，等待多久算超时
    session_wait: Duration,
}

impl HttpState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        engine: Arc<RwLock<Engine>>,
        token: Option<String>,
        web_dir: PathBuf,
        connections: Arc<AtomicUsize>,
        max_connections: usize,
        queries: Arc<AtomicUsize>,
        slow_queries: Arc<AtomicUsize>,
        rejected: Arc<AtomicUsize>,
    ) -> HttpState {
        HttpState {
            engine,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            started: Instant::now(),
            token,
            web_dir,
            connections,
            max_connections,
            queries,
            slow_queries,
            rejected,
            cors_origin: None,
            auth: Arc::new(AuthLimiter::new(DEFAULT_AUTH_FAIL_LIMIT)),
            session_wait: Duration::from_secs(DEFAULT_SESSION_WAIT_SECS),
        }
    }

    /// 调整「同会话并发等待」的超时（测试用；生产默认 30 秒）
    pub fn with_session_wait(mut self, wait: Duration) -> HttpState {
        self.session_wait = wait;
        self
    }

    /// 配置允许的跨源来源（逗号分隔，`*` 表示不限制）
    pub fn with_cors(mut self, policy: String) -> HttpState {
        let policy = policy.trim().to_string();
        self.cors_origin = if policy.is_empty() { None } else { Some(policy) };
        self
    }

    /// 与 LDBP 端口共用同一份认证失败计数
    pub fn with_auth_limiter(mut self, limiter: Arc<AuthLimiter>) -> HttpState {
        self.auth = limiter;
        self
    }
}

pub fn serve(state: HttpState, host: &str, port: u16) -> Result<()> {
    let address = format!("{}:{}", host, port);
    let listener = TcpListener::bind(&address)
        .map_err(|err| DbError::internal(format!("HTTP 无法监听 {}：{}", address, err)))?;

    crate::log::info(&format!(
        "HTTP 服务与 Web 控制台已监听 http://{}",
        listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or(address)
    ));
    if !state.web_dir.join("index.html").exists() {
        crate::log::warn(&format!(
            "未找到 Web 控制台目录（{}），仅 API 可用",
            state.web_dir.display()
        ));
    }

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                // 先 CAS 预约名额再 spawn：原来的「load 检查 + 线程内 fetch_add」
                // 在并发 accept 时会超卖 max_connections
                let mut current = state.connections.load(Ordering::SeqCst);
                let mut reserved = false;
                while current < state.max_connections {
                    match state.connections.compare_exchange(
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
                    state.rejected.fetch_add(1, Ordering::SeqCst);
                    let mut stream = stream;
                    let _ = stream.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }
                let state = state.clone();
                let connections = Arc::clone(&state.connections);
                let rejected = Arc::clone(&state.rejected);
                // 连接线程按解析器闸门给栈：MAX_EXPR_DEPTH=64、MAX_TOKENS=2万，
                // 2MB 足够；64 连接因此省下约 384MB 虚拟内存
                let slot = Arc::clone(&connections);
                let spawn = thread::Builder::new()
                    .stack_size(2 * 1024 * 1024)
                    .spawn(move || {
                        handle(state, stream);
                        slot.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawn.is_err() {
                    connections.fetch_sub(1, Ordering::SeqCst);
                    rejected.fetch_add(1, Ordering::SeqCst);
                }
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

struct Request {
    method: String,
    version: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|s| s.as_str())
    }

    fn param(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(|s| s.as_str())
    }

    fn json_body(&self) -> Result<Json> {
        if self.body.is_empty() {
            return Ok(Json::obj());
        }
        let text = std::str::from_utf8(&self.body)
            .map_err(|_| DbError::new(error::PROTOCOL, "请求体不是合法 UTF-8"))?;
        json::parse(text).map_err(|_| DbError::new(error::PROTOCOL, "请求体必须是 JSON"))
    }
}

fn handle(state: HttpState, mut stream: TcpStream) {
    // 来源 IP 用于认证失败限速（拿不到就当未知来源，不限速）
    let remote: Option<IpAddr> = stream.peer_addr().ok().map(|addr| addr.ip());
    // 读/写超时：连接建立后客户端必须在时限内继续发数据，
    // 否则线程不会永远等下去（防 slowloris；正常请求毫秒级完成）
    let _ = stream.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)));
    let _ = stream.set_nodelay(true);

    // keep-alive：HTTP/1.1 默认复用连接，客户端省掉每请求的 TCP 建立。
    // pending 保存流水线场景下「上一条请求多读出来的下一请求字节」。
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let request = match read_request(&mut stream, &mut pending) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(err) => {
                // 解析失败必须回一个错误响应再断开：
                // 旧行为直接关连接，客户端只看到「连接被重置」
                let status = if err.code == error::LENGTH_REQUIRED {
                    411
                } else {
                    400
                };
                let body = format!(
                    "{{\"ok\":false,\"error\":{{\"code\":\"{}\",\"message\":\"{}\"}}}}",
                    err.code, err.message
                );
                let _ = send_response(
                    &mut stream,
                    status,
                    status_text(status),
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    &[],
                    false,
                );
                return;
            }
        };
        let keep_alive = wants_keep_alive(&request);

        if request.method == "OPTIONS" {
            // 预检：只有配置了 --cors-origin 才回跨源头，否则同源访问本就不需要
            let extra = cors_headers(&state, &request);
            send_response(
                &mut stream,
                204,
                "No Content",
                "text/plain",
                b"",
                &extra,
                keep_alive,
            );
        } else if request.path.starts_with("/api/") {
            let (status, payload, mut extra) = route(&state, &request, remote);
            extra.extend(cors_headers(&state, &request));
            let body = payload.to_string();
            send_response(
                &mut stream,
                status,
                status_text(status),
                "application/json; charset=utf-8",
                body.as_bytes(),
                &extra,
                keep_alive,
            );
        } else {
            serve_static(&state, &mut stream, &request, keep_alive);
        }

        if !keep_alive {
            return;
        }
    }
}

/// HTTP/1.1 默认 keep-alive；HTTP/1.0 需要显式声明；Connection: close 一票否决
fn wants_keep_alive(request: &Request) -> bool {
    let connection = request
        .header("connection")
        .unwrap_or("")
        .to_ascii_lowercase();
    if connection.split(',').any(|t| t.trim() == "close") {
        return false;
    }
    if request.version == "HTTP/1.0" {
        return connection.split(',').any(|t| t.trim() == "keep-alive");
    }
    true
}

// ------------------------------------------------------------------ 请求解析

/// 读取一条请求。`pending` 是上一条请求遗留的字节（流水线），优先消费；
/// 本次多读出来的字节同样写回 `pending` 供下一条请求使用。
fn read_request(stream: &mut TcpStream, pending: &mut Vec<u8>) -> Result<Option<Request>> {
    let mut buffer = std::mem::take(pending);
    let mut chunk = [0u8; 4096];
    let header_end;

    loop {
        if let Some(pos) = find_subsequence(&buffer, b"\r\n\r\n") {
            header_end = pos;
            break;
        }
        if buffer.len() > MAX_HEADER {
            return Err(DbError::new(error::PROTOCOL, "请求头过大"));
        }
        let read = match stream.read(&mut chunk) {
            Ok(0) => return Ok(None),
            Ok(n) => n,
            Err(_) => return Ok(None),
        };
        buffer.extend_from_slice(&chunk[..read]);
    }

    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut tokens = request_line.split_whitespace();
    let method = tokens.next().unwrap_or("").to_uppercase();
    let target = tokens.next().unwrap_or("/").to_string();
    let version = tokens.next().unwrap_or("HTTP/1.0").to_uppercase();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some(index) = line.find(':') {
            let key = line[..index].trim().to_lowercase();
            let value = line[index + 1..].trim().to_string();
            // 关键头部重复出现是请求走私的典型手法，直接拒绝
            let critical = matches!(
                key.as_str(),
                "content-length" | "transfer-encoding" | "host"
            );
            if critical && headers.contains_key(&key) {
                return Err(DbError::new(
                    error::PROTOCOL,
                    format!("请求头 {} 重复出现", key),
                ));
            }
            headers.insert(key, value);
        }
    }

    // 不支持分块传输：静默当空 body 会让「POST /api/sql」变成
    // 「执行一条没有 body 的语句」，必须显式拒绝
    if let Some(encoding) = headers.get("transfer-encoding") {
        if encoding.to_ascii_lowercase().contains("chunked") {
            return Err(DbError::new(
                error::LENGTH_REQUIRED,
                "不支持 Transfer-Encoding: chunked，请改用 Content-Length",
            ));
        }
    }

    // Content-Length 存在但非法（负数、非数字）时要报错，不能当成 0 静默吞掉 body
    let content_length = match headers.get("content-length") {
        Some(raw) => raw.trim().parse::<usize>().map_err(|_| {
            DbError::new(error::PROTOCOL, "Content-Length 不是合法的非负整数")
        })?,
        None => 0,
    };
    if content_length > MAX_BODY {
        return Err(DbError::new(error::PROTOCOL, "请求体超过 2MB 上限"));
    }

    let mut body: Vec<u8> = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        body.extend_from_slice(&chunk[..read]);
    }
    if body.len() > content_length {
        // 超出本请求 body 的部分属于流水线中的下一条请求
        *pending = body[content_length..].to_vec();
        body.truncate(content_length);
    }

    let (path, query) = split_target(&target);

    Ok(Some(Request {
        method,
        version,
        path,
        query,
        headers,
        body,
    }))
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn split_target(target: &str) -> (String, HashMap<String, String>) {
    let mut query = HashMap::new();
    let (path, raw_query) = match target.find('?') {
        Some(index) => (&target[..index], &target[index + 1..]),
        None => (target, ""),
    };
    for pair in raw_query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let key = url_decode(parts.next().unwrap_or(""));
        let value = url_decode(parts.next().unwrap_or(""));
        query.insert(key, value);
    }
    (url_decode(path), query)
}

fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

// ------------------------------------------------------------------ 响应

fn send_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    extra: &[(String, String)],
    keep_alive: bool,
) {
    let mut head = format!("HTTP/1.1 {} {}\r\n", status, reason);
    head.push_str(&format!("Content-Type: {}\r\n", content_type));
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str(if keep_alive {
        "Connection: keep-alive\r\n"
    } else {
        "Connection: close\r\n"
    });
    // 跨源头不在这里写死：由 cors_headers() 按 --cors-origin 策略决定
    head.push_str("Cache-Control: no-store\r\n");
    head.push_str("X-Content-Type-Options: nosniff\r\n");
    for (key, value) in extra {
        head.push_str(&format!("{}: {}\r\n", key, value));
    }
    head.push_str("\r\n");

    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
    if !keep_alive {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

fn ok(payload: Json) -> (u16, Json, Vec<(String, String)>) {
    (
        200,
        Json::Obj(vec![
            ("ok".to_string(), Json::Bool(true)),
            ("result".to_string(), payload),
        ]),
        Vec::new(),
    )
}

/// 认证退避：429 + `Retry-After`（秒）
fn rate_limited(wait_secs: u64) -> (u16, Json, Vec<(String, String)>) {
    let (status, payload, mut extra) = fail(
        429,
        error::RATE_LIMITED,
        &format!("认证失败次数过多，请 {} 秒后再试", wait_secs),
    );
    extra.push(("Retry-After".to_string(), wait_secs.to_string()));
    (status, payload, extra)
}

fn fail(status: u16, code: &str, message: &str) -> (u16, Json, Vec<(String, String)>) {
    (
        status,
        Json::Obj(vec![
            ("ok".to_string(), Json::Bool(false)),
            (
                "error".to_string(),
                Json::Obj(vec![
                    ("code".to_string(), Json::str(code)),
                    ("message".to_string(), Json::str(message)),
                ]),
            ),
        ]),
        Vec::new(),
    )
}

/// 跨源响应头。默认（未配置 `--cors-origin`）返回空——控制台同源，不需要 CORS，
/// 而 `Access-Control-Allow-Origin: *` 会让任意网页借用户浏览器读写数据库。
fn cors_headers(state: &HttpState, request: &Request) -> Vec<(String, String)> {
    let Some(policy) = &state.cors_origin else {
        return Vec::new();
    };
    let allowed = if policy == "*" {
        Some("*".to_string())
    } else {
        let origin = request.header("origin").unwrap_or("");
        policy
            .split(',')
            .map(|item| item.trim())
            .find(|item| item.eq_ignore_ascii_case(origin))
            .map(|item| item.to_string())
    };
    let Some(allow) = allowed else {
        return Vec::new();
    };
    vec![
        ("Access-Control-Allow-Origin".to_string(), allow),
        (
            "Access-Control-Allow-Headers".to_string(),
            "Content-Type, Authorization, X-Session-Id".to_string(),
        ),
        (
            "Access-Control-Allow-Methods".to_string(),
            "GET, POST, DELETE, OPTIONS".to_string(),
        ),
        (
            "Access-Control-Expose-Headers".to_string(),
            "X-Session-Id".to_string(),
        ),
        ("Vary".to_string(), "Origin".to_string()),
    ]
}

// ------------------------------------------------------------------ 路由

fn route(
    state: &HttpState,
    request: &Request,
    remote: Option<IpAddr>,
) -> (u16, Json, Vec<(String, String)>) {
    if let Some(expected) = &state.token {
        // 退避窗口内直接 429，连比较都不做（避免继续给爆破方反馈）
        if let Some(ip) = remote {
            if let Err(wait) = state.auth.check(ip) {
                return rate_limited(wait);
            }
        }
        // 只认 Authorization 头：query 里的 token 会进访问日志和浏览器历史
        let provided = request
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        // 常量时间比较，避免逐字节短路的时序侧信道
        if !crate::protocol::secret_eq(provided, expected) {
            if let Some(ip) = remote {
                state.auth.record_failure(ip);
            }
            return fail(401, error::AUTH, "认证失败：缺少或错误的 access token");
        }
        if let Some(ip) = remote {
            state.auth.record_success(ip);
        }
    }

    let parts: Vec<&str> = request.path.split('/').filter(|p| !p.is_empty()).collect();

    match (request.method.as_str(), parts.as_slice()) {
        ("GET", ["api", "health"]) => match health(state) {
            Ok(body) => ok(body),
            Err(err) => to_error(err),
        },
        ("GET", ["api", "tables"]) => match list_tables(state) {
            Ok(body) => ok(body),
            Err(err) => to_error(err),
        },
        ("GET", ["api", "schema"]) => match schema(state) {
            Ok(body) => ok(body),
            Err(err) => to_error(err),
        },
        ("POST", ["api", "sql"]) => match run_sql(state, request) {
            Ok((result_json, elapsed, sid)) => {
                // elapsedMs 在顶层，与 ok / result 同级
                let mut extra = Vec::new();
                if let Some(id) = sid {
                    extra.push(("X-Session-Id".to_string(), id));
                }
                (
                    200,
                    Json::Obj(vec![
                        ("ok".to_string(), Json::Bool(true)),
                        ("result".to_string(), result_json),
                        ("elapsedMs".to_string(), Json::Num(elapsed)),
                    ]),
                    extra,
                )
            }
            Err(err) => to_error(err),
        },
        ("GET", ["api", "tables", name]) => match table_rows(state, request, name) {
            Ok((body, sid)) => {
                let mut out = ok(body);
                if let Some(id) = sid {
                    out.2.push(("X-Session-Id".to_string(), id));
                }
                out
            }
            Err(err) => to_error(err),
        },
        ("DELETE", ["api", "tables", name]) => match drop_table(state, request, name) {
            Ok(body) => ok(body),
            Err(err) => to_error(err),
        },
        ("POST", ["api", "tables", name, "alter"]) => match alter_table(state, request, name) {
            Ok(body) => ok(body),
            Err(err) => to_error(err),
        },
        _ => fail(
            404,
            error::NOT_FOUND,
            &format!("未知的 API 路由 {} {}", request.method, request.path),
        ),
    }
}

fn to_error(err: DbError) -> (u16, Json, Vec<(String, String)>) {
    let status = if err.code == error::AUTH {
        401
    } else if err.code == error::NOT_FOUND {
        404
    } else if err.code == error::WRITE_CONFLICT {
        409
    } else if err.code == error::RATE_LIMITED {
        429
    } else if err.code == error::SESSION_BUSY {
        503
    } else {
        400
    };
    let (status, payload, mut extra) = fail(status, err.code, &err.message);
    // 这两类错误都是「等一会儿再来」，带 Retry-After 让客户端知道别立刻重试
    if err.code == error::SESSION_BUSY || err.code == error::RATE_LIMITED {
        extra.push(("Retry-After".to_string(), "1".to_string()));
    }
    (status, payload, extra)
}

fn with_session<T>(
    state: &HttpState,
    sid: Option<&str>,
    action: impl FnOnce(&mut ConnSession) -> T,
) -> Result<(String, T)> {
    let (id, entry) = {
        let mut guard = lock_sessions(state);
        reap_sessions(&mut guard);

        let id = match sid {
            // 已知会话 id 且仍然存活 → 复用（事务状态因此跟着走）
            Some(existing) if guard.contains_key(existing) => existing.to_string(),
            // 未知/未提供 → 新建，id 通过响应头回给客户端
            _ => {
                let fresh = new_session_id();
                guard.insert(
                    fresh.clone(),
                    Arc::new(SessionEntry::new(Arc::clone(&state.engine))),
                );
                fresh
            }
        };
        let entry = guard
            .get(&id)
            .map(Arc::clone)
            .expect("会话刚插入/校验过，必然存在");
        (id, entry)
    };

    // 只锁「本会话」：同一 sid 的并发请求在此串行（一个会话就是一条连接），
    // 不同会话完全并行，长查询不会阻塞别的请求的会话解析。
    //
    // 但不能无限等：一个卡死的查询会让同会话的后续请求一直占着线程。
    // 用 try_lock 轮询实现超时（std 的 Mutex 没有 lock_timeout）。
    let deadline = Instant::now() + state.session_wait;
    let mut session = loop {
        match entry.session.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(DbError::new(
                        error::SESSION_BUSY,
                        format!(
                            "同一会话（{}）还有请求在执行，已等待 {} 秒；请稍后重试或换一个 X-Session-Id",
                            id,
                            state.session_wait.as_secs()
                        ),
                    ));
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    };
    let outcome = action(&mut session);
    drop(session);
    entry.touch();
    Ok((id, outcome))
}

fn lock_sessions(
    state: &HttpState,
) -> std::sync::MutexGuard<'_, HashMap<String, Arc<SessionEntry>>> {
    match state.sessions.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 清理过期会话；条目数达到上限时再淘汰最久未使用的空闲会话。
/// 只处理拿得到锁的会话——正在执行的请求不会被抽走底层 ConnSession。
fn reap_sessions(sessions: &mut HashMap<String, Arc<SessionEntry>>) {
    let expired: Vec<String> = sessions
        .iter()
        .filter(|(_, entry)| {
            // 事务中的会话用短超时：被抛弃的 BEGIN 会一直握着写锁
            let ttl = match entry.in_transaction() {
                Some(true) => TXN_SESSION_TTL_SECS,
                _ => SESSION_TTL_SECS,
            };
            entry.idle_secs() > ttl
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in expired {
        let idle = sessions.get(&id).map(|e| e.is_idle()).unwrap_or(false);
        if idle {
            sessions.remove(&id);
        }
    }

    while sessions.len() >= MAX_SESSIONS {
        let victim = sessions
            .iter()
            .filter(|(_, entry)| entry.is_idle())
            .max_by_key(|(_, entry)| entry.idle_secs())
            .map(|(id, _)| id.clone());
        match victim {
            Some(id) => {
                sessions.remove(&id);
            }
            None => break, // 全都在执行中：宁可暂时超限也不要打断请求
        }
    }
}

/// 会话 id。旧实现是 `h{毫秒时间戳}-{自增计数}`，完全可枚举，
/// 枚举到别人的 id 就能接手对方未提交的事务。改用 std 的 `RandomState`
/// （由操作系统熵做种）派生，不引入第三方依赖也不需要额外状态。
fn new_session_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut id = String::with_capacity(49);
    id.push('s');
    for round in 0..3u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(nanos);
        hasher.write_u64(round);
        id.push_str(&format!("{:016x}", hasher.finish()));
    }
    id
}

/// 引擎读锁：锁中毒时沿用内部数据。
/// 中毒只发生在「持锁线程 panic」之后，数据本身自洽；为一个请求线程的
/// panic 让整个服务持续返回 500 不划算。
fn read_engine(state: &HttpState) -> std::sync::RwLockReadGuard<'_, Engine> {
    state
        .engine
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 引擎写锁：同上
fn write_engine(state: &HttpState) -> std::sync::RwLockWriteGuard<'_, Engine> {
    state
        .engine
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn health(state: &HttpState) -> Result<Json> {
    let engine = read_engine(state);
    let stats = engine.store.stats();
    let total_rows: u64 = engine.tables.iter().map(|t| t.row_count as u64).sum();

    Ok(Json::Obj(vec![
        ("status".to_string(), Json::str("ok")),
        ("server".to_string(), Json::str("LittleDB")),
        ("protocol".to_string(), Json::Num(1.0)),
        (
            "uptime".to_string(),
            Json::Num(state.started.elapsed().as_secs() as f64),
        ),
        (
            "stats".to_string(),
            Json::Obj(vec![
                (
                    "dir".to_string(),
                    Json::str(engine.store.dir.display().to_string()),
                ),
                ("tables".to_string(), Json::Num(stats.tables as f64)),
                ("rows".to_string(), Json::Num(total_rows as f64)),
                ("pages".to_string(), Json::Num(stats.pages as f64)),
                ("bytes".to_string(), Json::Num(stats.bytes as f64)),
                ("pageSize".to_string(), Json::Num(4096.0)),
                ("walBytes".to_string(), Json::Num(stats.wal_bytes as f64)),
                ("walLimit".to_string(), Json::Num(8.0 * 1024.0 * 1024.0)),
                (
                    "pendingPages".to_string(),
                    Json::Num(stats.pending_pages as f64),
                ),
                (
                    "checkpoints".to_string(),
                    Json::Num(stats.checkpoints as f64),
                ),
            ]),
        ),
        (
            "connections".to_string(),
            Json::Num(state.connections.load(Ordering::SeqCst) as f64),
        ),
        (
            "queries".to_string(),
            Json::Num(state.queries.load(Ordering::SeqCst) as f64),
        ),
        (
            "slowQueries".to_string(),
            Json::Num(state.slow_queries.load(Ordering::SeqCst) as f64),
        ),
        (
            "rejected".to_string(),
            Json::Num(state.rejected.load(Ordering::SeqCst) as f64),
        ),
    ]))
}

fn list_tables(state: &HttpState) -> Result<Json> {
    let engine = read_engine(state);

    let names = engine.table_names();
    let mut items = Vec::with_capacity(names.len());
    for name in names {
        let (table_id, rows, columns) = {
            let table = engine.table(&name)?;
            (table.id, table.row_count, table.columns.len())
        };
        let pages = engine.store.page_count_shared(table_id);
        items.push(Json::Obj(vec![
            ("name".to_string(), Json::str(name)),
            ("rows".to_string(), Json::Num(rows as f64)),
            ("pages".to_string(), Json::Num(pages as f64)),
            ("columns".to_string(), Json::Num(columns as f64)),
        ]));
    }

    Ok(Json::Obj(vec![(
        "tables".to_string(),
        Json::Arr(items),
    )]))
}

fn schema(state: &HttpState) -> Result<Json> {
    let engine = read_engine(state);

    let mut items = Vec::new();
    for table in &engine.tables {
        items.push(describe_table(table));
    }

    Ok(Json::Obj(vec![(
        "tables".to_string(),
        Json::Arr(items),
    )]))
}

fn describe_table(table: &crate::table::Table) -> Json {
    let columns = table
        .columns
        .iter()
        .map(|c| {
            Json::Obj(vec![
                ("name".to_string(), Json::str(c.name.clone())),
                ("type".to_string(), Json::str(c.ty.as_str())),
                // DECIMAL 的标度必须单独暴露：只给 "DECIMAL" 的话，
                // 控制台按 schema 重建表时会退回默认标度，备份恢复会丢精度
                ("scale".to_string(), Json::Num(c.scale as f64)),
                ("notNull".to_string(), Json::Bool(c.not_null)),
                ("primaryKey".to_string(), Json::Bool(c.primary_key)),
                ("unique".to_string(), Json::Bool(c.unique)),
                (
                    "default".to_string(),
                    match &c.default {
                        Some(v) => Json::str(v.as_text()),
                        None => Json::Null,
                    },
                ),
            ])
        })
        .collect();

    let indexes = table
        .indexes
        .iter()
        .map(|i| {
            Json::Obj(vec![
                ("name".to_string(), Json::str(i.name.clone())),
                (
                    "columns".to_string(),
                    Json::Arr(
                        i.columns
                            .iter()
                            .map(|ci| Json::str(table.columns[*ci].name.clone()))
                            .collect(),
                    ),
                ),
                ("unique".to_string(), Json::Bool(i.unique)),
                ("primary".to_string(), Json::Bool(i.primary)),
                ("implicit".to_string(), Json::Bool(i.implicit)),
            ])
        })
        .collect();

    Json::Obj(vec![
        ("name".to_string(), Json::str(table.name.clone())),
        ("id".to_string(), Json::Num(table.id as f64)),
        ("rows".to_string(), Json::Num(table.row_count as f64)),
        ("columns".to_string(), Json::Arr(columns)),
        ("indexes".to_string(), Json::Arr(indexes)),
    ])
}

fn run_sql(
    state: &HttpState,
    request: &Request,
) -> Result<(Json, f64, Option<String>)> {
    let payload = request.json_body()?;
    let sql = payload
        .get("sql")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let params: Vec<Value> = match payload.get("params") {
        Some(Json::Arr(items)) => items.iter().map(json_to_value).collect(),
        _ => Vec::new(),
    };
    let sid = request
        .header("x-session-id")
        .map(|s| s.to_string())
        .or_else(|| payload.get("session").and_then(|v| v.as_str()).map(|s| s.to_string()));

    let started = Instant::now();
    let (id, outcome) =
        with_session(state, sid.as_deref(), |session| session.execute(&sql, &params))?;
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;

    state.queries.fetch_add(1, Ordering::SeqCst);
    let result = outcome?;

    Ok((result.to_wire_json(), elapsed, Some(id)))
}

fn table_rows(
    state: &HttpState,
    request: &Request,
    raw_name: &str,
) -> Result<(Json, Option<String>)> {
    let name = url_decode(raw_name);
    let limit = request
        .param("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(100)
        .clamp(1, 5000);
    let offset = request
        .param("offset")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);

    {
        let engine = read_engine(state);
        if engine.table(&name).is_err() {
            return Err(DbError::not_found(format!("表 \"{}\" 不存在", name)));
        }
    }

    let quoted = format!("\"{}\"", name.replace('"', "\"\""));
    let sql = format!("SELECT * FROM {} LIMIT {} OFFSET {}", quoted, limit, offset);
    let sid = request.header("x-session-id").map(|s| s.to_string());

    let (id, outcome) = with_session(state, sid.as_deref(), |session| session.execute(&sql, &[]))?;
    let result = outcome?;

    let mut fields = match result.to_wire_json() {
        Json::Obj(items) => items,
        other => vec![("data".to_string(), other)],
    };

    let (total, schema_json) = {
        let engine = read_engine(state);
        let table = engine.table(&name)?;
        (table.row_count, describe_table(table))
    };

    fields.push(("table".to_string(), Json::str(name)));
    fields.push(("total".to_string(), Json::Num(total as f64)));
    fields.push(("limit".to_string(), Json::Num(limit as f64)));
    fields.push(("offset".to_string(), Json::Num(offset as f64)));
    fields.push(("schema".to_string(), schema_json));

    Ok((Json::Obj(fields), Some(id)))
}

fn drop_table(
    state: &HttpState,
    request: &Request,
    raw_name: &str,
) -> Result<Json> {
    let name = url_decode(raw_name);
    let quoted = format!("\"{}\"", name.replace('"', "\"\""));
    let sql = format!("DROP TABLE {}", quoted);
    let sid = request.header("x-session-id").map(|s| s.to_string());

    let (_, outcome) = with_session(state, sid.as_deref(), |session| session.execute(&sql, &[]))?;
    Ok(outcome?.to_wire_json())
}

fn alter_table(
    state: &HttpState,
    request: &Request,
    raw_name: &str,
) -> Result<Json> {
    let name = url_decode(raw_name);
    let payload = request.json_body()?;
    let columns = match payload.get("columns") {
        Some(Json::Arr(items)) => items.clone(),
        _ => return Err(DbError::syntax("缺少数组字段 columns")),
    };

    let durable = { read_engine(state).store.durable_commits() };
    let mut engine = write_engine(state);
    engine.ensure_ddl_exclusive(crate::engine::INTERNAL_TXN_OWNER)?;

    // 重建是「删表 + 建表 + 逐行写回」，必须在单个事务里完成
    engine.begin(crate::engine::INTERNAL_TXN_OWNER)?;
    engine.store.set_active_owner(crate::engine::INTERNAL_TXN_OWNER);
    let (outcome, barrier, seq) = match engine.rebuild_table(&name, &columns) {
        Ok(out) => match engine.commit(crate::engine::INTERNAL_TXN_OWNER) {
            Ok(()) => {
                let barrier = engine.store.commit_barrier();
                let seq = engine.store.last_commit_seq();
                (out, barrier, seq)
            }
            Err(err) => return Err(err),
        },
        Err(err) => {
            engine.rollback(crate::engine::INTERNAL_TXN_OWNER);
            return Err(err);
        }
    };
    drop(engine);
    // full 模式：写锁释放后再等待 fsync 确认
    if durable {
        barrier.wait_for(seq)?;
    }

    Ok(Json::Obj(vec![
        (
            "table".to_string(),
            Json::Obj(vec![
                ("name".to_string(), Json::str(name)),
                (
                    "rows".to_string(),
                    Json::Num(outcome.rows as f64),
                ),
                (
                    "columns".to_string(),
                    Json::Arr(
                        outcome
                            .columns
                            .iter()
                            .map(|(n, t)| {
                                Json::Obj(vec![
                                    ("name".to_string(), Json::str(n.clone())),
                                    ("type".to_string(), Json::str(t.clone())),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]),
        ),
        ("rows".to_string(), Json::Num(outcome.rows as f64)),
    ]))
}

fn json_to_value(value: &Json) -> Value {
    match value {
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

// ------------------------------------------------------------------ 静态文件

fn serve_static(state: &HttpState, stream: &mut TcpStream, request: &Request, keep_alive: bool) {
    let mime_for = |path: &Path| -> &'static str {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase()
            .as_str()
        {
            "html" => "text/html; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "js" => "application/javascript; charset=utf-8",
            "json" => "application/json; charset=utf-8",
            "svg" => "image/svg+xml",
            "png" => "image/png",
            "ico" => "image/x-icon",
            "txt" => "text/plain; charset=utf-8",
            _ => "application/octet-stream",
        }
    };

    let mut rel = request.path.clone();
    if rel == "/" || rel.is_empty() {
        rel = "/index.html".to_string();
    }

    // 防止路径穿越与绝对路径逃逸：
    // Path::join 遇到 "C:/..." 或 "//server/share" 这类绝对路径会整体替换基准目录，
    // 所以除 ".." 之外，还必须拒绝带 Windows 盘符 / UNC 前缀的路径，
    // 并在拼接后校验最终路径仍然位于 web_dir 之内。
    let cleaned = rel.replace('\\', "/");
    let has_drive_prefix = {
        let bytes = cleaned.as_bytes();
        // "C:/" 或 "C:\\"（cleaned 已把 '\' 换成 '/'，这里只看 "x:/"）
        (bytes.len() >= 2 && bytes[1] == b':')
            || cleaned.starts_with("//")
    };
    if cleaned.contains("..") || has_drive_prefix {
        send_response(
            stream,
            403,
            "Forbidden",
            "text/plain; charset=utf-8",
            b"403 Forbidden",
            &[],
            keep_alive,
        );
        return;
    }

    let web_dir = &state.web_dir;
    let file_path = web_dir.join(cleaned.trim_start_matches('/'));
    let canonical_base = fs::canonicalize(web_dir).unwrap_or_else(|_| web_dir.clone());
    // 文件不存在时 canonicalize 会失败，交给后面的 index.html 兜底处理；
    // 这里只拦截「解析成功但落在 web_dir 之外」的路径。
    let escapes_base = fs::canonicalize(&file_path)
        .map(|resolved| !resolved.starts_with(&canonical_base))
        .unwrap_or(false);
    if escapes_base {
        send_response(
            stream,
            403,
            "Forbidden",
            "text/plain; charset=utf-8",
            b"403 Forbidden",
            &[],
            keep_alive,
        );
        return;
    }

    if !file_path.is_file() {
        // 只有根路径映射到 index.html；其余缺失文件老实回 404
        // （旧行为是任何路径都回 index.html + 200，前端拿到一堆「HTML 格式的 JS」）
        send_response(
            stream,
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            b"404 Not Found",
            &[],
            keep_alive,
        );
        return;
    }

    match fs::read(&file_path) {
        Ok(body) => {
            let extra = vec![("Cache-Control".to_string(), "no-cache".to_string())];
            send_response(
                stream,
                200,
                "OK",
                mime_for(&file_path),
                &body,
                &extra,
                keep_alive,
            );
        }
        Err(err) => {
            let text = format!("500 {}", err);
            send_response(
                stream,
                500,
                "Internal Server Error",
                "text/plain; charset=utf-8",
                text.as_bytes(),
                &[],
                keep_alive,
            );
        }
    }
}

/// 从 exe 所在目录或当前目录向上找 web 目录
pub fn find_web_dir() -> PathBuf {
    let candidates = ["web", "../web", "../../web"];
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.join("index.html").is_file() {
            return path;
        }
    }
    PathBuf::from("web")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn temp_state(name: &str, wait: Duration) -> (PathBuf, HttpState) {
        let mut dir = std::env::temp_dir();
        dir.push(format!("littledb-http-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let engine = Engine::open(&dir).unwrap();
        let counter = || Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = HttpState::new(
            Arc::new(RwLock::new(engine)),
            Some("t".to_string()),
            PathBuf::from("web"),
            counter(),
            4,
            counter(),
            counter(),
            counter(),
        )
        .with_session_wait(wait);
        (dir, state)
    }

    /// 同一会话被占用时，第二个请求必须超时报错，而不是无限阻塞
    #[test]
    fn same_session_wait_times_out() {
        let (dir, state) = temp_state("busy", Duration::from_millis(150));
        let (sid, _) = with_session(&state, None, |_| ()).unwrap();

        let (tx, rx) = mpsc::channel();
        let holder_state = state.clone();
        let holder_sid = sid.clone();
        let holder = thread::spawn(move || {
            with_session(&holder_state, Some(&holder_sid), |_| {
                let _ = tx.send(());
                thread::sleep(Duration::from_millis(600));
            })
            .unwrap();
        });
        // 等持有者真正拿到会话锁（而不是靠 sleep 猜）
        rx.recv_timeout(Duration::from_secs(5))
            .expect("持有者未能取得会话锁");

        let started = Instant::now();
        let err = with_session(&state, Some(&sid), |_| ()).unwrap_err();
        assert_eq!(err.code, error::SESSION_BUSY, "{}", err.message);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "应在超时后立即返回，实际 {:?}",
            started.elapsed()
        );

        holder.join().unwrap();
        // 持有者释放后，同一会话立刻恢复可用
        assert!(with_session(&state, Some(&sid), |_| ()).is_ok());
        drop(state);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 会话忙 / 认证退避都要映射成「等会儿再来」的状态码 + Retry-After
    #[test]
    fn busy_and_rate_limited_map_to_retryable_status() {
        let (status, payload, extra) = to_error(DbError::new(error::SESSION_BUSY, "busy"));
        assert_eq!(status, 503);
        assert!(extra.iter().any(|(k, _)| k == "Retry-After"));
        assert!(payload.to_string().contains("SESSION_BUSY"));

        let (status, _, extra) = to_error(DbError::new(error::RATE_LIMITED, "slow down"));
        assert_eq!(status, 429);
        assert!(extra.iter().any(|(k, _)| k == "Retry-After"));
    }
}
