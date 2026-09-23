//! SSH MCP「堡垒机模式」（resource_mode = "bastion"）的会话管理与执行。
//!
//! 适用场景：堡垒机（如 JumpServer）本身走 SSH 协议，登录后在终端里输入
//! 资产 IP / 名称回车即可进入对应主机。本模块把这一交互自动化：
//!
//! - 绑定资源是一个**SSH 会话配置**（即堡垒机服务器 + 账号）；
//! - `bastion_create_session(host)`：在**基础连接**上开一个新 channel，等登录
//!   菜单稳定后写入目标主机，再用「哨兵 echo」验证已进入目标主机 shell，
//!   按配置执行可选的「登录后命令」（如 `sudo su -`），登记会话并返回 session_id；
//! - `bastion_session_exec(sessionId, command)`：在对应 channel 的 PTY 里哨兵式
//!   执行命令（算法与 `exec_ssh_terminal` 一致：包装命令 + 轮询哨兵 + 截取新输出）；
//! - `bastion_upload_file(sessionId, localPath, remotePath)`：经会话 shell 把本地
//!   文件以 base64 分块写入远端临时文件、再解码落盘并校验（详见 [`crate::mcp::upload`]）
//!   ——到目标主机只有 shell 通道，没有 SFTP；
//! - `bastion_close_session(sessionId)`：关闭并移除会话；
//! - `bastion_list_hosts()`：复用基础连接开 channel，返回登录后回显的资产菜单
//!   文本（best-effort：不同堡垒机版本菜单格式不同，解析交由外部 AI 阅读）；
//! - `bastion_list_sessions()`：列出当前活跃会话（目标主机 / 存活 / 空闲时长）。
//!
//! # 连接模型：基础连接 + 多 channel（MFA 只输一次）
//!
//! MFA（keyboard-interactive 二次认证）属于**连接级认证**，与 channel 无关。
//! 因此本模块把「连接」与「目标主机会话」拆开：
//!
//! - **基础连接**（[`BaseConn`]）：一条完成认证的传输层，**不打开任何 channel**。
//!   首次使用时建立（此时才可能弹一次验证码），之后缓存复用；
//! - **目标主机会话**：在基础连接上用 [`SshSession::open_channel_on`] 开新
//!   channel（复用已认证 Handle，**不重新认证、不再弹 MFA**），走菜单进资产。
//!
//! 于是：整个服务生命周期内用户只需输入一次动态口令。基础连接在
//! 「无目标会话且超过 `base_idle_minutes`」或服务停止时释放（引用计数与
//! `SshSession::close` 一致：仍有会话复用时不真正断开）。
//!
//! # 会话生命周期
//! - 仅存在于内存（HashMap<session_id, ...>），session_id 由后端随机生成；
//! - 每次成功 exec 刷新 `last_active`；空闲超过 `session_idle_minutes` 由
//!   后台回收任务自动断开；服务停止时全部断开；
//! - 同一会话同时只允许一个调用执行（per-session 忙锁，防输出交叉/哨兵干扰）。
//!
//! 连接复用 [`crate::ssh::session::SshSession`]：PTY 建立走与终端相同的路径
//! （xterm-256color + OpenSSH 对齐的 PTY modes，兼容 JumpServer koko 的会话
//! 检测）；reader 会向前端 emit `terminal:data` 事件（session_id 无对应终端
//! 窗格，前端各监听者按 instanceId 过滤、自动忽略，无副作用）。会话存取用
//! `Arc<tokio::sync::Mutex<SshSession>>` 包装——exec 的各快照/写入方法只需
//! `&self`（内部通道/锁自带可变性），但 map 守卫不能跨 await 持有，克隆 Arc
//! 出来再锁即可。
//!
//! 限制：堡垒机账号若强制 MFA，首次建立基础连接会触发全局 `ssh:auth_challenge`
//! 弹窗（见 SshAuthPrompt），用户在 120 秒内输入动态口令即可继续（之后复用连接
//! 不再需要）；超时未输入则本次建连失败。无人值守场景建议使用免 MFA 账号或
//! 配置来源 IP 免验。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use russh::Disconnect;
use tokio::sync::Mutex as AsyncMutex;

use crate::error::{AppError, AppResult};
use crate::ssh::client::AuthMethod;
use crate::ssh::session::{SharedTransport, SshSession};
use crate::state::AppState;
use crate::storage::sessions_repo::{get_session, Session};

/// 输出截断上限（16 KiB，与 exec_ssh 一致）。
const EXEC_OUTPUT_CAP: usize = 16 * 1024;
/// 登录菜单 / 连接目标主机的输出静默判定宽限。
const SETTLE_GRACE: Duration = Duration::from_millis(1500);
/// 登录菜单稳定的整体等待上限（慢堡垒机 banner 兜底）。
const LOGIN_SETTLE_TIMEOUT: Duration = Duration::from_secs(20);
/// 写入目标主机后等待连接建立（banner/提示符出完）的上限。
const CONNECT_SETTLE_TIMEOUT: Duration = Duration::from_secs(15);
/// 哨兵验证（确认已进入目标主机 shell）的超时。
const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);
/// 「目标主机 shell 是否就绪」这一次验证的超时。
///
/// 比 [`VERIFY_TIMEOUT`] 短：真进了目标主机，哨兵回显是亚秒级的；迟迟等不到
/// 基本就是没进去（还停在堡垒机菜单）或行没被提交，早点失败好让用户看到原因，
/// 不必干等 20 秒。
const SHELL_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// 执行「登录后命令」（如 `sudo su -`）的等待上限。
const POST_LOGIN_TIMEOUT: Duration = Duration::from_secs(25);
/// 写入 PTY 的确认等待上限。
const WRITE_ACK_TIMEOUT: Duration = Duration::from_secs(15);
/// 释放基础连接时等待 disconnect 完成的上限（与 ssh::session 一致）。
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// 在基础连接上开 channel 的等待上限。
///
/// 连接半开（对端静默掉线、无 FIN）时 `channel_open_session` 会一直等回复，
/// 把整个 MCP 工具调用挂住——超时后按"连接已坏"处理并重建。
const CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(20);
/// 哨兵完成判定的静默宽限（"哨兵只出现一次且不在回显行"时用它判定无回显 shell）。
const SILENCE_GRACE: Duration = Duration::from_secs(1);
/// 默认哨兵轮询间隔（普通命令）。
const EXEC_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// 上传分块的哨兵轮询间隔。
///
/// 每块都要等一次哨兵确认，轮询间隔直接决定上传吞吐（分块本身很小），
/// 故用更密的轮询换取速度。
const UPLOAD_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// 上传分块与收尾命令各自的等待上限。
const UPLOAD_STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// 上传（base64 分块）的整体超时。
///
/// 会话 shell 通道每块都要一次往返，比 SFTP 慢；给足 15 分钟，超时即报错，
/// 由调用方决定重试（中途可用 bastion_close_session 放弃）。
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// 同时存在的堡垒机会话数上限（防御打满堡垒机连接数）。
const MAX_SESSIONS: usize = 16;
/// 空闲回收任务的扫描间隔。
const REAPER_INTERVAL: Duration = Duration::from_secs(60);
/// 基础连接保持时长的配置上限（分钟），防止误配成天文数字。
pub const MAX_BASE_IDLE_MINUTES: u32 = 24 * 60;

// ===========================================================================
// 运行参数
// ===========================================================================

/// 堡垒机模式的运行参数（`start_mcp_server` 启动时经 [`configure`] 注入）。
#[derive(Debug, Clone, Default)]
pub struct BastionOptions {
    /// 进入目标主机后自动执行的命令（如 `sudo su -`）。空 = 不执行。
    ///
    /// 在会话建立时执行一次；之后 `bastion_session_exec` 的命令都在其上下文
    /// （如 root 登录 shell）中执行。要求无需交互输入——如提权请给账号配
    /// 免密 sudo（NOPASSWD），否则会因等待输入而超时并报错。
    pub post_login_command: String,
    /// 基础连接（完成 MFA 的那条）的空闲保持时长（分钟）。
    ///
    /// 0 = 不保持：最后一个目标会话关闭后即释放，下次请求需重新认证（MFA）。
    pub base_idle_minutes: u32,
    /// 目标主机会话的空闲回收时长（分钟）。0 = 不回收。
    pub session_idle_minutes: u32,
}

