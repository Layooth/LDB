//! 预写日志（LDBW2 二进制格式）。
//!
//! ```text
//! 文件头: "LDBW2" + u8 版本(2) + 3 字节保留          （8 字节）
//! 记录:   u8 kind | u32 txn_id | u32 payload_len | payload
//!   kind 1 = page:    u32 table_id | u32 page_no | u32 data_len | data(PAGE_SIZE)
//!   kind 2 = catalog: u32 json_len | JSON 字节
//!   kind 3 = commit:  空
//! ```
//! 所有整数小端，与页/行编码一致。恢复时按 commit 分组重放，
//! 尾部残缺记录（崩溃时写了一半）直接丢弃。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::Result;

const MAGIC: &[u8; 5] = b"LDBW2";
const VERSION: u8 = 2;
const HEADER_LEN: usize = 8;

const KIND_PAGE: u8 = 1;
const KIND_CATALOG: u8 = 2;
const KIND_COMMIT: u8 = 3;

/// 单条记录 payload 上限（页镜像 4KB + 富余；防御损坏文件导致巨额分配）
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum WalRecord {
    Page {
        txn: u32,
        table_id: u32,
        page_no: u32,
        data: Vec<u8>,
    },
    Catalog {
        txn: u32,
        json: String,
    },
    Commit {
        txn: u32,
    },
}

impl WalRecord {
    pub fn txn(&self) -> u32 {
        match self {
            WalRecord::Page { txn, .. }
            | WalRecord::Catalog { txn, .. }
            | WalRecord::Commit { txn } => *txn,
        }
    }
}

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
        let len = file.metadata()?.len();
        let mut wal = Wal { file, bytes: len };

        if len == 0 {
            wal.write_header()?;
            return Ok(wal);
        }
        let mut probe = [0u8; 5];
        wal.file.seek(SeekFrom::Start(0))?;
        wal.file.read_exact(&mut probe)?;
        if &probe != MAGIC {
            return Err(crate::error::DbError::internal(
                "WAL 文件头不合法（不是 LDBW2 格式），请用 verify 检查数据目录",
            ));
        }
        Ok(wal)
    }

    fn write_header(&mut self) -> Result<()> {
        let mut header = [0u8; HEADER_LEN];
        header[..5].copy_from_slice(MAGIC);
        header[5] = VERSION;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        self.bytes = HEADER_LEN as u64;
        Ok(())
    }

    /// 追加一组记录（只写入 OS 缓存，不做 fsync）。
    /// fsync 由 Store 的后台同步线程统一批量完成。
    pub fn append(&mut self, records: &[WalRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut buf: Vec<u8> = Vec::new();
        for record in records {
            encode_record(record, &mut buf);
        }
        self.file.seek(SeekFrom::Start(self.bytes))?;
        self.file.write_all(&buf)?;
        self.bytes += buf.len() as u64;
        Ok(())
    }

    /// 清空日志并重写文件头。durable=false 时不 fsync
    /// （截断丢失只会导致重放一次，幂等无害）
    pub fn truncate(&mut self, durable: bool) -> Result<()> {
        self.file.set_len(0)?;
        self.write_header()?;
        if durable {
            self.file.sync_all()?;
        }
        Ok(())
    }

    pub fn read_all_bytes(&mut self) -> Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut buf = Vec::with_capacity(self.bytes as usize);
        self.file.read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// 重放已提交事务，返回 (事务数, 记录数)。
    /// 只有带 commit 记录的事务会被 apply；尾部残缺内容直接丢弃。
    pub fn recover<F>(&mut self, mut apply: F) -> Result<(usize, usize)>
    where
        F: FnMut(&WalRecord) -> Result<()>,
    {
        let (txns, applied) = self.recover_binary(&mut apply)?;
        // 恢复完成后统一截断成干净的日志
        self.truncate(true)?;
        Ok((txns, applied))
    }

    fn recover_binary<F>(&mut self, apply: &mut F) -> Result<(usize, usize)>
    where
        F: FnMut(&WalRecord) -> Result<()>,
    {
        let buf = self.read_all_bytes()?;
        let mut pending: HashMap<u32, Vec<WalRecord>> = HashMap::new();
        let mut txns = 0usize;
        let mut applied = 0usize;

        let mut pos = HEADER_LEN;
        while pos + 9 <= buf.len() {
            let kind = buf[pos];
            let txn = u32::from_le_bytes(buf[pos + 1..pos + 5].try_into().unwrap());
            let payload_len =
                u32::from_le_bytes(buf[pos + 5..pos + 9].try_into().unwrap()) as usize;
            if payload_len > MAX_PAYLOAD || pos + 9 + payload_len > buf.len() {
                // 残缺尾部（崩溃时写了一半），连同后续内容一起丢弃
                break;
            }
            let payload = &buf[pos + 9..pos + 9 + payload_len];
            let record = match decode_payload(kind, txn, payload) {
                Some(record) => record,
                None => break, // 未知 kind / payload 损坏：保守起见停止重放
            };
            pos += 9 + payload_len;

            if let WalRecord::Commit { txn } = record {
                if let Some(list) = pending.remove(&txn) {
                    for item in &list {
                        apply(item)?;
                        applied += 1;
                    }
                    txns += 1;
                }
            } else {
                pending.entry(txn).or_insert_with(Vec::new).push(record);
            }
        }
        Ok((txns, applied))
    }
}

