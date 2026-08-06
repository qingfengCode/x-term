//! 会话与分组管理命令，以及终端会话的建立、断开。

use tauri::State;

use crate::error::{AppError, AppResult};
use crate::ssh::session::{resolve_credential, SshSession};
use crate::state::AppState;
use crate::storage::sessions_repo::{Group, Session};

// ---------------------------------------------------------------------------
// 分组 CRUD
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_groups(state: State<'_, AppState>) -> AppResult<Vec<Group>> {
    let conn = state.conn()?;
    crate::storage::sessions_repo::list_groups(&conn)
}

#[tauri::command]
pub fn save_group(group: Group, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    crate::storage::sessions_repo::upsert_group(&conn, &group)
}

#[tauri::command]
pub fn delete_group(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    crate::storage::sessions_repo::delete_group(&conn, &id)
}

// ---------------------------------------------------------------------------
// 会话 CRUD
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn list_sessions(state: State<'_, AppState>) -> AppResult<Vec<Session>> {
    let conn = state.conn()?;
    crate::storage::sessions_repo::list_sessions(&conn)
}

#[tauri::command]
pub fn get_session(id: String, state: State<'_, AppState>) -> AppResult<Option<Session>> {
    let conn = state.conn()?;
    crate::storage::sessions_repo::get_session(&conn, &id)
}

#[tauri::command]
pub fn save_session(session: Session, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    crate::storage::sessions_repo::upsert_session(&conn, &session)
}

#[tauri::command]
pub fn delete_session(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    crate::storage::sessions_repo::delete_session(&conn, &id)
}

// ---------------------------------------------------------------------------
// 连接管理
// ---------------------------------------------------------------------------

