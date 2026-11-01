//! 对外引擎 API：表管理与 CRUD。
//!
//! 借用约定：写操作必须在事务内。用 `write(|engine| ...)` 包裹可以自动
//! 处理 begin / commit / rollback，避免调用方忘记。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use crate::catalog::{IndexDef, TableDef};
use crate::codec::{self, Column, ColumnType, Value};
use crate::error::{self, DbError, Result};
use crate::json::Json;
use crate::store::{IndexOp, Store, StoreStats};
use crate::table::{self, Index, RowId, Table};

pub struct Engine {
    pub store: Store,
    pub tables: Vec<Table>,
    /// 每表一把写锁：同表 DML 串行（页内容冲突要求）。
    ///
    /// 现状（与 mvcc-design.md §3.9 一致）：`ConnSession::execute` 对写语句
    /// 整条持有引擎的 **全局写锁**（`executor` 的语句执行全程需要 `&mut Engine`），
    /// 所以跨表写语句实际上也是串行的，这把表锁目前是冗余的第二道保险，
    /// 留着是为了将来把语句执行改成「全局读锁 + 表锁 + 提交期短写锁」时不用再改这里。
    /// 真正的并发收益在「事务级交错」：两个会话可以各持有一个开放事务而互不阻塞。
    table_locks: std::sync::Mutex<std::collections::HashMap<u32, std::sync::Arc<std::sync::RwLock<()>>>>,
}

/// 引擎内部隐式事务（engine.write）的属主 id；
/// 显式会话 id 由全局计数器分配，不会与它冲突
pub const INTERNAL_TXN_OWNER: u64 = 0;
/// CLI 独占引擎（Session）的属主 id
pub const EXCLUSIVE_TXN_OWNER: u64 = u64::MAX;

/// 一次索引查找的结果：命中的 rowId + 人类可读的访问方式（EXPLAIN 直接复用，
/// 保证「执行计划」与实际走的路一致）
pub struct IndexLookup {
    pub ids: Vec<RowId>,
    pub label: String,
}

