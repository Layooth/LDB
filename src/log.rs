//! 极简日志（只用 std）。
//!
//! - 没配置日志文件时写 stdout —— 保持原来的行为，终端里能直接看；
//! - 配置了文件则**追加**写，超过 `max_bytes` 轮转一次（`app.log` → `app.log.1`）；
//! - 时间戳是 UTC（与 catalog 里的 `createdAt` 一致），带 `Z` 后缀避免被当成本地时间。
//!
//! 线程安全：内部一个 `Mutex<Inner>`，日志量本身很小，不做无锁优化。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 默认单文件上限 8MB，超过就把旧内容挪到 `.1`（只保留一份备份）
pub const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;

struct Inner {
    /// None = 文件句柄已释放（轮转中或打开失败）
    file: Option<File>,
    written: u64,
}

pub struct Sink {
    inner: Mutex<Inner>,
    path: PathBuf,
    max_bytes: u64,
}

impl Sink {
    pub fn new(path: &Path, max_bytes: u64) -> std::io::Result<Sink> {
        let file = open_append(path)?;
        let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Sink {
            inner: Mutex::new(Inner {
                file: Some(file),
                written,
            }),
            path: path.to_path_buf(),
            max_bytes: max_bytes.max(1024),
        })
    }

    pub fn append(&self, line: &str) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.written + line.len() as u64 > self.max_bytes {
            rotate(&self.path, &mut inner);
        }
        if let Some(file) = inner.file.as_mut() {
            if file.write_all(line.as_bytes()).is_ok() {
                let _ = file.flush();
                inner.written += line.len() as u64;
            }
        }
    }
}

/// 轮转：关句柄 → 旧文件改名 `.1`（覆盖旧的备份）→ 重开
fn rotate(path: &Path, inner: &mut Inner) {
    inner.file = None; // 先释放句柄，否则 Windows 上 rename 会失败
    let backup = PathBuf::from(format!("{}.1", path.display()));
    let _ = std::fs::remove_file(&backup);
    let _ = std::fs::rename(path, &backup);
    match open_append(path) {
        Ok(file) => {
            inner.file = Some(file);
            inner.written = 0;
        }
        Err(err) => {
            // 打不开就暂时丢日志，但要让用户知道（打到 stderr，避免递归）
            eprintln!("[LittleDB] 日志文件重建失败（{}）：{}", path.display(), err);
        }
    }
}

fn open_append(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    OpenOptions::new().create(true).append(true).open(path)
}

static SINK: OnceLock<Sink> = OnceLock::new();

/// 启用文件日志。返回值用于把「日志写到哪」告诉用户。
pub fn init_file(path: &Path, max_bytes: u64) -> std::io::Result<()> {
    let sink = Sink::new(path, max_bytes)?;
    let _ = SINK.set(sink);
    Ok(())
}

pub fn info(message: &str) {
    write("INFO", message);
}

pub fn warn(message: &str) {
    write("WARN", message);
}

pub fn error(message: &str) {
    write("ERROR", message);
}

fn write(level: &str, message: &str) {
    let line = format!("{} [{}] {}\n", timestamp(), level, message);
    match SINK.get() {
        Some(sink) => sink.append(&line),
        // 没配文件：保持原来的 stdout 行为（服务一般由控制台/启动器带起来）
        None => print!("{}", line),
    }
}

fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    format!("{}Z", crate::codec::format_datetime_from_millis(millis))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("littledb-log-{}-{}.log", name, std::process::id()));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}.1", path.display()));
        path
    }

    #[test]
    fn writes_timestamped_lines() {
        let path = temp_file("basic");
        let sink = Sink::new(&path, DEFAULT_MAX_BYTES).unwrap();
        sink.append("2026-10-31 00:00:00Z [INFO] hello\n");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[INFO] hello"), "{}", text);
        assert!(text.ends_with('\n'));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rotates_when_exceeding_limit() {
        let path = temp_file("rotate");
        // 上限最小 1KB（防止配成 1 字节时每行都轮转）
        let limit = 1024u64;
        let sink = Sink::new(&path, limit).unwrap();
        for i in 0..60 {
            sink.append(&format!("2026-10-31 00:00:00Z [INFO] line {}\n", i));
        }
        let backup = PathBuf::from(format!("{}.1", path.display()));
        assert!(backup.exists(), "超过上限应轮转出 .1");
        let current = std::fs::metadata(&path).unwrap().len();
        assert!(
            current <= limit,
            "轮转后当前文件应不超过上限 {}，实际 {}",
            limit,
            current
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
    }

    #[test]
    fn appends_to_existing_file() {
        let path = temp_file("append");
        std::fs::write(&path, "旧内容\n").unwrap();
        {
            let sink = Sink::new(&path, DEFAULT_MAX_BYTES).unwrap();
            sink.append("新内容\n");
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("旧内容\n"), "不应截断已有日志：{}", text);
        assert!(text.contains("新内容"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn timestamp_has_utc_suffix() {
        let text = timestamp();
        assert!(text.ends_with('Z'), "{}", text);
        assert_eq!(text.len(), 20, "YYYY-MM-DD HH:MM:SSZ：{}", text);
    }
}
