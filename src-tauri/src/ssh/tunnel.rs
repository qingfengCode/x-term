//! SSH 端口转发（tunnel）封装。
//!
//! 支持三类转发：
//! - [`TunnelKind::Local`]（`-L`）：本地端口转发到远程目标。
//! - [`TunnelKind::Remote`]（`-R`）：远程端口转发到本地目标。服务端在远端
//!   监听，入站连接通过 SSH channel 推回客户端，由
//!   [`ClientHandler::server_channel_open_forwarded_tcpip`] 桥接到本地。
//! - [`TunnelKind::Dynamic`]（`-D`）：本地 SOCKS5 代理，按每个连接动态解析目标。
//!
//! 本地转发的实现：在 `local_host:local_port` 上监听 TCP，对每一条入站
//! 连接调用 `channel_open_direct_tcpip` 建立一条到 `remote_host:remote_port`
//! 的 SSH channel，再用 `tokio::io::copy_bidirectional` 双向桥接。

use russh::client::Handle;
use serde::{Deserialize, Serialize};
use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::error::{AppError, AppResult};
use crate::ssh::client::ClientHandler;

// ===========================================================================
// 数据模型
// ===========================================================================

/// 转发类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TunnelKind {
    /// 本地端口转发（`-L`）。
    Local,
    /// 远程端口转发（`-R`）。
    Remote,
    /// 动态 SOCKS5 转发（`-D`）。
    Dynamic,
}

/// 一条转发规则的静态描述。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelSpec {
    pub id: String,
    pub session_id: String,
    pub kind: TunnelKind,
    pub local_host: String,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
}

/// 一条正在运行的转发。
///
/// - `listener`：本地监听器。注意 `start_local` / `start_dynamic` 会把监听器
///   所有权移入后台 accept 循环，故此字段在这两类转发返回后为 `None`；保留是
///   为了兼容将来直接持有监听器的实现。
/// - `stop_tx`：发送一次即通知 accept 循环优雅退出。本地/动态转发用；远程
///   转发没有本地 accept 循环，此字段仍创建但发送后无消费者（无害）。
/// - `accept_task`：accept 循环的任务句柄，`stop` 时 abort 以强制释放端口。
///   远程转发没有本地 accept 任务，为 `None`。
/// - `remote_forward`：仅远程转发使用：stop 时据此调用
///   [`Handle::cancel_tcpip_forward`] 取消远端监听，并清理 ClientHandler 中的
///   转发映射。本地/动态转发为 `None`。
pub struct Tunnel {
    pub spec: TunnelSpec,
    pub listener: Option<TcpListener>,
    pub stop_tx: oneshot::Sender<()>,
    pub accept_task: Option<tokio::task::JoinHandle<()>>,
    pub remote_forward: Option<RemoteForwardHandle>,
}

/// 远程转发专用句柄（由 [`start_remote`] 填充）。
///
/// 持有 SSH 连接的 `Handle`（包在 Arc 中供 stop 异步调用）和远端监听的
/// `(host, port)`，stop 时据此取消远端监听。
pub struct RemoteForwardHandle {
    pub handle: std::sync::Arc<Handle<ClientHandler>>,
    pub remote_host: String,
    pub remote_port: u32,
}

// ===========================================================================
// 本地端口转发
// ===========================================================================