/// 当前运行参数（仅堡垒机模式使用）。
static OPTIONS: Lazy<parking_lot::Mutex<BastionOptions>> =
    Lazy::new(|| parking_lot::Mutex::new(BastionOptions::default()));

/// 注入运行参数（由 `start_mcp_server` 在堡垒机模式下调用）。
pub fn configure(options: BastionOptions) {
    *OPTIONS.lock() = options;
}

/// 读取当前运行参数（克隆一份，避免持锁跨 await）。
fn options() -> BastionOptions {
    OPTIONS.lock().clone()
}

// ===========================================================================
// 基础连接（完成 MFA 的那一条）
// ===========================================================================

/// 基础连接：一条**已完成认证**（含 MFA）的到堡垒机的 SSH 连接。
///
/// 只持有已认证的传输层，**不打开任何 channel**。MFA 属于连接级认证，因此
/// 之后每个目标主机都用 [`SshSession::open_channel_on`] 在它上面开新 channel
/// （走一遍菜单进资产即可），**不会再次弹验证码**。
struct BaseConn {
    transport: SharedTransport,
    /// 绑定的堡垒机会话配置 id（配置换了要重建）。
    config_id: String,
    /// 建连时的目标地址与账号。
    ///
    /// 与 `config_id` 一同参与匹配：用户改了绑定会话的 host/port/username 后
    /// id 不变，若只比对 id 会继续复用旧连接、改动被静默忽略。
    host: String,
    port: u16,
    username: String,
    /// 最近一次被使用（开 channel）的时间，空闲保持时长据此判定。
    last_used: Instant,
}

/// 基础连接缓存（只保留一条；`None` = 尚未建立或已失效）。
static BASE: Lazy<parking_lot::Mutex<Option<BaseConn>>> =
    Lazy::new(|| parking_lot::Mutex::new(None));

/// 基础连接的建连串行锁。
///
/// 建连可能等待用户在弹窗里输入动态口令（最长 120 秒），必须串行化：否则
/// 并发的 create_session 会各自发起认证、弹出多个验证码窗口。锁只护「建连」
/// 这一段，不护状态读写（状态走 [`BASE`] 的同步锁），避免慢建连拖住回收任务。
static BASE_CONNECT_LOCK: Lazy<AsyncMutex<()>> = Lazy::new(|| AsyncMutex::new(()));

/// 克隆共享部件（字段全是 Arc / String，克隆廉价）。
fn transport_clone(t: &SharedTransport) -> SharedTransport {
    SharedTransport {
        handle: t.handle.clone(),
        session_config_id: t.session_config_id.clone(),
        app: t.app.clone(),
        disconnect_key: t.disconnect_key.clone(),
        disconnect_reasons: t.disconnect_reasons.clone(),
        conn_refs: t.conn_refs.clone(),
    }
}

/// 一个堡垒机会话：在基础连接上开出的一个 channel（PTY 内已进入目标主机）。
struct BastionSession {
    /// 连接本体（Arc 克隆出来用；close 需要 &mut，经 tokio 锁取得）。
    session: Arc<AsyncMutex<SshSession>>,
    /// AI 传入的目标主机标识（资产 IP / 名称，原样记录供展示）。
    host_label: String,
    created_at: Instant,
    last_active: Instant,
    /// 同一会话的串行执行锁（并发写同一 PTY 会导致输出交叉、哨兵互相干扰）。
    busy: Arc<AsyncMutex<()>>,
}

/// 全局堡垒机会话表（仅 SSH MCP 堡垒机模式使用）。
static SESSIONS: Lazy<AsyncMutex<HashMap<String, BastionSession>>> =
    Lazy::new(|| AsyncMutex::new(HashMap::new()));

/// 空闲回收任务句柄（stop 时 abort）。
static REAPER: Lazy<parking_lot::Mutex<Option<tokio::task::AbortHandle>>> =
    Lazy::new(|| parking_lot::Mutex::new(None));

// ===========================================================================
// 基础原语
// ===========================================================================

/// 按 id 取堡垒机的 SSH 会话配置（必须是 SSH 协议）。
fn find_bastion_config(state: &AppState, config_id: &str) -> AppResult<Session> {
    let conn = state.conn()?;
    get_session(&conn, config_id)?
        .filter(|s| s.protocol == "ssh")
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "绑定的堡垒机会话配置不存在（可能已被删除），请重新绑定（id: {}）",
                config_id
            ))
        })
}

/// 解析堡垒机会话配置的认证方式（同步块，DB 连接短生命）。
fn resolve_auth(state: &AppState, config: &Session) -> AppResult<AuthMethod> {
    let vault = {
        let guard = state.vault_read()?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
            .clone()
    };
    let conn = state.conn()?;
    Ok(crate::ssh::session::resolve_credential(config, &vault, &conn)?.auth_method)
}

