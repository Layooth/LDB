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
    /// 裸列名 -> value（单表行；JOIN 行里同名列只保留首个并记入 ambiguous）
    fields: HashMap<String, Value>,
    /// JOIN 行：限定名 "alias.column" -> value
    qualified: HashMap<String, Value>,
    /// 同名列出现在多张表里：裸名有歧义，必须写 表.列
    ambiguous: std::collections::HashSet<String>,
    /// 本行涉及的表别名（小写）
    aliases: Vec<String>,
    /// 外层查询的行（相关子查询用）：本层找不到的列向外层找
    parent: Option<std::rc::Rc<RowContext>>,
}

impl RowContext {
    pub fn new(columns: &[Column], values: Vec<Value>) -> RowContext {
        let mut fields = HashMap::with_capacity(columns.len());
        for (i, column) in columns.iter().enumerate() {
            if i < values.len() {
                fields.insert(column.name.to_lowercase(), values[i].clone());
            }
        }
        RowContext {
            values,
            fields,
            qualified: HashMap::new(),
            ambiguous: std::collections::HashSet::new(),
            aliases: Vec::new(),
            parent: None,
        }
    }

    /// 挂上外层行（相关子查询）：本地找不到的列会向外层解析。
    pub fn with_parent(mut self, parent: Option<std::rc::Rc<RowContext>>) -> RowContext {
        self.parent = parent;
        self
    }

    /// JOIN 组合行：sources = [(别名, 列定义, 在 values 中的起始下标)]
    pub fn from_tables(sources: &[(&str, &[Column], usize)], values: Vec<Value>) -> RowContext {
        let mut fields: HashMap<String, Value> = HashMap::new();
        let mut qualified: HashMap<String, Value> = HashMap::new();
        let mut ambiguous = std::collections::HashSet::new();
        let mut aliases = Vec::with_capacity(sources.len());
        for (alias, columns, offset) in sources {
            let alias_lc = alias.to_lowercase();
            aliases.push(alias_lc.clone());
            for (i, column) in columns.iter().enumerate() {
                let value = values
                    .get(*offset + i)
                    .cloned()
                    .unwrap_or(Value::Null);
                let col_lc = column.name.to_lowercase();
                qualified.insert(format!("{}.{}", alias_lc, col_lc), value.clone());
                if fields.insert(col_lc.clone(), value).is_some() {
                    ambiguous.insert(col_lc);
                }
            }
        }
        RowContext {
            values,
            fields,
            qualified,
            ambiguous,
            aliases,
            parent: None,
        }
    }

    /// 构造「现有行 + 一个只能用限定名访问的来源」上下文。
    ///
    /// UPSERT 用：`values` = 现有行 ++ 待插入行，`alias`（excluded）只写入
    /// 限定名映射，不参与裸列名与歧义判定——否则 `SET hits = hits + 1`
    /// 会因为两段都有 `hits` 被判成「歧义」。
    pub fn with_qualified_alias(columns: &[Column], values: Vec<Value>, alias: &str) -> RowContext {
        let width = columns.len();
        let alias_lc = alias.to_lowercase();
        let mut ctx = RowContext::new(columns, values);
        for (i, column) in columns.iter().enumerate() {
            let value = ctx.values.get(width + i).cloned().unwrap_or(Value::Null);
            ctx.qualified
                .insert(format!("{}.{}", alias_lc, column.name.to_lowercase()), value);
        }
        ctx.aliases.push(alias_lc);
        ctx
    }

    /// qualifier：`u.name` 里的 `u`；单表行传 None。
    ///
    /// 相关子查询：本层找不到时向外层（`parent`）继续找——
    /// 标准语义是「内层优先」，所以本地命中就直接返回，不会因为外层同名而报歧义。
    pub fn field(&self, qualifier: Option<&str>, name: &str) -> Result<&Value> {
        let name_lc = name.to_lowercase();
        if let Some(qualifier) = qualifier {
            let q_lc = qualifier.to_lowercase();
            if let Some(value) = self.qualified.get(&format!("{}.{}", q_lc, name_lc)) {
                return Ok(value);
            }
            if self.aliases.iter().any(|a| *a == q_lc) {
                return Err(DbError::not_found(format!(
                    "列 \"{}.{}\" 不存在",
                    qualifier, name
                )));
            }
            if let Some(parent) = &self.parent {
                return parent.field(Some(qualifier), name);
            }
            return Err(DbError::not_found(format!(
                "表或别名 \"{}\" 不在本次查询中",
                qualifier
            )));
        }
        if self.ambiguous.contains(&name_lc) {
            return Err(DbError::syntax(format!(
                "列名 \"{}\" 在多张表中存在歧义，请用 \"表.列\" 限定",
                name
            )));
        }
        if let Some(value) = self.fields.get(&name_lc) {
            return Ok(value);
        }
        if let Some(parent) = &self.parent {
            return parent.field(None, name);
        }
        Err(DbError::not_found(format!("列 \"{}\" 不存在", name)))
    }
}