impl Engine {
    pub fn open(dir: &Path) -> Result<Engine> {
        let mut store = Store::open(dir)?;
        let defs: Vec<TableDef> = store.catalog.tables.clone();
        let mut tables = Vec::with_capacity(defs.len());
        for def in &defs {
            tables.push(Table::load(&mut store, def)?);
        }
        Ok(Engine {
            store,
            tables,
            table_locks: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    pub fn table_index(&self, name: &str) -> Result<usize> {
        let lower = name.to_lowercase();
        self.tables
            .iter()
            .position(|t| t.name.to_lowercase() == lower)
            .ok_or_else(|| DbError::not_found(format!("表 \"{}\" 不存在", name)))
    }

    pub fn table(&self, name: &str) -> Result<&Table> {
        let index = self.table_index(name)?;
        Ok(&self.tables[index])
    }

    pub fn table_names(&self) -> Vec<String> {
        self.tables.iter().map(|t| t.name.clone()).collect()
    }

    /// 获取指定表的 DML 写锁（惰性创建）
    pub fn table_lock(&self, table_id: u32) -> std::sync::Arc<std::sync::RwLock<()>> {
        let mut map = self
            .table_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.entry(table_id)
            .or_insert_with(|| std::sync::Arc::new(std::sync::RwLock::new(())))
            .clone()
    }

    /// 在一个事务里执行写操作，出错自动回滚（内部属主）
    pub fn write<T, F>(&mut self, action: F) -> Result<T>
    where
        F: FnOnce(&mut Engine) -> Result<T>,
    {
        self.write_as(INTERNAL_TXN_OWNER, action)
    }

    /// 同 write，但事务属主由调用方指定（会话 id），
    /// 这样 DDL 独占门禁不会把会话自己的隐式事务误判为「其它会话」
    pub fn write_as<T, F>(&mut self, owner: u64, action: F) -> Result<T>
    where
        F: FnOnce(&mut Engine) -> Result<T>,
    {
        self.store.begin(owner)?;
        // 用 reborrow：FnOnce 会 move 掉 &mut，之后的 commit 还要用 self
        let outcome = action(&mut *self);
        match outcome {
            Ok(value) => match self.commit(owner) {
                Ok(()) => Ok(value),
                Err(err) => {
                    self.rollback_txn(owner);
                    Err(err)
                }
            },
            Err(err) => {
                self.rollback_txn(owner);
                Err(err)
            }
        }
    }

    /// 回滚指定会话的事务：页/catalog 由 store 恢复；
    /// P3 延迟索引后索引 delta 从未应用到共享索引，随事务丢弃即可。
    /// 含 DDL 时从 catalog 重建内存表。
    fn rollback_txn(&mut self, owner: u64) {
        if let Some(catalog_dirty) = self.store.rollback(owner) {
            if catalog_dirty {
                self.reload_tables();
            }
        }
    }

    fn apply_op(&mut self, op: &IndexOp) {
        let (table_id, index_pos, key, row_id, added) = match op {
            IndexOp::Add {
                table_id,
                index_pos,
                key,
                row_id,
            } => (*table_id, *index_pos, key, *row_id, true),
            IndexOp::Remove {
                table_id,
                index_pos,
                key,
                row_id,
            } => (*table_id, *index_pos, key, *row_id, false),
        };
        if let Ok(pos) = self.table_index_by_id(table_id) {
            let table = &mut self.tables[pos];
            if added {
                table::index_add_key(table, index_pos, key, row_id);
            } else {
                table::index_remove_key(table, index_pos, key, row_id);
            }
        }
    }

    fn apply_inverse(&mut self, op: &IndexOp) {
        let (table_id, index_pos, key, row_id, added) = match op {
            IndexOp::Add {
                table_id,
                index_pos,
                key,
                row_id,
            } => (*table_id, *index_pos, key, *row_id, true),
            IndexOp::Remove {
                table_id,
                index_pos,
                key,
                row_id,
            } => (*table_id, *index_pos, key, *row_id, false),
        };
        if let Ok(pos) = self.table_index_by_id(table_id) {
            let table = &mut self.tables[pos];
            if added {
                table::index_remove_key(table, index_pos, key, row_id);
            } else {
                table::index_add_key(table, index_pos, key, row_id);
            }
        }
    }

    fn table_index_by_id(&self, table_id: u32) -> Result<usize> {
        self.tables
            .iter()
            .position(|t| t.id == table_id)
            .ok_or_else(|| DbError::internal("内部错误：索引操作指向不存在的表"))
    }

    fn reload_tables(&mut self) {
        // catalog 可能被回滚，内存表结构需要与之一致
        let defs: Vec<TableDef> = self.store.catalog.tables.clone();
        let mut tables = Vec::with_capacity(defs.len());
        for def in &defs {
            if let Ok(t) = Table::load(&mut self.store, def) {
                tables.push(t);
            }
        }
        self.tables = tables;
    }

    // ------------------------------------------------------------ 表管理

    pub fn create_table(
        &mut self,
        name: &str,
        columns: Vec<Column>,
        if_not_exists: bool,
    ) -> Result<()> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(DbError::syntax("表名不能为空"));
        }
        if self.table_index(trimmed).is_ok() {
            if if_not_exists {
                return Ok(());
            }
            return Err(DbError::new(
                error::ALREADY_EXISTS,
                format!("表 \"{}\" 已存在", trimmed),
            ));
        }
        if columns.is_empty() {
            return Err(DbError::syntax("表至少要保留一列"));
        }

        let mut seen: HashSet<String> = HashSet::new();
        for column in &columns {
            if column.name.trim().is_empty() {
                return Err(DbError::syntax("列名不能为空"));
            }
            if !seen.insert(column.name.to_lowercase()) {
                return Err(DbError::syntax(format!("列名重复：{}", column.name)));
            }
        }

        // 自增列：只支持 INT，且一张表最多一个
        let auto_columns: Vec<&Column> = columns.iter().filter(|c| c.auto_increment).collect();
        if auto_columns.len() > 1 {
            return Err(DbError::syntax("一张表最多只能有一个 AUTO_INCREMENT 列"));
        }
        if let Some(column) = auto_columns.first() {
            if column.ty != ColumnType::Int {
                return Err(DbError::syntax(format!(
                    "AUTO_INCREMENT 只支持 INT 列，\"{}\" 是 {}",
                    column.name,
                    column.ty.as_str()
                )));
            }
        }

        let id = self.store.catalog.next_table_id.max(1);
        self.store.catalog.next_table_id = id + 1;

        let mut index_defs: Vec<IndexDef> = Vec::new();
        let pk_columns: Vec<usize> = columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.primary_key)
            .map(|(i, _)| i)
            .collect();
        if !pk_columns.is_empty() {
            index_defs.push(IndexDef {
                name: "__pk__".to_string(),
                columns: pk_columns
                    .iter()
                    .map(|i| columns[*i].name.clone())
                    .collect(),
                unique: true,
                primary: true,
                implicit: false,
            });
        }
        for column in columns.iter() {
            if !column.unique || column.primary_key {
                continue;
            }
            index_defs.push(IndexDef {
                name: format!("uq_{}", column.name),
                columns: vec![column.name.clone()],
                unique: true,
                primary: false,
                implicit: true,
            });
        }

        let def = TableDef {
            id,
            name: trimmed.to_string(),
            columns,
            indexes: index_defs,
            next_auto_id: 1,
        };
        let table = Table::load(&mut self.store, &def)?;
        self.store.catalog.tables.push(def);
        self.store.mark_catalog_dirty()?;
        self.tables.push(table);
        Ok(())
    }

    pub fn drop_table(&mut self, name: &str, if_exists: bool) -> Result<()> {
        let index = match self.table_index(name) {
            Ok(i) => i,
            Err(err) => {
                if if_exists {
                    return Ok(());
                }
                return Err(err);
            }
        };
        let table = self.tables.remove(index);
        let id = table.id;
        self.store.catalog.tables.retain(|t| t.id != id);
        self.store.mark_catalog_dirty()?;
        self.store.add_drop(id)?;
        Ok(())
    }

    /// 表重命名：只改 catalog 与内存表名。
    /// 数据文件按表 id（`data/tN.tbl`）组织，所以不需要搬迁数据。
    pub fn rename_table(&mut self, from: &str, to: &str) -> Result<()> {
        let trimmed = to.trim();
        if trimmed.is_empty() {
            return Err(DbError::syntax("表名不能为空"));
        }
        let position = self.table_index(from)?;
        if self.table_index(trimmed).is_ok() {
            return Err(DbError::new(
                error::ALREADY_EXISTS,
                format!("表 \"{}\" 已存在", trimmed),
            ));
        }
        let id = self.tables[position].id;
        self.tables[position].name = trimmed.to_string();
        if let Some(def) = self.store.catalog.tables.iter_mut().find(|t| t.id == id) {
            def.name = trimmed.to_string();
        }
        self.store.mark_catalog_dirty()?;
        Ok(())
    }

