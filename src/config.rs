//! `littledb.json` 配置文件。
//!
//! 优先级（后者覆盖前者）：内置默认 → 配置文件 → 环境变量 `LDB_TOKEN` → 命令行参数。
//! 只用自带的 `json` 模块解析，不引入依赖。
//!
//! ```json
//! {
//!   "host": "127.0.0.1",
//!   "port": 7801,
//!   "httpPort": 7802,
//!   "noHttp": false,
//!   "token": "换成一串随机字符",
//!   "maxConnections": 64,
//!   "slowMs": 200,
//!   "verbose": false,
//!   "sync": "full",
//!   "corsOrigin": "https://app.example.com",
//!   "allowInsecure": false,
//!   "authFailLimit": 5,
//!   "webDir": "./web",
//!   "logFile": "./littledb.log",
//!   "logMaxBytes": 8388608
//! }
//! ```

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::json::{self, Json};
use crate::log::DEFAULT_MAX_BYTES;
use crate::server::ServerOptions;

/// 配置文件里出现的键（用于拼写检查式告警）
const KNOWN_KEYS: &[&str] = &[
    "host",
    "port",
    "httpPort",
    "noHttp",
    "token",
    "maxConnections",
    "slowMs",
    "verbose",
    "sync",
    "durableCommits",
    "corsOrigin",
    "allowInsecure",
    "authFailLimit",
    "webDir",
    "logFile",
    "logMaxBytes",
];

#[derive(Debug, Clone, Default)]
pub struct FileConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub http_port: Option<u16>,
    pub no_http: Option<bool>,
    pub token: Option<String>,
    pub max_connections: Option<usize>,
    pub slow_ms: Option<f64>,
    pub verbose: Option<bool>,
    pub durable_commits: Option<bool>,
    pub cors_origin: Option<String>,
    pub allow_insecure: Option<bool>,
    pub auth_fail_limit: Option<u32>,
    pub web_dir: Option<String>,
    pub log_file: Option<String>,
    pub log_max_bytes: Option<u64>,
}

impl FileConfig {
    pub fn load(path: &Path) -> Result<FileConfig> {
        let text = std::fs::read_to_string(path)?;
        if text.trim().is_empty() {
            return Ok(FileConfig::default());
        }
        let root = json::parse(&text)?;
        if root.as_object().is_none() {
            return Err(crate::error::DbError::syntax(format!(
                "配置文件 {} 顶层必须是对象",
                path.display()
            )));
        }
        // 拼错的键静默忽略最难查，这里逐个点名
        let unknown: Vec<String> = root
            .as_object()
            .map(|items| {
                items
                    .iter()
                    .map(|(key, _)| key.clone())
                    .filter(|key| !KNOWN_KEYS.iter().any(|known| known == key))
                    .collect()
            })
            .unwrap_or_default();
        for key in unknown {
            crate::log::warn(&format!("配置文件里未识别的键 \"{}\"（已忽略）", key));
        }

        // sync 既支持 `"sync": "normal"`，也兼容 `"durableCommits": false`
        let durable = match root.get("sync").and_then(|v| v.as_str()) {
            Some(value) if value.eq_ignore_ascii_case("normal") => Some(false),
            Some(value) if value.eq_ignore_ascii_case("full") => Some(true),
            Some(other) => {
                crate::log::warn(&format!(
                    "配置 sync 只能是 \"full\" 或 \"normal\"，实际 \"{}\"（按 full 处理）",
                    other
                ));
                Some(true)
            }
            None => root.get("durableCommits").and_then(|v| v.as_bool()),
        };

        Ok(FileConfig {
            host: text_of(&root, "host"),
            port: u16_of(&root, "port"),
            http_port: u16_of(&root, "httpPort"),
            no_http: root.get("noHttp").and_then(|v| v.as_bool()),
            token: text_of(&root, "token"),
            max_connections: usize_of(&root, "maxConnections"),
            slow_ms: root.get("slowMs").and_then(|v| v.as_f64()),
            verbose: root.get("verbose").and_then(|v| v.as_bool()),
            durable_commits: durable,
            cors_origin: text_of(&root, "corsOrigin"),
            allow_insecure: root.get("allowInsecure").and_then(|v| v.as_bool()),
            auth_fail_limit: u16_of(&root, "authFailLimit").map(u32::from),
            web_dir: text_of(&root, "webDir"),
            log_file: text_of(&root, "logFile"),
            log_max_bytes: root.get("logMaxBytes").and_then(|v| v.as_i64()).map(|v| v as u64),
        })
    }

