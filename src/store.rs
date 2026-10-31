//! 存储层：表文件、页缓存、事务与 WAL 提交。
//!
//! 提交协议：
//!   1. 把脏页镜像 + catalog 写进 WAL（一次 fsync）
//!   2. 已提交的页进入 `pending`，checkpoint 时才批量落盘
//!   3. WAL 超过阈值自动 checkpoint
//! 崩溃后只重放带 commit 标记的事务。

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use crate::catalog::Catalog;
use crate::error::{self, DbError, Result};
use crate::page::{init_page, PAGE_SIZE};
use crate::wal::{Wal, WalRecord};

pub type PageKey = (u32, u32);

/// 提交持久化屏障：写路径只负责把 WAL 字节落进 OS 缓存并发布序号，
/// fsync 由后台线程批量完成；full 模式下调用方在【释放引擎写锁之后】
/// 等待自己的序号被确认，从而把 fsync 从临界区里挪出去（组提交）。
pub struct CommitBarrier {
    state: Mutex<BarrierState>,
    cv: Condvar,
}

struct BarrierState {
    last_written: u64,
    last_synced: u64,
    error: Option<String>,
    shutting_down: bool,
}

impl CommitBarrier {
    fn new() -> CommitBarrier {
        CommitBarrier {
            state: Mutex::new(BarrierState {
                last_written: 0,
                last_synced: 0,
                error: None,
                shutting_down: false,
            }),
            cv: Condvar::new(),
        }
    }

    /// 提交线程发布「已写入 OS 缓存」的新序号
    fn publish(&self, seq: u64) {
        let mut state = self.state.lock().unwrap();
        if seq > state.last_written {
            state.last_written = seq;
        }
        drop(state);
        self.cv.notify_all();
    }

    /// 后台线程确认 fsync 覆盖到 seq（保守：只标记它醒来时看到的快照）
    fn mark_synced(&self, seq: u64, err: Option<String>) {
        let mut state = self.state.lock().unwrap();
        if err.is_some() {
            state.error = err;
        }
        if seq > state.last_synced {
            state.last_synced = seq;
        }
        drop(state);
        self.cv.notify_all();
    }

    /// 等待指定序号持久化完成。full 模式下「响应返回 ⇒ 已 fsync」由此保证。
    /// 带超时兜底，避免极端丢通知时永久挂起。
    pub fn wait_for(&self, seq: u64) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(err) = &state.error {
                return Err(crate::error::DbError::internal(format!(
                    "WAL fsync 失败，最近的提交可能未持久化，请重启确认状态：{}",
                    err
                )));
            }
            if state.last_synced >= seq {
                return Ok(());
            }
            if state.shutting_down {
                return Err(crate::error::DbError::internal("数据库正在关闭，提交未确认持久化"));
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(crate::error::DbError::internal(
                    "等待 WAL fsync 确认超时，请重试或重启",
                ));
            }
            let (guard, _) = self
                .cv
                .wait_timeout(state, Duration::from_millis(200))
                .unwrap();
            state = guard;
        }
    }

    /// 等待全部已写入的提交落盘（checkpoint / close 前 drain）
    fn wait_all(&self) -> Result<()> {
        let seq = self.state.lock().unwrap().last_written;
        self.wait_for(seq)
    }

    fn error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }

    fn shutdown(&self) {
        self.state.lock().unwrap().shutting_down = true;
        self.cv.notify_all();
    }

    /// 后台线程：阻塞直到有新工作或关闭；返回 (快照序号, 是否关闭中)
    fn wait_for_work(&self, synced_upto: u64) -> (u64, bool) {
        let mut state = self.state.lock().unwrap();
        while state.last_written == synced_upto && !state.shutting_down {
            state = self.cv.wait(state).unwrap();
        }
        (state.last_written, state.shutting_down)
    }

    fn drained(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.last_synced >= state.last_written
    }
}

pub struct TableFile {
    pub file: File,
    pub pages: u32,
}

