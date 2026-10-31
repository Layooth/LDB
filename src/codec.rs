//! 行记录编解码与类型系统。
//!
//! ```text
//! [null bitmap: ceil(n/8) 字节]
//! 非空列依次编码：
//!   INT      -> i64 小端
//!   REAL     -> f64 小端
//!   BOOL     -> 1 字节
//!   TEXT/DATE/DATETIME/JSON -> u32 长度 + UTF-8
//! ```

use std::cmp::Ordering;

use crate::error::{DbError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Int,
    Real,
    Text,
    Bool,
    Date,
    DateTime,
    Json,
    /// 定标十进制：i128 存储，标度（小数位数）在 Column.scale 上
    Decimal,
}

/// DECIMAL 默认标度
pub const DECIMAL_DEFAULT_SCALE: u8 = 4;
/// DECIMAL 标度上限（保证 i128 对齐运算不轻易溢出）
pub const DECIMAL_MAX_SCALE: u8 = 9;

impl ColumnType {
    pub fn parse(raw: &str) -> Result<ColumnType> {
        // 允许 "DECIMAL(10,2)" 形式：截取括号前的类型名
        let base = raw.find('(').map(|i| &raw[..i]).unwrap_or(raw);
        let upper = base.trim().to_uppercase();
        let ty = match upper.as_str() {
            "INT" | "INTEGER" | "BIGINT" | "SMALLINT" => ColumnType::Int,
            "REAL" | "FLOAT" | "DOUBLE" => ColumnType::Real,
            "TEXT" | "VARCHAR" | "CHAR" | "STRING" => ColumnType::Text,
            "BOOL" | "BOOLEAN" => ColumnType::Bool,
            "DATE" => ColumnType::Date,
            "DATETIME" | "TIMESTAMP" => ColumnType::DateTime,
            "JSON" | "JSONB" => ColumnType::Json,
            "DECIMAL" | "NUMERIC" => ColumnType::Decimal,
            _ => {
                return Err(DbError::syntax(format!(
                    "不支持的列类型 \"{}\"，可用类型: INT, REAL, TEXT, BOOL, DATE, DATETIME, JSON, DECIMAL(p,s)",
                    raw
                )))
            }
        };
        Ok(ty)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ColumnType::Int => "INT",
            ColumnType::Real => "REAL",
            ColumnType::Text => "TEXT",
            ColumnType::Bool => "BOOL",
            ColumnType::Date => "DATE",
            ColumnType::DateTime => "DATETIME",
            ColumnType::Json => "JSON",
            ColumnType::Decimal => "DECIMAL",
        }
    }

    /// TEXT / DATE / DATETIME / JSON 都按「长度 + UTF-8」编码
    pub fn is_string_kind(&self) -> bool {
        matches!(
            self,
            ColumnType::Text | ColumnType::Date | ColumnType::DateTime | ColumnType::Json
        )
    }

    /// 从类型声明里提取 DECIMAL 标度：DECIMAL(10,2) → 2，DECIMAL → 默认 4
    pub fn decimal_scale_from_type(raw: &str) -> Result<u8> {
        let upper = raw.trim().to_uppercase();
        if !upper.starts_with("DECIMAL") {
            return Ok(DECIMAL_DEFAULT_SCALE);
        }
        let inner = match (upper.find('('), upper.rfind(')')) {
            (Some(a), Some(b)) if b > a => &upper[a + 1..b],
            _ => return Ok(DECIMAL_DEFAULT_SCALE),
        };
        let parts: Vec<&str> = inner.split(',').map(|s| s.trim()).collect();
        let scale = match parts.len() {
            2 => parts[1]
                .parse::<u8>()
                .map_err(|_| DbError::syntax(format!("DECIMAL 标度非法：{}", raw)))?,
            1 => return Ok(DECIMAL_DEFAULT_SCALE),
            _ => return Err(DbError::syntax(format!("DECIMAL 声明非法：{}", raw))),
        };
        if scale > DECIMAL_MAX_SCALE {
            return Err(DbError::syntax(format!(
                "DECIMAL 标度 {} 超过上限 {}",
                scale, DECIMAL_MAX_SCALE
            )));
        }
        Ok(scale)
    }
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
    /// DECIMAL 列的小数位数（其它类型忽略）
    pub scale: u8,
    pub not_null: bool,
    pub unique: bool,
    pub primary_key: bool,
    pub default: Option<Value>,
    /// 自增列（目前仅 INT）：插入时值为 NULL 就分配下一个整数
    pub auto_increment: bool,
}

impl Column {
    pub fn new(name: impl Into<String>, ty: ColumnType) -> Column {
        Column {
            name: name.into(),
            ty,
            scale: DECIMAL_DEFAULT_SCALE,
            not_null: false,
            unique: false,
            primary_key: false,
            default: None,
            auto_increment: false,
        }
    }

