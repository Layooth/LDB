//! 语句执行器。
//!
//! SELECT 管线：取行 → WHERE → 分组 → 投影/聚合 → HAVING → DISTINCT
//!            → ORDER BY → LIMIT/OFFSET。
//! 写语句在 Session 层统一包事务：不在显式事务里就自动 begin/commit。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::codec::{self, Column, ColumnType, Value};
use crate::engine::{Engine, IndexLookup};
use crate::error::{self, DbError, Result};
use crate::table;
use crate::json::Json;

use super::ast::*;
use super::eval::{
    apply_binary, apply_unary, contains_aggregate, eval, eval_aggregate, matches, RowContext,
};
use super::parser::Parser;
use super::AGGREGATES;

/// 引擎读锁：锁中毒时沿用内部数据而不是 panic。
///
/// 中毒只发生在「持锁线程 panic」之后，此时数据本身仍是自洽的
/// （写路径都是临界区内完成、失败即回滚）；为了一个线程的 panic
/// 让整个服务不可用，代价远大于收益。
fn engine_read(engine: &RwLock<Engine>) -> std::sync::RwLockReadGuard<'_, Engine> {
    engine
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 表级写锁：同上，中毒不扩散
fn table_write(guard: &RwLock<()>) -> std::sync::RwLockWriteGuard<'_, ()> {
    guard
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 引擎写锁：同上，中毒不扩散
fn engine_write(engine: &RwLock<Engine>) -> std::sync::RwLockWriteGuard<'_, Engine> {
    engine
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ------------------------------------------------------------------ 结果

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub name: String,
    pub rows: u32,
    pub columns: usize,
    pub pages: u32,
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub ty: String,
    pub primary_key: bool,
    pub not_null: bool,
    pub unique: bool,
    pub default: String,
}

#[derive(Debug, Clone)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    pub implicit: bool,
}