pub fn literal_to_value(literal: &Literal) -> Value {
    match literal {
        Literal::Null => Value::Null,
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Int(n) => Value::Int(*n),
        Literal::Real(n) => Value::Real(*n),
        Literal::Decimal { v, scale } => Value::Decimal {
            v: *v,
            scale: *scale,
        },
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
        Value::Decimal { v, .. } => Some(*v != 0),
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
        Expr::InSubquery { expr, .. } => contains_aggregate(expr),
        Expr::Cast { expr, .. } => contains_aggregate(expr),
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

        Expr::Column { name, table } => ctx.field(table.as_deref(), name).cloned(),

        // EXISTS 要执行子查询，而 eval 手里没有引擎：
        // 执行器在逐行求值前会用 resolve_exists 把它换成布尔字面量。
        // 走到这里说明写在了不支持的位置（例如 SELECT 列表/ORDER BY）。
        Expr::Exists { .. } => Err(DbError::syntax(
            "EXISTS 只能用在 WHERE 或 JOIN ... ON 条件里（暂不支持 SELECT 列表、ORDER BY、HAVING）",
        )),

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

        Expr::InSubquery { .. } => Err(DbError::syntax(
            "子查询只支持在 WHERE 的 IN 条件中使用（且不支持关联外部列）",
        )),
        Expr::Cast { expr, ty, scale } => {
            let value = eval(expr, ctx, params)?;
            if value.is_null() {
                return Ok(Value::Null);
            }
            crate::codec::cast_value(*ty, *scale, &value)
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
            let matched = like_match(&text, &pattern_text)?;
            Ok(Value::Bool(if *negated { !matched } else { matched }))
        }
    }
}

/// 一元运算。抽成独立函数，供聚合表达式递归求值时复用。
pub fn apply_unary(op: UnOp, value: &Value) -> Result<Value> {
    match op {
        UnOp::Neg => match value {
            Value::Null => Ok(Value::Null),
            Value::Decimal { v, scale } => match v.checked_neg() {
                Some(nv) => Ok(Value::Decimal { v: nv, scale: *scale }),
                None => Err(DbError::type_error("DECIMAL 取负溢出")),
            },
            // i64::MIN 取负会溢出，升级为 REAL 而不是静默回绕
            Value::Int(n) => match n.checked_neg() {
                Some(v) => Ok(Value::Int(v)),
                None => Ok(Value::Real(-(*n as f64))),
            },
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
            compare_op(op, left, right)
        }
        _ => arith(op, left, right),
    }
}

/// 比较运算。NULL 参与 → NULL（三值逻辑）；
/// 非 NULL 但类型不可比 → 报 TYPE_ERROR。
///
/// 旧实现把「不可比」也返回 NULL，结果是 `WHERE id = '5'` 静默零行，
/// 用户只会觉得「数据莫名查不到」。宁可明确报错让调用方加 CAST。
fn compare_op(op: BinOp, left: &Value, right: &Value) -> Result<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let ordering = codec::compare(left, right).ok_or_else(|| {
        DbError::type_error(format!(
            "无法比较 {} 与 {}：类型不兼容，需要显式 CAST",
            left.type_name(),
            right.type_name()
        ))
    })?;
    let result = match op {
        BinOp::Eq => ordering == Ordering::Equal,
        BinOp::Ne => ordering != Ordering::Equal,
        BinOp::Lt => ordering == Ordering::Less,
        BinOp::Le => ordering != Ordering::Greater,
        BinOp::Gt => ordering == Ordering::Greater,
        BinOp::Ge => ordering != Ordering::Less,
        _ => false,
    };
    Ok(Value::Bool(result))
}