    pub fn lower_name(&self) -> String {
        self.name.to_lowercase()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    /// 定标十进制：v = 数值 × 10^scale（精确，无浮点误差）
    Decimal { v: i128, scale: u8 },
    Text(String),
    Bool(bool),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// 类型名，错误信息用
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Int(_) => "INT",
            Value::Real(_) => "REAL",
            Value::Decimal { .. } => "DECIMAL",
            Value::Text(_) => "TEXT",
            Value::Bool(_) => "BOOL",
        }
    }

    pub fn as_text(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Int(n) => n.to_string(),
            Value::Real(n) => format!("{}", n),
            Value::Decimal { v, scale } => format_decimal(*v, *scale),
            Value::Text(s) => s.clone(),
            Value::Bool(b) => {
                if *b {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Real(n) => Some(*n),
            Value::Decimal { v, scale } => {
                Some(*v as f64 / 10f64.powi(*scale as i32))
            }
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Text(s) => {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    t.parse::<f64>().ok()
                }
            }
            Value::Null => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Int(n) => Some(*n != 0),
            Value::Real(n) => Some(*n != 0.0),
            Value::Decimal { v, .. } => Some(*v != 0),
            Value::Text(s) => Some(!s.is_empty()),
            Value::Null => None,
        }
    }
}

// ------------------------------------------------------------------ 定标十进制

/// 把 i128 按标度格式化为十进制文本（去掉尾随 0）
pub fn format_decimal(v: i128, scale: u8) -> String {
    if scale == 0 {
        return v.to_string();
    }
    let negative = v < 0;
    let abs = v.unsigned_abs();
    let divisor = 10u128.pow(scale as u32);
    let int_part = abs / divisor;
    let frac_part = abs % divisor;
    let frac = format!("{:0width$}", frac_part, width = scale as usize);
    let sign = if negative { "-" } else { "" };
    format!("{}{}.{}", sign, int_part, frac)
}

