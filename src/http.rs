//! HTTP API + Web 控制台静态文件服务。
//!
//! 路由与响应结构对齐 Node 版 `src/server/http.js`：
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
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::codec::Value;
use crate::engine::Engine;
use crate::error::{self, DbError, Result};
use crate::json::{self, Json};
use crate::sql::executor::ConnSession;

const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_HEADER: usize = 64 * 1024;
const SESSION_TTL_SECS: u64 = 30 * 60;

#[derive(Clone)]
pub struct HttpState {
    pub engine: Arc<Mutex<Engine>>,
    sessions: Arc<Mutex<HashMap<String, (ConnSession, Instant)>>>,
    counter: Arc<AtomicU64>,
    started: Instant,
    pub token: Option<String>,
    pub web_dir: PathBuf,
    pub connections: Arc<AtomicUsize>,
    pub queries: Arc<AtomicUsize>,
    pub slow_queries: Arc<AtomicUsize>,
    pub rejected: Arc<AtomicUsize>,
}

impl HttpState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        engine: Arc<Mutex<Engine>>,
        token: Option<String>,
        web_dir: PathBuf,
        connections: Arc<AtomicUsize>,
        queries: Arc<AtomicUsize>,
        slow_queries: Arc<AtomicUsize>,
        rejected: Arc<AtomicUsize>,
    ) -> HttpState {
        HttpState {
            engine,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            counter: Arc::new(AtomicU64::new(0)),
            started: Instant::now(),
            token,
            web_dir,
            connections,
            queries,
            slow_queries,
            rejected,
        }
    }
}

