//! 表行操作与内存索引。
//!
//! 行号编码：`row_id = (page_no << 16) | slot`。
//! 索引全部在内存，启动时通过扫描数据页重建。

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::catalog::{IndexDef, TableDef};
use crate::codec::{self, Column, Value};
use crate::error::{self, DbError, Result};
use crate::page as pg;
use crate::store::{IndexOp, Store};

/// 页内剩余空间低于此值时不再作为插入候选（避免碎片化）
const FREE_PAGE_MIN: i32 = 128;

/// row_id = (page_no << 16) | slot，page_no 只有 16 位可用，
/// 超过上限后 row_id 会与前面的页冲突（静默数据损坏），必须显式拒绝。
pub const MAX_PAGES_PER_TABLE: u32 = 65535;

pub struct Index {
    pub name: String,
    pub columns: Vec<usize>,
    pub unique: bool,
    pub primary: bool,
    pub implicit: bool,
    pub map: BTreeMap<String, Vec<u32>>,
}

impl Index {
    fn new(def: &IndexDef, columns: Vec<usize>) -> Index {
        Index {
            name: def.name.clone(),
            columns,
            unique: def.unique,
            primary: def.primary,
            implicit: def.implicit,
            map: BTreeMap::new(),
        }
    }
}

pub struct Table {
    pub id: u32,
    pub name: String,
    pub columns: Vec<Column>,
    pub indexes: Vec<Index>,
    pub row_count: u32,
    pub free_pages: HashSet<u32>,
}

pub fn make_row_id(page_no: u32, slot: u16) -> u32 {
    (page_no << 16) | slot as u32
}

pub fn split_row_id(row_id: u32) -> (u32, u16) {
    (row_id >> 16, (row_id & 0xFFFF) as u16)
}

impl Table {
    /// 从 catalog 定义构造，并扫描数据页重建索引
    pub fn load(store: &mut Store, def: &TableDef) -> Result<Table> {
        let mut table = Table {
            id: def.id,
            name: def.name.clone(),
            columns: def.columns.clone(),
            indexes: Vec::new(),
            row_count: 0,
            free_pages: HashSet::new(),
        };

        let mut names: HashSet<String> = HashSet::new();
        for index_def in &def.indexes {
            let mut columns = Vec::new();
            let mut valid = true;
            for column_name in &index_def.columns {
                match table.column_index(column_name) {
                    Some(i) => columns.push(i),
                    None => {
                        valid = false;
                        break;
                    }
                }
            }
            if !valid || columns.is_empty() {
                continue;
            }
            names.insert(index_def.name.clone());
            table.indexes.push(Index::new(index_def, columns));
        }

        // catalog 里可能没有记录主键/列级唯一索引（早期数据），这里补齐
        let pk_columns: Vec<usize> = table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.primary_key)
            .map(|(i, _)| i)
            .collect();
        if !pk_columns.is_empty() && !table.indexes.iter().any(|ix| ix.primary) {
            let pk_names: Vec<String> = pk_columns
                .iter()
                .map(|i| table.columns[*i].name.clone())
                .collect();
            table.indexes.push(Index::new(
                &IndexDef {
                    name: "__pk__".to_string(),
                    columns: pk_names,
                    unique: true,
                    primary: true,
                    implicit: false,
                },
                pk_columns,
            ));
        }
        for i in 0..table.columns.len() {
            let column_name = table.columns[i].name.clone();
            let is_unique = table.columns[i].unique;
            let is_pk = table.columns[i].primary_key;
            if !is_unique || is_pk {
                continue;
            }
            let name = format!("uq_{}", column_name);
            if names.contains(&name) || table.indexes.iter().any(|ix| ix.columns == vec![i]) {
                continue;
            }
            table.indexes.push(Index::new(
                &IndexDef {
                    name,
                    columns: vec![column_name],
                    unique: true,
                    primary: false,
                    implicit: true,
                },
                vec![i],
            ));
        }

