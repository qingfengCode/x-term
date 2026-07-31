//! 应用全局状态。
//!
//! [`AppState`] 通过 Tauri 的 [`manage`](tauri::Builder::manage) 注入到运行时，
//! 所有 `#[tauri::command]` 通过 `State<'_, AppState>` 拿到共享状态。
//!
//! 内部状态分三类：
//! - 持久化层：数据库连接池 [`DbPool`]、配置 JSON 文件路径、加密凭据保险库。
//! - 运行时连接：已打开的 SSH 终端会话（终端 tab）、SFTP 会话、端口转发隧道。
//! - AI：模型 provider 配置（从 settings.json 读取后缓存）。
//!
//! 所有共享可变结构使用 [`parking_lot::RwLock`]（读多写少）或 [`Mutex`](parking_lot::Mutex)。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::error::{AppError, AppResult};
use crate::ssh::client::ClientHandler;
use crate::ssh::session::SshSession;
use crate::ssh::sftp::SftpSession;
use crate::ssh::tunnel::Tunnel;
use crate::storage::db::DbPool;
use crate::storage::secure::CredentialVault;
use crate::telnet::TelnetSession;

/// 终端会话统一枚举（SSH / Telnet 共用同一 terminals map）。
/// terminal_write / terminal_resize / disconnect_session 按 variant 分派。
pub enum TerminalSession {
    Ssh(SshSession),
    Telnet(TelnetSession),
}

impl TerminalSession {
    pub fn write(&self, data: Vec<u8>) -> AppResult<()> {
        match self {
            TerminalSession::Ssh(s) => s.write(data),
            TerminalSession::Telnet(s) => s.write(data),
        }
    }
    pub fn resize(&self, cols: u32, rows: u32) -> AppResult<()> {
        match self {
            TerminalSession::Ssh(s) => s.resize(cols, rows),
            TerminalSession::Telnet(s) => s.resize(cols, rows),
        }
    }
    pub fn snapshot(&self, max_bytes: usize) -> String {
        match self {
            TerminalSession::Ssh(s) => s.snapshot(max_bytes),
            TerminalSession::Telnet(s) => s.snapshot(max_bytes),
        }
    }
    pub fn output_offset(&self) -> usize {
        match self {
            TerminalSession::Ssh(s) => s.output_offset(),
            TerminalSession::Telnet(s) => s.output_offset(),
        }
    }
    pub fn id(&self) -> &str {
        match self {
            TerminalSession::Ssh(s) => &s.id,
            TerminalSession::Telnet(s) => &s.id,
        }
    }
    pub fn session_config_id(&self) -> &str {
        match self {
            TerminalSession::Ssh(s) => &s.session_config_id,
            TerminalSession::Telnet(s) => &s.session_config_id,
        }
    }
}

/// SSH client handle（russh 的 `Handle` 未实现 `Clone`，故用 `Arc` 共享）。
pub type SharedHandle = std::sync::Arc<russh::client::Handle<ClientHandler>>;

/// 应用全局状态。
///
/// 字段均为 `Arc<RwLock<_>>` / `Arc<Mutex<_>>`，使得 [`AppState`] 本身可以廉价地
/// 被 `Clone`（Tauri 内部会克隆），但内部数据共享同一份。
#[derive(Clone)]
pub struct AppState {
    /// 应用数据目录（如 `%APPDATA%/x-term`）。
    pub data_dir: Arc<PathBuf>,

    /// SQLite 连接池。
    pub db: Arc<DbPool>,

    /// 凭据保险库（首次解锁后填充）。
    ///
    /// 应用启动时若保险库已存在但尚未解锁，此处为 `None`；用户输入主密码后调用
    /// [`AppState::unlock_vault`] 填充。
    pub vault: Arc<RwLock<Option<CredentialVault>>>,

    /// 已打开的终端会话：sessionId -> TerminalSession（SSH/Telnet，每个终端 tab 一份）。
    pub terminals: Arc<Mutex<HashMap<String, TerminalSession>>>,