/// 解析十进制文本到指定标度（四舍五入到标度位）
pub fn parse_decimal(text: &str, scale: u8) -> Result<i128> {
    let t = text.trim();
    if t.is_empty() {
        return Err(DbError::type_error("DECIMAL 不能为空"));
    }
    let (negative, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int_part, frac_part) = match digits.find('.') {
        Some(i) => (&digits[..i], &digits[i + 1..]),
        None => (digits, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(DbError::type_error(format!("无法把 \"{}\" 转换为 DECIMAL", text)));
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(DbError::type_error(format!("无法把 \"{}\" 转换为 DECIMAL", text)));
    }

    let mut v: i128 = if int_part.is_empty() {
        0
    } else {
        int_part
            .parse::<i128>()
            .map_err(|_| DbError::type_error(format!("DECIMAL 整数位过大：{}", text)))?
    };
    v = v
        .checked_mul(10i128.pow(scale as u32))
        .ok_or_else(|| DbError::type_error(format!("DECIMAL 整数位过大：{}", text)))?;

    // 小数位：标度内直接取，超出的第一位做四舍五入
    let frac_bytes = frac_part.as_bytes();
    for i in 0..scale as usize {
        let digit = if i < frac_bytes.len() {
            (frac_bytes[i] - b'0') as i128
        } else {
            0
        };
        v = v
            .checked_add(digit * 10i128.pow((scale as usize - i - 1) as u32))
            .ok_or_else(|| DbError::type_error(format!("DECIMAL 精度过高：{}", text)))?;
    }
    if frac_bytes.len() > scale as usize && frac_bytes[scale as usize] >= b'5' {
        v = v
            .checked_add(if negative { -1 } else { 1 })
            .ok_or_else(|| DbError::type_error(format!("DECIMAL 精度过高：{}", text)))?;
    }

    Ok(if negative { -v } else { v })
}

/// 对齐两个 Decimal 到同一标度（较大的那个）
fn align_decimals(
    a: &Value,
    b: &Value,
) -> std::result::Result<(i128, i128, u8), ()> {
    let (av, ascale) = match a {
        Value::Decimal { v, scale } => (*v, *scale),
        Value::Int(n) => (*n as i128, 0),
        _ => return Err(()),
    };
    let (bv, bscale) = match b {
        Value::Decimal { v, scale } => (*v, *scale),
        Value::Int(n) => (*n as i128, 0),
        _ => return Err(()),
    };
    let scale = ascale.max(bscale);
    let a_up = av
        .checked_mul(10i128.pow((scale - ascale) as u32))
        .ok_or(())?;
    let b_up = bv
        .checked_mul(10i128.pow((scale - bscale) as u32))
        .ok_or(())?;
    Ok((a_up, b_up, scale))
}

// ------------------------------------------------------------------ 编码

pub fn encode_row(columns: &[Column], values: &[Value]) -> Result<Vec<u8>> {
    if columns.len() != values.len() {
        return Err(DbError::internal(format!(
            "列数量({})与值数量({})不一致",
            columns.len(),
            values.len()
        )));
    }
    let count = columns.len();
    let bitmap = (count + 7) / 8;
    let mut out = vec![0u8; bitmap];

    for i in 0..count {
        if values[i].is_null() {
            out[i >> 3] |= 1 << (i & 7);
        }
    }

    for i in 0..count {
        match &values[i] {
            Value::Null => {}
            Value::Int(n) => out.extend_from_slice(&n.to_le_bytes()),
            Value::Real(n) => out.extend_from_slice(&n.to_le_bytes()),
            Value::Decimal { v, .. } => out.extend_from_slice(&v.to_le_bytes()),
            Value::Bool(b) => out.push(if *b { 1 } else { 0 }),
            Value::Text(s) => {
                let bytes = s.as_bytes();
                let len = u32::try_from(bytes.len())
                    .map_err(|_| DbError::new(crate::error::ROW_TOO_LARGE, "单列文本过长"))?;
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(bytes);
            }
        }
    }
    Ok(out)
}

pub fn decode_row(columns: &[Column], buf: &[u8]) -> Result<Vec<Value>> {
    let count = columns.len();
    let bitmap = (count + 7) / 8;
    if buf.len() < bitmap {
        return Err(DbError::internal("记录长度不足，无法读取 null 位图"));
    }
    let mut values = Vec::with_capacity(count);
    let mut off = bitmap;

    for i in 0..count {
        let is_null = (buf[i >> 3] >> (i & 7)) & 1 == 1;
        if is_null {
            values.push(Value::Null);
            continue;
        }
        let ty = columns[i].ty;
        if ty == ColumnType::Int {
            if off + 8 > buf.len() {
                return Err(DbError::internal("记录截断：INT"));
            }
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&buf[off..off + 8]);
            values.push(Value::Int(i64::from_le_bytes(raw)));
            off += 8;
        } else if ty == ColumnType::Real {
            if off + 8 > buf.len() {
                return Err(DbError::internal("记录截断：REAL"));
            }
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&buf[off..off + 8]);
            values.push(Value::Real(f64::from_le_bytes(raw)));
            off += 8;
        } else if ty == ColumnType::Bool {
            if off + 1 > buf.len() {
                return Err(DbError::internal("记录截断：BOOL"));
            }
            values.push(Value::Bool(buf[off] == 1));
            off += 1;
        } else if ty == ColumnType::Decimal {
            if off + 16 > buf.len() {
                return Err(DbError::internal("记录截断：DECIMAL"));
            }
            let mut raw = [0u8; 16];
            raw.copy_from_slice(&buf[off..off + 16]);
            values.push(Value::Decimal {
                v: i128::from_le_bytes(raw),
                scale: columns[i].scale,
            });
            off += 16;
        } else {
            if off + 4 > buf.len() {
                return Err(DbError::internal("记录截断：TEXT 长度"));
            }
            let mut raw = [0u8; 4];
            raw.copy_from_slice(&buf[off..off + 4]);
            let len = u32::from_le_bytes(raw) as usize;
            off += 4;
            if off + len > buf.len() {
                return Err(DbError::internal("记录截断：TEXT 内容"));
            }
            let text = String::from_utf8_lossy(&buf[off..off + len]).to_string();
            values.push(Value::Text(text));
            off += len;
        }
    }
    Ok(values)
}

// ------------------------------------------------------------------ 类型转换