/// 建立基础连接（完成认证，**不打开 channel**）。
///
/// 这是整个堡垒机模式唯一会触发认证（含 MFA 弹窗）的地方。
async fn create_base(state: &AppState, config: &Session) -> AppResult<SharedTransport> {
    let auth = resolve_auth(state, config)?;
    // 键含随机后缀，与终端连接一致：同一 host:port 的多条连接互不覆盖断开原因。
    let disconnect_key = format!(
        "{}:{}#{}",
        config.host,
        config.port,
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let handle = crate::ssh::client::connect_direct_terminal(
        &config.host,
        config.port,
        &config.username,
        &config.id,
        auth,
        state.clone(),
        disconnect_key.clone(),
    )
    .await?;
    Ok(SharedTransport {
        handle: Arc::new(handle),
        session_config_id: config.id.clone(),
        app: state.app.clone(),
        disconnect_key,
        disconnect_reasons: state.ssh_disconnect_reasons.clone(),
        // 基础连接自身占 1 个引用：目标会话关闭递减计数时不会把传输层断掉，
        // 只有本模块显式释放（空闲超时 / 停止服务）才会归零并 disconnect。
        conn_refs: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
    })
}

/// 读缓存：配置匹配且连接未关闭时返回传输层克隆，并刷新使用时间。
fn base_if_usable(config: &Session) -> Option<SharedTransport> {
    let mut guard = BASE.lock();
    let base = guard.as_mut()?;
    if base.config_id != config.id
        || base.host != config.host
        || base.port != config.port
        || base.username != config.username
    {
        return None;
    }
    if base.transport.handle.is_closed() {
        log::info!("[mcp] 堡垒机基础连接已被关闭，将重建");
        return None;
    }
    base.last_used = Instant::now();
    Some(transport_clone(&base.transport))
}

/// 取可用的基础连接：优先复用已认证连接；不存在 / 失效 / 配置变更时建连。
///
/// 复用路径不触发认证，因此不会弹 MFA；只有首次（或连接被堡垒机回收后重建）
/// 才需要用户在弹窗里输入动态口令一次。
async fn get_base(state: &AppState, config: &Session) -> AppResult<SharedTransport> {
    if let Some(t) = base_if_usable(config) {
        return Ok(t);
    }
    // 串行建连：并发请求在这里排队，只弹一次验证码。
    let _guard = BASE_CONNECT_LOCK.lock().await;
    if let Some(t) = base_if_usable(config) {
        return Ok(t);
    }
    // 配置变更或连接已死：先释放旧的，再建新的。
    release_base().await;
    let transport = create_base(state, config).await?;
    log::info!(
        "[mcp] 堡垒机基础连接已建立（{}@{}:{}）：后续目标主机复用该连接，不再重复认证",
        config.username,
        config.host,
        config.port
    );
    *BASE.lock() = Some(BaseConn {
        transport: transport_clone(&transport),
        config_id: config.id.clone(),
        host: config.host.clone(),
        port: config.port,
        username: config.username.clone(),
        last_used: Instant::now(),
    });
    Ok(transport)
}

/// 释放基础连接：递减自身引用计数，无目标会话复用时断开传输层。
///
/// 与 [`SshSession::close`] 的引用计数语义一致——仍有目标会话在用这条连接时
/// 只递减不断开，否则会把这些会话一起断掉。
async fn release_base() {
    let Some(base) = BASE.lock().take() else {
        return;
    };
    if base.transport.conn_refs.fetch_sub(1, Ordering::AcqRel) != 1 {
        // 仍有目标会话复用这条连接：只摘掉基础连接引用，传输层留给它们。
        return;
    }
    match tokio::time::timeout(
        DISCONNECT_TIMEOUT,
        base.transport
            .handle
            .disconnect(Disconnect::ByApplication, "bye", "en"),
    )
    .await
    {
        Ok(Err(e)) => log::warn!("[mcp] 断开堡垒机基础连接失败（忽略）: {}", e),
        Err(_) => log::warn!("[mcp] 断开堡垒机基础连接超时，放弃等待"),
        Ok(Ok(())) => {}
    }
    log::info!("[mcp] 堡垒机基础连接已释放（下次请求需重新认证）");
}

/// 无目标会话且达到保持时长（或配置为不保持）时，释放基础连接。
///
/// 在会话移除后调用，使 `base_idle_minutes == 0`（不保持）能即时断开，
/// 不必等回收任务下一轮扫描。
async fn release_base_if_unused() {
    if !SESSIONS.lock().await.is_empty() {
        return;
    }
    let opts = options();
    let expired = {
        let guard = BASE.lock();
        match guard.as_ref() {
            Some(b) => {
                opts.base_idle_minutes == 0
                    || b.last_used.elapsed()
                        >= Duration::from_secs(opts.base_idle_minutes as u64 * 60)
            }
            None => false,
        }
    };
    if expired {
        release_base().await;
    }
}

/// 在基础连接上开一个新 channel（复用已认证连接 → **不重新认证、不弹 MFA**）。
///
/// 连接已失效（被堡垒机空闲回收 / 网络中断）时自动重建并重试一次；重建会
/// 重新认证，可能再次弹验证码。
async fn open_bastion_channel(state: &AppState, config: &Session) -> AppResult<SshSession> {
    let mut last_err: Option<AppError> = None;
    for attempt in 0..2 {
        let base = get_base(state, config).await?;
        // 开通道加超时：半开连接上这个 await 可能永不返回（见 CHANNEL_OPEN_TIMEOUT）。
        match tokio::time::timeout(CHANNEL_OPEN_TIMEOUT, SshSession::open_channel_on(base)).await {
            Ok(Ok(mut session)) => {
                session.spawn_reader()?;
                return Ok(session);
            }
            Ok(Err(e)) => {
                log::warn!(
                    "[mcp] 在堡垒机基础连接上打开 channel 失败（第 {} 次）：{}",
                    attempt + 1,
                    e
                );
                last_err = Some(e);
            }
            Err(_) => {
                log::warn!(
                    "[mcp] 在堡垒机基础连接上打开 channel 超时（第 {} 次，{} 秒）",
                    attempt + 1,
                    CHANNEL_OPEN_TIMEOUT.as_secs()
                );
                last_err = Some(AppError::Ssh(format!(
                    "在堡垒机连接上打开会话通道超时（{} 秒），连接可能已失效",
                    CHANNEL_OPEN_TIMEOUT.as_secs()
                )));
            }
        }
        // 连接大概率已被堡垒机回收：标记失效，下一轮重建后重试。
        release_base().await;
    }
    Err(last_err
        .unwrap_or_else(|| AppError::Ssh("无法在堡垒机连接上打开会话通道".into())))
}


/// 向 PTY 写入一行（按终端编码设置转码；等待写完成确认）。
///
/// **行结束符用 `\r` 而不是 `\n`**：真实终端（xterm / OpenSSH 客户端）按 Enter
/// 发的就是 CR，堡垒机菜单（如 JumpServer koko）多为原始模式行读取器，只认 CR；
/// 发 LF 会被回显但不提交——表现为"命令只回显、永远不执行"。内核 PTY 默认开着
/// ICRNL，CR 会被翻译成 NL，因此对普通 shell 同样有效。
async fn write_line(session: &SshSession, state: &AppState, line: &str) -> AppResult<()> {
    let bytes = {
        let label = crate::config::settings_load_inner(state)
            .map(|s| s.terminal.encoding)
            .unwrap_or_default();
        crate::encoding::encode_input(&label, format!("{line}\r").as_bytes())
    };
    let rx = session
        .write_with_ack(bytes)
        .map_err(|e| AppError::Ssh(format!("写入堡垒机会话失败: {e}")))?;
    match tokio::time::timeout(WRITE_ACK_TIMEOUT, rx).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(AppError::Ssh(
            "堡垒机会话连接已断开（reader 退出）".into(),
        )),
        Err(_) => Err(AppError::Ssh(
            "写入堡垒机会话超时（15 秒），连接可能已卡死".into(),
        )),
    }
}

/// 等待输出静默（登录菜单 / 连接 banner 出完），返回稳定后的完整快照。
///
/// 输出停止增长 `grace` 时长即视为稳定；`deadline` 兜底超时（返回当前快照，
/// 由调用方结合内容判断成败——如菜单未出全、连接失败错误文本等）。
async fn wait_settle(session: &SshSession, grace: Duration, deadline: Duration) -> String {
    let start = Instant::now();
    let mut last_total = session.total_output_bytes();
    let mut last_change = Instant::now();
    let mut snapshot = session.full_snapshot();
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let total = session.total_output_bytes();
        if total != last_total {
            last_total = total;
            last_change = Instant::now();
            snapshot = session.full_snapshot();
        }
        if last_change.elapsed() >= grace || start.elapsed() >= deadline {
            return snapshot;
        }
    }
}

/// 哨兵式执行的结果。
struct ExecOutcome {
    /// 清理后的输出文本（未完成时含提示）。
    text: String,
    /// 是否检测到哨兵（命令确实执行完毕）。用于「登录后命令」这类必须成功
    /// 才能继续的场景——超时未完成时要报错而不是把提示当输出返回。
    completed: bool,
}

/// 哨兵式执行：写入 `<cmd>; echo <SENTINEL>`，轮询哨兵出现判定完成，
/// 截取新增输出、清理回显与哨兵行后返回。
///
/// 算法与 `exec::exec_ssh_terminal` 一致（≥2 次出现 = 回显行 + 输出行；
/// 1 次且不在回显行 + 输出静默 = 无回显 shell 场景）。
///
/// `poll` 为轮询间隔：普通命令用 [`EXEC_POLL_INTERVAL`]；上传分块这类"每块都要
/// 等一次确认"的场景用更密的 [`UPLOAD_POLL_INTERVAL`]，否则间隔直接变成吞吐瓶颈。
async fn sentinel_exec_inner(
    session: &SshSession,
    state: &AppState,
    command: &str,
    timeout: Duration,
    poll: Duration,
) -> AppResult<ExecOutcome> {
    use rand::Rng;
    let nonce: u64 = rand::thread_rng().gen();
    let sentinel = format!("__XTERM_BASTION_{nonce:x}__");
    let echo_marker = format!("echo {sentinel}");
    let cmd = command.trim_end_matches(['\n', '\r']);
    let wrapped = format!("{cmd}; echo {sentinel}");

    let base = session.total_output_bytes();
    write_line(session, state, &wrapped).await?;

    let deadline = Instant::now() + timeout;
    let mut snapshot = String::new();
    let mut last_total = 0usize;
    let mut silence_since = Instant::now();
    loop {
        tokio::time::sleep(poll).await;
        let (snap, total) = {
            let total = session.total_output_bytes();
            if total == last_total {
                (None, total)
            } else {
                (Some(session.full_snapshot()), total)
            }
        };
        if let Some(s) = snap {
            snapshot = s;
        }

        let occurrences = snapshot.matches(&sentinel).count();
        if occurrences >= 2 {
            break;
        }
        if occurrences == 1
            && !crate::ai::tools::sentinel_in_echo_line(&snapshot, &sentinel, &echo_marker)
            && total == last_total
            && silence_since.elapsed() >= SILENCE_GRACE
        {
            break;
        }
        if total != last_total {
            silence_since = Instant::now();
        }
        last_total = total;

        if Instant::now() >= deadline {
            // 超时：返回已收集的部分输出并附提示（与 exec_ssh_terminal 一致）。
            let (window, overflow) = session.snapshot_after(base);
            let cleaned = crate::utils::strip_ansi(&window);
            let mut text = format!(
                "命令已写入堡垒机会话执行，但 {} 秒内未检测到完成（可能仍在运行或等待输入）。\n目前输出：\n{}",
                timeout.as_secs(),
                truncate(&cleaned)
            );
            if overflow {
                text.push_str(&crate::ai::tools::output_overflow_note());
            }
            return Ok(ExecOutcome {
                text,
                completed: false,
            });
        }
    }

    let (window, overflow) = session.snapshot_after(base);
    let cleaned =
        crate::ai::tools::clean_window(&crate::utils::strip_ansi(&window), cmd, &sentinel);
    let mut text = truncate(&cleaned);
    if overflow {
        text.push_str(&crate::ai::tools::output_overflow_note());
    }
    Ok(ExecOutcome {
        text,
        completed: true,
    })
}

