//! 不依赖活跃终端实例的执行实现（MCP 专用）。
//!
//! 外部 MCP 客户端不知道 X-Term 内部的 instanceId / connId，只传"连接名"。本模块
//! 负责：按名查 Session/DbProfile → 解析凭据 → 建立短连接 → 执行 → 返回输出。
//!
//! - [`exec_ssh_by_name`]：SSH exec（参考 `ai::tools::exec_ssh` 的"独立连接"分支）。
//! - [`exec_sql_by_name`]：MySQL 执行（解析密码 → 直连或 SSH 隧道）。
//! - [`list_ssh_sessions_view`] / [`list_db_profiles_view`]：只读元数据视图。
//!
//! 注意 `resolve_credential` / `fetch_mysql_password` 都是同步的且需要短生命 DB
//! 连接，本模块在调用前集中获取连接、解析凭据后立即释放，不在 `.await` 间持有。

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::time::timeout;

use crate::database::mysql::{connect_direct as mysql_connect_direct, connect_via_ssh};
use crate::database::profiles::{list_db_profiles, DbProfile};
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::storage::sessions_repo::{get_session, list_sessions, Session};

/// exec_ssh 单命令执行超时（30 秒）。
const EXEC_TIMEOUT: Duration = Duration::from_secs(30);
/// exec_ssh 输出截断上限（16 KiB）。
const EXEC_OUTPUT_CAP: usize = 16 * 1024;

// ===========================================================================
// 视图（不含敏感字段）
// ===========================================================================

/// SSH 会话的只读视图（不含密码/密钥）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshSessionView {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
}

/// DB profile 的只读视图（不含密码/凭据 id）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbProfileView {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub database: Option<String>,
}

/// 列出所有 SSH 会话配置（不含敏感字段）。
pub fn list_ssh_sessions_view(state: &AppState) -> AppResult<Vec<SshSessionView>> {
    let conn = state.conn()?;
    let sessions = list_sessions(&conn)?;
    Ok(sessions
        .into_iter()
        .filter(|s| s.protocol == "ssh")
        .map(|s| SshSessionView {
            id: s.id,
            name: s.name,
            host: s.host,
            port: s.port,
            username: s.username,
        })
        .collect())
}

/// 列出所有 DB profile（不含敏感字段）。
pub fn list_db_profiles_view(state: &AppState) -> AppResult<Vec<DbProfileView>> {
    let conn = state.conn()?;
    let profiles = list_db_profiles(&conn)?;
    Ok(profiles
        .into_iter()
        .map(|p| DbProfileView {
            id: p.id,
            name: p.name,
            host: p.host,
            port: p.port,
            username: p.username,
            database: p.default_database,
        })
        .collect())
}

// ===========================================================================
// 按名查找配置
// ===========================================================================

/// 按 name 查 SSH Session；不存在则报错。
fn find_session_by_name(state: &AppState, name: &str) -> AppResult<Session> {
    let conn = state.conn()?;
    crate::storage::sessions_repo::get_session_by_name(&conn, name)?
        .filter(|s| s.protocol == "ssh")
        .ok_or_else(|| AppError::NotFound(format!("找不到名为 `{}` 的 SSH 会话配置", name)))
}

/// 按 name 查 DbProfile；不存在则报错。
fn find_profile_by_name(state: &AppState, name: &str) -> AppResult<DbProfile> {
    let conn = state.conn()?;
    crate::database::profiles::get_db_profile_by_name(&conn, name)?
        .ok_or_else(|| AppError::NotFound(format!("找不到名为 `{}` 的数据库配置", name)))
}

// --- 按 id 查（MCP 绑定模式：用户在页面把某个 MCP 绑定到一个具体资源 id） ---

/// 按 id 查 SSH Session；不存在则报错。
fn find_session_by_id(state: &AppState, id: &str) -> AppResult<Session> {
    let conn = state.conn()?;
    crate::storage::sessions_repo::get_session(&conn, id)?
        .filter(|s| s.protocol == "ssh")
        .ok_or_else(|| AppError::NotFound(format!("找不到 id 为 `{}` 的 SSH 会话配置", id)))
}

/// 按 id 查 DbProfile；不存在则报错。
fn find_profile_by_id(state: &AppState, id: &str) -> AppResult<DbProfile> {
    let conn = state.conn()?;
    crate::database::profiles::get_db_profile(&conn, id)?
        .ok_or_else(|| AppError::NotFound(format!("找不到 id 为 `{}` 的数据库配置", id)))
}

/// 按 id 查 SSH 会话的展示名（供前端确认浮层标注来源；找不到时返回占位）。
pub fn session_name_by_id(state: &AppState, id: &str) -> String {
    find_session_by_id(state, id)
        .map(|s| s.name)
        .unwrap_or_else(|_| "(未知会话)".into())
}

/// 按 id 查 DB profile 的展示名（供前端确认浮层标注来源）。
pub fn profile_name_by_id(state: &AppState, id: &str) -> String {
    find_profile_by_id(state, id)
        .map(|p| p.name)
        .unwrap_or_else(|_| "(未知数据库)".into())
}

