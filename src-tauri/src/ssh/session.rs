//! 单个交互式 SSH 终端会话的封装。
//!
//! [`SshSession`] 把一次"连接 + 认证 + 打开 PTY + 启动 shell"的完整流程
//! 封装成一个对象，前端每开一个终端 tab 就对应一个 [`SshSession`]。
//!
//! ## 输入/输出与线程模型
//!
//! 远程 shell 的输出由 [`SshSession::spawn_reader`] 起一个后台 tokio 任务
//! 持续读取（循环 [`Channel::wait`]），并以 [`crate::events::TERMINAL_DATA`]
//! 事件的形式推送到前端。
//!
//! 由于 russh 的 [`Channel`] 在被 `wait()` 读取时需要独占所有权，前端输入
//! （键盘按键）和窗口大小变化不能直接复用 channel。因此引入一对
//! [`tokio::sync::mpsc`] 通道：
//! - [`InputMsg`]：写入数据或调整尺寸；
//! - reader 任务在 `select!` 中同时监听 channel 输出与输入通道，
//!   收到输入时调用 `channel.data()` / `channel.window_change()`。
//!
//! [`SshSession::write`] / [`SshSession::resize`] 实际只是把请求塞进 mpsc，
//! 真正的写操作发生在 reader 任务里。
//!
//! ## 凭据解析
//!
//! [`resolve_credential`] 从数据库的 `credentials` 表中取出加密 blob，用
//! [`crate::storage::secure::CredentialVault`] 解密，再根据会话配置的
//! [`AuthType`](crate::storage::sessions_repo::AuthType) 构造出
//! [`crate::ssh::client::AuthMethod`]。

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use russh::client::{Handle, Msg};
use russh::{Channel, ChannelMsg, Disconnect};
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{mpsc, oneshot};

// ===========================================================================
// 输出环形缓冲（用于 AI 终端上下文感知）
// ===========================================================================

/// 终端输出的环形字节缓冲。
///
/// reader 任务把远程 shell 的输出持续追加进来，仅保留最近的 N 字节，
/// 用于"终端上下文感知"——AI 可通过 [`SshSession::snapshot`] 取出最近输出
/// 作为分析上下文。
pub struct OutputRing {
    /// 容量上限（字节）。
    cap: usize,
    /// 当前缓冲。
    buf: VecDeque<u8>,
    /// 累计写入字节数（不受环形截断影响，用于判断"终端输出是否仍在增长"）。
    total: usize,
}

impl OutputRing {
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            cap: cap_bytes,
            buf: VecDeque::with_capacity(cap_bytes),
            total: 0,
        }
    }

    /// 追加字节，超出容量时从头丢弃。
    pub fn push(&mut self, data: &[u8]) {
        for &b in data {
            if self.buf.len() >= self.cap {
                self.buf.pop_front();
            }
            self.buf.push_back(b);
        }
        self.total += data.len();
    }

    /// 当前缓冲字节数（用于记录"命令执行前"的基准位置）。
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// 累计写入的字节数（含已被环形截断丢弃的头部）。
    ///
    /// 与 [`Self::len`] 的区别：缓冲写满后 `len` 恒等于容量，无法反映持续
    /// 到来的新输出；`total` 单调递增，适合做"输出是否还在增长"的检测。
    pub fn total_bytes(&self) -> usize {
        self.total
    }

    /// 取最近 `max_bytes` 字节的快照（UTF-8 lossy 转 String）。
    pub fn snapshot(&self, max_bytes: usize) -> String {
        let take = max_bytes.min(self.buf.len());
        let start = self.buf.len() - take;
        let slice: Vec<u8> = self.buf.iter().copied().skip(start).collect();
        String::from_utf8_lossy(&slice).into_owned()
    }

    /// 取 `since_total` 累计字节之后仍留在缓冲中的输出（UTF-8 lossy 转 String）。
    ///
    /// 与 [`Self::snapshot`] 的区别：按**累计写入位置**切窗口，而不是"最近 N 字节"。
    /// 用于截取"命令执行期间产生的新输出"——调用方在写入命令前记录
    /// [`Self::total_bytes`]，命令结束后取窗口即可拿到本次新增输出，不受缓冲中
    /// 历史内容（上一轮命令输出、登录横幅、提示符等）与终端折行的干扰。
    ///
    /// 返回 `(文本, 是否溢出)`：窗口字节数超过环形容量时，窗口头部已被环形
    /// 截断丢弃，溢出为 `true`（此时文本只剩窗口尾部）。调用方应把溢出情况
    /// 告知模型，避免其把残缺输出当作完整结果。
    pub fn snapshot_after(&self, since_total: usize) -> (String, bool) {
        let window = self.total.saturating_sub(since_total);
        let kept = window.min(self.buf.len());
        let truncated = kept < window;
        let start = self.buf.len() - kept;
        let slice: Vec<u8> = self.buf.iter().copied().skip(start).collect();
        (String::from_utf8_lossy(&slice).into_owned(), truncated)
    }
}

