//! russh 客户端封装。
//!
//! 本文件对 russh 0.45 的 `client` 模块做了一层薄封装，对外暴露：
//! - [`ClientHandler`]：实现 [`russh::client::Handler`] 的最小子集。
//! - [`AuthMethod`]：统一的认证参数（密码 / 私钥）。
//! - [`connect_direct`]：建立一条直连 SSH 通道并完成认证，返回可复用的
//!   [`russh::client::Handle`]。认证顺序：公钥 → 密码 → keyboard-interactive
//!   （支持二次认证：服务器弹出 OTP/验证码挑战时，通过 `ssh:auth_challenge`
//!   事件请前端填写）。
//! - [`load_private_key`]：从本地文件加载私钥。
//! - [`decode_private_key`]：从内存文本解析私钥。
//! - [`default_config`]：构造默认的客户端配置。
//!
//! # 安全说明
//! [`ClientHandler::check_server_key`] 采用 **TOFU（Trust On First Use）**
//! 策略：首次连接一台主机时静默接受其公钥并记录到 `known_hosts.json`；后续
//! 连接比对指纹，**仅在指纹与记录不符时**通过 `ssh:host_key_challenge` 事件
//! 弹窗请用户确认（接受并更新 / 仅本次接受 / 拒绝），以防范中间人攻击。
//!
//! # 关于 russh 0.45 API
//! russh 0.45 的私钥类型是 [`russh::keys::key::KeyPair`]（不是更新的
//! `PrivateKey`），公钥认证签名算法由 `KeyPair` 自身决定（RSA 走
//! [`SignatureHash`](russh::keys::key::SignatureHash)），无需像 main 分支
//! 那样显式传 `PrivateKeyWithHashAlg`。

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use russh::client::{self, Handle, KeyboardInteractiveAuthResponse, Msg, Session};
use russh::keys::key::{KeyPair, PublicKey};
use russh::Channel;

use crate::error::{AppError, AppResult};
use crate::events;
use crate::state::AppState;
use crate::storage::known_hosts;

// ===========================================================================
// ClientHandler
// ===========================================================================

/// russh 客户端事件处理器。
///
/// 当前关心两类事件：
/// - 服务器公钥校验（[`Handler::check_server_key`](client::Handler::check_server_key)）：
///   基于 known_hosts 做 TOFU 校验，指纹变更时弹窗。
/// - 远程端口转发的入站 channel（`server_channel_open_forwarded_tcpip`）：
///   查 [`Self::forwards`] 表桥接回本地目标。
///
/// `app` 句柄保留用于向前端 emit 事件。
///
/// 关联类型 `Error` 选用 [`russh::Error`]：russh 0.45 的 [`client::Handler`]
/// 要求 `Error: From<russh::Error>`，`russh::Error` 自身天然满足，避免为
/// 每个 handler 方法手写额外的 `From` 转换。
pub struct ClientHandler {
    /// Tauri 应用句柄，用于向前端 emit 事件。
    pub app: tauri::AppHandle,
    /// 目标主机名（用于 known_hosts 查找键与事件展示）。
    pub host: String,
    /// 目标端口。
    pub port: u16,
    /// known_hosts.json 的完整路径（由 `state.data_dir` 拼出）。
    pub known_hosts_path: PathBuf,
    /// 待确认的主机公钥变更注册表（与 [`AppState::pending_host_keys`] 共享同一 Arc）。
    pub pending_host_keys:
        Arc<parking_lot::Mutex<HashMap<String, tokio::sync::oneshot::Sender<HostKeyDecision>>>>,
    /// 远程端口转发注册表：远端 `(host,port)` -> 本地 `(host,port)`。
    /// 由 [`crate::ssh::tunnel::start_remote`] 登记，本 handler 的
    /// `server_channel_open_forwarded_tcpip` 回调查表桥接。每条 SSH 连接有独立
    /// 的 handler 实例，因此无需全局查找。
    pub forwards: Arc<parking_lot::Mutex<HashMap<(String, u16), (String, u16)>>>,
}

/// russh 0.45 的 `Handler` 是 `#[async_trait]`，因此 impl 上也需带上该属性。
#[async_trait]
impl client::Handler for ClientHandler {
    type Error = russh::Error;

