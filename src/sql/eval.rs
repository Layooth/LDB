//! 表达式求值。遵循 SQL 三值逻辑：NULL 参与比较得到 NULL，而不是 false。

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::codec::{self, Column, Value};
use crate::error::{DbError, Result};

use super::ast::{BinOp, Expr, Literal, UnOp};
use super::AGGREGATES;

#[derive(Debug, Clone)]
pub struct RowContext {
    pub values: Vec<Value>,
    fields: HashMap<String, Value>,
}

impl RowContext {
    pub fn new(columns: &[Column], values: Vec<Value>) -> RowContext {
        let mut fields = HashMap::with_capacity(columns.len());
        for (i, column) in columns.iter().enumerate() {
            if i < values.len() {
                fields.insert(column.name.to_lowercase(), values[i].clone());
            }
        }
        RowContext { values, fields }
    }

    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields.get(&name.to_lowercase())
    }
}

pub fn literal_to_value(literal: &Literal) -> Value {
    match literal {
        Literal::Null => Value::Null,
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Int(n) => Value::Int(*n),
        Literal::Real(n) => Value::Real(*n),
        Literal::Text(s) => Value::Text(s.clone()),
    }
}

/// SQL 布尔语义：只有 true 才「命中」
pub fn truthy(value: &Value) -> Option<bool> {
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(*b),
        Value::Int(n) => Some(*n != 0),
        Value::Real(n) => Some(*n != 0.0),
        Value::Text(s) => Some(!s.is_empty()),
    }
}

pub fn matches(value: &Value) -> bool {
    truthy(value) == Some(true)
}

fn bool_or_null(value: Option<bool>) -> Value {
    match value {
        Some(b) => Value::Bool(b),
        None => Value::Null,
    }
}

pub fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function { name, args, .. } => {
            AGGREGATES.contains(&name.as_str()) || args.iter().any(contains_aggregate)
        }
        Expr::Binary { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::Unary { operand, .. } => contains_aggregate(operand),
        Expr::IsNull { expr, .. } => contains_aggregate(expr),
        Expr::InList { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        Expr::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        Expr::Like { expr, pattern, .. } => {
            contains_aggregate(expr) || contains_aggregate(pattern)
        }
        _ => false,
    }
}

