//! 行记录编解码与类型系统，二进制格式与 Node 版一致。
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
}

impl ColumnType {
    pub fn parse(raw: &str) -> Result<ColumnType> {
        let upper = raw.trim().to_uppercase();
        let ty = match upper.as_str() {
            "INT" | "INTEGER" | "BIGINT" | "SMALLINT" => ColumnType::Int,
            "REAL" | "FLOAT" | "DOUBLE" | "DECIMAL" | "NUMERIC" => ColumnType::Real,
            "TEXT" | "VARCHAR" | "CHAR" | "STRING" => ColumnType::Text,
            "BOOL" | "BOOLEAN" => ColumnType::Bool,
            "DATE" => ColumnType::Date,
            "DATETIME" | "TIMESTAMP" => ColumnType::DateTime,
            "JSON" | "JSONB" => ColumnType::Json,
            _ => {
                return Err(DbError::syntax(format!(
                    "不支持的列类型 \"{}\"，可用类型: INT, REAL, TEXT, BOOL, DATE, DATETIME, JSON",
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
        }
    }

    /// TEXT / DATE / DATETIME / JSON 都按「长度 + UTF-8」编码
    pub fn is_string_kind(&self) -> bool {
        matches!(
            self,
            ColumnType::Text | ColumnType::Date | ColumnType::DateTime | ColumnType::Json
        )
    }
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
    pub not_null: bool,
    pub unique: bool,
    pub primary_key: bool,
    pub default: Option<Value>,
}

impl Column {
    pub fn new(name: impl Into<String>, ty: ColumnType) -> Column {
        Column {
            name: name.into(),
            ty,
            not_null: false,
            unique: false,
            primary_key: false,
            default: None,
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
    Text(String),
    Bool(bool),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_text(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Int(n) => n.to_string(),
            Value::Real(n) => format!("{}", n),
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
            Value::Text(s) => Some(!s.is_empty()),
            Value::Null => None,
        }
    }
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

pub fn coerce(ty: ColumnType, value: &Value) -> Result<Value> {
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
            Value::Null => Ok(Value::Null),
        },
        ColumnType::Real => match value {
            Value::Real(n) => Ok(Value::Real(*n)),
            Value::Int(n) => Ok(Value::Real(*n as f64)),
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
            let parsed = crate::json::parse(&text)?;
            Ok(Value::Text(parsed.to_string()))
        }
    }
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
        _ => None,
    }
}

pub fn values_equal(a: &Value, b: &Value) -> Option<bool> {
    compare(a, b).map(|ord| ord == Ordering::Equal)
}

/// 索引 key；返回 None 表示 NULL，不参与索引
pub fn index_key(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Int(n) => Some(format!("n{}", n)),
        Value::Real(n) => Some(format!("n{}", n)),
        Value::Bool(b) => Some(format!("b{}", if *b { 1 } else { 0 })),
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
            coerce(ColumnType::Int, &Value::Text("42".to_string())).unwrap(),
            Value::Int(42)
        );
        assert!(coerce(ColumnType::Int, &Value::Text("4.2".to_string())).is_err());
        assert_eq!(
            coerce(ColumnType::Bool, &Value::Text("yes".to_string())).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            coerce(ColumnType::Json, &Value::Text("{ \"a\": 1 }".to_string())).unwrap(),
            Value::Text("{\"a\":1}".to_string())
        );
    }

    #[test]
    fn index_keys() {
        assert_eq!(index_key(&Value::Int(5)).unwrap(), "n5");
        assert_eq!(
            index_key(&Value::Text("x".to_string())).unwrap(),
            "sx"
        );
        assert!(index_key(&Value::Null).is_none());
    }
}
