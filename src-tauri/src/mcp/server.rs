//! MCP 服务端：基于 axum 0.7 的 HTTP + SSE 传输。
//!
//! 协议子集（JSON-RPC 2.0 over HTTP+SSE）：
//! - `GET /sse`：建立 SSE 长连接。服务端先发
//!   `event: endpoint\ndata: /messages?sessionId=<id>`，随后保持连接并把后续 POST
//!   的 JSON-RPC 响应通过该 SSE 流推送回客户端。
//! - `POST /messages?sessionId=<id>`：接收 JSON-RPC 请求（`initialize` /
//!   `tools/list` / `tools/call`），解析后异步执行，结果通过对应 sessionId 的 SSE
//!   连接推送。POST 本身返回 HTTP 202（无 body）。
//!
//! # 安全
//! - Bearer token 校验（`Authorization: Bearer <token>` 或 `?token=<token>`）。
//! - 默认仅绑定 `127.0.0.1`。
//! - exec_* 工具调用必须经人工确认（见 [`crate::mcp::approval`]）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Router,
};
use futures::stream;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::AppHandle;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{AppError, AppResult};
use crate::mcp::approval::{ApprovalRequest, McpKind, SharedApprovalRegistry};
use crate::mcp::exec;
use crate::state::AppState;

/// MCP 协议版本（与 Claude Desktop 等客户端协商用）。
const PROTOCOL_VERSION: &str = "2024-11-05";

// ===========================================================================
// 全局服务端句柄（按 kind 管理：SSH MCP / DB MCP 各一个实例）
// ===========================================================================

/// 服务端运行状态（供命令查询/前端展示）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    pub running: bool,
    pub host: String,
    pub port: u16,
    /// 完整 SSE 入口 URL（如 `http://0.0.0.0:8765/sse`）。
    pub endpoint: String,
}

/// 全局运行中服务端的句柄（JoinHandle + shutdown 信号 + 状态）。
struct ServerHandle {
    join: JoinHandle<()>,
    /// abort 兜底（与 join 解耦：join 被 timeout 消费后仍可 abort）。
    abort: tokio::task::AbortHandle,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    /// 通知所有 SSE 长连接结束（停止服务时先断开，避免 graceful shutdown 被
    /// 常驻 SSE 连接阻塞、端口无法释放）。
    sse_shutdown: Arc<tokio::sync::Notify>,
    status: McpServerStatus,
}

/// 两个 kind 各自最多一个运行实例：Ssh / Db。
static SERVERS: Lazy<Mutex<HashMap<McpKind, ServerHandle>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 运行时自动放行开关（per-kind）。`mcp_save_config` 更新此值，改后立即生效无需重启。
static AUTO_APPROVE: Lazy<Mutex<HashMap<McpKind, bool>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// 设置指定 kind 的自动放行开关（由 commands::mcp::mcp_save_config 调用）。
pub fn set_auto_approve(kind: McpKind, enabled: bool) {
    AUTO_APPROVE.lock().insert(kind, enabled);
}

/// 查询指定 kind 是否开启了自动放行。
pub fn is_auto_approved(kind: McpKind) -> bool {
    AUTO_APPROVE.lock().get(&kind).copied().unwrap_or(false)
}

/// 查询指定 kind 的服务端状态（未启动时返回 running=false）。
pub fn mcp_server_status(kind: McpKind) -> McpServerStatus {
    let guard = SERVERS.lock();
    match guard.get(&kind) {
        Some(h) => h.status.clone(),
        None => McpServerStatus {
            running: false,
            host: String::new(),
            port: 0,
            endpoint: String::new(),
        },
    }
}

/// 启动 MCP 服务端。
///
/// 绑定 `host:port`，用 `token` 做 Bearer 校验。成功后后台 spawn 一个 axum 任务运行，
/// 句柄存入全局 [`SERVERS`]。
///
/// `bound_resource_id` 是该 MCP 绑定的资源 id（SSH 会话 id 或 DB profile id），执行
/// 工具时按此 id 解析目标；`resource_mode == "client"`（客户端直连）时传 `None`，
/// 目标与凭据由调用方在工具参数中传入。若该 kind 已有实例运行则返回错误。
pub async fn start_mcp_server(
    kind: McpKind,
    app: AppHandle,
    state: AppState,
    host: String,
    port: u16,
    token: String,
    bound_resource_id: Option<String>,
    bound_database: Option<String>,
    resource_mode: String,
    auto_approve: bool,
    enable_log: bool,
) -> AppResult<()> {
    {
        let guard = SERVERS.lock();
        if guard.contains_key(&kind) {
            return Err(AppError::InvalidInput(format!(
                "{} 已在运行，请先停止",
                kind.label()
            )));
        }
    }

    // 启动时设置自动放行开关。
    set_auto_approve(kind, auto_approve);

    // 资源模式规范化：仅 "client" 视为直连模式，其余一律按 bound 处理。
    let resource_mode = if resource_mode == "client" {
        "client"
    } else {
        "bound"
    };
    let bound_resource_id = bound_resource_id.unwrap_or_default();

    // 绑定监听端口。
    let addr: SocketAddr = format!("{}:{}", host, port).parse().map_err(|e| {
        AppError::InvalidInput(format!("非法 host:port ({}:{}): {}", host, port, e))
    })?;
    let listener = TcpListener::bind(addr).await.map_err(|e| {
        AppError::Io(std::io::Error::new(
            e.kind(),
            format!("绑定 {}:{} 失败: {}", host, port, e),
        ))
    })?;
    let bound_addr = listener.local_addr().map_err(|e| {
        AppError::Io(std::io::Error::new(
            e.kind(),
            format!("获取本地地址失败: {}", e),
        ))
    })?;
    let bound_port = bound_addr.port();

    // 创建执行日志文件（若开启）。
    let log_path: Option<PathBuf> = if enable_log {
        let log_dir = state.data_dir.join("mcp-logs");
        let _ = std::fs::create_dir_all(&log_dir);
        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let kind_str = match kind {
            McpKind::Ssh => "ssh",
            McpKind::Db => "db",
            McpKind::File => "file",
        };
        let path = log_dir.join(format!("mcp-{}-{}.log", kind_str, ts));
        let bound_desc = if resource_mode == "client" {
            "(客户端直连，未绑定)".to_string()
        } else {
            bound_resource_id.clone()
        };
        let header = format!(
            "=== X-Term {} 执行日志 ===\n启动时间: {}\n监听: {}:{}\n资源模式: {}\n绑定资源: {}\n绑定数据库: {}\n自动放行: {}\n---\n",
            kind.label(),
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            host,
            port,
            resource_mode,
            bound_desc,
            bound_database.as_deref().unwrap_or("(默认)"),
            if auto_approve { "是" } else { "否" },
        );
        let _ = std::fs::write(&path, header);
        log::info!("[mcp] {} 日志文件: {}", kind.label(), path.display());
        Some(path)
    } else {
        None
    };

    // 共享给所有路由：app 句柄、AppState 克隆、kind、token、绑定资源 id、SSE 客户端表。
    let sse_shutdown: Arc<tokio::sync::Notify> = Arc::new(tokio::sync::Notify::new());
    let shared = Arc::new(SharedState {
        app: app.clone(),
        state: state.clone(),
        kind,
        resource_mode: resource_mode.to_string(),
        bound_resource_id,
        bound_database,
        token,
        clients: Arc::new(Mutex::new(HashMap::new())),
        streamable_sessions: Arc::new(Mutex::new(HashMap::new())),
        sse_shutdown: sse_shutdown.clone(),
        log_path,
    });

    // CORS：允许所有来源/方法/头（MCP 客户端需要）。
    let cors = tower_http::cors::CorsLayer::very_permissive();

    let app_router = Router::new()
        .route("/mcp", post(mcp_handler))
        .route("/sse", get(sse_handler))
        .route("/messages", post(messages_handler))
        .layer(cors)
        .with_state(shared);

    // shutdown 信号。
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let serve = axum::serve(listener, app_router).with_graceful_shutdown(async move {
        let _ = shutdown_rx.await;
    });

    let status = McpServerStatus {
        running: true,
        host: host.clone(),
        port: bound_port,
        endpoint: format!("http://{}:{}/mcp", host, bound_port),
    };
    log::info!("[mcp] {} 已启动: {}", kind.label(), status.endpoint);

    // 并发启动竞态防护：注册前在锁内再检查一次。两个并发 start（如不同端口）
    // 可能都通过函数开头的 contains 检查；这里若发现已有实例，drop serve 关闭
    // 已绑定的 listener 后报错，避免产生无法停止的孤儿服务。
    {
        let mut guard = SERVERS.lock();
        if guard.contains_key(&kind) {
            drop(serve);
            log::warn!("[mcp] {} 已在运行（并发启动），已释放本次绑定", kind.label());
            return Err(AppError::InvalidInput(format!(
                "{} 已在运行，请先停止",
                kind.label()
            )));
        }
        let join = tokio::spawn(async move {
            if let Err(e) = serve.await {
                log::error!("[mcp] {} axum 服务退出出错: {}", kind.label(), e);
            }
        });
        let abort = join.abort_handle();
        guard.insert(
            kind,
            ServerHandle {
                join,
                abort,
                shutdown_tx,
                sse_shutdown,
                status,
            },
        );
    }

    Ok(())
}