/// 进程内共享的输出缓冲类型。
pub type SharedOutputRing = Arc<StdMutex<OutputRing>>;

use crate::error::{AppError, AppResult};
use crate::events::{
    self, TerminalClosedEvent, TerminalDataEvent, TerminalExitEvent, TERMINAL_CLOSED,
    TERMINAL_DATA, TERMINAL_EXIT,
};
use crate::ssh::client::{self, AuthMethod, ClientHandler};
use crate::state::AppState;
use crate::storage::db::DbConn;
use crate::storage::secure::CredentialVault;
use crate::storage::sessions_repo::{AuthType, Session};

// ===========================================================================
// 凭据
// ===========================================================================

/// 已解析好的认证凭据。
///
/// 由 [`resolve_credential`] 产出，包含可直接交给 [`client::connect_direct`]
/// 使用的 [`AuthMethod`]。
pub struct ResolvedCredential {
    pub auth_method: AuthMethod,
}

// 手写 Debug：内容依赖 [`AuthMethod`]（已脱敏），此处仅为说明安全边界保留手写实现。
impl std::fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("auth_method", &self.auth_method)
            .finish()
    }
}

/// `credentials.enc_data` 解密后的明文 JSON 结构。
///
/// 约定字段：
/// - `kind`: `"password"` 表示密码；`"private_key_text"` 表示私钥文本。
/// - `value`: 实际内容（密码字符串或 PEM/OpenSSH 私钥文本）。
/// - `passphrase`: 私钥被加密时所需的口令，密码类型忽略。
#[derive(Deserialize)]
struct CredentialData {
    kind: String,
    value: String,
    #[serde(default)]
    passphrase: Option<String>,
}

// 手写 Debug：`value` / `passphrase` 为明文机密，脱敏后仅展示 kind。
impl std::fmt::Debug for CredentialData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialData")
            .field("kind", &self.kind)
            .field("value", &"***")
            .field("passphrase", &self.passphrase.as_ref().map(|_| "***"))
            .finish()
    }
}

/// 根据会话配置解析出 [`ResolvedCredential`]。
///
/// 解析逻辑：
/// - [`AuthType::Password`]：从 `credentials` 表按 `credential_id` 取出加密
///   blob，解密为 [`CredentialData`]，`value` 字段即为密码。
/// - [`AuthType::PrivateKey`]：优先使用会话配置中的 `key_path`（私钥文件路径）；
///   若未设置则回退到凭据中的私钥文本（`kind = "private_key_text"`）。
///   带 passphrase 时一并传入。
///
/// # 参数
/// - `session_config`: 会话配置。
/// - `vault`: 已解锁的凭据保险库。
/// - `conn`: 数据库连接。
///
/// 注意：本函数内部全部是同步操作（DB 查询、解密、私钥解析），故为同步函数，
/// 便于在 async 命令中调用而不引入非 Send 的引用。
pub fn resolve_credential(
    session_config: &Session,
    vault: &CredentialVault,
    conn: &DbConn,
) -> AppResult<ResolvedCredential> {
    match session_config.auth_type {
        AuthType::Password => {
            let cred_id = session_config
                .credential_id
                .as_ref()
                .ok_or_else(|| AppError::Auth("密码认证缺少 credential_id".into()))?;
            let enc_data = fetch_enc_data(conn, cred_id)?;
            let blob = CredentialVault::decode_blob(&enc_data)?;
            let plain = vault.decrypt_str(&blob)?;
            let data: CredentialData = serde_json::from_str(&plain)?;
            if data.kind != "password" {
                return Err(AppError::Auth(format!(
                    "凭据类型不匹配：期望 password，实际 {}",
                    data.kind
                )));
            }
            Ok(ResolvedCredential {
                auth_method: AuthMethod::password(data.value),
            })
        }
        AuthType::PrivateKey => {
            // 优先用文件路径。
            if let Some(path) = &session_config.key_path {
                let key = client::load_private_key(path, None)?;
                return Ok(ResolvedCredential {
                    auth_method: AuthMethod::PrivateKey {
                        key_data: key,
                        passphrase: None,
                    },
                });
            }
            // 回退到凭据中的私钥文本。
            let cred_id = session_config
                .credential_id
                .as_ref()
                .ok_or_else(|| AppError::Auth("密钥认证缺少 credential_id".into()))?;
            let enc_data = fetch_enc_data(conn, cred_id)?;
            let blob = CredentialVault::decode_blob(&enc_data)?;
            let plain = vault.decrypt_str(&blob)?;
            let data: CredentialData = serde_json::from_str(&plain)?;
            if data.kind != "private_key_text" {
                return Err(AppError::Auth(format!(
                    "凭据类型不匹配：期望 private_key_text，实际 {}",
                    data.kind
                )));
            }
            let key = client::decode_private_key(&data.value, data.passphrase.as_deref())?;
            Ok(ResolvedCredential {
                auth_method: AuthMethod::PrivateKey {
                    key_data: key,
                    passphrase: data.passphrase.clone(),
                },
            })
        }
    }
}

