//! SQL 层：词法、语法、求值与执行。

pub mod ast;
pub mod eval;
pub mod executor;
pub mod lexer;
pub mod parser;

pub use ast::Statement;
pub use executor::{QueryResult, Session};
pub use parser::Parser;

/// 聚合函数名（大写）
pub const AGGREGATES: &[&str] = &["COUNT", "SUM", "AVG", "MIN", "MAX"];