/// 定标十进制四则运算：加/减对齐标度后整数运算（精确），
/// 乘法标度相加（精确），除法结果标度取较大者（截断）。
/// 溢出/除零显式报错——金额场景宁可不返回结果，也不返回错误结果。
fn decimal_arith(op: BinOp, av: i128, ascale: u8, bv: i128, bscale: u8) -> Result<Value> {
    // 加/减/取模：对齐到较大标度后整数运算
    if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mod) {
        let scale = ascale.max(bscale);
        let a_factor = 10i128
            .checked_pow((scale - ascale) as u32)
            .ok_or_else(|| DbError::type_error("DECIMAL 运算溢出"))?;
        let b_factor = 10i128
            .checked_pow((scale - bscale) as u32)
            .ok_or_else(|| DbError::type_error("DECIMAL 运算溢出"))?;
        let a = av
            .checked_mul(a_factor)
            .ok_or_else(|| DbError::type_error("DECIMAL 运算溢出"))?;
        let b = bv
            .checked_mul(b_factor)
            .ok_or_else(|| DbError::type_error("DECIMAL 运算溢出"))?;
        let v = match op {
            BinOp::Add => a.checked_add(b),
            BinOp::Sub => a.checked_sub(b),
            BinOp::Mod => {
                if b == 0 {
                    return Err(DbError::type_error("取模的除数为 0"));
                }
                a.checked_rem(b)
            }
            _ => unreachable!(),
        }
        .ok_or_else(|| DbError::type_error("DECIMAL 运算溢出"))?;
        return Ok(Value::Decimal { v, scale });
    }

    // 乘：标度相加（0.01[s2] × 3[s0] = 0.03[s2]）
    if op == BinOp::Mul {
        let v = av
            .checked_mul(bv)
            .ok_or_else(|| DbError::type_error("DECIMAL 运算溢出"))?;
        // 标度是 u8：直接相加会溢出；同时不能超过列的标度上限
        let scale = ascale.checked_add(bscale).ok_or_else(|| {
            DbError::type_error("DECIMAL 乘法结果标度溢出（两个操作数标度都过大）")
        })?;
        if scale > crate::codec::DECIMAL_MAX_SCALE {
            return Err(DbError::type_error(format!(
                "DECIMAL 乘法结果标度 {} 超过上限 {}，请先用 ROUND/CAST 收敛",
                scale,
                crate::codec::DECIMAL_MAX_SCALE
            )));
        }
        return Ok(Value::Decimal { v, scale });
    }

    // 除：结果标度取较大者（截断），v = av · 10^(bscale - ascale + scale) / bv
    if op == BinOp::Div {
        if bv == 0 {
            return Err(DbError::type_error("除数为 0"));
        }
        let scale = ascale.max(bscale);
        let shift = bscale as i32 - ascale as i32 + scale as i32;
        let v = if shift >= 0 {
            let factor = 10i128
                .checked_pow(shift as u32)
                .ok_or_else(|| DbError::type_error("DECIMAL 运算溢出"))?;
            av.checked_mul(factor)
                .ok_or_else(|| DbError::type_error("DECIMAL 运算溢出"))?
                .div_euclid(bv)
        } else {
            (av / bv) / 10i128.pow((-shift) as u32)
        };
        return Ok(Value::Decimal { v, scale });
    }

    Err(DbError::internal("非算术运算符"))
}

fn arith(op: BinOp, left: &Value, right: &Value) -> Result<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }

    // DECIMAL：精确十进制运算（金额场景零浮点误差）；
    // Int 与 Decimal 混合时把 Int 提升为标度 0 的 Decimal
    let dec_pair = match (left, right) {
        (Value::Decimal { v: av, scale: sa }, Value::Decimal { v: bv, scale: sb }) => {
            Some((*av, *sa, *bv, *sb))
        }
        (Value::Decimal { v: av, scale: sa }, Value::Int(n)) => Some((*av, *sa, *n as i128, 0)),
        (Value::Int(n), Value::Decimal { v: bv, scale: sb }) => Some((*n as i128, 0, *bv, *sb)),
        _ => None,
    };
    if let Some((av, sa, bv, sb)) = dec_pair {
        return decimal_arith(op, av, sa, bv, sb);
    }

    // INT × INT 的加减乘走 i64 精确运算，避免 2^53 以上静默丢精度；
    // 溢出时按 SQLite 语义升级为 REAL。除/取模保持 f64 语义（除 0 报错）。
    if let (Value::Int(x), Value::Int(y)) = (left, right) {
        let exact = match op {
            BinOp::Add => x.checked_add(*y),
            BinOp::Sub => x.checked_sub(*y),
            BinOp::Mul => x.checked_mul(*y),
            _ => None,
        };
        if let Some(v) = exact {
            return Ok(Value::Int(v));
        }
        if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul) {
            let (fx, fy) = (*x as f64, *y as f64);
            let result = match op {
                BinOp::Add => fx + fy,
                BinOp::Sub => fx - fy,
                BinOp::Mul => fx * fy,
                _ => unreachable!(),
            };
            return Ok(Value::Real(result));
        }
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

