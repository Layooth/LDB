//! catalog.json 的读写，字段名与 Node 版完全一致。
//!
//! ```json
//! {
//!   "version": 1,
//!   "nextTableId": 9,
//!   "tables": [
//!     {
//!       "id": 2,
//!       "name": "users",
//!       "columns": [{ "name": "id", "type": "INT", "notNull": true,
//!                     "unique": false, "primaryKey": true, "default": null }],
//!       "indexes": [{ "name": "__pk__", "columns": ["id"], "unique": true,
//!                     "primary": true, "implicit": false }],
//!       "createdAt": "2026-10-30 05:00:00"
//!     }
//!   ]
//! }
//! ```

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::codec::{format_datetime_from_millis, Column, ColumnType, Value};
use crate::error::{DbError, Result};
use crate::json::{self, Json};

pub const CATALOG_VERSION: i64 = 1;

#[derive(Debug, Clone)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    pub implicit: bool,
}

#[derive(Debug, Clone)]
pub struct TableDef {
    pub id: u32,
    pub name: String,
    pub columns: Vec<Column>,
    pub indexes: Vec<IndexDef>,
}

#[derive(Debug, Clone)]
pub struct Catalog {
    pub version: i64,
    pub next_table_id: u32,
    pub tables: Vec<TableDef>,
}

fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Int(n) => Json::Num(*n as f64),
        Value::Real(n) => Json::Num(*n),
        Value::Text(s) => Json::Str(s.clone()),
        Value::Bool(b) => Json::Bool(*b),
    }
}

fn value_from_json(value: &Json) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Num(n) => {
            if n.fract() == 0.0 {
                Value::Int(*n as i64)
            } else {
                Value::Real(*n)
            }
        }
        Json::Str(s) => Value::Text(s.clone()),
        _ => Value::Null,
    }
}

impl Catalog {
    pub fn empty() -> Catalog {
        Catalog {
            version: CATALOG_VERSION,
            next_table_id: 1,
            tables: Vec::new(),
        }
    }

    pub fn find(&self, name: &str) -> Option<&TableDef> {
        let lower = name.to_lowercase();
        self.tables.iter().find(|t| t.name.to_lowercase() == lower)
    }

    pub fn find_index(&self, name: &str) -> Option<usize> {
        let lower = name.to_lowercase();
        self.tables
            .iter()
            .position(|t| t.name.to_lowercase() == lower)
    }

    pub fn table_by_id(&self, id: u32) -> Option<&TableDef> {
        self.tables.iter().find(|t| t.id == id)
    }

    /// 读取 catalog；文件不存在时返回空 catalog
    pub fn load(path: &Path) -> Result<Catalog> {
        if !path.exists() {
            return Ok(Catalog::empty());
        }
        let text = fs::read_to_string(path)?;
        if text.trim().is_empty() {
            return Ok(Catalog::empty());
        }
        let root = json::parse(&text)?;

        let version = root.get("version").and_then(|v| v.as_i64()).unwrap_or(CATALOG_VERSION);
        let mut next_table_id = root
            .get("nextTableId")
            .and_then(|v| v.as_i64())
            .unwrap_or(1)
            .max(1) as u32;

        let mut tables = Vec::new();
        if let Some(Json::Arr(items)) = root.get("tables") {
            for item in items {
                let def = parse_table(item)?;
                if def.id >= next_table_id {
                    next_table_id = def.id + 1;
                }
                tables.push(def);
            }
        }

        Ok(Catalog {
            version,
            next_table_id,
            tables,
        })
    }

    /// 原子写：先写临时文件再 rename，避免半截 catalog。
    /// tmp 必须先 fsync：WAL 截断后 catalog 是唯一凭证，
    /// 掉电时 rename 到位的文件不能是空/半截内容。
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = self.to_json().to_string();
        let tmp = path.with_extension("json.tmp");
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn to_json(&self) -> Json {
        let mut tables = Vec::with_capacity(self.tables.len());
        for table in &self.tables {
            let mut columns = Vec::with_capacity(table.columns.len());
            for column in &table.columns {
                let mut fields = vec![
                    ("name".to_string(), Json::str(column.name.clone())),
                    ("type".to_string(), Json::str(column.ty.as_str())),
                    ("notNull".to_string(), Json::Bool(column.not_null)),
                    ("unique".to_string(), Json::Bool(column.unique)),
                    ("primaryKey".to_string(), Json::Bool(column.primary_key)),
                ];
                if let Some(default) = &column.default {
                    fields.push(("default".to_string(), value_to_json(default)));
                }
                columns.push(Json::Obj(fields));
            }

            let mut indexes = Vec::with_capacity(table.indexes.len());
            for index in &table.indexes {
                // 与 Node 版一致：catalog 只保存显式索引，
                // 主键（__pk__）和列级 UNIQUE（uq_*）由列定义推导，不落库
                if index.primary || index.implicit {
                    continue;
                }
                indexes.push(Json::Obj(vec![
                    ("name".to_string(), Json::str(index.name.clone())),
                    (
                        "columns".to_string(),
                        Json::Arr(index.columns.iter().map(|c| Json::str(c.clone())).collect()),
                    ),
                    ("unique".to_string(), Json::Bool(index.unique)),
                ]));
            }

            tables.push(Json::Obj(vec![
                ("id".to_string(), Json::Num(table.id as f64)),
                ("name".to_string(), Json::str(table.name.clone())),
                ("columns".to_string(), Json::Arr(columns)),
                ("indexes".to_string(), Json::Arr(indexes)),
                (
                    "createdAt".to_string(),
                    Json::str(iso_now()),
                ),
            ]));
        }

        Json::Obj(vec![
            ("version".to_string(), Json::Num(self.version as f64)),
            (
                "nextTableId".to_string(),
                Json::Num(self.next_table_id as f64),
            ),
            ("tables".to_string(), Json::Arr(tables)),
        ])
    }
}

