//! SFTP 文件操作命令。
//!
//! 所有命令都通过 `sftpId`（由 [`crate::commands::session::open_sftp_for_session`]
//! 返回）定位一个已打开的 SFTP 会话。
//!
//! 下载/上传是异步长任务，进度通过 [`crate::events::TRANSFER_PROGRESS`] 事件
//! 实时推送到前端；完成或失败时分别 emit [`crate::events::TRANSFER_DONE`] /
//! [`crate::events::TRANSFER_ERROR`]。

use std::path::PathBuf;

use serde::Deserialize;
use tauri::{AppHandle, State};

use crate::error::{AppError, AppResult};
use crate::events::{
    self, TransferDoneEvent, TransferErrorEvent, TransferProgressEvent, TRANSFER_DONE,
    TRANSFER_ERROR, TRANSFER_PROGRESS,
};
use crate::ssh::sftp::{FileEntry, FileMeta};
use crate::state::AppState;

/// 列出远程目录内容。
#[tauri::command]
pub async fn sftp_list(
    sftp_id: String,
    path: String,
    state: State<'_, AppState>,
) -> AppResult<Vec<FileEntry>> {
    let sftp = state
        .sftp_sessions
        .lock()
        .get(&sftp_id)
        .map(|(s, _)| s.clone());
    let sftp = sftp.ok_or_else(|| AppError::NotFound(format!("SFTP 会话 {} 不存在", sftp_id)))?;
    sftp.list_dir(&path).await
}

/// 获取远程文件元信息。
#[tauri::command]
pub async fn sftp_stat(
    sftp_id: String,
    path: String,
    state: State<'_, AppState>,
) -> AppResult<FileMeta> {
    let sftp = state
        .sftp_sessions
        .lock()
        .get(&sftp_id)
        .map(|(s, _)| s.clone());
    let sftp = sftp.ok_or_else(|| AppError::NotFound(format!("SFTP 会话 {} 不存在", sftp_id)))?;
    sftp.stat(&path).await
}

/// 创建远程目录。
#[tauri::command]
pub async fn sftp_mkdir(
    sftp_id: String,
    path: String,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let sftp = state
        .sftp_sessions
        .lock()
        .get(&sftp_id)
        .map(|(s, _)| s.clone());
    let sftp = sftp.ok_or_else(|| AppError::NotFound(format!("SFTP 会话 {} 不存在", sftp_id)))?;
    sftp.mkdir(&path).await
}

/// 重命名远程文件/目录。
#[tauri::command]
pub async fn sftp_rename(
    sftp_id: String,
    oldpath: String,
    newpath: String,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let sftp = state
        .sftp_sessions
        .lock()
        .get(&sftp_id)
        .map(|(s, _)| s.clone());
    let sftp = sftp.ok_or_else(|| AppError::NotFound(format!("SFTP 会话 {} 不存在", sftp_id)))?;
    sftp.rename(&oldpath, &newpath).await
}

/// 删除远程文件或空目录。
#[tauri::command]
pub async fn sftp_remove(
    sftp_id: String,
    path: String,
    is_dir: bool,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let sftp = state
        .sftp_sessions
        .lock()
        .get(&sftp_id)
        .map(|(s, _)| s.clone());
    let sftp = sftp.ok_or_else(|| AppError::NotFound(format!("SFTP 会话 {} 不存在", sftp_id)))?;
    if is_dir {
        sftp.remove_dir(&path).await
    } else {
        sftp.remove_file(&path).await
    }
}

/// 下载任务参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadParams {
    pub sftp_id: String,
    pub remote_path: String,
    pub local_path: String,
    /// 前端分配的任务 id，用于匹配进度事件。
    pub task_id: String,
}

/// 下载远程文件到本地（异步任务，进度通过事件推送）。
#[tauri::command]
pub async fn sftp_download(
    params: DownloadParams,
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<()> {
    let sftp = state
        .sftp_sessions
        .lock()
        .get(&params.sftp_id)
        .map(|(s, _)| s.clone());
    let sftp =
        sftp.ok_or_else(|| AppError::NotFound(format!("SFTP 会话 {} 不存在", params.sftp_id)))?;
    let local = PathBuf::from(&params.local_path);
    let task_id = params.task_id.clone();
    let app_for_progress = app.clone();

    let result = sftp
        .download(
            &params.remote_path,
            &local,
            std::sync::Arc::new(move |transferred, total| {
                events::emit(
                    &app_for_progress,
                    TRANSFER_PROGRESS,
                    TransferProgressEvent {
                        task_id: task_id.clone(),
                        transferred,
                        total,
                        speed: 0,
                    },
                );
            }) as crate::file_backend::ProgressCb,
        )
        .await;

    match result {
        Ok(()) => {
            events::emit(
                &app,
                TRANSFER_DONE,
                TransferDoneEvent {
                    task_id: params.task_id,
                    transferred: 0,
                    total: 0,
                },
            );
            Ok(())
        }
        Err(e) => {
            events::emit(
                &app,
                TRANSFER_ERROR,
                TransferErrorEvent {
                    task_id: params.task_id,
                    message: e.to_string(),
                },
            );
            Err(e)
        }
    }
}

/// 上传任务参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadParams {
    pub sftp_id: String,
    pub local_path: String,
    pub remote_path: String,
    pub task_id: String,
}

/// 上传本地文件到远程（异步任务，进度通过事件推送）。
#[tauri::command]
pub async fn sftp_upload(
    params: UploadParams,
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<()> {
    let sftp = state
        .sftp_sessions
        .lock()
        .get(&params.sftp_id)
        .map(|(s, _)| s.clone());
    let sftp =
        sftp.ok_or_else(|| AppError::NotFound(format!("SFTP 会话 {} 不存在", params.sftp_id)))?;
    let local = PathBuf::from(&params.local_path);
    let task_id = params.task_id.clone();
    let app_for_progress = app.clone();

    let result = sftp
        .upload(
            &local,
            &params.remote_path,
            std::sync::Arc::new(move |transferred, total| {
                events::emit(
                    &app_for_progress,
                    TRANSFER_PROGRESS,
                    TransferProgressEvent {
                        task_id: task_id.clone(),
                        transferred,
                        total,
                        speed: 0,
                    },
                );
            }) as crate::file_backend::ProgressCb,
        )
        .await;

    match result {
        Ok(()) => {
            events::emit(
                &app,
                TRANSFER_DONE,
                TransferDoneEvent {
                    task_id: params.task_id,
                    transferred: 0,
                    total: 0,
                },
            );
            Ok(())
        }
        Err(e) => {
            events::emit(
                &app,
                TRANSFER_ERROR,
                TransferErrorEvent {
                    task_id: params.task_id,
                    message: e.to_string(),
                },
            );
            Err(e)
        }
    }
}

/// 关闭 SFTP 会话（断开底层连接）。
#[tauri::command]
pub async fn sftp_close(sftp_id: String, state: State<'_, AppState>) -> AppResult<()> {
    let (_, handle) = state
        .sftp_sessions
        .lock()
        .remove(&sftp_id)
        .ok_or_else(|| AppError::NotFound(format!("SFTP 会话 {} 不存在", sftp_id)))?;
    use russh::Disconnect;
    let _ = handle
        .disconnect(Disconnect::ByApplication, "bye", "en")
        .await;
    Ok(())
}