pub fn eval(expr: &Expr, ctx: &RowContext, params: &[Value]) -> Result<Value> {
    match expr {
        Expr::Literal(literal) => Ok(literal_to_value(literal)),

        Expr::Param(index) => params
            .get(*index)
            .cloned()
            .ok_or_else(|| DbError::syntax(format!("缺少第 {} 个参数", index + 1))),

        Expr::Column { name, .. } => ctx
            .field(name)
            .cloned()
            .ok_or_else(|| DbError::not_found(format!("列 \"{}\" 不存在", name))),

        Expr::Unary { op, operand } => {
            let value = eval(operand, ctx, params)?;
            apply_unary(*op, &value)
        }

        Expr::Binary { op, left, right } => match op {
            BinOp::And => {
                let lv = eval(left, ctx, params)?;
                if truthy(&lv) == Some(false) {
                    return Ok(Value::Bool(false));
                }
                let rv = eval(right, ctx, params)?;
                match (truthy(&lv), truthy(&rv)) {
                    (Some(true), Some(true)) => Ok(Value::Bool(true)),
                    (_, Some(false)) => Ok(Value::Bool(false)),
                    _ => Ok(Value::Null),
                }
            }
            BinOp::Or => {
                let lv = eval(left, ctx, params)?;
                if truthy(&lv) == Some(true) {
                    return Ok(Value::Bool(true));
                }
                let rv = eval(right, ctx, params)?;
                match (truthy(&lv), truthy(&rv)) {
                    (Some(false), Some(false)) => Ok(Value::Bool(false)),
                    (_, Some(true)) => Ok(Value::Bool(true)),
                    _ => Ok(Value::Null),
                }
            }
            _ => {
                let lv = eval(left, ctx, params)?;
                let rv = eval(right, ctx, params)?;
                apply_binary(*op, &lv, &rv)
            }
        },

        Expr::Function {
            name,
            args,
            distinct,
            star,
        } => {
            if AGGREGATES.contains(&name.as_str()) {
                return Err(DbError::syntax(format!(
                    "聚合函数 {} 不能出现在此位置",
                    name
                )));
            }
            eval_scalar(name, args, *distinct, *star, ctx, params)
        }

        Expr::IsNull { expr, negated } => {
            let value = eval(expr, ctx, params)?;
            let is_null = value.is_null();
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }

        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let target = eval(expr, ctx, params)?;
            if target.is_null() {
                return Ok(Value::Null);
            }
            let mut found = false;
            let mut has_null = false;
            for item in list {
                let value = eval(item, ctx, params)?;
                if value.is_null() {
                    has_null = true;
                    continue;
                }
                if codec::values_equal(&target, &value) == Some(true) {
                    found = true;
                    break;
                }
            }
            if found {
                Ok(Value::Bool(!*negated))
            } else if has_null {
                Ok(Value::Null)
            } else {
                Ok(Value::Bool(*negated))
            }
        }

        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let value = eval(expr, ctx, params)?;
            let low_value = eval(low, ctx, params)?;
            let high_value = eval(high, ctx, params)?;
            if value.is_null() || low_value.is_null() || high_value.is_null() {
                return Ok(Value::Null);
            }
            let ge_low = codec::compare(&value, &low_value)
                .map(|o| o != Ordering::Less)
                .unwrap_or(false);
            let le_high = codec::compare(&value, &high_value)
                .map(|o| o != Ordering::Greater)
                .unwrap_or(false);
            let inside = ge_low && le_high;
            Ok(Value::Bool(if *negated { !inside } else { inside }))
        }

        Expr::Like {
            expr,
            pattern,
            negated,
        } => {
            let value = eval(expr, ctx, params)?;
            let pattern_value = eval(pattern, ctx, params)?;
            if value.is_null() || pattern_value.is_null() {
                return Ok(Value::Null);
            }
            let text = value.as_text();
            let pattern_text = pattern_value.as_text();
            let matched = like_match(&text, &pattern_text);
            Ok(Value::Bool(if *negated { !matched } else { matched }))
        }
    }
}

/// 一元运算。抽成独立函数，供聚合表达式递归求值时复用。
pub fn apply_unary(op: UnOp, value: &Value) -> Result<Value> {
    match op {
        UnOp::Neg => match value {
            Value::Null => Ok(Value::Null),
            Value::Int(n) => Ok(Value::Int(-n)),
            Value::Real(n) => Ok(Value::Real(-n)),
            other => Err(DbError::type_error(format!(
                "无法对 \"{}\" 取负",
                other.as_text()
            ))),
        },
        UnOp::Plus => Ok(value.clone()),
        UnOp::Not => Ok(bool_or_null(truthy(value).map(|b| !b))),
    }
}

/// 二元运算。And/Or 在这里不做短路（两侧都已求值），语义与短路版本一致。
pub fn apply_binary(op: BinOp, left: &Value, right: &Value) -> Result<Value> {
    match op {
        BinOp::And => Ok(match (truthy(left), truthy(right)) {
            (Some(false), _) | (_, Some(false)) => Value::Bool(false),
            (Some(true), Some(true)) => Value::Bool(true),
            _ => Value::Null,
        }),
        BinOp::Or => Ok(match (truthy(left), truthy(right)) {
            (Some(true), _) | (_, Some(true)) => Value::Bool(true),
            (Some(false), Some(false)) => Value::Bool(false),
            _ => Value::Null,
        }),
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            Ok(compare_op(op, left, right))
        }
        _ => arith(op, left, right),
    }
}

fn compare_op(op: BinOp, left: &Value, right: &Value) -> Value {
    if left.is_null() || right.is_null() {
        return Value::Null;
    }
    let ordering = match codec::compare(left, right) {
        Some(o) => o,
        None => return Value::Null,
    };
    let result = match op {
        BinOp::Eq => ordering == Ordering::Equal,
        BinOp::Ne => ordering != Ordering::Equal,
        BinOp::Lt => ordering == Ordering::Less,
        BinOp::Le => ordering != Ordering::Greater,
        BinOp::Gt => ordering == Ordering::Greater,
        BinOp::Ge => ordering != Ordering::Less,
        _ => false,
    };
    Value::Bool(result)
}

