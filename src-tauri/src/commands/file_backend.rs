//! 文件后端（S3 / 兼容存储）命令层。
//!
//! 持久化在 `file_accounts` 表；运行时连接（[`FileBackend`] trait 对象）保存在
//! [`AppState::file_backends`]，按 `backendId`（uuid）引用。凭据复用 `credentials`
//! 表（kind=`"s3_credential"`），由 [`file_accounts_repo::fetch_s3_credential`] 解密。
//!
//! 命令分三类：
//! - 账号 CRUD：`file_account_list / save / delete`
//! - 连接生命周期：`file_connect / file_disconnect`
//! - 文件操作：`file_list / file_stat / file_mkdir / file_rename / file_remove /
//!   file_download / file_upload`（下载/上传带进度事件，复用 transfer:* 事件）
//!
//! 范式参照 `commands/forward.rs` 与 `commands/sftp.rs`。

use std::path::PathBuf;
use std::sync::Arc;

use tauri::{AppHandle, State};

use crate::error::{AppError, AppResult};
use crate::events::{
    self, TransferDoneEvent, TransferErrorEvent, TransferProgressEvent, TRANSFER_DONE,
    TRANSFER_ERROR, TRANSFER_PROGRESS,
};
use crate::file_backend::s3::{S3Backend, S3Config};
use crate::file_backend::{FileBackend, FileEntry, FileMeta};
use crate::state::AppState;
use crate::storage::file_accounts_repo::{
    delete_file_account, get_file_account, list_file_accounts, upsert_file_account, FileAccount,
};

// ---------------------------------------------------------------------------
// 账号 CRUD
// ---------------------------------------------------------------------------

/// 列出所有文件账号。
#[tauri::command]
pub fn file_account_list(state: State<'_, AppState>) -> AppResult<Vec<FileAccount>> {
    let conn = state.conn()?;
    list_file_accounts(&conn)
}

/// 新增或更新文件账号（UPSERT）。
#[tauri::command]
pub fn file_account_save(account: FileAccount, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    upsert_file_account(&conn, &account)
}

/// 删除文件账号；同步断开该账号的运行时连接。
#[tauri::command]
pub fn file_account_delete(id: String, state: State<'_, AppState>) -> AppResult<()> {
    {
        let conn = state.conn()?;
        delete_file_account(&conn, &id)?;
    }
    // 按 account_id 精确清理（S3 连接无状态，drop 即释放）。
    state.file_backends.lock().remove(&id);
    Ok(())
}

// ---------------------------------------------------------------------------
// 连接生命周期
// ---------------------------------------------------------------------------

/// 连接到一个文件账号，返回 `backendId`（前端后续文件操作用它引用）。
///
/// 幂等：同一 account_id 重复 connect 返回同一 backendId（S3 等后端无状态可安全复用），
/// 避免连接泄漏。当前仅支持 S3。
#[tauri::command]
pub async fn file_connect(account_id: String, state: State<'_, AppState>) -> AppResult<String> {
    // 幂等：已存在连接直接返回。
    if let Some((backend_id, _)) = state.file_backends.lock().get(&account_id).cloned() {
        return Ok(backend_id);
    }

    let account = {
        let conn = state.conn()?;
        get_file_account(&conn, &account_id)?
            .ok_or_else(|| AppError::NotFound(format!("文件账号 {} 不存在", account_id)))?
    };

    // 按 kind 分派（当前仅 S3）。
    let backend: Arc<dyn FileBackend> = match account.kind.as_str() {
        "s3" => {
            // 解密凭据。
            let cred_id = account.credential_id.as_ref().ok_or_else(|| {
                AppError::Auth(format!("文件账号 `{}` 缺少 credential_id", account.name))
            })?;
            let (access_key, secret_key) = {
                let vault = {
                    let guard = state.vault_read()?;
                    guard
                        .as_ref()
                        .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
                        .clone()
                };
                let conn = state.conn()?;
                crate::storage::file_accounts_repo::fetch_s3_credential(&conn, cred_id, &vault)?
            };
            let config = S3Config {
                endpoint: account.endpoint.clone(),
                region: account.region.clone(),
                bucket: account.bucket.clone(),
                access_key,
                secret_key,
                path_style: account.path_style,
            };
            Arc::new(S3Backend::new(config)?)
        }
        other => {
            return Err(AppError::InvalidInput(format!(
                "不支持的文件后端种类: {}",
                other
            )))
        }
    };

    let backend_id = uuid::Uuid::new_v4().to_string();
    state
        .file_backends
        .lock()
        .insert(account_id, (backend_id.clone(), backend));
    Ok(backend_id)
}