/// [`sentinel_exec_inner`] 的便捷封装：只取输出文本（调用方不关心是否完成）。
async fn sentinel_exec(
    session: &SshSession,
    state: &AppState,
    command: &str,
    timeout: Duration,
) -> AppResult<String> {
    Ok(
        sentinel_exec_inner(session, state, command, timeout, EXEC_POLL_INTERVAL)
            .await?
            .text,
    )
}

/// 执行「登录后命令」并确认新上下文可用。
///
/// **不能用哨兵包装**（`cmd; echo SENTINEL`）：`sudo su -` 这类命令会启动一个
/// 交互式 shell，`; echo SENTINEL` 要等那个 shell 退出才会执行——哨兵永远等不到，
/// 功能正常也会被判为失败。因此改为：
///
/// 1. 原样写入命令（不追加哨兵）；
/// 2. 等输出稳定（命令若切换了 shell，稳定即表示新提示符已出来）；
/// 3. 回显末行若像"等待输入"（提权密码等）→ 立即报错并给出配置建议（快速失败，
///    不必干等哨兵超时）；
/// 4. 在**新上下文里**跑一次哨兵 `true`，确认后续 bastion_session_exec 可用。
async fn run_login_command(
    session: &SshSession,
    state: &AppState,
    command: &str,
) -> AppResult<()> {
    write_line(session, state, command).await?;
    let out = wait_settle(session, SETTLE_GRACE, POST_LOGIN_TIMEOUT).await;
    let cleaned = crate::utils::strip_ansi(&out);
    if let Some(hint) = login_waiting_hint(&cleaned) {
        return Err(AppError::Ssh(format!(
            "{}（请为该账号配置免密 sudo（NOPASSWD），或把「登录后命令」留空）。目前回显：\n{}",
            hint,
            truncate(cleaned.trim())
        )));
    }
    // 新上下文可用性验证：哨兵能正常回显，说明后续命令可以执行。
    let outcome = sentinel_exec_inner(
        session,
        state,
        "true",
        VERIFY_TIMEOUT,
        EXEC_POLL_INTERVAL,
    )
    .await?;
    if !outcome.completed {
        return Err(AppError::Ssh(format!(
            "命令已写入，但之后 {} 秒内未确认新的 shell 上下文就绪（可能仍在等待输入，或命令尚未结束）。目前输出：\n{}",
            VERIFY_TIMEOUT.as_secs(),
            truncate(outcome.text.trim())
        )));
    }
    Ok(())
}

/// 登录后命令的回显是否"停在等待输入"（提权密码 / 确认提示等）。
///
/// 只检查**末行**且要求行较短、不以 shell 提示符结尾——这些提示都是"光标停在
/// 行尾等输入"的形态（如 `[sudo] password for tom:`），而命令正常结束时会回到
/// 以 `$` / `#` / `>` 结尾的提示符。判定偏保守：宁可漏判（后面还有哨兵验证
/// 兜底并给出超时错误），也不要误判导致正常命令被中断。
fn login_waiting_hint(out: &str) -> Option<&'static str> {
    const MARKERS: &[(&str, &str)] = &[
        ("[sudo] password", "登录后命令在等待 sudo 密码"),
        ("password for", "登录后命令在等待密码"),
        ("password:", "登录后命令在等待密码"),
        ("passphrase", "登录后命令在等待口令"),
        ("密码", "登录后命令在等待密码"),
        ("口令", "登录后命令在等待口令"),
        ("(yes/no", "登录后命令在等待确认输入"),
        ("[y/n]", "登录后命令在等待确认输入"),
    ];
    let line = last_nonempty_line(out);
    if line.is_empty() || line.chars().count() > 120 {
        return None;
    }
    // 已回到 shell 提示符 → 命令已结束，并非在等输入。
    if line.ends_with('$') || line.ends_with('#') || line.ends_with('>') {
        return None;
    }
    let lower = line.to_lowercase();
    MARKERS
        .iter()
        .find(|(m, _)| lower.contains(m))
        .map(|(_, hint)| *hint)
}

/// 截断输出到 16 KiB，超出则尾部提示。
fn truncate(s: &str) -> String {
    if s.len() <= EXEC_OUTPUT_CAP {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(EXEC_OUTPUT_CAP).collect();
    truncated.push_str("\n... [输出已截断]");
    truncated
}

/// 连接目标主机失败的回显特征（小写匹配）。
///
/// 主要防御「绑定的其实是**普通服务器**（非堡垒机）」的静默错主机场景：
/// 写入 IP 被当 shell 命令执行，回显 `command not found` 等——此时哨兵验证
/// 在错误主机的 shell 上照样通过，必须靠回显特征拦截。
///
/// 刻意**只保留命令未找到族**特征：资产不存在/被拒等场景哨兵验证本身就会
/// 失败（菜单还在，哨兵不回显），而成功主机的 MOTD 可能包含 denied/timeout
/// 等词（如 "Unauthorized access DENIED"），宽泛特征会造成误判。
const CONNECT_FAIL_MARKERS: &[&str] = &[
    "command not found",
    "not found",
    "未找到命令",
    "找不到命令",
    "没有那个文件或目录",
];

/// 取文本的最后一行非空内容（去首尾空白），用于比较提示符。
///
/// 提示符通常是终端输出的末行；比较前后是否变化可判断 shell 是否发生了切换。
fn last_nonempty_line(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 在写入目标主机后的回显里嗅探失败特征；命中返回 true（判定连接未成功）。
fn connect_output_looks_failed(out: &str) -> bool {
    let lower = out.to_lowercase();
    CONNECT_FAIL_MARKERS.iter().any(|m| lower.contains(m))
}

/// 关闭一个会话（容忍失败：连接已死时底层清理仍需执行）。
async fn close_quietly(session: &Arc<AsyncMutex<SshSession>>) {
    if let Err(e) = session.lock().await.close().await {
        log::warn!("[mcp] 关闭堡垒机会话时出错（忽略）: {}", e);
    }
}

/// 从会话表中移除并关闭指定会话；返回目标主机名（供日志）。
///
/// 先把会话摘出表、再在锁外关闭：close 含两次网络等待（最多数秒），持锁会让
/// 其它会话操作（新建/执行/回收）全部排队。关闭后按需回收空闲的基础连接。
async fn remove_session(session_id: &str) -> Option<String> {
    let removed = SESSIONS.lock().await.remove(session_id);
    let s = removed?;
    let host = s.host_label.clone();
    close_quietly(&s.session).await;
    // 最后一个会话关掉后连接才真正空闲：把"空闲起点"重置为此刻。否则一个长
    // 时间运行的会话（last_used 停留在它创建时）一关闭就会被判为已超时，
    // 配置的保持时长形同虚设。
    if SESSIONS.lock().await.is_empty() {
        if let Some(base) = BASE.lock().as_mut() {
            base.last_used = Instant::now();
        }
    }
    release_base_if_unused().await;
    Some(host)
}

// ===========================================================================
// 对外工具实现（由 server.rs 的 run_target 分派）
// ===========================================================================

/// 从工具参数取 sessionId（bastion_create_session 的返回值）。
pub fn arg_session_id(args: &serde_json::Value) -> AppResult<String> {
    let id = args
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::InvalidInput(
                "缺少 sessionId 参数（bastion_create_session 返回的会话 id）".into(),
            )
        })?;
    Ok(id.to_string())
}