/// LIKE 回溯步数上限。
///
/// 双指针回溯的内存是 O(1)，但「多个 % 交叉回溯」的最坏时间是 O(n·m)：
/// 2MB 请求体足以构造出 10^12 量级的比较次数（CPU 慢查询 / 拖垮服务）。
/// 超过预算直接报错，而不是让查询跑到天荒地老。
const MAX_LIKE_STEPS: usize = 5_000_000;

/// `%` 匹配任意长度，`_` 匹配单个字符。
///
/// 贪心双指针回溯匹配：常规情况 O(n+m)、内存 O(1)。
/// 之前的 O(n·m) DP 表在「长文本 × 长模式」下会分配巨量内存
/// （2MB 请求即可要求约 1TB），必须避免。
pub fn like_match(text: &str, pattern: &str) -> Result<bool> {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut i, mut j) = (0usize, 0usize);
    // 最近一个 '%' 及其当前尝试匹配到的 text 位置，失配时从它回溯
    let mut star: Option<(usize, usize)> = None;
    let mut steps = 0usize;

    while i < text.len() {
        steps += 1;
        if steps > MAX_LIKE_STEPS {
            return Err(DbError::new(
                crate::error::LIMIT,
                format!(
                    "LIKE 匹配规模过大（文本 {} 字符 × 模式 {} 字符），请缩短文本或改用更具体的前缀",
                    text.len(),
                    pattern.len()
                ),
            ));
        }
        if j < pattern.len() && (pattern[j] == '_' || pattern[j] == text[i]) {
            i += 1;
            j += 1;
        } else if j < pattern.len() && pattern[j] == '%' {
            star = Some((j, i));
            j += 1;
        } else if let Some((star_j, star_i)) = star {
            // 让最近的 '%' 多吞一个字符，从它的下一位置重新匹配
            j = star_j + 1;
            i = star_i + 1;
            star = Some((star_j, star_i + 1));
        } else {
            return Ok(false);
        }
    }
    while j < pattern.len() && pattern[j] == '%' {
        j += 1;
    }
    Ok(j == pattern.len())
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
                // i64::MIN 没有对应的正数：debug 会 panic、release 会静默回绕成负数，
                // 两种情况都不可接受，直接报错
                Value::Int(n) => n
                    .checked_abs()
                    .map(Value::Int)
                    .ok_or_else(|| DbError::type_error("ABS 结果超出 INT 范围（绝对值溢出）")),
                Value::Real(n) => Ok(Value::Real(n.abs())),
                Value::Decimal { v, scale } => v
                    .checked_abs()
                    .map(|abs| Value::Decimal { v: abs, scale })
                    .ok_or_else(|| DbError::type_error("ABS 结果超出 DECIMAL 范围")),
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
                if d.is_null() {
                    return Ok(Value::Null);
                }
                d.as_f64().map(|n| n as i32).unwrap_or(0)
            } else {
                0
            };
            // 位数必须限制：10f64.powi(400) = inf、powi(-400) = 0，
            // 会让结果变成 inf/NaN 并写进结果集
            const MAX_ROUND_DIGITS: i32 = 30;
            if !(-MAX_ROUND_DIGITS..=MAX_ROUND_DIGITS).contains(&digits) {
                return Err(DbError::type_error(format!(
                    "ROUND 的小数位数必须在 {}..={} 之间，实际是 {}",
                    -MAX_ROUND_DIGITS, MAX_ROUND_DIGITS, digits
                )));
            }
            let number = value
                .as_f64()
                .ok_or_else(|| DbError::type_error("ROUND 需要数字"))?;
            if !number.is_finite() {
                return Err(DbError::type_error("ROUND 需要有限数字"));
            }
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
        // ------------------------------------------------------------ 日期时间
        // 输入统一按 UTC 的 "YYYY-MM-DD[ HH:MM:SS]" 解析；
        // 输出形态跟随输入：带时分秒的给 DATETIME，只写日期的给 DATE。
        "DATE_ADD" | "DATE_SUB" => {
            require_args(name, args, 3)?;
            let value = eval(&args[0], ctx, params)?;
            if value.is_null() {
                return Ok(Value::Null);
            }
            let amount_value = eval(&args[1], ctx, params)?;
            if amount_value.is_null() {
                return Ok(Value::Null);
            }
            let amount = match &amount_value {
                Value::Int(n) => *n,
                Value::Real(n) => *n as i64,
                other => {
                    return Err(DbError::type_error(format!(
                        "{} 的第二个参数必须是整数，实际是 \"{}\"",
                        name,
                        other.as_text()
                    )))
                }
            };
            let unit_value = eval(&args[2], ctx, params)?;
            if unit_value.is_null() {
                return Ok(Value::Null);
            }
            let text = value.as_text();
            let with_time = text.contains(':');
            let millis = codec::parse_datetime_millis(&text)?;
            let delta = if name == "DATE_SUB" { -amount } else { amount };
            let shifted = codec::add_datetime(millis, delta, &unit_value.as_text())?;
            Ok(Value::Text(codec::format_millis(shifted, with_time)))
        }
        "DATEDIFF" => {
            require_args(name, args, 2)?;
            let left = eval(&args[0], ctx, params)?;
            let right = eval(&args[1], ctx, params)?;
            if left.is_null() || right.is_null() {
                return Ok(Value::Null);
            }
            let a = codec::parse_datetime_millis(&left.as_text())?.div_euclid(86_400_000);
            let b = codec::parse_datetime_millis(&right.as_text())?.div_euclid(86_400_000);
            Ok(Value::Int(a - b))
        }
        "TO_DATE" | "DATE_ONLY" => {
            require_args(name, args, 1)?;
            let value = eval(&args[0], ctx, params)?;
            if value.is_null() {
                return Ok(Value::Null);
            }
            let millis = codec::parse_datetime_millis(&value.as_text())?;
            Ok(Value::Text(codec::format_millis(millis, false)))
        }
        "YEAR" | "MONTH" | "DAY" | "HOUR" | "MINUTE" | "SECOND" => {
            require_args(name, args, 1)?;
            let value = eval(&args[0], ctx, params)?;
            if value.is_null() {
                return Ok(Value::Null);
            }
            let millis = codec::parse_datetime_millis(&value.as_text())?;
            let days = millis.div_euclid(86_400_000);
            let (year, month, day) = codec::civil_from_days(days);
            let rest = millis.rem_euclid(86_400_000);
            let hour = rest / 3_600_000;
            let minute = (rest % 3_600_000) / 60_000;
            let second = (rest % 60_000) / 1000;
            let part = match name {
                "YEAR" => year,
                "MONTH" => month as i64,
                "DAY" => day as i64,
                "HOUR" => hour,
                "MINUTE" => minute,
                _ => second,
            };
            Ok(Value::Int(part))
        }
        "DATE_FORMAT" => {
            require_args(name, args, 2)?;
            let value = eval(&args[0], ctx, params)?;
            if value.is_null() {
                return Ok(Value::Null);
            }
            let format = eval(&args[1], ctx, params)?;
            if format.is_null() {
                return Ok(Value::Null);
            }
            let millis = codec::parse_datetime_millis(&value.as_text())?;
            Ok(Value::Text(format_datetime_pattern(
                millis,
                &format.as_text(),
            )?))
        }
        other => Err(DbError::syntax(format!("不支持的函数 {}", other))),
    }
}

