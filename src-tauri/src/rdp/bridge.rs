//! 内嵌 RDP 的「迷你网关」：WebSocket（RDCleanPath 协议）↔ RDP 服务器 3389。
//!
//! IronRDP 官方 web 客户端不直连 RDP TCP，而是通过 WebSocket 连接一个
//! Gateway 兼容代理。本模块实现该代理的最小可用子集：
//!
//! 1. 收到客户端首条消息（DER 编码的 RDCleanPath 请求，内含目标地址与
//!    X.224 Connection Request 字节）；
//! 2. TCP 连接目标 3389，转发 PCB 与 X.224 请求，读取服务器 X.224 确认；
//! 3. 由桥接端终止 TLS（rustls，不校验服务器证书——与官方 Gateway 的
//!    dangerous_connect 行为一致），并收集服务器证书链；
//! 4. 回发 RDCleanPath 响应（X.224 确认 + 证书链），客户端据此跳过自身 TLS；
//! 5. 此后双向透传 RDP 字节流（WS ↔ TLS），不再做任何协议解析。
//!
//! 安全性：监听仅绑定 127.0.0.1 临时端口 + 随机 token（与 MCP/VNC 约束一致）。
//! 注意：桥接不校验 RDP 服务器证书，客户端也无法验证服务器身份（凭据经
//! CredSSP 在隧道内协商，仍受保护）；敏感环境建议使用系统 mstsc。

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use der::{Decode, Encode};
use futures::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_rustls::TlsConnector;

use crate::error::{AppError, AppResult};

use super::rdcp::RDCleanPathPdu;

/// 建连 / X.224 / TLS 各阶段超时（秒）。目标不可达时给前端确定失败而非无限等待。
const CONNECT_TIMEOUT_SECS: u64 = 10;
/// X.224 包长度上限（TPKT 长度字段为 u16，直接沿用其最大值）。
const X224_MAX_SIZE: usize = u16::MAX as usize;
/// 目标端口缺省值（RDP 标准端口）。
const DEFAULT_RDP_PORT: u16 = 3389;

/// 桥接共享状态（axum `State` 需 `Clone`，字段均为廉价克隆）。
#[derive(Clone)]
struct BridgeState {
    /// URL 路径中必须匹配的随机 token。
    token: String,
    /// 目标 RDP 主机（兜底：请求 PDU 未带 destination 时使用）。
    host: String,
    /// 目标 RDP 端口（兜底同上）。
    port: u16,
    /// 是否校验服务器证书（严格模式：系统信任根验证证书链）。
    verify_cert: bool,
}

/// 一个运行中的 RDP 桥接实例。
pub struct RdpBridge {
    /// 实例 id（前端 tab 标识，与 ws_url 一一对应）。
    pub id: String,
    /// 前端 IronRDP WASM 直连的地址：`ws://127.0.0.1:{port}/ws/{token}`。
    pub ws_url: String,
    /// 优雅停机信号（stop 时触发 axum graceful shutdown）。
    stop_tx: Option<oneshot::Sender<()>>,
    /// axum server 任务。
    server_task: Option<tokio::task::JoinHandle<()>>,
}

