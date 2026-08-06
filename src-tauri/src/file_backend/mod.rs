//! 文件后端抽象。
//!
//! 把"远程文件系统"这一概念从 SFTP 协议中解耦出来，定义统一的
//! [`FileBackend`] trait。当前实现：
//! - SFTP（[`crate::ssh::sftp::SftpSession`]）
//! - S3 及兼容存储（[`s3::S3Backend`]）
//!
//! 前端 `sftp_*` 命令、传输进度事件、`FileEntry`/`FileMeta` 数据结构保持协议无关，
//! 新协议只需实现本 trait 即可被现有 UI 与命令层复用。

pub mod s3;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use crate::error::AppResult;

// ===========================================================================
// 数据模型（serde 友好，直接序列化给前端）
// ===========================================================================

/// 目录项的精简信息（serde 友好，直接序列化给前端）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<String>,
}

/// 文件元信息。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMeta {
    pub size: u64,
    pub is_dir: bool,
    pub modified: Option<String>,
}

/// 进度回调类型：`Arc<dyn Fn(transferred, total) + Send + Sync>`。
///
/// trait 对象方法不能使用泛型参数，故用 boxed 闭包。`Arc` 包装便于在 spawn 的
/// 异步任务之间克隆传递。`total` 未知时传 0，调用方可据此判断能否显示百分比。
pub type ProgressCb = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// 文件后端种类。
///
/// 用于运行时 map 区分不同后端实例（虽然 trait 对象本身已自描述，但前端 / 持久化
/// 需要一个稳定的字符串标识）。序列化为小写 "sftp" / "s3"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Sftp,
    S3,
}

impl BackendKind {
    /// 解析字符串为 BackendKind；非法输入回退到 Sftp。
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "s3" => BackendKind::S3,
            _ => BackendKind::Sftp,
        }
    }

    /// 中文展示名。
    pub fn label(self) -> &'static str {
        match self {
            BackendKind::Sftp => "SFTP",
            BackendKind::S3 => "S3",
        }
    }
}

// ===========================================================================
// FileBackend trait
// ===========================================================================

/// 远程文件后端的统一抽象。
///
/// 所有方法异步，返回 [`AppResult`]。实现方负责把协议原生错误映射到
/// [`crate::error::AppError`] 的合适变体（SFTP 用 `Ssh`，S3 用 `Storage`）。
///
/// 方法语义参照 SFTP：路径用 `/` 分隔（S3 key 同样以 `/` 作为逻辑前缀分隔），
/// `mkdir` 在无目录概念的存储（如 S3）上可以是空操作或写入占位对象。
#[async_trait]
pub trait FileBackend: Send + Sync {
    /// 列举目录下的所有条目（实现方自行决定是否跳过 `.` / `..`）。
    async fn list_dir(&self, path: &str) -> AppResult<Vec<FileEntry>>;

    /// 获取单个路径的元信息。
    async fn stat(&self, path: &str) -> AppResult<FileMeta>;

    /// 下载远程文件到本地路径，逐块写盘并通过 `progress` 回调进度。
    async fn download(
        &self,
        remote: &str,
        local_path: &Path,
        progress: ProgressCb,
    ) -> AppResult<()>;

    /// 上传本地文件到远程路径，逐块上传并通过 `progress` 回调进度。
    async fn upload(&self, local_path: &Path, remote: &str, progress: ProgressCb) -> AppResult<()>;

    /// 重命名远程文件或目录。
    async fn rename(&self, oldpath: &str, newpath: &str) -> AppResult<()>;

    /// 创建远程目录。
    async fn mkdir(&self, path: &str) -> AppResult<()>;

    /// 删除远程文件。
    async fn remove_file(&self, path: &str) -> AppResult<()>;

    /// 删除远程空目录（或递归删除，由实现决定）。
    async fn remove_dir(&self, path: &str) -> AppResult<()>;
}
