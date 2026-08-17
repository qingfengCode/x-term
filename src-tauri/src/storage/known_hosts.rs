//! SSH known_hosts 持久化。
//!
//! 以 JSON 文件 `<data_dir>/known_hosts.json` 存储每台主机的公钥指纹，
//! 用于 [`crate::ssh::client::ClientHandler::check_server_key`] 做 TOFU
//! （Trust On First Use）校验。
//!
//! 存储结构：`HashMap<"host:port", KnownHostEntry>`，复用
//! [`crate::storage::json_store`] 的原子读写。known_hosts 是公开数据（公钥
//! 指纹本身不算敏感凭据），无需加密。
//!
//! # 安全语义
//!
//! - 文件**不存在**视为首次使用，返回空表；
//! - 文件**存在但解析失败**（损坏）返回 [`AppError`]，调用方必须拒绝连接而非
//!   按"首次连接"静默放行——否则等于把 TOFU 静默降级，且下一次保存会把整个
//!   文件覆盖成单条记录，丢失全部已知主机指纹。
//!
//! # 并发
//!
//! 所有 load / save 经进程内 [`lock`] 串行化。[`crate::ssh::client::ClientHandler::check_server_key`]
//! 采用**两阶段加锁**：第一阶段持锁完成「读 → 比较」与首次记录写入；指纹冲突时
//! **先释放锁**再等待用户决策（一次弹窗最长 120s，若全程持锁会阻塞所有并发连接的
//! known_hosts 读写），拿到 `AcceptAndUpdate` 后重新持锁、二次比对记录未被并发
//! 修改，再写入更新。用 tokio Mutex：可在异步上下文中 await 获取锁；锁内只做
//! 同步 load/save，不跨 await 持有。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::storage::json_store;

/// known_hosts 文件名。
const KNOWN_HOSTS_FILENAME: &str = "known_hosts.json";

/// 单条 known_hosts 记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownHostEntry {
    /// 公钥算法名（如 `"ssh-ed25519"` / `"rsa-sha2-256"`）。
    pub key_type: String,
    /// 公钥指纹（SHA-256 base64，由 [`russh::keys::key::PublicKey::fingerprint`] 给出）。
    pub fingerprint: String,
}

/// 已知主机表：`"host:port"` -> [`KnownHostEntry`]。
pub type KnownHosts = HashMap<String, KnownHostEntry>;

/// 进程内 known_hosts 读写互斥（tokio Mutex：异步上下文中可 await 获取；
/// 锁内仅同步 load/save，不跨 .await 持有）。
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 获取 known_hosts 全局锁。
///
/// 锁内仅执行同步的 load/save（**不跨 .await 持有**），串行化所有读写，
/// 防止两个并发连接 load 到同一旧快照后各自保存、互相覆盖（丢记录）。
/// 等待用户确认主机公钥变更的 .await 必须在锁外进行，见模块级「并发」说明。
pub async fn lock() -> tokio::sync::MutexGuard<'static, ()> {
    LOCK.lock().await
}

/// 返回 known_hosts.json 的完整路径。
pub fn known_hosts_path(data_dir: &Path) -> PathBuf {
    data_dir.join(KNOWN_HOSTS_FILENAME)
}

/// 从磁盘加载 known_hosts（应持有 [`lock`]）。
///
/// - 文件不存在：返回空表（首次使用）。
/// - 文件存在但损坏/解析失败：返回错误——**不是**空表。调用方对校验失败
///   必须拒绝连接，防止 TOFU 被静默降级为"首次连接"。
pub fn load(data_dir: &Path) -> AppResult<KnownHosts> {
    let path = known_hosts_path(data_dir);
    if !path.exists() {
        return Ok(KnownHosts::new());
    }
    let content = std::fs::read_to_string(&path)?;
    serde_json::from_str(&content).map_err(|e| {
        AppError::InvalidInput(format!(
            "known_hosts 文件损坏（{}），请删除该文件后重试: {}",
            path.display(),
            e
        ))
    })
}

/// 把 known_hosts 原子写入磁盘（应持有 [`lock`]）。
pub fn save(data_dir: &Path, hosts: &KnownHosts) -> AppResult<()> {
    let path = known_hosts_path(data_dir);
    json_store::write_json(&path, hosts)
}

/// 生成查找表用的键。
pub fn host_key(host: &str, port: u16) -> String {
    format!("{}:{}", host, port)
}