impl RdpBridge {
    /// 启动桥接：绑定回环临时端口、起 axum server，返回可直接交给前端的桥接实例。
    ///
    /// 与 MCP 服务端相同的启动模式：在异步上下文（Tauri 异步命令）里直接绑定
    /// tokio 监听器并 `tokio::spawn` axum serve。此前用 std 监听器 + 任务内
    /// `from_std` 转换，出现过「TCP 能连上但 HTTP 永不响应」的假死（Windows 上
    /// std 监听器经 from_std 注册 reactor 不可靠），故统一改为 tokio 原生监听器。
    pub async fn start(host: &str, port: u16, verify_cert: bool) -> AppResult<Self> {
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
            verify_cert,
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
            log::info!("[rdp:{task_id}] 桥接服务已停止");
        });
        log::info!("[rdp:{id}] 桥接服务已启动（监听 127.0.0.1:{local_port}）");

        Ok(RdpBridge {
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

impl Drop for RdpBridge {
    fn drop(&mut self) {
        // 兜底回收：关监听、断连接，防止 tab 异常关闭时桥接泄漏。
        self.stop();
    }
}

/// 生成前端直连的 ws url。
pub(crate) fn ws_url_for(port: u16, token: &str) -> String {
    format!("ws://127.0.0.1:{port}/ws/{token}")
}

/// WS 升级端点：校验 token 后把连接升级为 WebSocket 并开始 RDP 桥接。
async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(path_token): Path<String>,
    State(state): State<BridgeState>,
) -> Response {
    if path_token != state.token {
        log::warn!("[rdp] 收到未知 token 的连接请求");
        return StatusCode::NOT_FOUND.into_response();
    }
    ws.on_upgrade(move |socket| bridge_ws_rdp(socket, state))
}

/// 发送 RDCleanPath 错误 PDU（一般错误 + HTTP 状态码）。返回是否发送成功。
async fn send_rdcp_error<T>(ws_tx: &mut T, status: u16) -> bool
where
    T: futures::SinkExt<Message> + Unpin,
{
    let pdu = RDCleanPathPdu::new_http_error(status);
    match pdu.to_der() {
        Ok(der) => ws_tx.send(Message::Binary(der)).await.is_ok(),
        Err(_) => false,
    }
}

/// RDP 桥接主流程：RDCleanPath 握手 + 字节隧道。任一阶段失败发错误 PDU 后关闭。
async fn bridge_ws_rdp(socket: WebSocket, state: BridgeState) {
    log::info!("[rdp] 桥接连接（期望目标 {}:{}）", state.host, state.port);

    let (mut ws_tx, mut ws_rx) = socket.split();

    // 1. 读首条消息：RDCleanPath 请求 PDU。
    let first = match ws_rx.next().await {
        Some(Ok(msg)) => msg.into_data(),
        _ => return, // 客户端直接断开
    };
    let request = match RDCleanPathPdu::from_der(&first) {
        Ok(pdu) => pdu,
        Err(e) => {
            log::warn!("[rdp] RDCleanPath 请求解码失败: {e}");
            let _ = send_rdcp_error(&mut ws_tx, 400).await;
            let _ = ws_tx.send(Message::Close(None)).await;
            return;
        }
    };

    // 2. 解析目标地址：优先取请求里的 destination，回退到桥接配置的 host/port。
    let (host, port) = match request
        .destination
        .as_deref()
        .map(parse_destination)
        .transpose()
    {
        Ok(Some(target)) => target,
        Ok(None) => (state.host, state.port),
        Err(_) => {
            log::warn!("[rdp] RDCleanPath destination 非法");
            let _ = send_rdcp_error(&mut ws_tx, 400).await;
            let _ = ws_tx.send(Message::Close(None)).await;
            return;
        }
    };

    // 3. 取 X.224 请求字节（必填）与可选 PCB。
    let Some(x224_request) = request.x224_connection_pdu.map(|p| p.into_bytes()) else {
        log::warn!("[rdp] RDCleanPath 请求缺少 X.224 数据");
        let _ = send_rdcp_error(&mut ws_tx, 400).await;
        let _ = ws_tx.send(Message::Close(None)).await;
        return;
    };
    let pcb = request
        .preconnection_blob
        .map(|p| p.as_bytes().to_vec())
        .unwrap_or_default();

    // 4. TCP 建连（带超时）。
    let mut tcp = match tokio::time::timeout(
        std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS),
        TcpStream::connect((host.as_str(), port)),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            log::warn!("[rdp] 连接 {host}:{port} 失败: {e}");
            let _ = send_rdcp_error(&mut ws_tx, 502).await;
            let _ = ws_tx.send(Message::Close(None)).await;
            return;
        }
        Err(_) => {
            log::warn!("[rdp] 连接 {host}:{port} 超时（{CONNECT_TIMEOUT_SECS} 秒）");
            let _ = send_rdcp_error(&mut ws_tx, 504).await;
            let _ = ws_tx.send(Message::Close(None)).await;
            return;
        }
    };
    log::info!("[rdp] 已连接 {host}:{port}，转发 X.224 握手");

    // 5. 转发 PCB + X.224 请求，读取服务器 X.224 确认。
    let x224_response = async {
        if !pcb.is_empty() {
            tcp.write_all(&pcb).await?;
        }
        tcp.write_all(&x224_request).await?;
        read_x224(&mut tcp).await
    };
    let x224_response = match tokio::time::timeout(
        std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS),
        x224_response,
    )
    .await
    {
        Ok(Ok(pdu)) => pdu,
        Ok(Err(e)) => {
            log::warn!("[rdp] X.224 握手失败: {e}");
            let _ = send_rdcp_error(&mut ws_tx, 502).await;
            let _ = ws_tx.send(Message::Close(None)).await;
            return;
        }
        Err(_) => {
            log::warn!("[rdp] 等待 X.224 确认超时（{CONNECT_TIMEOUT_SECS} 秒）");
            let _ = send_rdcp_error(&mut ws_tx, 504).await;
            let _ = ws_tx.send(Message::Close(None)).await;
            return;
        }
    };

    // 6. 桥接端终止 TLS（严格模式校验证书链 / 默认不校验），收集服务器证书链。
    let server_name = match ServerName::try_from(host.clone()) {
        Ok(name) => name,
        Err(e) => {
            log::warn!("[rdp] 目标地址无法作为 TLS 主机名: {e}");
            let _ = send_rdcp_error(&mut ws_tx, 400).await;
            let _ = ws_tx.send(Message::Close(None)).await;
            return;
        }
    };
    let connector = TlsConnector::from(Arc::new(client_config(state.verify_cert)));
    let tls = match tokio::time::timeout(
        std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS),
        connector.connect(server_name, tcp),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            log::warn!("[rdp] TLS 握手失败: {e}");
            let _ = send_rdcp_error(&mut ws_tx, 502).await;
            let _ = ws_tx.send(Message::Close(None)).await;
            return;
        }
        Err(_) => {
            log::warn!("[rdp] TLS 握手超时（{CONNECT_TIMEOUT_SECS} 秒）");
            let _ = send_rdcp_error(&mut ws_tx, 504).await;
            let _ = ws_tx.send(Message::Close(None)).await;
            return;
        }
    };
    let cert_chain: Vec<Vec<u8>> = tls
        .get_ref()
        .1
        .peer_certificates()
        .map(|chain| chain.iter().map(|c| c.as_ref().to_vec()).collect())
        .unwrap_or_default();
    if cert_chain.is_empty() {
        log::warn!("[rdp] 服务器证书链为空，客户端无法继续握手");
        let _ = send_rdcp_error(&mut ws_tx, 502).await;
        let _ = ws_tx.send(Message::Close(None)).await;
        return;
    }

    // 7. 回发 RDCleanPath 响应（X.224 确认 + 证书链 + 服务器地址）。
    let response = match RDCleanPathPdu::new_response(host.clone(), x224_response, cert_chain) {
        Ok(pdu) => pdu,
        Err(e) => {
            log::warn!("[rdp] RDCleanPath 响应编码失败: {e}");
            let _ = ws_tx.send(Message::Close(None)).await;
            return;
        }
    };
    if ws_tx
        .send(Message::Binary(response.to_der().unwrap_or_default()))
        .await
        .is_err()
    {
        return;
    }
    log::info!("[rdp] 握手完成，开始字节隧道 {host}:{port}");

    // 8. 双向隧道：WS 字节 → TLS 写入；TLS 数据 → WS Binary 帧。
    let (mut tls_rx, mut tls_tx) = tokio::io::split(tls);

    // WS → TLS：把前端发来的 RDP 字节写入 TLS 连接。
    let ws_to_tls = async {
        while let Some(msg) = ws_rx.next().await {
            let Ok(msg) = msg else { break };
            match msg {
                Message::Close(_) => break,
                // Ping/Pong 由 tungstenite 自动应答，无需处理。
                Message::Ping(_) | Message::Pong(_) => {}
                // Text / Binary → 原始字节。
                other => {
                    if tls_tx.write_all(&other.into_data()).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tls_tx.shutdown().await;
    };

    // TLS → WS：把 RDP 服务器数据封装成 Binary 帧发给前端。
    let tls_to_ws = async {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match tls_rx.read(&mut buf).await {
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
        _ = ws_to_tls => {}
        _ = tls_to_ws => {}
    }
    log::info!("[rdp] 桥接连接 {host}:{port} 已关闭");
}

/// 解析 destination：`host`、`host:port`、`[ipv6]`、`[ipv6]:port`，端口缺省 3389。
fn parse_destination(dest: &str) -> AppResult<(String, u16)> {
    let dest = dest.trim();
    if dest.is_empty() {
        return Err(AppError::InvalidInput("RDP 目标地址为空".into()));
    }

    // "[ipv6]" / "[ipv6]:port" 形式。
    if let Some(rest) = dest.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| AppError::InvalidInput(format!("RDP 目标地址非法: {dest}")))?;
        if host.is_empty() {
            return Err(AppError::InvalidInput(format!("RDP 目标地址非法: {dest}")));
        }
        let port = match tail.strip_prefix(':') {
            Some(p) => parse_port(p, dest)?,
            None => DEFAULT_RDP_PORT,
        };
        return Ok((host.to_string(), port));
    }

    // 恰好一个冒号 → 按 host:port 解析；否则（无冒号 = 主机名，多个冒号 = 裸 IPv6）
    // 整体作为主机名并采用缺省端口。
    if dest.matches(':').count() == 1 {
        let (host, port) = dest.split_once(':').expect("冒号数量已确认恰好为一");
        if host.is_empty() {
            return Err(AppError::InvalidInput(format!("RDP 目标地址非法: {dest}")));
        }
        return Ok((host.to_string(), parse_port(port, dest)?));
    }

    Ok((dest.to_string(), DEFAULT_RDP_PORT))
}

/// 解析端口字符串：必须是 1..=65535 的数字。
fn parse_port(port: &str, dest: &str) -> AppResult<u16> {
    let port = port
        .parse::<u16>()
        .map_err(|_| AppError::InvalidInput(format!("RDP 端口非法: {dest}")))?;
    if port == 0 {
        return Err(AppError::InvalidInput(format!("RDP 端口非法: {dest}")));
    }
    Ok(port)
}

/// 校验 TPKT 头并返回包总长（含 4 字节头）。非法时返回 None。
fn tpkt_total_len(header: &[u8; 4]) -> Option<usize> {
    // TPKT: 版本 3，保留字节 0，后两字节为大端长度。
    if header[0] != 0x03 || header[1] != 0x00 {
        return None;
    }
    let len = u16::from_be_bytes([header[2], header[3]]) as usize;
    if len < 4 || len > X224_MAX_SIZE {
        return None;
    }
    Some(len)
}

/// 读取一个完整的 X.224（TPKT）包：4 字节头 + 载荷。
async fn read_x224(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    let len = tpkt_total_len(&header)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "X.224 TPKT 头非法"))?;
    let mut pdu = Vec::with_capacity(len);
    pdu.extend_from_slice(&header);
    pdu.resize(len, 0);
    stream.read_exact(&mut pdu[4..]).await?;
    Ok(pdu)
}