    pub fn create_index(
        &mut self,
        name: &str,
        table_name: &str,
        columns: Vec<String>,
        unique: bool,
        if_not_exists: bool,
    ) -> Result<()> {
        let position = self.table_index(table_name)?;
        if self.tables[position].find_index(name).is_some() {
            if if_not_exists {
                return Ok(());
            }
            return Err(DbError::new(
                error::ALREADY_EXISTS,
                format!("索引 \"{}\" 已存在", name),
            ));
        }
        if columns.is_empty() {
            return Err(DbError::syntax("索引至少要包含一列"));
        }

        let mut column_indexes = Vec::with_capacity(columns.len());
        for column_name in &columns {
            match self.tables[position].column_index(column_name) {
                Some(i) => column_indexes.push(i),
                None => {
                    return Err(DbError::not_found(format!(
                        "列 \"{}\" 不存在于表 \"{}\"",
                        column_name, table_name
                    )))
                }
            }
        }

        let index = Index {
            name: name.to_string(),
            columns: column_indexes,
            unique,
            primary: false,
            implicit: false,
            map: BTreeMap::new(),
        };
        self.tables[position].indexes.push(index);

        let table_id = self.tables[position].id;
        if let Err(err) = table::build_indexes(&mut self.store, &mut self.tables[position]) {
            // 构建失败（唯一冲突），移除刚加的索引
            self.tables[position].indexes.pop();
            let _ = table::build_indexes(&mut self.store, &mut self.tables[position]);
            return Err(err);
        }

        if let Some(def) = self.store.catalog.tables.iter_mut().find(|t| t.id == table_id) {
            def.indexes.push(IndexDef {
                name: name.to_string(),
                columns: columns.clone(),
                unique,
                primary: false,
                implicit: false,
            });
        }
        self.store.mark_catalog_dirty()?;
        Ok(())
    }

    pub fn drop_index(&mut self, name: &str, if_exists: bool) -> Result<()> {
        let lower = name.to_lowercase();
        let mut found: Option<(usize, usize)> = None;
        for (ti, table) in self.tables.iter().enumerate() {
            if let Some(ii) = table.indexes.iter().position(|ix| ix.name.to_lowercase() == lower) {
                found = Some((ti, ii));
                break;
            }
        }
        let (ti, ii) = match found {
            Some(pair) => pair,
            None => {
                if if_exists {
                    return Ok(());
                }
                return Err(DbError::not_found(format!("索引 \"{}\" 不存在", name)));
            }
        };
        if self.tables[ti].indexes[ii].primary {
            return Err(DbError::syntax("不能删除主键索引"));
        }

        let removed = self.tables[ti].indexes.remove(ii);
        let table_id = self.tables[ti].id;
        if let Some(def) = self.store.catalog.tables.iter_mut().find(|t| t.id == table_id) {
            def.indexes.retain(|ix| ix.name != removed.name);
        }
        self.store.mark_catalog_dirty()?;
        Ok(())
    }

    // ------------------------------------------------------------ 行操作

    pub fn insert(&mut self, table_name: &str, values: Vec<Value>) -> Result<RowId> {
        Ok(self.insert_returning(table_name, values)?.0)
    }

    /// 插入一行，返回 `(row_id, 自增列最终值)`。
    ///
    /// 自增列的处理：
    /// - 值为 NULL → 分配 `next_auto_id` 并递增；
    /// - 显式值 ≥ `next_auto_id` → 游标顶到 `value + 1`
    ///   （这样 `dump`/`load` 或手工指定 id 之后不会撞车）；
    /// - 游标变化标记 catalog 脏，随本次提交一起进 WAL，崩溃后可恢复。
    pub fn insert_returning(
        &mut self,
        table_name: &str,
        mut values: Vec<Value>,
    ) -> Result<(RowId, Option<i64>)> {
        let position = self.table_index(table_name)?;
        let auto_value = self.assign_auto_increment(position, &mut values)?;
        let row_id = table::insert(&mut self.store, &mut self.tables[position], values)?;
        Ok((row_id, auto_value))
    }

    fn assign_auto_increment(
        &mut self,
        position: usize,
        values: &mut Vec<Value>,
    ) -> Result<Option<i64>> {
        let table_id = self.tables[position].id;
        let slot = match self.tables[position]
            .columns
            .iter()
            .position(|column| column.auto_increment)
        {
            Some(slot) => slot,
            None => return Ok(None),
        };
        if slot >= values.len() {
            return Ok(None);
        }

        let next = self
            .store
            .catalog
            .tables
            .iter()
            .find(|def| def.id == table_id)
            .map(|def| def.next_auto_id)
            .unwrap_or(1);

        let assigned = match &values[slot] {
            // NULL 表示「让数据库分配」
            Value::Null => {
                values[slot] = Value::Int(next);
                next.saturating_add(1)
            }
            // 显式给值：只把游标顶上去，不改用户写的值
            Value::Int(n) if *n >= next => n.saturating_add(1),
            _ => return Ok(None),
        };
        if let Some(def) = self
            .store
            .catalog
            .tables
            .iter_mut()
            .find(|def| def.id == table_id)
        {
            def.next_auto_id = assigned;
        }
        self.store.mark_catalog_dirty()?;
        match &values[slot] {
            Value::Int(n) => Ok(Some(*n)),
            _ => Ok(None),
        }
    }