    /// 叠加到运行参数上（调用方随后再用命令行覆盖）
    pub fn apply_to(&self, options: &mut ServerOptions) {
        if let Some(host) = &self.host {
            options.host = host.clone();
        }
        if let Some(port) = self.port {
            options.port = port;
        }
        if self.no_http == Some(true) {
            // noHttp 优先级高于 httpPort：两者同时写时按「关掉」处理
            options.http_port = None;
        } else if let Some(port) = self.http_port {
            options.http_port = Some(port);
        }
        if let Some(token) = &self.token {
            if !token.is_empty() {
                options.token = Some(token.clone());
            }
        }
        if let Some(value) = self.max_connections {
            options.max_connections = value;
        }
        if let Some(value) = self.slow_ms {
            options.slow_ms = value;
        }
        if let Some(value) = self.verbose {
            options.verbose = value;
        }
        if let Some(value) = self.durable_commits {
            options.durable_commits = value;
        }
        if let Some(value) = &self.cors_origin {
            options.cors_origin = Some(value.clone());
        }
        if let Some(value) = self.allow_insecure {
            options.allow_insecure = value;
        }
        if let Some(value) = self.auth_fail_limit {
            options.auth_fail_limit = value;
        }
        if let Some(value) = &self.web_dir {
            options.web_dir = PathBuf::from(value);
        }
    }

    pub fn log_path(&self) -> Option<PathBuf> {
        self.log_file
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    pub fn log_max_bytes(&self) -> u64 {
        self.log_max_bytes
            .filter(|value| *value >= 1024)
            .unwrap_or(DEFAULT_MAX_BYTES)
    }
}

/// 找配置文件：显式指定 > 数据目录下的 littledb.json > 当前目录的 littledb.json
pub fn discover(explicit: Option<&Path>, data_dir: &Path) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    let in_data = data_dir.join("littledb.json");
    if in_data.is_file() {
        return Some(in_data);
    }
    let in_cwd = PathBuf::from("littledb.json");
    if in_cwd.is_file() {
        return Some(in_cwd);
    }
    None
}

fn text_of(root: &Json, key: &str) -> Option<String> {
    root.get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

fn u16_of(root: &Json, key: &str) -> Option<u16> {
    root.get(key)
        .and_then(|value| value.as_i64())
        .map(|value| value.clamp(0, u16::MAX as i64) as u16)
}

fn usize_of(root: &Json, key: &str) -> Option<usize> {
    root.get(key)
        .and_then(|value| value.as_i64())
        .map(|value| value.max(1) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(name: &str, text: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("littledb-cfg-{}-{}.json", name, std::process::id()));
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn parses_full_config() {
        let path = write_config(
            "full",
            r#"{
                "host": "0.0.0.0",
                "port": 7901,
                "httpPort": 7902,
                "noHttp": false,
                "token": "abc",
                "maxConnections": 16,
                "slowMs": 250,
                "verbose": true,
                "sync": "normal",
                "corsOrigin": "https://app.example.com",
                "allowInsecure": true,
                "authFailLimit": 9,
                "webDir": "./web",
                "logFile": "./logs/db.log",
                "logMaxBytes": 4096
            }"#,
        );
        let config = FileConfig::load(&path).unwrap();
        let mut options = ServerOptions::default();
        config.apply_to(&mut options);

        assert_eq!(options.host, "0.0.0.0");
        assert_eq!(options.port, 7901);
        assert_eq!(options.http_port, Some(7902));
        assert_eq!(options.token.as_deref(), Some("abc"));
        assert_eq!(options.max_connections, 16);
        assert_eq!(options.slow_ms, 250.0);
        assert!(options.verbose);
        assert!(!options.durable_commits, "sync=normal 应关掉逐次 fsync");
        assert_eq!(options.cors_origin.as_deref(), Some("https://app.example.com"));
        assert!(options.allow_insecure);
        assert_eq!(options.auth_fail_limit, 9);
        assert_eq!(config.log_path(), Some(PathBuf::from("./logs/db.log")));
        assert_eq!(config.log_max_bytes(), 4096);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn no_http_wins_over_port() {
        let path = write_config("nohttp", r#"{"noHttp": true, "httpPort": 8000}"#);
        let config = FileConfig::load(&path).unwrap();
        let mut options = ServerOptions::default();
        config.apply_to(&mut options);
        assert_eq!(options.http_port, None, "noHttp=true 应关掉 HTTP 端口");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_empty_config() {
        let config = FileConfig::load(Path::new("no-such-config.json")).is_err();
        // 不存在 = 调用方负责判断；这里确认「空文件」是合法的
        assert!(config, "路径不存在时应当报错，由调用方决定是否忽略");
        let path = write_config("empty", "   ");
        let config = FileConfig::load(&path).unwrap();
        assert!(config.host.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn log_max_bytes_has_floor() {
        let path = write_config("tiny", r#"{"logMaxBytes": 10}"#);
        let config = FileConfig::load(&path).unwrap();
        assert_eq!(config.log_max_bytes(), DEFAULT_MAX_BYTES, "过小的上限应回落到默认");
        let _ = std::fs::remove_file(&path);
    }
}
