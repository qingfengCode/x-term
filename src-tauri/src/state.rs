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
use crate::local::LocalSession;
use crate::ssh::client::ClientHandler;
use crate::ssh::session::SshSession;
use crate::ssh::sftp::SftpSession;
use crate::ssh::tunnel::Tunnel;
use crate::storage::db::DbPool;
use crate::storage::secure::CredentialVault;
use crate::telnet::TelnetSession;

/// 终端会话统一枚举（SSH / Telnet / 本地 shell 共用同一 terminals map）。
/// terminal_write / terminal_resize / disconnect_session 按 variant 分派。
pub enum TerminalSession {
    Ssh(SshSession),
    Telnet(TelnetSession),
    Local(LocalSession),
}

impl TerminalSession {
    pub fn write(&self, data: Vec<u8>) -> AppResult<()> {
        match self {
            TerminalSession::Ssh(s) => s.write(data),
            TerminalSession::Telnet(s) => s.write(data),
            TerminalSession::Local(s) => s.write(data),
        }
    }

    /// 带写完成确认的写入（背压用）。
    ///
    /// SSH / 本地会话返回后台任务的写确认 receiver；Telnet 写是同步的，
    /// 直接返回已完成的 receiver。见 [`SshSession::write_with_ack`]。
    pub fn write_with_ack(&self, data: Vec<u8>) -> AppResult<tokio::sync::oneshot::Receiver<()>> {
        match self {
            TerminalSession::Ssh(s) => s.write_with_ack(data),
            TerminalSession::Local(s) => s.write_with_ack(data),
            TerminalSession::Telnet(s) => {
                s.write(data)?;
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = tx.send(());
                Ok(rx)
            }
        }
    }
    pub fn resize(&self, cols: u32, rows: u32) -> AppResult<()> {
        match self {
            TerminalSession::Ssh(s) => s.resize(cols, rows),
            TerminalSession::Telnet(s) => s.resize(cols, rows),
            TerminalSession::Local(s) => s.resize(cols, rows),
        }
    }
    pub fn snapshot(&self, max_bytes: usize) -> String {
        match self {
            TerminalSession::Ssh(s) => s.snapshot(max_bytes),
            TerminalSession::Telnet(s) => s.snapshot(max_bytes),
            TerminalSession::Local(s) => s.snapshot(max_bytes),
        }
    }
    /// 取终端输出的完整快照（整个环形缓冲），供 AI 可视化命令哨兵检测/截取。
    pub fn full_snapshot(&self) -> String {
        match self {
            TerminalSession::Ssh(s) => s.full_snapshot(),
            TerminalSession::Telnet(s) => s.full_snapshot(),
            TerminalSession::Local(s) => s.full_snapshot(),
        }
    }
    /// 取原始字节快照 + 累计字节数（terminal_attach 命令用，前端 attach 回放）。
    pub fn attach_snapshot(&self) -> (Vec<u8>, usize) {
        match self {
            TerminalSession::Ssh(s) => s.attach_snapshot(),
            TerminalSession::Telnet(s) => s.attach_snapshot(),
            TerminalSession::Local(s) => s.attach_snapshot(),
        }
    }
    /// 累计写入字节数（不受环形截断影响），判断"输出是否仍在增长"用。
    pub fn total_output_bytes(&self) -> usize {
        match self {
            TerminalSession::Ssh(s) => s.total_output_bytes(),
            TerminalSession::Telnet(s) => s.total_output_bytes(),
            TerminalSession::Local(s) => s.total_output_bytes(),
        }
    }
    pub fn output_offset(&self) -> usize {
        match self {
            TerminalSession::Ssh(s) => s.output_offset(),
            TerminalSession::Telnet(s) => s.output_offset(),
            TerminalSession::Local(s) => s.output_offset(),
        }
    }
    /// 取 `since_total` 累计字节之后的输出窗口（AI 可视化命令截取新增输出用）。
    /// 返回 (文本, 是否溢出窗口容量)。
    pub fn snapshot_after(&self, since_total: usize) -> (String, bool) {
        match self {
            TerminalSession::Ssh(s) => s.snapshot_after(since_total),
            TerminalSession::Telnet(s) => s.snapshot_after(since_total),
            TerminalSession::Local(s) => s.snapshot_after(since_total),
        }
    }
    pub fn id(&self) -> &str {
        match self {
            TerminalSession::Ssh(s) => &s.id,
            TerminalSession::Telnet(s) => &s.id,
            TerminalSession::Local(s) => &s.id,
        }
    }
    pub fn session_config_id(&self) -> &str {
        match self {
            TerminalSession::Ssh(s) => &s.session_config_id,
            TerminalSession::Telnet(s) => &s.session_config_id,
            // 本地 shell 不关联任何会话配置。
            TerminalSession::Local(_) => "",
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

    /// 进行中的 SFTP 传输任务：taskId -> 取消标志（前端"取消"按钮置位，
    /// 传输循环在下一个块边界退出并清理半截文件）。任务结束（完成/失败/
    /// 取消）后由命令层移除。
    pub sftp_transfers:
        Arc<Mutex<HashMap<String, crate::ssh::sftp::CancelFlag>>>,

    /// 正在运行的端口转发隧道：tunnelId -> Tunnel。
    pub tunnels: Arc<Mutex<HashMap<String, Tunnel>>>,

    /// 已打开的文件后端连接（S3 等）：accountId -> (backendId, Arc<dyn FileBackend>)。
    ///
    /// 按 account_id 索引：S3 等后端无状态、可安全复用，重复 connect 同一账号返回同一
    /// backendId，避免泄漏。`backendId` 为 uuid 字符串，前端文件操作时持有。
    /// 删除账号时按 account_id 精确清理。SFTP 会话独立存放在 `sftp_sessions`。
    pub file_backends:
        Arc<Mutex<HashMap<String, (String, std::sync::Arc<dyn crate::file_backend::FileBackend>)>>>,

    /// 已建立的 MySQL 业务连接：connId -> Arc<MySqlConn>。
    ///
    /// 用 `Arc` 而非裸值：命令按需 `get().cloned()` 出句柄后释放锁再跨 await 使用，
    /// 多个命令可并发操作同一连接（过去 remove/insert 模式在并发下会 NotFound 竞争，
    /// 导致前端展开库/执行 SQL 偶发失败）。
    pub mysql_conns: Arc<
        Mutex<HashMap<String, std::sync::Arc<crate::database::mysql::MySqlConn>>>,
    >,

    /// 待确认执行的 AI 工具调用：toolCallId -> (requestId, oneshot 发送端)。
    ///
    /// AI 编排循环发起 tool_call 后阻塞在此等待前端确认；前端通过
    /// `ai_execute_tool` / `ai_cancel_tool` 命令把结果发回。附带 requestId，
    /// 使 `ai_stop` 能精确清理**属于被终止请求**的确认项，不误伤并发会话。
    pub pending_tool_calls: Arc<
        Mutex<
            HashMap<
                String,
                (
                    String,
                    tokio::sync::oneshot::Sender<crate::ai::tools::ToolApproval>,
                ),
            >,
        >,
    >,

    /// 待前端执行的 AI 桌面工具（desktop_*，RDP 控制）：toolCallId -> (requestId, oneshot)。
    ///
    /// RDP 会话（IronRDP WASM）活在前端，桌面工具的后端侧无法执行——编排循环 emit
    /// `ai:tool_call` 后阻塞在此，等待前端通过 `ai_desktop_tool_respond` 回传
    /// 「批准 + 执行结果」一体的回执。附带 requestId，使 `ai_stop` 能精确清理
    /// **属于被终止请求**的等待项（仿照 [`Self::pending_tool_calls`]）。
    pub pending_desktop_calls: Arc<
        Mutex<
            HashMap<
                String,
                (
                    String,
                    tokio::sync::oneshot::Sender<crate::ai::tools::DesktopToolOutcome>,
                ),
            >,
        >,
    >,

    /// 待前端回答的 AI 提问（ask_user_question）：toolCallId -> (requestId, oneshot)。
    ///
    /// 提问的执行体在**前端**（用户填写回答）——编排循环 emit `ai:tool_call` 后
    /// 阻塞在此，等待前端通过 `ai_ask_user_respond` 命令回传答案。附带 requestId，
    /// 使 `ai_stop` 能精确清理**属于被终止请求**的等待项（仿照
    /// [`Self::pending_desktop_calls`]）。
    pub pending_ask_user_calls: Arc<
        Mutex<
            HashMap<
                String,
                (
                    String,
                    tokio::sync::oneshot::Sender<crate::ai::tools::AskUserOutcome>,
                ),
            >,
        >,
    >,

    /// 正在运行的 AI 请求后台任务：requestId -> JoinHandle。
    ///
    /// `ai_chat` spawn 时登记，`ai_stop` 取出 handle 调 `abort()` 终止；
    /// 任务自然结束时由其自身清理（run_agent_loop 返回后 remove）。
    pub pending_ai_tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,

    /// 待用户填写的 SSH 二次认证挑战：challengeId -> oneshot 发送端。
    ///
    /// keyboard-interactive 认证过程中后端 emit `ssh:auth_challenge` 后阻塞在此,
    /// 等待前端通过 `ssh_auth_respond` 命令回传答案（仿照 [`Self::pending_tool_calls`]）。
    pub pending_auth_challenges: Arc<
        Mutex<
            HashMap<String, tokio::sync::oneshot::Sender<crate::ssh::client::AuthChallengeReply>>,
        >,
    >,

    /// 待用户确认的 SSH 主机公钥变更：challengeId -> oneshot 发送端。
    ///
    /// [`crate::ssh::client::ClientHandler::check_server_key`] 在检测到主机公钥
    /// 与 known_hosts 记录不符时，emit `ssh:host_key_challenge` 后阻塞在此，
    /// 等待前端通过 `ssh_host_key_respond` 命令回传决策（接受并更新 / 仅本次接受 / 拒绝）。
    pub pending_host_keys: Arc<
        Mutex<HashMap<String, tokio::sync::oneshot::Sender<crate::ssh::client::HostKeyDecision>>>,
    >,

    /// Tauri 应用句柄（事件发射、日志等）。
    pub app: tauri::AppHandle,

    /// MCP 人工确认注册表（exec_ssh / exec_sql 必须经前端确认）。
    ///
    /// 外部 MCP 客户端发起工具调用时，MCP 服务端在此登记一个 oneshot，
    /// 阻塞等待前端通过 `mcp_respond_approval` 回结果。详见
    /// [`crate::mcp::approval`]。
    pub approval_registry: Arc<crate::mcp::approval::ApprovalRegistry>,

    /// 内嵌 VNC 查看器的 WebSocket↔TCP 桥接实例：instanceId -> VncBridge。
    ///
    /// `vnc_bridge_start` 启动时登记，`vnc_bridge_stop` 移除（Drop 兜底回收）。
    pub vnc_bridges: Arc<Mutex<HashMap<String, crate::vnc::VncBridge>>>,

    /// 内嵌 RDP 客户端的迷你网关桥接实例：instanceId -> RdpBridge。
    ///
    /// `rdp_bridge_start` 启动时登记，`rdp_bridge_stop` 移除（Drop 兜底回收）。
    pub rdp_bridges: Arc<Mutex<HashMap<String, crate::rdp::RdpBridge>>>,

    /// 正被 MCP「终端绑定」/ AI 可视化执行占用的终端实例 id（并发保护）。
    ///
    /// 同一终端标签页同一时刻只允许一个调用写入 PTY：多个并发写会导致输出
    /// 交叉、哨兵检测互相干扰。占用必须经 [`TerminalBusyGuard`] 释放——它的
    /// Drop 保证 abort（ai_stop 终止 AI 请求会直接丢弃整个 future 树）与
    /// panic 路径也能释放，否则占用永久残留、该终端后续所有命令永远报
    /// "正被占用"，只能重启应用。
    pub mcp_terminal_busy: Arc<Mutex<HashMap<String, ()>>>,

    /// settings.json 的路径（缓存的快捷访问）。
    pub settings_path: Arc<PathBuf>,

    /// settings.json 的内存缓存：首次读取后驻留，`settings_save` 时失效。
    ///
    /// 终端/SFTP/隧道/AI 每次建立连接都会读设置（空闲断开、保活等），此前每次
    /// 都同步读盘 + 反序列化，会话并发时在 tokio worker 上造成 IO 抖动。
    pub settings_cache: Arc<parking_lot::RwLock<Option<crate::config::Settings>>>,
}

/// 终端忙锁（[`AppState::mcp_terminal_busy`]）的自动释放守卫。
///
/// 占用必须在调用结束时释放。手工在函数末尾 `remove` 的缺陷：调用方的
/// future 可能被 **abort**（AI 请求点"终止"→ `ai_stop` → `join.abort()` 会
/// 在 await 点直接丢弃整个 future 树；MCP 请求取消同理）——末尾代码永远
/// 不执行，占用永久残留，该终端后续所有 AI 可视化命令 / MCP 终端命令
/// 永远报"该终端正被占用"，只能重启应用。Drop 守卫保证任何退出路径
/// （正常返回、错误、超时、abort、panic）都释放占用。
///
/// 锁用 parking_lot::Mutex：临界区只有 HashMap insert/remove（持锁期间无
/// await），同步锁即可；parking_lot 无中毒语义，Drop 内 lock() 恒可用。
pub struct TerminalBusyGuard {
    map: Arc<Mutex<HashMap<String, ()>>>,
    session_id: String,
}

impl Drop for TerminalBusyGuard {
    fn drop(&mut self) {
        self.map.lock().remove(&self.session_id);
    }
}

impl AppState {
    /// 尝试占用指定终端（忙锁）。已占用返回 `None`（调用方给"请稍后再试"类
    /// 引导）；成功返回 Drop 自动释放的守卫——**必须绑定到变量**（如
    /// `let _guard = ...`）持有到调用结束，`_`（不绑定）会立即 drop 失效。
    pub fn try_lock_terminal(&self, session_id: &str) -> Option<TerminalBusyGuard> {
        {
            let mut busy = self.mcp_terminal_busy.lock();
            if busy.insert(session_id.to_string(), ()).is_some() {
                return None;
            }
        }
        Some(TerminalBusyGuard {
            map: self.mcp_terminal_busy.clone(),
            session_id: session_id.to_string(),
        })
    }
}

impl AppState {
    /// 构造初始状态。
    pub fn new(
        data_dir: PathBuf,
        db: DbPool,
        settings_path: PathBuf,
        app: tauri::AppHandle,
    ) -> Self {
        Self {
            data_dir: Arc::new(data_dir),
            db: Arc::new(db),
            vault: Arc::new(RwLock::new(None)),
            terminals: Arc::new(Mutex::new(HashMap::new())),
            sftp_sessions: Arc::new(Mutex::new(HashMap::new())),
            sftp_transfers: Arc::new(Mutex::new(HashMap::new())),
            tunnels: Arc::new(Mutex::new(HashMap::new())),
            file_backends: Arc::new(Mutex::new(HashMap::new())),
            mysql_conns: Arc::new(Mutex::new(HashMap::new())),
            pending_tool_calls: Arc::new(Mutex::new(HashMap::new())),
            pending_desktop_calls: Arc::new(Mutex::new(HashMap::new())),
            pending_ask_user_calls: Arc::new(Mutex::new(HashMap::new())),
            pending_ai_tasks: Arc::new(Mutex::new(HashMap::new())),
            pending_auth_challenges: Arc::new(Mutex::new(HashMap::new())),
            pending_host_keys: Arc::new(Mutex::new(HashMap::new())),
            app,
            approval_registry: Arc::new(crate::mcp::approval::ApprovalRegistry::new()),
            vnc_bridges: Arc::new(Mutex::new(HashMap::new())),
            rdp_bridges: Arc::new(Mutex::new(HashMap::new())),
            mcp_terminal_busy: Arc::new(Mutex::new(HashMap::new())),
            settings_path: Arc::new(settings_path),
            settings_cache: Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    /// 从池中获取一个数据库连接。
    pub fn conn(&self) -> AppResult<crate::storage::db::DbConn> {
        self.db
            .get()
            .map_err(|e| AppError::Storage(format!("无法获取数据库连接: {}", e)))
    }

    /// 设置保险库（解锁/创建后调用）。
    pub fn set_vault(&self, vault: CredentialVault) {
        *self.vault.write() = Some(vault);
    }

    /// 获取保险库引用（如果已解锁）。
    ///
    /// 返回的 `RwLockReadGuard` 用于在凭据解析期间持有读锁；为避免在异步等待中
    /// 长时间持锁，调用方应在使用前先把所需数据 clone 出来。
    pub fn vault_read(
        &self,
    ) -> AppResult<parking_lot::RwLockReadGuard<'_, Option<CredentialVault>>> {
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