    /// 清空表数据：表结构、索引定义都保留（索引内容清空），自增游标归零。
    ///
    /// 实现上是**逐行删除**而不是重建文件：页内死空间会就地回收，
    /// 而且完全走既有事务路径（回滚后数据能回来，WAL 重放也一致）。
    /// 只收集 row_id（4 字节/行），不物化整行，所以不受查询行数上限限制。
    pub fn truncate(&mut self, table_name: &str) -> Result<u64> {
        let position = self.table_index(table_name)?;
        let table_id = self.tables[position].id;

        let mut ids: Vec<RowId> = Vec::new();
        {
            let table = &self.tables[position];
            table::scan_shared_while(&self.store, table, None, |row_id, _values| {
                ids.push(row_id);
                Ok(true)
            })?;
        }

        let mut removed = 0u64;
        for row_id in ids {
            if table::delete(&mut self.store, &mut self.tables[position], row_id)? {
                removed += 1;
            }
        }

        if let Some(def) = self
            .store
            .catalog
            .tables
            .iter_mut()
            .find(|def| def.id == table_id)
        {
            def.next_auto_id = 1;
        }
        self.store.mark_catalog_dirty()?;
        Ok(removed)
    }

    /// 把本事务的索引 delta 合并进查找结果（读己之写）。
    ///
    /// `hit` 判定某个 delta 的键是否落在本次查找覆盖的键区间内：
    /// 等值查找是精确相等，前缀查找是 starts_with，范围查找是区间比较。
    fn merge_index_deltas(
        &self,
        table_id: u32,
        index_pos: usize,
        mut ids: Vec<RowId>,
        owner: Option<u64>,
        mut hit: impl FnMut(&str) -> bool,
    ) -> Vec<RowId> {
        if let Some(o) = owner {
            if let Some(txn) = self.store.txn_of(o) {
                for op in &txn.index_ops {
                    let (op_table, op_pos, op_key, op_row, is_add) = match op {
                        IndexOp::Add {
                            table_id,
                            index_pos,
                            key,
                            row_id,
                        } => (*table_id, *index_pos, key, *row_id, true),
                        IndexOp::Remove {
                            table_id,
                            index_pos,
                            key,
                            row_id,
                        } => (*table_id, *index_pos, key, *row_id, false),
                    };
                    if op_table != table_id || op_pos != index_pos || !hit(op_key) {
                        continue;
                    }
                    if is_add {
                        if !ids.contains(&op_row) {
                            ids.push(op_row);
                        }
                    } else {
                        ids.retain(|id| *id != op_row);
                    }
                }
            }
        }
        ids
    }

    /// 前导列范围查找（共享视图 + 读己之写）。
    ///
    /// `bounds` 里的键必须已按 `codec::index_key` 编码（`range_candidates` 负责）。
    /// 返回 None 表示该列没有前导索引；有 delta 的一并合并，
    /// 所以本事务未提交的插入/删除在这里同样可见/不可见。
    pub fn find_by_range_shared(
        &self,
        table_name: &str,
        column_name: &str,
        bounds: &table::RangeBounds,
        owner: Option<u64>,
    ) -> Option<Vec<RowId>> {
        let position = self.table_index(table_name).ok()?;
        let table = &self.tables[position];
        let ci = table.column_index(column_name)?;
        let index_pos = table
            .indexes
            .iter()
            .position(|ix| ix.columns.first() == Some(&ci))?;
        let ids = table::find_by_range_at(table, index_pos, bounds)?;
        let table_id = table.id;
        Some(self.merge_index_deltas(table_id, index_pos, ids, owner, |op_key: &str| {
            let within = |bound: &Option<(String, bool)>, is_lower: bool| -> bool {
                match bound {
                    None => true,
                    Some((key, inclusive)) => {
                        let ord = op_key.cmp(key.as_str());
                        if is_lower {
                            ord == std::cmp::Ordering::Greater
                                || (ord == std::cmp::Ordering::Equal && *inclusive)
                        } else {
                            ord == std::cmp::Ordering::Less
                                || (ord == std::cmp::Ordering::Equal && *inclusive)
                        }
                    }
                }
            };
            within(&bounds.lower, true) && within(&bounds.upper, false)
        }))
    }

    /// 是否存在「别的会话」的活跃写事务。
    /// COUNT(*) 索引计数捷径的保险丝：个别路径（影子表重建的 build_indexes）
    /// 会把未提交行写进共享索引，此时必须回退到逐行可见性过滤。
    pub fn has_other_writers(&self, owner: u64) -> bool {
        self.store.has_txns_other_than(owner)
    }

    pub fn delete(&mut self, table_name: &str, row_id: RowId) -> Result<bool> {
        let position = self.table_index(table_name)?;
        table::delete(&mut self.store, &mut self.tables[position], row_id)
    }

    pub fn update(&mut self, table_name: &str, row_id: RowId, values: Vec<Value>) -> Result<Option<RowId>> {
        let position = self.table_index(table_name)?;
        table::update(&mut self.store, &mut self.tables[position], row_id, values)
    }

    pub fn get(&mut self, table_name: &str, row_id: RowId) -> Result<Option<Vec<Value>>> {
        let position = self.table_index(table_name)?;
        let table = &self.tables[position];
        table::get(&mut self.store, table, row_id)
    }

    pub fn scan(&mut self, table_name: &str) -> Result<Vec<(RowId, Vec<Value>)>> {
        let position = self.table_index(table_name)?;
        let table = &self.tables[position];
        table::scan(&mut self.store, table)
    }

