//! SQL 词法分析。

use crate::error::{DbError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    Ident,
    Number,
    Str,
    Punct,
    Param,
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    /// 关键字统一大写；标识符与字符串保留原样
    pub text: String,
    pub pos: usize,
}

impl Token {
    pub fn is_keyword(&self, word: &str) -> bool {
        self.kind == TokenKind::Keyword && self.text == word
    }

    pub fn is_punct(&self, ch: &str) -> bool {
        self.kind == TokenKind::Punct && self.text == ch
    }
}

const KEYWORDS: &[&str] = &[
    "SELECT", "DISTINCT", "ALL", "FROM", "WHERE", "GROUP", "BY", "HAVING", "ORDER", "ASC", "DESC",
    "LIMIT", "OFFSET", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE", "TABLE",
    "DROP", "IF", "NOT", "EXISTS", "PRIMARY", "KEY", "UNIQUE", "NULL", "DEFAULT", "AND", "OR",
    "TRUE", "FALSE", "LIKE", "IN", "BETWEEN", "IS", "AS", "INDEX", "ON", "BEGIN", "COMMIT",
    "ROLLBACK", "TRANSACTION", "SHOW", "TABLES", "DESCRIBE", "EXPLAIN", "JOIN", "LEFT", "RIGHT",
    "INNER", "OUTER", "CROSS", "FULL", "UNION",
];

fn is_keyword(word: &str) -> bool {
    KEYWORDS.iter().any(|k| *k == word)
}

/// 标识符：ASCII 字母 + 下划线 + CJK 统一表意文字（U+4E00–U+9FA5）
fn is_cjk(ch: char) -> bool {
    let code = ch as u32;
    (0x4e00..=0x9fa5).contains(&code)
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_' || is_cjk(ch)
}

fn is_ident_part(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || is_cjk(ch)
}