/// `bastion_list_hosts`：临时连接堡垒机，返回登录后回显的资产菜单文本。
///
/// best-effort：不同堡垒机（及版本）登录后的菜单格式各异，不做结构化解析，
/// 原样返回清洗 ANSI 后的文本由外部 AI 阅读；同时提示可直接把资产 IP/名称
/// 传给 `bastion_create_session`。
pub async fn list_hosts(state: &AppState, bastion_config_id: &str) -> AppResult<String> {
    let config = find_bastion_config(state, bastion_config_id)?;
    // 复用基础连接开 channel（不重新认证 → 不弹 MFA）。
    let mut session = open_bastion_channel(state, &config).await?;
    let banner = wait_settle(&session, SETTLE_GRACE, LOGIN_SETTLE_TIMEOUT).await;
    let cleaned = crate::utils::strip_ansi(&banner);
    let text = cleaned.trim();
    let body = if text.is_empty() {
        "（登录后没有回显资产菜单——该堡垒机可能不展示菜单，请直接向 bastion_create_session 传已知的资产 IP/名称。）".to_string()
    } else {
        truncate(text)
    };
    // 只关这个 channel：基础连接由引用计数保留，供后续目标主机复用。
    let _ = session.close().await;
    Ok(format!(
        "以下为堡垒机登录后的回显内容（资产菜单，供选择目标主机）：\n\n{}\n\n提示：把目标主机的 IP 或资产名称原样传给 bastion_create_session 的 host 参数即可建立会话。",
        body
    ))
}

/// `bastion_create_session`：为目标主机建立一条堡垒机会话，返回 session_id。
///
/// 流程：连接堡垒机 → 等登录菜单稳定 → 写入 host → 等连接输出稳定 →
/// 嗅探回显失败特征（防"绑定的其实是普通服务器，IP 被 shell 执行"的误判）→
/// 哨兵验证已进入目标主机 shell（失败则断开并带回回显上下文，便于 AI 判断
/// 是资产名错误、账号选择菜单还是 MFA 拦截）。
pub async fn create_session(
    state: &AppState,
    bastion_config_id: &str,
    host: &str,
) -> AppResult<String> {
    let host = host.trim();
    if host.is_empty() {
        return Err(AppError::InvalidInput("host 不能为空".into()));
    }

    // 快速前置检查：达上限直接拒绝。此时尚未开通道/走菜单，避免为一次注定失败的
    // 请求白耗一次堡垒机会话（真正的上限判定仍在登记时原子完成，防并发绕过）。
    if SESSIONS.lock().await.len() >= MAX_SESSIONS {
        return Err(AppError::InvalidInput(format!(
            "堡垒机会话数已达上限（{}）：请先用 bastion_close_session 关闭不再使用的会话，或等待空闲自动回收",
            MAX_SESSIONS
        )));
    }

    let config = find_bastion_config(state, bastion_config_id)?;
    // 复用基础连接开 channel（首次会连接并可能弹一次 MFA，之后不再弹）。
    let mut session = open_bastion_channel(state, &config).await?;

    // 等登录菜单稳定，并记下菜单提示符（末行）作为「是否真的进入了目标主机」的基线。
    let menu = wait_settle(&session, SETTLE_GRACE, LOGIN_SETTLE_TIMEOUT).await;
    let menu_prompt = last_nonempty_line(&crate::utils::strip_ansi(&menu));

    // 写入目标主机；只分析**新增**输出（避免把登录 banner 里的词误判成错误特征）。
    let base = session.total_output_bytes();
    write_line(&session, state, host).await?;
    // 等连接输出稳定（目标主机 banner / 提示符；主机不存在时此处是错误提示）。
    wait_settle(&session, SETTLE_GRACE, CONNECT_SETTLE_TIMEOUT).await;
    let (window, _) = session.snapshot_after(base);
    let connect_ctx = crate::utils::strip_ansi(&window);

    // 判定一：新增输出含失败特征。绑定的其实是**普通服务器**（非堡垒机）时，
    // 写入的资产名会被当 shell 命令执行并回显 "command not found"；资产不存在 /
    // 被拒绝时堡垒机也会回显错误。两种情况哨兵验证都会跑在错误的主机上。
    if connect_output_looks_failed(&connect_ctx) {
        let _ = session.close().await;
        return Err(AppError::Ssh(format!(
            "连接目标主机「{}」失败：堡垒机回显包含错误特征（绑定的会话可能是普通服务器而非堡垒机，或资产不存在/被拒绝）：\n{}",
            host,
            truncate(connect_ctx.trim())
        )));
    }

    // 判定二：提示符没变。真正进入目标主机后 shell 提示符必然与堡垒机菜单提示符
    // 不同；若逐字符相同，说明仍停在菜单上（该堡垒机用本地化 / 自定义文案回显
    // 失败时，上面的词表匹配不到，这里兜住），否则后续命令会静默执行在错误主机。
    let connected_prompt = last_nonempty_line(&connect_ctx);
    if !menu_prompt.is_empty() && connected_prompt == menu_prompt {
        let _ = session.close().await;
        return Err(AppError::Ssh(format!(
            "连接目标主机「{}」失败：堡垒机提示符未发生变化（仍停留在「{}」），判断未真正进入目标主机。\
请确认资产名 / IP 是否正确、是否需要先在菜单里选择。堡垒机回显：\n{}",
            host,
            menu_prompt,
            truncate(connect_ctx.trim())
        )));
    }

    // 哨兵验证：目标主机 shell 就绪后 `echo` 才会回显并输出哨兵。
    //
    // 必须判 completed：哨兵超时（命令只回显、没被执行）时 sentinel_exec_inner
    // 返回的是 Ok(未完成)，只判 Err 会把"根本没进到目标主机"的会话当成功返回，
    // 之后每条 bastion_session_exec 都在错误的位置执行并超时。
    match sentinel_exec_inner(
        &session,
        state,
        "true",
        SHELL_READY_TIMEOUT,
        EXEC_POLL_INTERVAL,
    )
    .await
    {
        Ok(outcome) if outcome.completed => {}
        Ok(outcome) => {
            let _ = session.close().await;
            return Err(AppError::Ssh(format!(
                "未能在目标主机「{}」上获得可用的 shell：已写入的验证命令只被回显、{} 秒内没有执行。\
常见原因：堡垒机菜单未接受主机名（提示符仍停在菜单）、或该会话需要先做选择。\
新建连接的完整回显：\n{}\n验证命令的回显：\n{}",
                host,
                SHELL_READY_TIMEOUT.as_secs(),
                truncate(connect_ctx.trim()),
                truncate(outcome.text.trim())
            )));
        }
        Err(e) => {
            let _ = session.close().await;
            return Err(AppError::Ssh(format!(
                "连接目标主机「{}」失败：{}。堡垒机回显（供诊断：资产名/IP 是否正确、是否弹出账号选择菜单、是否要求动态口令）：\n{}",
                host,
                e,
                truncate(connect_ctx.trim())
            )));
        }
    }

    // 可选的「登录后命令」：如 `sudo su -`，进入主机后执行一次；之后
    // bastion_session_exec 的命令都在其上下文（如 root 登录 shell）执行。
    // 注意不能用哨兵包装（见 run_login_command 的说明）。
    let login_cmd = options().post_login_command.trim().to_string();
    if !login_cmd.is_empty() {
        if let Err(e) = run_login_command(&session, state, &login_cmd).await {
            let _ = session.close().await;
            return Err(AppError::Ssh(format!(
                "已进入目标主机「{}」，但登录后命令「{}」未成功：{}",
                host, login_cmd, e
            )));
        }
        log::info!("[mcp] 已在目标主机 {} 上执行登录后命令: {}", host, login_cmd);
    }

    let session_id = uuid::Uuid::new_v4().simple().to_string();
    let wrapped = Arc::new(AsyncMutex::new(session));
    let now = Instant::now();
    // 上限检查与登记同锁原子完成（防并发 create 绕过开头的数量检查）。
    let count = {
        let mut map = SESSIONS.lock().await;
        if map.len() >= MAX_SESSIONS {
            None
        } else {
            map.insert(
                session_id.clone(),
                BastionSession {
                    session: wrapped.clone(),
                    host_label: host.to_string(),
                    created_at: now,
                    last_active: now,
                    busy: Arc::new(AsyncMutex::new(())),
                },
            );
            Some(map.len())
        }
    };
    match count {
        Some(n) => {
            log::info!(
                "[mcp] 堡垒机会话已创建: {} → {}（共 {} 个）",
                session_id,
                host,
                n
            );
            Ok(format!(
                "会话已建立。sessionId: {}（目标主机：{}；后续 bastion_session_exec / bastion_close_session 需携带该 id；空闲超过配置时长会自动回收）",
                session_id, host
            ))
        }
        None => {
            close_quietly(&wrapped).await;
            Err(AppError::InvalidInput(format!(
                "堡垒机会话数已达上限（{}）：请先用 bastion_close_session 关闭不再使用的会话，或等待空闲自动回收",
                MAX_SESSIONS
            )))
        }
    }
}