    /// 共享视图扫描（读锁 / &self）：owner 匹配事务属主时可见其未提交页
    pub fn scan_shared(&self, table_name: &str, owner: Option<u64>) -> Result<Vec<(RowId, Vec<Value>)>> {
        let position = self.table_index(table_name)?;
        let table = &self.tables[position];
        table::scan_shared(&self.store, table, owner)
    }

    /// 共享视图按 row_id 取行
    pub fn get_shared(&self, table_name: &str, row_id: RowId, owner: Option<u64>) -> Result<Option<Vec<Value>>> {
        let position = self.table_index(table_name)?;
        let table = &self.tables[position];
        table::get_shared(&self.store, table, row_id, owner)
    }

    /// 单列等值查索引（`find_by_index` 的薄封装，供 JOIN 辅助与外部调用）
    pub fn find_by_equality(
        &self,
        table_name: &str,
        column_name: &str,
        value: &Value,
        owner: Option<u64>,
    ) -> Result<Option<Vec<RowId>>> {
        let equals = [(column_name.to_string(), value.clone())];
        Ok(self
            .find_by_index(table_name, &equals, owner)?
            .map(|lookup| lookup.ids))
    }

    /// 按「列 = 常量」约束集合查索引：
    /// 在所有索引中挑**前导列全部命中、且前缀最长**的那个，
    /// 前缀覆盖全部索引列时做精确查找，否则做前缀区间查找（复合索引用得上）。
    /// 没有可用索引返回 None（调用方回退全表扫描）。
    ///
    /// owner 给定时合并该事务的索引 delta（读己之写：
    /// 自己未提交的插入/删除在索引查询中同样可见/不可见）。
    pub fn find_by_index(
        &self,
        table_name: &str,
        equals: &[(String, Value)],
        owner: Option<u64>,
    ) -> Result<Option<IndexLookup>> {
        let table = self.table(table_name)?;

        // 列下标 → 常量（同名多条件取第一个；NULL 不参与索引）
        let mut wanted: HashMap<usize, &Value> = HashMap::new();
        for (name, value) in equals {
            if value.is_null() {
                continue;
            }
            if let Some(ci) = table.column_index(name) {
                wanted.entry(ci).or_insert(value);
            }
        }
        if wanted.is_empty() {
            return Ok(None);
        }

        // 选前缀最长者：k = 从第一列起连续命中的列数
        let mut best: Option<(usize, usize)> = None;
        for (pos, index) in table.indexes.iter().enumerate() {
            let mut k = 0usize;
            for ci in &index.columns {
                if wanted.contains_key(ci) {
                    k += 1;
                } else {
                    break;
                }
            }
            if k > 0 && best.map(|(_, best_k)| k > best_k).unwrap_or(true) {
                best = Some((pos, k));
            }
        }
        let Some((index_pos, k)) = best else {
            return Ok(None);
        };
        let index = &table.indexes[index_pos];
        let full = k == index.columns.len();

        // 组装键：常量先按列类型转换（否则 TEXT '5' 与 INT 列对不上，
        // 会静默返回空集）；转不了就不走索引，交给全表扫描报准确错误。
        let mut parts: Vec<String> = Vec::with_capacity(k);
        for ci in index.columns.iter().take(k) {
            let column = &table.columns[*ci];
            let Ok(value) = codec::coerce(column.ty, column.scale, wanted[ci]) else {
                return Ok(None);
            };
            let Some(part) = codec::index_key(&value) else {
                return Ok(None);
            };
            parts.push(part);
        }
        let key = parts.join("\u{1}");
        // 完整键直接命中；部分键加分隔符做前缀区间
        let prefix = if full {
            key.clone()
        } else {
            format!("{}\u{1}", key)
        };

        let mut ids = if full {
            table::find_index_key(table, index_pos, &key)
        } else {
            table::find_by_prefix(table, index_pos, &prefix)
        };

        ids = self.merge_index_deltas(
            table.id,
            index_pos,
            ids,
            owner,
            |op_key: &str| {
                if full {
                    op_key == &key
                } else {
                    op_key.starts_with(&prefix)
                }
            },
        );

        let label = if full {
            format!("索引查找（{}，{} 列）", index.name, k)
        } else {
            format!("复合索引前缀查找（{}，前 {} 列）", index.name, k)
        };
        Ok(Some(IndexLookup { ids, label }))
    }

    // ------------------------------------------------------------ 事务

    pub fn begin(&mut self, owner: u64) -> Result<()> {
        self.store.begin(owner)
    }

    /// 提交：先做页级写冲突检测（首写者获胜），冲突则回滚本事务
    /// 提交编排：
    /// 1. 页级写冲突检测（首写者获胜）→ WRITE_CONFLICT
    /// 2. 延迟索引：把本事务的索引 delta 应用到共享索引，
    ///    唯一索引的 Add 若撞已提交的键 → CONSTRAINT_VIOLATION
    ///    （顺序应用，失败时逆序撤销已应用部分，保持原子）
    /// 3. WAL + committed 列表（store.commit）+ 行数增量
    pub fn commit(&mut self, owner: u64) -> Result<()> {
        if let Some(table_id) = self.store.check_conflict(owner) {
            self.rollback_txn(owner);
            let name = self
                .tables
                .iter()
                .find(|t| t.id == table_id)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| format!("#{}", table_id));
            return Err(DbError::new(
                error::WRITE_CONFLICT,
                format!(
                    "写冲突：事务期间表 \"{}\" 的数据已被其它事务修改并提交，请重试整个事务",
                    name
                ),
            ));
        }