/// 从 `credentials` 表取出指定 id 的加密 blob 字符串。
fn fetch_enc_data(conn: &DbConn, id: &str) -> AppResult<String> {
    // query_row 在找不到行时返回 rusqlite::Error::QueryReturnedNoRows，
    // 检测该错误并转成 NotFound。
    match conn.query_row(
        "SELECT enc_data FROM credentials WHERE id = ?1",
        [id],
        |r| r.get::<_, String>(0),
    ) {
        Ok(s) => Ok(s),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            Err(AppError::NotFound(format!("凭据 {} 不存在", id)))
        }
        Err(e) => Err(e.into()),
    }
}

// ===========================================================================
// 输入消息（mpsc）
// ===========================================================================

/// 前端 → reader 任务 的输入指令。
enum InputMsg {
    /// 写入一段字节数据到远程 shell。
    ///
    /// `ack` 存在时，reader 在字节被真正写入 SSH channel 后 send 通知；
    /// 命令层（`terminal_write`）等待该信号实现背压——ZMODEM 大文件上传时
    /// 逐块等待写完成，避免无界 mpsc 队列把整个文件堆进内存。键盘输入等
    /// 常规路径不传 ack，行为与原来一致。
    Write {
        buf: Vec<u8>,
        ack: Option<oneshot::Sender<()>>,
    },
    /// 调整终端窗口大小。
    Resize { cols: u32, rows: u32 },
    /// 关闭会话。reader 收到后退出循环并关闭 channel。
    Close(oneshot::Sender<()>),
}

// ===========================================================================
// SshSession
// ===========================================================================

/// 一个交互式 SSH 终端会话。
///
/// 字段说明见结构体注释。`channel` 在 [`SshSession::open`] 后被设置，
/// 在 [`SshSession::spawn_reader`] 时被 `take()` 移入后台任务。
pub struct SshSession {
    /// 会话实例 ID（前端 tab 的标识，与配置 [`Session::id`] 不同）。
    pub id: String,
    /// 对应的会话配置 ID（即 [`Session::id`]）。
    pub session_config_id: String,
    /// russh 客户端句柄，可用于打开新 channel、SFTP、端口转发。
    pub handle: Handle<ClientHandler>,
    /// 已打开的 PTY channel；`spawn_reader` 后为 `None`。
    pub channel: Option<Channel<Msg>>,
    /// Tauri 应用句柄。
    pub app: tauri::AppHandle,
    /// 后台 reader 任务的句柄，`close` 时用于 abort。
    pub reader_handle: Option<tokio::task::JoinHandle<()>>,
    /// 输入通道发送端（reader 启动后才有值）。
    input_tx: Option<mpsc::UnboundedSender<InputMsg>>,
    /// 共享输出环形缓冲，reader 写入、命令层读取（AI 上下文）。
    pub output_buffer: SharedOutputRing,
}

