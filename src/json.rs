//! 迷你 JSON 实现（零依赖）。
//!
//! 只服务于两件事：读写 catalog.json，以及 WAL 的 JSON Lines。
//! 结构很简单，不需要通用高性能实现。

use std::fmt::Write as _;

use crate::error::{DbError, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj() -> Json {
        Json::Obj(Vec::new())
    }

    pub fn arr() -> Json {
        Json::Arr(Vec::new())
    }

    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }

    pub fn num(n: impl Into<f64>) -> Json {
        Json::Num(n.into())
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Num(n) => Some(*n as i64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Vec<(String, Json)>> {
        match self {
            Json::Obj(items) => Some(items),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn to_string(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self);
        out
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_number(out: &mut String, n: f64) {
    if !n.is_finite() {
        out.push_str("null");
        return;
    }
    if n.fract() == 0.0 && n.abs() < 9007199254740992.0 {
        let _ = write!(out, "{}", n as i64);
    } else {
        let _ = write!(out, "{}", n);
    }
}

fn write_value(out: &mut String, value: &Json) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Num(n) => write_number(out, *n),
        Json::Str(s) => write_string(out, s),
        Json::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Json::Obj(items) => {
            out.push('{');
            for (i, (key, item)) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

/// JSON 嵌套深度上限。真实数据远用不到这么深；
/// 没有限制时深嵌套报文会让递归解析（以及 Json 树的递归 Drop）栈溢出。
pub const MAX_JSON_DEPTH: usize = 200;

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Parser {
            bytes: text.as_bytes(),
            pos: 0,
            depth: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, ch: u8) -> Result<()> {
        if self.peek() == Some(ch) {
            self.pos += 1;
            Ok(())
        } else {
            Err(DbError::internal(format!(
                "JSON 解析失败：期望 '{}'，位置 {}",
                ch as char, self.pos
            )))
        }
    }

    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_JSON_DEPTH {
            return Err(DbError::internal(format!(
                "JSON 嵌套超过 {} 层上限",
                MAX_JSON_DEPTH
            )));
        }
        Ok(())
    }

    fn parse_value(&mut self) -> Result<Json> {
        self.skip_ws();
        match self.peek() {
            Some(b'n') => {
                self.literal("null")?;
                Ok(Json::Null)
            }
            Some(b't') => {
                self.literal("true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Json::Bool(false))
            }
            Some(b'"') => Ok(Json::Str(self.parse_string()?)),
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_object(),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            _ => Err(DbError::internal(format!("JSON 解析失败：位置 {}", self.pos))),
        }
    }

    fn literal(&mut self, word: &str) -> Result<()> {
        let end = self.pos + word.len();
        if end <= self.bytes.len() && &self.bytes[self.pos..end] == word.as_bytes() {
            self.pos = end;
            Ok(())
        } else {
            Err(DbError::internal(format!(
                "JSON 解析失败：期望 {}，位置 {}",
                word, self.pos
            )))
        }
    }

    fn parse_array(&mut self) -> Result<Json> {
        self.enter()?;
        let outcome = self.parse_array_inner();
        self.depth -= 1;
        outcome
    }

    fn parse_array_inner(&mut self) -> Result<Json> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(DbError::internal("JSON 解析失败：数组未闭合")),
            }
        }
    }

    fn parse_object(&mut self) -> Result<Json> {
        self.enter()?;
        let outcome = self.parse_object_inner();
        self.depth -= 1;
        outcome
    }

    fn parse_object_inner(&mut self) -> Result<Json> {
        self.expect(b'{')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(items));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.parse_value()?;
            items.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(items));
                }
                _ => return Err(DbError::internal("JSON 解析失败：对象未闭合")),
            }
        }
    }

    fn parse_string(&mut self) -> Result<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let c = match self.peek() {
                Some(c) => c,
                None => return Err(DbError::internal("JSON 解析失败：字符串未闭合")),
            };
            self.pos += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = self.peek().ok_or_else(|| DbError::internal("JSON 转义不完整"))?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let code = self.parse_hex4()?;
                            if (0xD800..0xDC00).contains(&code) {
                                // 高位代理，尝试合并后面的低位代理
                                if self.peek() == Some(b'\\')
                                    && self.bytes.get(self.pos + 1) == Some(&b'u')
                                {
                                    self.pos += 2;
                                    let low = self.parse_hex4()?;
                                    if (0xDC00..0xE000).contains(&low) {
                                        let combined =
                                            0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                                        if let Some(ch) = char::from_u32(combined) {
                                            out.push(ch);
                                            continue;
                                        }
                                    }
                                }
                                out.push('\u{fffd}');
                            } else if let Some(ch) = char::from_u32(code) {
                                out.push(ch);
                            }
                        }
                        other => {
                            return Err(DbError::internal(format!(
                                "JSON 解析失败：非法转义 \\{}",
                                other as char
                            )))
                        }
                    }
                }
                _ => {
                    // 非 ASCII 的 UTF-8 多字节：按原始字节重新组装
                    let start = self.pos - 1;
                    let extra = utf8_extra_bytes(c);
                    if extra > 0 {
                        self.pos += extra;
                    }
                    let end = self.pos.min(self.bytes.len());
                    let slice = &self.bytes[start..end];
                    match std::str::from_utf8(slice) {
                        Ok(s) => out.push_str(s),
                        Err(_) => out.push('\u{fffd}'),
                    }
                }
            }
        }
    }

    fn parse_hex4(&mut self) -> Result<u32> {
        if self.pos + 4 > self.bytes.len() {
            return Err(DbError::internal("JSON \\u 转义不完整"));
        }
        let mut value = 0u32;
        for _ in 0..4 {
            let c = self.bytes[self.pos];
            self.pos += 1;
            let digit = match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' => (c - b'a') as u32 + 10,
                b'A'..=b'F' => (c - b'A') as u32 + 10,
                _ => return Err(DbError::internal("JSON \\u 转义非法")),
            };
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<Json> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || c == b'+' || c == b'-' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| DbError::internal("JSON 数字非法"))?;
        let value: f64 = text
            .parse()
            .map_err(|_| DbError::internal(format!("JSON 数字非法: {}", text)))?;
        Ok(Json::Num(value))
    }
}