pub struct Txn {
    id: i64,
    /// 事务开始时的提交序号（快照）：之后提交的内容对本事务不可见
    snapshot: u64,
    pages: HashMap<PageKey, Vec<u8>>,
    dirty: HashSet<PageKey>,
    catalog_dirty: bool,
    drops: Vec<u32>,
    page_count_before: HashMap<u32, u32>,
    /// 延迟索引 delta：语句期记账，提交时由 Engine 统一应用
    pub index_ops: Vec<IndexOp>,
    row_deltas: HashMap<u32, i64>,
}

/// 一次提交产生的页集合（按 seq 有序存放，checkpoint 时按水位线落盘）
struct CommittedPages {
    seq: u64,
    pages: HashMap<PageKey, Vec<u8>>,
}

/// 一条语句对内存索引的修改记录（回滚/冲突时按逆序精确撤销，
/// 避免全量重建抹掉其它开放事务的未提交索引状态）
#[derive(Debug, Clone)]
pub enum IndexOp {
    Add {
        table_id: u32,
        index_pos: usize,
        key: String,
        row_id: u32,
    },
    Remove {
        table_id: u32,
        index_pos: usize,
        key: String,
        row_id: u32,
    },
}

/// 回滚时交给引擎的撤销信息
pub struct RollbackInfo {
    pub index_ops: Vec<IndexOp>,
    /// 表 id → 行数增量（插入为正、删除为负）
    pub row_deltas: HashMap<u32, i64>,
    pub catalog_dirty: bool,
}