/// `bastion_session_exec`：在指定会话的 PTY 中哨兵式执行一条命令。
///
/// `timeout` 为单命令执行超时（工具参数 timeoutSeconds 解析而来，默认 30s）。
pub async fn session_exec(
    session_id: &str,
    command: &str,
    state: &AppState,
    timeout: Duration,
) -> AppResult<String> {
    if command.trim().is_empty() {
        return Err(AppError::InvalidInput("command 不能为空".into()));
    }
    // 取连接 Arc 与忙锁（不在 map 守卫跨 await：克隆出来后释放锁再执行）。
    let (session_arc, busy) = {
        let map = SESSIONS.lock().await;
        let s = map.get(session_id).ok_or_else(|| {
            AppError::NotFound(format!(
                "会话 {} 不存在（可能已被关闭或空闲回收）。可用会话见 bastion_list_sessions，或用 bastion_create_session 重新建立",
                session_id
            ))
        })?;
        (s.session.clone(), s.busy.clone())
    };
    // 同一会话串行执行（try 语义：并发调用立即报错而非排队）。
    let Ok(_guard) = busy.try_lock() else {
        return Err(AppError::InvalidInput(
            "该会话正有另一个命令在执行，请稍后再试".into(),
        ));
    };

    let result = {
        let session = session_arc.lock().await;
        sentinel_exec(&session, state, command, timeout).await
    };

    // 判定连接是否已断（写超时说明 PTY 已无响应，大概率被堡垒机单方面关闭）。
    let broken = if let Err(e) = &result {
        let msg = e.to_string();
        msg.contains("断开")
            || msg.contains("reader 已退出")
            || msg.contains("写入堡垒机会话超时")
    } else {
        false
    };

    // 断连：移除并关闭连接（直接 drop 不会中止 reader，连接会泄漏），给出可操作的错误。
    // remove_session 内部会顺带按需回收空闲的基础连接。
    if broken {
        if remove_session(session_id).await.is_some() {
            log::warn!("[mcp] 堡垒机会话 {} 已断开，自动移除", session_id);
        }
        return Err(AppError::Ssh(
            "会话连接已断开（堡垒机可能主动关闭或网络中断），该会话已移除；请重新 bastion_create_session".into(),
        ));
    }

    // 执行成功：刷新活跃时间（空闲回收依据）。
    if result.is_ok() {
        let mut map = SESSIONS.lock().await;
        if let Some(s) = map.get_mut(session_id) {
            s.last_active = Instant::now();
        }
    }
    result
}

/// `bastion_upload_file`：把 X-Term 所在主机的本地文件上传到会话所在的目标主机。
///
/// 走会话 shell（base64 分块追加到远端临时文件 → 解码到目标路径 → 校验字节数，
/// 远端有 sha256sum 时再比对哈希），因为绑定的 SSH 连接是**堡垒机**而非资产主机，
/// 没有到目标主机的 SFTP 通道。整段传输持有会话忙锁，避免与 `bastion_session_exec`
/// 交叉写同一个 PTY。
pub async fn upload_file(
    session_id: &str,
    local_path: &str,
    remote_path: &str,
    state: &AppState,
) -> AppResult<String> {
    let remote_path = remote_path.trim();
    if remote_path.is_empty() {
        return Err(AppError::InvalidInput("remotePath 不能为空".into()));
    }
    // 本地文件先读齐（含 base64 与 sha256），失败即早退，不占用会话。
    let prep = crate::mcp::upload::prepare(std::path::Path::new(local_path))?;

    let (session_arc, busy, host_label) = {
        let map = SESSIONS.lock().await;
        let s = map.get(session_id).ok_or_else(|| {
            AppError::NotFound(format!(
                "会话 {} 不存在（可能已被关闭或空闲回收）。可用会话见 bastion_list_sessions，或用 bastion_create_session 重新建立",
                session_id
            ))
        })?;
        (s.session.clone(), s.busy.clone(), s.host_label.clone())
    };
    let Ok(_guard) = busy.try_lock() else {
        return Err(AppError::InvalidInput(
            "该会话正有另一个命令/传输在执行，请稍后再试".into(),
        ));
    };

    touch_last_active(session_id).await;
    let result = {
        let session = session_arc.lock().await;
        do_upload(&session, state, &prep, remote_path).await
    };

    match result {
        Ok(verified) => {
            touch_last_active(session_id).await;
            log::info!(
                "[mcp] 上传成功：{} → {}（目标主机 {}，{} 字节，{}）",
                local_path,
                remote_path,
                host_label,
                prep.size,
                if verified { "sha256 校验一致" } else { "字节数校验一致" }
            );
            Ok(format!(
                "上传成功：{} → {}（目标主机 {}，{} 字节，{}）",
                local_path,
                remote_path,
                host_label,
                prep.size,
                if verified {
                    "已校验 sha256".to_string()
                } else {
                    "已校验字节数（未做 sha256 比对：远端无 sha256sum）".to_string()
                }
            ))
        }
        // 上传失败通常是远端侧问题（不可写 / base64 不可用 / 连接中断），会话本身
        // 通常仍可用，故不移除会话，把原始诊断信息交给调用方决定下一步。
        Err(e) => {
            // 例外：连接确实已断（写失败 / 写超时）时移除会话——与 session_exec 同一
            // 判定，否则后续调用只会在这条死连接上反复失败。
            let msg = e.to_string();
            let broken = msg.contains("断开")
                || msg.contains("reader 已退出")
                || msg.contains("写入堡垒机会话超时");
            if broken && remove_session(session_id).await.is_some() {
                log::warn!("[mcp] 堡垒机会话 {} 在传输中断开，自动移除", session_id);
                return Err(AppError::Ssh(
                    "会话连接已断开（堡垒机可能主动关闭或网络中断），该会话已移除；请重新 bastion_create_session".into(),
                ));
            }
            Err(e)
        }
    }
}

