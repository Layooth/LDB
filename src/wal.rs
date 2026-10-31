//! 预写日志，格式是 JSON Lines，与 Node 版完全互通。
//!
//! 记录类型：
//!   {"k":"page","t":1,"tb":2,"p":0,"d":"<base64 页镜像>"}
//!   {"k":"catalog","t":1,"d":"<catalog JSON 字符串>"}
//!   {"k":"commit","t":1}
//!
//! 恢复时只应用「带 commit 标记」的事务，未提交的日志直接丢弃。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::Result;
use crate::json::{self, Json};

pub struct Wal {
    file: File,
    pub bytes: u64,
}

impl Wal {
    pub fn open(path: &Path) -> Result<Wal> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        let bytes = file.metadata()?.len();
        Ok(Wal { file, bytes })
    }

    /// 追加一组记录并 fsync（调用方负责最后附加 commit 记录）
    pub fn append(&mut self, records: &[Json]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut text = String::new();
        for record in records {
            text.push_str(&record.to_string());
            text.push('\n');
        }
        self.file.seek(SeekFrom::Start(self.bytes))?;
        self.file.write_all(text.as_bytes())?;
        self.file.sync_all()?;
        self.bytes += text.len() as u64;
        Ok(())
    }

    /// 清空日志。durable=false 时不 fsync（截断丢失只会导致重放一次，幂等无害）
    pub fn truncate(&mut self, durable: bool) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        if durable {
            self.file.sync_all()?;
        }
        self.bytes = 0;
        Ok(())
    }

    pub fn read_all(&mut self) -> Result<String> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut text = String::new();
        self.file.read_to_string(&mut text)?;
        Ok(text)
    }

    /// 重放已提交事务，返回 (事务数, 记录数)
    pub fn recover<F>(&mut self, mut apply: F) -> Result<(usize, usize)>
    where
        F: FnMut(&Json) -> Result<()>,
    {
        let text = self.read_all()?;
        if text.is_empty() {
            return Ok((0, 0));
        }

        let mut pending: HashMap<i64, Vec<Json>> = HashMap::new();
        let mut txns = 0usize;
        let mut applied = 0usize;

        for line in text.split('\n') {
            if line.is_empty() {
                continue;
            }
            let record = match json::parse(line) {
                Ok(value) => value,
                // 尾部半条日志（崩溃时写了一半），丢弃后续全部
                Err(_) => break,
            };
            let kind = record
                .get("k")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if kind == "commit" {
                let txn_id = record.get("t").and_then(|v| v.as_i64()).unwrap_or(-1);
                if let Some(list) = pending.remove(&txn_id) {
                    for item in list {
                        apply(&item)?;
                        applied += 1;
                    }
                    txns += 1;
                }
            } else {
                match record.get("t").and_then(|v| v.as_i64()) {
                    Some(txn_id) => {
                        pending.entry(txn_id).or_insert_with(Vec::new).push(record);
                    }
                    None => {}
                }
            }
        }

        self.truncate(true)?;
        Ok((txns, applied))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::Json;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("littledb-wal-test-{}-{}.log", name, std::process::id()));
        dir
    }

    #[test]
    fn append_and_recover_committed_only() {
        let path = temp_path("a");
        let _ = std::fs::remove_file(&path);

        {
            let mut wal = Wal::open(&path).unwrap();
            wal.append(&[Json::Obj(vec![
                ("k".to_string(), Json::str("page")),
                ("t".to_string(), Json::Num(1.0)),
                ("d".to_string(), Json::str("AAA")),
            ])])
            .unwrap();
            wal.append(&[Json::Obj(vec![
                ("k".to_string(), Json::str("commit")),
                ("t".to_string(), Json::Num(1.0)),
            ])])
            .unwrap();

            // 事务 2 没有 commit，应被丢弃
            wal.append(&[Json::Obj(vec![
                ("k".to_string(), Json::str("page")),
                ("t".to_string(), Json::Num(2.0)),
                ("d".to_string(), Json::str("BBB")),
            ])])
            .unwrap();
        }

        let mut wal = Wal::open(&path).unwrap();
        let mut applied: Vec<String> = Vec::new();
        let (txns, records) = wal
            .recover(|record| {
                applied.push(record.get("d").unwrap().as_str().unwrap().to_string());
                Ok(())
            })
            .unwrap();

        assert_eq!(txns, 1);
        assert_eq!(records, 1);
        assert_eq!(applied, vec!["AAA".to_string()]);
        assert_eq!(wal.bytes, 0, "恢复后应清空日志");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn truncated_tail_is_ignored() {
        let path = temp_path("b");
        let _ = std::fs::remove_file(&path);

        {
            let mut wal = Wal::open(&path).unwrap();
            wal.append(&[Json::Obj(vec![
                ("k".to_string(), Json::str("commit")),
                ("t".to_string(), Json::Num(9.0)),
            ])])
            .unwrap();
        }
        // 手工追加半条损坏日志
        {
            use std::io::Write;
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(b"{\"k\":\"page\",\"t\":9,\"d\":\"XX").unwrap();
        }

        let mut wal = Wal::open(&path).unwrap();
        let (txns, _) = wal.recover(|_| Ok(())).unwrap();
        assert_eq!(txns, 0);

        let _ = std::fs::remove_file(&path);
    }
}
