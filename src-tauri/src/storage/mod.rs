//! 持久化存储层。
//!
//! 本模块组织以下子模块：
//! - [`db`]: SQLite 连接池与数据库迁移。
//! - [`sessions_repo`]: 会话（Session）和分组（Group）的 CRUD。
//! - `history_repo`: 命令历史的记录与检索。
//! - [`json_store`]: 应用数据目录管理及小型 JSON 配置文件的原子读写。
//! - [`secure`]: 基于 AES-256-GCM + Argon2id 的凭据保险库。
//! - [`known_hosts`]: SSH 主机公钥指纹（known_hosts）持久化。

pub mod db;
pub mod desktops_repo;
pub mod file_accounts_repo;
pub mod history_repo;
pub mod json_store;
pub mod known_hosts;
pub mod secure;
pub mod sessions_repo;
