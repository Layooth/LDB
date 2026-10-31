//! 对外引擎 API：表管理与 CRUD。
//!
//! 借用约定：写操作必须在事务内。用 `write(|engine| ...)` 包裹可以自动
//! 处理 begin / commit / rollback，避免调用方忘记。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::catalog::{IndexDef, TableDef};
use crate::codec::{self, Column, ColumnType, Value};
use crate::error::{self, DbError, Result};
use crate::json::Json;
use crate::store::{Store, StoreStats};
use crate::table::{self, Index, Table};

pub struct Engine {
    pub store: Store,
    pub tables: Vec<Table>,
}

impl Engine {
    pub fn open(dir: &Path) -> Result<Engine> {
        let mut store = Store::open(dir)?;
        let defs: Vec<TableDef> = store.catalog.tables.clone();
        let mut tables = Vec::with_capacity(defs.len());
        for def in &defs {
            tables.push(Table::load(&mut store, def)?);
        }
        Ok(Engine { store, tables })
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

    /// 在一个事务里执行写操作，出错自动回滚
    pub fn write<T, F>(&mut self, action: F) -> Result<T>
    where
        F: FnOnce(&mut Engine) -> Result<T>,
    {
        self.store.begin()?;
        // 用 reborrow：FnOnce 会 move 掉 &mut，之后的 commit 还要用 self
        let outcome = action(&mut *self);
        match outcome {
            Ok(value) => match self.store.commit() {
                Ok(()) => Ok(value),
                Err(err) => {
                    self.rollback_and_reload();
                    Err(err)
                }
            },
            Err(err) => {
                self.rollback_and_reload();
                Err(err)
            }
        }
    }

    fn rollback_and_reload(&mut self) {
        self.store.rollback();
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
            map: HashMap::new(),
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

    pub fn insert(&mut self, table_name: &str, values: Vec<Value>) -> Result<u32> {
        let position = self.table_index(table_name)?;
        table::insert(&mut self.store, &mut self.tables[position], values)
    }

    pub fn delete(&mut self, table_name: &str, row_id: u32) -> Result<bool> {
        let position = self.table_index(table_name)?;
        table::delete(&mut self.store, &mut self.tables[position], row_id)
    }

    pub fn update(&mut self, table_name: &str, row_id: u32, values: Vec<Value>) -> Result<Option<u32>> {
        let position = self.table_index(table_name)?;
        table::update(&mut self.store, &mut self.tables[position], row_id, values)
    }

    pub fn get(&mut self, table_name: &str, row_id: u32) -> Result<Option<Vec<Value>>> {
        let position = self.table_index(table_name)?;
        let table = &self.tables[position];
        table::get(&mut self.store, table, row_id)
    }

    pub fn scan(&mut self, table_name: &str) -> Result<Vec<(u32, Vec<Value>)>> {
        let position = self.table_index(table_name)?;
        let table = &self.tables[position];
        table::scan(&mut self.store, table)
    }

    pub fn find_by_equality(
        &self,
        table_name: &str,
        column_name: &str,
        value: &Value,
    ) -> Result<Option<Vec<u32>>> {
        let table = self.table(table_name)?;
        Ok(table::find_by_equality(table, column_name, value))
    }

    // ------------------------------------------------------------ 事务

    pub fn begin(&mut self) -> Result<()> {
        self.store.begin()
    }

    pub fn commit(&mut self) -> Result<()> {
        self.store.commit()
    }

    pub fn rollback(&mut self) {
        self.rollback_and_reload();
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        self.store.checkpoint()
    }

    pub fn close(&mut self) -> Result<()> {
        self.store.close()
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

impl Engine {
    /// 按新列定义重建表并迁移数据（影子表重建）。
    ///
    /// 每个列可以带 `source` 指向旧列名，因此「改名」也能保住数据；
    /// 新列没有 source 时取默认值，否则填 NULL。
    /// 必须在事务内调用，任何一步失败都会整体回滚。
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
            let primary_key = spec
                .get("primaryKey")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let mut column = Column::new(col_name.clone(), ty);
            column.primary_key = primary_key;
            column.not_null = spec
                .get("notNull")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || primary_key;
            column.unique = spec.get("unique").and_then(|v| v.as_bool()).unwrap_or(false);

            if let Some(default) = spec.get("default") {
                let value = json_to_value(default);
                if !value.is_null() && !value.as_text().is_empty() {
                    column.default = Some(codec::coerce(ty, &value)?);
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
                        codec::coerce(column.ty, &raw)?
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
        out.push(codec::coerce(column.ty, &raw[i])?);
    }
    Ok(out)
}