/// 停止指定 kind 的 MCP 服务端。
///
/// 先通知所有 SSE 长连接结束（否则 graceful shutdown 会被常驻连接无限阻塞、
/// 端口无法释放），再发 shutdown 信号；若 3 秒内任务未退出则 abort 兜底。
pub fn stop_mcp_server(kind: McpKind) -> AppResult<()> {
    let handle = {
        let mut guard = SERVERS.lock();
        guard.remove(&kind)
    };
    match handle {
        Some(h) => {
            // 1. 主动结束所有 SSE 长连接（它们不结束，axum graceful shutdown
            //    会一直等待，端口永远释放不了）。
            h.sse_shutdown.notify_waiters();
            // 2. 发送 graceful shutdown 信号，让 axum 优雅退出。
            let _ = h.shutdown_tx.send(());
            // 3. 兜底任务：3 秒后仍未退出则 abort（abort 与 join 解耦，join 被
            //    timeout 消费后仍可 abort）。
            let join = h.join;
            let abort = h.abort;
            tokio::spawn(async move {
                match tokio::time::timeout(std::time::Duration::from_secs(3), join).await {
                    Ok(_) => {}
                    Err(_) => {
                        abort.abort();
                        log::warn!("[mcp] 服务停止超时（3s），已强制 abort");
                    }
                }
            });
            log::info!("[mcp] {} 已停止", kind.label());
            Ok(())
        }
        None => Err(AppError::InvalidInput(format!("{} 未运行", kind.label()))),
    }
}

// ===========================================================================
// 共享状态
// ===========================================================================

/// 每个 SSE 连持有一个发送端：用于把 JSON-RPC 响应推回该连接。
type SseSender = mpsc::UnboundedSender<String>;

/// 路由共享状态。
struct SharedState {
    app: AppHandle,
    state: AppState,
    /// 该实例是 SSH MCP 还是 DB MCP（决定对外暴露哪个工具）。
    kind: McpKind,
    /// 资源模式："bound"（绑定本地资源）| "client"（客户端直连，目标/凭据来自参数）。
    /// 决定工具定义、目标解析与确认请求的脱敏行为。
    resource_mode: String,
    /// 绑定的资源 id：SSH 会话 id（kind=Ssh）或 DB profile id（kind=Db）。
    /// 工具执行时按此 id 解析目标，外部客户端无需传连接名。client 模式下为空串。
    bound_resource_id: String,
    /// 绑定的具体数据库名（仅 kind=Db 有效）。设置后 exec_sql 只针对该库。
    bound_database: Option<String>,
    token: String,
    /// sessionId -> 该 SSE 连接的 mpsc 发送端（旧 HTTP+SSE 传输用）。
    clients: Arc<Mutex<HashMap<String, SseSender>>>,
    /// Streamable HTTP 会话表：session_id -> ()（仅记录存在性，spec 2025-03-26）。
    streamable_sessions: Arc<Mutex<HashMap<String, ()>>>,
    /// 服务停止信号：`stop_mcp_server` 通知后所有 SSE 流结束（关闭长连接）。
    sse_shutdown: Arc<tokio::sync::Notify>,
    /// 执行日志文件路径（None = 未开启日志）。
    log_path: Option<PathBuf>,
}

// ===========================================================================
// 路由：GET /sse
// ===========================================================================

