//! 不依赖活跃终端实例的执行实现（MCP 专用）。
//!
//! 外部 MCP 客户端不知道 X-Term 内部的 instanceId / connId，只传"连接名"。本模块
//! 负责：按名查 Session/DbProfile → 解析凭据 → 建立短连接 → 执行 → 返回输出。
//!
//! - [`exec_ssh_by_name`]：SSH exec（参考 `ai::tools::exec_ssh` 的"独立连接"分支）。
//! - [`exec_sql_by_name`]：MySQL 执行（解析密码 → 直连或 SSH 隧道）。
//! - [`list_ssh_sessions_view`] / [`list_db_profiles_view`]：只读元数据视图。
//! - [`list_files_by_id`] / [`upload_file_by_id`] / [`download_file_by_id`]：基于
//!   SFTP 的文件级运维工具（MCP 绑定模式）；`*_direct` 为客户端直连模式。
//!
//! 注意 `resolve_credential` / `fetch_mysql_password` 都是同步的且需要短生命 DB
//! 连接，本模块在调用前集中获取连接、解析凭据后立即释放，不在 `.await` 间持有。

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::time::timeout;

use crate::database::mysql::{connect_direct as mysql_connect_direct, connect_via_ssh};
use crate::database::profiles::{list_db_profiles, DbProfile};
use crate::error::{AppError, AppResult};
use crate::file_backend::s3::{S3Backend, S3Config};
use crate::file_backend::FileBackend;
use crate::ssh::client::AuthMethod;
use crate::ssh::sftp::open_sftp;
use crate::state::AppState;
use crate::storage::file_accounts_repo::{fetch_s3_credential, get_file_account, FileAccount};
use crate::storage::sessions_repo::{get_session, list_sessions, Session};

/// exec_ssh 单命令执行超时（30 秒）。
const EXEC_TIMEOUT: Duration = Duration::from_secs(30);
/// exec_ssh 输出截断上限（16 KiB）。
const EXEC_OUTPUT_CAP: usize = 16 * 1024;
/// SFTP 文件操作超时（5 分钟）。文件传输比单条命令耗时，给较大余量。
const SFTP_OP_TIMEOUT: Duration = Duration::from_secs(5 * 60);

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
) -> AppResult<String> {
    let session_config = find_session_by_name(state, session_name)?;
    exec_ssh_with_config(state, session_config, command).await
}

/// 在指定（按 id 查到的）SSH 会话对应的服务器上执行一条命令（MCP 绑定模式）。
///
/// 与 [`exec_ssh_by_name`] 完全一致，仅查找方式由 name 改为 id。
pub async fn exec_ssh_by_id(
    state: &AppState,
    session_id: &str,
    command: &str,
) -> AppResult<String> {
    let session_config = find_session_by_id(state, session_id)?;
    exec_ssh_with_config(state, session_config, command).await
}

/// 在调用方指定的服务器上执行一条命令（MCP 客户端直连模式）。
///
/// 与 [`exec_ssh_with_config`] 的区别：host/port/username/密码直接由调用方在工具
/// 参数中传入，不经过本地会话配置与 vault 解析。密码仅本次连接使用，不缓存、
/// 不落日志、不写入任何配置文件。
pub async fn exec_ssh_direct(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    command: &str,
    state: AppState,
) -> AppResult<String> {
    if command.trim().is_empty() {
        return Err(AppError::InvalidInput("command 不能为空".into()));
    }
    if password.is_empty() {
        return Err(AppError::Auth("password 不能为空".into()));
    }
    log::info!(
        "[mcp] exec_ssh（直连）开始：{}@{}:{}, 命令：{}",
        username,
        host,
        port,
        command
    );
    exec_ssh_with_auth(
        host,
        port,
        username,
        // 直连模式没有会话配置，二次认证弹窗仅展示 host:port。
        "direct",
        AuthMethod::Password(password.to_string()),
        command,
        state,
    )
    .await
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

    exec_ssh_with_auth(
        &session_config.host,
        session_config.port,
        &session_config.username,
        &session_config.id,
        resolved.auth_method,
        command,
        state.clone(),
    )
    .await
}