pub fn coerce(ty: ColumnType, scale: u8, value: &Value) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    match ty {
        ColumnType::Int => match value {
            Value::Int(n) => Ok(Value::Int(*n)),
            Value::Bool(b) => Ok(Value::Int(if *b { 1 } else { 0 })),
            Value::Real(n) => {
                if n.fract() != 0.0 {
                    Err(DbError::type_error(format!("INT 列不接受小数 {}", n)))
                } else {
                    Ok(Value::Int(*n as i64))
                }
            }
            Value::Text(s) => {
                let t = s.trim();
                t.parse::<i64>()
                    .map(Value::Int)
                    .map_err(|_| DbError::type_error(format!("无法把 \"{}\" 转换为 INT", s)))
            }
            Value::Decimal { v, scale } => {
                // 仅整值 Decimal 可转 INT
                let divisor = 10i128.pow(*scale as u32);
                if v % divisor != 0 {
                    return Err(DbError::type_error(format!(
                        "DECIMAL {} 不是整数，无法转换为 INT",
                        format_decimal(*v, *scale)
                    )));
                }
                let q = v / divisor;
                i64::try_from(q)
                    .map(Value::Int)
                    .map_err(|_| DbError::type_error("DECIMAL 整数值超出 INT 范围"))
            }
            Value::Null => Ok(Value::Null),
        },
        ColumnType::Real => match value {
            Value::Real(n) => Ok(Value::Real(*n)),
            Value::Int(n) => Ok(Value::Real(*n as f64)),
            Value::Decimal { v, scale } => Ok(Value::Real(
                *v as f64 / 10f64.powi(*scale as i32),
            )),
            Value::Bool(b) => Ok(Value::Real(if *b { 1.0 } else { 0.0 })),
            Value::Text(s) => s
                .trim()
                .parse::<f64>()
                .map(Value::Real)
                .map_err(|_| DbError::type_error(format!("无法把 \"{}\" 转换为 REAL", s))),
            Value::Null => Ok(Value::Null),
        },
        ColumnType::Bool => match value {
            Value::Bool(b) => Ok(Value::Bool(*b)),
            Value::Int(n) => Ok(Value::Bool(*n != 0)),
            Value::Real(n) => Ok(Value::Bool(*n != 0.0)),
            Value::Decimal { v, .. } => Ok(Value::Bool(*v != 0)),
            Value::Text(s) => {
                let t = s.trim().to_lowercase();
                match t.as_str() {
                    "true" | "1" | "t" | "yes" => Ok(Value::Bool(true)),
                    "false" | "0" | "f" | "no" => Ok(Value::Bool(false)),
                    _ => Err(DbError::type_error(format!("无法把 \"{}\" 转换为 BOOL", s))),
                }
            }
            Value::Null => Ok(Value::Null),
        },
        ColumnType::Text => Ok(Value::Text(value.as_text())),
        ColumnType::Decimal => match value {
            Value::Decimal { v, scale: vs } => {
                // 标度对齐：升标度乘 10，降标度四舍五入
                if *vs == scale {
                    Ok(Value::Decimal { v: *v, scale: *vs })
                } else if *vs < scale {
                    let v = v
                        .checked_mul(10i128.pow((scale - *vs) as u32))
                        .ok_or_else(|| DbError::type_error("DECIMAL 精度溢出"))?;
                    Ok(Value::Decimal { v, scale })
                } else {
                    let divisor = 10i128.pow((*vs - scale) as u32);
                    let q = v / divisor;
                    let rem = (v % divisor).abs();
                    let round = if rem * 2 >= divisor { 1i128 } else { 0i128 };
                    let v = if *v < 0 { q - round } else { q + round };
                    Ok(Value::Decimal { v, scale })
                }
            }
            Value::Int(n) => {
                let v = (*n as i128)
                    .checked_mul(10i128.pow(scale as u32))
                    .ok_or_else(|| DbError::type_error("DECIMAL 整数位过大"))?;
                Ok(Value::Decimal { v, scale })
            }
            Value::Real(n) => {
                if !n.is_finite() {
                    return Err(DbError::type_error("DECIMAL 不接受无穷/NaN"));
                }
                let scaled = n * 10f64.powi(scale as i32);
                if scaled.abs() > 1.7e38 {
                    return Err(DbError::type_error("DECIMAL 数值过大"));
                }
                Ok(Value::Decimal {
                    v: scaled.round() as i128,
                    scale,
                })
            }
            Value::Text(s) => {
                let v = parse_decimal(s, scale)?;
                Ok(Value::Decimal { v, scale })
            }
            Value::Bool(b) => Ok(Value::Decimal {
                v: if *b { 10i128.pow(scale as u32) } else { 0 },
                scale,
            }),
            Value::Null => Ok(Value::Null),
        },
        ColumnType::Date => {
            let text = value.as_text();
            Ok(Value::Text(normalize_date(&text)?))
        }
        ColumnType::DateTime => {
            let text = value.as_text();
            Ok(Value::Text(normalize_datetime(&text)?))
        }
        ColumnType::Json => {
            let text = value.as_text();
            // 非法 JSON 是**用户输入错误**，不能报成 INTERNAL_ERROR：
            // 客户端会当成服务端故障去重试，实际应该修数据
            let parsed = crate::json::parse(&text).map_err(|err| {
                let preview: String = text.chars().take(40).collect();
                let suffix = if text.chars().count() > 40 { "…" } else { "" };
                DbError::type_error(format!(
                    "JSON 列只接受合法 JSON：{}{}（{}）",
                    preview, suffix, err.message
                ))
            })?;
            Ok(Value::Text(parsed.to_string()))
        }
    }
}