pub fn serve(state: HttpState, host: &str, port: u16) -> Result<()> {
    let address = format!("{}:{}", host, port);
    let listener = TcpListener::bind(&address)
        .map_err(|err| DbError::internal(format!("HTTP 无法监听 {}：{}", address, err)))?;

    println!(
        "[LittleDB] HTTP 服务与 Web 控制台已监听 http://{}",
        listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or(address)
    );
    if !state.web_dir.join("index.html").exists() {
        println!(
            "[LittleDB] 警告：未找到 Web 控制台目录（{}），仅 API 可用",
            state.web_dir.display()
        );
    }

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let state = state.clone();
                let connections = Arc::clone(&state.connections);
                thread::spawn(move || {
                    connections.fetch_add(1, Ordering::SeqCst);
                    handle(state, stream);
                    connections.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

struct Request {
    method: String,
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
    let request = match read_request(&mut stream) {
        Ok(Some(r)) => r,
        _ => return,
    };

    if request.method == "OPTIONS" {
        send_response(&mut stream, 204, "No Content", "text/plain", b"", &[]);
        return;
    }

    if request.path.starts_with("/api/") {
        let (status, payload, extra) = route(&state, &request);
        let body = payload.to_string();
        send_response(
            &mut stream,
            status,
            status_text(status),
            "application/json; charset=utf-8",
            body.as_bytes(),
            &extra,
        );
        return;
    }

    serve_static(&state, &mut stream, &request);
}

// ------------------------------------------------------------------ 请求解析

fn read_request(stream: &mut TcpStream) -> Result<Option<Request>> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end;

    loop {
        let read = match stream.read(&mut chunk) {
            Ok(0) => return Ok(None),
            Ok(n) => n,
            Err(_) => return Ok(None),
        };
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_HEADER {
            return Err(DbError::new(error::PROTOCOL, "请求头过大"));
        }
        match find_subsequence(&buffer, b"\r\n\r\n") {
            Some(pos) => {
                header_end = pos;
                break;
            }
            None => continue,
        }
    }

    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut tokens = request_line.split_whitespace();
    let method = tokens.next().unwrap_or("").to_uppercase();
    let target = tokens.next().unwrap_or("/").to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some(index) = line.find(':') {
            let key = line[..index].trim().to_lowercase();
            let value = line[index + 1..].trim().to_string();
            headers.insert(key, value);
        }
    }

    let content_length = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY {
        return Err(DbError::new(error::PROTOCOL, "请求体超过 2MB 上限"));
    }

    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    let (path, query) = split_target(&target);

    Ok(Some(Request {
        method,
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
) {
    let mut head = format!("HTTP/1.1 {} {}\r\n", status, reason);
    head.push_str(&format!("Content-Type: {}\r\n", content_type));
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str("Connection: close\r\n");
    head.push_str("Access-Control-Allow-Origin: *\r\n");
    head.push_str("Access-Control-Allow-Headers: Content-Type, Authorization, X-Session-Id\r\n");
    head.push_str("Access-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\n");
    head.push_str("Access-Control-Expose-Headers: X-Session-Id\r\n");
    head.push_str("Cache-Control: no-store\r\n");
    for (key, value) in extra {
        head.push_str(&format!("{}: {}\r\n", key, value));
    }
    head.push_str("\r\n");

    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
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

// ------------------------------------------------------------------ 路由

fn route(state: &HttpState, request: &Request) -> (u16, Json, Vec<(String, String)>) {
    if let Some(expected) = &state.token {
        let provided = request
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|s| s.to_string())
            .or_else(|| request.param("token").map(|s| s.to_string()))
            .unwrap_or_default();
        if provided != *expected {
            return fail(401, error::AUTH, "认证失败：缺少或错误的 access token");
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
                // 与 Node 版一致：elapsedMs 在顶层，与 ok / result 同级
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
    } else {
        400
    };
    fail(status, err.code, &err.message)
}

fn with_session<T>(
    state: &HttpState,
    sid: Option<&str>,
    action: impl FnOnce(&mut ConnSession) -> T,
) -> (String, T) {
    let mut guard = match state.sessions.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };

    // 顺手清理过期会话
    let now = Instant::now();
    let expired: Vec<String> = guard
        .iter()
        .filter(|(_, (_, last))| now.duration_since(*last).as_secs() > SESSION_TTL_SECS)
        .map(|(id, _)| id.clone())
        .collect();
    for id in expired {
        if let Some((mut session, _)) = guard.remove(&id) {
            session.close();
        }
    }

    let id = match sid {
        Some(existing) if guard.contains_key(existing) => existing.to_string(),
        _ => {
            let n = state.counter.fetch_add(1, Ordering::SeqCst);
            format!("h{}-{}", now_millis(), n)
        }
    };

    let mut session = match guard.remove(&id) {
        Some((session, _)) => session,
        None => ConnSession::new(Arc::clone(&state.engine)),
    };

    let outcome = action(&mut session);
    guard.insert(id.clone(), (session, Instant::now()));
    (id, outcome)
}

fn health(state: &HttpState) -> Result<Json> {
    let engine = state
        .engine
        .lock()
        .map_err(|_| DbError::internal("数据库锁状态异常"))?;
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
    let mut engine = state
        .engine
        .lock()
        .map_err(|_| DbError::internal("数据库锁状态异常"))?;

    let names = engine.table_names();
    let mut items = Vec::with_capacity(names.len());
    for name in names {
        let (table_id, rows, columns) = {
            let table = engine.table(&name)?;
            (table.id, table.row_count, table.columns.len())
        };
        let pages = engine.store.page_count(table_id);
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
    let engine = state
        .engine
        .lock()
        .map_err(|_| DbError::internal("数据库锁状态异常"))?;

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
    let (id, outcome) = with_session(state, sid.as_deref(), |session| session.execute(&sql, &params));
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
        let engine = state
            .engine
            .lock()
            .map_err(|_| DbError::internal("数据库锁状态异常"))?;
        if engine.table(&name).is_err() {
            return Err(DbError::not_found(format!("表 \"{}\" 不存在", name)));
        }
    }

    let quoted = format!("\"{}\"", name.replace('"', "\"\""));
    let sql = format!("SELECT * FROM {} LIMIT {} OFFSET {}", quoted, limit, offset);
    let sid = request.header("x-session-id").map(|s| s.to_string());

    let (id, outcome) = with_session(state, sid.as_deref(), |session| session.execute(&sql, &[]));
    let result = outcome?;

    let mut fields = match result.to_wire_json() {
        Json::Obj(items) => items,
        other => vec![("data".to_string(), other)],
    };

    let (total, schema_json) = {
        let engine = state
            .engine
            .lock()
            .map_err(|_| DbError::internal("数据库锁状态异常"))?;
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

    let (_, outcome) = with_session(state, sid.as_deref(), |session| session.execute(&sql, &[]));
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

    let mut engine = state
        .engine
        .lock()
        .map_err(|_| DbError::internal("数据库锁状态异常"))?;

    // 重建是「删表 + 建表 + 逐行写回」，必须在单个事务里完成
    engine.begin()?;
    let outcome = match engine.rebuild_table(&name, &columns) {
        Ok(out) => match engine.commit() {
            Ok(()) => out,
            Err(err) => return Err(err),
        },
        Err(err) => {
            engine.rollback();
            return Err(err);
        }
    };

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

fn serve_static(state: &HttpState, stream: &mut TcpStream, request: &Request) {
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

    // 防止路径穿越
    let cleaned = rel.replace('\\', "/");
    if cleaned.contains("..") {
        send_response(
            stream,
            403,
            "Forbidden",
            "text/plain; charset=utf-8",
            b"403 Forbidden",
            &[],
        );
        return;
    }

    let web_dir = &state.web_dir;
    let mut file_path = web_dir.join(cleaned.trim_start_matches('/'));

    if !file_path.is_file() {
        file_path = web_dir.join("index.html");
        if !file_path.is_file() {
            send_response(
                stream,
                404,
                "Not Found",
                "text/plain; charset=utf-8",
                b"404 Not Found",
                &[],
            );
            return;
        }
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
            );
        }
    }
}

fn now_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(_) => 0,
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