/// `DATE_FORMAT` 的格式串：支持 %Y %m %d %H %M %S %%，其它指示符直接报错
fn format_datetime_pattern(millis: i64, pattern: &str) -> Result<String> {
    let text = codec::format_datetime_from_millis(millis);
    let (year, month, day) = (&text[0..4], &text[5..7], &text[8..10]);
    let (hour, minute, second) = (&text[11..13], &text[14..16], &text[17..19]);

    let mut out = String::with_capacity(pattern.len() + 8);
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(year),
            Some('m') => out.push_str(month),
            Some('d') => out.push_str(day),
            Some('H') => out.push_str(hour),
            Some('M') => out.push_str(minute),
            Some('S') => out.push_str(second),
            Some('%') => out.push('%'),
            Some(other) => {
                return Err(DbError::syntax(format!(
                    "DATE_FORMAT 不支持 %{}（可用 %Y %m %d %H %M %S %%）",
                    other
                )))
            }
            None => return Err(DbError::syntax("DATE_FORMAT 的格式串以 % 结尾")),
        }
    }
    Ok(out)
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
        "SUM" | "AVG" if values.iter().all(|v| matches!(v, Value::Decimal { .. })) => {
            // DECIMAL 精确求和：对齐标度后整数累加（checked）
            let mut total: i128 = 0;
            let mut scale: u8 = 0;
            for value in &values {
                if let Value::Decimal { v, scale: s } = value {
                    // 先把累计值提升到较大标度，再对齐当前值
                    if *s > scale {
                        total = total
                            .checked_mul(10i128.pow((*s - scale) as u32))
                            .ok_or_else(|| DbError::type_error("SUM 溢出"))?;
                        scale = *s;
                    }
                    let aligned = v
                        .checked_mul(10i128.pow((*s - scale) as u32))
                        .ok_or_else(|| DbError::type_error("SUM 溢出"))?;
                    total = total
                        .checked_add(aligned)
                        .ok_or_else(|| DbError::type_error("SUM 溢出"))?;
                }
            }
            if name == "SUM" {
                return Ok(Value::Decimal { v: total, scale });
            }
            // AVG：商转 REAL（均值天然允许近似）
            let count = values.len() as f64;
            let total_f = total as f64 / 10f64.powi(scale as i32);
            Ok(Value::Real(total_f / count))
        }
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
        let m = |text: &str, pattern: &str| like_match(text, pattern).unwrap();
        assert!(m("alice", "a%"));
        assert!(m("alice", "%ice"));
        assert!(m("ace", "a_e"));
        assert!(!m("alice", "a_e"), "_ 只匹配单个字符");
        assert!(m("alice", "%"));
        assert!(!m("alice", "b%"));
        assert!(m("中文测试", "中文%"));
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

    #[test]
    fn like_semantics_match_sql() {
        // 与原 DP 实现语义对齐的基本用例
        let cases = [
            ("abc", "abc", true),
            ("abc", "a%", true),
            ("abc", "%c", true),
            ("abc", "%", true),
            ("abc", "a_c", true),
            ("abc", "b%", false),
            ("abc", "abcd", false),
            ("", "", true),
            ("", "%", true),
            ("", "_", false),
            ("abXbXc", "%X%", true),
            ("aXbXc", "a%b%c", true),
            ("aXbXd", "a%b%c", false),
            ("aaa", "%a%a%a%", true),
            ("abab", "%ab%ab%", true),
            ("中文abc", "中文%", true),
            ("中文abc", "%_c", true),
        ];
        for (text, pattern, expected) in cases {
            assert_eq!(
                like_match(text, pattern).unwrap(),
                expected,
                "'{}' LIKE '{}'",
                text,
                pattern
            );
        }
        // 多个 % 交叉回溯的对抗样例
        assert!(like_match("aaab", "a%a%a%b").unwrap());
        assert!(!like_match("aaab", "a%a%a%c").unwrap());
    }

    /// 对抗样例：`%aaaa…ab` 对长串 a 是 O(n·m) 量级
    /// （每次回溯都要重扫整段模式），必须撞上步数预算并快速报错
    #[test]
    fn like_backtracking_is_budgeted() {
        let text = "a".repeat(50_000);
        let pattern = format!("%{}b", "a".repeat(2_000));
        let start = std::time::Instant::now();
        let outcome = like_match(&text, &pattern);
        assert_eq!(
            outcome.as_ref().err().map(|e| e.code),
            Some(crate::error::LIMIT),
            "应撞上步数预算并报 LIMIT_EXCEEDED"
        );
        assert!(
            start.elapsed().as_millis() < 2_000,
            "应在预算内快速返回，实际 {:?}",
            start.elapsed()
        );
        // 同样规模但可快速判定的模式不受影响
        assert!(like_match(&text, "%aaaa%").unwrap());
    }

    #[test]
    fn like_is_linear_not_quadratic() {
        // 曾经：O(n·m) DP 表，n=m=30k 就要分配 ~900MB；
        // 现在是 O(n+m)，大输入应当瞬时完成
        let text = "a".repeat(200_000);
        let pattern = "%".to_string() + &"a".repeat(199_999);
        let start = std::time::Instant::now();
        assert!(like_match(&text, &pattern).unwrap());
        assert!(start.elapsed().as_millis() < 1000, "应远快于 1 秒");
    }

    #[test]
    fn int_arithmetic_is_exact() {
        let eval_int = |sql: &str| -> Value {
            let stmt = Parser::parse(sql).unwrap();
            match stmt {
                crate::sql::Statement::Select(select) => {
                    eval(&select.items[0].expr, &RowContext::new(&[], Vec::new()), &[]).unwrap()
                }
                other => panic!("期望 SELECT，得到 {:?}", other),
            }
        };

        // 2^53+1 曾经在 f64 里丢 1：返回 9007199254740992
        assert_eq!(
            eval_int("SELECT 9007199254740993 + 0"),
            Value::Int(9007199254740993)
        );
        // i64 溢出按 SQLite 语义升级为 REAL
        match eval_int("SELECT 9223372036854775807 + 1") {
            Value::Real(n) => assert!((n - 9.223372036854776e18).abs() < 1e10),
            other => panic!("溢出应升级为 REAL，实际 {:?}", other),
        }
        assert_eq!(eval_int("SELECT -9223372036854775807 - 1"), Value::Int(i64::MIN));
        assert_eq!(eval_int("SELECT 3 * 4"), Value::Int(12));
    }

    /// 标量函数的边界：一律报类型错误，不允许 panic / 回绕 / inf
    #[test]
    fn scalar_edge_cases_are_rejected() {
        fn eval_expr(sql: &str) -> Result<Value> {
            match Parser::parse(sql)? {
                crate::sql::Statement::Select(select) => {
                    eval(&select.items[0].expr, &RowContext::new(&[], Vec::new()), &[])
                }
                other => panic!("期望 SELECT，得到 {:?}", other),
            }
        }

        // ABS(i64::MIN)：没有对应的正数（旧实现 debug panic / release 回绕成负数）
        let err = eval_expr("SELECT ABS(CAST('-9223372036854775808' AS INT))").unwrap_err();
        assert_eq!(err.code, crate::error::TYPE, "{}", err.message);

        // ROUND 位数过大：10f64.powi(400) = inf，会把 inf/NaN 写进结果
        let err = eval_expr("SELECT ROUND(1.5, 400)").unwrap_err();
        assert_eq!(err.code, crate::error::TYPE, "{}", err.message);
        let err = eval_expr("SELECT ROUND(1.5, -400)").unwrap_err();
        assert_eq!(err.code, crate::error::TYPE, "{}", err.message);
        match eval_expr("SELECT ROUND(1.2345, 2)").unwrap() {
            Value::Real(n) => assert!((n - 1.23).abs() < 1e-9, "{}", n),
            other => panic!("期望 REAL，得到 {:?}", other),
        }

        // DECIMAL 乘法标度 9 + 9 = 18 > 上限 9：必须报错而不是溢出
        let err = eval_expr(
            "SELECT CAST('1.000000000' AS DECIMAL(20,9)) * CAST('2.000000000' AS DECIMAL(20,9))",
        )
        .unwrap_err();
        assert_eq!(err.code, crate::error::TYPE, "{}", err.message);
        // 小标度乘法仍然精确
        assert_eq!(
            eval_expr("SELECT CAST('1.25' AS DECIMAL(10,2)) * CAST('2' AS DECIMAL(10,0))").unwrap(),
            Value::Decimal { v: 250, scale: 2 }
        );
    }

    #[test]
    fn big_int_survives_wire_json() {
        // 曾经：计算层精确了，但线路上的 Json::Num 是 f64，>2^53 又被截断
        let stmt = Parser::parse("SELECT 9007199254740993").unwrap();
        let result = match stmt {
            crate::sql::Statement::Select(select) => {
                crate::sql::executor::exec_select(&test_engine(), &select, &[], false, None, None)
                    .unwrap()
            }
            other => panic!("期望 SELECT，得到 {:?}", other),
        };
        let wire = result.to_wire_json().to_string();
        assert!(wire.contains("9007199254740993"), "线路上应保留精确整数: {}", wire);
        assert!(!wire.contains("9007199254740992"), "不应被 f64 截断: {}", wire);
    }

    fn test_engine() -> crate::engine::Engine {
        let mut dir = std::env::temp_dir();
        dir.push(format!("littledb-eval-eng-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::engine::Engine::open(&dir).unwrap()
    }
}