/// 连接一个会话配置，打开交互式终端，返回终端实例 id（前端 tab 标识）。
///
/// 流程：
/// 1. 从 DB 取会话配置。
/// 2. 解析凭据（需要保险库已解锁）。
/// 3. [`SshSession::open`] 建立连接并打开 PTY。
/// 4. [`SshSession::spawn_reader`] 启动输出读取任务。
/// 5. 把 session 存入 [`AppState::terminals`]，返回实例 id。
#[tauri::command]
pub async fn connect_session(
    session_config_id: String,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> AppResult<String> {
    // 1. 取会话配置。
    let session_config = {
        let conn = state.conn()?;
        crate::storage::sessions_repo::get_session(&conn, &session_config_id)?
            .ok_or_else(|| AppError::NotFound(format!("会话 {} 不存在", session_config_id)))?
    };

    // 按协议分派。
    let instance_id = match session_config.protocol.as_str() {
        "telnet" => {
            // Telnet：纯 TCP，无 SSH 认证（用户名/密码在连接后交互输入）。
            let telnet = crate::telnet::TelnetSession::connect_and_spawn(
                &session_config.host,
                session_config.port,
                session_config.id.clone(),
                app,
            )
            .await?;
            let id = telnet.id.clone();
            state
                .terminals
                .lock()
                .insert(id.clone(), crate::state::TerminalSession::Telnet(telnet));
            id
        }
        _ => {
            // 默认 SSH（含未知协议回退）。
            // 解析凭据。把 vault clone 出来，避免 RwLockReadGuard 跨 await（非 Send）。
            let resolved = {
                let vault_guard = state.vault_read()?;
                let vault = vault_guard
                    .as_ref()
                    .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
                    .clone();
                drop(vault_guard);
                let conn = state.conn()?;
                resolve_credential(&session_config, &vault, &conn)?
            };
            // 打开 SSH 会话。
            let mut ssh =
                SshSession::open(&session_config, resolved, state.inner().clone()).await?;
            ssh.spawn_reader()?;
            let id = ssh.id.clone();
            state
                .terminals
                .lock()
                .insert(id.clone(), crate::state::TerminalSession::Ssh(ssh));
            id
        }
    };

    Ok(instance_id)
}

/// 断开一个终端实例。
#[tauri::command]
pub async fn disconnect_session(instance_id: String, state: State<'_, AppState>) -> AppResult<()> {
    let session = state
        .terminals
        .lock()
        .remove(&instance_id)
        .ok_or_else(|| AppError::NotFound(format!("终端 {} 不存在", instance_id)))?;
    match session {
        crate::state::TerminalSession::Ssh(mut ssh) => ssh.close().await,
        crate::state::TerminalSession::Telnet(_) => {
            // TelnetSession drop 时 reader_handle 被 abort（JoinHandle abort 在 Drop）。
            // 这里直接 drop 即可，连接断开后 reader 任务自然结束。
            Ok(())
        }
    }
}

/// 为某个会话配置单独打开一个 SFTP 会话，返回 sftpId。
///
/// MVP 阶段 SFTP 与终端使用**独立的** SSH 连接（不复用终端连接）。这是为了简化
/// handle 所有权管理；连接开销在局域网/可信网络下可接受，未来可优化为复用。
#[tauri::command]
pub async fn open_sftp_for_session(
    session_config_id: String,
    state: State<'_, AppState>,
) -> AppResult<String> {
    let session_config = {
        let conn = state.conn()?;
        crate::storage::sessions_repo::get_session(&conn, &session_config_id)?
            .ok_or_else(|| AppError::NotFound(format!("会话 {} 不存在", session_config_id)))?
    };

    let resolved = {
        let vault_guard = state.vault_read()?;
        let vault = vault_guard
            .as_ref()
            .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
            .clone();
        drop(vault_guard);
        let conn = state.conn()?;
        resolve_credential(&session_config, &vault, &conn)?
    };

    // 直接连接并认证，不经过 SshSession::open（避免触发 PTY/shell）。
    let handle = crate::ssh::client::connect_direct(
        &session_config.host,
        session_config.port,
        &session_config.username,
        &session_config.id,
        resolved.auth_method,
        state.inner().clone(),
    )
    .await?;

    let sftp = crate::ssh::sftp::open_sftp(&handle).await?;
    let sftp_id = uuid::Uuid::new_v4().to_string();
    state.sftp_sessions.lock().insert(
        sftp_id.clone(),
        (std::sync::Arc::new(sftp), std::sync::Arc::new(handle)),
    );

    Ok(sftp_id)
}

/// 前端回复 SSH 二次认证挑战（keyboard-interactive）。
///
/// 后端在认证过程中 emit `ssh:auth_challenge` 事件并阻塞等待本命令回传：
/// - `responses` 为 `Some(vec)` 表示提交，数组与事件中 `prompts` 一一对应；
/// - `responses` 为 `None` 表示用户取消认证。
///
/// 挑战不存在（已超时/已取消）时返回 NotFound，前端可忽略。
#[tauri::command]
pub fn ssh_auth_respond(
    challenge_id: String,
    responses: Option<Vec<String>>,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let tx = state
        .pending_auth_challenges
        .lock()
        .remove(&challenge_id)
        .ok_or_else(|| AppError::NotFound(format!("认证挑战 {} 不存在或已超时", challenge_id)))?;
    let reply = match responses {
        Some(responses) => crate::ssh::client::AuthChallengeReply::Respond(responses),
        None => crate::ssh::client::AuthChallengeReply::Cancel,
    };
    tx.send(reply)
        .map_err(|_| AppError::Auth("认证挑战已关闭".into()))?;
    Ok(())
}

/// 前端回复 SSH 主机公钥变更确认。
///
/// 后端在 [`crate::ssh::client::ClientHandler::check_server_key`] 检测到主机
/// 公钥与 known_hosts 记录不符时 emit `ssh:host_key_challenge` 事件并阻塞等待
/// 本命令回传 `decision`：
/// - `AcceptAndUpdate`：接受新公钥并更新 known_hosts 记录；
/// - `AcceptOnce`：仅本次接受，不更新记录；
/// - `Reject`：拒绝连接（认证失败）。
///
/// 挑战不存在（已超时/已关闭）时返回 NotFound，前端可忽略。
#[tauri::command]
pub fn ssh_host_key_respond(
    challenge_id: String,
    decision: String,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let tx = state
        .pending_host_keys
        .lock()
        .remove(&challenge_id)
        .ok_or_else(|| {
            AppError::NotFound(format!("主机公钥确认 {} 不存在或已超时", challenge_id))
        })?;
    let decision = match decision.as_str() {
        "AcceptAndUpdate" => crate::ssh::client::HostKeyDecision::AcceptAndUpdate,
        "AcceptOnce" => crate::ssh::client::HostKeyDecision::AcceptOnce,
        "Reject" => crate::ssh::client::HostKeyDecision::Reject,
        other => {
            return Err(AppError::InvalidInput(format!(
                "未知的主机公钥决策: {}（应为 AcceptAndUpdate / AcceptOnce / Reject）",
                other
            )))
        }
    };
    tx.send(decision)
        .map_err(|_| AppError::Auth("主机公钥确认已关闭".into()))?;
    Ok(())
}
