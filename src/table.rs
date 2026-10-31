//! 表行操作与内存索引。
//!
//! 行号编码：`row_id = (page_no << 16) | slot`，与 Node 版一致。
//! 索引全部在内存，启动时通过扫描数据页重建。

use std::collections::{HashMap, HashSet};

use crate::catalog::{IndexDef, TableDef};
use crate::codec::{self, Column, Value};
use crate::error::{self, DbError, Result};
use crate::page as pg;
use crate::store::Store;

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
    pub map: HashMap<String, Vec<u32>>,
}

impl Index {
    fn new(def: &IndexDef, columns: Vec<usize>) -> Index {
        Index {
            name: def.name.clone(),
            columns,
            unique: def.unique,
            primary: def.primary,
            implicit: def.implicit,
            map: HashMap::new(),
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
        index_add(table, values, *row_id);
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

fn index_add(table: &mut Table, values: &[Value], row_id: u32) {
    for index in table.indexes.iter_mut() {
        let picked: Vec<Value> = index.columns.iter().map(|i| values[*i].clone()).collect();
        if let Some(key) = codec::index_key_of(&picked) {
            index
                .map
                .entry(key)
                .or_insert_with(Vec::new)
                .push(row_id);
        }
    }
}

fn index_remove(table: &mut Table, values: &[Value], row_id: u32) {
    for index in table.indexes.iter_mut() {
        let picked: Vec<Value> = index.columns.iter().map(|i| values[*i].clone()).collect();
        if let Some(key) = codec::index_key_of(&picked) {
            let mut empty = false;
            if let Some(list) = index.map.get_mut(&key) {
                list.retain(|id| *id != row_id);
                empty = list.is_empty();
            }
            if empty {
                index.map.remove(&key);
            }
        }
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

fn check_unique(table: &Table, values: &[Value], skip_row: Option<u32>) -> Result<()> {
    for index in &table.indexes {
        if !index.unique {
            continue;
        }
        let picked: Vec<Value> = index.columns.iter().map(|i| values[*i].clone()).collect();
        let key = match codec::index_key_of(&picked) {
            Some(k) => k,
            None => continue, // NULL 不参与唯一性判断
        };
        if let Some(list) = index.map.get(&key) {
            for id in list {
                if Some(*id) != skip_row {
                    let column_names: Vec<String> = index
                        .columns
                        .iter()
                        .map(|i| table.columns[*i].name.clone())
                        .collect();
                    return Err(DbError::constraint(format!(
                        "唯一约束冲突：{} 上已存在相同的值",
                        column_names.join(", ")
                    )));
                }
            }
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
    check_unique(table, &values, None)?;
    let row_id = insert_physical(store, table, &record)?;
    index_add(table, &values, row_id);
    table.row_count += 1;
    Ok(row_id)
}

pub fn delete(store: &mut Store, table: &mut Table, row_id: u32) -> Result<bool> {
    let values = match get(store, table, row_id)? {
        Some(v) => v,
        None => return Ok(false),
    };
    index_remove(table, &values, row_id);
    let removed = delete_physical(store, table, row_id)?;
    if removed {
        table.row_count = table.row_count.saturating_sub(1);
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

    index_remove(table, &old, row_id);
    if let Err(err) = check_unique(table, &values, Some(row_id)) {
        index_add(table, &old, row_id);
        return Err(err);
    }

    if !delete_physical(store, table, row_id)? {
        index_add(table, &old, row_id);
        return Ok(None);
    }
    table.row_count = table.row_count.saturating_sub(1);

    // 走到这里记录长度已经校验过，insert_physical 失败的概率极低；
    // 真失败就交给上层回滚整个事务，不做半吊子的就地恢复。
    let new_row_id = insert_physical(store, table, &record)?;

    index_add(table, &values, new_row_id);
    table.row_count += 1;
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
        store.commit().unwrap();

        let row = get(&mut store, &table, id).unwrap().unwrap();
        assert_eq!(row[1], Value::Text("alice".to_string()));

        store.begin(0).unwrap();
        assert!(delete(&mut store, &mut table, id).unwrap());
        store.commit().unwrap();
        assert!(get(&mut store, &table, id).unwrap().is_none());

        store.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn primary_key_conflict() {
        let dir = temp_dir("b");
        let mut store = Store::open(&dir).unwrap();
        let mut table = sample_table(&mut store);
        store.begin(0).unwrap();
        insert(
            &mut store,
            &mut table,
            vec![Value::Int(1), Value::Text("a".to_string()), Value::Int(0)],
        )
        .unwrap();
        let err = insert(
            &mut store,
            &mut table,
            vec![Value::Int(1), Value::Text("b".to_string()), Value::Int(0)],
        )
        .unwrap_err();
        assert_eq!(err.code, error::CONSTRAINT);
        store.rollback();
        store.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_column_enforced() {
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
        let err = insert(
            &mut store,
            &mut table,
            vec![Value::Int(2), Value::Text("dup".to_string()), Value::Int(0)],
        )
        .unwrap_err();
        assert_eq!(err.code, error::CONSTRAINT);
        store.rollback();
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
        store.rollback();
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
            insert(
                &mut store,
                &mut table,
                vec![
                    Value::Int(i),
                    Value::Text(format!("p{}", i)),
                    Value::Int(i * 2),
                ],
            )
            .unwrap();
        }
        store.commit().unwrap();

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
                insert(
                    &mut store,
                    &mut table,
                    vec![
                        Value::Int(i),
                        Value::Text(format!("u{}", i)),
                        Value::Int(i),
                    ],
                )
                .unwrap();
            }
            store.commit().unwrap();
            store.close().unwrap();
        }

        let mut store = Store::open(&dir).unwrap();
        let def = store.catalog.tables[0].clone();
        let mut table = Table::load(&mut store, &def).unwrap();
        assert_eq!(table.row_count, 20);
        let ids = find_by_equality(&table, "id", &Value::Int(5)).unwrap();
        assert_eq!(ids.len(), 1);

        store.begin(0).unwrap();
        let err = insert(
            &mut store,
            &mut table,
            vec![Value::Int(5), Value::Text("dup".to_string()), Value::Int(0)],
        )
        .unwrap_err();
        assert_eq!(err.code, error::CONSTRAINT);
        store.rollback();
        store.close().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