fn encode_record(record: &WalRecord, out: &mut Vec<u8>) {
    // 先构建 payload，再按实际长度写头，避免两处长度不一致
    let mut payload: Vec<u8> = Vec::new();
    let kind = match record {
        WalRecord::Page {
            table_id,
            page_no,
            data,
            ..
        } => {
            payload.extend_from_slice(&table_id.to_le_bytes());
            payload.extend_from_slice(&page_no.to_le_bytes());
            payload.extend_from_slice(&(data.len() as u32).to_le_bytes());
            payload.extend_from_slice(data);
            KIND_PAGE
        }
        WalRecord::Catalog { json, .. } => {
            payload.extend_from_slice(&(json.len() as u32).to_le_bytes());
            payload.extend_from_slice(json.as_bytes());
            KIND_CATALOG
        }
        WalRecord::Commit { .. } => KIND_COMMIT,
    };
    let txn = record.txn();
    out.push(kind);
    out.extend_from_slice(&txn.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
}

fn decode_payload(kind: u8, txn: u32, payload: &[u8]) -> Option<WalRecord> {
    match kind {
        KIND_PAGE => {
            if payload.len() < 12 {
                return None;
            }
            let table_id = u32::from_le_bytes(payload[0..4].try_into().ok()?);
            let page_no = u32::from_le_bytes(payload[4..8].try_into().ok()?);
            let data_len = u32::from_le_bytes(payload[8..12].try_into().ok()?) as usize;
            if payload.len() != 12 + data_len {
                return None;
            }
            Some(WalRecord::Page {
                txn,
                table_id,
                page_no,
                data: payload[12..].to_vec(),
            })
        }
        KIND_CATALOG => {
            if payload.len() < 4 {
                return None;
            }
            let json_len = u32::from_le_bytes(payload[0..4].try_into().ok()?) as usize;
            if payload.len() != 4 + json_len {
                return None;
            }
            let text = String::from_utf8(payload[4..].to_vec()).ok()?;
            Some(WalRecord::Catalog { txn, json: text })
        }
        KIND_COMMIT => {
            if !payload.is_empty() {
                return None;
            }
            Some(WalRecord::Commit { txn })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("littledb-wal-test-{}-{}", name, std::process::id()));
        dir
    }

    fn page_record(txn: u32, data: &[u8]) -> WalRecord {
        WalRecord::Page {
            txn,
            table_id: 7,
            page_no: 3,
            data: data.to_vec(),
        }
    }

    #[test]
    fn append_and_recover_committed_only() {
        let path = temp_path("a");
        let _ = std::fs::remove_file(&path);

        {
            let mut wal = Wal::open(&path).unwrap();
            wal.append(&[
                page_record(1, b"AAAA"),
                WalRecord::Catalog {
                    txn: 1,
                    json: "{}".to_string(),
                },
                WalRecord::Commit { txn: 1 },
            ])
            .unwrap();
            // 事务 2 没有 commit，应被丢弃
            wal.append(&[page_record(2, b"BBBB")]).unwrap();
        }

        let mut wal = Wal::open(&path).unwrap();
        let mut applied: Vec<WalRecord> = Vec::new();
        let (txns, records) = wal
            .recover(|record| {
                applied.push(record.clone());
                Ok(())
            })
            .unwrap();

        assert_eq!(txns, 1);
        assert_eq!(records, 2);
        assert_eq!(
            applied,
            vec![
                page_record(1, b"AAAA"),
                WalRecord::Catalog {
                    txn: 1,
                    json: "{}".to_string(),
                },
            ]
        );
        assert_eq!(wal.bytes, HEADER_LEN as u64, "恢复后应只剩文件头");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn truncated_tail_is_ignored() {
        let path = temp_path("b");
        let _ = std::fs::remove_file(&path);

        {
            let mut wal = Wal::open(&path).unwrap();
            wal.append(&[
                page_record(9, b"CCCC"),
                WalRecord::Commit { txn: 9 },
            ])
            .unwrap();
        }
        // 手工追加残缺记录（声明了 payload 但没写完）
        {
            use std::io::Write;
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            let mut rec = Vec::new();
            rec.push(KIND_PAGE);
            rec.extend_from_slice(&9u32.to_le_bytes());
            rec.extend_from_slice(&4096u32.to_le_bytes());
            rec.extend_from_slice(b"partial");
            file.write_all(&rec).unwrap();
        }

        let mut wal = Wal::open(&path).unwrap();
        let (txns, _) = wal.recover(|_| Ok(())).unwrap();
        assert_eq!(txns, 1, "完整的第一个事务应恢复");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let path = temp_path("badmagic");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"{\"k\":\"page\"}").unwrap();
        assert!(Wal::open(&path).is_err(), "非 LDBW2 文件应被拒绝");
        let _ = std::fs::remove_file(&path);
    }
}
