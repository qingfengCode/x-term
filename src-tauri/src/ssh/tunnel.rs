//! SSH 端口转发（tunnel）封装。
//!
//! 支持三类转发：
//! - [`TunnelKind::Local`]（`-L`）：本地端口转发到远程目标，**已实现**。
//! - [`TunnelKind::Remote`]（`-R`）：远程端口转发到本地目标，MVP 占位
//!   （返回 [`AppError::InvalidInput`]）。
//! - [`TunnelKind::Dynamic`]（`-D`）：本地 SOCKS5 代理，MVP 占位
//!   （返回 [`AppError::InvalidInput`]）。
//!
//! 本地转发的实现：在 `local_host:local_port` 上监听 TCP，对每一条入站
//! 连接调用 `channel_open_direct_tcpip` 建立一条到 `remote_host:remote_port`
//! 的 SSH channel，再用 `tokio::io::copy_bidirectional` 双向桥接。

use russh::client::Handle;
use serde::{Deserialize, Serialize};
use tokio::io::copy_bidirectional;
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
/// - `listener`：本地监听器。注意 `start_local` 会把监听器所有权移入后台
///   accept 循环，故此字段在 `start_local` 返回后为 `None`；保留是为了兼容
///   远程/动态转发等将来直接持有监听器的实现。
/// - `stop_tx`：发送一次即通知 accept 循环优雅退出。
/// - `accept_task`：accept 循环的任务句柄，`stop` 时 abort 以强制释放端口。
pub struct Tunnel {
    pub spec: TunnelSpec,
    pub listener: Option<TcpListener>,
    pub stop_tx: oneshot::Sender<()>,
    pub accept_task: Option<tokio::task::JoinHandle<()>>,
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
pub async fn start_local(
    handle: Handle<ClientHandler>,
    spec: TunnelSpec,
) -> AppResult<Tunnel> {
    let bind_addr = format!("{}:{}", spec.local_host, spec.local_port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| AppError::Ssh(format!("绑定本地监听 {} 失败: {}", bind_addr, e)))?;
    log::info!("本地转发监听已启动: {} -> {}:{}", bind_addr, spec.remote_host, spec.remote_port);

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
    })
}

// ===========================================================================
// 远程 / 动态转发（MVP 占位）
// ===========================================================================

/// 启动一条远程端口转发（`-R`）。
///
/// 实现思路（TODO）：调用 `handle.tcpip_forward(remote_host, remote_port)`
/// 让 SSH 服务端在远程监听，并在 [`ClientHandler`] 的
/// `server_channel_open_forwarded_tcpip` 回调里把入站 channel 桥接回本地
/// `local_host:local_port`。
///
/// 当前 MVP 返回 [`AppError::InvalidInput`]。
pub async fn start_remote(
    _handle: &Handle<ClientHandler>,
    _spec: TunnelSpec,
) -> AppResult<Tunnel> {
    // TODO: handle.tcpip_forward(...) + Handler::server_channel_open_forwarded_tcpip
    Err(AppError::InvalidInput(
        "远程端口转发（-R）暂未实现".to_string(),
    ))
}

/// 启动一条动态 SOCKS5 转发（`-D`）。
///
/// 实现思路（TODO）：在 `local_host:local_port` 上监听，先完成 SOCKS5
/// 握手解析出目标地址，再 `channel_open_direct_tcpip`。
///
/// 当前 MVP 返回 [`AppError::InvalidInput`]。
pub async fn start_dynamic(
    _handle: &Handle<ClientHandler>,
    _spec: TunnelSpec,
) -> AppResult<Tunnel> {
    // TODO: SOCKS5 协议解析 + channel_open_direct_tcpip
    Err(AppError::InvalidInput(
        "动态 SOCKS5 转发（-D）暂未实现".to_string(),
    ))
}

// ===========================================================================
// 停止
// ===========================================================================

/// 停止一条转发。
///
/// 发送 stop 信号通知 accept 循环优雅退出，并 abort 掉 accept 任务（强制
/// 释放端口）。若有直接持有的 `listener` 也一并 drop。
pub async fn stop(mut tunnel: Tunnel) -> AppResult<()> {
    // 通知 accept 循环退出。
    let _ = tunnel.stop_tx.send(());

    // 强制 abort accept 循环，释放端口绑定。
    if let Some(task) = tunnel.accept_task.take() {
        task.abort();
    }

    // 兼容将来直接持有 listener 的实现。
    if let Some(listener) = tunnel.listener.take() {
        drop(listener);
    }
    Ok(())
}