/// 输出缓冲容量（字节），约 256 KiB，可保留数千行终端输出。
///
/// 容量大小直接影响可视化模式命令输出的可捕获范围：输出超过该容量后，
/// 命令回显行与输出开头会被滚出缓冲，AI 只能拿到尾部（见 `ai::tools` 的
/// `exec_ssh_visual`）。256 KiB 对绝大多数命令输出绰绰有余。
pub const OUTPUT_BUFFER_CAP: usize = 256 * 1024;

/// `close()` 中等待 disconnect 完成的最长时间。
///
/// russh 0.45 的 `Handle::disconnect` 通过内部有界 channel（容量 10）投递消息：
/// 底层连接任务卡住（半开连接、发送队列满）时该 await 永久挂起，表现为
/// 关 tab 永远转圈、连接不释放。超时后放弃等待，由 Handle 随 SshSession 释放兜底。
const DISCONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 单块数据写入 SSH channel 的最长等待时间。
///
/// `Channel::data` 在远端窗口耗尽（对端不读 stdin 的流控）或连接任务卡死时
/// 会无限阻塞；写入按此超时兜底，超时视为连接已坏、退出 reader 循环。
const CHANNEL_WRITE_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl SshSession {
    /// 打开一个新的交互式终端会话。
    ///
    /// 步骤：连接 + 认证 → 打开 session channel → 请求 PTY
    /// （`xterm-256color`，80×24）→ 请求 shell。
    ///
    /// 返回的 [`SshSession`] 尚未启动 reader；调用方应在随后调用
    /// [`SshSession::spawn_reader`] 把输出推送到前端。
    ///
    /// `state` 提供认证所需的全局状态（keyboard-interactive 二次认证时
    /// 需要它注册挑战、emit 事件），与 `app` 句柄。
    pub async fn open(
        session_config: &Session,
        resolved_credential: ResolvedCredential,
        state: AppState,
    ) -> AppResult<Self> {
        let app = state.app.clone();
        let handle = client::connect_direct(
            &session_config.host,
            session_config.port,
            &session_config.username,
            &session_config.id,
            resolved_credential.auth_method,
            state,
        )
        .await?;

        // 打开 session channel。
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| AppError::Ssh(format!("打开 channel 失败: {}", e)))?;

        // 请求 PTY。terminal_modes 传空切片（russh 会补一个合理的默认 ECHO 等）。
        channel
            .request_pty(false, "xterm-256color", 80, 24, 0, 0, &[])
            .await
            .map_err(|e| AppError::Ssh(format!("请求 PTY 失败: {}", e)))?;

        // 请求交互式 shell。
        channel
            .request_shell(true)
            .await
            .map_err(|e| AppError::Ssh(format!("请求 shell 失败: {}", e)))?;

        Ok(SshSession {
            id: uuid::Uuid::new_v4().to_string(),
            session_config_id: session_config.id.clone(),
            handle,
            channel: Some(channel),
            app,
            reader_handle: None,
            input_tx: None,
            output_buffer: Arc::new(StdMutex::new(OutputRing::new(OUTPUT_BUFFER_CAP))),
        })
    }

    /// 取终端最近输出的文本快照（用于 AI 上下文感知）。
    ///
    /// `max_bytes` 限制返回字节数，0 表示用默认（8 KiB）。
    pub fn snapshot(&self, max_bytes: usize) -> String {
        let n = if max_bytes == 0 { 8 * 1024 } else { max_bytes };
        match self.output_buffer.lock() {
            Ok(buf) => buf.snapshot(n),
            Err(_) => String::new(),
        }
    }

    /// 取终端输出的完整快照（整个环形缓冲）。
    ///
    /// 供可视化命令的哨兵检测/输出截取使用——那里需要"命令回显行 + 全部输出 +
    /// 哨兵输出行"完整落在快照内，不能用限长窗口，否则输出稍大就会被截没。
    pub fn full_snapshot(&self) -> String {
        match self.output_buffer.lock() {
            Ok(buf) => buf.snapshot(usize::MAX),
            Err(_) => String::new(),
        }
    }

    /// 返回环形缓冲累计写入的字节数（不受环形截断影响）。
    ///
    /// 与 [`Self::output_offset`] 的区别：缓冲写满后 `output_offset` 恒等于
    /// 容量、无法反映新输出；本值单调递增，用于判断"终端输出是否仍在增长"
    /// （可视化命令的静默完成检测）。
    pub fn total_output_bytes(&self) -> usize {
        match self.output_buffer.lock() {
            Ok(buf) => buf.total_bytes(),
            Err(_) => 0,
        }
    }

    /// 返回输出环形缓冲的当前字节数（缓冲写满后恒等于容量）。
    ///
    /// 如需记录"命令执行前"的基准位置，应使用 [`Self::total_output_bytes`]
    /// （累计值、单调递增），配合 [`Self::snapshot_after`] 截取新增输出。
    pub fn output_offset(&self) -> usize {
        match self.output_buffer.lock() {
            Ok(buf) => buf.len(),
            Err(_) => 0,
        }
    }

    /// 取 `since_total` 累计字节之后的输出窗口（配合
    /// [`Self::total_output_bytes`] 截取命令执行期间的新增输出）。
    /// 返回 (文本, 是否溢出窗口容量)。
    pub fn snapshot_after(&self, since_total: usize) -> (String, bool) {
        match self.output_buffer.lock() {
            Ok(buf) => buf.snapshot_after(since_total),
            Err(_) => (String::new(), false),
        }
    }

    /// 向远程 shell 写入数据（前端键盘输入）。
    ///
    /// 实际写入发生在 reader 任务里；此处只把数据塞进输入通道。
    /// 若通道已关闭（reader 退出），返回错误。
    pub fn write(&self, data: Vec<u8>) -> AppResult<()> {
        let tx = self
            .input_tx
            .as_ref()
            .ok_or_else(|| AppError::Ssh("终端会话尚未启动 reader 或已关闭".into()))?;
        tx.send(InputMsg::Write { buf: data, ack: None })
            .map_err(|_| AppError::Ssh("终端 reader 已退出".into()))
    }

    /// 带写完成确认的写入（背压用）。
    ///
    /// 返回的 oneshot receiver 在 reader 把字节写入 SSH channel 后收到信号；
    /// 调用方 await 它即获得真实背压。ZMODEM 上传时用此接口逐块发送，
    /// 避免无界输入通道堆积整个文件。
    pub fn write_with_ack(&self, data: Vec<u8>) -> AppResult<oneshot::Receiver<()>> {
        let tx = self
            .input_tx
            .as_ref()
            .ok_or_else(|| AppError::Ssh("终端会话尚未启动 reader 或已关闭".into()))?
            .clone();
        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(InputMsg::Write {
            buf: data,
            ack: Some(ack_tx),
        })
        .map_err(|_| AppError::Ssh("终端 reader 已退出".into()))?;
        Ok(ack_rx)
    }

    /// 通知远程终端窗口大小变化。
    pub fn resize(&self, cols: u32, rows: u32) -> AppResult<()> {
        let tx = self
            .input_tx
            .as_ref()
            .ok_or_else(|| AppError::Ssh("终端会话尚未启动 reader 或已关闭".into()))?;
        tx.send(InputMsg::Resize { cols, rows })
            .map_err(|_| AppError::Ssh("终端 reader 已退出".into()))
    }

    /// 启动后台任务持续读取远程输出并 emit 到前端。
    ///
    /// 内部把 `channel` 移入 spawn 的 tokio 任务，并创建输入 mpsc 通道。
    /// reader 任务在 `select!` 中处理三类事件：
    /// - channel 的下一条消息（`channel.wait()`）：Data/ExtendedData → emit；
    ///   Eof/Close → 退出；
    /// - 输入通道消息：Write → `channel.data()`；Resize → `channel.window_change()`；
    ///   Close → 关闭 channel 并退出。
    ///
    /// 退出后 emit `TERMINAL_EXIT` 和 `TERMINAL_CLOSED`。
    ///
    /// 若 `channel` 已被取走（`None`）则直接返回错误。
    pub fn spawn_reader(&mut self) -> AppResult<()> {
        let mut channel = self
            .channel
            .take()
            .ok_or_else(|| AppError::Ssh("channel 已被占用或关闭".to_string()))?;
        let app = self.app.clone();
        let session_id = self.id.clone();
        // 共享输出缓冲的克隆，移入 reader 任务持续写入。
        let output_buffer = self.output_buffer.clone();

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<InputMsg>();
        self.input_tx = Some(input_tx);

        let join = tokio::spawn(async move {
            let mut exit_code: Option<u32> = None;

            // 把数据写入输出环形缓冲（如果锁可用）。
            let record_output = |bytes: &[u8]| {
                if let Ok(mut buf) = output_buffer.lock() {
                    buf.push(bytes);
                }
            };

            // 待写数据队列：远端窗口（flow control）耗尽时写入停在此处，
            // 由循环顶部/尾部的按窗口冲刷逻辑在窗口恢复后继续写入。
            let mut pending_writes: VecDeque<(Vec<u8>, Option<oneshot::Sender<()>>)> =
                VecDeque::new();

            loop {
                // 先冲刷积压写入（窗口可用时）。返回 false 表示写失败/连接卡死。
                if !flush_pending_writes(&mut channel, &mut pending_writes).await {
                    break;
                }

                tokio::select! {
                    // 远程 → 前端：channel 输出。
                    // 注意：不能用 biased —— 高吞吐输出流会持续占用第一分支，
                    // 导致输入分支（用户按键/Ctrl+C）被饿死。
                    msg = channel.wait() => {
                        match msg {
                            Some(ChannelMsg::Data { ref data }) => {
                                record_output(data.as_ref());
                                let encoded = B64.encode(data);
                                events::emit(
                                    &app,
                                    TERMINAL_DATA,
                                    TerminalDataEvent {
                                        session_id: session_id.clone(),
                                        data: encoded,
                                    },
                                );
                            }
                            Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                                record_output(data.as_ref());
                                let encoded = B64.encode(data);
                                events::emit(
                                    &app,
                                    TERMINAL_DATA,
                                    TerminalDataEvent {
                                        session_id: session_id.clone(),
                                        data: encoded,
                                    },
                                );
                            }
                            Some(ChannelMsg::ExitStatus { exit_status }) => {
                                exit_code = Some(exit_status);
                                // 不立刻 break：可能仍有未读完的数据。
                            }
                            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                            Some(_) => {}
                        }
                    }
                    // 前端 → 远程：输入。
                    inp = input_rx.recv() => {
                        match inp {
                            Some(InputMsg::Write { buf, ack }) => {
                                // 不在这里 await channel.data()：远端不读 stdin 时
                                // 流控会无限阻塞，输出分支随之停摆、整个终端冻结。
                                // 入队即可，由 flush_pending_writes 按窗口余量分块冲刷。
                                pending_writes.push_back((buf, ack));
                            }
                            Some(InputMsg::Resize { cols, rows }) => {
                                // window_change 与 data 共用内部有界 channel，
                                // 连接卡死时同样会挂起：加超时兜底。
                                let _ = tokio::time::timeout(
                                    CHANNEL_WRITE_STALL_TIMEOUT,
                                    channel.window_change(cols, rows, 0, 0),
                                )
                                .await;
                            }
                            Some(InputMsg::Close(ack)) => {
                                // eof/close 同样可能被卡死的连接挂起：加超时兜底。
                                let _ = tokio::time::timeout(
                                    CHANNEL_WRITE_STALL_TIMEOUT,
                                    async {
                                        let _ = channel.eof().await;
                                        let _ = channel.close().await;
                                    },
                                )
                                .await;
                                let _ = ack.send(());
                                break;
                            }
                            None => break, // 输入端全部 drop，无人再交互
                        }
                    }
                }

                // 窗口调整可能发生在 select 等待期间，出 select 后再冲刷一次。
                if !flush_pending_writes(&mut channel, &mut pending_writes).await {
                    break;
                }
            }

            // 退出前回掉所有积压写入的 ack，避免命令层（terminal_write 背压）永久等待。
            for (_, ack) in pending_writes.drain(..) {
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }

            // 通知前端进程退出。
            events::emit(
                &app,
                TERMINAL_EXIT,
                TerminalExitEvent {
                    session_id: session_id.clone(),
                    code: exit_code.map(|c| c as i32),
                },
            );
            events::emit(&app, TERMINAL_CLOSED, TerminalClosedEvent { session_id });
        });

        self.reader_handle = Some(join);
        Ok(())
    }