        build_indexes(store, &mut table)?;
        Ok(table)
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        let lower = name.to_lowercase();
        self.columns
            .iter()
            .position(|c| c.name.to_lowercase() == lower)
    }

    pub fn find_index(&self, name: &str) -> Option<usize> {
        let lower = name.to_lowercase();
        self.indexes
            .iter()
            .position(|ix| ix.name.to_lowercase() == lower)
    }

    /// 找到列上可用的单列索引（等值查找用）
    pub fn single_column_index(&self, column_name: &str) -> Option<&Index> {
        let ci = self.column_index(column_name)?;
        self.indexes
            .iter()
            .find(|ix| ix.columns.len() == 1 && ix.columns[0] == ci)
    }
}

/// 扫描整表，返回 (row_id, values)
pub fn scan(store: &mut Store, table: &Table) -> Result<Vec<(u32, Vec<Value>)>> {
    let pages = store.page_count(table.id);
    let mut rows = Vec::new();
    for page_no in 0..pages {
        let page = store.read_page(table.id, page_no)?;
        let count = pg::slot_count(&page);
        for slot in 0..count {
            if let Some(record) = pg::read_record(&page, slot) {
                let values = codec::decode_row(&table.columns, &record)?;
                rows.push((make_row_id(page_no, slot), values));
            }
        }
    }
    Ok(rows)
}

/// 共享视图扫描（&store，读锁下并发安全）
pub fn scan_shared(store: &Store, table: &Table, owner: Option<u64>) -> Result<Vec<(u32, Vec<Value>)>> {
    let pages = store.page_count_shared(table.id);
    let mut rows = Vec::new();
    for page_no in 0..pages {
        let page = store.read_shared(table.id, page_no, owner)?;
        let count = pg::slot_count(&page);
        for slot in 0..count {
            if let Some(record) = pg::read_record(&page, slot) {
                let values = codec::decode_row(&table.columns, &record)?;
                rows.push((make_row_id(page_no, slot), values));
            }
        }
    }
    Ok(rows)
}