fn arith(op: BinOp, left: &Value, right: &Value) -> Result<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let x = left
        .as_f64()
        .ok_or_else(|| DbError::type_error(format!("无法把 \"{}\" 用于算术运算", left.as_text())))?;
    let y = right
        .as_f64()
        .ok_or_else(|| DbError::type_error(format!("无法把 \"{}\" 用于算术运算", right.as_text())))?;

    let result = match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::Div => {
            if y == 0.0 {
                return Err(DbError::type_error("除数为 0"));
            }
            x / y
        }
        BinOp::Mod => {
            if y == 0.0 {
                return Err(DbError::type_error("取模的除数为 0"));
            }
            x % y
        }
        _ => return Err(DbError::internal("非算术运算符")),
    };

    if result.fract() == 0.0 && result.abs() < 9007199254740992.0 {
        Ok(Value::Int(result as i64))
    } else {
        Ok(Value::Real(result))
    }
}

/// `%` 匹配任意长度，`_` 匹配单个字符
pub fn like_match(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let n = text.len();
    let m = pattern.len();

    let mut dp = vec![vec![false; m + 1]; n + 1];
    dp[0][0] = true;
    for j in 1..=m {
        if pattern[j - 1] == '%' {
            dp[0][j] = dp[0][j - 1];
        }
    }
    for i in 1..=n {
        for j in 1..=m {
            dp[i][j] = match pattern[j - 1] {
                '%' => dp[i][j - 1] || dp[i - 1][j],
                '_' => dp[i - 1][j - 1],
                ch => dp[i - 1][j - 1] && text[i - 1] == ch,
            };
        }
    }
    dp[n][m]
}

fn eval_scalar(
    name: &str,
    args: &[Expr],
    _distinct: bool,
    _star: bool,
    ctx: &RowContext,
    params: &[Value],
) -> Result<Value> {
    match name {
        "COALESCE" => {
            for arg in args {
                let value = eval(arg, ctx, params)?;
                if !value.is_null() {
                    return Ok(value);
                }
            }
            Ok(Value::Null)
        }
        "LENGTH" => {
            require_args(name, args, 1)?;
            let value = eval(&args[0], ctx, params)?;
            if value.is_null() {
                return Ok(Value::Null);
            }
            Ok(Value::Int(value.as_text().chars().count() as i64))
        }
        "UPPER" => {
            require_args(name, args, 1)?;
            let value = eval(&args[0], ctx, params)?;
            if value.is_null() {
                return Ok(Value::Null);
            }
            Ok(Value::Text(value.as_text().to_uppercase()))
        }
        "LOWER" => {
            require_args(name, args, 1)?;
            let value = eval(&args[0], ctx, params)?;
            if value.is_null() {
                return Ok(Value::Null);
            }
            Ok(Value::Text(value.as_text().to_lowercase()))
        }
        "ABS" => {
            require_args(name, args, 1)?;
            let value = eval(&args[0], ctx, params)?;
            match value {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Int(n.abs())),
                Value::Real(n) => Ok(Value::Real(n.abs())),
                other => Err(DbError::type_error(format!(
                    "ABS 需要数字，实际是 \"{}\"",
                    other.as_text()
                ))),
            }
        }
        "ROUND" => {
            if args.is_empty() || args.len() > 2 {
                return Err(DbError::syntax("ROUND 接受 1 到 2 个参数"));
            }
            let value = eval(&args[0], ctx, params)?;
            if value.is_null() {
                return Ok(Value::Null);
            }
            let digits = if args.len() == 2 {
                let d = eval(&args[1], ctx, params)?;
                d.as_f64().unwrap_or(0.0) as i32
            } else {
                0
            };
            let number = value
                .as_f64()
                .ok_or_else(|| DbError::type_error("ROUND 需要数字"))?;
            let factor = 10f64.powi(digits);
            let rounded = (number * factor).round() / factor;
            if rounded.fract() == 0.0 && rounded.abs() < 9007199254740992.0 {
                Ok(Value::Int(rounded as i64))
            } else {
                Ok(Value::Real(rounded))
            }
        }
        "NOW" | "CURRENT_TIMESTAMP" => {
            Ok(Value::Text(now_datetime()))
        }
        "CURDATE" | "CURRENT_DATE" | "TODAY" => Ok(Value::Text(now_date())),
        other => Err(DbError::syntax(format!("不支持的函数 {}", other))),
    }
}