#[derive(Debug, Clone)]
pub enum QueryResult {
    Select {
        columns: Vec<String>,
        /// 每列的类型名（INT/REAL/TEXT/BOOL/DATE/DATETIME/JSON/DECIMAL/UNKNOWN）。
        /// 由查询计划推定，客户端不必再反查 schema；推不出的列是 "UNKNOWN"。
        types: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    Insert {
        inserted: usize,
        /// UPSERT 命中的更新行数
        updated: usize,
        /// `ON CONFLICT DO NOTHING` 跳过的行数
        skipped: usize,
        /// 自增列分配出的值（表里没有自增列时为空）
        ids: Vec<i64>,
    },
    Update {
        updated: usize,
    },
    Delete {
        deleted: usize,
    },
    Ddl {
        message: String,
    },
    Transaction {
        message: String,
    },
    Tables {
        tables: Vec<TableInfo>,
    },
    Describe {
        table: String,
        rows: u32,
        columns: Vec<ColumnInfo>,
        indexes: Vec<IndexInfo>,
    },
    Explain {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    Batch {
        results: Vec<QueryResult>,
    },
}

impl QueryResult {
    /// 转成二维表格用于展示；非结果集返回 None
    pub fn to_grid(&self) -> Option<(Vec<String>, Vec<Vec<Value>>)> {
        match self {
            QueryResult::Select { columns, rows, .. } => Some((columns.clone(), rows.clone())),
            QueryResult::Batch { .. } => None,
            QueryResult::Explain { columns, rows } => Some((columns.clone(), rows.clone())),
            QueryResult::Tables { tables } => {
                let rows = tables
                    .iter()
                    .map(|t| {
                        vec![
                            Value::Text(t.name.clone()),
                            Value::Int(t.columns as i64),
                            Value::Int(t.rows as i64),
                        ]
                    })
                    .collect();
                Some((
                    vec!["表名".to_string(), "列数".to_string(), "行数".to_string()],
                    rows,
                ))
            }
            QueryResult::Describe { columns, .. } => {
                let rows = columns
                    .iter()
                    .map(|c| {
                        vec![
                            Value::Text(c.name.clone()),
                            Value::Text(c.ty.clone()),
                            Value::Text(if c.primary_key { "是".into() } else { String::new() }),
                            Value::Text(if c.not_null { "是".into() } else { String::new() }),
                            Value::Text(if c.unique { "是".into() } else { String::new() }),
                            Value::Text(c.default.clone()),
                        ]
                    })
                    .collect();
                Some((
                    vec![
                        "列名".to_string(),
                        "类型".to_string(),
                        "主键".to_string(),
                        "非空".to_string(),
                        "唯一".to_string(),
                        "默认值".to_string(),
                    ],
                    rows,
                ))
            }
            _ => None,
        }
    }

    pub fn summary(&self) -> String {
        match self {
            QueryResult::Select { rows, .. } => format!("{} 行", rows.len()),
            QueryResult::Explain { rows, .. } => format!("{} 步", rows.len()),
            QueryResult::Insert {
                inserted,
                updated,
                skipped,
                ..
            } => {
                if *updated == 0 && *skipped == 0 {
                    format!("插入 {} 行", inserted)
                } else {
                    format!("插入 {} 行，更新 {} 行，跳过 {} 行", inserted, updated, skipped)
                }
            }
            QueryResult::Update { updated } => format!("更新 {} 行", updated),
            QueryResult::Delete { deleted } => format!("删除 {} 行", deleted),
            QueryResult::Ddl { message } => message.clone(),
            QueryResult::Transaction { message } => message.clone(),
            QueryResult::Tables { tables } => format!("{} 张表", tables.len()),
            QueryResult::Describe { table, columns, .. } => {
                format!("表 {} · {} 列", table, columns.len())
            }
            QueryResult::Batch { results } => format!("批量执行 {} 条语句", results.len()),
        }
    }

    /// 转成 LDBP/1 协议里的 result 字段。
    /// 字段名即线协议契约，客户端按此解析。
    pub fn to_wire_json(&self) -> Json {
        match self {
            QueryResult::Select {
                columns,
                types,
                rows,
            } => Json::Obj(vec![
                ("type".to_string(), Json::str("select")),
                (
                    "columns".to_string(),
                    Json::Arr(columns.iter().map(|c| Json::str(c.clone())).collect()),
                ),
                // 与 columns 一一对应；客户端据此决定如何解析值
                // （尤其 DECIMAL 是字符串还是数字、DATE 是文本）
                (
                    "types".to_string(),
                    Json::Arr(types.iter().map(|t| Json::str(t.clone())).collect()),
                ),
                ("rows".to_string(), rows_to_json(rows)),
                ("rowCount".to_string(), Json::Num(rows.len() as f64)),
            ]),
            QueryResult::Insert {
                inserted,
                updated,
                skipped,
                ids,
            } => Json::Obj(vec![
                ("type".to_string(), Json::str("insert")),
                ("inserted".to_string(), Json::Num(*inserted as f64)),
                ("updated".to_string(), Json::Num(*updated as f64)),
                ("skipped".to_string(), Json::Num(*skipped as f64)),
                // 自增列分配出的值；没有自增列时是空数组、lastId 为 null
                (
                    "ids".to_string(),
                    Json::Arr(ids.iter().map(|id| Json::Int(*id)).collect()),
                ),
                (
                    "lastId".to_string(),
                    match ids.last() {
                        Some(id) => Json::Int(*id),
                        None => Json::Null,
                    },
                ),
            ]),
            QueryResult::Update { updated } => Json::Obj(vec![
                ("type".to_string(), Json::str("update")),
                ("updated".to_string(), Json::Num(*updated as f64)),
            ]),
            QueryResult::Delete { deleted } => Json::Obj(vec![
                ("type".to_string(), Json::str("delete")),
                ("deleted".to_string(), Json::Num(*deleted as f64)),
            ]),
            QueryResult::Ddl { message } => Json::Obj(vec![
                ("type".to_string(), Json::str("ddl")),
                ("message".to_string(), Json::str(message.clone())),
            ]),
            QueryResult::Transaction { message } => Json::Obj(vec![
                ("type".to_string(), Json::str("transaction")),
                ("message".to_string(), Json::str(message.clone())),
            ]),
            QueryResult::Tables { tables } => Json::Obj(vec![
                ("type".to_string(), Json::str("tables")),
                (
                    "tables".to_string(),
                    Json::Arr(
                        tables
                            .iter()
                            .map(|t| {
                                Json::Obj(vec![
                                    ("name".to_string(), Json::str(t.name.clone())),
                                    ("rows".to_string(), Json::Num(t.rows as f64)),
                                    ("columns".to_string(), Json::Num(t.columns as f64)),
                                    ("pages".to_string(), Json::Num(t.pages as f64)),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]),
            QueryResult::Describe {
                table,
                rows,
                columns,
                indexes,
            } => Json::Obj(vec![
                ("type".to_string(), Json::str("describe")),
                (
                    "table".to_string(),
                    Json::Obj(vec![
                        ("name".to_string(), Json::str(table.clone())),
                        ("rows".to_string(), Json::Num(*rows as f64)),
                        (
                            "columns".to_string(),
                            Json::Arr(
                                columns
                                    .iter()
                                    .map(|c| {
                                        Json::Obj(vec![
                                            ("name".to_string(), Json::str(c.name.clone())),
                                            ("type".to_string(), Json::str(c.ty.clone())),
                                            ("notNull".to_string(), Json::Bool(c.not_null)),
                                            ("primaryKey".to_string(), Json::Bool(c.primary_key)),
                                            ("unique".to_string(), Json::Bool(c.unique)),
                                            ("default".to_string(), Json::str(c.default.clone())),
                                        ])
                                    })
                                    .collect(),
                            ),
                        ),
                        (
                            "indexes".to_string(),
                            Json::Arr(
                                indexes
                                    .iter()
                                    .map(|i| {
                                        Json::Obj(vec![
                                            ("name".to_string(), Json::str(i.name.clone())),
                                            (
                                                "columns".to_string(),
                                                Json::Arr(
                                                    i.columns
                                                        .iter()
                                                        .map(|c| Json::str(c.clone()))
                                                        .collect(),
                                                ),
                                            ),
                                            ("unique".to_string(), Json::Bool(i.unique)),
                                            ("primary".to_string(), Json::Bool(i.primary)),
                                            ("implicit".to_string(), Json::Bool(i.implicit)),
                                        ])
                                    })
                                    .collect(),
                            ),
                        ),
                    ]),
                ),
            ]),
            QueryResult::Explain { columns, rows } => Json::Obj(vec![
                ("type".to_string(), Json::str("explain")),
                (
                    "columns".to_string(),
                    Json::Arr(columns.iter().map(|c| Json::str(c.clone())).collect()),
                ),
                ("rows".to_string(), rows_to_json(rows)),
            ]),
            QueryResult::Batch { results } => Json::Obj(vec![
                ("type".to_string(), Json::str("batch")),
                (
                    "results".to_string(),
                    Json::Arr(results.iter().map(|r| r.to_wire_json()).collect()),
                ),
            ]),
        }
    }
}

fn value_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Int(n) => Json::Int(*n),
        Value::Real(n) => Json::Num(*n),
        // DECIMAL 走字符串保真（金额精度不折损）
        Value::Decimal { v, scale } => {
            Json::Str(crate::codec::format_decimal(*v, *scale))
        }
        Value::Text(s) => Json::Str(s.clone()),
        Value::Bool(b) => Json::Bool(*b),
    }
}

fn rows_to_json(rows: &[Vec<Value>]) -> Json {
    Json::Arr(
        rows.iter()
            .map(|row| Json::Arr(row.iter().map(value_json).collect()))
            .collect(),
    )
}

// ------------------------------------------------------------------ 表达式标签

fn expr_label(expr: &Expr) -> String {
    match expr {
        Expr::Column { name, .. } => name.clone(),
        Expr::Literal(literal) => match literal {
            Literal::Null => "NULL".to_string(),
            Literal::Bool(b) => (if *b { "TRUE" } else { "FALSE" }).to_string(),
            Literal::Int(n) => n.to_string(),
            Literal::Real(n) => n.to_string(),
            Literal::Decimal { v, scale } => codec::format_decimal(*v, *scale),
            Literal::Text(s) => format!("'{}'", s),
        },
        Expr::Param(index) => format!("?{}", *index + 1),
        Expr::Function { name, star, .. } => {
            if *star {
                format!("{}(*)", name)
            } else {
                format!("{}()", name)
            }
        }
        Expr::Binary { op, left, right } => {
            format!("{} {} {}", expr_label(left), op_symbol(*op), expr_label(right))
        }
        Expr::Unary { op, operand } => match op {
            UnOp::Neg => format!("-{}", expr_label(operand)),
            UnOp::Plus => format!("+{}", expr_label(operand)),
            UnOp::Not => format!("NOT {}", expr_label(operand)),
        },
        Expr::InSubquery { expr, .. } => format!("{} IN (subquery)", expr_label(expr)),
        Expr::Cast { expr, ty, .. } => format!("CAST({} AS {})", expr_label(expr), ty.as_str()),
        _ => "expr".to_string(),
    }
}

fn op_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Eq => "=",
        BinOp::Ne => "<>",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "AND",
        BinOp::Or => "OR",
    }
}

// ------------------------------------------------------------------ 会话

pub struct Session {
    pub engine: Engine,
    in_txn: bool,
}

impl Session {
    pub fn open(dir: &Path) -> Result<Session> {
        Ok(Session {
            engine: Engine::open(dir)?,
            in_txn: false,
        })
    }

    pub fn in_transaction(&self) -> bool {
        self.in_txn
    }

    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        let statements = Parser::parse_batch(sql)?;
        if statements.len() == 1 {
            return self.execute_statement(&statements[0], params);
        }
        let mut results = Vec::new();
        for stmt in &statements {
            results.push(self.execute_statement(stmt, params)?);
        }
        Ok(QueryResult::Batch { results })
    }

    pub fn execute_statement(
        &mut self,
        statement: &Statement,
        params: &[Value],
    ) -> Result<QueryResult> {
        let outcome = execute_on(
            &mut self.engine,
            &mut self.in_txn,
            statement,
            params,
            crate::engine::EXCLUSIVE_TXN_OWNER,
        );
        // 独占引擎：full 模式下同样等待 fsync 确认再返回
        let barrier = self.engine.store.commit_barrier();
        let seq = self.engine.store.last_commit_seq();
        if self.engine.store.durable_commits() {
            barrier.wait_for(seq)?;
        }
        outcome
    }

    pub fn close(&mut self) -> Result<()> {
        if self.in_txn {
            self.engine.rollback(crate::engine::EXCLUSIVE_TXN_OWNER);
            self.in_txn = false;
        }
        self.engine.close()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.in_txn {
            self.engine.rollback(crate::engine::EXCLUSIVE_TXN_OWNER);
        }
    }
}

/// 服务端使用的会话：多连接共享同一个引擎。
///
/// 锁策略（实测结论，见 docs/mvcc-design.md §3.9）：
/// - **非事务只读语句**拿引擎读锁 → 多个会话的 SELECT 真正并行；
/// - **任何写语句 / 事务控制 / 事务内语句**整条持有引擎写锁 → 写语句串行。
///   语句执行全程需要 `&mut Engine`，在不动存储层内部可变性的前提下，
///   「写锁外构建脏页」无法实现，所以多写的收益体现在**事务级交错**
///   （两个会话可各自持有开放事务、互不 DATABASE_LOCKED），而不是语句级并行。
/// 行可见性由会话 id 决定（read_shared 的 owner 检查）。
pub struct ConnSession {
    engine: Arc<RwLock<Engine>>,
    session_id: u64,
    in_txn: bool,
}

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

impl ConnSession {
    pub fn new(engine: Arc<RwLock<Engine>>) -> ConnSession {
        ConnSession {
            engine,
            session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::SeqCst),
            in_txn: false,
        }
    }

    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        let statements = Parser::parse_batch(sql)?;
        let needs_write = statements.iter().any(|s| {
            !s.is_read_only()
                || self.in_txn
                || matches!(s, Statement::Begin | Statement::Commit | Statement::Rollback)
        });

        if needs_write {
            let (outcome, barrier, seq) = {
                let mut guard = engine_write(&self.engine);
                let mut results = Vec::new();
                for stmt in &statements {
                    let r = execute_on(
                        &mut *guard,
                        &mut self.in_txn,
                        stmt,
                        params,
                        self.session_id,
                    )?;
                    results.push(r);
                }
                let barrier = guard.store.commit_barrier();
                let seq = guard.store.last_commit_seq();
                // 单语句直接返回该语句的结果，不再套一层 batch：
                // 客户端（SDK/控制台/curl）解析 `result.type` 就行，
                // 不用先判 `batch` 再取 `results[0]`。多语句仍然是 batch。
                let outcome = if results.len() == 1 {
                    results.pop().expect("刚判断过长度")
                } else {
                    QueryResult::Batch { results }
                };
                (outcome, barrier, seq)
            };
            if engine_read(&self.engine).store.durable_commits() {
                barrier.wait_for(seq)?;
            }
            Ok(outcome)
        } else {
            let guard = engine_read(&self.engine);
            let committed_only = guard.store.in_txn();
            let mut results = Vec::new();
            for stmt in &statements {
                if stmt.is_read_only() {
                    results.push(run_read_only(&guard, stmt, params, committed_only, self.session_id)?);
                }
            }
            if results.len() == 1 {
                return Ok(results.pop().unwrap());
            }
            Ok(QueryResult::Batch { results })
        }
    }

    pub fn in_transaction(&self) -> bool {
        self.in_txn
    }

    pub fn close(&mut self) {
        if self.in_txn {
            // 中毒也要回滚：否则未提交事务会一直挂在 store.txns 里
            let mut guard = engine_write(&self.engine);
            guard.rollback(self.session_id);
            self.in_txn = false;
        }
    }
}

impl Drop for ConnSession {
    fn drop(&mut self) {
        self.close();
    }
}

/// 语句执行的统一入口：Session（独占引擎）与 ConnSession（共享引擎）都走这里
pub fn execute_on(
    engine: &mut Engine,
    in_txn: &mut bool,
    statement: &Statement,
    params: &[Value],
    owner: u64,
) -> Result<QueryResult> {
    // 写路径的脏页/索引操作都记到本语句属主名下（多写下必须逐语句设置）
    engine.store.set_active_owner(owner);
    match statement {
        Statement::Begin => {
            if *in_txn {
                return Ok(QueryResult::Transaction {
                    message: "事务已在进行中".to_string(),
                });
            }
            engine.begin(owner)?;
            *in_txn = true;
            return Ok(QueryResult::Transaction {
                message: "事务已开始".to_string(),
            });
        }
        Statement::Commit => {
            if !*in_txn {
                return Ok(QueryResult::Transaction {
                    message: "当前没有进行中的事务".to_string(),
                });
            }
            let outcome = engine.commit(owner);
            *in_txn = false;
            outcome?;
            return Ok(QueryResult::Transaction {
                message: "事务已提交".to_string(),
            });
        }
        Statement::Rollback => {
            if !*in_txn {
                return Ok(QueryResult::Transaction {
                    message: "当前没有进行中的事务".to_string(),
                });
            }
            engine.rollback(owner);
            *in_txn = false;
            return Ok(QueryResult::Transaction {
                message: "事务已回滚".to_string(),
            });
        }
        _ => {}
    }

    if statement.is_read_only() || *in_txn {
        // 其它会话持有写事务时，本会话的只读语句只能看已提交数据
        let committed_only = !*in_txn && engine.store.in_txn();
        return run_with_engine(engine, statement, params, committed_only, owner);
    }

    // 单语句隐式事务（属主 = 会话 id，DDL 门禁据此识别自己人）
    let outcome = engine.write_as(owner, |inner| {
        run_with_engine(inner, statement, params, false, owner)
    });
    if outcome.is_err() {
        *in_txn = false;
    }
    outcome
}

fn run_with_engine(
    engine: &mut Engine,
    statement: &Statement,
    params: &[Value],
    committed_only: bool,
    owner: u64,
) -> Result<QueryResult> {
    match statement {
        Statement::Select(select) => {
            exec_select(engine, select, params, committed_only, Some(owner), None)
        }
        Statement::Insert(insert) => exec_insert(engine, insert, params, owner),
        Statement::Update(update) => exec_update(engine, update, params, owner),
        Statement::Delete(delete) => exec_delete(engine, delete, params, owner),
        Statement::CreateTable(create) => {
            engine.ensure_ddl_exclusive(owner)?;
            exec_create_table(engine, create, params)
        }
        Statement::DropTable { name, if_exists } => {
            engine.ensure_ddl_exclusive(owner)?;
            engine.drop_table(name, *if_exists)?;
            Ok(QueryResult::Ddl {
                message: format!("已删除表 \"{}\"", name),
            })
        }
        Statement::CreateIndex(create) => {
            engine.ensure_ddl_exclusive(owner)?;
            engine.create_index(
                &create.name,
                &create.table,
                create.columns.clone(),
                create.unique,
                create.if_not_exists,
            )?;
            Ok(QueryResult::Ddl {
                message: format!(
                    "已{}在 {} 上创建索引 \"{}\"",
                    if create.unique { "唯一" } else { "" },
                    create.table,
                    create.name
                ),
            })
        }
        Statement::DropIndex { name, if_exists } => {
            engine.ensure_ddl_exclusive(owner)?;
            engine.drop_index(name, *if_exists)?;
            Ok(QueryResult::Ddl {
                message: format!("已删除索引 \"{}\"", name),
            })
        }
        Statement::AlterTable { table, action } => {
            engine.ensure_ddl_exclusive(owner)?;
            exec_alter_table(engine, table, action, params)
        }
        Statement::ShowTables | Statement::Describe { .. } | Statement::Explain { .. } => {
            run_read_only(engine, statement, params, committed_only, owner)
        }
        Statement::Begin | Statement::Commit | Statement::Rollback => {
            Err(DbError::internal("事务控制语句不应到达执行器"))
        }
    }
}

/// 只读语句的 `&Engine` 执行路径：读锁（并发只读）下使用。
/// 与 run_with_engine 的对应分支语义一致，但全部走共享视图。
fn run_read_only(
    engine: &Engine,
    statement: &Statement,
    params: &[Value],
    committed_only: bool,
    owner: u64,
) -> Result<QueryResult> {
    match statement {
        Statement::Select(select) => {
            exec_select(engine, select, params, committed_only, Some(owner), None)
        }
        Statement::ShowTables => {
            let mut tables = Vec::new();
            for name in engine.table_names() {
                let (table_id, rows, columns) = {
                    let table = engine.table(&name)?;
                    (table.id, table.row_count, table.columns.len())
                };
                let pages = engine.store.page_count_shared(table_id);
                tables.push(TableInfo {
                    name,
                    rows,
                    columns,
                    pages,
                });
            }
            Ok(QueryResult::Tables { tables })
        }
        Statement::Describe { table } => {
            let (name, rows, columns, indexes) = {
                let target = engine.table(table)?;
                let columns = target
                    .columns
                    .iter()
                    .map(|c| ColumnInfo {
                        name: c.name.clone(),
                        ty: c.ty.as_str().to_string(),
                        primary_key: c.primary_key,
                        not_null: c.not_null,
                        unique: c.unique,
                        default: match &c.default {
                            Some(v) => v.as_text(),
                            None => String::new(),
                        },
                    })
                    .collect();
                let indexes = target
                    .indexes
                    .iter()
                    .map(|i| IndexInfo {
                        name: i.name.clone(),
                        columns: i
                            .columns
                            .iter()
                            .map(|ci| target.columns[*ci].name.clone())
                            .collect(),
                        unique: i.unique,
                        primary: i.primary,
                        implicit: i.implicit,
                    })
                    .collect();
                (target.name.clone(), target.row_count, columns, indexes)
            };
            Ok(QueryResult::Describe {
                table: name,
                rows,
                columns,
                indexes,
            })
        }
        Statement::Explain { statement } => exec_explain(engine, statement, params, owner),
        _ => Err(DbError::internal("run_read_only 只接受只读语句")),
    }
}

// ------------------------------------------------------------------ SELECT

/// SELECT 执行管线。pub(crate) 供 eval 模块的线路精确性测试复用
/// 单个 FROM 叶子的执行计划
#[derive(Debug, Clone)]
struct TablePlan {
    /// 小写别名（无别名时为表名），限定名匹配用
    alias: String,
    /// 展示名（错误信息用）
    #[allow(dead_code)]
    display: String,
    /// 真实表名（engine 访问用）
    table_name: String,
    columns: Vec<Column>,
    /// 本表列在组合行中的起始下标
    offset: usize,
}

/// 无索引辅助的 JOIN 组合行数上限，超过直接报错而不是挂死
const MAX_JOIN_PAIRS: usize = 2_000_000;

/// 单条 SELECT 允许物化的行数上限。走索引或 LIMIT 下推的查询碰不到它，
/// 只有「全表扫描 + 无 LIMIT」才会，此时报错比把服务端内存打满更好。
const MAX_SCAN_ROWS: usize = 500_000;

fn scan_limit_error() -> DbError {
    DbError::new(
        error::LIMIT,
        format!(
            "语句需要处理超过 {} 行，请加 WHERE 条件（SELECT 也可用 LIMIT）缩小范围",
            MAX_SCAN_ROWS
        ),
    )
}

/// 叶子计划 → (别名, 列, 组合行内的起始下标)
fn plan_sources(plans: &[TablePlan]) -> Vec<(&str, &[Column], usize)> {
    plans
        .iter()
        .map(|p| (p.alias.as_str(), p.columns.as_slice(), p.offset))
        .collect()
}

/// LIMIT 下推的适用条件：无 ORDER BY / GROUP BY / HAVING / DISTINCT / UNION / 聚合，
/// 结果顺序就等于扫描顺序。返回扫描阶段需要凑够的行数（offset + limit）。
fn early_row_cap(stmt: &SelectStmt, params: &[Value]) -> Option<usize> {
    if stmt.union.is_some()
        || stmt.distinct
        || stmt.having.is_some()
        || !stmt.order_by.is_empty()
        || !stmt.group_by.is_empty()
    {
        return None;
    }
    if stmt.items.iter().any(|item| contains_aggregate(&item.expr))
        || stmt.order_by.iter().any(|o| contains_aggregate(&o.expr))
    {
        return None;
    }
    let empty = RowContext::new(&[], Vec::new());
    let limit = to_usize(&eval(stmt.limit.as_ref()?, &empty, params).ok()?).ok()?;
    let offset = match &stmt.offset {
        Some(expr) => to_usize(&eval(expr, &empty, params).ok()?).ok()?,
        None => 0,
    };
    Some(offset.saturating_add(limit))
}

/// 把表达式里的 `IN (SELECT ...)` 执行成 `IN (值列表)`（非关联子查询）。
fn resolve_subqueries(
    engine: &Engine,
    expr: &Expr,
    params: &[Value],
    committed_only: bool,
    owner: Option<u64>,
) -> Result<Expr> {
    match expr {
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => {
            let inner = resolve_subqueries(engine, expr, params, committed_only, owner)?;
            let result = exec_select(engine, query, params, committed_only, owner, None)?;
            let values = match result {
                QueryResult::Select {
                    columns, rows, ..
                } => {
                    if columns.len() != 1 {
                        return Err(DbError::syntax(format!(
                            "IN 子查询只能返回一列，实际 {} 列",
                            columns.len()
                        )));
                    }
                    let mut values = Vec::with_capacity(rows.len());
                    for row in rows {
                        values.push(match row.into_iter().next() {
                            Some(Value::Null) => Literal::Null,
                            Some(Value::Int(n)) => Literal::Int(n),
                            Some(Value::Real(n)) => Literal::Real(n),
                            // 保持 DECIMAL 类型：转成 Text 会让 `decimal 列 IN (子查询)`
                            // 变成 TEXT 与 DECIMAL 比较，恒定不命中
                            Some(Value::Decimal { v, scale }) => Literal::Decimal { v, scale },
                            Some(Value::Text(s)) => Literal::Text(s),
                            Some(Value::Bool(b)) => Literal::Bool(b),
                            None => Literal::Null,
                        });
                    }
                    values
                }
                other => {
                    return Err(DbError::syntax(format!(
                        "IN 子查询必须是 SELECT，实际是 {}",
                        other.summary()
                    )))
                }
            };
            Ok(Expr::InList {
                expr: Box::new(inner),
                list: values.into_iter().map(Expr::Literal).collect(),
                negated: *negated,
            })
        }
        Expr::Binary { op, left, right } => Ok(Expr::Binary {
            op: *op,
            left: Box::new(resolve_subqueries(engine, left, params, committed_only, owner)?),
            right: Box::new(resolve_subqueries(engine, right, params, committed_only, owner)?),
        }),
        Expr::Unary { op, operand } => Ok(Expr::Unary {
            op: *op,
            operand: Box::new(resolve_subqueries(engine, operand, params, committed_only, owner)?),
        }),
        Expr::IsNull { expr, negated } => Ok(Expr::IsNull {
            expr: Box::new(resolve_subqueries(engine, expr, params, committed_only, owner)?),
            negated: *negated,
        }),
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Ok(Expr::Like {
            expr: Box::new(resolve_subqueries(engine, expr, params, committed_only, owner)?),
            pattern: Box::new(resolve_subqueries(
                engine,
                pattern,
                params,
                committed_only,
                owner,
            )?),
            negated: *negated,
        }),
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Ok(Expr::Between {
            expr: Box::new(resolve_subqueries(engine, expr, params, committed_only, owner)?),
            low: Box::new(resolve_subqueries(engine, low, params, committed_only, owner)?),
            high: Box::new(resolve_subqueries(engine, high, params, committed_only, owner)?),
            negated: *negated,
        }),
        Expr::InList { expr, list, negated } => Ok(Expr::InList {
            expr: Box::new(resolve_subqueries(engine, expr, params, committed_only, owner)?),
            list: list
                .iter()
                .map(|e| resolve_subqueries(engine, e, params, committed_only, owner))
                .collect::<Result<Vec<_>>>()?,
            negated: *negated,
        }),
        Expr::Cast { expr, ty, scale } => Ok(Expr::Cast {
            expr: Box::new(resolve_subqueries(engine, expr, params, committed_only, owner)?),
            ty: *ty,
            scale: *scale,
        }),
        other => Ok(other.clone()),
    }
}

/// SELECT 管线：输出列 + (投影值, 排序键) 对，尚未做 ORDER BY / LIMIT / UNION
fn select_base(
    engine: &Engine,
    stmt: &SelectStmt,
    params: &[Value],
    committed_only: bool,
    owner: Option<u64>,
    outer: Option<std::rc::Rc<RowContext>>,
) -> Result<(Vec<String>, Vec<String>, Vec<(Vec<Value>, Vec<Value>)>)> {
    // ---------------------------------------------------------------- 行来源
    // 先解析 WHERE 里的子查询（索引候选与行过滤都用转换后的条件）
    let where_clause = match &stmt.where_clause {
        Some(expr) => Some(resolve_subqueries(engine, expr, params, committed_only, owner)?),
        None => None,
    };
    let mut plans: Vec<TablePlan> = Vec::new();
    let mut raw_rows: Vec<Vec<Value>> = Vec::new();
    // LIMIT 下推的流式扫描：结果已经是过滤过的 RowContext（WHERE 已在扫描里应用）
    let mut streamed_rows: Option<Vec<RowContext>> = None;
    let mut where_applied = false;

    if let Some(source) = &stmt.from {
        match source {
            // 单表 + 本会话可见未提交数据：等值条件可走内存索引
            TableSource::Table { name, alias } if !committed_only => {
                let table = engine.table(name)?;
                let display = alias.clone().unwrap_or_else(|| table.name.clone());
                plans.push(TablePlan {
                    alias: display.to_lowercase(),
                    display,
                    table_name: table.name.clone(),
                    columns: table.columns.clone(),
                    offset: 0,
                });
                // 等值索引 → 范围索引 → 全表扫描，逐级降级
                let candidates =
                    index_candidates(engine, name, where_clause.as_ref(), params, owner)?;
                let range_ids = if candidates.is_none() {
                    range_candidates(engine, name, where_clause.as_ref(), params)?
                } else {
                    None
                };
                match candidates.map(|lookup| lookup.ids).or(range_ids) {
                    Some(ids) => {
                        for id in ids {
                            if raw_rows.len() > MAX_SCAN_ROWS {
                                return Err(scan_limit_error());
                            }
                            if let Some(values) = engine.get_shared(name, id, owner)? {
                                raw_rows.push(values);
                            }
                        }
                    }
                    // 无排序/分组/聚合时把 LIMIT 推到扫描里：边扫边过滤，
                    // 凑够 offset+limit 行就停，不必把整表物化进内存
                    None => match early_row_cap(stmt, params) {
                        Some(cap) => {
                            let sources = plan_sources(&plans);
                            let mut kept: Vec<RowContext> = Vec::new();
                            table::scan_shared_while(
                                &engine.store,
                                table,
                                owner,
                                |_row_id, values| {
                                    let row = RowContext::from_tables(&sources, values)
                                        .with_parent(outer.clone());
                                    if let Some(where_clause) = &where_clause {
                                        if !matches(&eval_condition(
                                            engine,
                                            where_clause,
                                            &row,
                                            params,
                                            committed_only,
                                            owner,
                                        )?) {
                                            return Ok(true);
                                        }
                                    }
                                    kept.push(row);
                                    Ok(kept.len() < cap)
                                },
                            )?;
                            streamed_rows = Some(kept);
                            where_applied = true;
                        }
                        None => {
                            for (_, values) in engine.scan_shared(name, owner)? {
                                raw_rows.push(values);
                            }
                            if raw_rows.len() > MAX_SCAN_ROWS {
                                return Err(scan_limit_error());
                            }
                        }
                    },
                }
            }
            // JOIN 树，或 committed_only 的单表（绕开含未提交键的索引）
            source => {
                let (p, rows) =
                    join_frame(engine, source, committed_only, params, owner, outer.clone())?;
                plans = p;
                raw_rows = rows;
            }
        }
    } else {
        raw_rows.push(Vec::new());
    }

    let sources = plan_sources(&plans);
    let mut rows: Vec<RowContext> = match streamed_rows {
        Some(rows) => rows,
        None => raw_rows
            .into_iter()
            .map(|values| RowContext::from_tables(&sources, values).with_parent(outer.clone()))
            .collect(),
    };

    // ---------------------------------------------------------------- WHERE
    // （LIMIT 下推时 WHERE 已经在扫描过程中逐行应用过）
    if let Some(where_clause) = &where_clause {
        if !where_applied {
            let mut kept = Vec::with_capacity(rows.len());
            for row in rows {
                let value = eval_condition(engine, where_clause, &row, params, committed_only, owner)?;
                if matches(&value) {
                    kept.push(row);
                }
            }
            rows = kept;
        }
    }

    // ---------------------------------------------------------------- 投影列
    let mut out_columns: Vec<String> = Vec::new();
    let mut projections: Vec<Expr> = Vec::new();
    for item in &stmt.items {
        if item.star {
            // `*` 展开全部表；`t.*` 只展开限定表
            let targets: Vec<&TablePlan> = match &item.star_table {
                Some(t) => {
                    let t_lc = t.to_lowercase();
                    let found: Vec<&TablePlan> =
                        plans.iter().filter(|p| p.alias == t_lc).collect();
                    if found.is_empty() {
                        return Err(DbError::not_found(format!(
                            "表或别名 \"{}\" 不在本次查询中",
                            t
                        )));
                    }
                    found
                }
                None => plans.iter().collect(),
            };
            for plan in targets {
                for column in &plan.columns {
                    out_columns.push(column.name.clone());
                    projections.push(Expr::Column {
                        table: Some(plan.alias.clone()),
                        name: column.name.clone(),
                    });
                }
            }
        } else {
            out_columns.push(match &item.alias {
                Some(alias) => alias.clone(),
                None => expr_label(&item.expr),
            });
            projections.push(item.expr.clone());
        }
    }

    let has_aggregate = projections.iter().any(contains_aggregate)
        || stmt
            .having
            .as_ref()
            .map(contains_aggregate)
            .unwrap_or(false)
        || stmt.order_by.iter().any(|o| contains_aggregate(&o.expr));

    // ---------------------------------------------------------------- 分组
    let groups: Vec<Vec<RowContext>> = if !stmt.group_by.is_empty() {
        group_rows(rows, &stmt.group_by, params)?
    } else if has_aggregate {
        if rows.is_empty() {
            vec![Vec::new()]
        } else {
            vec![rows]
        }
    } else {
        rows.into_iter().map(|row| vec![row]).collect()
    };

    // -------------------------------------------- 投影 + HAVING + 排序键
    let mut output: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    for group in &groups {
        if let Some(having) = &stmt.having {
            let value = eval_in_group(having, group, params)?;
            if !matches(&value) {
                continue;
            }
        }
        let mut values = Vec::with_capacity(projections.len());
        for projection in &projections {
            values.push(eval_in_group(projection, group, params)?);
        }
        let mut sort_keys = Vec::with_capacity(stmt.order_by.len());
        for order in &stmt.order_by {
            sort_keys.push(eval_in_group(&order.expr, group, params)?);
        }
        output.push((values, sort_keys));
    }

    // ---------------------------------------------------------------- DISTINCT
    if stmt.distinct {
        dedup_pairs(&mut output);
    }

    // 输出列类型：从投影表达式 + 源表列推定（供线路 JSON 的 types 字段）
    let out_types: Vec<String> = projections
        .iter()
        .map(|expr| infer_expr_type(expr, &plans))
        .collect();

    Ok((out_columns, out_types, output))
}

/// 推定一个表达式的结果类型名。
///
/// 这是**尽力而为**的声明级推断（不是运行期观测）：能从源表列、字面量、
/// CAST 目标或函数名确定的就给准确值，否则给 "UNKNOWN"——
/// 客户端看到 UNKNOWN 时应回退到 /api/schema 或按值判断。
fn infer_expr_type(expr: &Expr, plans: &[TablePlan]) -> String {
    match expr {
        Expr::Column { table, name } => {
            let want_alias = table.as_ref().map(|t| t.to_lowercase());
            for plan in plans {
                if let Some(alias) = &want_alias {
                    if &plan.alias != alias {
                        continue;
                    }
                }
                if let Some(column) = plan
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(name))
                {
                    return column.ty.as_str().to_string();
                }
            }
            "UNKNOWN".to_string()
        }
        Expr::Literal(literal) => match literal {
            Literal::Null => "NULL".to_string(),
            Literal::Bool(_) => "BOOL".to_string(),
            Literal::Int(_) => "INT".to_string(),
            Literal::Real(_) => "REAL".to_string(),
            Literal::Decimal { .. } => "DECIMAL".to_string(),
            Literal::Text(_) => "TEXT".to_string(),
        },
        Expr::Unary { operand, .. } => infer_expr_type(operand, plans),
        Expr::Cast { ty, .. } => ty.as_str().to_string(),
        Expr::Binary { op, left, right } => match op {
            BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::And
            | BinOp::Or => "BOOL".to_string(),
            _ => {
                // 算术：REAL/DECIMAL 优先，其次 INT，最后跟随左操作数
                let left_ty = infer_expr_type(left, plans);
                let right_ty = infer_expr_type(right, plans);
                for candidate in ["REAL", "DECIMAL"] {
                    if left_ty == candidate || right_ty == candidate {
                        return candidate.to_string();
                    }
                }
                if left_ty == "INT" || right_ty == "INT" {
                    "INT".to_string()
                } else {
                    left_ty
                }
            }
        },
        Expr::IsNull { .. }
        | Expr::InList { .. }
        | Expr::InSubquery { .. }
        | Expr::Like { .. }
        | Expr::Exists { .. }
        | Expr::Between { .. } => "BOOL".to_string(),
        Expr::Function { name, args, .. } => {
            let upper = name.to_uppercase();
            match upper.as_str() {
                "COUNT" | "LENGTH" | "YEAR" | "MONTH" | "DAY" | "HOUR" | "MINUTE" | "SECOND"
                | "DATEDIFF" | "JSON_ARRAY_LENGTH" => "INT".to_string(),
                "JSON_VALID" => "BOOL".to_string(),
                "JSON_TYPE" => "TEXT".to_string(),
                "AVG" => "REAL".to_string(),
                "UPPER" | "LOWER" | "NOW" | "CURRENT_TIMESTAMP" | "CURDATE" | "CURRENT_DATE"
                | "TODAY" | "DATE_ADD" | "DATE_SUB" | "DATE_FORMAT" | "TO_DATE" | "DATE_ONLY" => {
                    "TEXT".to_string()
                }
                "COALESCE" => args
                    .first()
                    .map(|arg| infer_expr_type(arg, plans))
                    .unwrap_or_else(|| "UNKNOWN".to_string()),
                "ABS" | "ROUND" => args
                    .first()
                    .map(|arg| infer_expr_type(arg, plans))
                    .unwrap_or_else(|| "UNKNOWN".to_string()),
                // SUM/MIN/MAX 取决于输入列，留 UNKNOWN 比猜错好
                _ => "UNKNOWN".to_string(),
            }
        }
        Expr::Param(_) => "UNKNOWN".to_string(),
    }
}