/// SSE 连接建立。
///
/// 生成 sessionId，登记一个 mpsc 发送端到 clients map（供 POST 推送响应），
/// 先推一条 `event: endpoint` 事件告知客户端消息端点，随后把 mpsc 接收端转成
/// `Stream<Event>` 作为 SSE 响应体，保持长连接直到客户端断开、收到 None 或
/// 服务停止（[`SharedState::sse_shutdown`] 被触发）。
///
/// 客户端断开时（stream 被 drop），通过 [`SseCleanupGuard`] 自动从 clients map
/// 移除对应条目，防止内存泄漏。
async fn sse_handler(
    State(shared): State<Arc<SharedState>>,
    Query(q): Query<MessagesQuery>,
    headers: HeaderMap,
) -> Response {
    // token 校验（与 /messages 一致：Authorization 头或 ?token= 二选一）。
    // 不校验会导致未认证连接无限填充 clients 表。
    if !check_token(&shared, q.token.as_deref(), &headers) {
        log::warn!("[mcp] GET /sse token 校验失败");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::unbounded_channel::<String>();

    shared.clients.lock().insert(session_id.clone(), tx);

    log::info!("[mcp] SSE 客户端连接: {}", session_id);

    // 先推 endpoint 事件（告知客户端消息端点）。
    let endpoint_url = format!("/messages?sessionId={}", session_id);
    let initial = Some(Event::default().event("endpoint").data(endpoint_url));

    // cleanup guard：stream 被 drop 时（客户端断开）自动清理 clients map。
    let cleanup = SseCleanupGuard {
        clients: shared.clients.clone(),
        session_id: session_id.clone(),
    };

    // 把 mpsc 接收端转成 Stream<Event>：先发 initial，再持续从 rx 取消息；
    // 同时监听服务停止信号，触发时结束流（否则 stop_mcp_server 的 graceful
    // shutdown 会被本长连接无限阻塞）。
    let shutdown = shared.sse_shutdown.clone();
    // FnMut 闭包 + async move：session_id 会被多次消费，闭包内每次调用先 clone
    // 一份（async move 会把引用到的变量按值捕获进协程，外层直接捕获会 move 错）。
    let sid_for_log = session_id.clone();
    let stream = stream::unfold(
        (initial, rx, cleanup, shutdown),
        move |(mut first, mut rx, cleanup, shutdown)| {
            let sid = sid_for_log.clone();
            async move {
                if let Some(ev) = first.take() {
                    return Some((
                        Ok::<_, std::convert::Infallible>(ev),
                        (first, rx, cleanup, shutdown),
                    ));
                }
                tokio::select! {
                    m = rx.recv() => match m {
                        Some(json_str) => {
                            // 每条消息作为一个 default-event 的 data 行。
                            let ev = Event::default().data(json_str);
                            Some((Ok::<_, std::convert::Infallible>(ev), (first, rx, cleanup, shutdown)))
                        }
                        None => None, // 发送端全部 drop → 结束流（cleanup 随 state 被 drop）。
                    },
                    _ = shutdown.notified() => {
                        // 服务停止：结束 SSE 流，让 graceful shutdown 能完成。
                        log::info!("[mcp] 服务停止，SSE 会话 {} 结束", sid);
                        None
                    }
                }
            }
        },
    );

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response()
}

/// SSE 连接断开时的清理守卫。
///
/// 当持有此结构体的 stream 被 drop（客户端断开连接或流结束），
/// 自动从 `clients` map 中移除对应的 session 条目。
struct SseCleanupGuard {
    clients: Arc<Mutex<HashMap<String, SseSender>>>,
    session_id: String,
}

impl Drop for SseCleanupGuard {
    fn drop(&mut self) {
        self.clients.lock().remove(&self.session_id);
        log::info!("[mcp] SSE 客户端断开，已清理会话: {}", self.session_id);
    }
}

// ===========================================================================
// 路由：POST /messages?sessionId=<id>
// ===========================================================================

/// POST 消息查询参数。
#[derive(Debug, Deserialize)]
struct MessagesQuery {
    /// 可选 token（与 Authorization 头二选一）。
    token: Option<String>,
    #[allow(dead_code)]
    session_id: Option<String>,
}

/// POST 消息处理。
///
/// 1. 校验 token。
/// 2. 取 sessionId 对应的 SSE 发送端。
/// 3. 解析 JSON-RPC 请求。
/// 4. 异步执行（exec_* 会阻塞等待人工确认），把响应 JSON 推到 SSE 流。
/// 5. 立即返回 202。
async fn messages_handler(
    State(shared): State<Arc<SharedState>>,
    Query(q): Query<MessagesQuery>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // 1. token 校验。
    if !check_token(&shared, q.token.as_deref(), &headers) {
        log::warn!("[mcp] POST /messages token 校验失败");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // 2. 解析 body 为 JSON-RPC 请求。
    let req: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("JSON-RPC 请求解析失败: {}", e),
            )
                .into_response();
        }
    };

    // 3. 取 sessionId。
    let session_id = match &q.session_id {
        Some(s) => s.clone(),
        None => {
            // 有些客户端把 sessionId 放在 body 的 meta 里；这里简单要求 query 携带。
            return (
                StatusCode::BAD_REQUEST,
                "缺少 sessionId 查询参数".to_string(),
            )
                .into_response();
        }
    };

    // 4. 取 SSE 发送端。
    let tx = match shared.clients.lock().get(&session_id).cloned() {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                format!("未找到 SSE 会话 {}", session_id),
            )
                .into_response();
        }
    };

    // 5. 异步执行并推送响应（不阻塞 POST 响应）。
    let shared_clone = shared.clone();
    let req_clone = req.clone();
    tokio::spawn(async move {
        let response = dispatch(&shared_clone, &req_clone).await;
        let json_str = match serde_json::to_string(&response) {
            Ok(s) => s,
            Err(e) => {
                log::error!("[mcp] 序列化 JSON-RPC 响应失败: {}", e);
                return;
            }
        };
        if tx.send(json_str).is_err() {
            log::warn!("[mcp] SSE 会话 {} 已断开，响应未送达", session_id);
        }
    });

    StatusCode::ACCEPTED.into_response()
}

/// 校验 token：Authorization 头或 ?token= 二选一匹配。
fn check_token(shared: &SharedState, query_token: Option<&str>, headers: &HeaderMap) -> bool {
    if let Some(t) = query_token {
        return t == shared.token;
    }
    if let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(s) = auth.to_str() {
            if let Some(rest) = s.strip_prefix("Bearer ") {
                return rest == shared.token;
            }
            // 兼容直接传裸 token。
            return s == shared.token;
        }
    }
    false
}

// ===========================================================================
// 路由：POST /mcp（Streamable HTTP，spec 2025-03-26）
// ===========================================================================

/// MCP Streamable HTTP 会话头名（spec 定义）。
const MCP_SESSION_ID: &str = "mcp-session-id";

