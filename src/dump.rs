//! SQL 文本导出 / 导入（CLI `dump` / `load`）。
//!
//! 导出的是**自包含、可读、可改**的 SQL 脚本：
//! `CREATE TABLE`（类型 / DECIMAL 标度 / NOT NULL / UNIQUE / DEFAULT / 主键）、
//! 显式 `CREATE INDEX`、以及分批 `INSERT`。
//!
//! 之所以走 SQL 而不是「拷数据目录」：拷目录在运行中不安全（页缓存 + WAL
//! 可能不一致），而 SQL 脚本既能当备份、又能进版本控制、还能跨版本导入。

use std::fmt::Write as _;

use crate::codec::Value;
use crate::engine::Engine;
use crate::error::{DbError, Result};
use crate::sql::executor::Session;
use crate::table::Table;

/// 一条 INSERT 里最多放多少行（太大容易撞单语句行数/内存上限）
const ROWS_PER_INSERT: usize = 100;

/// 导出整个数据目录为 SQL 脚本
pub fn dump_script(engine: &mut Engine) -> Result<String> {
    // 先把 schema 渲染成字符串（scan 需要 &mut Engine，不能边迭代边扫）
    struct TableDump {
        name: String,
        create: String,
        indexes: Vec<String>,
        columns: String,
    }
    let schema: Vec<TableDump> = engine
        .tables
        .iter()
        .map(|table| {
            let mut indexes = Vec::new();
            for index in &table.indexes {
                // 主键与列级 UNIQUE 由列定义自动重建，不重复导出
                if index.primary || index.implicit {
                    continue;
                }
                let columns: Vec<String> = index
                    .columns
                    .iter()
                    .map(|ci| quote_ident(&table.columns[*ci].name))
                    .collect();
                indexes.push(format!(
                    "CREATE {}INDEX {} ON {} ({});",
                    if index.unique { "UNIQUE " } else { "" },
                    quote_ident(&index.name),
                    quote_ident(&table.name),
                    columns.join(", ")
                ));
            }
            TableDump {
                name: table.name.clone(),
                create: create_table_sql(table),
                indexes,
                columns: table
                    .columns
                    .iter()
                    .map(|c| quote_ident(&c.name))
                    .collect::<Vec<_>>()
                    .join(","),
            }
        })
        .collect();

    let mut out = String::new();
    let _ = writeln!(out, "-- LittleDB dump");
    let _ = writeln!(
        out,
        "-- 导入：littledb load <目标目录> <本文件>；DECIMAL 以字符串字面量保存"
    );

    for table in schema {
        let _ = writeln!(out);
        let _ = writeln!(out, "{}", table.create);
        for index_ddl in &table.indexes {
            let _ = writeln!(out, "{}", index_ddl);
        }

        let rows = engine.scan(&table.name)?;
        if rows.is_empty() {
            continue;
        }
        for chunk in rows.chunks(ROWS_PER_INSERT) {
            let mut values = Vec::with_capacity(chunk.len());
            for (_, row) in chunk {
                let rendered: Vec<String> = row.iter().map(sql_literal).collect();
                values.push(format!("({})", rendered.join(",")));
            }
            let _ = writeln!(
                out,
                "INSERT INTO {} ({}) VALUES {};",
                quote_ident(&table.name),
                table.columns,
                values.join(",")
            );
        }
    }
    Ok(out)
}

/// 表结构 → `CREATE TABLE`
fn create_table_sql(table: &Table) -> String {
    let pk: Vec<&str> = table
        .columns
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| c.name.as_str())
        .collect();
    let composite_pk = pk.len() > 1;

    let mut defs = Vec::with_capacity(table.columns.len() + 1);
    for column in &table.columns {
        let mut def = format!("  {} {}", quote_ident(&column.name), column_type_sql(column));
        // 复合主键必须用表级 PRIMARY KEY，列级写法语义不同
        if !composite_pk && column.primary_key {
            def.push_str(" PRIMARY KEY");
        }
        if column.not_null && !column.primary_key {
            def.push_str(" NOT NULL");
        }
        if column.unique && !column.primary_key {
            def.push_str(" UNIQUE");
        }
        if let Some(default) = &column.default {
            def.push_str(" DEFAULT ");
            def.push_str(&sql_literal(default));
        }
        defs.push(def);
    }
    if composite_pk {
        let names: Vec<String> = pk.iter().map(|name| quote_ident(name)).collect();
        defs.push(format!("  PRIMARY KEY ({})", names.join(", ")));
    }
    format!(
        "CREATE TABLE {} (\n{}\n);",
        quote_ident(&table.name),
        defs.join(",\n")
    )
}

fn column_type_sql(column: &crate::codec::Column) -> String {
    if column.ty == crate::codec::ColumnType::Decimal {
        // 不带标度会退回默认 4 位，导致精度丢失
        format!("DECIMAL(38,{})", column.scale)
    } else {
        column.ty.as_str().to_string()
    }
}