/// 共用的"建连 → exec → 收集输出"实现（by_config / direct 都委托到此）。
///
/// 流程：connect_direct → channel_open_session → `channel.exec(false, command)` →
/// 循环 channel.wait() 收集 Data / ExtendedData（去 ANSI，截断 16KB）→ disconnect。
/// 整个过程用 30s 超时包裹。`auth` 由调用方决定（vault 解析 / 参数直传）。
/// `session_config_id` 用于二次认证弹窗展示；直连模式传占位串。
async fn exec_ssh_with_auth(
    host: &str,
    port: u16,
    username: &str,
    session_config_id: &str,
    auth: AuthMethod,
    command: &str,
    state: AppState,
) -> AppResult<String> {
    // 连接 + exec（30s 超时）。
    let run = async {
        let handle = crate::ssh::client::connect_direct(
            host,
            port,
            username,
            session_config_id,
            auth,
            state,
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
) -> AppResult<String> {
    let profile = find_profile_by_name(state, profile_name)?;
    exec_sql_with_profile(state, profile, sql, limit, None).await
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
) -> AppResult<String> {
    let profile = find_profile_by_id(state, profile_id)?;
    exec_sql_with_profile(state, profile, sql, limit, database).await
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

    // SQL 全文可能含敏感数据（INSERT/UPDATE 的明文值），日志只记前 200 字符。
    log::info!(
        "[mcp] exec_sql 开始：{}@{}:{}/{}, SQL: {}",
        profile.username,
        profile.host,
        profile.port,
        effective_db.unwrap_or(""),
        truncate_log(&sql, 200)
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
            state.clone(),
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

/// 在调用方指定的 MySQL 服务器上执行 SQL（MCP 客户端直连模式）。
///
/// 与 [`exec_sql_with_profile`] 的区别：host/port/username/密码直接由调用方在工具
/// 参数中传入，不经过 DB profile 与 vault 解析；密码仅本次连接使用，不缓存、不落
/// 日志、不写入任何配置文件。
///
/// 不支持 SSH 隧道（如需要可后续扩展 sshHost/sshPort 等可选参数）。`database` 为
/// 可选：传则作为默认库连接，不传则不指定默认库（SQL 里可带 `db.table` 限定名）。
pub async fn exec_sql_direct(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    database: Option<&str>,
    sql: &str,
    limit: u32,
) -> AppResult<String> {
    if sql.trim().is_empty() {
        return Err(AppError::InvalidInput("sql 不能为空".into()));
    }
    if password.is_empty() {
        return Err(AppError::Auth("password 不能为空".into()));
    }
    // SQL 全文可能含敏感数据（INSERT/UPDATE 的明文值），日志只记前 200 字符。
    log::info!(
        "[mcp] exec_sql（直连）开始：{}@{}:{}/{}, SQL: {}",
        username,
        host,
        port,
        database.unwrap_or(""),
        truncate_log(sql, 200)
    );

    // 直连（不走 SSH 隧道）→ 执行 → 立即关闭。
    let conn_obj = mysql_connect_direct(host, port, username, password, database).await?;
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

// --- 客户端直连模式参数解析（host/port/username/password/database） ---

/// 解析工具参数中 `host` 字段（直连模式用，必填）。
pub fn arg_host(args: &Value) -> AppResult<String> {
    args.get("host")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::InvalidInput("缺少 host 参数（直连模式需指定目标服务器）".into()))
}

/// 解析工具参数中 `port` 字段（直连模式用，省略默认 22）。
pub fn arg_port(args: &Value) -> AppResult<u16> {
    match args.get("port") {
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=65535).contains(n))
            .map(|n| n as u16)
            .ok_or_else(|| AppError::InvalidInput("port 参数无效（需为 1-65535 的整数）".into())),
        None => Ok(22),
    }
}

/// 解析工具参数中 `username` 字段（直连模式用，必填）。
pub fn arg_username(args: &Value) -> AppResult<String> {
    args.get("username")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::InvalidInput("缺少 username 参数".into()))
}

/// 解析工具参数中 `password` 字段（直连模式用，必填；仅本次调用使用）。
pub fn arg_password(args: &Value) -> AppResult<String> {
    args.get("password")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::InvalidInput("缺少 password 参数".into()))
}