/// 启动一条本地端口转发。
///
/// 绑定 `spec.local_host:spec.local_port`，对每个入站连接：
/// 1. `handle.channel_open_direct_tcpip(remote_host, remote_port, origin, origin_port)`
/// 2. `channel.into_stream()` 转成 `AsyncRead + AsyncWrite`
/// 3. spawn 一个 task，用 [`copy_bidirectional`] 与入站 TCP 流双向桥接。
///
/// 返回的 [`Tunnel`] 通过 [`stop`] 关闭。accept 循环同时 select 一个
/// `oneshot::Receiver<()>`，收到信号即退出。
///
/// 注意：russh 0.45 的 [`Handle`] 未实现 `Clone`，而转发任务的生命周期长于
/// 调用栈，因此本函数**接管 `handle` 的所有权**。若调用方还需在同一连接上
/// 打开交互式终端 / SFTP，请先克隆出独立的 `Handle`（russh 的设计是每路用途
/// 各占一个 `Handle`，由同一底层连接复用）。
pub async fn start_local(handle: Handle<ClientHandler>, spec: TunnelSpec) -> AppResult<Tunnel> {
    let bind_addr = format!("{}:{}", spec.local_host, spec.local_port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| AppError::Ssh(format!("绑定本地监听 {} 失败: {}", bind_addr, e)))?;
    log::info!(
        "本地转发监听已启动: {} -> {}:{}",
        bind_addr,
        spec.remote_host,
        spec.remote_port
    );

    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();

    let remote_host = spec.remote_host.clone();
    let remote_port = spec.remote_port as u32;
    // 把 handle 包进 Arc，accept 循环里每条入站连接克隆一份 Arc 使用。
    let handle_arc = std::sync::Arc::new(handle);

    let accept_task = tokio::spawn(async move {
        loop {
            // accept 与 stop 信号二选一。
            tokio::select! {
                biased;

                _ = &mut stop_rx => {
                    log::info!("本地转发 {} 停止", bind_addr);
                    return;
                }

                accept = listener.accept() => {
                    let (mut tcp, peer) = match accept {
                        Ok(v) => v,
                        Err(e) => {
                            log::warn!("accept 失败 ({}): {}", bind_addr, e);
                            continue;
                        }
                    };

                    let handle = handle_arc.clone();
                    let remote_host = remote_host.clone();
                    tokio::spawn(async move {
                        let origin_host = peer.ip().to_string();
                        let origin_port = peer.port() as u32;

                        let channel = match handle
                            .channel_open_direct_tcpip(
                                remote_host.clone(),
                                remote_port,
                                origin_host,
                                origin_port,
                            )
                            .await
                        {
                            Ok(c) => c,
                            Err(e) => {
                                log::warn!("channel_open_direct_tcpip 失败: {}", e);
                                return;
                            }
                        };

                        // channel 转为 AsyncRead + AsyncWrite 流，与 TCP 双向复制。
                        let mut stream = channel.into_stream();
                        match copy_bidirectional(&mut tcp, &mut stream).await {
                            Ok((up, down)) => {
                                log::debug!(
                                    "转发通道结束: 上行 {} 字节, 下行 {} 字节",
                                    up, down
                                );
                            }
                            Err(e) => {
                                log::warn!("转发通道出错: {}", e);
                            }
                        }
                    });
                }
            }
        }
    });

    // listener 所有权已移入上面的 accept 循环，故此处 listener 字段为 None。
    Ok(Tunnel {
        spec,
        listener: None,
        stop_tx,
        accept_task: Some(accept_task),
        remote_forward: None,
    })
}

// ===========================================================================
// 远程端口转发（-R）
// ===========================================================================

/// 启动一条远程端口转发（`-R`）。
///
/// 流程：
/// 1. 向 `forwards` 表登记 `(remote_host, remote_port) -> (local_host, local_port)`，
///    供 [`ClientHandler::server_channel_open_forwarded_tcpip`] 回调查表桥接；
/// 2. 调用 [`Handle::tcpip_forward`] 让 SSH 服务端在 `remote_host:remote_port`
///    上监听（服务端通常要求 `remote_host` 为 `localhost` / `0.0.0.0`）；
/// 3. 服务端 accept 到的入站连接通过上述回调桥接回 `local_host:local_port`。
///
/// # 关于 `forwards` 参数
/// russh 0.45 的 `Handle` 不暴露内部 handler 的访问器，因此无法在 `connect`
/// 之后写入 handler 的字段。`forwards` 表由 [`crate::ssh::client::connect_direct_tunnel`]
/// 在构造 handler 时注入并回传，调用方持有同一 Arc 副本，故这里写入的映射，
/// handler 回调能读到同一份。
///
/// 本函数接管 `handle` 的所有权（包进 Arc 存入返回的 [`Tunnel`]，供 [`stop`]
/// 调用 `cancel_tcpip_forward`）。
pub async fn start_remote(
    handle: Handle<ClientHandler>,
    spec: TunnelSpec,
    forwards: crate::ssh::client::ForwardsMap,
) -> AppResult<Tunnel> {
    let remote_host = spec.remote_host.clone();
    let remote_port = spec.remote_port;

    // 1. 登记转发映射（回调据此查表桥接）。
    forwards.lock().insert(
        (remote_host.clone(), remote_port),
        (spec.local_host.clone(), spec.local_port),
    );

    // 2. 请求服务端在远端监听（tcpip_forward 需要 &mut，故在包 Arc 前调用）。
    let mut handle = handle;
    handle
        .tcpip_forward(remote_host.clone(), u32::from(remote_port))
        .await
        .map_err(|e| AppError::Ssh(format!("tcpip_forward 失败: {}", e)))?;
    log::info!(
        "远程转发已启动: 服务端 {}:{} -> 本地 {}:{}",
        remote_host,
        remote_port,
        spec.local_host,
        spec.local_port
    );

    let handle_arc = std::sync::Arc::new(handle);

    // 远程转发没有本地 accept 循环，stop_tx 仍创建（stop 会发送但无消费者，无害），
    // accept_task 为 None。
    let (stop_tx, _stop_rx) = oneshot::channel::<()>();
    Ok(Tunnel {
        spec,
        listener: None,
        stop_tx,
        accept_task: None,
        remote_forward: Some(RemoteForwardHandle {
            handle: handle_arc,
            remote_host,
            remote_port: u32::from(remote_port),
        }),
    })
}