        // 索引 delta 与行数增量都必须在 store.commit 之前取快照：
        // commit 成功会从事务表里移除该事务，之后再问就只剩空表。
        let ops = self.store.txn_index_ops(owner);
        let row_deltas = self.store.txn_row_deltas(owner);
        let mut applied: Vec<IndexOp> = Vec::new();
        for op in &ops {
            if let IndexOp::Add {
                table_id,
                index_pos,
                key,
                row_id,
            } = op
            {
                if let Ok(pos) = self.table_index_by_id(*table_id) {
                    let unique = self.tables[pos]
                        .indexes
                        .get(*index_pos)
                        .map(|ix| ix.unique)
                        .unwrap_or(false);
                    if unique {
                        // 冲突 = 索引里已有**别的** row_id 占着这个键。
                        // 不能只看「列表非空」：同一事务里 CREATE INDEX /
                        // 重建表会经由 build_indexes 把本事务的行写进索引，
                        // 提交时这批 delta 会再 Add 一次，只看非空会误报冲突。
                        let taken = self.tables[pos]
                            .indexes
                            .get(*index_pos)
                            .and_then(|ix| ix.map.get(key))
                            .map(|list| list.iter().any(|existing| existing != row_id))
                            .unwrap_or(false);
                        if taken {
                            let column_names: Vec<String> = self.tables[pos]
                                .indexes[*index_pos]
                                .columns
                                .iter()
                                .map(|ci| self.tables[pos].columns[*ci].name.clone())
                                .collect();
                            for prev in applied.iter().rev() {
                                self.apply_inverse(prev);
                            }
                            self.store.rollback(owner);
                            return Err(DbError::constraint(format!(
                                "唯一约束冲突：{} 上已存在相同的值（{}）",
                                column_names.join(", "),
                                key
                            )));
                        }
                    }
                }
            }
            self.apply_op(op);
            applied.push(op.clone());
        }

        match self.store.commit(owner) {
            Ok(()) => {
                // 行数增量在提交成功后计入（语句期不改共享行数）
                for (table_id, delta) in row_deltas {
                    if let Ok(pos) = self.table_index_by_id(table_id) {
                        let table = &mut self.tables[pos];
                        table.row_count = if delta >= 0 {
                            table.row_count.saturating_add(delta as u32)
                        } else {
                            table.row_count.saturating_sub(((-delta) as u32).min(table.row_count))
                        };
                    }
                }
                Ok(())
            }
            Err(err) => {
                // WAL 写入失败：撤销已应用的索引 delta
                for prev in applied.iter().rev() {
                    self.apply_inverse(prev);
                }
                self.store.rollback(owner);
                Err(err)
            }
        }
    }

    pub fn rollback(&mut self, owner: u64) {
        self.rollback_txn(owner);
    }

    /// DDL 独占门禁：其它会话有开放事务时拒绝
    pub fn ensure_ddl_exclusive(&self, owner: u64) -> Result<()> {
        self.store.ensure_no_other_txns(owner)
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        self.store.checkpoint()
    }

    pub fn close(&mut self) -> Result<()> {
        self.store.close()
    }

    /// 深度一致性校验：全表扫描 + 行数比对 + 索引重建比对。
    ///
    /// 返回问题列表（空 = 通过）。检查项：
    /// 1. 每一页都能解析、每个 row_id 的页号都在页数范围内；
    /// 2. catalog 的 row_count 与实际行数一致；
    /// 3. 每个索引的内存内容 == 按行重建的期望内容（能发现「索引指向不存在行」
    ///    与「行存在但索引漏了」两类静默损坏）；
    /// 4. 唯一索引的键不重复；
    /// 5. 空闲页列表里的页不含活行。
    pub fn deep_verify(&mut self) -> Vec<String> {
        let mut problems: Vec<String> = Vec::new();
        let names = self.table_names();

        for name in &names {
            let position = match self.table_index(name) {
                Ok(pos) => pos,
                Err(err) => {
                    problems.push(format!("[{name}] 打开表失败：{err}"));
                    continue;
                }
            };
            let (page_count, row_count_claimed, free_pages) = {
                let table = &self.tables[position];
                let page_count = self.store.page_count(table.id);
                (page_count, table.row_count, table.free_pages.clone())
            };

            let rows = match self.scan(name) {
                Ok(rows) => rows,
                Err(err) => {
                    problems.push(format!("[{name}] 全表扫描失败（存在无法解析的页/行）：{err}"));
                    continue;
                }
            };

            // ---- 1. row_count 与实际行数
            if rows.len() as u64 != row_count_claimed as u64 {
                problems.push(format!(
                    "[{name}] row_count 不一致：catalog 记录 {row_count_claimed}，实际扫描 {}",
                    rows.len()
                ));
            }

            // ---- 2. row_id 页号越界 + 空闲页列表合法性
            // （free_pages 的语义是「有空闲空间可复用的页」，半空页带活行属正常，
            //   所以只校验页号范围，不要求「无活行」）
            let free: std::collections::HashSet<u32> = free_pages;
            let mut seen_ids: std::collections::HashSet<RowId> = std::collections::HashSet::new();
            for (row_id, _) in &rows {
                let (page_no, _slot) = crate::table::split_row_id(*row_id);
                if page_no >= page_count {
                    problems.push(format!(
                        "[{name}] row_id {row_id} 的页号 {page_no} 超出页数 {page_count}"
                    ));
                }
                if !seen_ids.insert(*row_id) {
                    problems.push(format!("[{name}] row_id {row_id} 重复出现"));
                }
            }
            for page_no in &free {
                if *page_no >= page_count {
                    problems.push(format!(
                        "[{name}] 空闲页 {page_no} 超出页数 {page_count}"
                    ));
                }
            }

            // ---- 3. 索引一致性：重建期望并与内存比对
            let index_count = self.tables[position].indexes.len();
            let mut expected: Vec<std::collections::BTreeMap<String, Vec<RowId>>> =
                vec![std::collections::BTreeMap::new(); index_count];
            for (row_id, values) in &rows {
                for (pos, key) in crate::table::index_keys_of(&self.tables[position], values) {
                    expected[pos].entry(key).or_default().push(*row_id);
                }
            }
            for pos in 0..index_count {
                let index_name = self.tables[position].indexes[pos].name.clone();
                let is_unique = self.tables[position].indexes[pos].unique;
                let actual = &self.tables[position].indexes[pos].map;

                // 期望里有、索引里没有 = 行存在但索引漏了；反之 = 索引指向不存在行
                for (key, ids) in &expected[pos] {
                    match actual.get(key) {
                        None => problems.push(format!(
                            "[{name}] 索引 {index_name} 漏条目：键 {key:?} 应指向 {} 行",
                            ids.len()
                        )),
                        Some(actual_ids) if actual_ids != ids => problems.push(format!(
                            "[{name}] 索引 {index_name} 条目不一致：键 {key:?} 期望 {ids:?}，实际 {actual_ids:?}"
                        )),
                        _ => {}
                    }
                }
                for key in actual.keys() {
                    if !expected[pos].contains_key(key) {
                        problems.push(format!(
                            "[{name}] 索引 {index_name} 指向不存在的行：键 {key:?}"
                        ));
                    }
                }
                // 唯一索引键重复
                if is_unique {
                    for (key, ids) in actual {
                        if ids.len() > 1 {
                            problems.push(format!(
                                "[{name}] 唯一索引 {index_name} 键 {key:?} 有 {} 行（应唯一）",
                                ids.len()
                            ));
                        }
                    }
                }
            }
        }
        problems
    }

    pub fn stats(&self) -> StoreStats {
        self.store.stats()
    }

    /// 表结构描述，供 CLI / HTTP 输出
    pub fn describe(&self, table_name: &str) -> Result<Vec<(String, String, String, String, String)>> {
        let table = self.table(table_name)?;
        let mut rows = Vec::with_capacity(table.columns.len());
        for column in &table.columns {
            rows.push((
                column.name.clone(),
                column.ty.as_str().to_string(),
                if column.primary_key { "PK".to_string() } else { String::new() },
                if column.not_null { "NOT NULL".to_string() } else { String::new() },
                match &column.default {
                    Some(v) => v.as_text(),
                    None => String::new(),
                },
            ));
        }
        Ok(rows)
    }
}

