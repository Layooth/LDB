//! 存储层：表文件、页缓存、事务与 WAL 提交。
//!
//! 提交语义与 Node 版一致：
//!   1. 把脏页镜像 + catalog 写进 WAL（一次 fsync）
//!   2. 已提交的页进入 `pending`，checkpoint 时才批量落盘
//!   3. WAL 超过阈值自动 checkpoint
//! 崩溃后只重放带 commit 标记的事务。

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::catalog::Catalog;
use crate::error::{self, DbError, Result};
use crate::json::{self, Json};
use crate::page::{init_page, PAGE_SIZE};
use crate::wal::Wal;

pub type PageKey = (u32, u32);

pub struct TableFile {
    pub file: File,
    pub pages: u32,
}

struct Txn {
    id: i64,
    pages: HashMap<PageKey, Vec<u8>>,
    dirty: HashSet<PageKey>,
    catalog_dirty: bool,
    drops: Vec<u32>,
    page_count_before: HashMap<u32, u32>,
}

pub struct Store {
    pub dir: PathBuf,
    data_dir: PathBuf,
    catalog_path: PathBuf,
    /// 独占锁文件。`close()` 时置空以释放锁，析构时也会自动释放。
    _lock: Option<File>,
    files: HashMap<u32, TableFile>,
    pub catalog: Catalog,
    committed_catalog: Catalog,
    wal: Wal,
    txn: Option<Txn>,
    pending: HashMap<PageKey, Vec<u8>>,
    pending_drops: Vec<u32>,
    catalog_pending: bool,
    wal_limit: u64,
    txn_seq: i64,
    pub needs_recovery: bool,
    pub checkpoints: u64,
    pub recovered_txns: usize,
    pub recovered_records: usize,
    pub orphans_removed: usize,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Store> {
        let dir = dir.to_path_buf();
        let data_dir = dir.join("data");
        fs::create_dir_all(&data_dir)?;

        let catalog_path = dir.join("catalog.json");
        let wal_path = dir.join("wal.log");
        let lock = acquire_lock(&dir)?;

        let mut store = Store {
            dir,
            data_dir,
            catalog_path,
            _lock: Some(lock),
            files: HashMap::new(),
            catalog: Catalog::empty(),
            committed_catalog: Catalog::empty(),
            wal: Wal::open(&wal_path)?,
            txn: None,
            pending: HashMap::new(),
            pending_drops: Vec::new(),
            catalog_pending: false,
            wal_limit: 8 * 1024 * 1024,
            txn_seq: 0,
            needs_recovery: false,
            checkpoints: 0,
            recovered_txns: 0,
            recovered_records: 0,
            orphans_removed: 0,
        };

        store.recover()?;
        store.catalog = Catalog::load(&store.catalog_path)?;
        store.committed_catalog = store.catalog.clone();
        store.orphans_removed = store.cleanup_orphans();
        Ok(store)
    }

    pub fn set_wal_limit(&mut self, bytes: u64) {
        self.wal_limit = bytes;
    }

    pub fn assert_usable(&self) -> Result<()> {
        if self.needs_recovery {
            return Err(DbError::internal(
                "数据库处于待恢复状态（上次提交落盘失败），请重启进程以重放 WAL",
            ));
        }
        Ok(())
    }

    // ------------------------------------------------------------ 恢复

    fn recover(&mut self) -> Result<()> {
        let text = self.wal.read_all()?;
        if !text.is_empty() {
            let mut pending: HashMap<i64, Vec<Json>> = HashMap::new();
            let mut txns = 0usize;
            let mut records = 0usize;

            for line in text.split('\n') {
                if line.is_empty() {
                    continue;
                }
                let record = match json::parse(line) {
                    Ok(value) => value,
                    Err(_) => break,
                };
                let kind = record.get("k").and_then(|v| v.as_str()).unwrap_or("");
                if kind == "commit" {
                    let txn_id = record.get("t").and_then(|v| v.as_i64()).unwrap_or(-1);
                    if let Some(list) = pending.remove(&txn_id) {
                        for item in list {
                            self.apply_record(&item)?;
                            records += 1;
                        }
                        txns += 1;
                    }
                } else if let Some(txn_id) = record.get("t").and_then(|v| v.as_i64()) {
                    pending.entry(txn_id).or_insert_with(Vec::new).push(record);
                }
            }

            self.recovered_txns = txns;
            self.recovered_records = records;
        }
        self.wal.truncate(true)?;
        Ok(())
    }