// ===========================================================================
// exec_ssh_by_name
// ===========================================================================

/// 在指定（按名查到的）SSH 会话对应的服务器上执行一条命令。
///
/// 流程（参考 `ai::tools::exec_ssh` 的独立连接分支）：
/// 1. 按 `session_name` 查 Session 配置。
/// 2. resolve_credential（同步，短生命 DB 连接）。
/// 3. connect_direct → channel_open_session → `channel.exec(false, command)`。
/// 4. 循环 channel.wait() 收集 Data / ExtendedData，去 ANSI，截断 16KB。
/// 5. disconnect，返回输出文本。
///
/// 整个过程用 30s 超时包裹。`app` 用于 russh 的事件 handler。
pub async fn exec_ssh_by_name(
    state: &AppState,
    session_name: &str,
    command: &str,
    app: tauri::AppHandle,
) -> AppResult<String> {
    let session_config = find_session_by_name(state, session_name)?;
    exec_ssh_with_config(state, session_config, command, app).await
}

/// 在指定（按 id 查到的）SSH 会话对应的服务器上执行一条命令（MCP 绑定模式）。
///
/// 与 [`exec_ssh_by_name`] 完全一致，仅查找方式由 name 改为 id。
pub async fn exec_ssh_by_id(
    state: &AppState,
    session_id: &str,
    command: &str,
    app: tauri::AppHandle,
) -> AppResult<String> {
    let session_config = find_session_by_id(state, session_id)?;
    exec_ssh_with_config(state, session_config, command, app).await
}

/// 共用的"解析凭据 → 建连 → exec → 收集输出"实现（by_name / by_id 都委托到此）。
///
/// 流程参考 `ai::tools::exec_ssh` 的独立连接分支：
/// 1. resolve_credential（同步，短生命 DB 连接）。
/// 2. connect_direct → channel_open_session → `channel.exec(false, command)`。
/// 3. 循环 channel.wait() 收集 Data / ExtendedData，去 ANSI，截断 16KB。
/// 4. disconnect，返回输出文本。整个过程用 30s 超时包裹。
async fn exec_ssh_with_config(
    state: &AppState,
    session_config: Session,
    command: &str,
    app: tauri::AppHandle,
) -> AppResult<String> {
    if command.trim().is_empty() {
        return Err(AppError::InvalidInput("command 不能为空".into()));
    }

    // 解析凭据（同步块，DB 连接短生命）。
    let resolved = {
        let vault = {
            let guard = state.vault_read()?;
            guard
                .as_ref()
                .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
                .clone()
        };
        let conn = state.conn()?;
        crate::ssh::session::resolve_credential(&session_config, &vault, &conn)?
    };

    log::info!(
        "[mcp] exec_ssh 开始：{}@{}:{}, 命令：{}",
        session_config.username,
        session_config.host,
        session_config.port,
        command
    );

    // 连接 + exec（30s 超时）。
    let run = async {
        let handle = crate::ssh::client::connect_direct(
            &session_config.host,
            session_config.port,
            &session_config.username,
            resolved.auth_method,
            app,
        )
        .await?;

        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|e| AppError::Ssh(format!("打开 channel 失败: {}", e)))?;

        // russh 0.45: exec(want_reply: bool, command: &str)。
        channel
            .exec(false, command)
            .await
            .map_err(|e| AppError::Ssh(format!("exec 失败: {}", e)))?;

        // 收集输出。
        let mut raw: Vec<u8> = Vec::new();
        let mut exit_code: Option<u32> = None;
        use russh::ChannelMsg;
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { ref data }) => {
                    raw.extend_from_slice(data.as_ref());
                }
                Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                    raw.extend_from_slice(data.as_ref());
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exit_code = Some(exit_status);
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            }
            if raw.len() >= EXEC_OUTPUT_CAP {
                raw.truncate(EXEC_OUTPUT_CAP);
                break;
            }
        }

        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "bye", "en")
            .await;

        Ok::<_, AppError>((raw, exit_code))
    };

    match timeout(EXEC_TIMEOUT, run).await {
        Ok(Ok((raw, exit_code))) => {
            let text = strip_ansi(&String::from_utf8_lossy(&raw));
            let truncated = if text.len() > EXEC_OUTPUT_CAP {
                let mut s: String = text.chars().take(EXEC_OUTPUT_CAP).collect();
                s.push_str("\n... [输出已截断]");
                s
            } else {
                text
            };
            let code_suffix = match exit_code {
                Some(0) | None => String::new(),
                Some(c) => format!("\n[exit: {}]", c),
            };
            Ok(format!("{}{}", truncated, code_suffix))
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err(AppError::Ssh("exec_ssh 执行超时（30s）".into())),
    }
}

// ===========================================================================
// exec_sql_by_name
// ===========================================================================