pub struct Store {
    pub dir: PathBuf,
    data_dir: PathBuf,
    catalog_path: PathBuf,
    /// 独占锁文件。`close()` 时置空以释放锁，析构时也会自动释放。
    _lock: Option<File>,
    files: HashMap<u32, TableFile>,
    /// 只读文件句柄：与写句柄分离，配合定位读（seek_read/read_at），
    /// 多个读线程并发读同一表文件不会互相干扰文件位置
    read_handles: HashMap<u32, File>,
    /// 当前写事务的属主（会话 id）；读路径据此判断能否看到未提交页
    txn_owner: Option<u64>,
    /// 每会话一个开放事务（P1 阶段由单写者门禁保证同时最多一个）
    txns: HashMap<u64, Txn>,
    /// 已提交未落盘的页集合，按 seq 升序
    committed: Vec<CommittedPages>,
    pub catalog: Catalog,
    committed_catalog: Catalog,
    wal: Wal,
    pending_drops: Vec<u32>,
    catalog_pending: bool,
    wal_limit: u64,
    txn_seq: i64,
    /// 已提交（字节已写入 WAL）的事务序号
    commit_seq: u64,
    /// full 模式下调用方在锁外等待 fsync 确认；normal 模式不等待
    durable_commits: bool,
    commit_barrier: Arc<CommitBarrier>,
    sync_thread: Option<thread::JoinHandle<()>>,
    sync_thread_started: AtomicBool,
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
            read_handles: HashMap::new(),
            txn_owner: None,
            catalog: Catalog::empty(),
            committed_catalog: Catalog::empty(),
            wal: Wal::open(&wal_path)?,
            txns: HashMap::new(),
            committed: Vec::new(),
            pending_drops: Vec::new(),
            catalog_pending: false,
            wal_limit: 8 * 1024 * 1024,
            txn_seq: 0,
            commit_seq: 0,
            durable_commits: true,
            commit_barrier: Arc::new(CommitBarrier::new()),
            sync_thread: None,
            sync_thread_started: AtomicBool::new(false),
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
        // 只读句柄就绪后，读路径（RwLock 读锁）不再需要惰性打开文件
        let table_ids: Vec<u32> = store.catalog.tables.iter().map(|d| d.id).collect();
        for id in table_ids {
            let _ = store.table_file(id);
        }
        store.spawn_sync_thread();
        Ok(store)
    }

    /// 后台 WAL 同步线程：把 fsync 从引擎写锁里挪出去。
    /// 使用独立的文件句柄做 sync_all，与写路径互不干扰。
    fn spawn_sync_thread(&mut self) {
        if self.sync_thread_started.swap(true, AtomicOrdering::SeqCst) {
            return;
        }
        let barrier = Arc::clone(&self.commit_barrier);
        let sync_handle = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.dir.join("wal.log"))
        {
            Ok(handle) => handle,
            Err(_) => return, // 打不开就不做后台同步；full 模式的等待会超时报错
        };
        let handle = thread::Builder::new()
            .name("ldb-wal-sync".to_string())
            .spawn(move || {
                let mut synced_upto = 0u64;
                loop {
                    let (target, shutting) = barrier.wait_for_work(synced_upto);
                    if target > synced_upto {
                        let res = sync_handle.sync_all();
                        barrier.mark_synced(
                            target,
                            res.err().map(|e| e.to_string()),
                        );
                        synced_upto = target;
                    }
                    if shutting && barrier.drained() {
                        break;
                    }
                }
            });
        self.sync_thread = handle.ok();
    }

    pub fn set_wal_limit(&mut self, bytes: u64) {
        self.wal_limit = bytes;
    }

    /// 提交耐久性：true = full，调用方等待 fsync 确认后才返回（默认，掉电安全）；
    /// false = normal，立即返回，后台线程异步 fsync（掉电可能丢最近提交）
    pub fn set_sync_mode(&mut self, durable: bool) {
        self.durable_commits = durable;
    }

    pub fn durable_commits(&self) -> bool {
        self.durable_commits
    }

    pub fn last_commit_seq(&self) -> u64 {
        self.commit_seq
    }

    pub fn commit_barrier(&self) -> Arc<CommitBarrier> {
        Arc::clone(&self.commit_barrier)
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
        let mut replayed: Vec<WalRecord> = Vec::new();
        let (txns, records) = self.wal.recover(|record| {
            replayed.push(record.clone());
            Ok(())
        })?;
        for record in &replayed {
            self.apply_record(record)?;
        }
        self.recovered_txns = txns;
        self.recovered_records = records;
        Ok(())
    }

    fn apply_record(&mut self, record: &WalRecord) -> Result<()> {
        match record {
            WalRecord::Page {
                table_id,
                page_no,
                data,
                ..
            } => {
                if data.len() != PAGE_SIZE {
                    return Err(DbError::internal("WAL 中的页镜像长度非法"));
                }
                self.write_page_raw(*table_id, *page_no, data)?;
            }
            WalRecord::Catalog { json, .. } => {
                fs::write(&self.catalog_path, json.as_bytes())?;
            }
            WalRecord::Commit { .. } => {}
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
            let read_handle = OpenOptions::new().read(true).open(&path)?;
            self.read_handles.insert(table_id, read_handle);
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
            self.read_handles.remove(&id);
            let mut orphan = entry.file_name();
            orphan.push(".orphan");
            if fs::rename(entry.path(), self.data_dir.join(orphan)).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    // ------------------------------------------------------------ 事务

    /// 是否有任何开放事务（其它会话的）——只读路径据此走已提交视图
    pub fn in_txn(&self) -> bool {
        !self.txns.is_empty()
    }

    /// 设置「当前正在执行语句的属主」。
    ///
    /// P2 之前单写者时代，begin 时设置一次即可；多写后多个事务同时开放，
    /// 别的会话 begin 会覆盖这个字段——所以每条语句开始时都必须重新设置
    /// （语句持有引擎写锁，期间不会被覆盖）。
    pub fn set_active_owner(&mut self, owner: u64) {
        self.txn_owner = Some(owner);
    }

    /// DDL 独占门禁：存在其它会话的开放事务时拒绝 DDL
    pub fn ensure_no_other_txns(&self, owner: u64) -> Result<()> {
        if self.txns.keys().any(|existing| *existing != owner) {
            return Err(DbError::new(
                crate::error::LOCKED,
                "存在其它会话的开放事务，DDL 语句暂不支持并发执行",
            ));
        }
        Ok(())
    }

    /// 提交冲突预检：我的写集与快照之后他人提交的页是否重叠
    pub fn check_conflict(&self, owner: u64) -> Option<u32> {
        let txn = self.txns.get(&owner)?;
        for entry in &self.committed {
            if entry.seq <= txn.snapshot {
                continue;
            }
            for key in &txn.dirty {
                if entry.pages.contains_key(key) {
                    return Some(key.0); // 冲突表 id
                }
            }
        }
        None
    }

    /// 记录语句期的索引修改（回滚/冲突时撤销用）
    pub fn record_index_op(&mut self, op: IndexOp) {
        if let Some(owner) = self.txn_owner {
            if let Some(txn) = self.txns.get_mut(&owner) {
                txn.index_ops.push(op);
            }
        }
    }

    /// 记录行数增量
    pub fn record_row_delta(&mut self, table_id: u32, delta: i64) {
        if let Some(owner) = self.txn_owner {
            if let Some(txn) = self.txns.get_mut(&owner) {
                *txn.row_deltas.entry(table_id).or_insert(0) += delta;
            }
        }
    }

    /// 指定会话的开放事务（读己之写的 delta 合并用）
    pub fn txn_of(&self, owner: u64) -> Option<&Txn> {
        self.txns.get(&owner)
    }

    /// 指定会话事务的索引 delta 快照（提交期应用用）
    pub fn txn_index_ops(&self, owner: u64) -> Vec<IndexOp> {
        self.txns
            .get(&owner)
            .map(|t| t.index_ops.clone())
            .unwrap_or_default()
    }

    /// 指定会话事务的行数增量快照
    pub fn txn_row_deltas(&self, owner: u64) -> HashMap<u32, i64> {
        self.txns
            .get(&owner)
            .map(|t| t.row_deltas.clone())
            .unwrap_or_default()
    }

    /// 最老活跃快照：checkpoint 只能落盘 seq <= watermark 的已提交页，
    /// 否则会破坏开放事务的读视图
    fn snapshot_watermark(&self) -> u64 {
        self.txns
            .values()
            .map(|t| t.snapshot)
            .min()
            .unwrap_or(self.commit_seq)
    }

    pub fn begin(&mut self, owner: u64) -> Result<()> {
        self.assert_usable()?;
        // P2：多写开放。写-写冲突在提交时按页检测（首写者获胜）。
        if self.txns.contains_key(&owner) {
            return Err(DbError::new(
                crate::error::LOCKED,
                "本会话已有进行中的事务",
            ));
        }
        self.txn_seq += 1;
        self.txn_owner = Some(owner);
        self.txns.insert(
            owner,
            Txn {
                id: self.txn_seq,
                snapshot: self.commit_seq,
                pages: HashMap::new(),
                dirty: HashSet::new(),
                catalog_dirty: false,
                drops: Vec::new(),
                page_count_before: HashMap::new(),
                index_ops: Vec::new(),
                row_deltas: HashMap::new(),
            },
        );
        Ok(())
    }

    pub fn commit(&mut self, owner: u64) -> Result<()> {
        self.assert_usable()?;
        let txn = match self.txns.remove(&owner) {
            Some(t) => t,
            None => return Err(DbError::internal("没有进行中的事务")),
        };
        if self.txns.is_empty() {
            self.txn_owner = None;
        } else if self.txn_owner == Some(owner) {
            // 还有其它开放事务：owner 指针让给任意一个存活事务，
            // 写路径（dirty_page/alloc_page）按调用时的 txn_owner 定位
            self.txn_owner = None;
        }

        let empty = txn.dirty.is_empty() && !txn.catalog_dirty;
        if empty {
            return Ok(());
        }

        let mut records: Vec<WalRecord> = Vec::new();
        if txn.catalog_dirty {
            records.push(WalRecord::Catalog {
                txn: txn.id as u32,
                json: self.catalog.to_json().to_string(),
            });
        }
        for key in &txn.dirty {
            if let Some(page) = txn.pages.get(key) {
                records.push(WalRecord::Page {
                    txn: txn.id as u32,
                    table_id: key.0,
                    page_no: key.1,
                    data: page.clone(),
                });
            }
        }
        records.push(WalRecord::Commit { txn: txn.id as u32 });

        // 上一次同步已报错（磁盘故障）的场景：拒绝新写入，等待重启恢复
        if let Some(err) = self.commit_barrier.error() {
            self.discard_txn(&txn);
            self.needs_recovery = true;
            return Err(DbError::internal(format!(
                "之前的 WAL fsync 失败（{}），本事务未提交，请重启恢复",
                err
            )));
        }

        if let Err(err) = self.wal.append(&records) {
            self.discard_txn(&txn);
            return Err(DbError::internal(format!(
                "提交失败，事务已回滚：{}",
                err.message
            )));
        }

        // 提交页集合挂入有序列表；seq 在下方递增后回填
        let entry_seq = self.commit_seq + 1;
        self.committed.push(CommittedPages {
            seq: entry_seq,
            pages: txn
                .dirty
                .iter()
                .filter_map(|key| txn.pages.get(key).map(|p| (*key, p.clone())))
                .collect(),
        });
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

        // 发布提交序号；full 模式由调用方在【释放引擎写锁之后】等待 fsync 确认
        self.commit_seq += 1;
        debug_assert_eq!(self.committed.last().unwrap().seq, self.commit_seq);
        self.commit_barrier.publish(self.commit_seq);
        Ok(())
    }

    /// 回滚指定会话的事务（None = 该会话没有开放事务）。
    /// P3 延迟索引后，索引 delta 从未应用到共享索引——直接随事务丢弃；
    /// 页与 catalog 的恢复在 discard_txn 内完成。
    /// 返回是否含 catalog 变更（引擎据此决定是否重建内存表）。
    pub fn rollback(&mut self, owner: u64) -> Option<bool> {
        let txn = self.txns.remove(&owner)?;
        if self.txns.is_empty() {
            self.txn_owner = None;
        } else if self.txn_owner == Some(owner) {
            self.txn_owner = None;
        }
        let catalog_dirty = txn.catalog_dirty;
        self.discard_txn(&txn);
        Some(catalog_dirty)
    }

    fn discard_txn(&mut self, txn: &Txn) {
        for (table_id, count) in &txn.page_count_before {
            // 其它开放事务在此表上分配过页或写过页时，不能收缩页计数
            // （会砍掉它们的页），容忍空洞页——空闲页扫描会复用
            let others_use_table = self.txns.values().any(|t| {
                t.dirty.iter().any(|k| k.0 == *table_id) || t.page_count_before.contains_key(table_id)
            });
            if !others_use_table {
                if let Some(f) = self.files.get_mut(table_id) {
                    f.pages = (*count).min(f.pages);
                }
            }
        }
        if txn.catalog_dirty {
            self.catalog = self.committed_catalog.clone();
        }
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        if !self.txns.is_empty() {
            return Err(DbError::internal("不能在事务进行中执行 checkpoint"));
        }
        // WAL 截断前必须确认所有已写入的提交都已 fsync
        self.commit_barrier.wait_all()?;
        // 水位线：只落盘 seq <= 最老活跃快照 的提交页，
        // 更新的条目留给开放事务的快照读
        let watermark = self.snapshot_watermark();
        let mut pending: HashMap<PageKey, Vec<u8>> = HashMap::new();
        self.committed.retain(|entry| {
            if entry.seq <= watermark {
                // 升序合并：同一页以更高的 seq 为准
                for (key, page) in &entry.pages {
                    pending.insert(*key, page.clone());
                }
                false
            } else {
                true
            }
        });
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
            self.read_handles.remove(&table_id);
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

        let page = self.read_shared(table_id, page_no, self.txn_owner)?;
        if let Some(txn_owner) = self.txn_owner {
            if let Some(txn) = self.txns.get_mut(&txn_owner) {
                txn.pages.insert(key, page.clone());
            }
        }
        Ok(page)
    }

    /// 共享读：读锁下的页读取（多个读线程并发安全）。
    ///
    /// 可见性：`owner` 匹配当前写事务属主时可见未提交页（自己的事务），
    /// 否则只见已提交数据（pending + 磁盘）。
    /// 文件读取用定位读（Windows seek_read / Unix read_at），不改共享文件位置。
    pub fn read_shared(&self, table_id: u32, page_no: u32, owner: Option<u64>) -> Result<Vec<u8>> {
        if let Some(page) = self.read_at(table_id, page_no, owner, self.commit_seq)? {
            return Ok(page);
        }
        match self.read_handles.get(&table_id) {
            None => Ok(init_page(page_no)),
            Some(handle) => {
                let offset = page_no as u64 * PAGE_SIZE as u64;
                let mut buf = vec![0u8; PAGE_SIZE];
                // 定位读：可能部分读取，循环补齐
                let mut filled = 0usize;
                while filled < PAGE_SIZE {
                    let n = positional_read(handle, &mut buf[filled..], offset + filled as u64)?;
                    if n == 0 {
                        // 页在内存页数内但磁盘上还没有：活动事务刚分配的页，
                        // 对共享/已提交视图就是空页
                        return Ok(init_page(page_no));
                    }
                    filled += n;
                }
                Ok(buf)
            }
        }
    }

    /// 共享读的页数：写路径分配页时同步递增了 `files.pages`，
    /// 读侧按此值扫描，越界内容会读到空页（对未提交分配是正确语义）
    pub fn page_count_shared(&self, table_id: u32) -> u32 {
        self.files.get(&table_id).map(|f| f.pages).unwrap_or(0)
    }

    /// 快照可见性核心：先看属主事务自己的未提交页，再从新到旧
    /// 找第一个 `seq <= 快照` 且包含该页的已提交条目，最后落磁盘。
    /// 返回 None 表示三层都没有（磁盘读由调用方处理）。
    fn read_at(
        &self,
        table_id: u32,
        page_no: u32,
        owner: Option<u64>,
        latest_visible: u64,
    ) -> Result<Option<Vec<u8>>> {
        let key: PageKey = (table_id, page_no);
        if let Some(o) = owner {
            if let Some(txn) = self.txns.get(&o) {
                if let Some(page) = txn.pages.get(&key) {
                    return Ok(Some(page.clone()));
                }
                // 属主事务：已提交条目按它自己的快照过滤
                for entry in self.committed.iter().rev() {
                    if entry.seq > txn.snapshot {
                        continue;
                    }
                    if let Some(page) = entry.pages.get(&key) {
                        return Ok(Some(page.clone()));
                    }
                }
                return Ok(None);
            }
        }
        // 非属主（普通读）：可见全部已提交内容，直到 latest_visible
        for entry in self.committed.iter().rev() {
            if entry.seq > latest_visible {
                continue;
            }
            if let Some(page) = entry.pages.get(&key) {
                return Ok(Some(page.clone()));
            }
        }
        Ok(None)
    }

    /// 把修改后的页写回事务缓存并标记为脏页（写锁路径，owner 必须已 begin）
    pub fn dirty_page(&mut self, table_id: u32, page_no: u32, data: Vec<u8>) -> Result<()> {
        let key: PageKey = (table_id, page_no);
        let owner = self.txn_owner.ok_or_else(|| {
            DbError::internal("写操作必须在事务内执行")
        })?;
        let txn = self
            .txns
            .get_mut(&owner)
            .ok_or_else(|| DbError::internal("写操作必须在事务内执行"))?;
        if data.len() != PAGE_SIZE {
            return Err(DbError::internal("页长度必须为 4096 字节"));
        }
        txn.pages.insert(key, data);
        txn.dirty.insert(key);
        Ok(())
    }

    /// 分配新页并初始化为空页
    pub fn alloc_page(&mut self, table_id: u32) -> Result<u32> {
        let owner = self.txn_owner.ok_or_else(|| {
            DbError::internal("分配页面必须在事务内执行")
        })?;
        self.table_file(table_id)?;
        let current = match self.files.get(&table_id) {
            Some(f) => f.pages,
            None => 0,
        };

        if let Some(f) = self.files.get_mut(&table_id) {
            f.pages = current + 1;
        }
        if let Some(txn) = self.txns.get_mut(&owner) {
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
        let owner = self
            .txn_owner
            .ok_or_else(|| DbError::internal("元数据修改必须在事务内执行"))?;
        match self.txns.get_mut(&owner) {
            Some(txn) => {
                txn.catalog_dirty = true;
                Ok(())
            }
            None => Err(DbError::internal("元数据修改必须在事务内执行")),
        }
    }

    pub fn add_drop(&mut self, table_id: u32) -> Result<()> {
        let owner = self
            .txn_owner
            .ok_or_else(|| DbError::internal("删除表必须在事务内执行"))?;
        match self.txns.get_mut(&owner) {
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
        // 关闭时无法等待开放事务：直接丢弃（进程退出，未提交数据本就不该持久化）
        if !self.txns.is_empty() {
            self.txns.clear();
            self.txn_owner = None;
        }
        // 先确认全部提交落盘，再 checkpoint（会截断 WAL）
        let _ = self.commit_barrier.wait_all();
        let _ = self.checkpoint();
        self.shutdown_sync_thread();
        for (_, f) in self.files.iter() {
            let _ = f.file.sync_all();
        }
        self.files.clear();
        self.read_handles.clear();
        // 释放目录锁，允许之后重新打开
        self._lock = None;
        Ok(())
    }

    fn shutdown_sync_thread(&mut self) {
        self.commit_barrier.shutdown();
        if let Some(handle) = self.sync_thread.take() {
            let _ = handle.join();
        }
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
            pending_pages: self.committed.iter().map(|e| e.pages.len()).sum::<usize>(),
            checkpoints: self.checkpoints,
        }
    }
}

/// 只负责停掉后台同步线程；故意不触碰 WAL / 数据文件——
/// 「打开后不 close 直接丢弃」是崩溃模拟测试依赖的行为。
impl Drop for Store {
    fn drop(&mut self) {
        self.shutdown_sync_thread();
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

/// 位置无关读取：Windows seek_read / 其它平台 read_at。
/// 同一文件句柄可被多个读线程并发使用（不移动共享文件位置）。
fn positional_read(file: &File, buf: &mut [u8], offset: u64) -> Result<usize> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        match file.seek_read(buf, offset) {
            Ok(n) => Ok(n),
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(0),
            Err(err) => Err(err.into()),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        match file.read_at(buf, offset) {
            Ok(n) => Ok(n),
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(0),
            Err(err) => Err(err.into()),
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (file, buf, offset);
        Err(DbError::internal("该平台不支持并发定位读"))
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
            store.begin(0).unwrap();
            let page_no = store.alloc_page(1).unwrap();
            assert_eq!(page_no, 0);
            let mut page = store.read_page(1, 0).unwrap();
            page[100] = 42;
            store.dirty_page(1, 0, page).unwrap();
            store.commit(0).unwrap();
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
            store.begin(0).unwrap();
            store.alloc_page(1).unwrap();
            assert_eq!(store.page_count(1), 1);
            store.rollback(0);
            assert_eq!(store.page_count(1), 0);

            // 回滚后重新分配应得到同一个页号
            store.begin(0).unwrap();
            let page_no = store.alloc_page(1).unwrap();
            assert_eq!(page_no, 0);
            store.commit(0).unwrap();
            store.close().unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn committed_pages_visible_before_checkpoint() {
        let dir = temp_dir("c");
        {
            let mut store = Store::open(&dir).unwrap();
            store.begin(0).unwrap();
            store.alloc_page(7).unwrap();
            let mut page = store.read_page(7, 0).unwrap();
            page[10] = 9;
            store.dirty_page(7, 0, page).unwrap();
            store.commit(0).unwrap();

            // 还没 checkpoint，但新事务必须能读到
            store.begin(0).unwrap();
            let page = store.read_page(7, 0).unwrap();
            assert_eq!(page[10], 9);
            store.rollback(0);
            store.close().unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recover_from_wal_after_simulated_crash() {
        let dir = temp_dir("d");
        {
            let mut store = Store::open(&dir).unwrap();
            store.begin(0).unwrap();
            store.alloc_page(3).unwrap();
            let mut page = store.read_page(3, 0).unwrap();
            page[500] = 77;
            store.dirty_page(3, 0, page).unwrap();
            store.commit(0).unwrap();
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