/// CAST 语义的显式转换：与 coerce（列赋值，严格）不同，
/// CAST 到 INT 时对 Real/Decimal 截断小数部分（标准 SQL 行为）
pub fn cast_value(ty: ColumnType, scale: u8, value: &Value) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    if ty == ColumnType::Int {
        return match value {
            Value::Int(n) => Ok(Value::Int(*n)),
            Value::Real(n) => Ok(Value::Int(n.trunc() as i64)),
            Value::Decimal { v, scale: vs } => {
                let divisor = 10i128.pow(*vs as u32);
                let q = v.div_euclid(divisor);
                i64::try_from(q)
                    .map(Value::Int)
                    .map_err(|_| DbError::type_error("DECIMAL 整数值超出 INT 范围"))
            }
            Value::Bool(b) => Ok(Value::Int(if *b { 1 } else { 0 })),
            Value::Text(s) => {
                let t = s.trim();
                // 截断小数部分（标准 CAST 语义）
                if let Ok(n) = t.parse::<i64>() {
                    return Ok(Value::Int(n));
                }
                match t.parse::<f64>() {
                    Ok(f) => Ok(Value::Int(f.trunc() as i64)),
                    Err(_) => Err(DbError::type_error(format!(
                        "无法把 \"{}\" 转换为 INT",
                        s
                    ))),
                }
            }
            Value::Null => Ok(Value::Null),
        };
    }
    coerce(ty, scale, value)
}

// ------------------------------------------------------------------ 日期

/// Howard Hinnant 的 civil_from_days 算法
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn split_date(text: &str) -> Option<(i64, u32, u32)> {
    let parts: Vec<&str> = text.split(|c| c == '-' || c == '/').collect();
    if parts.len() != 3 {
        return None;
    }
    let y = parts[0].parse::<i64>().ok()?;
    let m = parts[1].parse::<u32>().ok()?;
    let d = parts[2].parse::<u32>().ok()?;
    Some((y, m, d))
}

fn check_date(y: i64, m: u32, d: u32, original: &str) -> Result<()> {
    if y < 1 || y > 9999 || m < 1 || m > 12 || d < 1 || d > 31 {
        return Err(DbError::type_error(format!("日期不存在：{}", original)));
    }
    let days = days_from_civil(y, m, d);
    let (y2, m2, d2) = civil_from_days(days);
    if y2 != y || m2 != m || d2 != d {
        return Err(DbError::type_error(format!("日期不存在：{}", original)));
    }
    Ok(())
}

pub fn normalize_date(input: &str) -> Result<String> {
    let text = input.trim();
    let (y, m, d) = split_date(text).ok_or_else(|| {
        DbError::type_error(format!(
            "无法把 \"{}\" 转换为 DATE，请使用 YYYY-MM-DD",
            input
        ))
    })?;
    check_date(y, m, d, text)?;
    Ok(format!("{:04}-{:02}-{:02}", y, m, d))
}

pub fn normalize_datetime(input: &str) -> Result<String> {
    let text = input.trim();
    if text.is_empty() {
        return Err(DbError::type_error("DATETIME 不能为空"));
    }
    if text.ends_with('Z')
        || text.ends_with('z')
        || text.contains("+")
        || text.rfind('-').map(|i| i > 10).unwrap_or(false)
    {
        return Err(DbError::type_error(
            "Rust 版暂不支持带时区的 DATETIME 输入，请使用 UTC 的 YYYY-MM-DD HH:MM:SS",
        ));
    }

    let split_at = text.find(|c| c == ' ' || c == 'T');
    let (date_part, time_part) = match split_at {
        Some(i) => (&text[..i], &text[i + 1..]),
        None => (text, ""),
    };

    let (y, m, d) = split_date(date_part).ok_or_else(|| {
        DbError::type_error(format!(
            "无法把 \"{}\" 转换为 DATETIME，请使用 YYYY-MM-DD HH:MM:SS",
            input
        ))
    })?;
    check_date(y, m, d, text)?;

    let (mut hh, mut mm, mut ss) = (0u32, 0u32, 0u32);
    if !time_part.is_empty() {
        let base = match time_part.find('.') {
            Some(i) => &time_part[..i],
            None => time_part,
        };
        let parts: Vec<&str> = base.split(':').collect();
        if parts.len() < 2 || parts.len() > 3 {
            return Err(DbError::type_error(format!("时间格式错误：{}", time_part)));
        }
        hh = parts[0]
            .parse()
            .map_err(|_| DbError::type_error(format!("时间格式错误：{}", time_part)))?;
        mm = parts[1]
            .parse()
            .map_err(|_| DbError::type_error(format!("时间格式错误：{}", time_part)))?;
        if parts.len() == 3 {
            ss = parts[2]
                .parse()
                .map_err(|_| DbError::type_error(format!("时间格式错误：{}", time_part)))?;
        }
        if hh > 23 || mm > 59 || ss > 59 {
            return Err(DbError::type_error(format!("时间超出范围：{}", time_part)));
        }
    }

    Ok(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y, m, d, hh, mm, ss
    ))
}