/// 在指定（按名查到的）DB profile 对应的数据库上执行 SQL。
///
/// 流程：
/// 1. 按 `profile_name` 查 DbProfile。
/// 2. 取 MySQL 密码（`credential_id` → `credentials` 表 → vault 解密）。
/// 3. 若 profile 指定了 `ssh_session_config_id`，先建立 SSH 隧道再连 MySQL；
///    否则直连。
/// 4. 执行 SQL（限制返回行数 = `limit`），结果格式化为对齐文本表格。
pub async fn exec_sql_by_name(
    state: &AppState,
    profile_name: &str,
    sql: &str,
    limit: u32,
    app: tauri::AppHandle,
) -> AppResult<String> {
    let profile = find_profile_by_name(state, profile_name)?;
    exec_sql_with_profile(state, profile, sql, limit, None, app).await
}

/// 在指定（按 id 查到的）DB profile 对应的数据库上执行 SQL（MCP 绑定模式）。
///
/// `database`：若为 Some，覆盖 profile 的 default_database（MCP 绑定到具体库时使用）。
pub async fn exec_sql_by_id(
    state: &AppState,
    profile_id: &str,
    sql: &str,
    limit: u32,
    database: Option<&str>,
    app: tauri::AppHandle,
) -> AppResult<String> {
    let profile = find_profile_by_id(state, profile_id)?;
    exec_sql_with_profile(state, profile, sql, limit, database, app).await
}

/// 共用的"取密码 → 建连 → 执行 → 关闭"实现（by_name / by_id 都委托到此）。
///
/// `database_override`：若为 Some，覆盖 profile.default_database（MCP 绑定具体库）。
async fn exec_sql_with_profile(
    state: &AppState,
    profile: DbProfile,
    sql: &str,
    limit: u32,
    database_override: Option<&str>,
    app: tauri::AppHandle,
) -> AppResult<String> {
    if sql.trim().is_empty() {
        return Err(AppError::InvalidInput("sql 不能为空".into()));
    }

    // 确定实际使用的数据库：override 优先，否则用 profile 的 default_database。
    let effective_db: Option<&str> = database_override.or(profile.default_database.as_deref());

    // 取 MySQL 密码（同步块）。
    let mysql_pass = {
        let cred_id = profile.credential_id.as_ref().ok_or_else(|| {
            AppError::Auth(format!("DB profile `{}` 缺少 credential_id", profile.name))
        })?;
        let vault = {
            let guard = state.vault_read()?;
            guard
                .as_ref()
                .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
                .clone()
        };
        let conn = state.conn()?;
        crate::database::mysql::fetch_mysql_password(&conn, cred_id, &vault)?
    };

    log::info!(
        "[mcp] exec_sql 开始：{}@{}:{}/{}, SQL: {}",
        profile.username,
        profile.host,
        profile.port,
        effective_db.unwrap_or(""),
        sql
    );

    // 建立连接。
    let conn_obj = if let Some(ssh_id) = &profile.ssh_session_config_id {
        // SSH 隧道模式。
        let ssh_config = {
            let conn = state.conn()?;
            get_session(&conn, ssh_id)?
                .ok_or_else(|| AppError::NotFound(format!("SSH 会话 {} 不存在", ssh_id)))?
        };
        let resolved = {
            let vault = {
                let guard = state.vault_read()?;
                guard
                    .as_ref()
                    .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
                    .clone()
            };
            let conn = state.conn()?;
            crate::ssh::session::resolve_credential(&ssh_config, &vault, &conn)?
        };
        connect_via_ssh(
            &ssh_config,
            resolved,
            &profile.host,
            profile.port,
            &profile.username,
            &mysql_pass,
            effective_db,
            app,
        )
        .await?
    } else {
        // 直连。
        mysql_connect_direct(
            &profile.host,
            profile.port,
            &profile.username,
            &mysql_pass,
            effective_db,
        )
        .await?
    };

    // 执行并立即关闭连接（MCP 模式不缓存连接池）。
    let res = conn_obj.execute(sql, limit).await;
    conn_obj.close().await;

    let qr = res?;
    Ok(format_query_result(&qr))
}

// ===========================================================================
// 辅助
// ===========================================================================

use crate::utils::{format_query_result, strip_ansi};

/// 解析工具参数中 `sessionName` 字段（exec_ssh 用）。
pub fn arg_session_name(args: &Value) -> AppResult<String> {
    args.get("sessionName")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .ok_or_else(|| AppError::InvalidInput("缺少 sessionName 参数".into()))
}

/// 解析工具参数中 `profileName` 字段（exec_sql 用）。
pub fn arg_profile_name(args: &Value) -> AppResult<String> {
    args.get("profileName")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .ok_or_else(|| AppError::InvalidInput("缺少 profileName 参数".into()))
}

/// 解析工具参数中 `command` 字段（exec_ssh 用）。
pub fn arg_command(args: &Value) -> AppResult<String> {
    args.get("command")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .ok_or_else(|| AppError::InvalidInput("缺少 command 参数".into()))
}

/// 解析工具参数中 `sql` 字段（exec_sql 用）。
pub fn arg_sql(args: &Value) -> AppResult<String> {
    args.get("sql")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .ok_or_else(|| AppError::InvalidInput("缺少 sql 参数".into()))
}

/// 解析工具参数中 `limit` 字段（exec_sql 用，默认 100）。
pub fn arg_limit(args: &Value) -> u32 {
    args.get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(100)
}
