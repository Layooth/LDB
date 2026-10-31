//! JSON 路径取值（`$.a.b[0]`），给 `JSON_EXTRACT` / `JSON_TYPE` 等函数用。
//!
//! 路径语法（刻意做小，够用又不会有歧义）：
//! - `$` 根；`$.key` 取对象字段；`$.list[2]` 取数组元素；可以混写 `$.a.b[0].c`
//! - 键名区分大小写；不存在的路径返回 `None`（SQL 层翻成 NULL），不是错误
//! - 语法错误（不以 `$` 开头、`[` 没闭合、索引不是数字）报 `SYNTAX_ERROR`

use crate::codec::Value;
use crate::error::{DbError, Result};
use crate::json::{self, Json};

#[derive(Debug, Clone, PartialEq)]
enum Step {
    Key(String),
    Index(usize),
}

fn parse_path(path: &str) -> Result<Vec<Step>> {
    let text = path.trim();
    if !text.starts_with('$') {
        return Err(DbError::syntax(format!(
            "JSON 路径必须以 $ 开头，实际是 \"{}\"（例如 $.user.name 或 $.items[0]）",
            path
        )));
    }
    let bytes: Vec<char> = text.chars().collect();
    let mut steps = Vec::new();
    let mut i = 1usize;
    while i < bytes.len() {
        match bytes[i] {
            '.' => {
                i += 1;
                let start = i;
                // `]` 也必须终止键名：否则 `$.list]` 会把 "]"
                // 悄悄当成键的一部分，而不是报语法错误
                while i < bytes.len() && !matches!(bytes[i], '.' | '[' | ']') {
                    i += 1;
                }
                if start == i {
                    return Err(DbError::syntax(format!(
                        "JSON 路径里 \".\" 后面缺少键名：{}",
                        path
                    )));
                }
                steps.push(Step::Key(bytes[start..i].iter().collect()));
            }
            '[' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != ']' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err(DbError::syntax(format!(
                        "JSON 路径里的 \"[\" 没有闭合：{}",
                        path
                    )));
                }
                let raw: String = bytes[start..i].iter().collect();
                let index = raw.trim().parse::<usize>().map_err(|_| {
                    DbError::syntax(format!(
                        "JSON 路径的数组下标必须是非负整数，实际是 \"{}\"（路径 {}）",
                        raw, path
                    ))
                })?;
                steps.push(Step::Index(index));
                i += 1; // 跳过 ']'
            }
            other => {
                return Err(DbError::syntax(format!(
                    "JSON 路径在第 {} 个字符处无法解析（\"{}\"）：只支持 .键 与 [下标]，完整路径 {}",
                    i + 1,
                    other,
                    path
                )))
            }
        }
    }
    Ok(steps)
}

/// 解析 JSON 文本（错误信息带上提示，方便排查）
pub fn parse_document(text: &str) -> Result<Json> {
    json::parse(text).map_err(|err| {
        DbError::type_error(format!(
            "不是合法 JSON：{}（可先用 JSON_VALID 检查）",
            err.message
        ))
    })
}

/// 按路径取值；路径不存在返回 `Ok(None)`（SQL 层是 NULL），路径语法错才报错
pub fn extract<'a>(root: &'a Json, path: &str) -> Result<Option<&'a Json>> {
    let steps = parse_path(path)?;
    let mut current = root;
    for step in steps {
        current = match step {
            Step::Key(key) => match current.get(&key) {
                Some(next) => next,
                None => return Ok(None),
            },
            Step::Index(index) => match current.as_array().and_then(|items| items.get(index)) {
                Some(next) => next,
                None => return Ok(None),
            },
        };
    }
    Ok(Some(current))
}

/// JSON 类型名（`JSON_TYPE` 的返回值）
pub fn type_name(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Int(_) | Json::Num(_) => "number",
        Json::Str(_) => "string",
        Json::Arr(_) => "array",
        Json::Obj(_) => "object",
    }
}

/// JSON 节点 → SQL 值：标量原样给出，对象/数组给 JSON 文本（列类型是 TEXT/JSON）
pub fn to_value(value: &Json) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        // 精确整数保持 INT，避免落到 f64 丢精度
        Json::Int(n) => Value::Int(*n),
        Json::Num(n) => Value::Real(*n),
        Json::Str(s) => Value::Text(s.clone()),
        Json::Arr(_) | Json::Obj(_) => Value::Text(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Json {
        parse_document(text).unwrap()
    }

    fn get<'a>(root: &'a Json, path: &str) -> Option<&'a Json> {
        extract(root, path).unwrap()
    }

    #[test]
    fn reads_nested_fields_and_indexes() {
        let root = doc(r#"{"user":{"name":"alice","tags":["a","b"],"meta":{"ok":true}}}"#);
        assert_eq!(
            get(&root, "$.user.name").and_then(|v| v.as_str()),
            Some("alice")
        );
        assert_eq!(
            get(&root, "$.user.tags[1]").and_then(|v| v.as_str()),
            Some("b")
        );
        assert_eq!(get(&root, "$.user.meta.ok").and_then(|v| v.as_bool()), Some(true));
        // 根自身
        assert!(get(&root, "$").is_some());
        // 不存在的路径是 None，不是错误
        assert!(get(&root, "$.user.missing").is_none());
        assert!(get(&root, "$.user.tags[9]").is_none());
        assert!(get(&root, "$.user.name.deeper").is_none(), "标量上继续取值应返回 None");
    }

    #[test]
    fn array_root_and_mixed_path() {
        let root = doc(r#"[{"id":1},{"id":2}]"#);
        assert_eq!(get(&root, "$[1].id").map(to_value), Some(Value::Int(2)));
        let root = doc(r#"{"a":[{"b":[10,20]}]}"#);
        assert_eq!(get(&root, "$.a[0].b[1]").map(to_value), Some(Value::Int(20)));
    }

    #[test]
    fn bad_paths_are_syntax_errors() {
        for path in ["user.name", "$.user.", "$.list[", "$.list[x]", "$.list]"] {
            assert!(parse_path(path).is_err(), "应报错：{}", path);
        }
        assert!(parse_path("$.a.b[0]").is_ok());
        assert!(parse_path("$").is_ok());
    }

    #[test]
    fn type_names_and_value_mapping() {
        let root = doc(r#"{"s":"x","i":9007199254740993,"f":1.5,"b":false,"n":null,"a":[1],"o":{}}"#);
        assert_eq!(type_name(get(&root, "$.s").unwrap()), "string");
        assert_eq!(type_name(get(&root, "$.i").unwrap()), "number");
        assert_eq!(type_name(get(&root, "$.b").unwrap()), "boolean");
        assert_eq!(type_name(get(&root, "$.n").unwrap()), "null");
        assert_eq!(type_name(get(&root, "$.a").unwrap()), "array");
        assert_eq!(type_name(get(&root, "$.o").unwrap()), "object");

        // 2^53 以上的整数必须保持 INT（不能被 f64 吃掉精度）
        assert_eq!(
            get(&root, "$.i").map(to_value),
            Some(Value::Int(9007199254740993))
        );
        assert_eq!(get(&root, "$.f").map(to_value), Some(Value::Real(1.5)));
        assert_eq!(get(&root, "$.a").map(to_value), Some(Value::Text("[1]".into())));
    }
}
