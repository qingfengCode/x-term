//! 会话与分组管理命令，以及终端会话的建立、断开。
//!
//! 分组/会话 CRUD 全部 async + `spawn_blocking`：Tauri 的非 async 命令在**主线程**
//! 执行，而每条 CRUD 都要从 r2d2 池取连接（池耗尽等待上限 5s）并执行 SQLite 语句
//! （`busy_timeout` 5000ms）。一旦在主线程阻塞，窗口消息循环停止，表现为整个 UI
//! 冻结；改成 async 后阻塞发生在 tokio 阻塞线程池，界面不受影响。

use tauri::{Manager, State};

use crate::error::{AppError, AppResult};
use crate::ssh::client::{AuthMethod, PasswordAuth};
use crate::ssh::session::{resolve_credential, ResolvedCredential, SshSession};
use crate::state::AppState;
use crate::storage::sessions_repo::{Group, Session};

// ---------------------------------------------------------------------------
// 分组 CRUD
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_groups(state: State<'_, AppState>) -> AppResult<Vec<Group>> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::sessions_repo::list_groups(&conn)
    })
    .await
    .map_err(|e| AppError::Storage(format!("读取分组任务失败: {}", e)))?
}

#[tauri::command]
pub async fn save_group(group: Group, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::sessions_repo::upsert_group(&conn, &group)
    })
    .await
    .map_err(|e| AppError::Storage(format!("保存分组任务失败: {}", e)))?
}

#[tauri::command]
pub async fn delete_group(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::sessions_repo::delete_group(&conn, &id)
    })
    .await
    .map_err(|e| AppError::Storage(format!("删除分组任务失败: {}", e)))?
}

// ---------------------------------------------------------------------------
// 会话 CRUD
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_sessions(state: State<'_, AppState>) -> AppResult<Vec<Session>> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::sessions_repo::list_sessions(&conn)
    })
    .await
    .map_err(|e| AppError::Storage(format!("读取会话列表任务失败: {}", e)))?
}

#[tauri::command]
pub async fn get_session(id: String, state: State<'_, AppState>) -> AppResult<Option<Session>> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::sessions_repo::get_session(&conn, &id)
    })
    .await
    .map_err(|e| AppError::Storage(format!("读取会话任务失败: {}", e)))?
}

#[tauri::command]
pub async fn save_session(session: Session, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::sessions_repo::upsert_session(&conn, &session)
    })
    .await
    .map_err(|e| AppError::Storage(format!("保存会话任务失败: {}", e)))?
}

#[tauri::command]
pub async fn delete_session(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::sessions_repo::delete_session(&conn, &id)
    })
    .await
    .map_err(|e| AppError::Storage(format!("删除会话任务失败: {}", e)))?
}

// ---------------------------------------------------------------------------
// 连接管理
// ---------------------------------------------------------------------------

/// 从保险库解析会话凭据（vault 未解锁时返回 Auth 错误）。
///
/// 把 vault 引用 clone 出来，避免 RwLockReadGuard 跨 await（非 Send）。
fn resolve_session_credential(
    session_config: &Session,
    state: &AppState,
) -> AppResult<ResolvedCredential> {
    let vault_guard = state.vault_read()?;
    let vault = vault_guard
        .as_ref()
        .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
        .clone();
    drop(vault_guard);
    let conn = state.conn()?;
    resolve_credential(session_config, &vault, &conn)
}

/// 打开一个 SSH 终端会话：连接+认证 → PTY → shell → 注册到 state，返回实例 id。
async fn open_ssh_terminal(
    session_config: &Session,
    resolved: ResolvedCredential,
    state: &AppState,
) -> AppResult<String> {
    let mut ssh = SshSession::open(session_config, resolved, state.clone()).await?;
    ssh.spawn_reader()?;
    let id = ssh.id.clone();
    let terminal = crate::state::TerminalSession::Ssh(ssh);
    attach_output_log_if_enabled(state, &terminal, &session_config.name);
    state.terminals.lock().insert(id.clone(), terminal);
    Ok(id)
}

/// 设置开启时为终端会话装配输出日志（logs/<会话名>_<时间>.log）。
///
/// reader 持有共享句柄，装配即时生效；失败只告警不阻断连接。
fn attach_output_log_if_enabled(
    state: &AppState,
    session: &crate::state::TerminalSession,
    name: &str,
) {
    let enabled = crate::config::settings_load_inner(state)
        .map(|s| s.terminal.output_log)
        .unwrap_or(false);
    if enabled {
        session.attach_output_log(name, &state.data_dir.join("logs"));
    }
}

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
            // 建连超时复用设置里的 SSH 连接超时（0 = 永不超时），默认 15s。
            let timeout_secs = crate::config::settings_load_inner(&state)
                .map(|s| s.terminal.ssh_connect_timeout_secs)
                .unwrap_or(15);
            let telnet = crate::telnet::TelnetSession::connect_and_spawn(
                &session_config.host,
                session_config.port,
                session_config.id.clone(),
                timeout_secs,
                app,
            )
            .await?;
            let id = telnet.id.clone();
            let terminal = crate::state::TerminalSession::Telnet(telnet);
            attach_output_log_if_enabled(state.inner(), &terminal, &session_config.name);
            state.terminals.lock().insert(id.clone(), terminal);
            id
        }
        _ => {
            // 默认 SSH（含未知协议回退）。
            let resolved = resolve_session_credential(&session_config, state.inner())?;
            open_ssh_terminal(&session_config, resolved, state.inner()).await?
        }
    };

    Ok(instance_id)
}