/// 流式共享视图扫描：每解出一行回调一次，回调返回 Ok(false) 立即停止。
///
/// 给「没有排序/分组 + 带 LIMIT」的查询用：边扫边过滤，凑够
/// offset+limit 行就停，不必把整表物化进内存。
pub fn scan_shared_while<F>(
    store: &Store,
    table: &Table,
    owner: Option<u64>,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(u32, Vec<Value>) -> Result<bool>,
{
    let pages = store.page_count_shared(table.id);
    for page_no in 0..pages {
        let page = store.read_shared(table.id, page_no, owner)?;
        let count = pg::slot_count(&page);
        for slot in 0..count {
            if let Some(record) = pg::read_record(&page, slot) {
                let values = codec::decode_row(&table.columns, &record)?;
                if !visit(make_row_id(page_no, slot), values)? {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// 共享视图按 row_id 取行
pub fn get_shared(
    store: &Store,
    table: &Table,
    row_id: u32,
    owner: Option<u64>,
) -> Result<Option<Vec<Value>>> {
    let (page_no, slot) = split_row_id(row_id);
    if page_no >= store.page_count_shared(table.id) {
        return Ok(None);
    }
    let page = store.read_shared(table.id, page_no, owner)?;
    match pg::read_record(&page, slot) {
        Some(record) => Ok(Some(codec::decode_row(&table.columns, &record)?)),
        None => Ok(None),
    }
}

pub fn build_indexes(store: &mut Store, table: &mut Table) -> Result<()> {
    for index in table.indexes.iter_mut() {
        index.map.clear();
    }
    let rows = scan(store, table)?;
    check_all_unique(table, &rows)?;
    table.row_count = rows.len() as u32;
    for (row_id, values) in &rows {
        for (pos, key) in index_keys_of(table, values) {
            if let Some(index) = table.indexes.get_mut(pos) {
                index
                    .map
                    .entry(key)
                    .or_insert_with(Vec::new)
                    .push(*row_id);
            }
        }
    }
    table.free_pages = collect_free_pages(store, table)?;
    Ok(())
}

/// 重建索引前先确认现有数据不违反唯一性（比如给已有重复值的列加唯一索引）
fn check_all_unique(table: &Table, rows: &[(u32, Vec<Value>)]) -> Result<()> {
    for index in &table.indexes {
        if !index.unique {
            continue;
        }
        let mut seen: HashMap<String, u32> = HashMap::new();
        for (row_id, values) in rows {
            let picked: Vec<Value> = index.columns.iter().map(|i| values[*i].clone()).collect();
            if let Some(key) = codec::index_key_of(&picked) {
                if seen.contains_key(&key) {
                    let names: Vec<String> = index
                        .columns
                        .iter()
                        .map(|i| table.columns[*i].name.clone())
                        .collect();
                    return Err(DbError::constraint(format!(
                        "唯一约束冲突：{} 上存在重复值（索引 {}）",
                        names.join(", "),
                        index.name
                    )));
                }
                seen.insert(key, *row_id);
            }
        }
    }
    Ok(())
}

fn collect_free_pages(store: &mut Store, table: &Table) -> Result<HashSet<u32>> {
    let pages = store.page_count(table.id);
    let mut free = HashSet::new();
    for page_no in 0..pages {
        let page = store.read_page(table.id, page_no)?;
        if pg::free_space(&page) >= FREE_PAGE_MIN {
            free.insert(page_no);
        }
    }
    Ok(free)
}

pub fn get(store: &mut Store, table: &Table, row_id: u32) -> Result<Option<Vec<Value>>> {
    let (page_no, slot) = split_row_id(row_id);
    if page_no >= store.page_count(table.id) {
        return Ok(None);
    }
    let page = store.read_page(table.id, page_no)?;
    match pg::read_record(&page, slot) {
        Some(record) => Ok(Some(codec::decode_row(&table.columns, &record)?)),
        None => Ok(None),
    }
}

/// key 级索引应用（提交期把 delta 落到共享索引时使用；带去重）
pub fn index_add_key(table: &mut Table, index_pos: usize, key: &str, row_id: u32) {
    if let Some(index) = table.indexes.get_mut(index_pos) {
        let list = index.map.entry(key.to_string()).or_insert_with(Vec::new);
        if !list.contains(&row_id) {
            list.push(row_id);
        }
    }
}

pub fn index_remove_key(table: &mut Table, index_pos: usize, key: &str, row_id: u32) {
    if let Some(index) = table.indexes.get_mut(index_pos) {
        let mut empty = false;
        if let Some(list) = index.map.get_mut(key) {
            list.retain(|id| *id != row_id);
            empty = list.is_empty();
        }
        if empty {
            index.map.remove(key);
        }
    }
}

/// 计算一行在所有索引上的 (index_pos, key)
fn index_keys_of(table: &Table, values: &[Value]) -> Vec<(usize, String)> {
    table
        .indexes
        .iter()
        .enumerate()
        .filter_map(|(pos, index)| {
            let picked: Vec<Value> = index.columns.iter().map(|i| values[*i].clone()).collect();
            codec::index_key_of(&picked).map(|key| (pos, key))
        })
        .collect()
}

/// P3 延迟索引：语句期【不】更新共享索引，只把 delta 记进事务，
/// 提交时由 Engine 在临界区内统一应用并做唯一约束检查。
fn record_index_delta(
    store: &mut Store,
    table: &Table,
    keys: &[(usize, String)],
    row_id: u32,
    add: bool,
) {
    for (pos, key) in keys {
        let op = if add {
            IndexOp::Add {
                table_id: table.id,
                index_pos: *pos,
                key: key.clone(),
                row_id,
            }
        } else {
            IndexOp::Remove {
                table_id: table.id,
                index_pos: *pos,
                key: key.clone(),
                row_id,
            }
        };
        store.record_index_op(op);
    }
}

fn check_not_null(table: &Table, values: &[Value]) -> Result<()> {
    if values.len() != table.columns.len() {
        return Err(DbError::internal(format!(
            "列数量({})与值数量({})不一致",
            table.columns.len(),
            values.len()
        )));
    }
    for (i, column) in table.columns.iter().enumerate() {
        if column.not_null && values[i].is_null() {
            return Err(DbError::constraint(format!(
                "列 \"{}\" 不允许为 NULL",
                column.name
            )));
        }
    }
    Ok(())
}

fn insert_physical(store: &mut Store, table: &mut Table, record: &[u8]) -> Result<u32> {
    if record.len() > pg::MAX_RECORD_SIZE {
        return Err(DbError::new(
            error::ROW_TOO_LARGE,
            format!(
                "单行数据过大（{} 字节），上限约 {} 字节",
                record.len(),
                pg::MAX_RECORD_SIZE
            ),
        ));
    }

    let mut candidates: Vec<u32> = table.free_pages.iter().copied().collect();
    candidates.sort_unstable();
    for page_no in candidates {
        let mut page = store.read_page(table.id, page_no)?;
        match pg::insert_record(&mut page, record) {
            Some(slot) => {
                let space = pg::free_space(&page);
                store.dirty_page(table.id, page_no, page)?;
                if space < FREE_PAGE_MIN {
                    table.free_pages.remove(&page_no);
                }
                return Ok(make_row_id(page_no, slot));
            }
            None => {
                table.free_pages.remove(&page_no);
            }
        }
    }

    let count = store.page_count(table.id);
    if count > 0 {
        let page_no = count - 1;
        let mut page = store.read_page(table.id, page_no)?;
        if let Some(slot) = pg::insert_record(&mut page, record) {
            let space = pg::free_space(&page);
            store.dirty_page(table.id, page_no, page)?;
            if space < FREE_PAGE_MIN {
                table.free_pages.remove(&page_no);
            }
            return Ok(make_row_id(page_no, slot));
        }
        table.free_pages.insert(page_no);
    }

    let count = store.page_count(table.id);
    if count >= MAX_PAGES_PER_TABLE {
        return Err(DbError::new(
            error::LIMIT,
            format!(
                "表 \"{}\" 已达单表页数上限（{} 页 ≈ {} MB），无法再分配新页",
                table.name,
                MAX_PAGES_PER_TABLE,
                MAX_PAGES_PER_TABLE * pg::PAGE_SIZE as u32 / 1024 / 1024
            ),
        ));
    }
    let page_no = store.alloc_page(table.id)?;
    let mut page = store.read_page(table.id, page_no)?;
    let slot = pg::insert_record(&mut page, record).ok_or_else(|| {
        DbError::new(
            error::ROW_TOO_LARGE,
            format!("单行超过页容量上限（{} 字节）", pg::MAX_RECORD_SIZE),
        )
    })?;
    store.dirty_page(table.id, page_no, page)?;
    Ok(make_row_id(page_no, slot))
}

fn delete_physical(store: &mut Store, table: &mut Table, row_id: u32) -> Result<bool> {
    let (page_no, slot) = split_row_id(row_id);
    if page_no >= store.page_count(table.id) {
        return Ok(false);
    }
    let mut page = store.read_page(table.id, page_no)?;
    if !pg::delete_record(&mut page, slot) {
        return Ok(false);
    }
    store.dirty_page(table.id, page_no, page)?;
    table.free_pages.insert(page_no);
    Ok(true)
}

pub fn insert(store: &mut Store, table: &mut Table, values: Vec<Value>) -> Result<u32> {
    check_not_null(table, &values)?;
    let record = codec::encode_row(&table.columns, &values)?;
    let keys = index_keys_of(table, &values);
    let row_id = insert_physical(store, table, &record)?;
    // P3：唯一约束延迟到提交期检查；索引 delta 只记账不应用
    record_index_delta(store, table, &keys, row_id, true);
    store.record_row_delta(table.id, 1);
    Ok(row_id)
}

pub fn delete(store: &mut Store, table: &mut Table, row_id: u32) -> Result<bool> {
    let values = match get(store, table, row_id)? {
        Some(v) => v,
        None => return Ok(false),
    };
    let keys = index_keys_of(table, &values);
    let removed = delete_physical(store, table, row_id)?;
    if removed {
        record_index_delta(store, table, &keys, row_id, false);
        store.record_row_delta(table.id, -1);
    }
    Ok(removed)
}

pub fn update(
    store: &mut Store,
    table: &mut Table,
    row_id: u32,
    values: Vec<Value>,
) -> Result<Option<u32>> {
    let old = match get(store, table, row_id)? {
        Some(v) => v,
        None => return Ok(None),
    };
    check_not_null(table, &values)?;
    let record = codec::encode_row(&table.columns, &values)?;

    let old_keys = index_keys_of(table, &old);
    let new_keys = index_keys_of(table, &values);

    if !delete_physical(store, table, row_id)? {
        return Ok(None);
    }
    // delta 顺序：先删旧键后加新键（提交期按序应用，净零变化自然成立）
    record_index_delta(store, table, &old_keys, row_id, false);
    // 走到这里记录长度已经校验过，insert_physical 失败的概率极低；
    // 真失败就交给上层回滚整个事务，不做半吊子的就地恢复。
    let new_row_id = insert_physical(store, table, &record)?;
    record_index_delta(store, table, &new_keys, new_row_id, true);
    // 删 + 插，行数净变化为 0（delta 相互抵消）
    Ok(Some(new_row_id))
}

/// 按等值条件查索引，返回 rowId 列表；没有可用索引返回 None
pub fn find_by_equality(table: &Table, column_name: &str, value: &Value) -> Option<Vec<u32>> {
    let index = table.single_column_index(column_name)?;
    let key = codec::index_key(value)?;
    match index.map.get(&key) {
        Some(list) => Some(list.clone()),
        None => Some(Vec::new()),
    }
}

/// 在指定索引上按完整 key 取 rowId（键不存在返回空）
pub fn find_index_key(table: &Table, index_pos: usize, key: &str) -> Vec<u32> {
    match table.indexes.get(index_pos).and_then(|ix| ix.map.get(key)) {
        Some(list) => list.clone(),
        None => Vec::new(),
    }
}

/// 在指定索引上按 key 前缀取 rowId。
///
/// 复合索引的键是「各列编码 + \u{1}」拼起来的，前导列等值时整段前缀相同，
/// BTreeMap 从下界顺序扫到不再是该前缀为止即可（命中数而非表行数）。
pub fn find_by_prefix(table: &Table, index_pos: usize, prefix: &str) -> Vec<u32> {
    let mut ids = Vec::new();
    if let Some(index) = table.indexes.get(index_pos) {
        for (key, list) in index
            .map
            .range::<str, _>((std::ops::Bound::Included(prefix), std::ops::Bound::Unbounded))
        {
            if !key.starts_with(prefix) {
                break;
            }
            ids.extend_from_slice(list);
        }
    }
    ids
}

/// 范围扫描的边界（键已按 index_key 编码；None 表示该侧无界）
pub struct RangeBounds {
    pub lower: Option<(String, bool)>, // (key, inclusive)
    pub upper: Option<(String, bool)>,
}

/// 按范围查索引；只有目标列存在单列索引时才可用
pub fn find_by_range(table: &Table, column_name: &str, bounds: &RangeBounds) -> Option<Vec<u32>> {
    let index = table.single_column_index(column_name)?;
    // BTreeMap 的范围迭代天然按键升序输出
    let lower_bound = bounds
        .lower
        .as_ref()
        .map(|(k, inc)| {
            if *inc {
                std::ops::Bound::Included(k.clone())
            } else {
                std::ops::Bound::Excluded(k.clone())
            }
        })
        .unwrap_or(std::ops::Bound::Unbounded);
    let upper_bound = bounds
        .upper
        .as_ref()
        .map(|(k, inc)| {
            if *inc {
                std::ops::Bound::Included(k.clone())
            } else {
                std::ops::Bound::Excluded(k.clone())
            }
        })
        .unwrap_or(std::ops::Bound::Unbounded);
    let mut ids = Vec::new();
    for (_, list) in index.map.range((lower_bound, upper_bound)) {
        ids.extend_from_slice(list);
    }
    Some(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::ColumnType;
    use crate::store::Store;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("littledb-table-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample_table(store: &mut Store) -> Table {
        let mut id = Column::new("id", ColumnType::Int);
        id.primary_key = true;
        id.not_null = true;
        let mut name = Column::new("name", ColumnType::Text);
        name.unique = true;
        let score = Column::new("score", ColumnType::Int);

        let def = TableDef {
            id: 1,
            name: "players".to_string(),
            columns: vec![id, name, score],
            indexes: vec![IndexDef {
                name: "__pk__".to_string(),
                columns: vec!["id".to_string()],
                unique: true,
                primary: true,
                implicit: false,
            }],
        };
        // 登记到 catalog，否则重开时表文件会被当成孤儿清理掉
        store.catalog.tables.push(def.clone());
        Table::load(store, &def).unwrap()
    }

    #[test]
    fn insert_get_delete() {
        let dir = temp_dir("a");
        let mut store = Store::open(&dir).unwrap();
        let mut table = sample_table(&mut store);

        store.begin(0).unwrap();
        let id = insert(
            &mut store,
            &mut table,
            vec![
                Value::Int(1),
                Value::Text("alice".to_string()),
                Value::Int(10),
            ],
        )
        .unwrap();
        store.commit(0).unwrap();

        let row = get(&mut store, &table, id).unwrap().unwrap();
        assert_eq!(row[1], Value::Text("alice".to_string()));

        store.begin(0).unwrap();
        assert!(delete(&mut store, &mut table, id).unwrap());
        store.commit(0).unwrap();
        assert!(get(&mut store, &table, id).unwrap().is_none());

        store.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_delta_recorded_not_applied() {
        // P3：语句期不检查唯一约束（延迟到提交期），delta 只记账不进共享索引
        let dir = temp_dir("c");
        let mut store = Store::open(&dir).unwrap();
        let mut table = sample_table(&mut store);
        store.begin(0).unwrap();
        insert(
            &mut store,
            &mut table,
            vec![Value::Int(1), Value::Text("dup".to_string()), Value::Int(0)],
        )
        .unwrap();
        insert(
            &mut store,
            &mut table,
            vec![Value::Int(2), Value::Text("dup".to_string()), Value::Int(0)],
        )
        .unwrap();
        // 共享索引里没有这些键（delta 未应用）
        assert!(table.indexes.iter().all(|ix| ix.map.is_empty()));
        store.rollback(0);
        store.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn not_null_enforced() {
        let dir = temp_dir("d");
        let mut store = Store::open(&dir).unwrap();
        let mut table = sample_table(&mut store);
        store.begin(0).unwrap();
        let err = insert(
            &mut store,
            &mut table,
            vec![Value::Null, Value::Text("x".to_string()), Value::Int(0)],
        )
        .unwrap_err();
        assert_eq!(err.code, error::CONSTRAINT);
        store.rollback(0);
        store.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_lookup_matches_scan() {
        let dir = temp_dir("e");
        let mut store = Store::open(&dir).unwrap();
        let mut table = sample_table(&mut store);

        store.begin(0).unwrap();
        for i in 0..50 {
            let values = vec![
                Value::Int(i),
                Value::Text(format!("p{}", i)),
                Value::Int(i * 2),
            ];
            let record = codec::encode_row(&table.columns, &values).unwrap();
            let row_id = insert_physical(&mut store, &mut table, &record).unwrap();
            // 模拟提交期索引应用（真实路径在 Engine::commit）
            for (pos, key) in index_keys_of(&table, &values) {
                if let Some(index) = table.indexes.get_mut(pos) {
                    index.map.entry(key).or_insert_with(Vec::new).push(row_id);
                }
            }
            table.row_count += 1;
        }
        store.commit(0).unwrap();

        let ids = find_by_equality(&table, "id", &Value::Int(17)).unwrap();
        assert_eq!(ids.len(), 1);
        let row = get(&mut store, &table, ids[0]).unwrap().unwrap();
        assert_eq!(row[1], Value::Text("p17".to_string()));

        store.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_rebuilds_indexes() {
        let dir = temp_dir("f");
        {
            let mut store = Store::open(&dir).unwrap();
            let mut table = sample_table(&mut store);
            store.begin(0).unwrap();
            store.mark_catalog_dirty().unwrap();
            for i in 0..20 {
                let values = vec![
                    Value::Int(i),
                    Value::Text(format!("u{}", i)),
                    Value::Int(i),
                ];
                let record = codec::encode_row(&table.columns, &values).unwrap();
                let row_id = insert_physical(&mut store, &mut table, &record).unwrap();
                for (pos, key) in index_keys_of(&table, &values) {
                    if let Some(index) = table.indexes.get_mut(pos) {
                        index.map.entry(key).or_insert_with(Vec::new).push(row_id);
                    }
                }
                table.row_count += 1;
            }
            store.commit(0).unwrap();
            store.close().unwrap();
        }

        let mut store = Store::open(&dir).unwrap();
        let def = store.catalog.tables[0].clone();
        let table = Table::load(&mut store, &def).unwrap();
        assert_eq!(table.row_count, 20);
        let ids = find_by_equality(&table, "id", &Value::Int(5)).unwrap();
        assert_eq!(ids.len(), 1);

        store.close().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