// 刻意不实现 Drop：不调用 close 时数据留在 WAL 里，下次打开会自动重放，
// 这也是「模拟崩溃」测试依赖的行为。需要落盘请显式调用 `close()`。

pub struct RebuildOutcome {
    pub rows: usize,
    pub indexes: usize,
    pub columns: Vec<(String, String)>,
}

/// SQL 值 → 配置用 Json（VACUUM 构造列 spec 时用；Text/Decimal 走文本保精度）
fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(n) => Json::Int(*n),
        Value::Real(n) => Json::Num(*n),
        Value::Decimal { v, scale } => Json::Str(codec::format_decimal(*v, *scale)),
        Value::Text(s) => Json::Str(s.clone()),
    }
}

impl Engine {
    /// 按新列定义重建表并迁移数据（影子表重建）。
    ///
    /// 每个列可以带 `source` 指向旧列名，因此「改名」也能保住数据；
    /// 新列没有 source 时取默认值，否则填 NULL。
    /// 必须在事务内调用，任何一步失败都会整体回滚。
    /// VACUUM：对一张表做影子表重建（同 schema），数据紧凑地写进新文件，
    /// 旧文件随之删除——把删除/更新留下的空闲空间还给文件系统。
    /// 列属性（主键/唯一/非空/默认值/自增/DECIMAL 标度）与索引全部保留。
    /// 返回（整理前字节, 整理后字节）。
    pub fn vacuum_table(&mut self, name: &str) -> Result<(u64, u64)> {
        let before = self.store.stats().bytes;
        let position = self.table_index(name)?;
        let columns = self.tables[position].columns.clone();

        let specs: Vec<Json> = columns
            .iter()
            .map(|c| {
                let mut items: Vec<(String, Json)> = vec![
                    ("name".to_string(), Json::str(&c.name)),
                    ("type".to_string(), Json::str(c.ty.as_str())),
                    (
                        "primaryKey".to_string(),
                        Json::Bool(c.primary_key),
                    ),
                    ("notNull".to_string(), Json::Bool(c.not_null)),
                    ("unique".to_string(), Json::Bool(c.unique)),
                    (
                        "autoIncrement".to_string(),
                        Json::Bool(c.auto_increment),
                    ),
                    // 数据来源 = 同名列（保数据）
                    ("source".to_string(), Json::str(&c.name)),
                ];
                if c.ty == ColumnType::Decimal {
                    // 精度不存于列定义，显式带 scale 防丢
                    items.push(("scale".to_string(), Json::Num(c.scale as f64)));
                }
                if let Some(default) = &c.default {
                    items.push(("default".to_string(), value_to_json(default)));
                }
                Json::Obj(items)
            })
            .collect();

        self.rebuild_table(name, &specs)?;
        let after = self.store.stats().bytes;
        Ok((before, after))
    }

