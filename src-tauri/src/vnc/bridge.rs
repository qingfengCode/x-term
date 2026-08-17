//! VNC 桥接实现：axum WebSocket 端点 ↔ 目标 VNC TCP 连接的双向字节管道。
//!
//! 一个 [`VncBridge`] 对应一个终端页 VNC 标签页：`start` 时绑定 `127.0.0.1:0`
//! 临时端口并启动 axum server，前端拿 `ws_url` 直连；`stop`/Drop 时整体回收
//! （关监听 + abort 连接任务），防止泄漏。

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

use crate::error::{AppError, AppResult};

/// 连接 VNC 服务端的 TCP 建连超时（秒）。服务端不可达时给前端一个确定的失败，
/// 而不是让 noVNC 无限等待。
const TCP_CONNECT_TIMEOUT_SECS: u64 = 10;

/// 桥接共享状态（axum `State` 需 `Clone`，字段均为廉价克隆）。
#[derive(Clone)]
struct BridgeState {
    /// URL 路径中必须匹配的随机 token。
    token: String,
    /// 目标 VNC 主机。
    host: String,
    /// 目标 VNC 端口。
    port: u16,
}

/// 一个运行中的 VNC 桥接实例。
pub struct VncBridge {
    /// 实例 id（前端 tab 标识，与 ws_url 一一对应）。
    pub id: String,
    /// 前端 noVNC 直连的地址：`ws://127.0.0.1:{port}/ws/{token}`。
    pub ws_url: String,
    /// 优雅停机信号（stop 时触发 axum graceful shutdown）。
    stop_tx: Option<oneshot::Sender<()>>,
    /// axum server 任务。
    server_task: Option<tokio::task::JoinHandle<()>>,
}

impl VncBridge {
    /// 启动桥接：绑定回环临时端口、起 axum server，返回可直接交给前端的桥接实例。
    ///
    /// 与 MCP 服务端相同的启动模式：在异步上下文（Tauri 异步命令）里直接绑定
    /// tokio 监听器并 `tokio::spawn` axum serve。此前用 std 监听器 + 任务内
    /// `from_std` 转换，出现过「TCP 能连上但 HTTP 永不响应」的假死（Windows 上
    /// std 监听器经 from_std 注册 reactor 不可靠），故统一改为 tokio 原生监听器。
    pub async fn start(host: &str, port: u16) -> AppResult<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        let token = uuid::Uuid::new_v4().simple().to_string();

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| AppError::Io(e))?;
        let local_port = listener.local_addr().map_err(AppError::Io)?.port();

        let state = BridgeState {
            token: token.clone(),
            host: host.to_string(),
            port,
        };
        let router = Router::new()
            // 注意：axum 0.7 的路径参数语法是 :token（{token} 是 0.8 语法，
            // 会被当成字面量段导致所有实际路径 404）。
            .route("/ws/:token", get(ws_handler))
            .with_state(state);

        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let task_id = id.clone();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stop_rx.await;
                })
                .await;
            log::info!("[vnc:{task_id}] 桥接服务已停止");
        });
        log::info!("[vnc:{id}] 桥接服务已启动（监听 127.0.0.1:{local_port}）");

        Ok(VncBridge {
            id,
            ws_url: ws_url_for(local_port, &token),
            stop_tx: Some(stop_tx),
            server_task: Some(server_task),
        })
    }

    /// 停止桥接：触发优雅停机并立即 abort server 任务（断掉所有活跃连接）。
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
    }
}

impl Drop for VncBridge {
    fn drop(&mut self) {
        // 兜底回收：关监听、断连接，防止 tab 异常关闭时桥接泄漏。
        self.stop();
    }
}

/// 生成前端直连的 ws url。
pub(crate) fn ws_url_for(port: u16, token: &str) -> String {
    format!("ws://127.0.0.1:{port}/ws/{token}")
}

/// WS 升级端点：校验 token 后把连接升级为 WebSocket 并开始双向桥接。
async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(path_token): Path<String>,
    State(state): State<BridgeState>,
) -> Response {
    if path_token != state.token {
        log::warn!("[vnc] 收到未知 token 的连接请求");
        return StatusCode::NOT_FOUND.into_response();
    }
    ws.on_upgrade(move |socket| bridge_ws_tcp(socket, state.host, state.port))
}