/// Streamable HTTP 传输处理器（当前主流 MCP 客户端：Cursor、Claude Desktop 新版、ZCode）。
///
/// 协议要点（spec 2025-03-26）：
/// - 客户端 POST JSON-RPC 到单一 `/mcp` 端点。
/// - `initialize` 请求：服务端生成 session，在响应头 `mcp-session-id` 返回。
/// - 后续请求：客户端应携带 `mcp-session-id` 头（宽松处理：缺失也放行）。
/// - 通知（无 id 字段，如 `notifications/initialized`）：返回 202，无 body。
/// - 普通请求：返回 `application/json`，body 为 JSON-RPC 响应。
async fn mcp_handler(
    State(shared): State<Arc<SharedState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // 1. token 校验（仅 Bearer 头；Streamable HTTP 不用 ?token= 查询参数）。
    if !check_token(&shared, None, &headers) {
        log::warn!("[mcp] POST /mcp token 校验失败");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // 2. 解析 JSON-RPC 请求。
    let req: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("JSON-RPC 请求解析失败: {}", e),
            )
                .into_response();
        }
    };

    // 3. initialize：生成 session，响应头带 mcp-session-id。
    if req.method == "initialize" {
        let session_id = uuid::Uuid::new_v4().to_string();
        {
            let mut sessions = shared.streamable_sessions.lock();
            // 防止长期运行后 map 无限增长：超过上限时清空重建。
            // 该 map 仅用于诊断日志（宽松放行），清空不影响功能。
            if sessions.len() >= 1024 {
                sessions.clear();
            }
            sessions.insert(session_id.clone(), ());
        }
        log::info!("[mcp] Streamable HTTP 客户端初始化: session={}", session_id);

        let response = dispatch(&shared, &req).await;
        let json_body = serde_json::to_string(&response).unwrap_or_else(|_| "{}".into());
        let mut resp = (StatusCode::OK, json_body).into_response();
        resp.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        resp.headers_mut().insert(
            axum::http::HeaderName::from_static(MCP_SESSION_ID),
            session_id.parse().unwrap(),
        );
        return resp;
    }

    // 4. 通知（无 id 字段，如 notifications/initialized）：处理后返回 202。
    if req.id.is_null() {
        log::debug!("[mcp] 收到通知: {}", req.method);
        return StatusCode::ACCEPTED.into_response();
    }

    // 5. 普通请求：宽松校验 mcp-session-id（缺失/未知也放行，兼容不规范客户端）。
    if let Some(sid) = headers.get(MCP_SESSION_ID) {
        if let Ok(s) = sid.to_str() {
            if !shared.streamable_sessions.lock().contains_key(s) {
                log::warn!("[mcp] 未知 mcp-session-id: {}（宽松放行）", s);
            }
        }
    }

    // 6. 执行并返回 application/json。
    let response = dispatch(&shared, &req).await;
    let json_body = serde_json::to_string(&response).unwrap_or_else(|_| "{}".into());
    let mut resp = (StatusCode::OK, json_body).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    resp
}

// ===========================================================================
// JSON-RPC 分派
// ===========================================================================

/// 分派 JSON-RPC 请求到对应方法，构造响应。
///
/// - `initialize`：返回协议版本、capabilities、serverInfo。
/// - `tools/list`：返回 4 个工具定义。
/// - `tools/call`：执行工具（list_* 直接；exec_* 经人工确认）。
/// - 其它方法：返回 method not found 错误。
async fn dispatch(shared: &Arc<SharedState>, req: &JsonRpcRequest) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => {
            let result = json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "x-term",
                    "version": "0.1.0"
                }
            });
            JsonRpcResponse::success(req.id.clone(), result)
        }
        "tools/list" => {
            let tools = tool_defs(
                shared.kind,
                &shared.resource_mode,
                shared.bound_database.as_deref(),
            );
            JsonRpcResponse::success(req.id.clone(), json!({ "tools": tools }))
        }
        "tools/call" => {
            let (name, arguments) = match parse_call_params(&req.params) {
                Ok(v) => v,
                Err(e) => {
                    return JsonRpcResponse::error(
                        req.id.clone(),
                        -32602,
                        &format!("无效的 tools/call 参数: {}", e),
                    );
                }
            };
            let content = handle_tool_call(shared, &name, &arguments).await;
            JsonRpcResponse::success(req.id.clone(), content)
        }
        other => JsonRpcResponse::error(req.id.clone(), -32601, &format!("未知方法: {}", other)),
    }
}

/// 解析 tools/call 的 params：取 name 和 arguments。
fn parse_call_params(params: &Option<Value>) -> Result<(String, Value), String> {
    let p = params.as_ref().ok_or("params 为空")?;
    let name = p
        .get("name")
        .and_then(Value::as_str)
        .ok_or("缺少 name")?
        .to_string();
    let arguments = p
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));
    Ok((name, arguments))
}

/// 执行一个工具调用，返回 MCP `tools/call` 的 result 内容。
///
/// `result` 形如 `{content: [{type:"text", text:"..."}], isError: bool}`。
///
/// 按 `shared.kind` 分派到该实例唯一的工具：SSH MCP 只认 `exec_ssh`，DB MCP 只认
/// `exec_sql`。目标资源由模式决定：
/// - bound 模式：`shared.bound_resource_id`（用户在页面手动绑定），不从 arguments 读
///   连接名——外部客户端只需传 command / sql。
/// - client 模式：目标与凭据从 arguments 解析（host/port/username/password），
///   确认请求中的参数副本会剔除 password（防明文出现在前端浮层/事件）。
///
/// 写操作仍经人工确认。
async fn handle_tool_call(shared: &Arc<SharedState>, name: &str, arguments: &Value) -> Value {
    // 校验：工具名必须在该实例 kind 允许的工具集合内。
    if !kind_supports_tool(shared.kind, name) {
        return tool_text_result(Err(AppError::InvalidInput(format!(
            "{} 不提供工具 `{}`",
            shared.kind.label(),
            name
        ))));
    }

    // 只读工具（list_files）跳过人工确认，直接执行。
    if is_readonly_tool(name) {
        log::info!(
            "[mcp] {} 只读工具 `{}` 直接执行（目标: {}）",
            shared.kind.label(),
            name,
            resolve_target_display(shared, arguments)
        );
        let target = match resolve_target(shared, arguments) {
            Ok(t) => t,
            Err(e) => return tool_text_result(Err(e)),
        };
        let started = std::time::Instant::now();
        let res = run_target(shared, name, &target, arguments).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        log_execution(
            shared,
            name,
            &format!(
                "[只读] {} | {}",
                target.display_name(),
                readonly_detail(name, arguments)
            ),
            &res,
            elapsed_ms,
        );
        return tool_text_result(res);
    }

    // 解析本次调用的执行目标（bound：绑定资源；client：参数直连）。
    let target = match resolve_target(shared, arguments) {
        Ok(t) => t,
        Err(e) => return tool_text_result(Err(e)),
    };
    let resource_name = target.display_name();

    // 执行内容原文（记录到日志，按工具名提取关键字段，不含密码）。
    let exec_detail = exec_detail_for(name, arguments);

    // 自动放行：跳过人工确认直接执行。
    if is_auto_approved(shared.kind) {
        log::info!(
            "[mcp] {} 自动放行，直接执行（目标: {}）",
            shared.kind.label(),
            resource_name
        );
        let started = std::time::Instant::now();
        let res = run_target(shared, name, &target, arguments).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        log_execution(
            shared,
            name,
            &format!("[自动放行] {} | {}", resource_name, exec_detail),
            &res,
            elapsed_ms,
        );
        return tool_text_result(res);
    }

    // 人工确认流程（默认路径）。
    let client_name = client_name_from_args(arguments);
    let description = describe_tool(name, arguments, &resource_name);
    // client 模式：确认请求的参数副本剔除 password，避免明文密码出现在前端浮层/事件。
    let approval_args = redact_arguments(shared, arguments);
    let approval = ApprovalRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        kind: shared.kind,
        tool_name: name.into(),
        arguments: approval_args,
        description,
        client_name,
        resource_name: resource_name.clone(),
    };

    match shared
        .approval_registry()
        .request_approval(approval, &shared.app)
        .await
    {
        Ok(true) => {
            let started = std::time::Instant::now();
            let res = run_target(shared, name, &target, arguments).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            log_execution(
                shared,
                name,
                &format!("[已确认] {} | {}", resource_name, exec_detail),
                &res,
                elapsed_ms,
            );
            tool_text_result(res)
        }
        Ok(false) => {
            append_log(
                &shared.log_path,
                &format!(
                    "[{}] {} | [用户拒绝] {} | {} | REJECTED",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    name,
                    resource_name,
                    exec_detail,
                ),
            );
            tool_text_result(Err(AppError::Auth("用户拒绝了执行请求".into())))
        }
        Err(e) => tool_text_result(Err(e)),
    }
}