/// 关闭会话：通过输入通道通知 reader 关闭 channel、abort 任务、断开底层连接。
pub async fn close(&mut self) -> AppResult<()> {
        // 先通知 reader 关闭 channel（优雅）。
        if let Some(tx) = self.input_tx.take() {
            let (ack_tx, ack_rx) = oneshot::channel();
            let _ = tx.send(InputMsg::Close(ack_tx));
            // 给最多 1 秒等待 reader 关闭。
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), ack_rx).await;
        }

        // 强制 abort reader（若仍在运行）。
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }

        // channel 此时已被 reader 持有；若 reader 未能启动（channel 仍在），关闭它。
        if let Some(channel) = self.channel.take() {
            let _ = channel.eof().await;
            let _ = channel.close().await;
        }

        // 断开传输层。disconnect 经内部有界 channel 投递，底层任务卡死时永久
        // 挂起（关 tab 永远转圈），加超时兜底：超时后放弃等待，SshSession 被
        // drop 时 Handle 随之释放，连接最终被清理。
        match tokio::time::timeout(
            DISCONNECT_TIMEOUT,
            self.handle
                .disconnect(Disconnect::ByApplication, "bye", "en"),
        )
        .await
        {
            Ok(res) => res.map_err(|e| AppError::Ssh(format!("disconnect 失败: {}", e)))?,
            Err(_) => {
                log::warn!(
                    "disconnect 超过 {} 秒未完成，放弃等待（连接由 handle 释放兜底）",
                    DISCONNECT_TIMEOUT.as_secs()
                );
            }
        }

        Ok(())
    }
}