/// 认证失败后，用手动输入的密码/验证码重试连接（`connect_session` 的手动认证版）。
///
/// 与 [`connect_session`] 的区别：
/// - `password` 非空时，忽略会话配置的认证方式（私钥会话也回退），改用该
///   密码认证；
/// - `otp` 为二次认证验证码（口令码/动态口令等），keyboard-interactive 流程
///   自动预填验证码类提示（跳板机常见第 1 轮问密码、第 2 轮问口令码，任意
///   轮次均可预填），其余提示仍弹窗请用户输入；
/// - 两者均可为空：密码为空回退使用会话配置已保存的凭据，验证码为空则不预填。
///
/// 仅支持 SSH 协议；Telnet 等协议直接回退 [`connect_session`] 原流程。
#[tauri::command]
pub async fn connect_session_with_manual_auth(
    session_config_id: String,
    password: Option<String>,
    otp: Option<String>,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> AppResult<String> {
    let session_config = {
        let conn = state.conn()?;
        crate::storage::sessions_repo::get_session(&conn, &session_config_id)?
            .ok_or_else(|| AppError::NotFound(format!("会话 {} 不存在", session_config_id)))?
    };

    if session_config.protocol.as_str() != "ssh" {
        // 非 SSH 协议无手动认证概念，回退普通连接流程。
        return connect_session(session_config_id, state, app).await;
    }

    let mut resolved = resolve_session_credential(&session_config, state.inner())?;
    let manual_password = password.filter(|p| !p.trim().is_empty());
    let otp = otp.filter(|c| !c.trim().is_empty());
    // 手动密码优先：提供则一律改用密码认证（私钥会话也回退），验证码一并预填。
    if let Some(pw) = manual_password {
        resolved.auth_method = AuthMethod::Password(PasswordAuth { password: pw, otp });
    } else if let AuthMethod::Password(pa) = &mut resolved.auth_method {
        // 未提供密码但原本是密码认证：沿用已保存密码，仅附加验证码。
        pa.otp = otp;
    } else if otp.is_some() {
        // 私钥会话 + 只提供了验证码：私钥大概率仍会失败，改走密码认证让
        // keyboard-interactive 弹窗收集密码（验证码已预填）。
        resolved.auth_method = AuthMethod::Password(PasswordAuth {
            password: String::new(),
            otp,
        });
    }
    // 私钥会话 + 均未提供：保持原认证方式重试。

    open_ssh_terminal(&session_config, resolved, state.inner()).await
}

/// 复制一个已连接的 SSH 终端会话：在同一条**已认证**连接上打开新 channel
/// （PTY + shell），返回新终端实例 id。
///
/// 不新建 TCP 连接、不重新认证——二次认证（口令码/动态口令）服务器上复制
/// 通道无需再次输入验证码。连接生命周期由共享计数管理：任一副本关闭只
/// 关自己的 channel，最后一个关闭者才断开传输层（见 [`SshSession::close`]）。
///
/// 仅支持 SSH 协议；源实例不存在 / 非 SSH / 通道打开失败时返回错误，由前端
/// 回退到全量重连（`connect_session`）。
#[tauri::command]
pub async fn clone_terminal_session(
    instance_id: String,
    state: State<'_, AppState>,
) -> AppResult<String> {
    // 短锁取出共享部件后立即放锁：channel_open_session 是网络操作，
    // 锁内 await 会冻结所有终端的读写命令。
    let transport = {
        let terminals = state.terminals.lock();
        match terminals.get(&instance_id) {
            Some(crate::state::TerminalSession::Ssh(s)) => s.shared_transport(),
            _ => {
                return Err(AppError::NotFound(format!(
                    "终端 {} 不存在或非 SSH 会话（仅 SSH 支持复制通道）",
                    instance_id
                )))
            }
        }
    };

    let mut ssh = SshSession::open_channel_on(transport).await?;
    ssh.spawn_reader()?;
    let id = ssh.id.clone();
    // 复制通道：日志名沿用源会话配置名（查 DB；查不到用配置 id）。
    // 在 move 进 TerminalSession 之前取。
    let config_id = ssh.session_config_id.clone();
    let log_name = {
        let conn = state.conn()?;
        crate::storage::sessions_repo::get_session(&conn, &config_id)
            .ok()
            .flatten()
            .map(|s| s.name)
            .unwrap_or(config_id)
    };
    let terminal = crate::state::TerminalSession::Ssh(ssh);
    attach_output_log_if_enabled(state.inner(), &terminal, &log_name);
    state.terminals.lock().insert(id.clone(), terminal);
    Ok(id)
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
        crate::state::TerminalSession::Local(mut local) => {
            // kill 子进程 + abort reader（Drop 亦兜底），本地 shell 直接关闭。
            local.close()
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

    let resolved = resolve_session_credential(&session_config, state.inner())?;

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