pub fn format_date_from_days(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// 把 `YYYY-MM-DD[ HH:MM:SS]` 解析成 UTC 毫秒（会先按 DATETIME 规则校验）
pub fn parse_datetime_millis(input: &str) -> Result<i64> {
    // normalize_datetime 已经校验过各段范围，返回定宽格式，切片安全
    let text = normalize_datetime(input)?;
    let num = |from: usize, to: usize| -> Result<i64> {
        text.get(from..to)
            .and_then(|part| part.parse::<i64>().ok())
            .ok_or_else(|| DbError::type_error(format!("无法解析时间：{}", input)))
    };
    let year = num(0, 4)?;
    let month = num(5, 7)? as u32;
    let day = num(8, 10)? as u32;
    let hour = num(11, 13)?;
    let minute = num(14, 16)?;
    let second = num(17, 19)?;
    Ok(days_from_civil(year, month, day) * 86_400_000
        + hour * 3_600_000
        + minute * 60_000
        + second * 1000)
}

/// 毫秒 → 文本；`with_time=false` 时只给日期部分
pub fn format_millis(millis: i64, with_time: bool) -> String {
    let full = format_datetime_from_millis(millis);
    if with_time {
        full
    } else {
        full[..10].to_string()
    }
}

/// 某年某月有多少天（含闰年）
pub fn days_in_month(year: i64, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    (days_from_civil(next_year, next_month, 1) - days_from_civil(year, month, 1)) as u32
}

/// 按单位加减时间。
///
/// `month`/`year` 按日历推进，并对「目标月天数不足」做裁剪
/// （1-31 加一个月 = 2-28/29，而不是溢出到 3 月）。
pub fn add_datetime(millis: i64, amount: i64, unit: &str) -> Result<i64> {
    const DAY: i128 = 86_400_000;
    let unit = unit.trim().to_ascii_lowercase();
    let amount = amount as i128;
    let shifted: i128 = match unit.as_str() {
        "day" | "days" | "d" => millis as i128 + amount * DAY,
        "week" | "weeks" | "w" => millis as i128 + amount * 7 * DAY,
        "hour" | "hours" | "h" => millis as i128 + amount * 3_600_000,
        "minute" | "minutes" | "min" | "mi" => millis as i128 + amount * 60_000,
        "second" | "seconds" | "sec" | "s" => millis as i128 + amount * 1000,
        "month" | "months" | "mon" | "mo" => {
            return add_months(millis, amount.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
        }
        "year" | "years" | "y" => {
            let months = (amount * 12).clamp(i64::MIN as i128, i64::MAX as i128);
            return add_months(millis, months as i64);
        }
        other => {
            return Err(DbError::syntax(format!(
                "不支持的日期单位 \"{}\"：可用 day/week/hour/minute/second/month/year",
                other
            )))
        }
    };
    if shifted < i64::MIN as i128 || shifted > i64::MAX as i128 {
        return Err(DbError::type_error("日期加减结果超出可表示范围"));
    }
    Ok(shifted as i64)
}

fn add_months(millis: i64, months: i64) -> Result<i64> {
    const DAY: i64 = 86_400_000;
    let days = millis.div_euclid(DAY);
    let rest = millis.rem_euclid(DAY);
    let (year, month, day) = civil_from_days(days);

    let total = year as i128 * 12 + (month as i128 - 1) + months as i128;
    let new_year = total.div_euclid(12);
    let new_month = (total.rem_euclid(12) as u32) + 1;
    if !(1..=9999).contains(&new_year) {
        return Err(DbError::type_error(
            "日期加减结果超出可表示范围（公元 1..9999）",
        ));
    }
    let new_year = new_year as i64;
    let clamped_day = day.min(days_in_month(new_year, new_month));
    Ok(days_from_civil(new_year, new_month, clamped_day) * DAY + rest)
}

pub fn format_datetime_from_millis(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y, m, d, hh, mm, ss
    )
}

// ------------------------------------------------------------------ 比较与索引

pub fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y),
        (Value::Int(x), Value::Real(y)) => (*x as f64).partial_cmp(y),
        (Value::Real(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)),
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Int(y)) => Some((if *x { 1i64 } else { 0 }).cmp(y)),
        (Value::Int(x), Value::Bool(y)) => Some(x.cmp(&(if *y { 1i64 } else { 0 }))),
        // Decimal：与另一 Decimal 或 Int 对齐标度后精确比较
        (Value::Decimal { .. }, Value::Decimal { .. })
        | (Value::Decimal { .. }, Value::Int(_))
        | (Value::Int(_), Value::Decimal { .. }) => match align_decimals(a, b) {
            Ok((x, y, _)) => Some(x.cmp(&y)),
            Err(()) => None,
        },
        (Value::Decimal { .. }, Value::Real(_)) => {
            a.as_f64().and_then(|x| b.as_f64().and_then(|y| x.partial_cmp(&y)))
        }
        (Value::Real(_), Value::Decimal { .. }) => {
            a.as_f64().and_then(|x| b.as_f64().and_then(|y| x.partial_cmp(&y)))
        }
        _ => None,
    }
}

pub fn values_equal(a: &Value, b: &Value) -> Option<bool> {
    compare(a, b).map(|ord| ord == Ordering::Equal)
}