// ===========================================================================
// 执行目标解析（bound / client 双模式）
// ===========================================================================

/// 本次工具调用的执行目标。
#[derive(Debug)]
enum ResolvedTarget {
    /// 绑定模式：本地资源 id（SSH 会话 / DB profile）。
    Bound {
        resource_id: String,
        display: String,
    },
    /// 客户端直连模式：目标与凭据全部来自工具参数。
    Direct {
        host: String,
        port: u16,
        username: String,
        password: String,
        display: String,
    },
}

impl ResolvedTarget {
    /// 展示名：bound 为资源名，client 为 `user@host:port`（供确认浮层/日志标注来源）。
    fn display_name(&self) -> String {
        match self {
            ResolvedTarget::Bound { display, .. } | ResolvedTarget::Direct { display, .. } => {
                display.clone()
            }
        }
    }
}

/// 解析本次调用的执行目标。
///
/// - bound 模式：校验绑定资源非空，展示名为资源名（找不到时回退占位）。
/// - client 模式：从工具参数解析 host/port/username/password（password 必填，
///   仅本次调用使用），展示名为 `user@host:port`。
fn resolve_target(shared: &SharedState, arguments: &Value) -> Result<ResolvedTarget, AppError> {
    let client_mode = shared.resource_mode == "client";
    match shared.kind {
        McpKind::Ssh if client_mode => {
            let host = exec::arg_host(arguments)?;
            let port = exec::arg_port(arguments)?;
            let username = exec::arg_username(arguments)?;
            let password = exec::arg_password(arguments)?;
            Ok(ResolvedTarget::Direct {
                display: format!("{}@{}:{}", username, host, port),
                host,
                port,
                username,
                password,
            })
        }
        McpKind::Ssh => {
            if shared.bound_resource_id.is_empty() {
                return Err(AppError::InvalidInput(format!(
                    "{} 未绑定资源",
                    shared.kind.label()
                )));
            }
            let display = exec::session_name_by_id(&shared.state, &shared.bound_resource_id);
            Ok(ResolvedTarget::Bound {
                resource_id: shared.bound_resource_id.clone(),
                display,
            })
        }
        McpKind::Db if client_mode => {
            let host = exec::arg_host(arguments)?;
            let port = exec::arg_port(arguments)?;
            let username = exec::arg_username(arguments)?;
            let password = exec::arg_password(arguments)?;
            Ok(ResolvedTarget::Direct {
                display: format!("{}@{}:{}", username, host, port),
                host,
                port,
                username,
                password,
            })
        }
        McpKind::Db => {
            if shared.bound_resource_id.is_empty() {
                return Err(AppError::InvalidInput(format!(
                    "{} 未绑定资源",
                    shared.kind.label()
                )));
            }
            let display = exec::profile_name_by_id(&shared.state, &shared.bound_resource_id);
            Ok(ResolvedTarget::Bound {
                resource_id: shared.bound_resource_id.clone(),
                display,
            })
        }
        // File MCP 仅支持 bound 模式（绑定 file_account）；client 模式拒绝。
        McpKind::File if client_mode => Err(AppError::InvalidInput(format!(
            "{} 不支持客户端直连模式，请在页面绑定 S3 文件账号",
            shared.kind.label()
        ))),
        McpKind::File => {
            if shared.bound_resource_id.is_empty() {
                return Err(AppError::InvalidInput(format!(
                    "{} 未绑定资源",
                    shared.kind.label()
                )));
            }
            let display = exec::account_name_by_id(&shared.state, &shared.bound_resource_id);
            Ok(ResolvedTarget::Bound {
                resource_id: shared.bound_resource_id.clone(),
                display,
            })
        }
    }
}

