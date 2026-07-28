//! russh 客户端封装。
//!
//! 本文件对 russh 0.45 的 `client` 模块做了一层薄封装，对外暴露：
//! - [`ClientHandler`]：实现 [`russh::client::Handler`] 的最小子集。
//! - [`AuthMethod`]：统一的认证参数（密码 / 私钥）。
//! - [`connect_direct`]：建立一条直连 SSH 通道并完成认证，返回可复用的
//!   [`russh::client::Handle`]。
//! - [`load_private_key`]：从本地文件加载私钥。
//! - [`decode_private_key`]：从内存文本解析私钥。
//! - [`default_config`]：构造默认的客户端配置。
//!
//! # 安全说明
//! 当前 [`ClientHandler::check_server_key`] 简化为**无条件接受**所有服务器公钥
//! （MVP 阶段优先打通流程）。生产环境应改为基于 `known_hosts` 的严格校验，
//! 首次连接时提示用户指纹确认，并在后续连接中比对，以防止中间人攻击。
//!
//! # 关于 russh 0.45 API
//! russh 0.45 的私钥类型是 [`russh::keys::key::KeyPair`]（不是更新的
//! `PrivateKey`），公钥认证签名算法由 `KeyPair` 自身决定（RSA 走
//! [`SignatureHash`](russh::keys::key::SignatureHash)），无需像 main 分支
//! 那样显式传 `PrivateKeyWithHashAlg`。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use russh::client::{self, Handle};
use russh::keys::key::{KeyPair, PublicKey};

use crate::error::{AppError, AppResult};

// ===========================================================================
// ClientHandler
// ===========================================================================

/// russh 客户端事件处理器。
///
/// 当前只关心服务器公钥校验（[`Handler::check_server_key`](client::Handler::check_server_key)），
/// 其余事件由 russh 的默认实现处理。`app` 句柄保留下来，供将来在
/// host-key 变更告警、异步通知等场景下向前端发送事件。
///
/// 关联类型 `Error` 选用 [`russh::Error`]：russh 0.45 的 [`client::Handler`]
/// 要求 `Error: From<russh::Error>`，`russh::Error` 自身天然满足，避免为
/// 每个 handler 方法手写额外的 `From` 转换。
pub struct ClientHandler {
    /// Tauri 应用句柄，用于向前端 emit 事件。
    pub app: tauri::AppHandle,
}

/// russh 0.45 的 `Handler` 是 `#[async_trait]`，因此 impl 上也需带上该属性。
#[async_trait]
impl client::Handler for ClientHandler {
    type Error = russh::Error;

    /// 校验服务器公钥。
    ///
    /// **MVP 实现：无条件接受。** 真正的指纹比对留待后续接入 known_hosts 时实现。
    /// 这里通过 `log::warn!` 记录一条告警，方便调试时观测到主机公钥。
    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        log::warn!(
            "接受服务器公钥（未做 known_hosts 校验）: {}",
            server_public_key.fingerprint()
        );
        Ok(true)
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
/// 主要设置空闲超时为 300 秒，其余保持 russh 默认（算法偏好、窗口大小等）。
pub fn default_config() -> client::Config {
    client::Config {
        inactivity_timeout: Some(Duration::from_secs(300)),
        ..Default::default()
    }
}

/// 建立一条直连 SSH 连接并完成认证。
///
/// 流程：
/// 1. 调用 [`client::connect`] 完成 TCP 建链 + 密钥交换，得到一个
///    [`Handle`]（[`ClientHandler`] 作为事件处理器被消费）。
/// 2. 根据 [`AuthMethod`] 调用 `authenticate_password` 或
///    `authenticate_publickey`；认证失败时返回 [`AppError::Auth`]。
///
/// 返回的 `Handle` 可用于后续打开 channel、SFTP、端口转发等。
pub async fn connect_direct(
    host: &str,
    port: u16,
    username: &str,
    auth: AuthMethod,
    app: tauri::AppHandle,
) -> AppResult<Handle<ClientHandler>> {
    let config = Arc::new(default_config());
    let handler = ClientHandler { app };
    let addr = (host, port);

    log::info!("正在连接 SSH {}@{}:{}...", username, host, port);
    let mut handle = client::connect(config, addr, handler)
        .await
        .map_err(|e| AppError::Ssh(format!("连接 SSH 失败: {}", e)))?;

    let success = match auth {
        AuthMethod::Password(password) => handle
            .authenticate_password(username, password)
            .await
            .map_err(|e| AppError::Ssh(format!("密码认证请求失败: {}", e)))?,
        AuthMethod::PrivateKey { key_data, .. } => {
            let key = Arc::new(key_data);
            handle
                .authenticate_publickey(username, key)
                .await
                .map_err(|e| AppError::Ssh(format!("公钥认证请求失败: {}", e)))?
        }
    };

    if !success {
        return Err(AppError::Auth(format!(
            "SSH 认证失败: {}@{}:{}",
            username, host, port
        )));
    }

    log::info!("SSH 认证成功: {}@{}", username, host);
    Ok(handle)
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