    /// 已打开的 SFTP 会话：sftpId -> (Arc<SftpSession>, 关闭用的 SharedHandle)。
    ///
    /// SFTP 会话与终端会话独立，可以单独打开；为支持多个 SFTP tab 同时浏览，
    /// 这里按独立的 sftpId 维护。`Arc<SftpSession>` 因为下载/上传是长任务，
    /// 需要在不长时间持锁的情况下取出引用。`SharedHandle` 用于关闭时 disconnect。
    pub sftp_sessions: Arc<Mutex<HashMap<String, (std::sync::Arc<SftpSession>, SharedHandle)>>>,

    /// 正在运行的端口转发隧道：tunnelId -> Tunnel。
    pub tunnels: Arc<Mutex<HashMap<String, Tunnel>>>,

    /// 已建立的 MySQL 业务连接：connId -> MySqlConn。
    pub mysql_conns: Arc<Mutex<HashMap<String, crate::database::mysql::MySqlConn>>>,

    /// 待确认执行的 AI 工具调用：toolCallId -> oneshot 发送端。
    ///
    /// AI 编排循环发起 tool_call 后阻塞在此等待前端确认；前端通过
    /// `ai_execute_tool` / `ai_cancel_tool` 命令把结果发回。
    pub pending_tool_calls:
        Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<crate::ai::tools::ToolApproval>>>>,

    /// 正在运行的 AI 请求后台任务：requestId -> JoinHandle。
    ///
    /// `ai_chat` spawn 时登记，`ai_stop` 取出 handle 调 `abort()` 终止；
    /// 任务自然结束时由其自身清理（run_agent_loop 返回后 remove）。
    pub pending_ai_tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,

    /// MCP 人工确认注册表（exec_ssh / exec_sql 必须经前端确认）。
    ///
    /// 外部 MCP 客户端发起工具调用时，MCP 服务端在此登记一个 oneshot，
    /// 阻塞等待前端通过 `mcp_respond_approval` 回结果。详见
    /// [`crate::mcp::approval`]。
    pub approval_registry: Arc<crate::mcp::approval::ApprovalRegistry>,

    /// settings.json 的路径（缓存的快捷访问）。
    pub settings_path: Arc<PathBuf>,
}

impl AppState {
    /// 构造初始状态。
    pub fn new(data_dir: PathBuf, db: DbPool, settings_path: PathBuf) -> Self {
        Self {
            data_dir: Arc::new(data_dir),
            db: Arc::new(db),
            vault: Arc::new(RwLock::new(None)),
            terminals: Arc::new(Mutex::new(HashMap::new())),
            sftp_sessions: Arc::new(Mutex::new(HashMap::new())),
            tunnels: Arc::new(Mutex::new(HashMap::new())),
            mysql_conns: Arc::new(Mutex::new(HashMap::new())),
            pending_tool_calls: Arc::new(Mutex::new(HashMap::new())),
            pending_ai_tasks: Arc::new(Mutex::new(HashMap::new())),
            approval_registry: Arc::new(crate::mcp::approval::ApprovalRegistry::new()),
            settings_path: Arc::new(settings_path),
        }
    }

    /// 从池中获取一个数据库连接。
    pub fn conn(&self) -> AppResult<crate::storage::db::DbConn> {
        self.db.get().map_err(|e| {
            AppError::Storage(format!("无法获取数据库连接: {}", e))
        })
    }

    /// 设置保险库（解锁/创建后调用）。
    pub fn set_vault(&self, vault: CredentialVault) {
        *self.vault.write() = Some(vault);
    }

    /// 获取保险库引用（如果已解锁）。
    ///
    /// 返回的 `RwLockReadGuard` 用于在凭据解析期间持有读锁；为避免在异步等待中
    /// 长时间持锁，调用方应在使用前先把所需数据 clone 出来。
    pub fn vault_read(&self) -> AppResult<parking_lot::RwLockReadGuard<'_, Option<CredentialVault>>> {
        let guard = self.vault.read();
        if guard.is_none() {
            return Err(AppError::Auth("凭据保险库尚未解锁，请先输入主密码".into()));
        }
        Ok(guard)
    }

    /// 保险库是否已就绪（解锁或创建过）。
    pub fn vault_ready(&self) -> bool {
        self.vault.read().is_some()
    }
}