/// 按已解析的目标执行工具（bound → 本地绑定资源；client → 参数直连）。
///
/// 按 `(kind, tool_name)` 双维度分派。SSH kind 下支持 exec_ssh / list_files /
/// upload_file / download_file；DB kind 下支持 exec_sql。
async fn run_target(
    shared: &Arc<SharedState>,
    tool_name: &str,
    target: &ResolvedTarget,
    arguments: &Value,
) -> AppResult<String> {
    match (shared.kind, tool_name) {
        (McpKind::Ssh, "exec_ssh") => {
            let command = exec::arg_command(arguments)?;
            match target {
                ResolvedTarget::Bound { resource_id, .. } => {
                    exec::exec_ssh_by_id(&shared.state, resource_id, &command).await
                }
                ResolvedTarget::Direct {
                    host,
                    port,
                    username,
                    password,
                    ..
                } => {
                    exec::exec_ssh_direct(
                        host,
                        *port,
                        username,
                        password,
                        &command,
                        shared.state.clone(),
                    )
                    .await
                }
            }
        }
        (McpKind::Ssh, "list_files") => {
            let path = exec::arg_path(arguments)?;
            match target {
                ResolvedTarget::Bound { resource_id, .. } => {
                    exec::list_files_by_id(&shared.state, resource_id, &path).await
                }
                ResolvedTarget::Direct {
                    host,
                    port,
                    username,
                    password,
                    ..
                } => {
                    exec::list_files_direct(
                        host,
                        *port,
                        username,
                        password,
                        &path,
                        shared.state.clone(),
                    )
                    .await
                }
            }
        }
        (McpKind::Ssh, "upload_file") => {
            let local_path = exec::arg_local_path(arguments)?;
            let remote_path = exec::arg_remote_path(arguments)?;
            match target {
                ResolvedTarget::Bound { resource_id, .. } => {
                    exec::upload_file_by_id(&shared.state, resource_id, &local_path, &remote_path)
                        .await
                }
                ResolvedTarget::Direct {
                    host,
                    port,
                    username,
                    password,
                    ..
                } => {
                    exec::upload_file_direct(
                        host,
                        *port,
                        username,
                        password,
                        &local_path,
                        &remote_path,
                        shared.state.clone(),
                    )
                    .await
                }
            }
        }
        (McpKind::Ssh, "download_file") => {
            let remote_path = exec::arg_remote_path(arguments)?;
            let local_path = exec::arg_local_path(arguments)?;
            match target {
                ResolvedTarget::Bound { resource_id, .. } => {
                    exec::download_file_by_id(&shared.state, resource_id, &remote_path, &local_path)
                        .await
                }
                ResolvedTarget::Direct {
                    host,
                    port,
                    username,
                    password,
                    ..
                } => {
                    exec::download_file_direct(
                        host,
                        *port,
                        username,
                        password,
                        &remote_path,
                        &local_path,
                        shared.state.clone(),
                    )
                    .await
                }
            }
        }
        (McpKind::Db, "exec_sql") => {
            let sql = exec::arg_sql(arguments)?;
            let limit = exec::arg_limit(arguments);
            match target {
                ResolvedTarget::Bound { resource_id, .. } => {
                    exec::exec_sql_by_id(
                        &shared.state,
                        resource_id,
                        &sql,
                        limit,
                        shared.bound_database.as_deref(),
                    )
                    .await
                }
                ResolvedTarget::Direct {
                    host,
                    port,
                    username,
                    password,
                    ..
                } => {
                    // 客户端直连：database 取参数（可选），未传则不指定默认库。
                    let database = exec::arg_database(arguments);
                    exec::exec_sql_direct(
                        host,
                        *port,
                        username,
                        password,
                        database.as_deref(),
                        &sql,
                        limit,
                    )
                    .await
                }
            }
        }
        // File MCP（绑定 S3 账号）：三个文件工具，仅 bound 模式。
        (McpKind::File, "list_files") => {
            let path = exec::arg_path(arguments)?;
            match target {
                ResolvedTarget::Bound { resource_id, .. } => {
                    exec::list_files_by_account(&shared.state, resource_id, &path).await
                }
                // resolve_target 已拒绝 File 的 client 模式，这里不会命中。
                ResolvedTarget::Direct { .. } => Err(AppError::InvalidInput(format!(
                    "{} 不支持客户端直连模式",
                    shared.kind.label()
                ))),
            }
        }
        (McpKind::File, "upload_file") => {
            let local_path = exec::arg_local_path(arguments)?;
            let remote_path = exec::arg_remote_path(arguments)?;
            match target {
                ResolvedTarget::Bound { resource_id, .. } => {
                    exec::upload_file_by_account(
                        &shared.state,
                        resource_id,
                        &local_path,
                        &remote_path,
                    )
                    .await
                }
                ResolvedTarget::Direct { .. } => Err(AppError::InvalidInput(format!(
                    "{} 不支持客户端直连模式",
                    shared.kind.label()
                ))),
            }
        }
        (McpKind::File, "download_file") => {
            let remote_path = exec::arg_remote_path(arguments)?;
            let local_path = exec::arg_local_path(arguments)?;
            match target {
                ResolvedTarget::Bound { resource_id, .. } => {
                    exec::download_file_by_account(
                        &shared.state,
                        resource_id,
                        &remote_path,
                        &local_path,
                    )
                    .await
                }
                ResolvedTarget::Direct { .. } => Err(AppError::InvalidInput(format!(
                    "{} 不支持客户端直连模式",
                    shared.kind.label()
                ))),
            }
        }
        _ => Err(AppError::InvalidInput(format!(
            "{} 不提供工具 `{}`",
            shared.kind.label(),
            tool_name
        ))),
    }
}

/// 构造发给前端的确认请求参数副本。
///
/// client 模式下工具参数含 password，原样透传会把明文密码送进前端浮层/事件，
/// 这里剔除后返回（执行仍用原始 arguments）。
fn redact_arguments(shared: &SharedState, arguments: &Value) -> Value {
    if shared.resource_mode != "client" {
        return arguments.clone();
    }
    let mut map = arguments.as_object().cloned().unwrap_or_default();
    map.remove("password");
    Value::Object(map)
}

/// 把一个 `AppResult<String>`（工具输出）封装成 MCP `tools/call` 的 result JSON。
///
/// 错误也以 `{content, isError:true}` 形式返回（MCP 规范允许在 isError 里标记）。
fn tool_text_result(res: AppResult<String>) -> Value {
    match res {
        Ok(text) => json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false,
        }),
        Err(e) => json!({
            "content": [{ "type": "text", "text": e.to_string() }],
            "isError": true,
        }),
    }
}

/// 追加一行执行日志到日志文件（若开启）。
fn append_log(log_path: &Option<PathBuf>, line: &str) {
    if let Some(path) = log_path {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
            let _ = writeln!(f, "{}", line);
        }
    }
}

/// 记录一次工具执行到日志。
fn log_execution(
    shared: &SharedState,
    tool_name: &str,
    description: &str,
    res: &AppResult<String>,
    elapsed_ms: u64,
) {
    let status = match res {
        Ok(_) => "OK".to_string(),
        Err(e) => format!("ERR: {}", e),
    };
    let line = format!(
        "[{}] {} | {} | {} ({}ms)",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        tool_name,
        description,
        status,
        elapsed_ms,
    );
    append_log(&shared.log_path, &line);
}

/// 从 arguments 推断一个 client_name（暂无标准字段，留作占位）。
fn client_name_from_args(_args: &Value) -> String {
    "mcp-client".to_string()
}

/// 生成 exec_ssh 的可读描述（用于确认弹窗）。`resource` 为绑定的 SSH 会话名。
fn describe_exec_ssh(arguments: &Value, resource: &str) -> String {
    let cmd = arguments
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("");
    format!("[SSH MCP · {}] 执行命令: {}", resource, cmd)
}

/// 生成 exec_sql 的可读描述（用于确认弹窗）。`resource` 为绑定的 DB profile 名。
fn describe_exec_sql(arguments: &Value, resource: &str) -> String {
    let sql = arguments.get("sql").and_then(Value::as_str).unwrap_or("");
    let preview: String = sql.chars().take(80).collect();
    format!("[DB MCP · {}] {}", resource, preview)
}

// ===========================================================================
// 工具元信息辅助：名称校验 / 只读判定 / 日志与确认描述
// ===========================================================================

/// 该 kind 的 MCP 实例是否支持指定工具。
///
/// SSH MCP 暴露 exec_ssh + 3 个文件工具；DB MCP 仅暴露 exec_sql。
fn kind_supports_tool(kind: McpKind, name: &str) -> bool {
    matches!(
        (kind, name),
        (McpKind::Ssh, "exec_ssh")
            | (McpKind::Ssh, "list_files")
            | (McpKind::Ssh, "upload_file")
            | (McpKind::Ssh, "download_file")
            | (McpKind::Db, "exec_sql")
            | (McpKind::File, "list_files")
            | (McpKind::File, "upload_file")
            | (McpKind::File, "download_file")
    )
}

