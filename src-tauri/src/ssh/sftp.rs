//! SFTP 子系统封装。
//!
//! [`SftpSession`] 持有一个 [`russh_sftp::client::SftpSession`]，对外提供
//! 目录列举、文件元信息、上传、下载、重命名、删除等高层操作。所有方法
//! 把 russh-sftp 的错误统一映射到 [`crate::error::AppError`]。
//!
//! 上传/下载采用分块循环 + 进度回调的形式，调用方（传输命令层）可在回调里
//! emit [`crate::events::TRANSFER_PROGRESS`] 等事件。

use std::path::Path;

use chrono::{DateTime, Utc};
use russh::client::Handle;
use russh_sftp::client::SftpSession as RawSftpSession;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{AppError, AppResult};
use crate::ssh::client::ClientHandler;

/// SFTP 客户端侧的文件元信息类型别名（russh-sftp 把它定义为
/// `protocol::FileAttributes` 的别名）。
type SftpMetadata = russh_sftp::client::fs::Metadata;

/// 默认的读写块大小（64 KiB），兼顾吞吐与进度刷新频率。
const CHUNK_SIZE: usize = 64 * 1024;

// ===========================================================================
// 数据模型
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

// ===========================================================================
// SftpSession 包装
// ===========================================================================

/// 对 [`russh_sftp::client::SftpSession`] 的薄封装。
pub struct SftpSession {
    /// 底层 russh-sftp 会话。
    pub channel: RawSftpSession,
}

/// 在已有 SSH 连接上打开 SFTP 子系统。
///
/// 步骤：`channel_open_session` → `request_subsystem("sftp")` →
/// `SftpSession::new(channel.into_stream())`。
pub async fn open_sftp(handle: &Handle<ClientHandler>) -> AppResult<SftpSession> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| AppError::Ssh(format!("打开 SFTP channel 失败: {}", e)))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| AppError::Ssh(format!("请求 sftp 子系统失败: {}", e)))?;
    let sftp = RawSftpSession::new(channel.into_stream())
        .await
        .map_err(|e| AppError::Ssh(format!("初始化 SFTP 会话失败: {}", e)))?;
    Ok(SftpSession { channel: sftp })
}

impl SftpSession {
    /// 列举目录下的所有条目（自动跳过 `.` 和 `..`，由 russh-sftp 处理）。
    pub async fn list_dir(&self, path: &str) -> AppResult<Vec<FileEntry>> {
        let mut out = Vec::new();
        // read_dir 返回 ReadDir 迭代器；逐项转换。
        let iter = self
            .channel
            .read_dir(path)
            .await
            .map_err(|e| AppError::Ssh(format!("read_dir `{}` 失败: {}", path, e)))?;
        for entry in iter {
            let name = entry.file_name();
            let meta = entry.metadata();
            out.push(FileEntry {
                name,
                is_dir: meta.is_dir(),
                size: meta.len(),
                modified: format_modified(&meta),
            });
        }
        Ok(out)
    }

    /// 获取单个路径的元信息。
    pub async fn stat(&self, path: &str) -> AppResult<FileMeta> {
        let meta = self
            .channel
            .metadata(path)
            .await
            .map_err(|e| AppError::Ssh(format!("stat `{}` 失败: {}", path, e)))?;
        Ok(FileMeta {
            size: meta.len(),
            is_dir: meta.is_dir(),
            modified: format_modified(&meta),
        })
    }

