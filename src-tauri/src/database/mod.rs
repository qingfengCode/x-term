//! 业务数据库（MySQL）连接管理。
//!
//! 注意：与 [`crate::storage::db`]（SQLite，存配置）区分。本模块管理对用户
//! MySQL 服务的运行时连接，供 SQL 控制台和 AI 工具使用。
pub mod mysql;
pub mod profiles;

pub use mysql::{MySqlConn, QueryResult};