/// 索引 key；返回 None 表示 NULL，不参与索引
/// 索引键：**保序编码**——键的字典序 == 值的大小序，
/// 使 BTreeMap 索引支持范围扫描。同一逻辑值的编码必须稳定一致。
pub fn index_key(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        // 符号翻转 + 大端 hex：负数 < 正数全序成立
        Value::Int(n) => Some(format!(
            "i{:016x}",
            (*n as u64).wrapping_add(0x8000_0000_0000_0000)
        )),
        Value::Real(n) => {
            let bits = n.to_bits();
            let ordered = if bits >> 63 == 1 { !bits } else { bits | 0x8000_0000_0000_0000 };
            Some(format!("r{:016x}", ordered))
        }
        Value::Decimal { v, .. } => {
            // 同列同标度：符号翻转 + 大端 hex 保序
            let ordered = (*v as u128) ^ (1u128 << 127);
            Some(format!("d{:032x}", ordered))
        }
        Value::Bool(b) => Some(format!("b{}", if *b { 1 } else { 0 })),
        // UTF-8 字节序 == 码点序，天然保序
        Value::Text(s) => Some(format!("s{}", s)),
    }
}

pub fn index_key_of(values: &[Value]) -> Option<String> {
    let mut parts: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        match index_key(value) {
            Some(part) => parts.push(part),
            None => return None,
        }
    }
    Some(parts.join("\u{1}"))
}

#[cfg(test)]
mod date_math_tests {
    use super::*;

    fn ms(text: &str) -> i64 {
        parse_datetime_millis(text).unwrap()
    }

    fn add(text: &str, amount: i64, unit: &str) -> String {
        format_datetime_from_millis(add_datetime(ms(text), amount, unit).unwrap())
    }

    #[test]
    fn day_and_time_arithmetic() {
        assert_eq!(add("2026-01-01", 1, "day"), "2026-01-02 00:00:00");
        assert_eq!(add("2026-01-01", -1, "day"), "2025-12-31 00:00:00");
        assert_eq!(add("2026-01-01 10:00:00", 7, "hour"), "2026-01-01 17:00:00");
        assert_eq!(add("2026-01-01 23:59:30", 30, "second"), "2026-01-02 00:00:00");
        assert_eq!(add("2026-01-01", 2, "week"), "2026-01-15 00:00:00");
        assert_eq!(add("2026-01-01", 1, "d"), "2026-01-02 00:00:00");
    }

    #[test]
    fn month_arithmetic_clamps_to_month_end() {
        // 1-31 加一个月：2 月没有 31 号，应裁到月末而不是溢出到 3 月
        assert_eq!(add("2026-01-31", 1, "month"), "2026-02-28 00:00:00");
        assert_eq!(add("2024-01-31", 1, "month"), "2024-02-29 00:00:00", "闰年");
        assert_eq!(add("2026-03-31", -1, "month"), "2026-02-28 00:00:00");
        // 跨年
        assert_eq!(add("2026-12-15", 1, "month"), "2027-01-15 00:00:00");
        assert_eq!(add("2026-01-15", -1, "month"), "2025-12-15 00:00:00");
        // 年份
        assert_eq!(add("2024-02-29", 1, "year"), "2025-02-28 00:00:00", "闰日裁剪");
        // 时分秒要保持
        assert_eq!(add("2026-01-31 08:30:00", 1, "month"), "2026-02-28 08:30:00");
    }

    #[test]
    fn month_arithmetic_rejects_out_of_range() {
        assert!(add_datetime(ms("9999-12-31"), 1, "month").is_err());
        assert!(add_datetime(ms("0001-01-01"), -1, "month").is_err());
        assert!(add_datetime(ms("2026-01-01"), 1, "fortnight").is_err());
    }

