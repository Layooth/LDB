//! 语句执行器。
//!
//! SELECT 管线：取行 → WHERE → 分组 → 投影/聚合 → HAVING → DISTINCT
//!            → ORDER BY → LIMIT/OFFSET。
//! 写语句在 Session 层统一包事务：不在显式事务里就自动 begin/commit。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::codec::{self, Column, ColumnType, Value};
use crate::engine::Engine;
use crate::error::{DbError, Result};
use crate::json::Json;

use super::ast::*;
use super::eval::{
    apply_binary, apply_unary, contains_aggregate, eval, eval_aggregate, matches, RowContext,
};
use super::parser::Parser;
use super::AGGREGATES;

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
        rows: Vec<Vec<Value>>,
    },
    Insert {
        inserted: usize,
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
}

impl QueryResult {
    /// 转成二维表格用于展示；非结果集返回 None
    pub fn to_grid(&self) -> Option<(Vec<String>, Vec<Vec<Value>>)> {
        match self {
            QueryResult::Select { columns, rows } => Some((columns.clone(), rows.clone())),
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
            QueryResult::Insert { inserted } => format!("插入 {} 行", inserted),
            QueryResult::Update { updated } => format!("更新 {} 行", updated),
            QueryResult::Delete { deleted } => format!("删除 {} 行", deleted),
            QueryResult::Ddl { message } => message.clone(),
            QueryResult::Transaction { message } => message.clone(),
            QueryResult::Tables { tables } => format!("{} 张表", tables.len()),
            QueryResult::Describe { table, columns, .. } => {
                format!("表 {} · {} 列", table, columns.len())
            }
        }
    }

    /// 转成 LDBP/1 协议里的 result 字段。
    /// 字段名与 Node 版逐一对齐，否则现有客户端解析不了。
    pub fn to_wire_json(&self) -> Json {
        match self {
            QueryResult::Select { columns, rows } => Json::Obj(vec![
                ("type".to_string(), Json::str("select")),
                (
                    "columns".to_string(),
                    Json::Arr(columns.iter().map(|c| Json::str(c.clone())).collect()),
                ),
                ("rows".to_string(), rows_to_json(rows)),
                ("rowCount".to_string(), Json::Num(rows.len() as f64)),
            ]),
            QueryResult::Insert { inserted } => Json::Obj(vec![
                ("type".to_string(), Json::str("insert")),
                ("inserted".to_string(), Json::Num(*inserted as f64)),
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
        }
    }
}

fn value_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Int(n) => Json::Num(*n as f64),
        Value::Real(n) => Json::Num(*n),
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
        let statement = Parser::parse(sql)?;
        self.execute_statement(&statement, params)
    }

    pub fn execute_statement(
        &mut self,
        statement: &Statement,
        params: &[Value],
    ) -> Result<QueryResult> {
        execute_on(&mut self.engine, &mut self.in_txn, statement, params)
    }

    pub fn close(&mut self) -> Result<()> {
        if self.in_txn {
            self.engine.rollback();
            self.in_txn = false;
        }
        self.engine.close()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.in_txn {
            self.engine.rollback();
        }
    }
}

/// 服务端使用的会话：多个连接共享同一个引擎，靠互斥锁串行化，
/// 等价于 Node 版的「全局单写锁」语义。
pub struct ConnSession {
    engine: Arc<Mutex<Engine>>,
    in_txn: bool,
}

