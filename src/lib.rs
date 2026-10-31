//! LittleDB 的 Rust 实现。
//!
//! 设计目标：与 Node 版共享同一套磁盘格式（页布局、行编码、catalog.json、WAL），
//! 因此两套实现可以打开同一个数据目录、互为备份验证。

pub mod catalog;
pub mod codec;
pub mod engine;
pub mod error;
pub mod http;
pub mod json;
pub mod page;
pub mod protocol;
pub mod server;
pub mod sql;
pub mod store;
pub mod table;
pub mod wal;

pub use codec::{Column, ColumnType, Value};
pub use engine::Engine;
pub use error::{DbError, Result};
pub use sql::Statement;
