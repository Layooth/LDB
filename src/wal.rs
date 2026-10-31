//! 预写日志。
//!
//! ## 当前格式（LDBW2，二进制）
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
//!
//! ## 兼容性
//!
//! 恢复同时支持旧版 JSON Lines 格式（Node 版 1.0 写的，以 `{` 开头），
//! 但本版本新写入一律是二进制。**Node 版 1.0 无法重放 LDBW2 记录**——
//! 把数据目录交给 Node 版之前请先 checkpoint（干净关闭即可），或给 Node
//! 版同步移植本格式。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::Result;
use crate::json;

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
    /// 打开时是 JSON 旧格式（恢复前不能按二进制扫描）
    legacy: bool,
}

impl Wal {
    pub fn open(path: &Path) -> Result<Wal> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        let len = file.metadata()?.len();
        let mut wal = Wal {
            file,
            bytes: len,
            legacy: false,
        };

        if len == 0 {
            wal.write_header()?;
            return Ok(wal);
        }
        let mut probe = [0u8; 5];
        wal.file.seek(SeekFrom::Start(0))?;
        wal.file.read_exact(&mut probe)?;
        if &probe == MAGIC {
            Ok(wal)
        } else {
            // 旧版 JSON Lines：恢复流程会读完并 truncate，届时转成二进制
            wal.legacy = true;
            Ok(wal)
        }
    }

    fn write_header(&mut self) -> Result<()> {
        let mut header = [0u8; HEADER_LEN];
        header[..5].copy_from_slice(MAGIC);
        header[5] = VERSION;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        self.bytes = HEADER_LEN as u64;
        self.legacy = false;
        Ok(())
    }

    /// 追加一组记录并 fsync（调用方负责最后附加 commit 记录）
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
        self.file.sync_all()?;
        self.bytes += buf.len() as u64;
        Ok(())
    }

    /// 清空日志并重写二进制头。durable=false 时不 fsync
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

    /// 重放已提交事务，返回 (事务数, 记录数)
    ///
    /// 兼容两种文件格式：LDBW2 二进制与旧版 JSON Lines。
    /// 只有带 commit 记录的事务会被 apply；尾部残缺内容直接丢弃。
    pub fn recover<F>(&mut self, mut apply: F) -> Result<(usize, usize)>
    where
        F: FnMut(&WalRecord) -> Result<()>,
    {
        let (txns, applied) = if self.legacy {
            self.recover_legacy(&mut apply)?
        } else {
            self.recover_binary(&mut apply)?
        };
        // 恢复完成后统一截断成干净的二进制日志
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

    fn recover_legacy<F>(&mut self, apply: &mut F) -> Result<(usize, usize)>
    where
        F: FnMut(&WalRecord) -> Result<()>,
    {
        let mut text = String::new();
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_to_string(&mut text)?;
        if text.is_empty() {
            return Ok((0, 0));
        }

        let mut pending: HashMap<i64, Vec<WalRecord>> = HashMap::new();
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
            let kind = record.get("k").and_then(|v| v.as_str()).unwrap_or("");
            let txn = record.get("t").and_then(|v| v.as_i64()).unwrap_or(-1);
            let record = match kind {
                "commit" => WalRecord::Commit { txn: txn as u32 },
                "page" => {
                    let table_id = record.get("tb").and_then(|v| v.as_i64()).unwrap_or(-1) as u32;
                    let page_no = record.get("p").and_then(|v| v.as_i64()).unwrap_or(-1) as u32;
                    let data = record.get("d").and_then(|v| v.as_str()).unwrap_or("");
                    let bytes = json::b64_decode(data)?;
                    WalRecord::Page {
                        txn: txn as u32,
                        table_id,
                        page_no,
                        data: bytes,
                    }
                }
                "catalog" => {
                    let data = record.get("d").and_then(|v| v.as_str()).unwrap_or("");
                    WalRecord::Catalog {
                        txn: txn as u32,
                        json: data.to_string(),
                    }
                }
                _ => continue,
            };

            if let WalRecord::Commit { txn } = record {
                if let Some(list) = pending.remove(&(txn as i64)) {
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
        assert!(!wal.legacy);

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
    fn recovers_legacy_json_wal() {
        let path = temp_path("legacy");
        let _ = std::fs::remove_file(&path);

        // 模拟 Node 版 1.0 写出的 JSON Lines WAL（无 LDBW2 头）
        let page_b64 = json::b64_encode(b"DDDD");
        std::fs::write(
            &path,
            format!(
                "{{\"k\":\"page\",\"t\":4,\"tb\":7,\"p\":3,\"d\":\"{}\"}}\n{{\"k\":\"commit\",\"t\":4}}\n",
                page_b64
            ),
        )
        .unwrap();

        let mut wal = Wal::open(&path).unwrap();
        assert!(wal.legacy);
        let mut applied = Vec::new();
        let (txns, records) = wal
            .recover(|record| {
                applied.push(record.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(txns, 1);
        assert_eq!(records, 1);
        assert_eq!(applied[0], page_record(4, b"DDDD"));
        // 恢复后已转成二进制格式
        assert!(!wal.legacy);
        let head = wal.read_all_bytes().unwrap();
        assert_eq!(&head[..5], MAGIC);

        let _ = std::fs::remove_file(&path);
    }
}
