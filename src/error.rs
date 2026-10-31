//! 统一错误类型。错误码经协议层直接透传给客户端。

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbError {
    pub code: &'static str,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, DbError>;

pub const SYNTAX: &str = "SYNTAX_ERROR";
pub const TYPE: &str = "TYPE_ERROR";
pub const NOT_FOUND: &str = "NOT_FOUND";
pub const ALREADY_EXISTS: &str = "ALREADY_EXISTS";
pub const CONSTRAINT: &str = "CONSTRAINT_VIOLATION";
pub const LOCKED: &str = "DATABASE_LOCKED";
pub const WRITE_CONFLICT: &str = "WRITE_CONFLICT";
pub const LIMIT: &str = "LIMIT_EXCEEDED";
pub const AUTH: &str = "AUTH_FAILED";
pub const INTERNAL: &str = "INTERNAL_ERROR";
pub const ROW_TOO_LARGE: &str = "ROW_TOO_LARGE";
pub const PROTOCOL: &str = "PROTOCOL_ERROR";

impl DbError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        DbError {
            code,
            message: message.into(),
        }
    }

    pub fn syntax(msg: impl Into<String>) -> Self {
        Self::new(SYNTAX, msg)
    }

    pub fn type_error(msg: impl Into<String>) -> Self {
        Self::new(TYPE, msg)
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(NOT_FOUND, msg)
    }

    pub fn constraint(msg: impl Into<String>) -> Self {
        Self::new(CONSTRAINT, msg)
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(INTERNAL, msg)
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for DbError {}

impl From<std::io::Error> for DbError {
    fn from(err: std::io::Error) -> Self {
        DbError::new(INTERNAL, format!("I/O 错误: {}", err))
    }
}