/// 断开一个文件后端连接（按 backendId 查找对应 account_id 移除）。
#[tauri::command]
pub fn file_disconnect(backend_id: String, state: State<'_, AppState>) -> AppResult<()> {
    let mut map = state.file_backends.lock();
    let account_id = map
        .iter()
        .find(|(_, (bid, _))| bid == &backend_id)
        .map(|(aid, _)| aid.clone());
    match account_id {
        Some(aid) => {
            map.remove(&aid);
            Ok(())
        }
        None => Err(AppError::NotFound(format!(
            "文件后端连接 {} 不存在",
            backend_id
        ))),
    }
}

// ---------------------------------------------------------------------------
// 文件操作（按 backendId 从运行时 map 取后端）
// ---------------------------------------------------------------------------

/// 从运行时 map 取出后端句柄（按 backendId 反查）；不存在返回错误。
fn get_backend(state: &AppState, backend_id: &str) -> AppResult<Arc<dyn FileBackend>> {
    state
        .file_backends
        .lock()
        .iter()
        .find(|(_, (bid, _))| bid == backend_id)
        .map(|(_, (_, b))| b.clone())
        .ok_or_else(|| AppError::NotFound(format!("文件后端连接 {} 不存在", backend_id)))
}

/// 列出远程目录内容。
#[tauri::command]
pub async fn file_list(
    backend_id: String,
    path: String,
    state: State<'_, AppState>,
) -> AppResult<Vec<FileEntry>> {
    let backend = get_backend(&state, &backend_id)?;
    backend.list_dir(&path).await
}

/// 获取远程文件元信息。
#[tauri::command]
pub async fn file_stat(
    backend_id: String,
    path: String,
    state: State<'_, AppState>,
) -> AppResult<FileMeta> {
    let backend = get_backend(&state, &backend_id)?;
    backend.stat(&path).await
}

/// 创建远程目录。
#[tauri::command]
pub async fn file_mkdir(
    backend_id: String,
    path: String,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let backend = get_backend(&state, &backend_id)?;
    backend.mkdir(&path).await
}

/// 重命名远程文件/目录。
#[tauri::command]
pub async fn file_rename(
    backend_id: String,
    oldpath: String,
    newpath: String,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let backend = get_backend(&state, &backend_id)?;
    backend.rename(&oldpath, &newpath).await
}

/// 删除远程文件或目录。
#[tauri::command]
pub async fn file_remove(
    backend_id: String,
    path: String,
    is_dir: bool,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let backend = get_backend(&state, &backend_id)?;
    if is_dir {
        backend.remove_dir(&path).await
    } else {
        backend.remove_file(&path).await
    }
}

/// 下载任务参数。
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDownloadParams {
    pub backend_id: String,
    pub remote_path: String,
    pub local_path: String,
    pub task_id: String,
}

/// 下载远程文件到本地（异步任务，进度通过事件推送）。
#[tauri::command]
pub async fn file_download(
    params: FileDownloadParams,
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<()> {
    let backend = get_backend(&state, &params.backend_id)?;
    let local = PathBuf::from(&params.local_path);
    let task_id = params.task_id.clone();
    let app_for_progress = app.clone();

    let result = backend
        .download(
            &params.remote_path,
            &local,
            Arc::new(move |transferred, total| {
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
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileUploadParams {
    pub backend_id: String,
    pub local_path: String,
    pub remote_path: String,
    pub task_id: String,
}

/// 上传本地文件到远程（异步任务，进度通过事件推送）。
#[tauri::command]
pub async fn file_upload(
    params: FileUploadParams,
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<()> {
    let backend = get_backend(&state, &params.backend_id)?;
    let local = PathBuf::from(&params.local_path);
    let task_id = params.task_id.clone();
    let app_for_progress = app.clone();

    let result = backend
        .upload(
            &local,
            &params.remote_path,
            Arc::new(move |transferred, total| {
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