// ===========================================================================
// 动态 SOCKS5 转发（-D）
// ===========================================================================

/// 启动一条动态 SOCKS5 转发（`-D`）。
///
/// 在 `local_host:local_port` 上监听一个 SOCKS5 代理。对每个入站连接：
/// 1. 完成 SOCKS5 握手（仅支持 NO_AUTH 模式），解析出目标 `host:port`
///    （支持 IPv4 / 域名 / IPv6 三种地址类型）；
/// 2. 调用 [`Handle::channel_open_direct_tcpip`] 建立到目标的 SSH channel；
/// 3. `copy_bidirectional` 双向桥接。
///
/// `spec.remote_host` / `spec.remote_port` 对动态转发无意义（目标由 SOCKS5
/// 协议在运行时决定），调用方通常填占位值。
///
/// 与 [`start_local`] 一致，本函数接管 `handle` 的所有权。
pub async fn start_dynamic(handle: Handle<ClientHandler>, spec: TunnelSpec) -> AppResult<Tunnel> {
    let bind_addr = format!("{}:{}", spec.local_host, spec.local_port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| AppError::Ssh(format!("绑定本地监听 {} 失败: {}", bind_addr, e)))?;
    log::info!("动态 SOCKS5 转发监听已启动: {}", bind_addr);

    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let handle_arc = std::sync::Arc::new(handle);

    let accept_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;

                _ = &mut stop_rx => {
                    log::info!("动态转发 {} 停止", bind_addr);
                    return;
                }

                accept = listener.accept() => {
                    let (mut tcp, peer) = match accept {
                        Ok(v) => v,
                        Err(e) => {
                            log::warn!("accept 失败 ({}): {}", bind_addr, e);
                            continue;
                        }
                    };

                    let handle = handle_arc.clone();
                    tokio::spawn(async move {
                        // 1. SOCKS5 握手，解析目标地址。
                        let target = match socks5_handshake(&mut tcp).await {
                            Ok(t) => t,
                            Err(e) => {
                                log::debug!("SOCKS5 握手失败 ({}): {}", peer, e);
                                return;
                            }
                        };

                        // 2. 建立到目标的 SSH channel。
                        let channel = match handle
                            .channel_open_direct_tcpip(
                                target.host.clone(),
                                u32::from(target.port),
                                peer.ip().to_string(),
                                u32::from(peer.port()),
                            )
                            .await
                        {
                            Ok(c) => c,
                            Err(e) => {
                                log::warn!("channel_open_direct_tcpip 失败 ({}): {}", target, e);
                                return;
                            }
                        };

                        // 3. 双向桥接。
                        let mut stream = channel.into_stream();
                        match copy_bidirectional(&mut tcp, &mut stream).await {
                            Ok((up, down)) => log::debug!(
                                "动态转发通道结束 [{}]: 上行 {} 字节, 下行 {} 字节",
                                target, up, down
                            ),
                            Err(e) => log::warn!("动态转发通道出错 [{}]: {}", target, e),
                        }
                    });
                }
            }
        }
    });

    Ok(Tunnel {
        spec,
        listener: None,
        stop_tx,
        accept_task: Some(accept_task),
        remote_forward: None,
    })
}

// ---- SOCKS5 协议实现 -------------------------------------------------------

/// SOCKS5 握手解析出的目标。
struct SocksTarget {
    host: String,
    port: u16,
}