impl ConnSession {
    pub fn new(engine: Arc<Mutex<Engine>>) -> ConnSession {
        ConnSession {
            engine,
            in_txn: false,
        }
    }

    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        let statement = Parser::parse(sql)?;
        let mut guard = self
            .engine
            .lock()
            .map_err(|_| DbError::internal("数据库锁状态异常"))?;
        execute_on(&mut *guard, &mut self.in_txn, &statement, params)
    }

    pub fn in_transaction(&self) -> bool {
        self.in_txn
    }

    pub fn close(&mut self) {
        if self.in_txn {
            if let Ok(mut guard) = self.engine.lock() {
                guard.rollback();
            }
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
) -> Result<QueryResult> {
    match statement {
        Statement::Begin => {
            if *in_txn {
                return Ok(QueryResult::Transaction {
                    message: "事务已在进行中".to_string(),
                });
            }
            engine.begin()?;
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
            let outcome = engine.commit();
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
            engine.rollback();
            *in_txn = false;
            return Ok(QueryResult::Transaction {
                message: "事务已回滚".to_string(),
            });
        }
        _ => {}
    }

    if statement.is_read_only() || *in_txn {
        return run_with_engine(engine, statement, params);
    }

    // 单语句隐式事务
    let outcome = engine.write(|inner| run_with_engine(inner, statement, params));
    if outcome.is_err() {
        *in_txn = false;
    }
    outcome
}

fn run_with_engine(
    engine: &mut Engine,
    statement: &Statement,
    params: &[Value],
) -> Result<QueryResult> {
    match statement {
        Statement::Select(select) => exec_select(engine, select, params),
        Statement::Insert(insert) => exec_insert(engine, insert, params),
        Statement::Update(update) => exec_update(engine, update, params),
        Statement::Delete(delete) => exec_delete(engine, delete, params),
        Statement::CreateTable(create) => exec_create_table(engine, create, params),
        Statement::DropTable { name, if_exists } => {
            engine.drop_table(name, *if_exists)?;
            Ok(QueryResult::Ddl {
                message: format!("已删除表 \"{}\"", name),
            })
        }
        Statement::CreateIndex(create) => {
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
            engine.drop_index(name, *if_exists)?;
            Ok(QueryResult::Ddl {
                message: format!("已删除索引 \"{}\"", name),
            })
        }
        Statement::ShowTables => {
            let mut tables = Vec::new();
            for name in engine.table_names() {
                let (table_id, rows, columns) = {
                    let table = engine.table(&name)?;
                    (table.id, table.row_count, table.columns.len())
                };
                let pages = engine.store.page_count(table_id);
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
        Statement::Explain { statement } => exec_explain(engine, statement, params),
        Statement::Begin | Statement::Commit | Statement::Rollback => {
            Err(DbError::internal("事务控制语句不应到达执行器"))
        }
    }
}

// ------------------------------------------------------------------ SELECT

fn exec_select(engine: &mut Engine, stmt: &SelectStmt, params: &[Value]) -> Result<QueryResult> {
    let mut table_columns: Vec<Column> = Vec::new();
    let mut rows: Vec<RowContext> = Vec::new();

    if let Some(from) = &stmt.from {
        {
            let table = engine.table(&from.table)?;
            table_columns = table.columns.clone();
        }

        let candidates = index_candidates(&*engine, &from.table, stmt.where_clause.as_ref(), params)?;
        match candidates {
            Some(ids) => {
                for id in ids {
                    if let Some(values) = engine.get(&from.table, id)? {
                        rows.push(RowContext::new(&table_columns, values));
                    }
                }
            }
            None => {
                for (_, values) in engine.scan(&from.table)? {
                    rows.push(RowContext::new(&table_columns, values));
                }
            }
        }
    } else {
        rows.push(RowContext::new(&[], Vec::new()));
    }

    // WHERE
    if let Some(where_clause) = &stmt.where_clause {
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            let value = eval(where_clause, &row, params)?;
            if matches(&value) {
                kept.push(row);
            }
        }
        rows = kept;
    }

    // 投影列
    let mut out_columns: Vec<String> = Vec::new();
    let mut projections: Vec<Expr> = Vec::new();
    for item in &stmt.items {
        if item.star {
            for column in &table_columns {
                out_columns.push(column.name.clone());
                projections.push(Expr::Column {
                    table: None,
                    name: column.name.clone(),
                });
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

    // 分组
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

    // 投影 + HAVING + 排序键
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

    // DISTINCT
    if stmt.distinct {
        let mut seen: HashSet<String> = HashSet::new();
        output.retain(|(values, _)| {
            let key = values
                .iter()
                .map(|v| codec::index_key(v).unwrap_or_else(|| "null".to_string()))
                .collect::<Vec<String>>()
                .join("\u{1}");
            seen.insert(key)
        });
    }

    // ORDER BY
    if !stmt.order_by.is_empty() {
        let dirs: Vec<bool> = stmt.order_by.iter().map(|o| o.desc).collect();
        output.sort_by(|a, b| {
            for (i, desc) in dirs.iter().enumerate() {
                let left = a.1.get(i).cloned().unwrap_or(Value::Null);
                let right = b.1.get(i).cloned().unwrap_or(Value::Null);
                let mut ordering = codec::compare(&left, &right).unwrap_or(std::cmp::Ordering::Equal);
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

    // LIMIT / OFFSET
    let empty = RowContext::new(&[], Vec::new());
    let offset = match &stmt.offset {
        Some(expr) => to_usize(&eval(expr, &empty, params)?)?,
        None => 0,
    };
    let limit = match &stmt.limit {
        Some(expr) => Some(to_usize(&eval(expr, &empty, params)?)?),
        None => None,
    };

    let rows_out: Vec<Vec<Value>> = output
        .into_iter()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .map(|(values, _)| values)
        .collect();

    Ok(QueryResult::Select {
        columns: out_columns,
        rows: rows_out,
    })
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

/// 找到 WHERE 里可用的等值索引条件
fn index_candidates(
    engine: &Engine,
    table: &str,
    where_clause: Option<&Expr>,
    params: &[Value],
) -> Result<Option<Vec<u32>>> {
    let expr = match where_clause {
        Some(e) => e,
        None => return Ok(None),
    };
    let empty = RowContext::new(&[], Vec::new());
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
                if let Expr::Column { name, .. } = left.as_ref() {
                    let constant = eval(right, &empty, params);
                    if let Ok(value) = constant {
                        if !value.is_null() {
                            if let Some(ids) = engine.find_by_equality(table, name, &value)? {
                                return Ok(Some(ids));
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

// ------------------------------------------------------------------ 写语句

fn exec_insert(engine: &mut Engine, stmt: &InsertStmt, params: &[Value]) -> Result<QueryResult> {
    let columns = engine.table(&stmt.table)?.columns.clone();
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

    let empty = RowContext::new(&[], Vec::new());
    let mut inserted = 0usize;

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
            values[*slot] = codec::coerce(columns[*slot].ty, &raw)?;
        }

        engine.insert(&stmt.table, values)?;
        inserted += 1;
    }

    Ok(QueryResult::Insert { inserted })
}

fn exec_update(engine: &mut Engine, stmt: &UpdateStmt, params: &[Value]) -> Result<QueryResult> {
    let columns = engine.table(&stmt.table)?.columns.clone();
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

    let snapshot = engine.scan(&stmt.table)?;
    let mut updated = 0usize;

    for (row_id, values) in snapshot {
        let ctx = RowContext::new(&columns, values.clone());
        if let Some(where_clause) = &stmt.where_clause {
            let value = eval(where_clause, &ctx, params)?;
            if !matches(&value) {
                continue;
            }
        }

        let mut next = values.clone();
        for (slot, expr) in &assignments {
            let raw = eval(expr, &ctx, params)?;
            next[*slot] = codec::coerce(columns[*slot].ty, &raw)?;
        }

        engine.update(&stmt.table, row_id, next)?;
        updated += 1;
    }

    Ok(QueryResult::Update { updated })
}

fn exec_delete(engine: &mut Engine, stmt: &DeleteStmt, params: &[Value]) -> Result<QueryResult> {
    let columns = engine.table(&stmt.table)?.columns.clone();
    let snapshot = engine.scan(&stmt.table)?;
    let mut deleted = 0usize;

    for (row_id, values) in snapshot {
        if let Some(where_clause) = &stmt.where_clause {
            let ctx = RowContext::new(&columns, values);
            let value = eval(where_clause, &ctx, params)?;
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

fn exec_create_table(
    engine: &mut Engine,
    stmt: &CreateTableStmt,
    params: &[Value],
) -> Result<QueryResult> {
    if stmt.columns.is_empty() {
        return Err(DbError::syntax("建表至少需要一列"));
    }

    let empty = RowContext::new(&[], Vec::new());
    let mut columns = Vec::with_capacity(stmt.columns.len());

    for def in &stmt.columns {
        let ty = ColumnType::parse(&def.type_name)?;
        let is_pk = def.primary_key
            || stmt
                .primary_key
                .iter()
                .any(|n| n.to_lowercase() == def.name.to_lowercase());

        let mut column = Column::new(def.name.clone(), ty);
        column.primary_key = is_pk;
        column.not_null = def.not_null || is_pk;
        column.unique = def.unique;

        if let Some(default_expr) = &def.default {
            if !is_literal_expr(default_expr) {
                return Err(DbError::syntax(format!(
                    "列 {} 的默认值必须是字面量（例如 '2026-01-01'、0、TRUE），不支持函数或列引用",
                    def.name
                )));
            }
            let raw = eval(default_expr, &empty, params)?;
            column.default = if raw.is_null() {
                None
            } else {
                Some(codec::coerce(ty, &raw)?)
            };
        }

        columns.push(column);
    }

    engine.create_table(&stmt.name, columns, stmt.if_not_exists)?;
    Ok(QueryResult::Ddl {
        message: format!("已创建表 \"{}\"", stmt.name),
    })
}

// ------------------------------------------------------------------ EXPLAIN

fn kv(key: &str, value: String) -> Vec<Value> {
    vec![Value::Text(key.to_string()), Value::Text(value)]
}

fn exec_explain(
    engine: &mut Engine,
    statement: &Statement,
    params: &[Value],
) -> Result<QueryResult> {
    let mut rows: Vec<Vec<Value>> = Vec::new();
    rows.push(kv("语句", statement.kind_name().to_uppercase()));

    match statement {
        Statement::Select(select) => {
            if let Some(from) = &select.from {
                let (table_name, total) = {
                    let table = engine.table(&from.table)?;
                    (table.name.clone(), table.row_count)
                };
                rows.push(kv("表", table_name));

                let candidates =
                    index_candidates(&*engine, &from.table, select.where_clause.as_ref(), params)?;
                match candidates {
                    Some(ids) => {
                        rows.push(kv("访问方式", "索引查找".to_string()));
                        rows.push(kv("扫描行数", ids.len().to_string()));
                    }
                    None => {
                        rows.push(kv("访问方式", "全表扫描".to_string()));
                        rows.push(kv("扫描行数", total.to_string()));
                    }
                }
                rows.push(kv("表总行数", total.to_string()));

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
            rows.push(kv("访问方式", "全表扫描后逐行更新".to_string()));
            rows.push(kv("赋值", format!("{} 个列", update.assignments.len())));
        }
        Statement::Delete(delete) => {
            rows.push(kv("表", delete.table.clone()));
            rows.push(kv("访问方式", "全表扫描后逐行删除".to_string()));
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