/// 刷新会话活跃时间：长传输期间不刷新会被空闲回收任务当成"没活动"。
async fn touch_last_active(session_id: &str) {
    let mut map = SESSIONS.lock().await;
    if let Some(s) = map.get_mut(session_id) {
        s.last_active = Instant::now();
    }
}

/// 执行 base64 上传：分块追加 → 校验临时文件 → 解码落盘 → 校验目标文件。
///
/// 返回 `Ok(true)` 表示连 sha256 都比对通过；`Ok(false)` 表示远端没有 sha256sum，
/// 只校验了字节数（解码失败与丢块已由前面步骤排除）。
async fn do_upload(
    session: &SshSession,
    state: &AppState,
    prep: &crate::mcp::upload::PreparedUpload,
    remote_path: &str,
) -> AppResult<bool> {
    use crate::mcp::upload as up;

    // 空文件：不经过临时文件与 base64 -d（0 字节时那条链路没有意义）。
    if prep.size == 0 {
        let out = run_upload_step(session, state, &up::create_empty_command(remote_path)).await?;
        if up::parse_size(&out) != Some(0) {
            return Err(AppError::Ssh(format!(
                "创建空文件失败（目标路径可能不可写）。远端回显：\n{}",
                truncate(out.trim())
            )));
        }
        return check_sha256(session, state, prep, remote_path).await;
    }

    // 1. 分块追加。每块单行、长度受控，且等到哨兵确认（shell 已回到提示符）
    //    才写下一条——两条约束共同保证终端输入队列不溢出丢字符。
    let deadline = Instant::now() + UPLOAD_TIMEOUT;
    let total_chunks = prep.b64.len().div_ceil(up::CHUNK_CHARS);
    for (idx, piece) in prep.b64.as_bytes().chunks(up::CHUNK_CHARS).enumerate() {
        if Instant::now() >= deadline {
            cleanup_temp(session, state, &prep.temp_path).await;
            return Err(AppError::Ssh(format!(
                "上传超时（{} 分钟，已完成 {}/{} 块）。可改用更小的文件或更强的网络后重试",
                UPLOAD_TIMEOUT.as_secs() / 60,
                idx,
                total_chunks
            )));
        }
        // base64 全为 ASCII，按字节切块不会破字符（失败在此不可达，保留清理以保持一致）。
        let Ok(piece) = std::str::from_utf8(piece) else {
            cleanup_temp(session, state, &prep.temp_path).await;
            return Err(AppError::Ssh("base64 分块编码异常".into()));
        };
        let cmd = up::append_command(piece, &prep.temp_path);
        if let Err(e) = run_upload_step(session, state, &cmd).await {
            cleanup_temp(session, state, &prep.temp_path).await;
            return Err(AppError::Ssh(format!(
                "第 {}/{} 块写入失败：{}",
                idx + 1,
                total_chunks,
                e
            )));
        }
    }

    // 2. 校验临时文件字节数：不等于 base64 长度说明有块丢失/重复（丢字符的唯一
    //    表现），此时立刻失败并清理，不冒险解码出半个文件。
    let expected_b64 = prep.b64.len() as u64;
    let out = match run_upload_step(session, state, &up::temp_size_command(&prep.temp_path)).await {
        Ok(out) => out,
        Err(e) => {
            cleanup_temp(session, state, &prep.temp_path).await;
            return Err(e);
        }
    };
    let Some(remote_b64) = up::parse_size(&out) else {
        cleanup_temp(session, state, &prep.temp_path).await;
        return Err(AppError::Ssh(format!(
            "无法读取远端临时文件大小（临时文件可能未创建，如 /tmp 不可写）。远端回显：\n{}",
            truncate(out.trim())
        )));
    };
    if remote_b64 != expected_b64 {
        cleanup_temp(session, state, &prep.temp_path).await;
        return Err(AppError::Ssh(format!(
            "传输不完整：远端临时文件 {} 字节，期望 {} 字节（可能有分块未落盘）。已清理临时文件，请重试",
            remote_b64, expected_b64
        )));
    }

    // 3. 解码落盘并校验目标文件字节数。
    let out = match run_upload_step(
        session,
        state,
        &up::finalize_command(&prep.temp_path, remote_path),
    )
    .await
    {
        Ok(out) => out,
        Err(e) => {
            cleanup_temp(session, state, &prep.temp_path).await;
            return Err(e);
        }
    };
    let Some(final_size) = up::parse_size(&out) else {
        cleanup_temp(session, state, &prep.temp_path).await;
        return Err(AppError::Ssh(format!(
            "解码或写入失败（目标路径不可写、或远端缺少 base64）。远端回显：\n{}",
            truncate(out.trim())
        )));
    };
    if final_size != prep.size {
        return Err(AppError::Ssh(format!(
            "目标文件字节数不符：远端 {} 字节，本地 {} 字节（已删除临时文件，请重试）",
            final_size, prep.size
        )));
    }

    // 4. 尽力比对 sha256（远端无 sha256sum 时跳过，字节数校验已兜底）。
    check_sha256(session, state, prep, remote_path).await
}

/// 远端有 sha256sum 时比对哈希；拿不到哈希则返回 false（只依赖字节数校验）。
///
/// 注意"拿不到"包含两种情况：远端没有 sha256sum，以及该步命令没能跑起来。两者都
/// **不判为上传失败**——此时文件已按字节数校验通过并落盘，只是少了哈希比对；
/// 真正的哈希不一致仍返回错误。
async fn check_sha256(
    session: &SshSession,
    state: &AppState,
    prep: &crate::mcp::upload::PreparedUpload,
    remote_path: &str,
) -> AppResult<bool> {
    use crate::mcp::upload as up;
    let out = match run_upload_step(session, state, &up::sha256_command(remote_path)).await {
        Ok(out) => out,
        Err(e) => {
            log::warn!("[mcp] 上传后的 sha256 校验未执行（忽略）: {}", e);
            return Ok(false);
        }
    };
    match up::parse_sha256(&out) {
        Some(hash) if hash == prep.sha256 => Ok(true),
        Some(hash) => Err(AppError::Ssh(format!(
            "校验和不一致：远端 {} ≠ 本地 {}（传输损坏）",
            hash, prep.sha256
        ))),
        None => Ok(false),
    }
}

/// 执行上传播路径中的一条命令，要求哨兵确认（完成）才返回其输出。
async fn run_upload_step(session: &SshSession, state: &AppState, cmd: &str) -> AppResult<String> {
    match sentinel_exec_inner(
        session,
        state,
        cmd,
        UPLOAD_STEP_TIMEOUT,
        UPLOAD_POLL_INTERVAL,
    )
    .await
    {
        Ok(outcome) if outcome.completed => Ok(outcome.text),
        Ok(outcome) => Err(AppError::Ssh(format!(
            "命令未获确认（只被回显或未执行）。远端回显：\n{}",
            truncate(outcome.text.trim())
        ))),
        Err(e) => Err(e),
    }
}

/// 尽力删除远端临时文件（忽略结果：清理失败不该掩盖原始错误）。
async fn cleanup_temp(session: &SshSession, state: &AppState, temp_path: &str) {
    let cmd = crate::mcp::upload::cleanup_command(temp_path);
    let _ = sentinel_exec_inner(session, state, &cmd, UPLOAD_STEP_TIMEOUT, UPLOAD_POLL_INTERVAL)
        .await;
}