/// 不校验服务器证书的验证器（与官方 Gateway `dangerous_connect` 一致）。
///
/// 桥接把服务器证书链原样转交给 WASM 客户端（客户端用其公钥做 CredSSP），
/// 自身不做任何信任判断。
#[derive(Debug)]
struct NoCertVerifier;

impl ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
        ]
    }
}

/// 构建桥接端 TLS 客户端配置（rustls）。
///
/// 必须显式指定 ring provider：本项目依赖树中 aws-lc-rs（reqwest/sqlx 的 rustls
/// 特性）与 ring（本模块）同时启用，rustls 无法自动确定进程级 provider，
/// `ClientConfig::builder()` 会在运行时 panic（此前 RDP 连接在此处崩溃，
/// 连接被异常终止 → 客户端 WebSocket 1006）。显式指定即可绕开全局默认。
///
/// - `verify_cert = false`（默认）：不校验服务器证书，与官方 Gateway 的
///   `dangerous_connect` 一致；凭据仍经 CredSSP 在隧道内协商受保护。
/// - `verify_cert = true`：严格模式，用 Windows 系统信任根校验服务器证书链。
fn client_config(verify_cert: bool) -> ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    if verify_cert {
        // 严格模式：系统证书库作为信任根，标准链校验（含主机名）。
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("TLS 协议版本配置失败")
            .with_root_certificates(roots)
            .with_no_client_auth()
    } else {
        // 危险模式：不校验任何服务器证书（与官方 Gateway 一致）。
        ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("TLS 协议版本配置失败")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
            .with_no_client_auth()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ws_url_format() {
        let url = ws_url_for(55124, "abc123");
        assert_eq!(url, "ws://127.0.0.1:55124/ws/abc123");
    }

    #[test]
    fn parse_destination_basic() {
        assert_eq!(
            parse_destination("10.10.0.3").unwrap(),
            ("10.10.0.3".into(), 3389)
        );
        assert_eq!(
            parse_destination("10.10.0.3:3390").unwrap(),
            ("10.10.0.3".into(), 3390)
        );
        assert_eq!(
            parse_destination("win-host.example.com").unwrap(),
            ("win-host.example.com".into(), 3389)
        );
        assert_eq!(
            parse_destination("[2001:db8::1]:3389").unwrap(),
            ("2001:db8::1".into(), 3389)
        );
        assert_eq!(
            parse_destination("[2001:db8::1]").unwrap(),
            ("2001:db8::1".into(), 3389)
        );
        // 裸 IPv6 不带端口：整个作为主机名。
        assert_eq!(
            parse_destination("2001:db8::1").unwrap(),
            ("2001:db8::1".into(), 3389)
        );
    }

    #[test]
    fn parse_destination_invalid() {
        assert!(parse_destination("").is_err());
        assert!(parse_destination("[]").is_err());
        assert!(parse_destination("host:notaport").is_err());
        assert!(parse_destination("host:99999").is_err());
        assert!(parse_destination("host:0").is_err());
        assert!(parse_destination(":3389").is_err());
    }

    #[test]
    fn tpkt_header_len() {
        // 长度 19 的合法 TPKT 头。
        assert_eq!(tpkt_total_len(&[0x03, 0x00, 0x00, 19]), Some(19));
        // 版本错误。
        assert_eq!(tpkt_total_len(&[0x02, 0x00, 0x00, 19]), None);
        // 长度小于头。
        assert_eq!(tpkt_total_len(&[0x03, 0x00, 0x00, 2]), None);
    }

    /// 回归测试：TLS 客户端配置必须可构造（危险模式与严格模式）。
    ///
    /// 依赖树同时启用 rustls 的 ring（本模块）与 aws-lc-rs（reqwest/sqlx）特性时，
    /// `ClientConfig::builder()` 会因无法自动确定进程级 CryptoProvider 而在运行时
    /// panic（RDP 连接在此崩溃、客户端收到 WebSocket 1006）；必须显式指定 provider。
    #[test]
    fn client_config_builds() {
        let _ = client_config(false);
        let _ = client_config(true);
    }

    /// 回归测试：桥接 HTTP 层必须正常响应（曾出现 TCP 能连上但 HTTP 永不响应的
    /// 假死——std 监听器 + from_std 的启动方式在 Windows 上不可靠）。
    ///
    /// 同时回归路由参数语法缺陷：axum 0.7 下 `{token}` 会被当作字面量段，导致
    /// 所有实际路径（含正确 token）全部 404、浏览器 WebSocket 立即关闭。
    /// 普通 GET 会被 WebSocketUpgrade 提取器拒绝（400），绝不可能是 404。
    #[tokio::test]
    async fn bridge_http_layer_responds() {
        let mut bridge = RdpBridge::start("127.0.0.1", 1, false).await.unwrap();
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

    /// 回归测试：真实 WebSocket 升级握手必须成功（IronRDP WASM 直连的前提）。
    /// 曾因 `{token}` 字面量路由缺陷导致升级请求被 404，客户端立即关闭。
    /// 顺带覆盖 token 校验：错误 token 的真实升级请求必须被 404 拒绝。
    #[tokio::test]
    async fn bridge_ws_upgrade_succeeds() {
        let mut bridge = RdpBridge::start("127.0.0.1", 1, false).await.unwrap();
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