fn require_args(name: &str, args: &[Expr], count: usize) -> Result<()> {
    if args.len() != count {
        return Err(DbError::syntax(format!(
            "{} 需要 {} 个参数，实际给了 {}",
            name,
            count,
            args.len()
        )));
    }
    Ok(())
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(_) => 0,
    }
}

pub fn now_datetime() -> String {
    codec::format_datetime_from_millis(now_millis())
}

pub fn now_date() -> String {
    let text = now_datetime();
    text[..10].to_string()
}

/// 把聚合函数在一组行上算出来
pub fn eval_aggregate(
    name: &str,
    arg: Option<&Expr>,
    distinct: bool,
    star: bool,
    rows: &[RowContext],
    params: &[Value],
) -> Result<Value> {
    if star || arg.is_none() {
        if name == "COUNT" {
            return Ok(Value::Int(rows.len() as i64));
        }
        return Err(DbError::syntax(format!("{} 需要一个参数", name)));
    }
    let expr = arg.unwrap();

    let mut values: Vec<Value> = Vec::with_capacity(rows.len());
    for row in rows {
        let value = eval(expr, row, params)?;
        if value.is_null() {
            continue;
        }
        values.push(value);
    }

    if distinct {
        let mut seen: Vec<String> = Vec::new();
        let mut unique: Vec<Value> = Vec::new();
        for value in values {
            if let Some(key) = codec::index_key(&value) {
                if !seen.contains(&key) {
                    seen.push(key);
                    unique.push(value);
                }
            }
        }
        values = unique;
    }

    match name {
        "COUNT" => Ok(Value::Int(values.len() as i64)),
        "SUM" | "AVG" => {
            if values.is_empty() {
                return Ok(Value::Null);
            }
            let mut total = 0.0;
            let mut all_int = true;
            for value in &values {
                match value {
                    Value::Int(n) => total += *n as f64,
                    Value::Real(n) => {
                        all_int = false;
                        total += *n;
                    }
                    other => {
                        return Err(DbError::type_error(format!(
                            "{} 只能用于数字列，遇到 \"{}\"",
                            name,
                            other.as_text()
                        )))
                    }
                }
            }
            if name == "AVG" {
                return Ok(Value::Real(total / values.len() as f64));
            }
            if all_int && total.fract() == 0.0 && total.abs() < 9007199254740992.0 {
                Ok(Value::Int(total as i64))
            } else {
                Ok(Value::Real(total))
            }
        }
        "MIN" | "MAX" => {
            let mut best: Option<Value> = None;
            for value in values {
                best = match best {
                    None => Some(value),
                    Some(current) => {
                        let ordering = codec::compare(&value, &current).unwrap_or(Ordering::Equal);
                        let replace = if name == "MIN" {
                            ordering == Ordering::Less
                        } else {
                            ordering == Ordering::Greater
                        };
                        Some(if replace { value } else { current })
                    }
                };
            }
            Ok(best.unwrap_or(Value::Null))
        }
        other => Err(DbError::syntax(format!("不支持的聚合函数 {}", other))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::ColumnType;
    use crate::sql::parser::Parser;

    fn columns() -> Vec<Column> {
        vec![
            Column::new("id", ColumnType::Int),
            Column::new("name", ColumnType::Text),
            Column::new("score", ColumnType::Int),
        ]
    }

    fn ctx(id: i64, name: &str, score: Value) -> RowContext {
        RowContext::new(
            &columns(),
            vec![Value::Int(id), Value::Text(name.to_string()), score],
        )
    }

    fn eval_where(sql: &str, row: &RowContext) -> Value {
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            crate::sql::Statement::Select(select) => {
                eval(&select.where_clause.unwrap(), row, &[]).unwrap()
            }
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    }

    #[test]
    fn three_valued_logic() {
        let row = ctx(1, "a", Value::Null);
        assert_eq!(eval_where("SELECT * FROM t WHERE id > 0", &row), Value::Bool(true));
        // NULL 参与比较得到 NULL，不是 false
        assert_eq!(eval_where("SELECT * FROM t WHERE score > 5", &row), Value::Null);
        assert_eq!(
            eval_where("SELECT * FROM t WHERE NOT (score > 5)", &row),
            Value::Null
        );
        // false AND NULL -> false
        assert_eq!(
            eval_where("SELECT * FROM t WHERE id < 0 AND score > 5", &row),
            Value::Bool(false)
        );
        // true AND NULL -> NULL
        assert_eq!(
            eval_where("SELECT * FROM t WHERE id > 0 AND score > 5", &row),
            Value::Null
        );
    }

    #[test]
    fn like_patterns() {
        assert!(like_match("alice", "a%"));
        assert!(like_match("alice", "%ice"));
        assert!(like_match("ace", "a_e"));
        assert!(!like_match("alice", "a_e"), "_ 只匹配单个字符");
        assert!(like_match("alice", "%"));
        assert!(!like_match("alice", "b%"));
        assert!(like_match("中文测试", "中文%"));
    }

    #[test]
    fn arithmetic_and_precedence() {
        let row = ctx(1, "a", Value::Int(10));
        let stmt = Parser::parse("SELECT score + 5 * 2 AS x").unwrap();
        match stmt {
            crate::sql::Statement::Select(select) => {
                let value = eval(&select.items[0].expr, &row, &[]).unwrap();
                assert_eq!(value, Value::Int(20));
            }
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    }

    #[test]
    fn in_and_between() {
        let row = ctx(3, "a", Value::Int(10));
        assert_eq!(
            eval_where("SELECT * FROM t WHERE id IN (1, 3, 5)", &row),
            Value::Bool(true)
        );
        assert_eq!(
            eval_where("SELECT * FROM t WHERE id IN (1, 2)", &row),
            Value::Bool(false)
        );
        assert_eq!(
            eval_where("SELECT * FROM t WHERE score BETWEEN 5 AND 15", &row),
            Value::Bool(true)
        );
        assert_eq!(
            eval_where("SELECT * FROM t WHERE score NOT BETWEEN 5 AND 15", &row),
            Value::Bool(false)
        );
    }

    #[test]
    fn is_null_and_functions() {
        let row = ctx(1, "alice", Value::Null);
        assert_eq!(
            eval_where("SELECT * FROM t WHERE score IS NULL", &row),
            Value::Bool(true)
        );
        assert_eq!(
            eval_where("SELECT * FROM t WHERE score IS NOT NULL", &row),
            Value::Bool(false)
        );

        let stmt = Parser::parse("SELECT UPPER(name), LENGTH(name), COALESCE(score, -1)").unwrap();
        match stmt {
            crate::sql::Statement::Select(select) => {
                assert_eq!(
                    eval(&select.items[0].expr, &row, &[]).unwrap(),
                    Value::Text("ALICE".to_string())
                );
                assert_eq!(eval(&select.items[1].expr, &row, &[]).unwrap(), Value::Int(5));
                assert_eq!(eval(&select.items[2].expr, &row, &[]).unwrap(), Value::Int(-1));
            }
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    }

    #[test]
    fn aggregation() {
        let rows = vec![
            ctx(1, "a", Value::Int(10)),
            ctx(2, "b", Value::Int(20)),
            ctx(3, "c", Value::Null),
        ];
        let stmt = Parser::parse("SELECT COUNT(score) AS c").unwrap();
        match stmt {
            crate::sql::Statement::Select(select) => match &select.items[0].expr {
                Expr::Function {
                    name,
                    args,
                    distinct,
                    star,
                } => {
                    let value =
                        eval_aggregate(name, args.get(0), *distinct, *star, &rows, &[]).unwrap();
                    assert_eq!(value, Value::Int(2), "COUNT 应忽略 NULL");
                }
                other => panic!("期望函数，得到 {:?}", other),
            },
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    }

    #[test]
    fn aggregate_detection() {
        let stmt = Parser::parse("SELECT COUNT(*) + 1").unwrap();
        match stmt {
            crate::sql::Statement::Select(select) => {
                assert!(contains_aggregate(&select.items[0].expr));
            }
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
        let stmt = Parser::parse("SELECT id + 1").unwrap();
        match stmt {
            crate::sql::Statement::Select(select) => {
                assert!(!contains_aggregate(&select.items[0].expr));
            }
            other => panic!("期望 SELECT，得到 {:?}", other),
        }
    }
}