    #[test]
    fn days_in_month_is_correct() {
        assert_eq!(days_in_month(2026, 1), 31);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 12), 31);
    }

    #[test]
    fn parse_rejects_invalid_date() {
        assert!(parse_datetime_millis("2026-02-30").is_err());
        assert!(parse_datetime_millis("not-a-date").is_err());
        // 只有日期时按 00:00:00 处理
        assert_eq!(ms("2026-01-01"), ms("2026-01-01 00:00:00"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_columns() -> Vec<Column> {
        vec![
            Column::new("id", ColumnType::Int),
            Column::new("name", ColumnType::Text),
            Column::new("score", ColumnType::Real),
            Column::new("ok", ColumnType::Bool),
        ]
    }

    #[test]
    fn row_roundtrip() {
        let columns = sample_columns();
        let values = vec![
            Value::Int(42),
            Value::Text("张三".to_string()),
            Value::Real(3.5),
            Value::Bool(true),
        ];
        let buf = encode_row(&columns, &values).unwrap();
        let decoded = decode_row(&columns, &buf).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn row_with_nulls() {
        let columns = sample_columns();
        let values = vec![Value::Int(1), Value::Null, Value::Null, Value::Bool(false)];
        let buf = encode_row(&columns, &values).unwrap();
        assert_eq!(decode_row(&columns, &buf).unwrap(), values);
    }

    #[test]
    fn date_normalization() {
        assert_eq!(normalize_date("2026-10-30").unwrap(), "2026-10-30");
        assert_eq!(normalize_date("2026/1/5").unwrap(), "2026-01-05");
        assert!(normalize_date("2026-02-30").is_err());
        assert!(normalize_date("abc").is_err());
    }

    #[test]
    fn datetime_normalization() {
        assert_eq!(
            normalize_datetime("2026-10-30 12:34:56").unwrap(),
            "2026-10-30 12:34:56"
        );
        assert_eq!(
            normalize_datetime("2026-10-30T12:34:56.789").unwrap(),
            "2026-10-30 12:34:56"
        );
        assert_eq!(
            normalize_datetime("2026-10-30").unwrap(),
            "2026-10-30 00:00:00"
        );
        assert!(normalize_datetime("2026-02-30 10:00:00").is_err());
    }

    #[test]
    fn civil_roundtrip() {
        for days in [-100000i64, -1, 0, 1, 19000, 20000, 100000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "days={} -> {}-{}-{}", days, y, m, d);
        }
    }

    #[test]
    fn coerce_rules() {
        assert_eq!(
            coerce(ColumnType::Int, 0, &Value::Text("42".to_string())).unwrap(),
            Value::Int(42)
        );
        assert!(coerce(ColumnType::Int, 0, &Value::Text("4.2".to_string())).is_err());
        assert_eq!(
            coerce(ColumnType::Bool, 0, &Value::Text("yes".to_string())).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            coerce(ColumnType::Json, 0, &Value::Text("{ \"a\": 1 }".to_string())).unwrap(),
            Value::Text("{\"a\":1}".to_string())
        );
    }

    #[test]
    fn index_keys() {
        // 保序编码：键的字典序 == 值的大小序（范围扫描依赖此性质）
        let i5 = index_key(&Value::Int(5)).unwrap();
        let i10 = index_key(&Value::Int(10)).unwrap();
        let ineg = index_key(&Value::Int(-1)).unwrap();
        assert!(ineg < i5, "负数键应小于正数键");
        assert!(i5 < i10, "5 应小于 10");
        assert_eq!(
            index_key(&Value::Text("x".to_string())).unwrap(),
            "sx"
        );
        assert!(index_key(&Value::Null).is_none());
    }
}

#[cfg(test)]
mod decimal_tests {
    use super::*;

    #[test]
    fn parse_and_format_roundtrip() {
        // 标度固定：DECIMAL(p,s) 值总是显示 s 位小数（金额格式化）
        for (text, scale) in [("123.45", 2u8), ("0.1000", 4), ("-99.999", 3), ("42", 0), ("0.0001", 4)] {
            let v = parse_decimal(text, scale).unwrap();
            assert_eq!(format_decimal(v, scale), text, "roundtrip {}", text);
        }
        // 四舍五入到标度位（half-up）
        assert_eq!(parse_decimal("1.005", 2).unwrap(), 101); // 1.01
        assert_eq!(parse_decimal("1.004", 2).unwrap(), 100); // 1.00
        assert_eq!(format_decimal(parse_decimal("1.006", 2).unwrap(), 2), "1.01");
    }

    #[test]
    fn decimal_compare_exact() {
        let a = Value::Decimal { v: 10, scale: 1 }; // 1.0
        let b = Value::Decimal { v: 99, scale: 2 }; // 0.99
        assert_eq!(compare(&a, &b), Some(Ordering::Greater));
        let c = Value::Decimal { v: 10, scale: 1 }; // 1.0
        assert_eq!(compare(&a, &c), Some(Ordering::Equal));
        // 跨类型：Decimal vs Int
        assert_eq!(compare(&Value::Int(1), &a), Some(Ordering::Equal));
    }

    #[test]
    fn decimal_index_key_ordered() {
        // 同列同标度下保序；不同标度各自编码（delta 键只在同列内比较）
        let a = index_key(&Value::Decimal { v: 12345, scale: 4 }).unwrap();
        let b = index_key(&Value::Decimal { v: 12346, scale: 4 }).unwrap();
        assert!(a < b, "键应保序");
        let neg = index_key(&Value::Decimal { v: -1, scale: 4 }).unwrap();
        let zero = index_key(&Value::Decimal { v: 0, scale: 4 }).unwrap();
        assert!(neg < zero, "负数键应小于零键");
    }

    #[test]
    fn garbage_rejected() {
        assert!(parse_decimal("abc", 4).is_err());
        assert!(parse_decimal("1.2.3", 4).is_err());
        assert!(parse_decimal("", 4).is_err());
    }
}
