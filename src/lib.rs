//! LittleDB 的 Rust 实现（独立项目，磁盘格式以本实现为准：
//! slotted page、行编码、catalog.json、LDBW2 二进制 WAL）。

pub mod auth;
pub mod catalog;
pub mod config;
pub mod dump;
pub mod log;
pub mod codec;
pub mod engine;
pub mod error;
pub mod http;
pub mod json;
pub mod jsonpath;
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