/// 解析工具参数中可选的 `database` 字段（DB 直连用；空/缺省返回 None）。
pub fn arg_database(args: &Value) -> Option<String> {
    args.get("database")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

// ===========================================================================
// 文件操作（list_files / upload_file / download_file）
// ===========================================================================
//
// 三个文件级 MCP 工具的底层实现。与 exec_ssh 同样采用"短连接"语义：
// connect_direct → open_sftp → 单次操作 → disconnect，不复用前端长连接 SFTP。
//
// 文件数据采用"本地路径往返"语义：
// - upload_file：AI 传 localPath（本机已存在的文件）→ 上传到远端 remotePath。
// - download_file：远端 remotePath → 下载到本机 localPath，返回本地路径。
// AI 可配合 exec_ssh 先把内容写到本地（如 `cat > /tmp/x`），再上传；避免 base64
// 编码大文件膨胀 context。

/// 日志截断：命令/SQL 等用户输入可能很长且含敏感内容，日志只保留前 `max` 字符。
fn truncate_log(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{}…", head)
    }
}

/// 解析工具参数中 `path` 字段（list_files 用，必填）。
pub fn arg_path(args: &Value) -> AppResult<String> {
    args.get("path")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::InvalidInput("缺少 path 参数".into()))
}

/// 解析工具参数中 `localPath` 字段（upload/download 用，必填）。
pub fn arg_local_path(args: &Value) -> AppResult<String> {
    args.get("localPath")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::InvalidInput("缺少 localPath 参数".into()))
}

/// 解析工具参数中 `remotePath` 字段（upload/download 用，必填）。
pub fn arg_remote_path(args: &Value) -> AppResult<String> {
    args.get("remotePath")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::InvalidInput("缺少 remotePath 参数".into()))
}

/// 共用的"解析凭据 → 建连 → 打开 SFTP → 操作 → 断开"实现，返回 SFTP 后端句柄。
///
/// 与 [`exec_ssh_with_config`] 一致的凭据解析流程，但在拿到 SSH Handle 后调用
/// [`open_sftp`] 而非 `channel.exec`。返回 `(FileBackend, Handle)`，调用方
/// 完成操作后用 handle 断开连接。
async fn open_sftp_for_config(
    state: &AppState,
    session_config: Session,
) -> AppResult<(
    std::sync::Arc<dyn FileBackend>,
    russh::client::Handle<crate::ssh::client::ClientHandler>,
)> {
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

    let handle = crate::ssh::client::connect_direct(
        &session_config.host,
        session_config.port,
        &session_config.username,
        &session_config.id,
        resolved.auth_method,
        state.clone(),
    )
    .await?;

    let sftp = open_sftp(&handle).await?;
    Ok((std::sync::Arc::new(sftp), handle))
}

/// 在指定（按 id 查到的）SSH 会话对应的服务器上列举目录（MCP 绑定模式）。
///
/// 返回 JSON 序列化的 `Vec<FileEntry>`（字段：name / isDir / size / modified）。
pub async fn list_files_by_id(state: &AppState, session_id: &str, path: &str) -> AppResult<String> {
    let session_config = find_session_by_id(state, session_id)?;
    let run = async {
        let (backend, handle) = open_sftp_for_config(state, session_config).await?;
        let entries = backend.list_dir(path).await?;
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "bye", "en")
            .await;
        Ok::<_, AppError>(entries)
    };
    let entries = match timeout(SFTP_OP_TIMEOUT, run).await {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(AppError::Ssh("list_files 执行超时".into())),
    };
    serde_json::to_string(&entries)
        .map_err(|e| AppError::Storage(format!("序列化目录列表失败: {}", e)))
}