fn dedup_pairs(pairs: &mut Vec<(Vec<Value>, Vec<Value>)>) {
    let mut seen: HashSet<String> = HashSet::new();
    pairs.retain(|(values, _)| {
        let key = values
            .iter()
            .map(|v| codec::index_key(v).unwrap_or_else(|| "null".to_string()))
            .collect::<Vec<String>>()
            .join("\u{1}");
        seen.insert(key)
    });
}

fn apply_offset_limit(
    pairs: &mut Vec<(Vec<Value>, Vec<Value>)>,
    offset_expr: &Option<Expr>,
    limit_expr: &Option<Expr>,
    params: &[Value],
) -> Result<()> {
    let empty = RowContext::new(&[], Vec::new());
    let offset = match offset_expr {
        Some(expr) => to_usize(&eval(expr, &empty, params)?)?,
        None => 0,
    };
    let limit = match limit_expr {
        Some(expr) => Some(to_usize(&eval(expr, &empty, params)?)?),
        None => None,
    };
    // 就地裁剪（旧实现是 iter().cloned().collect()，白复制一整份结果集）
    if offset >= pairs.len() {
        pairs.clear();
        return Ok(());
    }
    pairs.drain(..offset);
    if let Some(limit) = limit {
        pairs.truncate(limit);
    }
    Ok(())
}