/// 工具是否为只读（跳过人工确认）。目前 `list_files` 是只读。
fn is_readonly_tool(name: &str) -> bool {
    matches!(name, "list_files")
}

/// 只读工具执行前预先解析目标展示名（用于日志，失败时回退占位）。
fn resolve_target_display(shared: &SharedState, arguments: &Value) -> String {
    resolve_target(shared, arguments)
        .map(|t| t.display_name())
        .unwrap_or_else(|_| "(未知目标)".into())
}

/// 只读工具的日志详情（按工具名提取关键字段）。
fn readonly_detail(name: &str, arguments: &Value) -> String {
    match name {
        "list_files" => {
            let path = arguments.get("path").and_then(Value::as_str).unwrap_or("");
            format!("list_files: {}", path)
        }
        _ => name.to_string(),
    }
}

/// 按工具名生成执行内容原文（用于日志，不含密码）。
fn exec_detail_for(name: &str, arguments: &Value) -> String {
    match name {
        "exec_ssh" => {
            let cmd = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("command: {}", cmd)
        }
        "exec_sql" => {
            let sql = arguments.get("sql").and_then(Value::as_str).unwrap_or("");
            format!("sql: {}", sql)
        }
        "list_files" => {
            let path = arguments.get("path").and_then(Value::as_str).unwrap_or("");
            format!("path: {}", path)
        }
        "upload_file" | "download_file" => {
            let local = arguments
                .get("localPath")
                .and_then(Value::as_str)
                .unwrap_or("");
            let remote = arguments
                .get("remotePath")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("{}: {} ⇄ {}", name, local, remote)
        }
        other => other.to_string(),
    }
}

/// 按工具名生成确认弹窗的可读描述。
fn describe_tool(name: &str, arguments: &Value, resource: &str) -> String {
    match name {
        "exec_ssh" => describe_exec_ssh(arguments, resource),
        "exec_sql" => describe_exec_sql(arguments, resource),
        "upload_file" => {
            let local = arguments
                .get("localPath")
                .and_then(Value::as_str)
                .unwrap_or("");
            let remote = arguments
                .get("remotePath")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("[SSH MCP · {}] 上传文件: {} → {}", resource, local, remote)
        }
        "download_file" => {
            let local = arguments
                .get("localPath")
                .and_then(Value::as_str)
                .unwrap_or("");
            let remote = arguments
                .get("remotePath")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("[SSH MCP · {}] 下载文件: {} → {}", resource, remote, local)
        }
        _ => format!("[SSH MCP · {}] {}", resource, name),
    }
}

// ===========================================================================
// 工具定义（MCP inputSchema）
// ===========================================================================