/// 在调用方指定的服务器上列举目录（MCP 客户端直连模式）。
pub async fn list_files_direct(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    path: &str,
    state: AppState,
) -> AppResult<String> {
    if password.is_empty() {
        return Err(AppError::Auth("password 不能为空".into()));
    }
    log::info!(
        "[mcp] list_files（直连）开始：{}@{}:{} {}",
        username,
        host,
        port,
        path
    );
    let run = async {
        let handle = crate::ssh::client::connect_direct(
            host,
            port,
            username,
            "direct",
            AuthMethod::Password(password.to_string()),
            state,
        )
        .await?;
        let sftp = open_sftp(&handle).await?;
        let backend: std::sync::Arc<dyn FileBackend> = std::sync::Arc::new(sftp);
        let entries = backend.list_dir(path).await?;
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "bye", "en")
            .await;
        Ok::<_, AppError>(entries)
    };
    let entries = match timeout(SFTP_OP_TIMEOUT, run).await {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(AppError::Ssh("list_files 执行超时".into())),
    };
    serde_json::to_string(&entries)
        .map_err(|e| AppError::Storage(format!("序列化目录列表失败: {}", e)))
}

/// 在指定（按 id 查到的）SSH 会话对应的服务器上上传文件（MCP 绑定模式）。
///
/// `local_path` 必须是 X-Term 所在主机的本地路径；`remote_path` 为远端目标。
/// 进度回调为空操作（MCP 工具不向客户端推送进度事件，与 exec_ssh 一致）。
pub async fn upload_file_by_id(
    state: &AppState,
    session_id: &str,
    local_path: &str,
    remote_path: &str,
) -> AppResult<String> {
    let session_config = find_session_by_id(state, session_id)?;
    let local = PathBuf::from(local_path);
    if !local.exists() {
        return Err(AppError::InvalidInput(format!(
            "本地文件 `{}` 不存在",
            local_path
        )));
    }
    let run = async {
        let (backend, handle) = open_sftp_for_config(state, session_config).await?;
        let noop: crate::file_backend::ProgressCb = std::sync::Arc::new(|_, _| {});
        backend.upload(&local, remote_path, noop).await?;
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "bye", "en")
            .await;
        Ok::<_, AppError>(())
    };
    match timeout(SFTP_OP_TIMEOUT, run).await {
        Ok(Ok(())) => Ok(format!("已上传 {} → {}", local_path, remote_path)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(AppError::Ssh("upload_file 执行超时".into())),
    }
}

/// 在调用方指定的服务器上上传文件（MCP 客户端直连模式）。
pub async fn upload_file_direct(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    local_path: &str,
    remote_path: &str,
    state: AppState,
) -> AppResult<String> {
    if password.is_empty() {
        return Err(AppError::Auth("password 不能为空".into()));
    }
    let local = PathBuf::from(local_path);
    if !local.exists() {
        return Err(AppError::InvalidInput(format!(
            "本地文件 `{}` 不存在",
            local_path
        )));
    }
    log::info!(
        "[mcp] upload_file（直连）开始：{}@{}:{} {} → {}",
        username,
        host,
        port,
        local_path,
        remote_path
    );
    let run = async {
        let handle = crate::ssh::client::connect_direct(
            host,
            port,
            username,
            "direct",
            AuthMethod::Password(password.to_string()),
            state,
        )
        .await?;
        let sftp = open_sftp(&handle).await?;
        let backend: std::sync::Arc<dyn FileBackend> = std::sync::Arc::new(sftp);
        let noop: crate::file_backend::ProgressCb = std::sync::Arc::new(|_, _| {});
        backend.upload(&local, remote_path, noop).await?;
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "bye", "en")
            .await;
        Ok::<_, AppError>(())
    };
    match timeout(SFTP_OP_TIMEOUT, run).await {
        Ok(Ok(())) => Ok(format!("已上传 {} → {}", local_path, remote_path)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(AppError::Ssh("upload_file 执行超时".into())),
    }
}