    fn apply_record(&mut self, record: &Json) -> Result<()> {
        let kind = record.get("k").and_then(|v| v.as_str()).unwrap_or("");
        if kind == "page" {
            let table_id = record.get("tb").and_then(|v| v.as_i64()).unwrap_or(-1) as u32;
            let page_no = record.get("p").and_then(|v| v.as_i64()).unwrap_or(-1) as u32;
            let data = record.get("d").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = json::b64_decode(data)?;
            if bytes.len() != PAGE_SIZE {
                return Err(DbError::internal("WAL 中的页镜像长度非法"));
            }
            self.write_page_raw(table_id, page_no, &bytes)?;
        } else if kind == "catalog" {
            let data = record.get("d").and_then(|v| v.as_str()).unwrap_or("");
            fs::write(&self.catalog_path, data.as_bytes())?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ 文件

    pub fn table_file_path(&self, table_id: u32) -> PathBuf {
        self.data_dir.join(format!("t{}.tbl", table_id))
    }

    fn table_file(&mut self, table_id: u32) -> Result<&mut TableFile> {
        if !self.files.contains_key(&table_id) {
            let path = self.table_file_path(table_id);
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&path)?;
            let size = file.metadata()?.len();
            self.files.insert(
                table_id,
                TableFile {
                    file,
                    pages: (size / PAGE_SIZE as u64) as u32,
                },
            );
        }
        self.files
            .get_mut(&table_id)
            .ok_or_else(|| DbError::internal("表文件打开失败"))
    }

    fn write_page_raw(&mut self, table_id: u32, page_no: u32, data: &[u8]) -> Result<()> {
        let f = self.table_file(table_id)?;
        f.file
            .seek(SeekFrom::Start(page_no as u64 * PAGE_SIZE as u64))?;
        f.file.write_all(data)?;
        if page_no + 1 > f.pages {
            f.pages = page_no + 1;
        }
        Ok(())
    }

    fn read_from_disk(&mut self, table_id: u32, page_no: u32) -> Result<Vec<u8>> {
        let f = self.table_file(table_id)?;
        if page_no >= f.pages {
            return Ok(init_page(page_no));
        }
        let mut buf = vec![0u8; PAGE_SIZE];
        f.file
            .seek(SeekFrom::Start(page_no as u64 * PAGE_SIZE as u64))?;
        f.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// 把 catalog 里没登记的表文件改名隔离（.orphan 后缀），而不是直接删除。
    ///
    /// 直接删除在「catalog 回退到旧版本」的掉电场景下等于丢整张表；
    /// 改名后数据还在磁盘上，可以人工找回。
    fn cleanup_orphans(&mut self) -> usize {
        let known: HashSet<u32> = self.catalog.tables.iter().map(|t| t.id).collect();
        let entries = match fs::read_dir(&self.data_dir) {
            Ok(list) => list,
            Err(_) => return 0,
        };
        let mut removed = 0usize;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let id = match parse_table_file_name(&name) {
                Some(id) => id,
                None => continue,
            };
            if known.contains(&id) {
                continue;
            }
            self.files.remove(&id);
            let mut orphan = entry.file_name();
            orphan.push(".orphan");
            if fs::rename(entry.path(), self.data_dir.join(orphan)).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    // ------------------------------------------------------------ 事务

    pub fn in_txn(&self) -> bool {
        self.txn.is_some()
    }

    pub fn begin(&mut self) -> Result<()> {
        self.assert_usable()?;
        if self.txn.is_some() {
            return Err(DbError::new(
                crate::error::LOCKED,
                "数据库正被其它写事务占用，请稍后重试",
            ));
        }
        self.txn_seq += 1;
        self.txn = Some(Txn {
            id: self.txn_seq,
            pages: HashMap::new(),
            dirty: HashSet::new(),
            catalog_dirty: false,
            drops: Vec::new(),
            page_count_before: HashMap::new(),
        });
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.assert_usable()?;
        let txn = match self.txn.take() {
            Some(t) => t,
            None => return Err(DbError::internal("没有进行中的事务")),
        };

        let empty = txn.dirty.is_empty() && !txn.catalog_dirty;
        if empty {
            return Ok(());
        }

        let mut records: Vec<Json> = Vec::new();
        if txn.catalog_dirty {
            records.push(Json::Obj(vec![
                ("k".to_string(), Json::str("catalog")),
                ("t".to_string(), Json::Num(txn.id as f64)),
                (
                    "d".to_string(),
                    Json::str(self.catalog.to_json().to_string()),
                ),
            ]));
        }
        for key in &txn.dirty {
            if let Some(page) = txn.pages.get(key) {
                records.push(Json::Obj(vec![
                    ("k".to_string(), Json::str("page")),
                    ("t".to_string(), Json::Num(txn.id as f64)),
                    ("tb".to_string(), Json::Num(key.0 as f64)),
                    ("p".to_string(), Json::Num(key.1 as f64)),
                    ("d".to_string(), Json::str(json::b64_encode(page))),
                ]));
            }
        }
        records.push(Json::Obj(vec![
            ("k".to_string(), Json::str("commit")),
            ("t".to_string(), Json::Num(txn.id as f64)),
        ]));

        if let Err(err) = self.wal.append(&records) {
            self.discard_txn(&txn);
            return Err(DbError::internal(format!(
                "提交失败，事务已回滚：{}",
                err.message
            )));
        }

        for key in &txn.dirty {
            if let Some(page) = txn.pages.get(key) {
                self.pending.insert(*key, page.clone());
            }
        }
        if txn.catalog_dirty {
            self.catalog_pending = true;
            self.committed_catalog = self.catalog.clone();
        }
        for id in &txn.drops {
            self.pending_drops.push(*id);
        }

        if self.wal.bytes >= self.wal_limit {
            if let Err(err) = self.checkpoint() {
                self.needs_recovery = true;
                return Err(DbError::internal(format!(
                    "checkpoint 失败（{}），数据库已标记为待恢复",
                    err.message
                )));
            }
        }
        Ok(())
    }

    pub fn rollback(&mut self) -> bool {
        match self.txn.take() {
            Some(txn) => {
                self.discard_txn(&txn);
                true
            }
            None => false,
        }
    }

    fn discard_txn(&mut self, txn: &Txn) {
        for (table_id, count) in &txn.page_count_before {
            if let Some(f) = self.files.get_mut(table_id) {
                f.pages = *count;
            }
        }
        if txn.catalog_dirty {
            self.catalog = self.committed_catalog.clone();
        }
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        if self.txn.is_some() {
            return Err(DbError::internal("不能在事务进行中执行 checkpoint"));
        }
        let pending = std::mem::take(&mut self.pending);
        let mut touched: HashSet<u32> = HashSet::new();
        for ((table_id, page_no), page) in pending.iter() {
            self.write_page_raw(*table_id, *page_no, page)?;
            touched.insert(*table_id);
        }
        for table_id in &touched {
            if let Some(f) = self.files.get(table_id) {
                f.file.sync_all()?;
            }
        }
        if self.catalog_pending {
            self.catalog.save(&self.catalog_path)?;
            self.catalog_pending = false;
        }
        for table_id in std::mem::take(&mut self.pending_drops) {
            self.files.remove(&table_id);
            let _ = fs::remove_file(self.table_file_path(table_id));
        }
        self.wal.truncate(false)?;
        self.checkpoints += 1;
        Ok(())
    }

    // ------------------------------------------------------------ 页访问

    pub fn read_page(&mut self, table_id: u32, page_no: u32) -> Result<Vec<u8>> {
        self.assert_usable()?;
        let key: PageKey = (table_id, page_no);

        if let Some(txn) = &self.txn {
            if let Some(page) = txn.pages.get(&key) {
                return Ok(page.clone());
            }
        }

        if let Some(page) = self.pending.get(&key) {
            let copy = page.clone();
            if let Some(txn) = &mut self.txn {
                txn.pages.insert(key, copy.clone());
            }
            return Ok(copy);
        }

        let page = self.read_from_disk(table_id, page_no)?;
        if let Some(txn) = &mut self.txn {
            txn.pages.insert(key, page.clone());
        }
        Ok(page)
    }

    /// 已提交视图的页读取：跳过活动事务的页缓存。
    ///
    /// 引擎是全局单写锁 + 单一 `store.txn`，其它会话的只读查询不能看到
    /// 未提交事务的脏页（脏读），也不能把自己的读缓存写进别人的事务。
    pub fn read_page_committed(&mut self, table_id: u32, page_no: u32) -> Result<Vec<u8>> {
        self.assert_usable()?;
        if let Some(page) = self.pending.get(&(table_id, page_no)) {
            return Ok(page.clone());
        }
        let f = self.table_file(table_id)?;
        if page_no >= f.pages {
            return Ok(init_page(page_no));
        }
        let mut buf = vec![0u8; PAGE_SIZE];
        f.file
            .seek(SeekFrom::Start(page_no as u64 * PAGE_SIZE as u64))?;
        match f.file.read_exact(&mut buf) {
            Ok(()) => Ok(buf),
            // 页号在内存页数内但磁盘上还没有：那是活动事务刚分配的页，
            // 对已提交视图来说就是空页
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(init_page(page_no)),
            Err(err) => Err(err.into()),
        }
    }

    /// 把修改后的页写回事务缓存并标记为脏页
    pub fn dirty_page(&mut self, table_id: u32, page_no: u32, data: Vec<u8>) -> Result<()> {
        let key: PageKey = (table_id, page_no);
        let txn = match &mut self.txn {
            Some(t) => t,
            None => return Err(DbError::internal("写操作必须在事务内执行")),
        };
        if data.len() != PAGE_SIZE {
            return Err(DbError::internal("页长度必须为 4096 字节"));
        }
        txn.pages.insert(key, data);
        txn.dirty.insert(key);
        Ok(())
    }

    /// 分配新页并初始化为空页
    pub fn alloc_page(&mut self, table_id: u32) -> Result<u32> {
        if self.txn.is_none() {
            return Err(DbError::internal("分配页面必须在事务内执行"));
        }
        self.table_file(table_id)?;
        let current = match self.files.get(&table_id) {
            Some(f) => f.pages,
            None => 0,
        };

        if let Some(f) = self.files.get_mut(&table_id) {
            f.pages = current + 1;
        }
        if let Some(txn) = &mut self.txn {
            txn.page_count_before.entry(table_id).or_insert(current);
            let page = init_page(current);
            txn.pages.insert((table_id, current), page);
            txn.dirty.insert((table_id, current));
        }
        Ok(current)
    }

    /// 页数。文件尚未打开时会先打开（按真实文件大小初始化），
    /// 否则重启后首次扫描会得到 0 页。
    pub fn page_count(&mut self, table_id: u32) -> u32 {
        if !self.files.contains_key(&table_id) {
            let _ = self.table_file(table_id);
        }
        self.files.get(&table_id).map(|f| f.pages).unwrap_or(0)
    }

    // ------------------------------------------------------------ 元数据变更

    pub fn mark_catalog_dirty(&mut self) -> Result<()> {
        match &mut self.txn {
            Some(txn) => {
                txn.catalog_dirty = true;
                Ok(())
            }
            None => Err(DbError::internal("元数据修改必须在事务内执行")),
        }
    }

    pub fn add_drop(&mut self, table_id: u32) -> Result<()> {
        match &mut self.txn {
            Some(txn) => {
                if !txn.drops.contains(&table_id) {
                    txn.drops.push(table_id);
                }
                Ok(())
            }
            None => Err(DbError::internal("删除表必须在事务内执行")),
        }
    }

    pub fn close(&mut self) -> Result<()> {
        if self.txn.is_some() {
            self.rollback();
        }
        let _ = self.checkpoint();
        for (_, f) in self.files.iter() {
            let _ = f.file.sync_all();
        }
        self.files.clear();
        // 释放目录锁，允许之后重新打开
        self._lock = None;
        Ok(())
    }

    pub fn stats(&self) -> StoreStats {
        let mut pages = 0u64;
        for f in self.files.values() {
            pages += f.pages as u64;
        }
        StoreStats {
            tables: self.catalog.tables.len(),
            pages,
            bytes: pages * PAGE_SIZE as u64,
            wal_bytes: self.wal.bytes,
            pending_pages: self.pending.len(),
            checkpoints: self.checkpoints,
        }
    }
}

pub struct StoreStats {
    pub tables: usize,
    pub pages: u64,
    pub bytes: u64,
    pub wal_bytes: u64,
    pub pending_pages: usize,
    pub checkpoints: u64,
}

/// 对数据目录加独占锁。
///
/// 两个进程同时打开同一个目录会互相覆盖页面、直接损坏数据，
/// 所以这里用操作系统级文件锁把第二个进程挡在门外，而不是让它静默写坏。
fn acquire_lock(dir: &Path) -> Result<File> {
    let path = dir.join("littledb.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)?;

    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(DbError::new(
            error::LOCKED,
            format!(
                "数据目录 {} 已被另一个进程打开。LittleDB 不支持多个进程同时访问同一个数据目录，\
                 请先停止占用它的进程",
                dir.display()
            ),
        )),
        Err(std::fs::TryLockError::Error(err)) => Err(DbError::from(err)),
    }
}

fn parse_table_file_name(name: &str) -> Option<u32> {
    if !name.starts_with('t') || !name.ends_with(".tbl") {
        return None;
    }
    let middle = &name[1..name.len() - 4];
    if middle.is_empty() || !middle.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    middle.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("littledb-store-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn page_write_and_read() {
        let dir = temp_dir("a");
        {
            let mut store = Store::open(&dir).unwrap();
            store.begin().unwrap();
            let page_no = store.alloc_page(1).unwrap();
            assert_eq!(page_no, 0);
            let mut page = store.read_page(1, 0).unwrap();
            page[100] = 42;
            store.dirty_page(1, 0, page).unwrap();
            store.commit().unwrap();
            store.checkpoint().unwrap();

            let back = store.read_page(1, 0).unwrap();
            assert_eq!(back[100], 42);
            store.close().unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollback_discards_pages() {
        let dir = temp_dir("b");
        {
            let mut store = Store::open(&dir).unwrap();
            store.begin().unwrap();
            store.alloc_page(1).unwrap();
            assert_eq!(store.page_count(1), 1);
            store.rollback();
            assert_eq!(store.page_count(1), 0);

            // 回滚后重新分配应得到同一个页号
            store.begin().unwrap();
            let page_no = store.alloc_page(1).unwrap();
            assert_eq!(page_no, 0);
            store.commit().unwrap();
            store.close().unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn committed_pages_visible_before_checkpoint() {
        let dir = temp_dir("c");
        {
            let mut store = Store::open(&dir).unwrap();
            store.begin().unwrap();
            store.alloc_page(7).unwrap();
            let mut page = store.read_page(7, 0).unwrap();
            page[10] = 9;
            store.dirty_page(7, 0, page).unwrap();
            store.commit().unwrap();

            // 还没 checkpoint，但新事务必须能读到
            store.begin().unwrap();
            let page = store.read_page(7, 0).unwrap();
            assert_eq!(page[10], 9);
            store.rollback();
            store.close().unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recover_from_wal_after_simulated_crash() {
        let dir = temp_dir("d");
        {
            let mut store = Store::open(&dir).unwrap();
            store.begin().unwrap();
            store.alloc_page(3).unwrap();
            let mut page = store.read_page(3, 0).unwrap();
            page[500] = 77;
            store.dirty_page(3, 0, page).unwrap();
            store.commit().unwrap();
            // 故意不 checkpoint、不 close：模拟崩溃
        }

        // 表 3 必须登记在 catalog 里，否则重启时会被当成孤儿文件清掉
        let catalog = r#"{"version":1,"nextTableId":4,"tables":[{"id":3,"name":"t3","columns":[{"name":"id","type":"INT","notNull":false,"unique":false,"primaryKey":false}],"indexes":[]}]}"#;
        fs::write(dir.join("catalog.json"), catalog).unwrap();

        let mut store = Store::open(&dir).unwrap();
        assert!(store.recovered_txns >= 1, "应从 WAL 恢复出已提交事务");
        assert_eq!(store.orphans_removed, 0, "登记过的表文件不该被清理");
        let page = store.read_page(3, 0).unwrap();
        assert_eq!(page[500], 77);
        store.close().unwrap();

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_process_cannot_open_same_dir() {
        let dir = temp_dir("lock");
        let first = Store::open(&dir).unwrap();

        let second = Store::open(&dir);
        assert!(second.is_err(), "同一目录不应允许两个实例同时打开");
        let message = second.err().unwrap().message;
        assert!(message.contains("已被另一个进程打开"), "{}", message);

        // close 之后锁应释放，可以重新打开
        let mut first = first;
        first.close().unwrap();
        let third = Store::open(&dir);
        assert!(third.is_ok(), "释放锁后应能重新打开");

        drop(third);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn orphan_files_are_removed() {
        let dir = temp_dir("e");
        fs::create_dir_all(dir.join("data")).unwrap();
        fs::write(dir.join("data").join("t99.tbl"), vec![0u8; PAGE_SIZE]).unwrap();

        let store = Store::open(&dir).unwrap();
        assert_eq!(store.orphans_removed, 1);
        assert!(!dir.join("data").join("t99.tbl").exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