/// 返回该 kind 实例对外暴露的工具定义列表 `{name, description, inputSchema}`。
///
/// - SSH MCP 暴露 `exec_ssh` + `list_files` / `upload_file` / `download_file`（文件
///   级运维，基于 SFTP 短连接）。
/// - DB MCP 暴露 `exec_sql`（若绑定了具体库，描述中注明）。
///
/// 按 `resource_mode` 分支：
/// - `"bound"`（默认）：目标由绑定资源决定，工具参数只传 command/sql/path 等。
/// - `"client"`（客户端直连）：目标与凭据由调用方在参数中传入
///   （host/port/username/password），工具描述中注明密码不存储不落日志。
fn tool_defs(kind: McpKind, resource_mode: &str, bound_database: Option<&str>) -> Vec<Value> {
    let client_mode = resource_mode == "client";
    match kind {
        McpKind::Ssh => {
            // 连接参数（client 模式专用）。
            let conn_props = if client_mode {
                json!({
                    "host": { "type": "string", "description": "目标服务器 IP 或域名" },
                    "port": {
                        "type": "integer",
                        "description": "SSH 端口，默认 22",
                        "minimum": 1,
                        "maximum": 65535
                    },
                    "username": { "type": "string", "description": "登录用户名" },
                    "password": {
                        "type": "string",
                        "description": "登录密码（敏感字段，仅本次调用有效）"
                    }
                })
            } else {
                json!({})
            };
            let conn_required = if client_mode {
                vec!["host", "username", "password"]
            } else {
                vec![]
            };

            // exec_ssh
            let mut tools = vec![];
            let mut exec_props = conn_props.clone();
            if let Some(obj) = exec_props.as_object_mut() {
                obj.insert(
                    "command".into(),
                    json!({
                        "type": "string",
                        "description": "要执行的 shell 命令（单条，非交互）"
                    }),
                );
            }
            let mut exec_required = conn_required.clone();
            exec_required.push("command");
            let exec_desc = if client_mode {
                "在调用方指定的服务器上执行一条 shell 命令（非交互），返回标准输出和标准错误的合并文本。\
目标服务器由参数 host/port/username 指定，password 为登录密码（敏感字段，仅本次调用有效，\
X-Term 不存储、不落日志）。单命令超时 30 秒，输出截断 16KB。执行前需要 X-Term 用户人工确认。"
            } else {
                "在当前 SSH MCP 绑定的服务器上执行一条 shell 命令（非交互），\
返回标准输出和标准错误的合并文本。单命令超时 30 秒，输出截断 16KB。\
目标服务器由 X-Term 用户在页面绑定（无需传连接名）。执行前需要 X-Term 用户人工确认。"
            };
            tools.push(json!({
                "name": "exec_ssh",
                "description": exec_desc,
                "inputSchema": {
                    "type": "object",
                    "properties": exec_props,
                    "required": exec_required
                }
            }));

            // list_files（只读，免确认）
            let mut list_props = conn_props.clone();
            if let Some(obj) = list_props.as_object_mut() {
                obj.insert(
                    "path".into(),
                    json!({
                        "type": "string",
                        "description": "要列举的远端目录绝对路径，如 /home/user、/var/log"
                    }),
                );
            }
            let mut list_required = conn_required.clone();
            list_required.push("path");
            let list_desc = if client_mode {
                "列出调用方指定服务器上某目录的内容（基于 SFTP）。\
返回 JSON 数组，元素含 name / isDir / size / modified。\
目标由 host/port/username 指定，password 为登录密码（敏感字段，仅本次调用有效）。\
只读操作，无需人工确认。"
            } else {
                "列出当前 SSH MCP 绑定服务器上某目录的内容（基于 SFTP）。\
返回 JSON 数组，元素含 name / isDir / size / modified。只读操作，无需人工确认。"
            };
            tools.push(json!({
                "name": "list_files",
                "description": list_desc,
                "inputSchema": {
                    "type": "object",
                    "properties": list_props,
                    "required": list_required
                }
            }));

            // upload_file（写，需确认）
            let mut up_props = conn_props.clone();
            if let Some(obj) = up_props.as_object_mut() {
                obj.insert(
                    "localPath".into(),
                    json!({
                        "type": "string",
                        "description": "X-Term 所在主机的本地文件路径（必须已存在）"
                    }),
                );
                obj.insert(
                    "remotePath".into(),
                    json!({
                        "type": "string",
                        "description": "远端目标路径（绝对路径）"
                    }),
                );
            }
            let mut up_required = conn_required.clone();
            up_required.push("localPath");
            up_required.push("remotePath");
            let up_desc = if client_mode {
                "把 X-Term 所在主机的一个本地文件上传到调用方指定服务器的远端路径（基于 SFTP）。\
目标服务器由 host/port/username 指定，password 为登录密码（敏感字段，仅本次调用有效）。\
单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。\
提示：可先用 exec_ssh 配合 shell 命令把内容写入本地文件（如 heredoc），再调用本工具上传。"
            } else {
                "把 X-Term 所在主机的一个本地文件上传到当前 SSH MCP 绑定服务器的远端路径（基于 SFTP）。\
单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。\
提示：可先用 exec_ssh 配合 shell 命令把内容写入本地文件（如 heredoc），再调用本工具上传。"
            };
            tools.push(json!({
                "name": "upload_file",
                "description": up_desc,
                "inputSchema": {
                    "type": "object",
                    "properties": up_props,
                    "required": up_required
                }
            }));

            // download_file（写本地，需确认）
            let mut dl_props = conn_props.clone();
            if let Some(obj) = dl_props.as_object_mut() {
                obj.insert(
                    "remotePath".into(),
                    json!({
                        "type": "string",
                        "description": "要下载的远端文件路径（绝对路径）"
                    }),
                );
                obj.insert(
                    "localPath".into(),
                    json!({
                        "type": "string",
                        "description": "X-Term 所在主机的本地保存路径（父目录必须存在）"
                    }),
                );
            }
            let mut dl_required = conn_required.clone();
            dl_required.push("remotePath");
            dl_required.push("localPath");
            let dl_desc = if client_mode {
                "从调用方指定服务器下载一个远端文件到 X-Term 所在主机的本地路径（基于 SFTP），\
成功后返回本地路径。目标服务器由 host/port/username 指定，password 为登录密码\
（敏感字段，仅本次调用有效）。单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。"
            } else {
                "从当前 SSH MCP 绑定服务器下载一个远端文件到 X-Term 所在主机的本地路径\
（基于 SFTP），成功后返回本地路径。单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。"
            };
            tools.push(json!({
                "name": "download_file",
                "description": dl_desc,
                "inputSchema": {
                    "type": "object",
                    "properties": dl_props,
                    "required": dl_required
                }
            }));

            tools
        }
        McpKind::Db => {
            if client_mode {
                vec![json!({
                    "name": "exec_sql",
                    "description": "在调用方指定的 MySQL 服务器上执行 SQL，返回结果表格文本。\
                目标数据库由参数 host/port/username 指定，password 为连接密码（敏感字段，仅本次调用有效，\
                X-Term 不存储、不落日志）。database 可选，传则作为默认库。\
                执行前需要 X-Term 用户人工确认。",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "host": {
                                "type": "string",
                                "description": "目标 MySQL 服务器 IP 或域名"
                            },
                            "port": {
                                "type": "integer",
                                "description": "MySQL 端口，默认 3306",
                                "minimum": 1,
                                "maximum": 65535
                            },
                            "username": {
                                "type": "string",
                                "description": "连接用户名"
                            },
                            "password": {
                                "type": "string",
                                "description": "连接密码（敏感字段，仅本次调用有效）"
                            },
                            "database": {
                                "type": "string",
                                "description": "默认数据库（可选，省略则 SQL 需带库名限定）"
                            },
                            "sql": { "type": "string", "description": "要执行的 SQL 语句" },
                            "limit": {
                                "type": "integer",
                                "description": "返回行数上限，默认 100",
                                "minimum": 1
                            }
                        },
                        "required": ["host", "username", "password", "sql"]
                    }
                })]
            } else {
                let db_hint = match bound_database {
                    Some(db) => format!("当前绑定的数据库为「{}」，SQL 将在此库上执行。", db),
                    None => "目标数据库由 X-Term 用户在页面绑定（无需传连接名）。".to_string(),
                };
                vec![json!({
                    "name": "exec_sql",
                    "description": format!(
                        "在当前 DB MCP 绑定的 MySQL 上执行 SQL，返回结果表格文本。{}\
                执行前需要 X-Term 用户人工确认。",
                        db_hint
                    ),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "sql": { "type": "string", "description": "要执行的 SQL 语句" },
                            "limit": {
                                "type": "integer",
                                "description": "返回行数上限，默认 100",
                                "minimum": 1
                            }
                        },
                        "required": ["sql"]
                    }
                })]
            }
        }
        // File MCP：绑定 S3 文件账号，仅 bound 模式，暴露三个文件工具。
        McpKind::File => vec![
            json!({
                "name": "list_files",
                "description": "列出当前 File MCP 绑定的 S3 存储桶中某前缀下的内容。\
            返回 JSON 数组，元素含 name / isDir / size / modified。只读操作，无需人工确认。\
            目标账号由 X-Term 用户在页面绑定（无需传连接信息）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "对象 key 前缀（目录），以 `/` 结尾，如 logs/、backup/2024/。传空串表示 bucket 根。"
                        }
                    },
                    "required": ["path"]
                }
            }),
            json!({
                "name": "upload_file",
                "description": "把 X-Term 所在主机的一个本地文件上传到当前 File MCP 绑定的 S3 存储桶。\
            单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。\
            提示：可先用 exec_ssh 配合 shell 命令把内容写入本地文件（如 heredoc），再调用本工具上传。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "localPath": {
                            "type": "string",
                            "description": "X-Term 所在主机的本地文件路径（必须已存在）"
                        },
                        "remotePath": {
                            "type": "string",
                            "description": "S3 对象 key（目标路径），如 logs/app.log"
                        }
                    },
                    "required": ["localPath", "remotePath"]
                }
            }),
            json!({
                "name": "download_file",
                "description": "从当前 File MCP 绑定的 S3 存储桶下载一个对象到 X-Term 所在主机的本地路径，\
            成功后返回本地路径。单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "remotePath": {
                            "type": "string",
                            "description": "要下载的 S3 对象 key"
                        },
                        "localPath": {
                            "type": "string",
                            "description": "X-Term 所在主机的本地保存路径（父目录必须存在）"
                        }
                    },
                    "required": ["remotePath", "localPath"]
                }
            }),
        ],
    }
}

// ===========================================================================
// JSON-RPC 类型
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    /// JSON-RPC 请求 id。通知（notification）无此字段，serde default 为 Null。
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

impl JsonRpcResponse {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, code: i64, message: &str) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

// ===========================================================================
// SharedState 便捷访问
// ===========================================================================

impl SharedState {
    /// 获取 approval registry（从 AppState 取）。
    fn approval_registry(&self) -> SharedApprovalRegistry {
        self.state.approval_registry.clone()
    }
}