fn parse_table(value: &Json) -> Result<TableDef> {
    let id = value
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| DbError::internal("catalog 损坏：表缺少 id"))? as u32;
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| DbError::internal("catalog 损坏：表缺少 name"))?
        .to_string();

    let mut columns = Vec::new();
    if let Some(Json::Arr(items)) = value.get("columns") {
        for item in items {
            columns.push(parse_column(item)?);
        }
    }
    if columns.is_empty() {
        return Err(DbError::internal(format!("catalog 损坏：表 {} 没有列", name)));
    }

    let mut indexes = Vec::new();
    if let Some(Json::Arr(items)) = value.get("indexes") {
        for item in items {
            let mut cols = Vec::new();
            if let Some(Json::Arr(list)) = item.get("columns") {
                for c in list {
                    if let Some(s) = c.as_str() {
                        cols.push(s.to_string());
                    }
                }
            }
            indexes.push(IndexDef {
                name: item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("idx")
                    .to_string(),
                columns: cols,
                unique: item.get("unique").and_then(|v| v.as_bool()).unwrap_or(false),
                primary: item.get("primary").and_then(|v| v.as_bool()).unwrap_or(false),
                implicit: item.get("implicit").and_then(|v| v.as_bool()).unwrap_or(false),
            });
        }
    }

    Ok(TableDef {
        id,
        name,
        columns,
        indexes,
    })
}

fn parse_column(value: &Json) -> Result<Column> {
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| DbError::internal("catalog 损坏：列缺少 name"))?
        .to_string();
    let type_text = value
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| DbError::internal("catalog 损坏：列缺少 type"))?;
    let ty = ColumnType::parse(type_text)?;

    let default = match value.get("default") {
        Some(json_value) => {
            let raw = value_from_json(json_value);
            if raw.is_null() {
                None
            } else {
                Some(raw)
            }
        }
        None => None,
    };

    Ok(Column {
        name,
        ty,
        not_null: value.get("notNull").and_then(|v| v.as_bool()).unwrap_or(false),
        unique: value.get("unique").and_then(|v| v.as_bool()).unwrap_or(false),
        primary_key: value
            .get("primaryKey")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        default,
    })
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(_) => 0,
    }
}

/// 生成与 Node 版 `new Date().toISOString()` 形状一致的时间串
fn iso_now() -> String {
    let text = format_datetime_from_millis(now_millis());
    // "YYYY-MM-DD HH:MM:SS" -> "YYYY-MM-DDTHH:MM:SS.000Z"
    format!("{}T{}.000Z", &text[..10], &text[11..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_catalog() -> Catalog {
        let mut id = Column::new("id", ColumnType::Int);
        id.primary_key = true;
        id.not_null = true;
        let mut name = Column::new("name", ColumnType::Text);
        name.unique = true;
        name.default = Some(Value::Text("anon".to_string()));

        Catalog {
            version: CATALOG_VERSION,
            next_table_id: 3,
            tables: vec![TableDef {
                id: 2,
                name: "users".to_string(),
                columns: vec![id, name],
                indexes: vec![
                    IndexDef {
                        name: "__pk__".to_string(),
                        columns: vec!["id".to_string()],
                        unique: true,
                        primary: true,
                        implicit: false,
                    },
                    IndexDef {
                        name: "idx_name".to_string(),
                        columns: vec!["name".to_string()],
                        unique: false,
                        primary: false,
                        implicit: false,
                    },
                ],
            }],
        }
    }

    #[test]
    fn json_roundtrip() {
        let catalog = sample_catalog();
        let text = catalog.to_json().to_string();
        let parsed = json::parse(&text).unwrap();

        let back = Catalog {
            version: parsed.get("version").unwrap().as_i64().unwrap(),
            next_table_id: parsed.get("nextTableId").unwrap().as_i64().unwrap() as u32,
            tables: match parsed.get("tables") {
                Some(Json::Arr(items)) => items.iter().map(|i| parse_table(i).unwrap()).collect(),
                _ => Vec::new(),
            },
        };

        assert_eq!(back.next_table_id, 3);
        assert_eq!(back.tables.len(), 1);
        assert_eq!(back.tables[0].name, "users");
        assert_eq!(back.tables[0].columns.len(), 2);
        assert!(back.tables[0].columns[0].primary_key);
        assert_eq!(
            back.tables[0].columns[1].default,
            Some(Value::Text("anon".to_string()))
        );
        // catalog 只保存显式索引：__pk__ 由列定义推导，不落库
        assert_eq!(back.tables[0].indexes.len(), 1, "只保留显式索引");
        assert_eq!(back.tables[0].indexes[0].name, "idx_name");
        assert_eq!(
            back.tables[0].indexes[0].columns,
            vec!["name".to_string()]
        );
    }

    #[test]
    fn missing_file_is_empty_catalog() {
        let path = std::env::temp_dir().join("littledb-catalog-not-exist.json");
        let _ = std::fs::remove_file(&path);
        let catalog = Catalog::load(&path).unwrap();
        assert_eq!(catalog.tables.len(), 0);
        assert_eq!(catalog.next_table_id, 1);
    }

    #[test]
    fn save_and_load() {
        let path = std::env::temp_dir().join(format!(
            "littledb-catalog-test-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        sample_catalog().save(&path).unwrap();
        let loaded = Catalog::load(&path).unwrap();
        assert_eq!(loaded.tables.len(), 1);
        assert_eq!(loaded.find("USERS").unwrap().id, 2);
        assert_eq!(loaded.table_by_id(2).unwrap().name, "users");

        let _ = std::fs::remove_file(&path);
    }
}
