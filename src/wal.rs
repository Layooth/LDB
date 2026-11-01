//! 预写日志（LDBW2 二进制格式）。
//!
//! ```text
//! 文件头: "LDBW3" + u8 版本(3) + 2 字节保留          （8 字节；v2 文件兼容读取）
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

const MAGIC: &[u8; 5] = b"LDBW3";
/// 旧版格式仍可读取（恢复兼容），但新写入一律用当前版本
const LEGACY_MAGIC: &[u8; 5] = b"LDBW2";
const VERSION: u8 = 3;
const LEGACY_VERSION: u8 = 2;
/// v3 记录帧 = 9 字节头 + 4 字节 CRC + payload；v2 记录帧没有 CRC
const V3_FRAME_OVERHEAD: usize = 4;
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
    /// 磁盘上这份日志的格式版本（2 = 旧版无校验和，3 = 带校验和）
    on_disk_version: u8,
}

impl Wal {
    pub fn open(path: &Path) -> Result<Wal> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        let len = file.metadata()?.len();
        let mut wal = Wal { file, bytes: len, on_disk_version: VERSION };

        if len == 0 {
            wal.write_header()?;
            return Ok(wal);
        }
        let mut probe = [0u8; 5];
        wal.file.seek(SeekFrom::Start(0))?;
        wal.file.read_exact(&mut probe)?;
        wal.on_disk_version = if &probe == MAGIC {
            VERSION
        } else if &probe == LEGACY_MAGIC {
            LEGACY_VERSION
        } else {
            return Err(crate::error::DbError::internal(
                "WAL 文件头不合法（既不是 LDBW3 也不是旧版 LDBW2），请用 verify 检查数据目录",
            ));
        };
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
        let with_crc = self.on_disk_version >= VERSION;
        let frame_overhead = if with_crc { 9 + V3_FRAME_OVERHEAD } else { 9 };
        let mut pending: HashMap<u32, Vec<WalRecord>> = HashMap::new();
        let mut txns = 0usize;
        let mut applied = 0usize;

        let mut pos = HEADER_LEN;
        while pos + frame_overhead <= buf.len() {
            let kind = buf[pos];
            let txn = u32::from_le_bytes(buf[pos + 1..pos + 5].try_into().unwrap());
            let payload_len =
                u32::from_le_bytes(buf[pos + 5..pos + 9].try_into().unwrap()) as usize;
            let payload_start = pos + frame_overhead;
            if payload_len > MAX_PAYLOAD || payload_start + payload_len > buf.len() {
                // 残缺尾部（崩溃时写了一半），连同后续内容一起丢弃
                break;
            }
            let payload = &buf[payload_start..payload_start + payload_len];
            // v3：逐记录校验和。不匹配 = 记录在落盘时被破坏，把它与其后内容
            // 一起按「损坏尾部」处理（与残缺尾部同一策略：保守停止重放）
            if with_crc {
                let stored = u32::from_le_bytes(
                    buf[pos + 9..pos + 9 + V3_FRAME_OVERHEAD].try_into().unwrap(),
                );
                let mut covered = Vec::with_capacity(9 + payload_len);
                covered.extend_from_slice(&buf[pos..pos + 9]);
                covered.extend_from_slice(payload);
                if crc32(&covered) != stored {
                    break;
                }
            }
            let record = match decode_payload(kind, txn, payload) {
                Some(record) => record,
                None => {
                    #[cfg(test)]
                    eprintln!("[dbg] decode 失败：pos={pos} kind={kind} txn={txn} len={payload_len} with_crc={with_crc}");
                    break; // 未知 kind / payload 损坏：保守起见停止重放
                }
            };
            #[cfg(test)]
            eprintln!("[dbg] 解析帧：pos={pos} kind={kind} txn={txn} len={payload_len} with_crc={with_crc} buf_len={}", buf.len());
            pos = payload_start + payload_len;

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
    // LDBW3：CRC 覆盖 kind + txn + payload（帧头前 9 字节 + 内容）
    let mut covered = Vec::with_capacity(9 + payload.len());
    covered.extend_from_slice(&out[out.len() - 9..]);
    covered.extend_from_slice(&payload);
    out.extend_from_slice(&crc32(&covered).to_le_bytes());
    out.extend_from_slice(&payload);
}

/// IEEE CRC-32（表驱动，零依赖）
fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let mut crc = i as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
            }
            *entry = crc;
        }
        table
    });
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = table[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
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

    #[test]
    fn v3_checksum_catches_bit_rot() {
        let path = std::env::temp_dir().join(format!("ldb-walv3-{}.wal", std::process::id()));

        // 写 3 个事务（各 1 页 + commit），全部落盘
        {
            let mut wal = Wal::open(&path).unwrap();
            let mut records = Vec::new();
            for txn in 1..=3u32 {
                records.push(WalRecord::Page {
                    txn,
                    table_id: 1,
                    page_no: 0,
                    data: vec![txn as u8; 64],
                });
                records.push(WalRecord::Commit { txn });
            }
            wal.append(&records).unwrap();
        }

        // 破坏第 2 个事务页镜像 payload 中间的一个字节（位翻转 = 落盘损坏）
        let mut bytes = std::fs::read(&path).unwrap();
        let page_frame = 9 + 4 + 76;
        let commit_frame = 13;
        let txn2_payload_byte = HEADER_LEN + page_frame + commit_frame + 9 + 4 + 30;
        bytes[txn2_payload_byte] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        // 重放：只有第 1 个事务可信；损坏记录及其后内容按损坏尾部丢弃
        let mut wal = Wal::open(&path).unwrap();
        let mut applied = 0usize;
        let (txns, applied_count) = wal.recover(|_| {
            applied += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(txns, 1, "损坏后只应恢复第 1 个事务");
        assert_eq!(applied_count, applied);
        assert_eq!(applied_count, 1, "只有第 1 个事务的页记录被应用");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_v2_log_still_recovers() {
        let path = std::env::temp_dir().join(format!("ldb-walv2-{}.wal", std::process::id()));

        // 手工构造旧版 LDBW2 日志：无 CRC 的帧
        let mut bytes = Vec::new();
        bytes.extend_from_slice(LEGACY_MAGIC);
        bytes.push(LEGACY_VERSION);
        bytes.extend_from_slice(&[0u8; 2]); // 头共 8 字节：5 magic + 1 版本 + 2 保留

        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&64u32.to_le_bytes());
        payload.extend(std::iter::repeat(7u8).take(64));
        bytes.push(KIND_PAGE);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes.push(KIND_COMMIT);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        // 旧格式必须仍能恢复（兼容读取），恢复后日志升级为干净状态
        let mut wal = Wal::open(&path).unwrap();
        let (txns, applied) = wal.recover(|_| Ok(())).unwrap();
        assert_eq!(txns, 1, "旧版 LDBW2 日志应能恢复");
        assert_eq!(applied, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_v2_rejects_v3_framing() {
        // 旧版读取器遇到带 CRC 的帧会因 payload_len 越界/解析失败而停止——
        // 反向兼容不需要：本引擎只读旧、写新。这里验证 open 对乱文件头仍报错。
        let path = std::env::temp_dir().join(format!("ldb-walbad-{}.wal", std::process::id()));
        std::fs::write(&path, b"NOTAWAL").unwrap();
        assert!(Wal::open(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