/// 双向桥接：WS 字节 → TCP 写入；TCP 数据 → WS Binary 帧。任一端结束即整体回收。
async fn bridge_ws_tcp(socket: WebSocket, host: String, port: u16) {
    log::info!("[vnc] 桥接连接 {}:{}", host, port);

    // 连接目标 VNC 服务端（带超时，失败回 WS Close）。
    let tcp = match tokio::time::timeout(
        std::time::Duration::from_secs(TCP_CONNECT_TIMEOUT_SECS),
        TcpStream::connect((host.as_str(), port)),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            log::warn!("[vnc] 连接 {}:{} 失败: {}", host, port, e);
            let mut socket = socket;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
        Err(_) => {
            log::warn!(
                "[vnc] 连接 {}:{} 超时（{} 秒）",
                host,
                port,
                TCP_CONNECT_TIMEOUT_SECS
            );
            let mut socket = socket;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (mut tcp_rx, mut tcp_tx) = tcp.into_split();

    // WS → TCP：把前端发来的字节写入 VNC 服务端。
    let ws_to_tcp = async {
        while let Some(msg) = ws_rx.next().await {
            let Ok(msg) = msg else { break };
            match msg {
                Message::Close(_) => break,
                // Ping/Pong 由 tungstenite 自动应答，无需处理。
                Message::Ping(_) | Message::Pong(_) => {}
                // Text / Binary → 原始字节。
                other => {
                    if tcp_tx.write_all(&other.into_data()).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tcp_tx.shutdown().await;
    };

    // TCP → WS：把 VNC 服务端的数据封装成 Binary 帧发给前端。
    let tcp_to_ws = async {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match tcp_rx.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_tx
                        .send(Message::Binary(buf[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = ws_tx.send(Message::Close(None)).await;
    };

    // 任一端结束即整体结束：select 丢弃输者 future，其持有的半连接随之 drop，
    // 两侧 socket 全部关闭（无泄漏）。
    tokio::select! {
        _ = ws_to_tcp => {}
        _ = tcp_to_ws => {}
    }
    log::info!("[vnc] 桥接连接 {}:{} 已关闭", host, port);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ws_url_format() {
        let url = ws_url_for(55123, "abc123");
        assert_eq!(url, "ws://127.0.0.1:55123/ws/abc123");
    }

    /// 回归测试：桥接 HTTP 层必须正常响应（曾出现 TCP 能连上但 HTTP 永不响应的
    /// 假死——std 监听器 + from_std 的启动方式在 Windows 上不可靠）。
    ///
    /// 同时回归路由参数语法缺陷：axum 0.7 下 `{token}` 会被当作字面量段，导致
    /// 所有实际路径（含正确 token）全部 404、浏览器 WebSocket 立即关闭。
    /// 普通 GET 会被 WebSocketUpgrade 提取器拒绝（400），绝不可能是 404。
    #[tokio::test]
    async fn bridge_http_layer_responds() {
        let mut bridge = VncBridge::start("127.0.0.1", 1).await.unwrap();
        // ws_url = ws://127.0.0.1:{port}/ws/{token} → http 前缀即完整路径。
        let origin = &bridge.ws_url["ws://".len()..];

        let ok_url = format!("http://{origin}");
        let resp = tokio::time::timeout(Duration::from_secs(5), async {
            reqwest::Client::new().get(&ok_url).send().await
        })
        .await
        .expect("桥接 HTTP 层 5 秒内未响应（假死）")
        .expect("请求发送失败");
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "正确 token 路径不应 404"
        );
        bridge.stop();
    }

    /// 回归测试：真实 WebSocket 升级握手必须成功（浏览器 noVNC 直连的前提）。
    /// 曾因 `{token}` 字面量路由缺陷导致升级请求被 404，客户端立即关闭。
    /// 顺带覆盖 token 校验：错误 token 的真实升级请求必须被 404 拒绝。
    #[tokio::test]
    async fn bridge_ws_upgrade_succeeds() {
        let mut bridge = VncBridge::start("127.0.0.1", 1).await.unwrap();
        let (mut ws, resp) = tokio_tungstenite::connect_async(bridge.ws_url.clone())
            .await
            .expect("WebSocket 升级必须成功（回归：{token} 字面量路由 404 缺陷）");
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
        // 目标 127.0.0.1:1 不可达，桥接会回 Close；客户端主动关闭完成回收。
        let _ = ws.close(None).await;

        // 错误 token：handler 内校验拒绝 → 404（tungstenite 以 Http 错误报告）。
        let (base, _path) = bridge.ws_url["ws://".len()..]
            .split_once('/')
            .expect("ws url 含路径");
        let wrong_url = format!("ws://{base}/ws/wrong-token");
        let err = tokio_tungstenite::connect_async(&wrong_url)
            .await
            .expect_err("错误 token 必须被拒绝");
        assert!(
            err.to_string().contains("404"),
            "错误 token 应 404，实际: {err}"
        );
        bridge.stop();
    }
}