    pub fn rebuild_table(&mut self, name: &str, specs: &[Json]) -> Result<RebuildOutcome> {
        if specs.is_empty() {
            return Err(DbError::syntax("表至少要保留一列"));
        }

        let (old_columns, old_indexes) = {
            let table = self.table(name)?;
            let indexes = table
                .indexes
                .iter()
                .map(|i| {
                    (
                        i.name.clone(),
                        i.columns.clone(),
                        i.unique,
                        i.primary,
                        i.implicit,
                    )
                })
                .collect::<Vec<(String, Vec<usize>, bool, bool, bool)>>();
            (table.columns.clone(), indexes)
        };
        let old_rows = self.scan(name)?;

        // 解析新列定义
        let mut new_columns: Vec<Column> = Vec::with_capacity(specs.len());
        let mut sources: Vec<String> = Vec::with_capacity(specs.len());
        let mut seen: HashSet<String> = HashSet::new();

        for spec in specs {
            let col_name = spec
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if col_name.is_empty() {
                return Err(DbError::syntax("列名不能为空"));
            }
            if !seen.insert(col_name.to_lowercase()) {
                return Err(DbError::syntax(format!("列名重复：{}", col_name)));
            }

            let type_name = spec.get("type").and_then(|v| v.as_str()).unwrap_or("TEXT");
            let ty = ColumnType::parse(type_name)?;
            // DECIMAL 标度：优先用显式 scale 键（Column 不存精度，
            // 只传 "DECIMAL" 会把标度丢成默认值），否则从类型串里解析
            let scale = match spec.get("scale").and_then(|v| v.as_i64()) {
                Some(scale) if scale >= 0 && scale <= u8::MAX as i64 => scale as u8,
                _ => ColumnType::decimal_scale_from_type(type_name)?,
            };
            let primary_key = spec
                .get("primaryKey")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let mut column = Column::new(col_name.clone(), ty);
            column.scale = scale;
            column.primary_key = primary_key;
            column.not_null = spec
                .get("notNull")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || primary_key;
            column.unique = spec.get("unique").and_then(|v| v.as_bool()).unwrap_or(false);
            column.auto_increment = spec
                .get("autoIncrement")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if let Some(default) = spec.get("default") {
                let value = json_to_value(default);
                if !value.is_null() && !value.as_text().is_empty() {
                    column.default = Some(codec::coerce(ty, column.scale, &value)?);
                }
            }

            let source = spec
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or(&col_name)
                .to_lowercase();
            sources.push(source);
            new_columns.push(column);
        }

        // 把旧数据映射到新结构（先全部算好，避免半途改到真实表）
        let old_index: HashMap<String, usize> = old_columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.to_lowercase(), i))
            .collect();

        let mut mapped_rows: Vec<Vec<Value>> = Vec::with_capacity(old_rows.len());
        for (_, values) in &old_rows {
            let mut next: Vec<Value> = Vec::with_capacity(new_columns.len());
            for (i, column) in new_columns.iter().enumerate() {
                let value = match old_index.get(&sources[i]) {
                    Some(old_at) => {
                        let raw = values.get(*old_at).cloned().unwrap_or(Value::Null);
                        codec::coerce(column.ty, column.scale, &raw)?
                    }
                    None => match &column.default {
                        Some(default) => default.clone(),
                        None => Value::Null,
                    },
                };
                if column.not_null && value.is_null() {
                    return Err(DbError::constraint(format!(
                        "列 \"{}\" 不允许为 NULL，但旧数据中存在空值，请给它设置默认值或允许为 NULL",
                        column.name
                    )));
                }
                next.push(value);
            }
            mapped_rows.push(next);
        }

        // 重建
        self.drop_table(name, false)?;
        self.create_table(name, new_columns.clone(), false)?;
        for row in &mapped_rows {
            self.insert(name, row.clone())?;
        }

        // 迁移显式索引（列还在就按列名映射回去）
        let mut restored = 0usize;
        for (index_name, columns, unique, primary, implicit) in old_indexes {
            if primary || implicit {
                continue;
            }
            let mut mapped: Vec<String> = Vec::with_capacity(columns.len());
            let mut ok = true;
            for ci in columns {
                let old_name = old_columns[ci].name.to_lowercase();
                match sources.iter().position(|s| *s == old_name) {
                    Some(pos) => mapped.push(new_columns[pos].name.clone()),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok || mapped.is_empty() {
                continue;
            }
            if self
                .create_index(&index_name, name, mapped, unique, true)
                .is_ok()
            {
                restored += 1;
            }
        }

        Ok(RebuildOutcome {
            rows: mapped_rows.len(),
            indexes: restored,
            columns: new_columns
                .iter()
                .map(|c| (c.name.clone(), c.ty.as_str().to_string()))
                .collect(),
        })
    }
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

/// 供测试与 CLI 使用：把字符串字面量按列类型转换
pub fn coerce_row(columns: &[Column], raw: Vec<Value>) -> Result<Vec<Value>> {
    if raw.len() != columns.len() {
        return Err(DbError::internal("列数量与值数量不一致"));
    }
    let mut out = Vec::with_capacity(columns.len());
    for (i, column) in columns.iter().enumerate() {
        out.push(codec::coerce(column.ty, column.scale, &raw[i])?);
    }
    Ok(out)
}