/// 在指定（按 id 查到的）SSH 会话对应的服务器上下载文件（MCP 绑定模式）。
///
/// 下载到 X-Term 所在主机的 `local_path`，返回该本地路径。
pub async fn download_file_by_id(
    state: &AppState,
    session_id: &str,
    remote_path: &str,
    local_path: &str,
) -> AppResult<String> {
    let session_config = find_session_by_id(state, session_id)?;
    let local = PathBuf::from(local_path);
    if let Some(parent) = local.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(AppError::InvalidInput(format!(
                "本地目标目录 `{}` 不存在",
                parent.display()
            )));
        }
    }
    let run = async {
        let (backend, handle) = open_sftp_for_config(state, session_config).await?;
        let noop: crate::file_backend::ProgressCb = std::sync::Arc::new(|_, _| {});
        backend.download(remote_path, &local, noop).await?;
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "bye", "en")
            .await;
        Ok::<_, AppError>(())
    };
    match timeout(SFTP_OP_TIMEOUT, run).await {
        Ok(Ok(())) => Ok(format!("已下载 {} → {}", remote_path, local_path)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(AppError::Ssh("download_file 执行超时".into())),
    }
}

/// 在调用方指定的服务器上下载文件（MCP 客户端直连模式）。
pub async fn download_file_direct(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    remote_path: &str,
    local_path: &str,
    state: AppState,
) -> AppResult<String> {
    if password.is_empty() {
        return Err(AppError::Auth("password 不能为空".into()));
    }
    let local = PathBuf::from(local_path);
    if let Some(parent) = local.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(AppError::InvalidInput(format!(
                "本地目标目录 `{}` 不存在",
                parent.display()
            )));
        }
    }
    log::info!(
        "[mcp] download_file（直连）开始：{}@{}:{} {} → {}",
        username,
        host,
        port,
        remote_path,
        local_path
    );
    let run = async {
        let handle = crate::ssh::client::connect_direct(
            host,
            port,
            username,
            "direct",
            AuthMethod::Password(password.to_string()),
            state,
        )
        .await?;
        let sftp = open_sftp(&handle).await?;
        let backend: std::sync::Arc<dyn FileBackend> = std::sync::Arc::new(sftp);
        let noop: crate::file_backend::ProgressCb = std::sync::Arc::new(|_, _| {});
        backend.download(remote_path, &local, noop).await?;
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "bye", "en")
            .await;
        Ok::<_, AppError>(())
    };
    match timeout(SFTP_OP_TIMEOUT, run).await {
        Ok(Ok(())) => Ok(format!("已下载 {} → {}", remote_path, local_path)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(AppError::Ssh("download_file 执行超时".into())),
    }
}

// ===========================================================================
// 文件操作（File MCP · S3 账号）：list_files / upload_file / download_file
// ===========================================================================
//
// 与 SSH kind 的文件工具并列，但底层走 S3（绑定 file_account，仅 bound 模式）。
// 范式与 SSH 短连接一致：每次调用读配置 → 解密凭据 → 构造 S3Backend → 操作。
// S3 无长连接，S3Backend 构造零成本，Arc drop 即释放。

/// 按 id 查 FileAccount；不存在则报错。
fn find_file_account_by_id(state: &AppState, id: &str) -> AppResult<FileAccount> {
    let conn = state.conn()?;
    get_file_account(&conn, id)?
        .ok_or_else(|| AppError::NotFound(format!("找不到 id 为 `{}` 的 S3 文件账号", id)))
}

/// 按 id 查 S3 文件账号的展示名（供前端确认浮层标注来源；找不到时返回占位）。
pub fn account_name_by_id(state: &AppState, id: &str) -> String {
    find_file_account_by_id(state, id)
        .map(|a| a.name)
        .unwrap_or_else(|_| "(未知账号)".into())
}