    /// 校验服务器公钥（TOFU 策略）。
    ///
    /// 流程：
    /// 1. 计算 `name()` + `fingerprint()`；
    /// 2. 读 known_hosts：
    ///    - **无记录（首次）**：接受并写入记录；
    ///    - **记录匹配**：接受；
    ///    - **记录冲突**：emit `ssh:host_key_challenge`，阻塞等待前端决策
    ///      （超时 [`HOST_KEY_CHALLENGE_TIMEOUT`] 视为拒绝）。
    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        let key_type = server_public_key.name().to_string();
        let fingerprint = server_public_key.fingerprint();
        let key_str = known_hosts::host_key(&self.host, self.port);
        // data_dir = known_hosts_path 的父目录。
        let data_dir = match self.known_hosts_path.parent() {
            Some(p) => p,
            None => {
                log::error!("known_hosts_path 无父目录，跳过校验直接接受");
                return Ok(true);
            }
        };

        // 读 known_hosts。整个「读 → 比较 → 写」持 known_hosts 全局锁执行
        // （tokio Mutex guard 是 Send，可跨下方 .await 持有），防止并发连接时
        // load-modify-save 互相覆盖、丢失记录。
        let _kh_lock = known_hosts::lock().await;
        let hosts = match known_hosts::load(data_dir) {
            Ok(h) => h,
            Err(e) => {
                // 安全原则：文件损坏时绝不能按"首次连接"静默放行——那等于把
                // TOFU 静默降级，且下次保存会全量覆盖、丢掉所有已记录的主机指纹。
                log::error!("known_hosts 损坏，拒绝连接: {}", e);
                return Ok(false);
            }
        };

        match hosts.get(&key_str) {
            None => {
                // 首次连接：接受并记录。
                log::info!(
                    "首次连接 {}:{}，记录主机公钥 ({} {})",
                    self.host,
                    self.port,
                    key_type,
                    fingerprint
                );
                let mut updated = hosts;
                updated.insert(
                    key_str,
                    known_hosts::KnownHostEntry {
                        key_type: key_type.clone(),
                        fingerprint: fingerprint.clone(),
                    },
                );
                if let Err(e) = known_hosts::save(data_dir, &updated) {
                    log::warn!("写入 known_hosts 失败: {}", e);
                }
                Ok(true)
            }
            Some(entry) if entry.fingerprint == fingerprint => {
                // 指纹匹配：正常接受。
                log::debug!("主机公钥匹配 known_hosts 记录: {}:{}", self.host, self.port);
                Ok(true)
            }
            Some(entry) => {
                // 指纹冲突：可能 MITM 或服务器重装，弹窗请用户确认。
                log::warn!(
                    "主机 {}:{} 公钥变更（known_hosts: {} {}, 实际: {} {}）→ 弹窗确认",
                    self.host,
                    self.port,
                    entry.key_type,
                    entry.fingerprint,
                    key_type,
                    fingerprint
                );
                let decision = self
                    .ask_host_key_decision(entry.fingerprint.clone(), &key_type, &fingerprint)
                    .await;
                match decision {
                    HostKeyDecision::AcceptAndUpdate => {
                        let mut updated = hosts;
                        updated.insert(
                            key_str,
                            known_hosts::KnownHostEntry {
                                key_type: key_type.clone(),
                                fingerprint: fingerprint.clone(),
                            },
                        );
                        if let Err(e) = known_hosts::save(data_dir, &updated) {
                            log::warn!("更新 known_hosts 失败: {}", e);
                        }
                        Ok(true)
                    }
                    HostKeyDecision::AcceptOnce => Ok(true),
                    HostKeyDecision::Reject => Ok(false),
                }
            }
        }
    }

    /// 远程端口转发（-R）的入站回调。
    ///
    /// SSH 服务端在远端 `tcpip_forward` 监听到连接后，通过本回调把 channel 推给
    /// 客户端。本回调查 [`Self::forwards`] 表，找到对应的本地目标后桥接：
    /// 连接本地 TCP → `channel.into_stream()` → `copy_bidirectional`。
    ///
    /// `connected_address:connected_port` 是远端绑定的地址端口（与 `start_remote`
    /// 调用 `tcpip_forward` 时登记的 key 一致）。查不到映射说明该 channel 来源
    /// 异常，直接关闭（返回 Ok 让 russh 关闭 channel）。
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let port = u16::try_from(connected_port).unwrap_or(0);
        let target = self
            .forwards
            .lock()
            .get(&(connected_address.to_string(), port))
            .cloned();
        let (local_host, local_port) = match target {
            Some(t) => t,
            None => {
                log::warn!(
                    "收到远端转发 channel 但无对应映射: {}:{}（已关闭）",
                    connected_address,
                    connected_port
                );
                return Ok(());
            }
        };

        log::debug!(
            "远端转发入站: {}:{} -> {}:{}",
            connected_address,
            connected_port,
            local_host,
            local_port
        );
        tokio::spawn(async move {
            let mut tcp =
                match tokio::net::TcpStream::connect((local_host.as_str(), local_port)).await {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!(
                            "远端转发：连接本地 {}:{} 失败: {}",
                            local_host,
                            local_port,
                            e
                        );
                        return;
                    }
                };
            let mut stream = channel.into_stream();
            match tokio::io::copy_bidirectional(&mut tcp, &mut stream).await {
                Ok((up, down)) => log::debug!(
                    "远端转发通道结束: 本地 {}:{} 上行 {} 字节, 下行 {} 字节",
                    local_host,
                    local_port,
                    up,
                    down
                ),
                Err(e) => log::warn!("远端转发通道出错: {}", e),
            }
        });
        Ok(())
    }
}