/// SELECT 执行入口：处理 UNION 与最终 ORDER BY / LIMIT
pub(crate) fn exec_select(
    engine: &Engine,
    stmt: &SelectStmt,
    params: &[Value],
    committed_only: bool,
    owner: Option<u64>,
    outer: Option<std::rc::Rc<RowContext>>,
) -> Result<QueryResult> {
    // CTE 先展开成派生表（展开后 ctes 为空，不会再递归）
    if !stmt.ctes.is_empty() {
        let expanded = expand_ctes(stmt)?;
        return exec_select(engine, &expanded, params, committed_only, owner, outer);
    }

    let (columns, mut types, mut pairs) =
        select_base(engine, stmt, params, committed_only, owner, outer.clone())?;

    if let Some((kind, right)) = &stmt.union {
        let (rcolumns, rtypes, mut rpairs) =
            select_base(engine, right, params, committed_only, owner, outer)?;
        if rcolumns.len() != columns.len() {
            return Err(DbError::syntax(format!(
                "UNION 两侧列数不一致：左侧 {} 列，右侧 {} 列",
                columns.len(),
                rcolumns.len()
            )));
        }
        // UNION 的输出类型：左侧推不出来时用右侧补
        for (slot, rtype) in rtypes.iter().enumerate() {
            if types[slot] == "UNKNOWN" {
                types[slot] = rtype.clone();
            }
        }
        // 右侧若写了 LIMIT/OFFSET，先作用于右侧结果
        apply_offset_limit(&mut rpairs, &right.offset, &right.limit, params)?;
        pairs.extend(rpairs);

        if *kind == UnionKind::Distinct {
            dedup_pairs(&mut pairs);
        }

        // ORDER BY 写在整条语句最后：对合并后的输出列求值
        if !stmt.order_by.is_empty() {
            let out_cols: Vec<Column> = columns
                .iter()
                .map(|name| Column::new(name.clone(), ColumnType::Text))
                .collect();
            pairs.sort_by(|a, b| {
                let av = RowContext::new(&out_cols, a.0.clone());
                let bv = RowContext::new(&out_cols, b.0.clone());
                for order in &stmt.order_by {
                    let left_value = eval(&order.expr, &av, params).unwrap_or(Value::Null);
                    let right_value = eval(&order.expr, &bv, params).unwrap_or(Value::Null);
                    let mut ordering = codec::compare(&left_value, &right_value)
                        .unwrap_or(std::cmp::Ordering::Equal);
                    if order.desc {
                        ordering = ordering.reverse();
                    }
                    if ordering != std::cmp::Ordering::Equal {
                        return ordering;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
        apply_offset_limit(&mut pairs, &stmt.offset, &stmt.limit, params)?;

        let rows = pairs.into_iter().map(|(values, _)| values).collect();
        return Ok(QueryResult::Select {
            columns,
            types,
            rows,
        });
    }

    // 单条 SELECT：组内排序键排序 + LIMIT/OFFSET
    if !stmt.order_by.is_empty() {
        let dirs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
        pairs.sort_by(|a, b| {
            for (i, desc) in dirs.iter().enumerate() {
                let left = a.1.get(i).cloned().unwrap_or(Value::Null);
                let right = b.1.get(i).cloned().unwrap_or(Value::Null);
                let mut ordering =
                    codec::compare(&left, &right).unwrap_or(std::cmp::Ordering::Equal);
                if *desc {
                    ordering = ordering.reverse();
                }
                if ordering != std::cmp::Ordering::Equal {
                    return ordering;
                }
            }
            std::cmp::Ordering::Equal
        });
    }
    apply_offset_limit(&mut pairs, &stmt.offset, &stmt.limit, params)?;

    let rows = pairs.into_iter().map(|(values, _)| values).collect();
    Ok(QueryResult::Select {
        columns,
        types,
        rows,
    })
}

// ------------------------------------------------------------------ JOIN

/// 递归生成 JOIN 树的行：叶子全扫，连接处优先走「跨表等值 + 右表索引」
fn join_frame(
    engine: &Engine,
    source: &TableSource,
    committed_only: bool,
    params: &[Value],
    owner: Option<u64>,
    outer: Option<std::rc::Rc<RowContext>>,
) -> Result<(Vec<TablePlan>, Vec<Vec<Value>>)> {
    match source {
        TableSource::Table { name, alias } => {
            let table = engine.table(name)?;
            let display = alias.clone().unwrap_or_else(|| table.name.clone());
            let plan = TablePlan {
                alias: display.to_lowercase(),
                display,
                table_name: table.name.clone(),
                columns: table.columns.clone(),
                offset: 0,
            };
            // committed_only 与共享读的语义一致：owner 不匹配开放事务时只见已提交
            let pairs = engine.scan_shared(name, owner)?;
            Ok((vec![plan], pairs.into_iter().map(|(_, v)| v).collect()))
        }
        // 派生表：先跑子查询把行物化出来，再当成一张「只有列定义、没有底层表」的表。
        // 空 table_name 就是「不是真表」的标记（索引辅助等路径靠它识别）。
        TableSource::Derived { query, alias } => {
            let (columns, types, rows) =
                run_subquery(engine, query, params, committed_only, owner, outer.clone())?;
            let cols: Vec<Column> = columns
                .iter()
                .zip(types.iter())
                .map(|(name, ty)| {
                    Column::new(
                        name.clone(),
                        ColumnType::parse(ty).unwrap_or(ColumnType::Text),
                    )
                })
                .collect();
            let plan = TablePlan {
                alias: alias.to_lowercase(),
                display: alias.clone(),
                table_name: String::new(),
                columns: cols,
                offset: 0,
            };
            Ok((vec![plan], rows))
        }
        TableSource::Join {
            left,
            right,
            kind,
            on,
        } => {
            let (lplans, lrows) =
                join_frame(engine, left, committed_only, params, owner, outer.clone())?;
            let (rplans, rrows) =
                join_frame(engine, right, committed_only, params, owner, outer.clone())?;
            let left_width: usize = lplans.iter().map(|p| p.columns.len()).sum();
            let total_width = left_width + rplans.iter().map(|p| p.columns.len()).sum::<usize>();

            // 索引辅助要求会话可见未提交数据（committed_only 时索引含未提交键）
            let assist = if committed_only {
                None
            } else {
                find_join_assist(engine, on.as_ref(), &lplans, &rplans)?
            };

            let mut combined_plans = lplans.clone();
            for mut plan in rplans {
                plan.offset += left_width;
                combined_plans.push(plan);
            }
            let sources: Vec<(&str, &[Column], usize)> = combined_plans
                .iter()
                .map(|p| (p.alias.as_str(), p.columns.as_slice(), p.offset))
                .collect();

            let mut out: Vec<Vec<Value>> = Vec::new();
            match assist {
                Some(assist) => {
                    for lrow in &lrows {
                        let key = lrow[assist.left_pos].clone();
                        let mut matched = false;
                        if !key.is_null() {
                            if let Some(ids) = engine
                                .find_by_equality(&assist.table_name, &assist.column_name, &key, owner)?
                            {
                                for id in ids {
                                    if let Some(rvals) =
                                        engine.get_shared(&assist.table_name, id, owner)?
                                    {
                                        let combined = concat_rows(lrow, &rvals);
                                        // ON 可能还带其它条件，候选行统一再过滤
                                        if let Some(on_expr) = on {
                                            let ctx =
                                                RowContext::from_tables(&sources, combined.clone())
                                                    .with_parent(outer.clone());
                                            if !matches(&eval_condition(
                                                engine,
                                                on_expr,
                                                &ctx,
                                                params,
                                                committed_only,
                                                owner,
                                            )?) {
                                                continue;
                                            }
                                        }
                                        out.push(combined);
                                        matched = true;
                                        if out.len() > MAX_SCAN_ROWS {
                                            return Err(scan_limit_error());
                                        }
                                    }
                                }
                            }
                        }
                        if !matched && *kind == JoinKind::Left {
                            out.push(extend_null(lrow, total_width));
                        }
                    }
                }
                None => {
                    if lrows.len().saturating_mul(rrows.len()) > MAX_JOIN_PAIRS {
                        return Err(DbError::type_error(format!(
                            "JOIN 组合规模过大（{} × {} = {} 行对），请在连接列上建索引或减小结果集",
                            lrows.len(),
                            rrows.len(),
                            lrows.len().saturating_mul(rrows.len())
                        )));
                    }
                    for lrow in &lrows {
                        let mut matched = false;
                        for rrow in &rrows {
                            let combined = concat_rows(lrow, rrow);
                            let pass = match on {
                                Some(on_expr) => {
                                    let ctx = RowContext::from_tables(&sources, combined.clone())
                                        .with_parent(outer.clone());
                                    matches(&eval_condition(
                                        engine,
                                        on_expr,
                                        &ctx,
                                        params,
                                        committed_only,
                                        owner,
                                    )?)
                                }
                                None => true,
                            };
                            if pass {
                                out.push(combined);
                                matched = true;
                                // 输出行数上限：行对数有闸门，但结果是物化的，
                                // 这里再兜一层，避免把内存吃光
                                if out.len() > MAX_SCAN_ROWS {
                                    return Err(scan_limit_error());
                                }
                            }
                        }
                        if !matched && *kind == JoinKind::Left {
                            out.push(extend_null(lrow, total_width));
                        }
                    }
                }
            }
            Ok((combined_plans, out))
        }
    }
}

struct JoinAssist {
    /// 左侧组合行中连接键的下标
    left_pos: usize,
    /// 右侧表名 / 连接列名
    table_name: String,
    column_name: String,
}

/// 在 ON 里找「左帧.列 = 右帧.列」且右表有对应单列索引的等值条件
fn find_join_assist(
    engine: &Engine,
    on: Option<&Expr>,
    lplans: &[TablePlan],
    rplans: &[TablePlan],
) -> Result<Option<JoinAssist>> {
    let on = match on {
        Some(e) => e,
        None => return Ok(None),
    };
    // 右侧必须是单表叶子：索引辅助按 row_id 回表取行
    if rplans.len() != 1 {
        return Ok(None);
    }
    let right = &rplans[0];
    // 派生表/CTE 没有底层表 id，也就没有索引可回表
    if right.table_name.is_empty() {
        return Ok(None);
    }

    let mut stack: Vec<&Expr> = vec![on];
    while let Some(current) = stack.pop() {
        match current {
            Expr::Binary {
                op: BinOp::And,
                left: l_expr,
                right: r_expr,
            } => {
                stack.push(l_expr.as_ref());
                stack.push(r_expr.as_ref());
            }
            Expr::Binary {
                op: BinOp::Eq,
                left: l_expr,
                right: r_expr,
            } => {
                for (a, b) in [(l_expr.as_ref(), r_expr.as_ref()), (r_expr.as_ref(), l_expr.as_ref())] {
                    if let (
                        Expr::Column {
                            table: Some(ta),
                            name: ca,
                        },
                        Expr::Column {
                            table: Some(tb),
                            name: cb,
                        },
                    ) = (a, b)
                    {
                        let ta = ta.to_lowercase();
                        let tb = tb.to_lowercase();
                        if let Some(lp) = lplans.iter().find(|p| p.alias == ta) {
                            if right.alias == tb {
                                if let Some(rci) = right
                                    .columns
                                    .iter()
                                    .position(|c| c.name.to_lowercase() == cb.to_lowercase())
                                {
                                    if let Some(lci) = lp
                                        .columns
                                        .iter()
                                        .position(|c| c.name.to_lowercase() == ca.to_lowercase())
                                    {
                                        let col_name = right.columns[rci].name.clone();
                                        if engine
                                            .table(&right.table_name)?
                                            .single_column_index(&col_name)
                                            .is_some()
                                        {
                                            return Ok(Some(JoinAssist {
                                                left_pos: lp.offset + lci,
                                                table_name: right.table_name.clone(),
                                                column_name: col_name,
                                            }));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

/// 表达式里是否含 EXISTS（含就得走「先解析、再求值」的慢路径）
fn expr_has_exists(expr: &Expr) -> bool {
    match expr {
        Expr::Exists { .. } => true,
        Expr::Binary { left, right, .. } => expr_has_exists(left) || expr_has_exists(right),
        Expr::Unary { operand, .. } => expr_has_exists(operand),
        Expr::IsNull { expr, .. } => expr_has_exists(expr),
        Expr::InList { expr, list, .. } => {
            expr_has_exists(expr) || list.iter().any(expr_has_exists)
        }
        Expr::InSubquery { expr, .. } => expr_has_exists(expr),
        Expr::Cast { expr, .. } => expr_has_exists(expr),
        Expr::Between {
            expr, low, high, ..
        } => expr_has_exists(expr) || expr_has_exists(low) || expr_has_exists(high),
        Expr::Like { expr, pattern, .. } => expr_has_exists(expr) || expr_has_exists(pattern),
        Expr::Function { args, .. } => args.iter().any(expr_has_exists),
        _ => false,
    }
}

/// 把 EXISTS 子查询就地求值成布尔字面量。
///
/// `eval` 拿不到引擎、也没法在求值中途执行查询，所以在这里预处理：
/// **当前行**被当作子查询的外层上下文传进去，相关子查询就是这么实现的。
fn resolve_exists(
    engine: &Engine,
    expr: &Expr,
    ctx: &RowContext,
    params: &[Value],
    committed_only: bool,
    owner: Option<u64>,
) -> Result<Expr> {
    let resolve = |inner: &Expr| resolve_exists(engine, inner, ctx, params, committed_only, owner);
    Ok(match expr {
        Expr::Exists { query } => {
            let outer = std::rc::Rc::new(ctx.clone());
            let (_, _, rows) =
                run_subquery(engine, query, params, committed_only, owner, Some(outer))?;
            Expr::Literal(Literal::Bool(!rows.is_empty()))
        }
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(resolve(left)?),
            right: Box::new(resolve(right)?),
        },
        Expr::Unary { op, operand } => Expr::Unary {
            op: *op,
            operand: Box::new(resolve(operand)?),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(resolve(expr)?),
            negated: *negated,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(resolve(expr)?),
            list: list.iter().map(&resolve).collect::<Result<Vec<_>>>()?,
            negated: *negated,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(resolve(expr)?),
            low: Box::new(resolve(low)?),
            high: Box::new(resolve(high)?),
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: Box::new(resolve(expr)?),
            pattern: Box::new(resolve(pattern)?),
            negated: *negated,
        },
        Expr::Cast { expr, ty, scale } => Expr::Cast {
            expr: Box::new(resolve(expr)?),
            ty: *ty,
            scale: *scale,
        },
        Expr::Function {
            name,
            args,
            distinct,
            star,
        } => Expr::Function {
            name: name.clone(),
            args: args.iter().map(&resolve).collect::<Result<Vec<_>>>()?,
            distinct: *distinct,
            star: *star,
        },
        // 其它形态（列、字面量、参数、IN 子查询……）与 EXISTS 无关，原样保留
        other => other.clone(),
    })
}

/// 条件求值：含 EXISTS 时先按当前行解析成布尔，再走普通求值。
fn eval_condition(
    engine: &Engine,
    expr: &Expr,
    ctx: &RowContext,
    params: &[Value],
    committed_only: bool,
    owner: Option<u64>,
) -> Result<Value> {
    if !expr_has_exists(expr) {
        return eval(expr, ctx, params);
    }
    let resolved = resolve_exists(engine, expr, ctx, params, committed_only, owner)?;
    eval(&resolved, ctx, params)
}

/// 把 CTE 引用展开成派生表。
///
/// CTE 只在本语句内可见，所以展开比在执行器里维护命名空间简单得多：
/// `FROM cte` 直接换成 `FROM (子查询) AS cte`，之后走派生表同一条路径。
///
/// 限制（README 已写明）：只展开 FROM 树里的引用；`WHERE x IN (SELECT ... FROM cte)`
/// 这类内层引用暂不支持。
fn expand_ctes(stmt: &SelectStmt) -> Result<SelectStmt> {
    // 依次展开：每个 CTE 只能引用**前面**定义好的（不支持递归/前向引用）
    let mut resolved: Vec<Cte> = Vec::with_capacity(stmt.ctes.len());
    for cte in &stmt.ctes {
        let body = rewrite_cte_sources(&cte.query, &resolved)?;
        resolved.push(Cte {
            name: cte.name.clone(),
            query: Box::new(body),
        });
    }
    let mut main = stmt.clone();
    main.ctes.clear();
    rewrite_cte_sources(&main, &resolved)
}

fn rewrite_cte_sources(stmt: &SelectStmt, ctes: &[Cte]) -> Result<SelectStmt> {
    let mut out = stmt.clone();
    out.ctes.clear();
    if let Some(from) = &stmt.from {
        out.from = Some(rewrite_source(from, ctes)?);
    }
    Ok(out)
}

fn rewrite_source(source: &TableSource, ctes: &[Cte]) -> Result<TableSource> {
    Ok(match source {
        TableSource::Table { name, alias } => {
            match ctes.iter().find(|cte| cte.name.eq_ignore_ascii_case(name)) {
                Some(cte) => TableSource::Derived {
                    query: Box::new((*cte.query).clone()),
                    // 没写别名就用 CTE 名（`FROM cte` 里 cte.列 依旧可用）
                    alias: alias.clone().unwrap_or_else(|| cte.name.clone()),
                },
                None => source.clone(),
            }
        }
        TableSource::Derived { query, alias } => TableSource::Derived {
            query: Box::new(rewrite_cte_sources(query, ctes)?),
            alias: alias.clone(),
        },
        TableSource::Join {
            left,
            right,
            kind,
            on,
        } => TableSource::Join {
            left: Box::new(rewrite_source(left, ctes)?),
            right: Box::new(rewrite_source(right, ctes)?),
            kind: *kind,
            on: on.clone(),
        },
    })
}

/// 执行派生表 / CTE 的子查询，取出「列名、列类型、物化行」。
/// 行数受统一上限保护：派生表本身也是整体物化的。
fn run_subquery(
    engine: &Engine,
    stmt: &SelectStmt,
    params: &[Value],
    committed_only: bool,
    owner: Option<u64>,
    outer: Option<std::rc::Rc<RowContext>>,
) -> Result<(Vec<String>, Vec<String>, Vec<Vec<Value>>)> {
    match exec_select(engine, stmt, params, committed_only, owner, outer)? {
        QueryResult::Select {
            columns,
            types,
            rows,
        } => {
            if rows.len() > MAX_SCAN_ROWS {
                return Err(scan_limit_error());
            }
            Ok((columns, types, rows))
        }
        other => Err(DbError::syntax(format!(
            "派生表只能是查询，实际得到 {}",
            other.summary()
        ))),
    }
}

fn concat_rows(lrow: &[Value], rrow: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(lrow.len() + rrow.len());
    out.extend_from_slice(lrow);
    out.extend_from_slice(rrow);
    out
}

fn extend_null(lrow: &[Value], width: usize) -> Vec<Value> {
    let mut out = Vec::with_capacity(width);
    out.extend_from_slice(lrow);
    out.resize(width, Value::Null);
    out
}

fn to_usize(value: &Value) -> Result<usize> {
    match value {
        Value::Int(n) if *n >= 0 => Ok(*n as usize),
        Value::Real(n) if *n >= 0.0 => Ok(*n as usize),
        Value::Null => Ok(0),
        other => Err(DbError::type_error(format!(
            "LIMIT/OFFSET 需要非负整数，实际是 \"{}\"",
            other.as_text()
        ))),
    }
}

fn group_rows(
    rows: Vec<RowContext>,
    group_by: &[Expr],
    params: &[Value],
) -> Result<Vec<Vec<RowContext>>> {
    let mut buckets: HashMap<String, Vec<RowContext>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for row in rows {
        let mut parts = Vec::with_capacity(group_by.len());
        for expr in group_by {
            let value = eval(expr, &row, params)?;
            parts.push(codec::index_key(&value).unwrap_or_else(|| "null".to_string()));
        }
        let key = parts.join("\u{1}");
        if !buckets.contains_key(&key) {
            order.push(key.clone());
        }
        buckets.entry(key).or_insert_with(Vec::new).push(row);
    }

    let mut groups = Vec::with_capacity(order.len());
    for key in order {
        if let Some(bucket) = buckets.remove(&key) {
            groups.push(bucket);
        }
    }
    Ok(groups)
}

/// 在一组行上求值：含聚合的表达式递归拆解，普通表达式用组内第一行
fn eval_in_group(expr: &Expr, rows: &[RowContext], params: &[Value]) -> Result<Value> {
    if !contains_aggregate(expr) {
        return match rows.first() {
            Some(row) => eval(expr, row, params),
            None => Ok(Value::Null),
        };
    }

    match expr {
        Expr::Function {
            name,
            args,
            distinct,
            star,
        } => {
            if AGGREGATES.contains(&name.as_str()) {
                eval_aggregate(name, args.first(), *distinct, *star, rows, params)
            } else {
                Err(DbError::syntax(format!(
                    "暂不支持在 {} 的参数中嵌套聚合函数",
                    name
                )))
            }
        }
        Expr::Binary { op, left, right } => {
            let left_value = eval_in_group(left, rows, params)?;
            let right_value = eval_in_group(right, rows, params)?;
            apply_binary(*op, &left_value, &right_value)
        }
        Expr::Unary { op, operand } => {
            let value = eval_in_group(operand, rows, params)?;
            apply_unary(*op, &value)
        }
        Expr::IsNull { expr, negated } => {
            let value = eval_in_group(expr, rows, params)?;
            Ok(Value::Bool(if *negated {
                !value.is_null()
            } else {
                value.is_null()
            }))
        }
        _ => Err(DbError::syntax("聚合表达式的组合形式不受支持")),
    }
}

/// 单列上的范围约束（ WHERE 重写后由 BTreeMap 范围扫描消费）
#[derive(Default)]
struct ColumnRange {
    lower: Option<(Value, bool)>, // (值, 是否含端点)
    upper: Option<(Value, bool)>,
}


fn merge_lower(range: &mut ColumnRange, value: Value, inclusive: bool) -> Option<()> {
    match &range.lower {
        None => range.lower = Some((value, inclusive)),
        Some((existing, existing_inc)) => {
            let ord = codec::compare(&value, existing)?;
            if ord == std::cmp::Ordering::Greater
                || (ord == std::cmp::Ordering::Equal && *existing_inc && !inclusive)
            {
                // 取更紧的下界
                range.lower = Some((value, inclusive));
            }
        }
    }
    Some(())
}

fn merge_upper(range: &mut ColumnRange, value: Value, inclusive: bool) -> Option<()> {
    match &range.upper {
        None => range.upper = Some((value, inclusive)),
        Some((existing, existing_inc)) => {
            let ord = codec::compare(&value, existing)?;
            if ord == std::cmp::Ordering::Less
                || (ord == std::cmp::Ordering::Equal && *existing_inc && !inclusive)
            {
                range.upper = Some((value, inclusive));
            }
        }
    }
    Some(())
}

/// 从 WHERE 里提取某列的范围索引条件（AND 树，列 OP 常量 / BETWEEN）
fn range_candidates(
    engine: &Engine,
    table_name: &str,
    where_clause: Option<&Expr>,
    params: &[Value],
) -> Result<Option<Vec<u32>>> {
    let expr = match where_clause {
        Some(e) => e,
        None => return Ok(None),
    };
    let candidates = extract_range_columns(expr, params)?;

    // 逐列尝试：找到第一个「索引前导列」且能取到范围的列
    // （复合索引只对第一列的范围有效，后续列要靠 WHERE 复检）
    for (column_name, range) in &candidates {
        let table = engine.table(table_name)?;
        if table.leading_index(column_name).is_none() {
            continue;
        }
        // 边界值必须 coerce 到列的类型/标度，否则索引键格式不匹配
        let col = table.columns.iter().find(|c| c.name.eq_ignore_ascii_case(column_name));
        let (ty, scale) = col.map(|c| (c.ty, c.scale)).unwrap_or((ColumnType::Text, 0));
        let coerce_bound = |v: &Value| -> Option<String> {
            codec::coerce(ty, scale, v).ok().and_then(|cv| codec::index_key(&cv))
        };
        let bounds = crate::table::RangeBounds {
            lower: range
                .lower
                .as_ref()
                .and_then(|(v, inc)| coerce_bound(v).map(|k| (k, *inc))),
            upper: range
                .upper
                .as_ref()
                .and_then(|(v, inc)| coerce_bound(v).map(|k| (k, *inc))),
        };
        if bounds.lower.is_none() && bounds.upper.is_none() {
            continue;
        }
        if let Some(ids) = crate::table::find_by_range(table, column_name, &bounds) {
            return Ok(Some(ids));
        }
    }
    Ok(None)
}

/// 收集 WHERE 中所有单列范围约束（列名 → 约束）
fn extract_range_columns(
    expr: &Expr,
    params: &[Value],
) -> Result<HashMap<String, ColumnRange>> {
    let empty = RowContext::new(&[], Vec::new());
    // 按 AND 拆开后，每个子句若是指向单列的范围条件则收下
    let mut out: HashMap<String, ColumnRange> = HashMap::new();
    let mut stack: Vec<&Expr> = vec![expr];
    while let Some(current) = stack.pop() {
        match current {
            Expr::Binary {
                op: BinOp::And,
                left,
                right,
            } => {
                stack.push(left.as_ref());
                stack.push(right.as_ref());
            }
            Expr::Between {
                expr,
                low,
                high,
                negated: false,
            } => {
                if let Expr::Column { name, .. } = expr.as_ref() {
                    let mut entry = out.entry(name.clone()).or_default();
                    let empty = RowContext::new(&[], Vec::new());
                    // 求值失败 = 不是常量（例如相关子查询里引用了外层列），跳过这条约束
                    let (Ok(lo), Ok(hi)) = (eval(low, &empty, params), eval(high, &empty, params))
                    else {
                        continue;
                    };
                    if !lo.is_null() && !hi.is_null() {
                        merge_lower(&mut entry, lo, true);
                        merge_upper(&mut entry, hi, true);
                    }
                }
            }
            Expr::Binary {
                op,
                left,
                right,
            } if matches!(
                op,
                BinOp::Gt | BinOp::Ge | BinOp::Lt | BinOp::Le
            ) =>
            {
                // 列在左、常量在右；列在右的 (常量 OP 列) 形式交换方向
                let (column_expr, operand, op) = match (left.as_ref(), right.as_ref()) {
                    (Expr::Column { .. }, other) => (left.as_ref(), other, *op),
                    (other_side, Expr::Column { .. }) => {
                        let _ = other_side;
                        let flipped = match op {
                            BinOp::Gt => BinOp::Lt,
                            BinOp::Ge => BinOp::Le,
                            BinOp::Lt => BinOp::Gt,
                            BinOp::Le => BinOp::Ge,
                            other_op => *other_op,
                        };
                        (right.as_ref(), left.as_ref(), flipped)
                    }
                    _ => continue,
                };
                if let Expr::Column { name, .. } = column_expr {
                    // 求值失败 = 不是常量（相关子查询会引用外层列），跳过这条约束
                    let Ok(bound) = eval(operand, &empty, params) else {
                        continue;
                    };
                    if bound.is_null() {
                        continue;
                    }
                    let mut entry = out.entry(name.clone()).or_default();
                    match op {
                        BinOp::Gt => {
                            merge_lower(&mut entry, bound, false);
                        }
                        BinOp::Ge => {
                            merge_lower(&mut entry, bound, true);
                        }
                        BinOp::Lt => {
                            merge_upper(&mut entry, bound, false);
                        }
                        BinOp::Le => {
                            merge_upper(&mut entry, bound, true);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// 收集 WHERE 里所有 `列 = 常量` 约束，交给引擎挑索引。
///
/// 旧实现只认「第一个等值条件 + 单列索引」，复合索引用不上；
/// 现在把整棵 AND 树的等值条件都交上去，由引擎选前导列匹配最长的索引。
fn index_candidates(
    engine: &Engine,
    table: &str,
    where_clause: Option<&Expr>,
    params: &[Value],
    owner: Option<u64>,
) -> Result<Option<IndexLookup>> {
    let Some(expr) = where_clause else {
        return Ok(None);
    };
    let empty = RowContext::new(&[], Vec::new());
    let mut equals: Vec<(String, Value)> = Vec::new();
    let mut stack: Vec<&Expr> = vec![expr];

    while let Some(current) = stack.pop() {
        match current {
            Expr::Binary {
                op: BinOp::And,
                left,
                right,
            } => {
                stack.push(left.as_ref());
                stack.push(right.as_ref());
            }
            Expr::Binary {
                op: BinOp::Eq,
                left,
                right,
            } => {
                // 常量在左、列在右也算（`5 = id`）
                let (column, constant_expr) = match (left.as_ref(), right.as_ref()) {
                    (Expr::Column { name, .. }, other) => (name, other),
                    (other, Expr::Column { name, .. }) => (name, other),
                    _ => continue,
                };
                if let Ok(value) = eval(constant_expr, &empty, params) {
                    if !value.is_null() {
                        equals.push((column.clone(), value));
                    }
                }
            }
            _ => {}
        }
    }

    if equals.is_empty() {
        return Ok(None);
    }
    engine.find_by_index(table, &equals, owner)
}

// ------------------------------------------------------------------ 写语句

fn exec_insert(
    engine: &mut Engine,
    stmt: &InsertStmt,
    params: &[Value],
    owner: u64,
) -> Result<QueryResult> {
    let columns = engine.table(&stmt.table)?.columns.clone();
    // 表级写锁：同表 DML 串行（页内容冲突要求）。
    // 注意调用方已持引擎全局写锁，跨表并行当前并不成立（见 ConnSession 的说明）
    let table_guard = engine.table_lock(engine.table(&stmt.table)?.id);
    let _table_guard = table_write(&table_guard);
    let mut position: HashMap<String, usize> = HashMap::with_capacity(columns.len());
    for (i, column) in columns.iter().enumerate() {
        position.insert(column.name.to_lowercase(), i);
    }

    let target: Vec<usize> = match &stmt.columns {
        Some(names) => {
            let mut slots = Vec::with_capacity(names.len());
            for name in names {
                match position.get(&name.to_lowercase()) {
                    Some(i) => slots.push(*i),
                    None => {
                        return Err(DbError::not_found(format!(
                            "列 \"{}\" 不存在于表 \"{}\"",
                            name, stmt.table
                        )))
                    }
                }
            }
            slots
        }
        None => (0..columns.len()).collect(),
    };

    // 冲突判定列（UPSERT）：显式 target，未写则用主键。
    // 必须有可用的索引，否则每行都全表扫描，代价离谱。
    let conflict_slots: Option<(Vec<usize>, Option<&Vec<(String, Expr)>>)> = match &stmt.on_conflict {
        None => None,
        Some(conflict) => {
            let names: Vec<String> = if conflict.target.is_empty() {
                columns
                    .iter()
                    .filter(|c| c.primary_key)
                    .map(|c| c.name.clone())
                    .collect()
            } else {
                conflict.target.clone()
            };
            if names.is_empty() {
                return Err(DbError::syntax(
                    "ON CONFLICT 需要指定冲突列，或表上要有主键",
                ));
            }
            let mut slots = Vec::with_capacity(names.len());
            for name in &names {
                match position.get(&name.to_lowercase()) {
                    Some(slot) => slots.push(*slot),
                    None => {
                        return Err(DbError::not_found(format!(
                            "冲突列 \"{}\" 不存在于表 \"{}\"",
                            name, stmt.table
                        )))
                    }
                }
            }
            let assigns = match &conflict.action {
                // Some(_) = DO UPDATE SET ...；None = DO NOTHING
                ConflictAction::Update(assigns) => Some(assigns),
                ConflictAction::Nothing => None,
            };
            Some((slots, assigns))
        }
    };

    let empty = RowContext::new(&[], Vec::new());
    let mut inserted = 0usize;
    let mut updated = 0usize;
    let mut skipped = 0usize;
    // 自增列分配出来的值（没有自增列时为空）
    let mut ids: Vec<i64> = Vec::new();

    for row_exprs in &stmt.values {
        if row_exprs.len() != target.len() {
            return Err(DbError::syntax(format!(
                "列数不匹配：期望 {} 个值，实际给了 {}",
                target.len(),
                row_exprs.len()
            )));
        }

        let mut values: Vec<Value> = Vec::with_capacity(columns.len());
        for column in &columns {
            values.push(match &column.default {
                Some(default) => default.clone(),
                None => Value::Null,
            });
        }

        for (slot, expr) in target.iter().zip(row_exprs.iter()) {
            let raw = eval(expr, &empty, params)?;
            values[*slot] = codec::coerce(columns[*slot].ty, columns[*slot].scale, &raw)?;
        }

        // ---------------------------------------------------------- UPSERT
        // 冲突判定走索引（主键 / 唯一索引 / 前导列索引）：命中则更新或跳过。
        // 我们的唯一约束是**提交期**才检查的，所以这里必须显式查一次索引，
        // 否则同一事务里连续两条 UPSERT 会各自插一行、提交时才报冲突。
        if let Some((slots, assignments)) = &conflict_slots {
            let mut key: Vec<(String, Value)> = Vec::with_capacity(slots.len());
            let mut usable = true;
            for slot in slots {
                if values[*slot].is_null() {
                    // 冲突列含 NULL：唯一索引不索引 NULL，按「不冲突」处理
                    usable = false;
                    break;
                }
                key.push((columns[*slot].name.clone(), values[*slot].clone()));
            }
            let hit = if usable {
                match engine.find_by_index(&stmt.table, &key, Some(owner))? {
                    Some(lookup) => lookup.ids.first().copied(),
                    // 冲突列上没有索引 → 没法判定冲突，直接报错而不是逐行全表扫
                    None => {
                        let names: Vec<String> =
                            slots.iter().map(|s| columns[*s].name.clone()).collect();
                        return Err(DbError::syntax(format!(
                            "ON CONFLICT 的列（{}）上没有可用索引：需要主键或唯一索引",
                            names.join(", ")
                        )));
                    }
                }
            } else {
                None
            };
            if let Some(row_id) = hit {
                match assignments {
                    None => {
                        skipped += 1;
                        continue;
                    }
                    Some(assignments) => {
                        let current = engine
                            .get(&stmt.table, row_id)?
                            .ok_or_else(|| DbError::internal("冲突行在更新时消失"))?;
                        // 现有行用裸列名，新行以 EXCLUDED.列 暴露：
                        // 这样 `SET name = EXCLUDED.name` 与 `SET n = n + 1` 都能写
                        let mut combined = current.clone();
                        combined.extend(values.iter().cloned());
                        let ctx = RowContext::with_qualified_alias(&columns, combined, "excluded");
                        let mut next = current;
                        for (name, expr) in assignments.iter() {
                            let slot = *position.get(&name.to_lowercase()).ok_or_else(|| {
                                DbError::not_found(format!(
                                    "列 \"{}\" 不存在于表 \"{}\"",
                                    name, stmt.table
                                ))
                            })?;
                            let raw = eval(expr, &ctx, params)?;
                            next[slot] = codec::coerce(columns[slot].ty, columns[slot].scale, &raw)?;
                        }
                        engine.update(&stmt.table, row_id, next)?;
                        updated += 1;
                        continue;
                    }
                }
            }
        }

        let (_, auto_value) = engine.insert_returning(&stmt.table, values)?;
        if let Some(id) = auto_value {
            ids.push(id);
        }
        inserted += 1;
    }

    Ok(QueryResult::Insert {
        inserted,
        updated,
        skipped,
        ids,
    })
}

/// UPDATE / DELETE 的行来源：能用索引就只取候选行，否则退回全表扫描。
///
/// 索引只用来筛**候选**，WHERE 依旧逐行复检（索引条件可能只是 WHERE 的一部分）。
fn write_candidates(
    engine: &Engine,
    table: &str,
    where_clause: Option<&Expr>,
    params: &[Value],
    owner: u64,
) -> Result<Option<Vec<u32>>> {
    let Some(expr) = where_clause else {
        return Ok(None);
    };
    if let Some(lookup) = index_candidates(engine, table, Some(expr), params, Some(owner))? {
        return Ok(Some(lookup.ids));
    }
    range_candidates(engine, table, Some(expr), params)
}

fn exec_update(
    engine: &mut Engine,
    stmt: &UpdateStmt,
    params: &[Value],
    owner: u64,
) -> Result<QueryResult> {
    let columns = engine.table(&stmt.table)?.columns.clone();
    let table_guard = engine.table_lock(engine.table(&stmt.table)?.id);
    let _table_guard = table_write(&table_guard);
    let mut position: HashMap<String, usize> = HashMap::with_capacity(columns.len());
    for (i, column) in columns.iter().enumerate() {
        position.insert(column.name.to_lowercase(), i);
    }

    let mut assignments: Vec<(usize, Expr)> = Vec::with_capacity(stmt.assignments.len());
    for (name, expr) in &stmt.assignments {
        match position.get(&name.to_lowercase()) {
            Some(i) => assignments.push((*i, expr.clone())),
            None => {
                return Err(DbError::not_found(format!(
                    "列 \"{}\" 不存在于表 \"{}\"",
                    name, stmt.table
                )))
            }
        }
    }

    // 先展开 WHERE 里的子查询（索引候选与行过滤都用同一个条件）
    let where_clause = match &stmt.where_clause {
        Some(expr) => Some(resolve_subqueries(&*engine, expr, params, false, Some(owner))?),
        None => None,
    };

    // 行来源：索引候选（通常只有几行）→ 全表快照（受行数上限保护）
    let rows: Vec<(u32, Vec<Value>)> =
        match write_candidates(&*engine, &stmt.table, where_clause.as_ref(), params, owner)? {
            Some(ids) => {
                let mut rows = Vec::with_capacity(ids.len());
                for id in ids {
                    if let Some(values) = engine.get(&stmt.table, id)? {
                        rows.push((id, values));
                    }
                }
                rows
            }
            None => {
                let snapshot = engine.scan(&stmt.table)?;
                if snapshot.len() > MAX_SCAN_ROWS {
                    return Err(scan_limit_error());
                }
                snapshot
            }
        };

    let mut updated = 0usize;
    for (row_id, values) in rows {
        let ctx = RowContext::new(&columns, values.clone());
        if let Some(where_clause) = &where_clause {
            // 写语句的 WHERE 也支持 EXISTS（相关子查询会用当前行作外层上下文）
            let value = eval_condition(engine, where_clause, &ctx, params, false, Some(owner))?;
            if !matches(&value) {
                continue;
            }
        }

        let mut next = values.clone();
        for (slot, expr) in &assignments {
            let raw = eval(expr, &ctx, params)?;
            next[*slot] = codec::coerce(columns[*slot].ty, columns[*slot].scale, &raw)?;
        }

        engine.update(&stmt.table, row_id, next)?;
        updated += 1;
    }

    Ok(QueryResult::Update { updated })
}

fn exec_delete(
    engine: &mut Engine,
    stmt: &DeleteStmt,
    params: &[Value],
    owner: u64,
) -> Result<QueryResult> {
    let columns = engine.table(&stmt.table)?.columns.clone();
    let table_guard = engine.table_lock(engine.table(&stmt.table)?.id);
    let _table_guard = table_write(&table_guard);

    let where_clause = match &stmt.where_clause {
        Some(expr) => Some(resolve_subqueries(&*engine, expr, params, false, Some(owner))?),
        None => None,
    };
    let rows: Vec<(u32, Vec<Value>)> =
        match write_candidates(&*engine, &stmt.table, where_clause.as_ref(), params, owner)? {
            Some(ids) => {
                let mut rows = Vec::with_capacity(ids.len());
                for id in ids {
                    if let Some(values) = engine.get(&stmt.table, id)? {
                        rows.push((id, values));
                    }
                }
                rows
            }
            None => {
                let snapshot = engine.scan(&stmt.table)?;
                if snapshot.len() > MAX_SCAN_ROWS {
                    return Err(scan_limit_error());
                }
                snapshot
            }
        };
    let mut deleted = 0usize;

    for (row_id, values) in rows {
        if let Some(where_clause) = &where_clause {
            let ctx = RowContext::new(&columns, values);
            // 写语句的 WHERE 也支持 EXISTS（相关子查询会用当前行作外层上下文）
            let value = eval_condition(engine, where_clause, &ctx, params, false, Some(owner))?;
            if !matches(&value) {
                continue;
            }
        }
        if engine.delete(&stmt.table, row_id)? {
            deleted += 1;
        }
    }

    Ok(QueryResult::Delete { deleted })
}

// ------------------------------------------------------------------ DDL

fn is_literal_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(_) => true,
        Expr::Unary { op, operand } => {
            matches!(op, UnOp::Neg | UnOp::Plus) && is_literal_expr(operand)
        }
        _ => false,
    }
}

/// 列定义 → `Column`（CREATE TABLE 与 ALTER TABLE ADD COLUMN 共用）
fn column_from_def(def: &ColumnDef, is_pk: bool, params: &[Value]) -> Result<Column> {
    let ty = ColumnType::parse(&def.type_name)?;
    let scale = ColumnType::decimal_scale_from_type(&def.type_name)?;

    let mut column = Column::new(def.name.clone(), ty);
    column.scale = scale;
    column.primary_key = is_pk;
    column.not_null = def.not_null || is_pk;
    column.unique = def.unique;
    column.auto_increment = def.auto_increment;

    if let Some(default_expr) = &def.default {
        if !is_literal_expr(default_expr) {
            return Err(DbError::syntax(format!(
                "列 {} 的默认值必须是字面量（例如 '2026-01-01'、0、TRUE），不支持函数或列引用",
                def.name
            )));
        }
        let empty = RowContext::new(&[], Vec::new());
        let raw = eval(default_expr, &empty, params)?;
        column.default = if raw.is_null() {
            None
        } else {
            Some(codec::coerce(ty, column.scale, &raw)?)
        };
    }
    Ok(column)
}

fn exec_create_table(
    engine: &mut Engine,
    stmt: &CreateTableStmt,
    params: &[Value],
) -> Result<QueryResult> {
    if stmt.columns.is_empty() {
        return Err(DbError::syntax("建表至少需要一列"));
    }

    let mut columns = Vec::with_capacity(stmt.columns.len());
    for def in &stmt.columns {
        let is_pk = def.primary_key
            || stmt
                .primary_key
                .iter()
                .any(|n| n.to_lowercase() == def.name.to_lowercase());
        columns.push(column_from_def(def, is_pk, params)?);
    }

    engine.create_table(&stmt.name, columns, stmt.if_not_exists)?;
    Ok(QueryResult::Ddl {
        message: format!("已创建表 \"{}\"", stmt.name),
    })
}

/// 值 → catalog/重建表用的 JSON（DECIMAL 以文本表示，重建时再按标度解析回来）
fn value_to_spec_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(n) => Json::Int(*n),
        Value::Real(n) => Json::Num(*n),
        Value::Decimal { v, scale } => Json::Str(codec::format_decimal(*v, *scale)),
        Value::Text(s) => Json::Str(s.clone()),
    }
}

/// 列 → 重建表的列规格。`source` 指向旧列名（改名时用；为空表示同名）
fn column_to_spec(column: &Column, source: Option<&str>) -> Json {
    let type_name = if column.ty == ColumnType::Decimal {
        // rebuild_table 从类型串解析标度，所以必须带 (p,s)
        format!("DECIMAL(38,{})", column.scale)
    } else {
        column.ty.as_str().to_string()
    };

    let mut fields = vec![
        ("name".to_string(), Json::str(column.name.clone())),
        ("type".to_string(), Json::str(type_name)),
        ("primaryKey".to_string(), Json::Bool(column.primary_key)),
        ("notNull".to_string(), Json::Bool(column.not_null)),
        ("unique".to_string(), Json::Bool(column.unique)),
    ];
    if let Some(source) = source {
        if !source.eq_ignore_ascii_case(&column.name) {
            fields.push(("source".to_string(), Json::str(source.to_string())));
        }
    }
    if column.auto_increment {
        fields.push(("autoIncrement".to_string(), Json::Bool(true)));
    }
    if let Some(default) = &column.default {
        fields.push(("default".to_string(), value_to_spec_json(default)));
    }
    Json::Obj(fields)
}

/// ALTER TABLE：
/// - `RENAME TO` 只改元数据（O(1)，不动数据）
/// - 列级变更走「影子表重建」（`Engine::rebuild_table`）：数据完整保留，
///   但表 id 会变、行号会重排、索引按列名迁移
fn exec_alter_table(
    engine: &mut Engine,
    table_name: &str,
    action: &AlterAction,
    params: &[Value],
) -> Result<QueryResult> {
    if let AlterAction::RenameTable { to } = action {
        engine.rename_table(table_name, to)?;
        return Ok(QueryResult::Ddl {
            message: format!("已将表 \"{}\" 重命名为 \"{}\"", table_name, to),
        });
    }

    let columns = {
        let table = engine.table(table_name)?;
        table.columns.clone()
    };
    // (新列定义, 来源列名)
    let mut specs: Vec<(Column, String)> = columns
        .iter()
        .map(|c| (c.clone(), c.name.clone()))
        .collect();

    let message = match action {
        AlterAction::AddColumn(def) => {
            let column = column_from_def(def, false, params)?;
            if columns
                .iter()
                .any(|c| c.name.to_lowercase() == column.name.to_lowercase())
            {
                return Err(DbError::new(
                    error::ALREADY_EXISTS,
                    format!("列 \"{}\" 已存在", column.name),
                ));
            }
            let name = column.name.clone();
            specs.push((column, String::new()));
            format!("已为表 \"{}\" 添加列 \"{}\"", table_name, name)
        }
        AlterAction::DropColumn { name } => {
            let position = columns
                .iter()
                .position(|c| c.name.to_lowercase() == name.to_lowercase())
                .ok_or_else(|| {
                    DbError::not_found(format!("列 \"{}\" 不存在于表 \"{}\"", name, table_name))
                })?;
            if columns[position].primary_key {
                return Err(DbError::syntax(
                    "不能删除主键列（会丢掉行标识）：需要换主键请新建表再迁移数据",
                ));
            }
            if columns.len() == 1 {
                return Err(DbError::syntax("表至少要保留一列"));
            }
            specs.remove(position);
            format!(
                "已删除表 \"{}\" 的列 \"{}\"",
                table_name, columns[position].name
            )
        }
        AlterAction::RenameColumn { from, to } => {
            let position = columns
                .iter()
                .position(|c| c.name.to_lowercase() == from.to_lowercase())
                .ok_or_else(|| {
                    DbError::not_found(format!("列 \"{}\" 不存在于表 \"{}\"", from, table_name))
                })?;
            if columns
                .iter()
                .any(|c| c.name.to_lowercase() == to.to_lowercase())
            {
                return Err(DbError::new(
                    error::ALREADY_EXISTS,
                    format!("列 \"{}\" 已存在", to),
                ));
            }
            let old_name = specs[position].0.name.clone();
            specs[position].0.name = to.clone();
            specs[position].1 = old_name;
            format!("已将列 \"{}\" 重命名为 \"{}\"", from, to)
        }
        AlterAction::RenameTable { .. } => unreachable!("RENAME TO 已在上方提前返回"),
    };

    let spec_json: Vec<Json> = specs
        .iter()
        .map(|(column, source)| {
            let source = if source.is_empty() { None } else { Some(source.as_str()) };
            column_to_spec(column, source)
        })
        .collect();

    let outcome = engine.rebuild_table(table_name, &spec_json)?;
    Ok(QueryResult::Ddl {
        message: format!(
            "{}（迁移 {} 行、恢复 {} 个索引）",
            message, outcome.rows, outcome.indexes
        ),
    })
}

// ------------------------------------------------------------------ EXPLAIN

/// UPDATE / DELETE 的行来源说明（与 `write_candidates` 的实际决策保持一致）
fn explain_write_source(
    rows: &mut Vec<Vec<Value>>,
    engine: &Engine,
    table: &str,
    where_clause: Option<&Expr>,
    params: &[Value],
    owner: u64,
) -> Result<()> {
    match write_candidates(engine, table, where_clause, params, owner)? {
        Some(ids) => {
            rows.push(kv(
                "访问方式",
                format!("索引定位后逐行处理（候选 {} 行，WHERE 仍逐行复检）", ids.len()),
            ));
        }
        None => {
            let total = engine.table(table).map(|t| t.row_count).unwrap_or(0);
            rows.push(kv(
                "访问方式",
                format!("全表扫描后逐行处理（约 {} 行，受 50 万行上限保护）", total),
            ));
        }
    }
    Ok(())
}

fn kv(key: &str, value: String) -> Vec<Value> {
    vec![Value::Text(key.to_string()), Value::Text(value)]
}

fn exec_explain(
    engine: &Engine,
    statement: &Statement,
    params: &[Value],
    owner: u64,
) -> Result<QueryResult> {
    let mut rows: Vec<Vec<Value>> = Vec::new();
    rows.push(kv("语句", statement.kind_name().to_uppercase()));

    // 带 CTE 的语句先展开，否则 FROM 里的 CTE 名会被当成真表去查
    if let Statement::Select(select) = statement {
        if !select.ctes.is_empty() {
            let names: Vec<String> = select.ctes.iter().map(|c| c.name.clone()).collect();
            let expanded = expand_ctes(select)?;
            rows.push(kv("CTE", names.join(", ")));
            rows.push(kv(
                "说明",
                "CTE 会被展开成派生表（整体物化），不产生独立执行计划".to_string(),
            ));
            let inner = exec_explain(engine, &Statement::Select(expanded), params, owner)?;
            if let QueryResult::Explain {
                rows: mut inner_rows,
                ..
            } = inner
            {
                rows.append(&mut inner_rows);
            }
            return Ok(QueryResult::Explain {
                columns: vec!["步骤".to_string(), "内容".to_string()],
                rows,
            });
        }
    }

    match statement {
        Statement::Select(select) => {
            if let Some(from) = &select.from {
                match from {
                    TableSource::Table { name, .. } => {
                        let (table_name, total) = {
                            let table = engine.table(name)?;
                            (table.name.clone(), table.row_count)
                        };
                        rows.push(kv("表", table_name));

                        // 报的访问方式必须与 select_base 的实际决策一致：
                        // 等值索引 → 范围索引 → 全表扫描（旧版漏了范围索引）
                        let candidates = index_candidates(
                            &*engine,
                            name,
                            select.where_clause.as_ref(),
                            params,
                            Some(owner),
                        )?;
                        let read_rows = match candidates {
                            Some(lookup) => {
                                let read = lookup.ids.len();
                                rows.push(kv("访问方式", lookup.label));
                                rows.push(kv("扫描行数", read.to_string()));
                                read
                            }
                            None => {
                                match range_candidates(engine, name, select.where_clause.as_ref(), params)? {
                                    Some(ids) => {
                                        rows.push(kv(
                                            "访问方式",
                                            "范围索引查找（前导列索引，边界已按列类型转换）".to_string(),
                                        ));
                                        rows.push(kv("扫描行数", ids.len().to_string()));
                                        ids.len()
                                    }
                                    None => {
                                        rows.push(kv("访问方式", "全表扫描".to_string()));
                                        rows.push(kv("扫描行数", total.to_string()));
                                        total as usize
                                    }
                                }
                            }
                        };
                        rows.push(kv("表总行数", total.to_string()));
                        // 代价粗估：没有直方图/NDV 统计，只能按「读取行数占比」给个量级
                        let total_usize = total.max(1) as usize;
                        let percent = if total_usize == 0 {
                            0
                        } else {
                            (read_rows * 100 / total_usize).min(100)
                        };
                        rows.push(kv(
                            "代价粗估",
                            format!(
                                "预计读取 {} 行（约全表 {}%）；仅按行数比例粗估，未做直方图统计",
                                read_rows, percent
                            ),
                        ));
                        if let Some(cap) = early_row_cap(select, params) {
                            rows.push(kv(
                                "LIMIT 下推",
                                format!("无排序/分组/聚合，扫描最多凑够 {} 行即停", cap),
                            ));
                        }
                    }
                    TableSource::Join { kind, .. } => {
                        rows.push(kv("表", from.describe()));
                        rows.push(kv(
                            "访问方式",
                            match kind {
                                JoinKind::Inner | JoinKind::Cross => "嵌套循环连接".to_string(),
                                JoinKind::Left => "嵌套循环连接（外连接）".to_string(),
                            },
                        ));
                        rows.push(kv("说明", "等值连接列上有索引时自动改走索引查找".to_string()));
                    }
                    TableSource::Derived { alias, query } => {
                        rows.push(kv("表", format!("(子查询) {}", alias)));
                        rows.push(kv(
                            "访问方式",
                            "先把子查询整体物化，再当作临时表参与过滤/连接".to_string(),
                        ));
                        rows.push(kv(
                            "子查询",
                            format!(
                                "{} 个投影列{}",
                                query.items.len(),
                                if query.from.is_some() { "" } else { "（无 FROM）" }
                            ),
                        ));
                        rows.push(kv(
                            "说明",
                            format!(
                                "派生表不支持索引下推；子查询结果超过 {} 行会直接报错",
                                MAX_SCAN_ROWS
                            ),
                        ));
                    }
                }

                if !select.group_by.is_empty() {
                    rows.push(kv(
                        "分组",
                        format!("{} 个键（内存哈希分组）", select.group_by.len()),
                    ));
                }
                if !select.order_by.is_empty() {
                    rows.push(kv(
                        "排序",
                        format!("{} 个键（内存排序）", select.order_by.len()),
                    ));
                }
                if select.distinct {
                    rows.push(kv("去重", "是".to_string()));
                }
                if let Some(limit) = &select.limit {
                    rows.push(kv("LIMIT", expr_label(limit)));
                }
            } else {
                rows.push(kv("访问方式", "常量查询，无需扫描".to_string()));
            }
        }
        Statement::Insert(insert) => {
            rows.push(kv("表", insert.table.clone()));
            rows.push(kv("访问方式", "追加写入（查找可用页）".to_string()));
            rows.push(kv("写入行数", insert.values.len().to_string()));
        }
        Statement::Update(update) => {
            rows.push(kv("表", update.table.clone()));
            explain_write_source(&mut rows, engine, &update.table, update.where_clause.as_ref(), params, owner)?;
            rows.push(kv("访问方式追加", "逐行更新（先删旧行再插新行，索引 delta 延迟到提交期）".to_string()));
            rows.push(kv("赋值", format!("{} 个列", update.assignments.len())));
        }
        Statement::Delete(delete) => {
            rows.push(kv("表", delete.table.clone()));
            explain_write_source(&mut rows, engine, &delete.table, delete.where_clause.as_ref(), params, owner)?;
            rows.push(kv("访问方式追加", "逐行删除（死空间超过阈值会就地整理页）".to_string()));
        }
        _ => {
            rows.push(kv("说明", "该语句没有执行计划".to_string()));
        }
    }

    Ok(QueryResult::Explain {
        columns: vec!["步骤".to_string(), "内容".to_string()],
        rows,
    })
}
