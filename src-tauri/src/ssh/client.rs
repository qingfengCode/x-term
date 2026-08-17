//! russh 客户端封装。
//!
//! 本文件对 russh 0.45 的 `client` 模块做了一层薄封装，对外暴露：
//! - [`ClientHandler`]：实现 [`russh::client::Handler`] 的最小子集。
//! - [`AuthMethod`]：统一的认证参数（密码 / 私钥）。
//! - [`connect_direct`]：建立一条直连 SSH 通道并完成认证，返回可复用的
//!   [`russh::client::Handle`]。认证顺序：公钥 → 密码 → keyboard-interactive
//!   （支持二次认证：服务器弹出 OTP/验证码挑战时，通过 `ssh:auth_challenge`
//!   事件请前端填写；密码类提示自动填充，口令码/动态口令等验证码类提示
//!   不自动填充，留给用户手动输入；自动填充被拒后重试一轮全部手动输入）。
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
use russh::client::{
    self, DisconnectReason, Handle, KeyboardInteractiveAuthResponse, Msg, Session,
};
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
    /// 远程端口转发注册表：远端 `(host,port)` -> [`ForwardTarget`]（本地目标 +
    /// 桥接任务注册表）。
    /// 由 [`crate::ssh::tunnel::start_remote`] 登记，本 handler 的
    /// `server_channel_open_forwarded_tcpip` 回调查表桥接。每条 SSH 连接有独立
    /// 的 handler 实例，因此无需全局查找。
    pub forwards: ForwardsMap,
    /// 本连接是否正处于主机公钥确认等待中（与 `connect_direct_inner` 共享）。
    /// 置位期间连接超时暂停计（用户比对指纹需要完整时间窗口，决策自身有
    /// [`HOST_KEY_CHALLENGE_TIMEOUT`] 上限）。
    pub host_key_pending: Arc<parking_lot::Mutex<bool>>,
    /// 连接关闭信号（与 `connect_direct_inner` 共享）。
    ///
    /// russh 0.45 的 `wait_recv_keyboard_interactive_reply` 在认证通道关闭后
    /// 会无限忙旋（`_ => {}` 分支不处理 None），认证阶段通过监听本信号在
    /// 连接断开时立即中止，避免忙旋到认证总超时。
    pub connection_closed: Arc<tokio::sync::Notify>,
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
                // 安全原则：路径异常（如指向根/相对路径无父目录）时绝不能静默放行，
                // 否则等于把 TOFU 降级成无校验，存在 MITM 风险。
                log::error!("known_hosts_path 无父目录，拒绝连接");
                return Ok(false);
            }
        };

        // 第一阶段：持锁「读 → 比较」。锁**不能**跨过等待用户决策的 .await——
        // 一次弹窗最长 120s，若全程持 known_hosts 全局锁，所有并发连接（其他
        // tab、SFTP、隧道，即使是不同主机）都会被阻塞在锁上。
        let conflicting_known: Option<String> = {
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
                        key_str.clone(),
                        known_hosts::KnownHostEntry {
                            key_type: key_type.clone(),
                            fingerprint: fingerprint.clone(),
                        },
                    );
                    if let Err(e) = known_hosts::save(data_dir, &updated) {
                        log::warn!("写入 known_hosts 失败: {}", e);
                    }
                    return Ok(true);
                }
                Some(entry) if entry.fingerprint == fingerprint => {
                    // 指纹匹配：正常接受。
                    log::debug!("主机公钥匹配 known_hosts 记录: {}:{}", self.host, self.port);
                    return Ok(true);
                }
                Some(entry) => Some(entry.fingerprint.clone()),
            }
        };

        // 指纹冲突：可能 MITM 或服务器重装，**不持锁**弹窗请用户确认。
        let known_fingerprint = conflicting_known.unwrap_or_default();
        log::warn!(
            "主机 {}:{} 公钥变更（known_hosts: {}，实际: {} {}）→ 弹窗确认",
            self.host,
            self.port,
            known_fingerprint,
            key_type,
            fingerprint
        );
        let decision = self
            .ask_host_key_decision(known_fingerprint.clone(), &key_type, &fingerprint)
            .await;
        match decision {
            HostKeyDecision::AcceptAndUpdate => {
                // 第二阶段：重新持锁写回。确认期间其他连接可能已修改该记录，
                // 二次比对「当前记录仍是用户看到的旧指纹」（或已被删除）才允许
                // 更新，避免覆盖用户从未见过的变更。
                let _kh_lock = known_hosts::lock().await;
                let mut hosts = match known_hosts::load(data_dir) {
                    Ok(h) => h,
                    Err(e) => {
                        log::error!("known_hosts 损坏，拒绝连接: {}", e);
                        return Ok(false);
                    }
                };
                match hosts.get(&key_str) {
                    Some(entry) if entry.fingerprint == known_fingerprint => {}
                    None => {}
                    Some(entry) => {
                        log::warn!(
                            "known_hosts 记录在确认期间被并发修改（{} → {}），拒绝本次连接，请重连",
                            known_fingerprint,
                            entry.fingerprint
                        );
                        return Ok(false);
                    }
                }
                hosts.insert(
                    key_str,
                    known_hosts::KnownHostEntry {
                        key_type: key_type.clone(),
                        fingerprint: fingerprint.clone(),
                    },
                );
                if let Err(e) = known_hosts::save(data_dir, &hosts) {
                    log::warn!("更新 known_hosts 失败: {}", e);
                }
                Ok(true)
            }
            HostKeyDecision::AcceptOnce => Ok(true),
            HostKeyDecision::Reject => Ok(false),
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
        let (local_host, local_port, bridge_tasks) = match target {
            Some(t) => (t.host, t.port, t.bridge_tasks),
            None => {
                log::warn!(
                    "收到远端转发 channel 但无对应映射: {}:{}（已关闭）",
                    connected_address,
                    connected_port
                );
                return Ok(());
            }
        };

        // 并发上限：入站连接来自远端，不设上限会无界占用桥接任务。
        {
            let tasks = bridge_tasks.lock();
            if tasks.len() >= MAX_R_BRIDGES {
                log::warn!(
                    "远端转发 {}:{} 并发桥接已达上限（{}），拒绝新连接",
                    connected_address,
                    connected_port,
                    MAX_R_BRIDGES
                );
                return Ok(());
            }
        }

        log::debug!(
            "远端转发入站: {}:{} -> {}:{}",
            connected_address,
            connected_port,
            local_host,
            local_port
        );
        let bridge = tokio::spawn(async move {
            // 连接本地目标限时：本地服务不可达时 connect 可能挂数十秒。
            let mut tcp = match tokio::time::timeout(
                R_BRIDGE_CONNECT_TIMEOUT,
                tokio::net::TcpStream::connect((local_host.as_str(), local_port)),
            )
            .await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    log::warn!(
                        "远端转发：连接本地 {}:{} 失败: {}",
                        local_host,
                        local_port,
                        e
                    );
                    return;
                }
                Err(_) => {
                    log::warn!(
                        "远端转发：连接本地 {}:{} 超时（{} 秒）",
                        local_host,
                        local_port,
                        R_BRIDGE_CONNECT_TIMEOUT.as_secs()
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
        // 登记句柄，stop 时统一 abort（释放 Arc<Handle>）。
        bridge_tasks.lock().push(bridge);
        Ok(())
    }

    /// 连接断开（服务端主动断开或出错）时通知认证阶段的看门狗。
    ///
    /// 不向 russh 返回错误：russh 默认实现会把 `DisconnectReason::Error` 原样
    /// 回传（导致连接任务 join 报错），这里吞掉——断开本身对上层不是异常，
    /// 会话清理由各自的 reader 任务处理。
    async fn disconnected(
        &mut self,
        _reason: DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        self.connection_closed.notify_waiters();
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
pub enum AuthMethod {
    /// 用户名 + 密码认证。
    ///
    /// `otp` 为二次认证验证码（口令码/动态口令等，可选）：keyboard-interactive
    /// 流程会把它预填进首个验证码提示；为 `None` 时验证码类提示照常弹窗请用户填写。
    Password(PasswordAuth),
    /// 公钥认证。`key_data` 是已解析好的 [`KeyPair`]；`passphrase` 字段保留
    /// 以便扩展（russh 在 `decode_secret_key` 阶段就已解密，这里实际不再需要口令）。
    PrivateKey {
        key_data: KeyPair,
        passphrase: Option<String>,
    },
}

/// 密码认证参数。
pub struct PasswordAuth {
    /// 明文密码。
    pub password: String,
    /// 二次认证验证码（OTP / 口令码），可选。
    pub otp: Option<String>,
}

impl AuthMethod {
    /// 构造纯密码认证（不预填二次认证验证码）。
    pub fn password(pw: String) -> Self {
        AuthMethod::Password(PasswordAuth {
            password: pw,
            otp: None,
        })
    }
}

// 手写 Debug：派生实现会把密码 / 口令明文打进日志，这里脱敏。
// `KeyPair` 的 Debug 由 russh 提供且已隐藏私钥（只打印公钥类型/公钥），可直接展示。
impl std::fmt::Debug for AuthMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthMethod::Password(pa) => f
                .debug_struct("Password")
                .field("password", &"***")
                .field("otp", &pa.otp.as_ref().map(|_| "***"))
                .finish(),
            AuthMethod::PrivateKey { key_data, passphrase } => f
                .debug_struct("PrivateKey")
                .field("key_data", key_data)
                .field("passphrase", &passphrase.as_ref().map(|_| "***"))
                .finish(),
        }
    }
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
/// `keepalive_interval` 为保活间隔（与设置页"SSH 保活间隔"对应，等价于 OpenSSH
/// 的 `ServerAliveInterval`）：超过该时长未收到服务端数据即发送一个保活包。
/// 服务端收到后会回包，既证明连接仍存活（重置双方的空闲计时），又能让中间
/// NAT/防火墙的闲置超时不断被刷新，避免"挂机一会儿就断"。`None` 表示不保活
/// （对应设置值 0）。
///
/// 另外在主机密钥算法列表末尾追加 `ssh-rsa`，以兼容只提供 `ssh-rsa`（RSA/SHA-1）
/// 主机密钥的服务器（如部分 JumpServer 堡垒机）。现代算法（ed25519/ecdsa/rsa-sha2）
/// 仍排在前面，优先级不受影响。
pub fn default_config(
    idle_timeout: Option<Duration>,
    keepalive_interval: Option<Duration>,
) -> client::Config {
    client::Config {
        inactivity_timeout: idle_timeout,
        // 保活：超过 keepalive_interval 未收到服务端数据即发包探测；
        // 连续 keepalive_max 次无回应（服务端已死/网络中断）才断开。
        keepalive_interval,
        keepalive_max: 3,
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
///
/// 注意：认证整体另有 [`AUTH_TIMEOUT`] 总闸（先启动、先到期），实际生效的是
/// 总闸；本常量保留作为防御性兜底（总闸调整后仍能约束单轮挑战等待）。
const AUTH_CHALLENGE_TIMEOUT: Duration = Duration::from_secs(120);

/// 等待用户确认主机公钥变更的最长时间（超时视为拒绝连接）。
///
/// 与认证挑战同为 120s：用户可能需要比对指纹、思考是否信任。前端弹窗兜底
/// 115s 先于本值触发。
const HOST_KEY_CHALLENGE_TIMEOUT: Duration = Duration::from_secs(120);

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
        // 置位"确认等待中"：connect_direct_inner 据此暂停连接超时，
        // 保证用户比对指纹有完整时间窗口（本函数自身有 120s 上限）。
        *self.host_key_pending.lock() = true;

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

        // 无论结果如何都清理注册表（前端回传命令也会 remove，这里是兜底），
        // 并复位"确认等待中"标志。
        self.pending_host_keys.lock().remove(&challenge_id);
        *self.host_key_pending.lock() = false;

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

/// 判断一条 keyboard-interactive 提示是否属于"密码"类，可用已保存/手动输入的
/// 密码自动填充。
///
/// 覆盖常见英文提示（Password / Passphrase）与中文提示（口令 / 密码）。
/// OTP/验证码类提示（Verification code / OTP / MFA / 口令码 / 动态口令 / 短信口令
/// 等）不在此列，需用户输入——注意"口令码"等**含"口令"字样**的二次认证提示
/// 不能误判为密码，否则会被已保存密码自动填充，用户永远看不到输入框。
fn looks_like_password(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    if lower.contains("password") || lower.contains("passphrase") {
        return true;
    }
    if lower.contains("密码") {
        return true;
    }
    lower.contains("口令")
        && !lower.contains("口令码")
        && !lower.contains("动态口令")
        && !lower.contains("短信口令")
        && !lower.contains("验证口令")
}

/// 判断一条 keyboard-interactive 提示是否属于"验证码/OTP"类，可用手动输入的
/// 验证码自动填充（与 [`looks_like_password`] 互补，优先判密码再判验证码）。
fn looks_like_otp(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    lower.contains("verification")
        || lower.contains("passcode")
        || lower.contains("otp")
        || lower.contains("mfa")
        || lower.contains("two-factor")
        || lower.contains("2fa")
        || lower.contains("token")
        || lower.contains("验证码")
        || lower.contains("动态码")
        || lower.contains("口令码")
        || lower.contains("动态口令")
        || lower.contains("短信口令")
        || lower.contains("校验码")
        || lower.contains("安全码")
}

/// 待处理认证挑战的注册守卫。
///
/// 认证挑战注册到 [`AppState::pending_auth_challenges`] 后，等待前端回传期间
/// 存在**取消**路径：外层 [`AUTH_TIMEOUT`] 超时或连接断开看门狗会直接丢弃
/// 认证 future，此时等待点之后的清理代码不会执行。守卫在 Drop（任何退出
/// 路径）时移除注册条目，避免残留（前端回传命令侧的 remove 与之幂等）。
struct PendingAuthChallengeGuard<'a> {
    state: &'a AppState,
    challenge_id: String,
}

impl Drop for PendingAuthChallengeGuard<'_> {
    fn drop(&mut self) {
        self.state
            .pending_auth_challenges
            .lock()
            .remove(&self.challenge_id);
    }
}

/// 一条 -R 映射的本地目标 + 该映射下存量桥接任务的注册表。
///
/// 入站桥接任务由 [`ClientHandler::server_channel_open_forwarded_tcpip`] 登记，
/// [`crate::ssh::tunnel::stop`] 停止转发时统一 abort，释放桥接任务持有的
/// `Arc<Handle>`——否则一条空闲连接就足以让整条 SSH 连接永不释放。
#[derive(Clone)]
pub struct ForwardTarget {
    /// 本地目标主机。
    pub host: String,
    /// 本地目标端口。
    pub port: u16,
    /// 该映射下的存量桥接任务句柄。
    pub bridge_tasks: Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

/// 远程端口转发注册表类型：`Arc<Mutex<(远端 host,port) -> ForwardTarget>>`。
///
/// 由 [`connect_direct_tunnel`] 创建并返回给调用方，调用方（[`crate::ssh::tunnel::start_remote`]）
/// 在 `tcpip_forward` 前向其中登记映射，[`ClientHandler::server_channel_open_forwarded_tcpip`]
/// 回调据此查表桥接。共享同一 Arc，故 handler 内部与外部调用方看到的是同一份表。
pub type ForwardsMap = Arc<parking_lot::Mutex<HashMap<(String, u16), ForwardTarget>>>;

/// 单条 -R 映射允许的最大并发桥接数。
///
/// 入站连接来自远端（可能被公网扫描/滥用），不设上限会让桥接任务无界增长，
/// 耗尽内存并放大连接泄漏面；超限的新入站 channel 直接关闭（返回 Ok 让
/// russh 关闭 channel）。
const MAX_R_BRIDGES: usize = 32;

/// -R 入站桥接连接本地目标的最长等待时间。
///
/// 本地服务不可达（SYN 被丢）时 `TcpStream::connect` 可能挂数十秒，加超时兜底。
const R_BRIDGE_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 建立一条直连 SSH 连接并完成认证。
///
/// 认证顺序（与常见 SSH 客户端一致，逐级回退）：
/// 1. 公钥认证（配置为私钥时）；
/// 2. 密码认证（配置为密码时）；
/// 3. 前两步都失败后回退到 keyboard-interactive：密码类提示用已保存的密码
///    自动填充，其余提示（OTP/验证码等二次认证）通过
///    [`events::SSH_AUTH_CHALLENGE`] 事件发往前端弹窗，等待用户填写后继续认证。
///
/// 客户端配置（空闲断开、保活、连接超时）从 settings.json 的
/// `terminal.sshIdleTimeoutMinutes` / `terminal.sshKeepaliveSecs`
/// / `terminal.sshConnectTimeoutSecs` 读取：空闲断开 0 = 永不自动断开，否则按分钟
/// 映射为 russh 的 `inactivity_timeout`；保活 0 = 不发送保活包，否则按秒映射为
/// russh 的 `keepalive_interval`；连接超时 0 = 永不超时，否则 TCP 建连 +
/// SSH 握手（含密钥交换）超时即失败。
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
    // 终端设置（空闲断开 + 保活 + 连接超时）：读一次 settings.json 复用。
    let terminal = crate::config::settings_load_inner(&state)
        .map(|s| s.terminal)
        .unwrap_or_default();
    // 空闲断开时间：设置 0 表示永不断开（inactivity_timeout 为 None）。
    let idle_timeout = if terminal.ssh_idle_timeout_minutes == 0 {
        None
    } else {
        Some(Duration::from_secs(u64::from(terminal.ssh_idle_timeout_minutes) * 60))
    };
    // 保活间隔：设置 0 表示不发送保活包（keepalive_interval 为 None）。
    let keepalive_interval = if terminal.ssh_keepalive_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(u64::from(terminal.ssh_keepalive_secs)))
    };
    // 连接超时（秒）：设置 0 表示永不超时。
    let connect_timeout_secs = terminal.ssh_connect_timeout_secs;

    let config = Arc::new(default_config(idle_timeout, keepalive_interval));
    // 连接/认证阶段与 handler 共享的两个信号：
    // - host_key_pending：连接期间是否正在等待主机公钥确认（超时暂停计）；
    // - connection_closed：连接断开通知（认证阶段看门狗用）。
    let host_key_pending = Arc::new(parking_lot::Mutex::new(false));
    let connection_closed = Arc::new(tokio::sync::Notify::new());
    let handler = ClientHandler {
        app: state.app.clone(),
        host: host.to_string(),
        port,
        known_hosts_path: crate::storage::known_hosts::known_hosts_path(&state.data_dir),
        pending_host_keys: state.pending_host_keys.clone(),
        forwards: forwards.unwrap_or_else(|| Arc::new(parking_lot::Mutex::new(HashMap::new()))),
        host_key_pending: host_key_pending.clone(),
        connection_closed: connection_closed.clone(),
    };
    let addr = (host.to_string(), port);

    log::info!("正在连接 SSH {}@{}:{}...", username, host, port);
    // connect 放入独立任务：连接期间若进入主机公钥确认（等待用户决策，自身
    // 有 120s 上限），连接超时暂停计——指纹弹窗不能被短连接超时截断。
    let connect_task = tokio::spawn(client::connect(config, addr, handler));
    let mut handle = match connect_timeout_secs {
        // 0 = 永不超时。
        0 => match connect_task.await {
            Ok(res) => res.map_err(|e| AppError::Ssh(format!("连接 SSH 失败: {}", e)))?,
            Err(e) => return Err(e.into()),
        },
        secs => {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(secs));
            loop {
                if connect_task.is_finished() {
                    break match connect_task.await {
                        Ok(res) => {
                            res.map_err(|e| AppError::Ssh(format!("连接 SSH 失败: {}", e)))?
                        }
                        Err(e) => return Err(e.into()),
                    };
                }
                // 主机公钥确认等待用户决策期间，连接超时暂停计。
                if *host_key_pending.lock() {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
                if tokio::time::Instant::now() >= deadline {
                    connect_task.abort();
                    return Err(AppError::Ssh(format!(
                        "连接 SSH 超时（{} 秒，可在设置中调整）",
                        secs
                    )));
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };

    // 认证阶段（密码/公钥 + 键盘交互回退）：
    // 1. 整体超时 120s 兜底——服务器握手后不回应认证请求时认证会永久挂起。
    //    前端认证弹窗兜底 115s 先于该值触发，用户输入有完整时间窗口；
    // 2. 并行监听连接关闭信号——russh 0.45 在认证通道关闭后会忙旋等待
    //    （wait_recv_keyboard_interactive_reply 不处理 recv None），连接一断
    //    立即中止，避免忙旋到整体超时。
    let handle = tokio::select! {
        r = tokio::time::timeout(AUTH_TIMEOUT, async {
            // 1/2. 公钥或密码认证（按配置的 AuthMethod 单次尝试）。
            let (mut authenticated, fallback_password, fallback_otp) = match auth {
                AuthMethod::PrivateKey { key_data, .. } => {
                    let key = Arc::new(key_data);
                    let ok = handle
                        .authenticate_publickey(username, key)
                        .await
                        .map_err(|e| AppError::Ssh(format!("公钥认证请求失败: {}", e)))?;
                    (ok, None, None)
                }
                AuthMethod::Password(pa) => {
                    let ok = handle
                        .authenticate_password(username, pa.password.clone())
                        .await
                        .map_err(|e| AppError::Ssh(format!("密码认证请求失败: {}", e)))?;
                    (ok, Some(pa.password), pa.otp)
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
                    fallback_otp.as_deref(),
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
            Ok::<_, AppError>(handle)
        }) => {
            match r {
                Ok(res) => res?,
                Err(_) => {
                    return Err(AppError::Ssh(
                        "SSH 认证超时（120 秒）：服务器未完成认证握手，已断开连接".into(),
                    ))
                }
            }
        },
        _ = connection_closed.notified() => {
            return Err(AppError::Ssh("SSH 连接已断开，认证中止".into()));
        }
    };

    Ok(handle)
}

/// keyboard-interactive 认证流程。
///
/// 循环处理服务器发来的 [`KeyboardInteractiveAuthResponse::InfoRequest`]：
/// - 密码类提示（且本地有密码）→ 自动填充（仅首次尝试的首轮、首个未回显的
///   密码提示，与 OpenSSH 行为一致），不发往前端（避免凭据泄露）；
/// - 验证码类提示（口令码/动态口令等，且调用方提供了验证码）→ 自动填充首个；
/// - 其余提示（二次认证码等）→ 注册 oneshot 并 emit [`events::SSH_AUTH_CHALLENGE`]，
///   等待前端通过 `ssh_auth_respond` 回传；用户取消或超时返回认证错误。
///
/// 自动填充的尝试被服务器拒绝（`Failure`）时，自动重试一轮**全部手动输入**
/// （不再自动填充任何提示），给用户改正密码/口令码的机会——常见场景：已保存的
/// 密码过期、服务器要求验证码等；全部手动输入仍被拒绝才返回认证失败。
///
/// 返回是否认证成功。
/// 认证阶段整体超时。
///
/// 时序约定：前端认证弹窗 115s 兜底先触发（回传取消），后端 [`AUTH_TIMEOUT`]
/// 总闸 120s 后触发——保证用户有完整的输入窗口，同时防止服务器接受 TCP
/// 握手后不回应认证请求导致永久挂起。
const AUTH_TIMEOUT: Duration = Duration::from_secs(120);

/// keyboard-interactive 最大轮次。
///
/// 恶意/故障服务器可反复发 `InfoRequest`（每轮都能自动填充时无需用户介入，
/// 会形成无限往返忙循环），设上限兜底。
const MAX_KI_ROUNDS: usize = 5;

async fn auth_keyboard_interactive(
    handle: &mut Handle<ClientHandler>,
    host: &str,
    port: u16,
    username: &str,
    session_config_id: &str,
    password: Option<&str>,
    otp: Option<&str>,
    state: &AppState,
) -> AppResult<bool> {
    // 首次尝试可自动填充密码/验证码；被服务器拒绝后（manual_only 置位）重试
    // 一轮全部手动输入——提示全部弹窗由用户填写，不再自动填充。
    let mut manual_only = false;
    let mut rounds = 0usize;
    loop {
        rounds += 1;
        if rounds > MAX_KI_ROUNDS {
            return Err(AppError::Auth(format!(
                "keyboard-interactive 认证轮次超过上限（{}），服务器可能异常",
                MAX_KI_ROUNDS
            )));
        }
        let mut reply = handle
            .authenticate_keyboard_interactive_start(username, None::<String>)
            .await
            .map_err(|e| AppError::Ssh(format!("keyboard-interactive 认证请求失败: {}", e)))?;

        // 处理本次尝试内的所有 InfoRequest 轮次。
        let mut ki_round = 0usize;
        loop {
            match reply {
                KeyboardInteractiveAuthResponse::Success => return Ok(true),
                KeyboardInteractiveAuthResponse::Failure => {
                    if !manual_only {
                        // 自动填充（含已保存密码）被服务器拒绝：重试一轮全部手动
                        // 输入，让用户有机会修正密码/口令码（OpenSSH 也会
                        // "Permission denied, please try again" 重新提示）。
                        log::warn!(
                            "keyboard-interactive 认证被拒，重试一轮全部手动输入: {}@{}:{}",
                            username,
                            host,
                            port
                        );
                        manual_only = true;
                        break;
                    }
                    return Ok(false);
                }
                KeyboardInteractiveAuthResponse::InfoRequest {
                    name,
                    instructions,
                    prompts,
                } => {
                    ki_round += 1;
                    rounds += 1;
                    if rounds > MAX_KI_ROUNDS {
                        return Err(AppError::Auth(format!(
                            "keyboard-interactive 认证轮次超过上限（{}），服务器可能异常",
                            MAX_KI_ROUNDS
                        )));
                    }

                    // 逐条决定答案：密码/验证码类提示自动填充，其余打上"需用户输入"标记。
                    // manual 元素：(responses 中的占位下标, 提示文本, 是否回显)。
                    //
                    // 自动填充仅限首次尝试的首轮（与 OpenSSH 行为一致）：强制改密
                    // 等流程会出现 "New password:" / "Confirm password:" 等提示，
                    // 无差别填充会把旧密码填进新密码框破坏流程；后续轮次也不再填充。
                    let can_prefill = !manual_only && ki_round == 1;
                    let mut responses: Vec<String> = Vec::with_capacity(prompts.len());
                    let mut manual: Vec<(usize, String, bool)> = Vec::new();
                    let mut password_autofilled = false;
                    let mut otp_autofilled = false;
                    for p in prompts {
                        if can_prefill && !password_autofilled && !p.echo
                            && looks_like_password(&p.prompt)
                        {
                            if let Some(pw) = password.filter(|pw| !pw.is_empty()) {
                                password_autofilled = true;
                                responses.push(pw.to_string());
                                continue;
                            }
                        }
                        if can_prefill && !otp_autofilled && looks_like_otp(&p.prompt) {
                            if let Some(code) = otp.filter(|c| !c.is_empty()) {
                                otp_autofilled = true;
                                responses.push(code.to_string());
                                continue;
                            }
                        }
                        manual.push((responses.len(), p.prompt, p.echo));
                        responses.push(String::new()); // 占位，等用户回填
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
                    // 守卫保证注册条目在所有退出路径（含外层超时/看门狗取消）
                    // 都被清理，不残留。
                    let challenge_id = uuid::Uuid::new_v4().to_string();
                    let (tx, rx) = tokio::sync::oneshot::channel::<AuthChallengeReply>();
                    state
                        .pending_auth_challenges
                        .lock()
                        .insert(challenge_id.clone(), tx);
                    let guard = PendingAuthChallengeGuard {
                        state,
                        challenge_id: challenge_id.clone(),
                    };

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
                    drop(guard); // 正常路径显式清理（与守卫 Drop、回传命令的 remove 均幂等）。

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