fn utf8_extra_bytes(first: u8) -> usize {
    if first < 0x80 {
        0
    } else if first >> 5 == 0b110 {
        1
    } else if first >> 4 == 0b1110 {
        2
    } else if first >> 3 == 0b11110 {
        3
    } else {
        0
    }
}

pub fn parse(text: &str) -> Result<Json> {
    let mut parser = Parser::new(text);
    let value = parser.parse_value()?;
    parser.skip_ws();
    Ok(value)
}

// ------------------------------------------------------------------ base64

const B64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | data[i + 2] as u32;
        out.push(B64_TABLE[(n >> 18) as usize & 63] as char);
        out.push(B64_TABLE[(n >> 12) as usize & 63] as char);
        out.push(B64_TABLE[(n >> 6) as usize & 63] as char);
        out.push(B64_TABLE[n as usize & 63] as char);
        i += 3;
    }
    let rest = data.len() - i;
    if rest == 1 {
        let n = (data[i] as u32) << 16;
        out.push(B64_TABLE[(n >> 18) as usize & 63] as char);
        out.push(B64_TABLE[(n >> 12) as usize & 63] as char);
        out.push('=');
        out.push('=');
    } else if rest == 2 {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        out.push(B64_TABLE[(n >> 18) as usize & 63] as char);
        out.push(B64_TABLE[(n >> 12) as usize & 63] as char);
        out.push(B64_TABLE[(n >> 6) as usize & 63] as char);
        out.push('=');
    }
    out
}

pub fn b64_decode(text: &str) -> Result<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for byte in text.bytes() {
        if byte == b'=' || byte == b'\n' || byte == b'\r' {
            continue;
        }
        let value = match byte {
            b'A'..=b'Z' => (byte - b'A') as u32,
            b'a'..=b'z' => (byte - b'a') as u32 + 26,
            b'0'..=b'9' => (byte - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(DbError::internal("base64 包含非法字符")),
        };
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_object() {
        let root = Json::Obj(vec![
            ("version".to_string(), Json::Num(1.0)),
            ("name".to_string(), Json::str("用户")),
            ("flag".to_string(), Json::Bool(true)),
            ("none".to_string(), Json::Null),
        ]);
        let text = root.to_string();
        assert!(text.contains("\"version\":1"));
        let parsed = parse(&text).unwrap();
        assert_eq!(parsed.get("name").unwrap().as_str(), Some("用户"));
        assert_eq!(parsed.get("flag").unwrap().as_bool(), Some(true));
        assert!(parsed.get("none").unwrap().is_null());
    }

    #[test]
    fn parse_nested() {
        let text = r#"{"a":[1,2,3],"b":{"c":"x\n"}}"#;
        let value = parse(text).unwrap();
        let arr = value.get("a").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[2].as_i64(), Some(3));
        assert_eq!(value.get("b").unwrap().get("c").unwrap().as_str(), Some("x\n"));
    }

    #[test]
    fn parse_unicode_escape() {
        let value = parse(r#""\u4e2d\u6587""#).unwrap();
        assert_eq!(value.as_str(), Some("中文"));
    }

    #[test]
    fn base64_roundtrip() {
        let data: Vec<u8> = (0..=255u8).collect();
        let encoded = b64_encode(&data);
        let decoded = b64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);

        assert_eq!(b64_encode(b"abc"), "YWJj");
        assert_eq!(b64_encode(b"abcd"), "YWJjZA==");
        assert_eq!(b64_decode("YWJj").unwrap(), b"abc".to_vec());
    }

    #[test]
    fn deep_nesting_is_rejected_not_crash() {
        // 曾经：深嵌套会让递归解析爆栈、整个进程 abort
        for depth in [MAX_JSON_DEPTH + 1, 100_000] {
            let text = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
            assert!(parse(&text).is_err(), "深度 {} 应被拒绝", depth);
        }
        // 上限内的正常嵌套不受影响
        let ok = format!("{}{}", "[".repeat(MAX_JSON_DEPTH), "]".repeat(MAX_JSON_DEPTH));
        assert!(parse(&ok).is_ok());
    }
}