/// 标识符：双引号包裹，内部 `"` 翻倍（与词法器的双引号标识符一致）
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// 值 → SQL 字面量
pub fn sql_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(true) => "TRUE".to_string(),
        Value::Bool(false) => "FALSE".to_string(),
        Value::Int(n) => n.to_string(),
        Value::Real(n) => {
            // Rust 的 `{:?}` 给的是「能原样解析回来」的最短表示；
            // 极值会带指数（1e300），词法器不认指数，所以走 CAST
            let text = format!("{:?}", n);
            if text.contains('e') || text.contains('E') || !text.contains('.') {
                format!("CAST('{}' AS REAL)", text)
            } else {
                text
            }
        }
        // DECIMAL 以字符串保真（标度信息在列定义里）
        Value::Decimal { v, scale } => format!("'{}'", crate::codec::format_decimal(*v, *scale)),
        Value::Text(text) => format!("'{}'", text.replace('\'', "''")),
    }
}

/// SQL 感知的语句切分：只认引号/注释外的 `;`
pub fn split_statements(script: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = script.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(ch) = chars.next() {
        // 行注释：丢弃到行尾（引号内不算）
        if !in_single && !in_double && ch == '-' && chars.peek() == Some(&'-') {
            while let Some(next) = chars.next() {
                if next == '\n' {
                    break;
                }
            }
            continue;
        }
        // 块注释
        if !in_single && !in_double && ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(next) = chars.next() {
                if next == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
            continue;
        }
        if in_single {
            current.push(ch);
            if ch == '\'' {
                if chars.peek() == Some(&'\'') {
                    current.push(chars.next().unwrap_or('\''));
                } else {
                    in_single = false;
                }
            }
            continue;
        }
        if in_double {
            current.push(ch);
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    current.push(chars.next().unwrap_or('"'));
                } else {
                    in_double = false;
                }
            }
            continue;
        }
        match ch {
            '\'' => {
                in_single = true;
                current.push(ch);
            }
            '"' => {
                in_double = true;
                current.push(ch);
            }
            ';' => {
                let trimmed = current.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    out
}

/// 导入结果
pub struct LoadStats {
    pub statements: usize,
    pub batches: usize,
}

/// 把 SQL 脚本导入到会话指向的库。
///
/// 每 `batch_size` 条语句包一个事务：既摊薄 fsync，又不让 WAL 无界增长。
/// 任一批失败即回滚该批并返回错误（带出错语句的片段），不做「跳过继续」——
/// 静默漏数据比直接失败更难查。
pub fn load_script(session: &mut Session, script: &str, batch_size: usize) -> Result<LoadStats> {
    let statements = split_statements(script);
    if statements.is_empty() {
        return Err(DbError::syntax("脚本里没有任何语句"));
    }
    let batch = batch_size.max(1);
    let mut batches = 0usize;
    for chunk in statements.chunks(batch) {
        run_chunk(session, chunk)?;
        batches += 1;
    }
    Ok(LoadStats {
        statements: statements.len(),
        batches,
    })
}

fn run_chunk(session: &mut Session, statements: &[String]) -> Result<()> {
    if statements.len() == 1 {
        session.execute(&statements[0], &[])?;
        return Ok(());
    }
    session.execute("BEGIN", &[])?;
    for statement in statements {
        if let Err(err) = session.execute(statement, &[]) {
            let _ = session.execute("ROLLBACK", &[]);
            return Err(DbError::new(
                err.code,
                format!(
                    "导入失败（本批 {} 条已回滚）：{}\n出错语句：{}",
                    statements.len(),
                    err.message,
                    preview(statement)
                ),
            ));
        }
    }
    session.execute("COMMIT", &[])?;
    Ok(())
}

fn preview(statement: &str) -> String {
    let one_line: String = statement.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut text: String = one_line.chars().take(160).collect();
    if one_line.chars().count() > 160 {
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_ignores_semicolons_in_literals_and_comments() {
        let script = "-- 注释里有个 ; 不算\n\
                      CREATE TABLE t (a TEXT);\n\
                      INSERT INTO t VALUES ('x;y');\n\
                      INSERT INTO t VALUES ('it''s'); -- 行尾注释 ;\n\
                      /* 块注释 ; */ INSERT INTO t VALUES (\"半;列\");";
        let statements = split_statements(script);
        assert_eq!(statements.len(), 4, "{:?}", statements);
        assert!(statements[0].starts_with("CREATE TABLE"));
        assert!(statements[1].contains("'x;y'"));
        assert!(statements[2].contains("'it''s'"));
        assert!(statements[3].contains("\"半;列\""));
    }

    #[test]
    fn literals_round_trip_shape() {
        assert_eq!(sql_literal(&Value::Text("a'b".into())), "'a''b'");
        assert_eq!(sql_literal(&Value::Int(-5)), "-5");
        assert_eq!(sql_literal(&Value::Real(1.5)), "1.5");
        // 整数值的 REAL 必须带小数点或 CAST，否则会被当成 INT
        assert_eq!(sql_literal(&Value::Real(2.0)), "2.0");
        assert_eq!(
            sql_literal(&Value::Real(f64::MIN)),
            format!("CAST('{:?}' AS REAL)", f64::MIN)
        );
        assert_eq!(
            sql_literal(&Value::Decimal {
                v: 150,
                scale: 2
            }),
            "'1.50'"
        );
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }
}
