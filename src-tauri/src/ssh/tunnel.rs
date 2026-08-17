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

/// SOCKS5 握手的最长等待时间。客户端只连不发的连接会永久占用一个桥接任务
/// （连同其持有的 `Arc<Handle>`），超时直接断开。
const SOCKS5_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 该转发下存量桥接任务的注册表类型。
///
/// 桥接任务持有 `Arc<Handle>`，只要有一条空闲连接不关闭，整条 SSH 连接就
/// 永不释放。`stop` 时遍历 abort 所有句柄，切断这条泄漏链。
pub type BridgeTasks = std::sync::Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>;

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
/// - `bridge_tasks`：本地/动态转发下所有存量桥接任务句柄，`stop` 时逐个 abort。
///   远程转发没有本地 accept 循环，其桥接注册表在
///   [`crate::ssh::client::ForwardsMap`] 的每个目标条目里（由 handler 回调登记），
///   本字段为空。
/// - `remote_forward`：仅远程转发使用：stop 时据此调用
///   [`Handle::cancel_tcpip_forward`] 取消远端监听，并清理 ClientHandler 中的
///   转发映射与其存量桥接任务。本地/动态转发为 `None`。
/// - `handle`：SSH 连接句柄的共享引用，仅用于连接存活检测
///   （[`Handle::is_closed`]，监控任务据此发现隧道异常退出）。三类转发均填充。
pub struct Tunnel {
    pub spec: TunnelSpec,
    pub listener: Option<TcpListener>,
    pub stop_tx: oneshot::Sender<()>,
    pub accept_task: Option<tokio::task::JoinHandle<()>>,
    pub bridge_tasks: BridgeTasks,
    pub remote_forward: Option<RemoteForwardHandle>,
    pub handle: Option<std::sync::Arc<Handle<ClientHandler>>>,
}

/// 远程转发专用句柄（由 [`start_remote`] 填充）。
///
/// 持有 SSH 连接的 `Handle`（包在 Arc 中供 stop 异步调用）、远端监听的
/// `(host, port)` 与转发映射表，stop 时据此取消远端监听、移除映射并
/// abort 该映射下的存量桥接任务。
pub struct RemoteForwardHandle {
    pub handle: std::sync::Arc<Handle<ClientHandler>>,
    pub remote_host: String,
    pub remote_port: u32,
    /// 转发映射表（与 handler 回调共享）：stop 时按 key 移除条目。
    pub forwards: crate::ssh::client::ForwardsMap,
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
    // 存活监控用副本：handle_arc 本体会被移入下方 accept 循环闭包。
    let monitor_handle = handle_arc.clone();
    // 桥接任务注册表：每条入站连接的桥接任务句柄登记于此，stop 时统一 abort。
    let bridge_tasks: BridgeTasks = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let accept_bridge_tasks = bridge_tasks.clone();

    let accept_task = tokio::spawn(async move {
        let bridge_tasks = accept_bridge_tasks;
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
                    let bridge_tasks = bridge_tasks.clone();
                    let bridge = tokio::spawn(async move {
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
                    bridge_tasks.lock().push(bridge);
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
        bridge_tasks,
        remote_forward: None,
        handle: Some(monitor_handle),
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

    // 1. 登记转发映射（回调据此查表桥接）。目标条目自带桥接任务注册表，
    //    入站桥接任务由 handler 回调登记，stop 时统一 abort。
    forwards.lock().insert(
        (remote_host.clone(), remote_port),
        crate::ssh::client::ForwardTarget {
            host: spec.local_host.clone(),
            port: spec.local_port,
            bridge_tasks: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        },
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
    // accept_task / bridge_tasks 为空（桥接注册表在 forwards 映射的条目里）。
    let (stop_tx, _stop_rx) = oneshot::channel::<()>();
    Ok(Tunnel {
        spec,
        listener: None,
        stop_tx,
        accept_task: None,
        bridge_tasks: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        remote_forward: Some(RemoteForwardHandle {
            handle: handle_arc.clone(),
            remote_host,
            remote_port: u32::from(remote_port),
            forwards,
        }),
        handle: Some(handle_arc),
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
    // 存活监控用副本：handle_arc 本体会被移入下方 accept 循环闭包。
    let monitor_handle = handle_arc.clone();
    // 桥接任务注册表（与 start_local 一致）：stop 时统一 abort 释放 Arc<Handle>。
    let bridge_tasks: BridgeTasks = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let accept_bridge_tasks = bridge_tasks.clone();

    let accept_task = tokio::spawn(async move {
        let bridge_tasks = accept_bridge_tasks;
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
                    let bridge_tasks = bridge_tasks.clone();
                    let bridge = tokio::spawn(async move {
                        // 1. SOCKS5 握手，解析目标地址。握手必须限时：客户端只连
                        //    不发的连接会永久占用一个桥接任务 + Arc<Handle>。
                        let target = match tokio::time::timeout(
                            SOCKS5_HANDSHAKE_TIMEOUT,
                            socks5_handshake(&mut tcp),
                        )
                        .await
                        {
                            Ok(Ok(t)) => t,
                            Ok(Err(e)) => {
                                log::debug!("SOCKS5 握手失败 ({}): {}", peer, e);
                                return;
                            }
                            Err(_) => {
                                log::debug!(
                                    "SOCKS5 握手超时（{} 秒），关闭连接 ({})",
                                    SOCKS5_HANDSHAKE_TIMEOUT.as_secs(),
                                    peer
                                );
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
                    bridge_tasks.lock().push(bridge);
                }
            }
        }
    });

    Ok(Tunnel {
        spec,
        listener: None,
        stop_tx,
        accept_task: Some(accept_task),
        bridge_tasks,
        remote_forward: None,
        handle: Some(monitor_handle),
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
/// - 远程转发：调用 [`Handle::cancel_tcpip_forward`] 让服务端停止远端监听，
///   并从映射表移除条目（之后入站 channel 会被回调直接关闭），abort 该映射下
///   的存量桥接任务。
/// - 所有种类：abort 存量桥接任务——桥接任务持有 `Arc<Handle>`，不显式终止的
///   话一条空闲连接就足以让整条 SSH 连接永不释放。
///
/// 若有直接持有的 `listener` 也一并 drop。
pub async fn stop(mut tunnel: Tunnel) -> AppResult<()> {
    // 通知 accept 循环退出（本地/动态转发；远程转发无消费者，无害）。
    let _ = tunnel.stop_tx.send(());

    // 强制 abort accept 循环，释放端口绑定（本地/动态转发）。
    if let Some(task) = tunnel.accept_task.take() {
        task.abort();
    }

    // 本地/动态转发：abort 所有存量桥接任务，释放各自持有的 Arc<Handle>。
    for bridge in tunnel.bridge_tasks.lock().drain(..) {
        bridge.abort();
    }

    // 远程转发：取消远端监听 + 移除映射 + abort 存量桥接任务。
    if let Some(rf) = tunnel.remote_forward.take() {
        if let Err(e) = rf
            .handle
            .cancel_tcpip_forward(rf.remote_host.clone(), rf.remote_port)
            .await
        {
            log::warn!("cancel_tcpip_forward 失败: {}", e);
        }
        let key = (
            rf.remote_host.clone(),
            u16::try_from(rf.remote_port).unwrap_or(0),
        );
        if let Some(target) = rf.forwards.lock().remove(&key) {
            for bridge in target.bridge_tasks.lock().drain(..) {
                bridge.abort();
            }
        }
    }

    // 兼容将来直接持有 listener 的实现。
    if let Some(listener) = tunnel.listener.take() {
        drop(listener);
    }
    Ok(())
}