/// 按 SSH 窗口余量冲刷待写数据（分块写入）。
///
/// 背景：`Channel::data` 在远端窗口耗尽（对端不读 stdin 的流控）或底层连接
/// 任务卡死时会无限阻塞。若在 reader 的 select 臂内直接 await 它，输出分支
/// 随之停摆、整个终端冻结。因此写入改走队列：本函数只在窗口有余量时写入
/// （每块 ≤ 当前窗口余量，单块写不会跨窗口等待），窗口为 0 时立即返回、
/// 数据留在队列等窗口恢复；单块写入超时（[`CHANNEL_WRITE_STALL_TIMEOUT`]）
/// 视为连接已坏。
///
/// 带 ack 的写入在**整条**数据写完（或失败/退出）时才回 ack，保持
/// `write_with_ack` 的真实背压语义。返回 `false` 表示写失败或连接卡死，
/// 调用方应退出 reader 循环。
async fn flush_pending_writes(
    channel: &mut Channel<Msg>,
    pending: &mut VecDeque<(Vec<u8>, Option<oneshot::Sender<()>>)>,
) -> bool {
    loop {
        let Some((mut buf, ack)) = pending.pop_front() else {
            return true;
        };
        // 一条写入可能因窗口耗尽分多块完成，整条写完才回 ack。
        loop {
            let win = channel.writable_packet_size().await;
            if win == 0 {
                // 远端流控：整条放回队首，等窗口恢复（不阻塞输出分支）。
                pending.push_front((buf, ack));
                return true;
            }
            let n = buf.len().min(win);
            let chunk = buf[..n].to_vec();
            let done = n == buf.len();
            match tokio::time::timeout(CHANNEL_WRITE_STALL_TIMEOUT, channel.data(&chunk[..]))
                .await
            {
                Ok(Ok(())) => {
                    if done {
                        if let Some(ack) = ack {
                            let _ = ack.send(());
                        }
                        break; // 本条写完，处理队列下一条
                    }
                    buf = buf[n..].to_vec();
                }
                Ok(Err(_)) | Err(_) => {
                    // 写失败或连接卡死：回 ack 并通知退出。
                    if let Some(ack) = ack {
                        let _ = ack.send(());
                    }
                    return false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 窗口取基准之后的全部新写入内容。
    #[test]
    fn snapshot_after_returns_window_since_total() {
        let mut ring = OutputRing::new(16);
        ring.push(b"AAAA");
        let base = ring.total_bytes();
        ring.push(b"BBBB");
        assert_eq!(ring.snapshot_after(base), ("BBBB".to_string(), false));
    }

    /// 窗口超过环形容量时头部被环形截断丢弃，必须报溢出。
    #[test]
    fn snapshot_after_reports_overflow() {
        let mut ring = OutputRing::new(4);
        ring.push(b"ABCD");
        let base = ring.total_bytes();
        ring.push(b"EFGHIJ"); // 窗口 6 字节 > 容量 4，头部丢失
        let (text, overflow) = ring.snapshot_after(base);
        assert!(overflow);
        assert_eq!(text, "GHIJ");
    }
}