/// `bastion_close_session`：关闭并移除指定会话。
///
/// 正在执行命令 / 传输的会话**直接拒绝**：`close` 需要拿到会话锁，而长任务
/// （如 base64 分块上传）会一直持有它——不拒绝的话这次 MCP 调用会一直挂到任务
/// 结束（最长 15 分钟），客户端早就超时了。
pub async fn close_session(session_id: &str) -> AppResult<String> {
    let busy = {
        let map = SESSIONS.lock().await;
        match map.get(session_id) {
            Some(s) => s.busy.clone(),
            None => {
                return Err(AppError::NotFound(format!(
                    "会话 {} 不存在（可能已被关闭或空闲回收）",
                    session_id
                )))
            }
        }
    };
    // 持有忙锁直到关闭完成：既确认当前没有任务在跑，也阻止新任务在关闭期间插进来。
    let Ok(_guard) = busy.try_lock() else {
        return Err(AppError::InvalidInput(
            "该会话正在执行命令或传输，请等它结束后再关闭（可用 bastion_list_sessions 查看状态）".into(),
        ));
    };
    match remove_session(session_id).await {
        Some(host) => {
            log::info!("[mcp] 堡垒机会话已关闭: {}（{}）", session_id, host);
            Ok(format!("会话 {}（目标主机：{}）已关闭。", session_id, host))
        }
        None => Err(AppError::NotFound(format!(
            "会话 {} 不存在（可能已被关闭或空闲回收）",
            session_id
        ))),
    }
}

/// `bastion_list_sessions`：列出当前活跃会话（只读）。
pub async fn list_sessions() -> String {
    let map = SESSIONS.lock().await;
    if map.is_empty() {
        return "当前没有活跃的堡垒机会话。".to_string();
    }
    let mut out = format!("当前活跃的堡垒机会话共 {} 个：\n", map.len());
    for (id, s) in map.iter() {
        out.push_str(&format!(
            "- sessionId: {} | 目标主机: {} | 存活: {} | 空闲: {}\n",
            id,
            s.host_label,
            human_duration(s.created_at.elapsed()),
            human_duration(s.last_active.elapsed())
        ));
    }
    out.push_str("提示：不再使用的会话请用 bastion_close_session 关闭；空闲超过配置时长会被自动回收。");
    out
}

/// 时长的人类可读形式（分秒）。
fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

// ===========================================================================
// 生命周期：空闲回收 / 停止服务全量断开
// ===========================================================================

/// 一轮扫描回收：
/// 1. 空闲超时的目标主机会话（`session_idle_minutes`）；
/// 2. 无目标会话且超过保持时长的基础连接（`base_idle_minutes`）。
async fn sweep() {
    let opts = options();

    // 1. 目标会话空闲回收（关掉会话后由 remove_session 顺带判断基础连接）。
    if opts.session_idle_minutes > 0 {
        let idle = Duration::from_secs(opts.session_idle_minutes as u64 * 60);
        let expired: Vec<String> = {
            let map = SESSIONS.lock().await;
            map.iter()
                .filter(|(_, s)| s.last_active.elapsed() >= idle)
                // 正在执行命令 / 传输的会话跳过：忙锁被持有说明有不活跃计时覆盖不到
                // 的长任务（如 base64 分块上传），回收它既会打断任务，也会让 close 卡在
                // 会话锁上把整个回收循环堵住。
                .filter(|(_, s)| s.busy.try_lock().is_ok())
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in expired {
            if let Some(host) = remove_session(&id).await {
                log::info!("[mcp] 堡垒机会话 {}（{}）空闲超时，自动回收", id, host);
            }
        }
    }

    // 2. 基础连接保持时长到期（无会话在用时才释放）。
    release_base_if_unused().await;
}

/// 启动后台回收任务（无目标会话 / 基础连接的空闲回收）。
///
/// 由 `start_mcp_server` 在 SSH MCP 以堡垒机模式启动时调用；重复调用会先停掉
/// 旧任务（服务重启场景）。参数经 [`configure`] 注入，每轮扫描都重新读取，
/// 因此任务本身总是启动（两项时长都为 0 时扫描为空操作，代价可忽略）。
pub fn start_reaper() {
    let mut reaper = REAPER.lock();
    if let Some(old) = reaper.take() {
        old.abort();
    }
    let join = tokio::spawn(async move {
        loop {
            tokio::time::sleep(REAPER_INTERVAL).await;
            sweep().await;
        }
    });
    *reaper = Some(join.abort_handle());
}

/// 停止回收任务并断开全部堡垒机会话（stop_mcp_server 时调用）。
///
/// 本函数经 `tauri::async_runtime::spawn` 异步执行，与紧随其后的
/// `mcp_start`（停止后立即重启）存在竞态：若 SSH MCP 已重新以堡垒机模式
/// 运行则**直接返回**——既不回收会话，也不动 reaper。
///
/// 注意「先判断、后 abort」的顺序很关键：新实例启动时会用自己的
/// [`start_reaper`] 覆盖 REAPER 句柄，若这里先无条件 abort 再判断，会把
/// **新实例的**回收任务掐掉，导致新实例的堡垒机会话再也不会空闲回收。
pub async fn stop_all() {
    if crate::mcp::server::ssh_bastion_mode_running() {
        log::info!("[mcp] SSH MCP 已以堡垒机模式重启，跳过会话回收与回收任务清理（新实例自管）");
        return;
    }
    if let Some(handle) = REAPER.lock().take() {
        handle.abort();
    }
    let taken: Vec<(String, BastionSession)> = SESSIONS.lock().await.drain().collect();
    if !taken.is_empty() {
        log::info!("[mcp] 停止 SSH MCP：断开 {} 个堡垒机会话", taken.len());
    }
    for (id, s) in taken {
        close_quietly(&s.session).await;
        log::info!(
            "[mcp] 堡垒机会话已随服务停止关闭: {}（{}）",
            id,
            s.host_label
        );
    }
    // 目标会话都已关闭，基础连接不会再被复用：显式释放（含断开传输层）。
    release_base().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_nonempty_line_picks_trailing_prompt() {
        // 末尾空行 / 空白行应被跳过，取最后一行有效内容并去首尾空白。
        assert_eq!(
            last_nonempty_line("welcome\n[Host]> \n\n  \n"),
            "[Host]>"
        );
        assert_eq!(
            last_nonempty_line("[Host]> \nLast login: x\nuser@10.0.0.5:~$ "),
            "user@10.0.0.5:~$"
        );
        // 全空 / 空串返回空串（调用方据此跳过提示符比较）。
        assert_eq!(last_nonempty_line("   \n\n"), "");
        assert_eq!(last_nonempty_line(""), "");
    }

    #[test]
    fn connect_failure_markers_match_command_not_found() {
        // 普通服务器把资产名当命令执行：应判为失败。
        assert!(connect_output_looks_failed(
            "bash: 10.0.0.5: command not found"
        ));
        assert!(connect_output_looks_failed("bash: 10.0.0.5: 未找到命令"));
        // 正常目标主机的 banner 含 denied/timeout 等词时**不应**误判
        // （特征表刻意只保留「命令未找到」族，正是为此）。
        assert!(!connect_output_looks_failed(
            "Unauthorized access denied. Session timeout: 30m\nLast login: Mon"
        ));
        assert!(!connect_output_looks_failed("user@web01:~$ "));
    }

    #[test]
    fn login_waiting_hint_detects_password_prompt() {
        // sudo 等待密码：末行是提示符且未回到 shell 提示符 → 命中。
        assert!(login_waiting_hint("[sudo] password for tom:").is_some());
        assert!(login_waiting_hint("root@web01:~# sudo su -\n[sudo] password for tom:").is_some());
        assert!(login_waiting_hint("请输入密码：").is_some());
        assert!(login_waiting_hint("Do you want to continue? [y/n]").is_some());
    }

    #[test]
    fn login_waiting_hint_ignores_normal_prompts() {
        // 已回到 shell 提示符（$ / # / >）→ 命令已结束，不判为等待输入。
        assert!(login_waiting_hint("tom@web01:~$ ").is_none());
        assert!(login_waiting_hint("sudo su -\n[root@web01 ~]# ").is_none());
        assert!(login_waiting_hint("tom@web01:/etc$ ").is_none());
        // 提示词出现在正常输出中间（非末行 / 末尾已回提示符）也不误判。
        assert!(login_waiting_hint("updated password for user tom\n[root@web01 ~]# ").is_none());
        // 空输出 / 超长末行（banner）不判。
        assert!(login_waiting_hint("").is_none());
        assert!(login_waiting_hint(&"x".repeat(200)).is_none());
    }
}