    /// 下载远程文件到本地路径，逐块写盘并回调进度。
    ///
    /// `progress(transferred, total)`：`total` 来自远程文件元信息；若获取失败
    /// 则传 0，调用方可据此判断能否显示百分比。
    pub async fn download<F>(
        &self,
        remote: &str,
        local_path: &Path,
        progress: F,
    ) -> AppResult<()>
    where
        F: Fn(u64, u64),
    {
        // 取文件大小用于进度百分比（失败则置 0）。
        let total = self
            .channel
            .metadata(remote)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        let mut remote_file = self
            .channel
            .open(remote)
            .await
            .map_err(|e| AppError::Ssh(format!("打开远程文件 `{}` 失败: {}", remote, e)))?;

        let mut local_file = tokio::fs::File::create(local_path).await?;

        let mut transferred: u64 = 0;
        let mut buf = vec![0u8; CHUNK_SIZE];
        loop {
            let n = remote_file
                .read(&mut buf)
                .await
                .map_err(|e| AppError::Ssh(format!("读取远程文件失败: {}", e)))?;
            if n == 0 {
                break;
            }
            local_file
                .write_all(&buf[..n])
                .await
                .map_err(|e| AppError::Ssh(format!("写入本地文件失败: {}", e)))?;
            transferred += n as u64;
            progress(transferred, total);
        }
        local_file.flush().await?;
        Ok(())
    }

    /// 上传本地文件到远程路径，逐块上传并回调进度。
    ///
    /// `progress(transferred, total)`：`total` 来自本地文件元信息。
    pub async fn upload<F>(
        &self,
        local_path: &Path,
        remote: &str,
        progress: F,
    ) -> AppResult<()>
    where
        F: Fn(u64, u64),
    {
        let local_meta = tokio::fs::metadata(local_path).await?;
        let total = local_meta.len();

        let mut local_file = tokio::fs::File::open(local_path).await?;
        let mut remote_file = self
            .channel
            .create(remote)
            .await
            .map_err(|e| AppError::Ssh(format!("创建远程文件 `{}` 失败: {}", remote, e)))?;

        let mut transferred: u64 = 0;
        let mut buf = vec![0u8; CHUNK_SIZE];
        loop {
            let n = local_file
                .read(&mut buf)
                .await
                .map_err(|e| AppError::Ssh(format!("读取本地文件失败: {}", e)))?;
            if n == 0 {
                break;
            }
            remote_file
                .write_all(&buf[..n])
                .await
                .map_err(|e| AppError::Ssh(format!("写入远程文件失败: {}", e)))?;
            transferred += n as u64;
            progress(transferred, total);
        }
        remote_file
            .flush()
            .await
            .map_err(|e| AppError::Ssh(format!("flush 远程文件失败: {}", e)))?;
        // 优雅关闭文件句柄，确保服务端落盘。
        let _ = remote_file.shutdown().await;
        Ok(())
    }

    /// 重命名远程文件或目录。
    pub async fn rename(&self, oldpath: &str, newpath: &str) -> AppResult<()> {
        self.channel
            .rename(oldpath, newpath)
            .await
            .map_err(|e| AppError::Ssh(format!("rename `{}` -> `{}` 失败: {}", oldpath, newpath, e)))
    }

    /// 创建远程目录。
    pub async fn mkdir(&self, path: &str) -> AppResult<()> {
        self.channel
            .create_dir(path)
            .await
            .map_err(|e| AppError::Ssh(format!("mkdir `{}` 失败: {}", path, e)))
    }

    /// 删除远程文件。
    pub async fn remove_file(&self, path: &str) -> AppResult<()> {
        self.channel
            .remove_file(path)
            .await
            .map_err(|e| AppError::Ssh(format!("remove_file `{}` 失败: {}", path, e)))
    }

    /// 删除远程空目录。
    pub async fn remove_dir(&self, path: &str) -> AppResult<()> {
        self.channel
            .remove_dir(path)
            .await
            .map_err(|e| AppError::Ssh(format!("remove_dir `{}` 失败: {}", path, e)))
    }
}

/// 把 SFTP 元信息的 `modified()`（`SystemTime`）格式化为 RFC3339 字符串；
/// 失败返回 `None`。
fn format_modified(meta: &SftpMetadata) -> Option<String> {
    use std::time::SystemTime;
    let modified: SystemTime = meta.modified().ok()?;
    let dt: DateTime<Utc> = DateTime::<Utc>::from(modified);
    Some(dt.to_rfc3339())
}