// ===========================================================================
// 认证参数
// ===========================================================================

/// 统一的认证参数。
///
/// 调用方在 [`crate::ssh::session::resolve_credential`] 中根据会话配置解析出
/// 本枚举后，传给 [`connect_direct`] 完成认证。
#[derive(Debug)]
pub enum AuthMethod {
    /// 用户名 + 密码认证。
    Password(String),
    /// 公钥认证。`key_data` 是已解析好的 [`KeyPair`]；`passphrase` 字段保留
    /// 以便扩展（russh 在 `decode_secret_key` 阶段就已解密，这里实际不再需要口令）。
    PrivateKey {
        key_data: KeyPair,
        passphrase: Option<String>,
    },
}

// ===========================================================================
// 配置与连接
// ===========================================================================

/// 构造默认的 SSH 客户端配置。
///
/// `idle_timeout` 为客户端侧的空闲断开时长（与设置页"SSH 空闲断开时间"对应）：
/// 在此时长内没有收到服务端任何数据即自动断开；`None` 表示永不自动断开。
/// 传 `None` 可关闭该机制（对应设置值 0）。
///
/// 另外在主机密钥算法列表末尾追加 `ssh-rsa`，以兼容只提供 `ssh-rsa`（RSA/SHA-1）
/// 主机密钥的服务器（如部分 JumpServer 堡垒机）。现代算法（ed25519/ecdsa/rsa-sha2）
/// 仍排在前面，优先级不受影响。
pub fn default_config(idle_timeout: Option<Duration>) -> client::Config {
    client::Config {
        inactivity_timeout: idle_timeout,
        preferred: russh::Preferred {
            key: Cow::Borrowed(&[
                russh::keys::key::ED25519,
                russh::keys::key::ECDSA_SHA2_NISTP256,
                russh::keys::key::ECDSA_SHA2_NISTP521,
                russh::keys::key::RSA_SHA2_256,
                russh::keys::key::RSA_SHA2_512,
                russh::keys::key::SSH_RSA,
            ]),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// 等待用户填写二次认证挑战的最长时间（超时视为认证失败）。
const AUTH_CHALLENGE_TIMEOUT: Duration = Duration::from_secs(120);

/// 等待用户确认主机公钥变更的最长时间（超时视为拒绝连接）。
///
/// 比 auth challenge 长，因为用户可能需要比对指纹、思考是否信任。
const HOST_KEY_CHALLENGE_TIMEOUT: Duration = Duration::from_secs(120);

/// 默认空闲断开时间（分钟），与设置页默认值保持一致；读取设置失败时兜底。
fn default_ssh_idle_timeout_minutes() -> u32 {
    30
}

/// 前端对一次认证挑战的回复（通过 `ssh_auth_respond` 命令回传）。
#[derive(Debug)]
pub enum AuthChallengeReply {
    /// 用户填写了全部输入项，`responses` 与事件中 `prompts` 一一对应。
    Respond(Vec<String>),
    /// 用户取消认证。
    Cancel,
}

/// 前端对一次主机公钥变更确认的回复（通过 `ssh_host_key_respond` 命令回传）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyDecision {
    /// 接受新公钥，并更新 known_hosts 记录。
    AcceptAndUpdate,
    /// 仅本次接受，不更新记录。
    AcceptOnce,
    /// 拒绝连接。
    Reject,
}

impl ClientHandler {
    /// 发起一次主机公钥变更确认，阻塞等待前端决策。
    ///
    /// 通过 `ssh:host_key_challenge` 事件把新旧指纹发往前端，前端弹窗让用户选择；
    /// 同时在 [`Self::pending_host_keys`] 注册一个 oneshot，等待
    /// `ssh_host_key_respond` 命令回传 [`HostKeyDecision`]。超时或前端取消
    /// 均视为 [`HostKeyDecision::Reject`]。
    async fn ask_host_key_decision(
        &self,
        known_fingerprint: String,
        key_type: &str,
        fingerprint: &str,
    ) -> HostKeyDecision {
        let challenge_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = tokio::sync::oneshot::channel::<HostKeyDecision>();
        self.pending_host_keys
            .lock()
            .insert(challenge_id.clone(), tx);

        events::emit(
            &self.app,
            events::SSH_HOST_KEY_CHALLENGE,
            events::SshHostKeyEvent {
                challenge_id: challenge_id.clone(),
                host: self.host.clone(),
                port: self.port,
                key_type: key_type.to_string(),
                fingerprint: fingerprint.to_string(),
                known_fingerprint,
            },
        );

        let result = tokio::time::timeout(HOST_KEY_CHALLENGE_TIMEOUT, rx).await;

        // 无论结果如何都清理注册表（前端回传命令也会 remove，这里是兜底）。
        self.pending_host_keys.lock().remove(&challenge_id);

        match result {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) => {
                // oneshot 被 drop（前端关闭等）→ 视为拒绝。
                log::warn!("主机公钥确认 oneshot 被 drop，视为拒绝");
                HostKeyDecision::Reject
            }
            Err(_) => {
                log::warn!(
                    "等待主机公钥确认超时（{}s），视为拒绝",
                    HOST_KEY_CHALLENGE_TIMEOUT.as_secs()
                );
                HostKeyDecision::Reject
            }
        }
    }
}

/// 判断一条 keyboard-interactive 提示是否属于"密码"类，可用已保存的密码自动填充。
///
/// 覆盖常见英文提示（Password / Passphrase）与中文提示（口令 / 密码）。
/// OTP/验证码类提示（Verification code / OTP / MFA 等）不在此列，需用户输入。
fn looks_like_password(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    lower.contains("password")
        || lower.contains("passphrase")
        || lower.contains("口令")
        || lower.contains("密码")
}

/// 远程端口转发注册表类型：`Arc<Mutex<(远端 host,port) -> (本地 host,port)>>`。
///
/// 由 [`connect_direct_tunnel`] 创建并返回给调用方，调用方（[`crate::ssh::tunnel::start_remote`]）
/// 在 `tcpip_forward` 前向其中登记映射，[`ClientHandler::server_channel_open_forwarded_tcpip`]
/// 回调据此查表桥接。共享同一 Arc，故 handler 内部与外部调用方看到的是同一份表。
pub type ForwardsMap = Arc<parking_lot::Mutex<HashMap<(String, u16), (String, u16)>>>;

/// 建立一条直连 SSH 连接并完成认证。
///
/// 认证顺序（与常见 SSH 客户端一致，逐级回退）：
/// 1. 公钥认证（配置为私钥时）；
/// 2. 密码认证（配置为密码时）；
/// 3. 前两步都失败后回退到 keyboard-interactive：密码类提示用已保存的密码
///    自动填充，其余提示（OTP/验证码等二次认证）通过
///    [`events::SSH_AUTH_CHALLENGE`] 事件发往前端弹窗，等待用户填写后继续认证。
///
/// 客户端配置（含空闲断开时间）从 settings.json 的 `terminal.sshIdleTimeoutMinutes`
/// 读取：0 = 永不自动断开；否则按分钟映射为 russh 的 `inactivity_timeout`。
///
/// 认证失败时返回 [`AppError::Auth`]。
///
/// 返回的 `Handle` 可用于后续打开 channel、SFTP、端口转发等。
#[allow(clippy::too_many_arguments)]
pub async fn connect_direct(
    host: &str,
    port: u16,
    username: &str,
    session_config_id: &str,
    auth: AuthMethod,
    state: AppState,
) -> AppResult<Handle<ClientHandler>> {
    connect_direct_inner(host, port, username, session_config_id, auth, state, None).await
}

/// 与 [`connect_direct`] 相同，但额外返回远程转发注册表的共享句柄。
///
/// 供 [`crate::ssh::tunnel::start_remote`] 使用：调用方拿到 `forwards` 后即可
/// 在 `tcpip_forward` 之前登记映射，而 handler 的回调也能读到同一份表。
pub async fn connect_direct_tunnel(
    host: &str,
    port: u16,
    username: &str,
    session_config_id: &str,
    auth: AuthMethod,
    state: AppState,
) -> AppResult<(Handle<ClientHandler>, ForwardsMap)> {
    let forwards: ForwardsMap = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let handle = connect_direct_inner(
        host,
        port,
        username,
        session_config_id,
        auth,
        state,
        Some(forwards.clone()),
    )
    .await?;
    Ok((handle, forwards))
}

/// [`connect_direct`] / [`connect_direct_tunnel`] 的共享实现。
///
/// `forwards` 为 `Some` 时用调用方提供的注册表（隧道场景，需外部写入）；
/// 为 `None` 时内部新建一个空表（终端 / SFTP / MySQL 等场景，不需要 -R）。
#[allow(clippy::too_many_arguments)]
async fn connect_direct_inner(
    host: &str,
    port: u16,
    username: &str,
    session_config_id: &str,
    auth: AuthMethod,
    state: AppState,
    forwards: Option<ForwardsMap>,
) -> AppResult<Handle<ClientHandler>> {
    // 空闲断开时间：设置 0 表示永不断开（inactivity_timeout 为 None）。
    let idle_timeout = crate::config::settings_load_inner(&state)
        .map(|s| s.terminal.ssh_idle_timeout_minutes)
        .unwrap_or(default_ssh_idle_timeout_minutes());
    let idle_timeout = if idle_timeout == 0 {
        None
    } else {
        Some(Duration::from_secs(u64::from(idle_timeout) * 60))
    };

    let config = Arc::new(default_config(idle_timeout));
    let handler = ClientHandler {
        app: state.app.clone(),
        host: host.to_string(),
        port,
        known_hosts_path: crate::storage::known_hosts::known_hosts_path(&state.data_dir),
        pending_host_keys: state.pending_host_keys.clone(),
        forwards: forwards.unwrap_or_else(|| Arc::new(parking_lot::Mutex::new(HashMap::new()))),
    };
    let addr = (host, port);

    log::info!("正在连接 SSH {}@{}:{}...", username, host, port);
    let mut handle = client::connect(config, addr, handler)
        .await
        .map_err(|e| AppError::Ssh(format!("连接 SSH 失败: {}", e)))?;

    // 1/2. 公钥或密码认证（按配置的 AuthMethod 单次尝试）。
    let (mut authenticated, fallback_password) = match auth {
        AuthMethod::PrivateKey { key_data, .. } => {
            let key = Arc::new(key_data);
            let ok = handle
                .authenticate_publickey(username, key)
                .await
                .map_err(|e| AppError::Ssh(format!("公钥认证请求失败: {}", e)))?;
            (ok, None)
        }
        AuthMethod::Password(password) => {
            let ok = handle
                .authenticate_password(username, password.clone())
                .await
                .map_err(|e| AppError::Ssh(format!("密码认证请求失败: {}", e)))?;
            (ok, Some(password))
        }
    };

    // 3. 回退 keyboard-interactive（服务器只提供该方法，或要求二次认证）。
    if !authenticated {
        authenticated = auth_keyboard_interactive(
            &mut handle,
            host,
            port,
            username,
            session_config_id,
            fallback_password.as_deref(),
            &state,
        )
        .await?;
    }

    if !authenticated {
        return Err(AppError::Auth(format!(
            "SSH 认证失败: {}@{}:{}（若服务器启用了二次认证，请检查验证码输入或稍后重试）",
            username, host, port
        )));
    }

    log::info!("SSH 认证成功: {}@{}", username, host);
    Ok(handle)
}

/// keyboard-interactive 认证流程。
///
/// 循环处理服务器发来的 [`KeyboardInteractiveAuthResponse::InfoRequest`]：
/// - 密码类提示（[`looks_like_password`]）且本地有保存的密码 → 自动填充，
///   不发往前端（避免凭据泄露）；
/// - 其余提示（二次认证码等）→ 注册 oneshot 并 emit [`events::SSH_AUTH_CHALLENGE`]，
///   等待前端通过 `ssh_auth_respond` 回传；用户取消或超时返回认证错误。
///
/// 返回是否认证成功。
async fn auth_keyboard_interactive(
    handle: &mut Handle<ClientHandler>,
    host: &str,
    port: u16,
    username: &str,
    session_config_id: &str,
    password: Option<&str>,
    state: &AppState,
) -> AppResult<bool> {
    let mut reply = handle
        .authenticate_keyboard_interactive_start(username, None::<String>)
        .await
        .map_err(|e| AppError::Ssh(format!("keyboard-interactive 认证请求失败: {}", e)))?;

    loop {
        match reply {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                // 逐条决定答案：密码类提示自动填充，其余打上"需用户输入"标记。
                // manual 元素：(responses 中的占位下标, 提示文本, 是否回显)。
                let mut responses: Vec<String> = Vec::with_capacity(prompts.len());
                let mut manual: Vec<(usize, String, bool)> = Vec::new();
                for p in prompts {
                    if let Some(pw) = password.filter(|_| looks_like_password(&p.prompt)) {
                        responses.push(pw.to_string());
                    } else {
                        manual.push((responses.len(), p.prompt, p.echo));
                        responses.push(String::new()); // 占位，等用户回填
                    }
                }

                if manual.is_empty() {
                    // 全部自动填充（如纯密码的键盘交互，或服务器只发说明文字），
                    // 无需弹窗，直接提交继续。
                    reply = handle
                        .authenticate_keyboard_interactive_respond(responses)
                        .await
                        .map_err(|e| {
                            AppError::Ssh(format!("keyboard-interactive 提交失败: {}", e))
                        })?;
                    continue;
                }

                // 需要用户输入：发事件弹窗，等待 ssh_auth_respond 回传。
                let challenge_id = uuid::Uuid::new_v4().to_string();
                let (tx, rx) = tokio::sync::oneshot::channel::<AuthChallengeReply>();
                state
                    .pending_auth_challenges
                    .lock()
                    .insert(challenge_id.clone(), tx);

                let event_prompts: Vec<events::SshAuthPrompt> = manual
                    .iter()
                    .map(|&(_, ref prompt, echo)| events::SshAuthPrompt {
                        prompt: prompt.clone(),
                        echo,
                    })
                    .collect();
                events::emit(
                    &state.app,
                    events::SSH_AUTH_CHALLENGE,
                    events::SshAuthChallengeEvent {
                        challenge_id: challenge_id.clone(),
                        session_config_id: session_config_id.to_string(),
                        host: host.to_string(),
                        port,
                        username: username.to_string(),
                        name,
                        instructions,
                        prompts: event_prompts,
                    },
                );

                let wait = tokio::time::timeout(AUTH_CHALLENGE_TIMEOUT, rx).await;
                state.pending_auth_challenges.lock().remove(&challenge_id);

                let user_responses = match wait {
                    Ok(Ok(AuthChallengeReply::Respond(responses))) => responses,
                    Ok(Ok(AuthChallengeReply::Cancel)) => {
                        return Err(AppError::Auth(format!(
                            "二次认证已取消: {}@{}:{}",
                            username, host, port
                        )))
                    }
                    Ok(Err(_)) | Err(_) => {
                        return Err(AppError::Auth(format!(
                            "等待二次认证输入超时或连接已关闭: {}@{}:{}",
                            username, host, port
                        )))
                    }
                };

                if user_responses.len() != manual.len() {
                    return Err(AppError::Auth("二次认证答案数量与提示不匹配".into()));
                }

                // 把用户输入回填到占位位置，提交本轮答案。
                for ((idx, _, _), answer) in manual.into_iter().zip(user_responses) {
                    responses[idx] = answer;
                }
                reply = handle
                    .authenticate_keyboard_interactive_respond(responses)
                    .await
                    .map_err(|e| AppError::Ssh(format!("keyboard-interactive 提交失败: {}", e)))?;
            }
        }
    }
}

/// 从本地文件加载私钥。
///
/// 薄封装 [`russh::keys::load_secret_key`]，把 russh 的错误统一映射到
/// [`AppError::Ssh`]。`passphrase` 用于解密加密私钥，无加密时传 `None`。
pub fn load_private_key(path: &str, passphrase: Option<&str>) -> AppResult<KeyPair> {
    russh::keys::load_secret_key(path, passphrase)
        .map_err(|e| AppError::Ssh(format!("加载私钥失败 `{}`: {}", path, e)))
}

/// 从 PEM/OpenSSH 文本解析私钥。
///
/// 与 [`load_private_key`] 的区别：本函数接收**已读取到内存的私钥文本**，
/// 用于凭据保险库里以密文存储的私钥字符串。底层调用
/// [`russh::keys::decode_secret_key`]。
pub fn decode_private_key(text: &str, passphrase: Option<&str>) -> AppResult<KeyPair> {
    russh::keys::decode_secret_key(text, passphrase)
        .map_err(|e| AppError::Ssh(format!("解析私钥文本失败: {}", e)))
}