impl std::fmt::Display for SocksTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// 完成 SOCKS5 握手并返回目标地址。
///
/// 仅支持无认证（NO_AUTH, 0x00）模式。握手分两步：
/// 1. 方法协商：客户端发 `[VER=5, NMETHODS, METHODS...]`，本实现固定回复
///    NO_AUTH（`[5, 0]`）；若客户端只提供需认证的方法则回复 `0xFF` 拒绝。
/// 2. 请求：客户端发 `[VER=5, CMD, RSV, ATYP, DST.ADDR, DST.PORT]`，仅处理
///    `CMD=CONNECT(1)`，按 ATYP 解析 IPv4(1)/域名(3)/IPv6(4) 地址。
async fn socks5_handshake<R>(stream: &mut R) -> std::io::Result<SocksTarget>
where
    R: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // ---- 1. 方法协商 ----
    let mut hdr = [0u8; 2];
    stream.read_exact(&mut hdr).await?;
    if hdr[0] != 0x05 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("非 SOCKS5 版本: {}", hdr[0]),
        ));
    }
    let nmethods = hdr[1] as usize;
    let mut methods = vec![0u8; nmethods];
    stream.read_exact(&mut methods).await?;
    // 仅接受 NO_AUTH(0x00)。
    if !methods.contains(&0x00) {
        stream.write_all(&[0x05, 0xFF]).await?; // 无可接受方法
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "SOCKS5 需要认证，本代理仅支持 NO_AUTH",
        ));
    }
    stream.write_all(&[0x05, 0x00]).await?; // 选用 NO_AUTH

    // ---- 2. 请求 ----
    let mut req = [0u8; 4]; // VER, CMD, RSV, ATYP
    stream.read_exact(&mut req).await?;
    if req[0] != 0x05 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "请求阶段版本号非 5",
        ));
    }
    if req[1] != 0x01 {
        // 仅支持 CONNECT；回复 COMMAND_NOT_SUPPORTED(0x07)。
        stream
            .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await?;
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("仅支持 CONNECT(1)，收到 {}", req[1]),
        ));
    }

    let host = match req[3] {
        0x01 => {
            // IPv4：4 字节地址。
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr).await?;
            std::net::Ipv4Addr::from(addr).to_string()
        }
        0x03 => {
            // 域名：1 字节长度 + 域名。
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut buf = vec![0u8; len[0] as usize];
            stream.read_exact(&mut buf).await?;
            String::from_utf8(buf)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
        }
        0x04 => {
            // IPv6：16 字节地址。
            let mut addr = [0u8; 16];
            stream.read_exact(&mut addr).await?;
            std::net::Ipv6Addr::from(addr).to_string()
        }
        other => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("不支持的 SOCKS5 ATYP: {}", other),
            ));
        }
    };

    let mut port_buf = [0u8; 2];
    stream.read_exact(&mut port_buf).await?;
    let port = u16::from_be_bytes(port_buf);

    // 回复成功（0x00），ATYP=IPv4 + 全零地址/端口（连接已由后续 channel 承载）。
    stream
        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;

    Ok(SocksTarget { host, port })
}

// ===========================================================================
// 停止
// ===========================================================================

/// 停止一条转发。
///
/// - 本地/动态转发：发送 stop 信号通知 accept 循环退出，并 abort 掉 accept
///   任务以强制释放端口绑定。
/// - 远程转发：调用 [`Handle::cancel_tcpip_forward`] 让服务端停止远端监听。
///
/// 若有直接持有的 `listener` 也一并 drop。
pub async fn stop(mut tunnel: Tunnel) -> AppResult<()> {
    // 通知 accept 循环退出（本地/动态转发；远程转发无消费者，无害）。
    let _ = tunnel.stop_tx.send(());

    // 强制 abort accept 循环，释放端口绑定（本地/动态转发）。
    if let Some(task) = tunnel.accept_task.take() {
        task.abort();
    }

    // 远程转发：取消远端监听。
    if let Some(rf) = tunnel.remote_forward.take() {
        if let Err(e) = rf
            .handle
            .cancel_tcpip_forward(rf.remote_host.clone(), rf.remote_port)
            .await
        {
            log::warn!("cancel_tcpip_forward 失败: {}", e);
        }
    }

    // 兼容将来直接持有 listener 的实现。
    if let Some(listener) = tunnel.listener.take() {
        drop(listener);
    }
    Ok(())
}