pub fn tokenize(sql: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens: Vec<Token> = Vec::new();
    let mut i = 0usize;

    while i < chars.len() {
        let ch = chars[i];

        // 空白
        if ch.is_whitespace() {
            i += 1;
            continue;
        }

        // 行注释
        if ch == '-' && i + 1 < chars.len() && chars[i + 1] == '-' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        // 块注释
        if ch == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            let mut closed = false;
            while i + 1 < chars.len() {
                if chars[i] == '*' && chars[i + 1] == '/' {
                    i += 2;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return Err(DbError::syntax("块注释未闭合"));
            }
            continue;
        }

        // 参数占位符
        if ch == '?' {
            tokens.push(Token {
                kind: TokenKind::Param,
                text: "?".to_string(),
                pos: i,
            });
            i += 1;
            continue;
        }

        // 字符串字面量（单引号，'' 表示一个单引号）
        if ch == '\'' {
            let start = i;
            i += 1;
            let mut text = String::new();
            let mut closed = false;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if i + 1 < chars.len() && chars[i + 1] == '\'' {
                        text.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    closed = true;
                    break;
                }
                text.push(chars[i]);
                i += 1;
            }
            if !closed {
                return Err(DbError::syntax("字符串未闭合"));
            }
            tokens.push(Token {
                kind: TokenKind::Str,
                text,
                pos: start,
            });
            continue;
        }

        // 双引号标识符
        if ch == '"' {
            let start = i;
            i += 1;
            let mut text = String::new();
            let mut closed = false;
            while i < chars.len() {
                if chars[i] == '"' {
                    if i + 1 < chars.len() && chars[i + 1] == '"' {
                        text.push('"');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    closed = true;
                    break;
                }
                text.push(chars[i]);
                i += 1;
            }
            if !closed {
                return Err(DbError::syntax("标识符未闭合"));
            }
            tokens.push(Token {
                kind: TokenKind::Ident,
                text,
                pos: start,
            });
            continue;
        }

        // 数字
        if ch.is_ascii_digit() || (ch == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())
        {
            let start = i;
            let mut seen_dot = false;
            let mut seen_exp = false;
            while i < chars.len() {
                let c = chars[i];
                if c.is_ascii_digit() {
                    i += 1;
                } else if c == '.' && !seen_dot && !seen_exp {
                    seen_dot = true;
                    i += 1;
                } else if (c == 'e' || c == 'E') && !seen_exp && i + 1 < chars.len() {
                    seen_exp = true;
                    i += 1;
                    if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
                        i += 1;
                    }
                } else {
                    break;
                }
            }
            let text: String = chars[start..i].iter().collect();
            tokens.push(Token {
                kind: TokenKind::Number,
                text,
                pos: start,
            });
            continue;
        }

        // 标识符 / 关键字
        if is_ident_start(ch) {
            let start = i;
            i += 1;
            while i < chars.len() && is_ident_part(chars[i]) {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            let upper = text.to_uppercase();
            let kind = if is_keyword(&upper) {
                TokenKind::Keyword
            } else {
                TokenKind::Ident
            };
            tokens.push(Token {
                kind,
                text: if kind == TokenKind::Keyword { upper } else { text },
                pos: start,
            });
            continue;
        }

        // 多字符符号
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        if two == "!=" || two == "<>" || two == "<=" || two == ">=" {
            tokens.push(Token {
                kind: TokenKind::Punct,
                text: two,
                pos: i,
            });
            i += 2;
            continue;
        }

        // 单字符符号
        if "+-*/%=<>(),.;".contains(ch) {
            tokens.push(Token {
                kind: TokenKind::Punct,
                text: ch.to_string(),
                pos: i,
            });
            i += 1;
            continue;
        }

        return Err(DbError::syntax(format!("无法识别的字符 '{}'", ch)));
    }

    tokens.push(Token {
        kind: TokenKind::Eof,
        text: String::new(),
        pos: chars.len(),
    });
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str) -> Vec<TokenKind> {
        tokenize(sql).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn basic_tokens() {
        let tokens = tokenize("SELECT id, name FROM users").unwrap();
        assert!(tokens[0].is_keyword("SELECT"));
        assert_eq!(tokens[1].kind, TokenKind::Ident);
        assert_eq!(tokens[1].text, "id");
        assert!(tokens[2].is_punct(","));
        assert!(tokens[4].is_keyword("FROM"));
        assert_eq!(tokens[5].text, "users");
    }

    #[test]
    fn string_escape() {
        let tokens = tokenize("SELECT 'it''s ok'").unwrap();
        assert_eq!(tokens[1].kind, TokenKind::Str);
        assert_eq!(tokens[1].text, "it's ok");
    }

    #[test]
    fn quoted_identifier() {
        let tokens = tokenize(r#"SELECT "user name" FROM "t""#).unwrap();
        assert_eq!(tokens[1].kind, TokenKind::Ident);
        assert_eq!(tokens[1].text, "user name");
    }

    #[test]
    fn comments_are_skipped() {
        let tokens = tokenize("SELECT 1 -- 注释\n/* 块注释 */ FROM t").unwrap();
        assert!(tokens[0].is_keyword("SELECT"));
        assert!(tokens[2].is_keyword("FROM"));
    }

    #[test]
    fn numbers() {
        let tokens = tokenize("1 2.5 3e2 0.1").unwrap();
        assert_eq!(tokens[0].text, "1");
        assert_eq!(tokens[1].text, "2.5");
        assert_eq!(tokens[2].text, "3e2");
        assert_eq!(tokens[3].text, "0.1");
    }

    #[test]
    fn comparison_operators() {
        let tokens = tokenize("a != b <> c <= d >= e").unwrap();
        assert!(tokens[1].is_punct("!="));
        assert!(tokens[3].is_punct("<>"));
        assert!(tokens[5].is_punct("<="));
        assert!(tokens[7].is_punct(">="));
    }

    #[test]
    fn unterminated_string_is_error() {
        assert!(tokenize("SELECT 'abc").is_err());
        assert!(tokenize("SELECT /* abc").is_err());
    }

    #[test]
    fn eof_is_last() {
        let k = kinds("SELECT 1");
        assert_eq!(k.last(), Some(&TokenKind::Eof));
    }

    #[test]
    fn chinese_identifiers_are_supported() {
        let tokens = tokenize("SELECT 名字 FROM 玩家 WHERE 等级 > 10").unwrap();
        assert_eq!(tokens[1].kind, TokenKind::Ident);
        assert_eq!(tokens[1].text, "名字");
        assert_eq!(tokens[3].text, "玩家");
        assert_eq!(tokens[5].text, "等级");
    }
}