/// 解析 file_account 凭据并构造 S3 后端（短连接，Arc drop 即释放）。
///
/// 流程：读 file_account → vault 解密 access_key/secret_key → S3Backend::new。
/// 与 [`open_sftp_for_config`] 的区别：无 disconnect 步骤（S3 无长连接）。
async fn open_s3_for_account(
    state: &AppState,
    account: FileAccount,
) -> AppResult<std::sync::Arc<dyn FileBackend>> {
    let cred_id = account
        .credential_id
        .as_ref()
        .ok_or_else(|| AppError::Auth(format!("文件账号 `{}` 缺少 credential_id", account.name)))?;
    let (access_key, secret_key) = {
        let vault = {
            let guard = state.vault_read()?;
            guard
                .as_ref()
                .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
                .clone()
        };
        let conn = state.conn()?;
        fetch_s3_credential(&conn, cred_id, &vault)?
    };
    let config = S3Config {
        endpoint: account.endpoint,
        region: account.region,
        bucket: account.bucket,
        access_key,
        secret_key,
        path_style: account.path_style,
    };
    let backend = S3Backend::new(config)?;
    Ok(std::sync::Arc::new(backend))
}

/// 在指定（按 id 查到的）S3 账号对应的存储桶上列举目录（File MCP 绑定模式）。
///
/// `path` 为对象 key 前缀（约定以 `/` 结尾表示目录）。返回 JSON 序列化的 `Vec<FileEntry>`。
pub async fn list_files_by_account(
    state: &AppState,
    account_id: &str,
    path: &str,
) -> AppResult<String> {
    let account = find_file_account_by_id(state, account_id)?;
    let run = async {
        let backend = open_s3_for_account(state, account).await?;
        let entries = backend.list_dir(path).await?;
        Ok::<_, AppError>(entries)
    };
    let entries = match timeout(SFTP_OP_TIMEOUT, run).await {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(AppError::Storage(format!(
                "list_files(S3) 执行超时（目录 `{}`，上限 5 分钟）",
                path
            )))
        }
    };
    serde_json::to_string(&entries)
        .map_err(|e| AppError::Storage(format!("序列化目录列表失败: {}", e)))
}

/// 上传本地文件到指定 S3 账号对应的存储桶（File MCP 绑定模式）。
pub async fn upload_file_by_account(
    state: &AppState,
    account_id: &str,
    local_path: &str,
    remote_path: &str,
) -> AppResult<String> {
    let account = find_file_account_by_id(state, account_id)?;
    let local = PathBuf::from(local_path);
    if !local.exists() {
        return Err(AppError::InvalidInput(format!(
            "本地文件 `{}` 不存在",
            local_path
        )));
    }
    let run = async {
        let backend = open_s3_for_account(state, account).await?;
        let noop: crate::file_backend::ProgressCb = std::sync::Arc::new(|_, _| {});
        backend.upload(&local, remote_path, noop).await?;
        Ok::<_, AppError>(())
    };
    match timeout(SFTP_OP_TIMEOUT, run).await {
        Ok(Ok(())) => Ok(format!("已上传 {} → {}", local_path, remote_path)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(AppError::Storage(format!(
            "upload_file(S3) 执行超时（`{}` → `{}`，上限 5 分钟；文件可能过大）",
            local_path, remote_path
        ))),
    }
}

/// 从指定 S3 账号对应的存储桶下载文件到本地（File MCP 绑定模式）。
pub async fn download_file_by_account(
    state: &AppState,
    account_id: &str,
    remote_path: &str,
    local_path: &str,
) -> AppResult<String> {
    let account = find_file_account_by_id(state, account_id)?;
    let local = PathBuf::from(local_path);
    if let Some(parent) = local.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(AppError::InvalidInput(format!(
                "本地目标目录 `{}` 不存在",
                parent.display()
            )));
        }
    }
    let run = async {
        let backend = open_s3_for_account(state, account).await?;
        let noop: crate::file_backend::ProgressCb = std::sync::Arc::new(|_, _| {});
        backend.download(remote_path, &local, noop).await?;
        Ok::<_, AppError>(())
    };
    match timeout(SFTP_OP_TIMEOUT, run).await {
        Ok(Ok(())) => Ok(format!("已下载 {} → {}", remote_path, local_path)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(AppError::Storage(format!(
            "download_file(S3) 执行超时（`{}` → `{}`，上限 5 分钟；文件可能过大）",
            remote_path, local_path
        ))),
    }
}
