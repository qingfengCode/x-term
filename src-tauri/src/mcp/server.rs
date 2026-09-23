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
//! - 默认仅绑定 `127.0.0.1`；监听地址可由用户配置为 0.0.0.0 / 局域网 IP，
//!   暴露风险由用户自行承担。
//! - exec_* 工具调用必须经人工确认（见 [`crate::mcp::approval`]）。

use std::collections::HashMap;
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
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinHandle;

use crate::error::{AppError, AppResult};
use crate::mcp::approval::{ApprovalRequest, McpKind, SharedApprovalRegistry};
use crate::mcp::bastion;
use crate::mcp::exec;
use crate::mcp::tools;
use crate::state::AppState;

/// MCP 协议版本（与 Claude Desktop 等客户端协商用）。
const PROTOCOL_VERSION: &str = "2024-11-05";

/// 资源上限（防御持 token 的恶意/故障客户端打爆本机资源）：
/// - 并发工具调用数上限（每个调用最长可阻塞 5 分钟等人工确认 + 30s 执行）；
/// - SSE 长连接数上限（每个连接占一个后台任务 + 一个 channel 缓冲）；
/// - 每个 SSE 连接的待推送响应队列长度（客户端不读时限制内存堆积）。
const MAX_CONCURRENT_CALLS: usize = 16;
const MAX_SSE_CLIENTS: usize = 32;
const SSE_CHANNEL_CAPACITY: usize = 256;

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
    /// 完整 SSE 入口 URL（如 `http://127.0.0.1:8765/sse`）。
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
    /// 路由共享状态（`mcp_rebind` 热切换绑定资源时直接改这里的字段）。
    shared: Arc<SharedState>,
}

/// 两个 kind 各自最多一个运行实例：Ssh / Db。
static SERVERS: Lazy<Mutex<HashMap<McpKind, ServerHandle>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 运行时运行模式（per-kind，"manual"/"whitelist"/"auto"）。`mcp_save_config`
/// 更新此值，改后立即生效无需重启。
static RUN_MODES: Lazy<Mutex<HashMap<McpKind, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// 设置指定 kind 的运行模式（由 commands::mcp 的启动/保存配置调用）。
pub fn set_run_mode(kind: McpKind, mode: &str) {
    let m = match mode {
        "whitelist" | "auto" => mode.to_string(),
        _ => "manual".to_string(),
    };
    RUN_MODES.lock().insert(kind, m);
}

/// 查询指定 kind 的运行模式（默认 "manual"，全部人工确认）。
pub fn run_mode_of(kind: McpKind) -> String {
    RUN_MODES
        .lock()
        .get(&kind)
        .cloned()
        .unwrap_or_else(|| "manual".into())
}

/// 运行模式的中文标签（执行日志头部用）。
fn run_mode_label(mode: &str) -> &'static str {
    match mode {
        "whitelist" => "白名单运行",
        "auto" => "自动运行",
        _ => "手动运行",
    }
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

/// 查询 SSH MCP 是否正以堡垒机模式运行。
///
/// 供 `bastion::stop_all` 区分「正常停止」与「停止后立即重启」的竞态：
/// - 未运行（正常停止）：回收全部堡垒机会话；
/// - 以堡垒机模式重新运行：跳过回收（旧会话仍可用，且新实例会重启空闲
///   回收任务兜底），避免误杀重启间隙建立的会话；
/// - 以其它模式重新运行：回收（堡垒机使用已结束，不再有回收任务清理）。
pub(crate) fn ssh_bastion_mode_running() -> bool {
    let guard = SERVERS.lock();
    match guard.get(&crate::mcp::approval::McpKind::Ssh) {
        Some(h) => h.shared.resource_mode == "bastion",
        None => false,
    }
}

/// 启动 MCP 服务端。
///
/// 绑定 `host:port`，用 `token` 做 Bearer 校验。成功后后台 spawn 一个 axum 任务运行，
/// 句柄存入全局 [`SERVERS`]。host 允许任意监听地址（0.0.0.0 / 局域网 IP 亦可，
/// 暴露风险由用户自行承担）。
///
/// `bound_resource_id` 是该 MCP 绑定的资源 id（SSH 会话 id 或 DB profile id，或
/// bound_source=terminal 时的终端实例 id），执行工具时按此 id 解析目标；
/// `resource_mode == "client"`（客户端直连）时传 `None`，目标与凭据由调用方在
/// 工具参数中传入。若该 kind 已有实例运行则返回错误。
///
/// `bound_resource_ids` 是多机模式（`resource_mode == "multi"`，仅 SSH）绑定的
/// 会话 id 集合；其它模式传空 Vec。
///
/// `bastion` 是堡垒机模式的运行参数（登录后命令 / 基础连接保持时长 /
/// 会话空闲回收），经 [`bastion::configure`] 注入。
#[allow(clippy::too_many_arguments)]
pub async fn start_mcp_server(
    kind: McpKind,
    app: AppHandle,
    state: AppState,
    host: String,
    port: u16,
    token: String,
    bound_resource_id: Option<String>,
    bound_resource_ids: Vec<String>,
    bound_source: String,
    bound_database: Option<String>,
    resource_mode: String,
    run_mode: String,
    enable_log: bool,
    bastion: bastion::BastionOptions,
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

    // 启动时设置运行模式。
    set_run_mode(kind, &run_mode);

    // 资源模式规范化：multi / bastion 仅 SSH kind 支持（其它 kind 回退 bound），
    // 其余按 client / bound 处理。
    let resource_mode = match resource_mode.as_str() {
        "client" => "client",
        "multi" if kind == McpKind::Ssh => "multi",
        "multi" => {
            log::warn!("[mcp] {} 不支持多机模式，已回退为单资源绑定", kind.label());
            "bound"
        }
        "bastion" if kind == McpKind::Ssh => "bastion",
        "bastion" => {
            log::warn!(
                "[mcp] {} 不支持堡垒机模式，已回退为单资源绑定",
                kind.label()
            );
            "bound"
        }
        _ => "bound",
    };
    let bound_resource_id = bound_resource_id.unwrap_or_default();
    // 多机模式：空集合视为配置错误（启动前应已校验，双保险）。
    if resource_mode == "multi" && bound_resource_ids.is_empty() {
        return Err(AppError::Config(
            "SSH MCP 多机模式未勾选任何机器：请先在 MCP 页面勾选至少一台".into(),
        ));
    }
    // 绑定来源规范化：仅 "terminal" 视为终端标签页绑定，其余一律按 "config"。
    // （commands 层已校验合法性，这里双保险。）
    let bound_source = if bound_source == "terminal" {
        "terminal"
    } else {
        "config"
    };

    // 绑定监听端口：填什么就 bind 什么（IP / 主机名均可，由系统解析；
    // 地址不存在、不可用或端口被占用时启动失败并返回错误）。
    let listener = TcpListener::bind((host.as_str(), port)).await.map_err(|e| {
        AppError::Io(std::io::Error::new(
            e.kind(),
            format!(
                "绑定 {}:{} 失败（地址不存在/不可用或端口被占用）: {}",
                host, port, e
            ),
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
        } else if resource_mode == "multi" {
            // 启动时快照机器清单（热切换的变更由逐条执行日志体现）。
            exec::multi_machines(&state, &bound_resource_ids)
                .map(|ms| {
                    format!(
                        "(多机 {} 台) {}",
                        ms.len(),
                        ms.iter()
                            .map(|m| m.display_name.as_str())
                            .collect::<Vec<_>>()
                            .join("、")
                    )
                })
                .unwrap_or_else(|e| format!("(多机) 机器清单解析失败: {}", e))
        } else if resource_mode == "bastion" {
            format!(
                "(堡垒机) {}",
                exec::session_name_by_id(&state, &bound_resource_id)
            )
        } else if bound_source == "terminal" {
            format!("(终端标签页) {}", bound_resource_id)
        } else {
            bound_resource_id.clone()
        };
        let header = format!(
            "=== X-Term {} 执行日志 ===\n启动时间: {}\n监听: {}:{}\n资源模式: {}\n绑定资源: {}\n绑定数据库: {}\n运行模式: {}\n---\n",
            kind.label(),
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            host,
            port,
            resource_mode,
            bound_desc,
            bound_database.as_deref().unwrap_or("(默认)"),
            run_mode_label(&run_mode),
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
        bound_source: Mutex::new(bound_source.to_string()),
        bound_resource_id: Mutex::new(bound_resource_id),
        bound_resource_ids: Mutex::new(bound_resource_ids),
        bound_database,
        token,
        clients: Arc::new(Mutex::new(HashMap::new())),
        client_names: Arc::new(Mutex::new(HashMap::new())),
        streamable_sessions: Arc::new(Mutex::new(HashMap::new())),
        sse_shutdown: sse_shutdown.clone(),
        call_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_CALLS)),
        sse_slots: Arc::new(Semaphore::new(MAX_SSE_CLIENTS)),
        log_path,
    });

    // CORS：允许所有来源/方法/头（MCP 客户端需要）。
    let cors = tower_http::cors::CorsLayer::very_permissive();

    let app_router = Router::new()
        .route("/mcp", post(mcp_handler))
        .route("/sse", get(sse_handler))
        .route("/messages", post(messages_handler))
        .layer(cors)
        .with_state(shared.clone());

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
                shared,
            },
        );
    }

    // 堡垒机模式：注入运行参数并启动后台回收任务（stop_mcp_server 时随 stop_all 终止）。
    if resource_mode == "bastion" {
        let login_cmd = bastion.post_login_command.trim().to_string();
        let base_idle = bastion.base_idle_minutes;
        let session_idle = bastion.session_idle_minutes;
        bastion::configure(bastion);
        bastion::start_reaper();
        log::info!(
            "[mcp] SSH MCP 堡垒机模式：登录后命令「{}」｜基础连接保持 {}｜会话空闲回收 {}",
            if login_cmd.is_empty() {
                "（未配置）"
            } else {
                &login_cmd
            },
            if base_idle == 0 {
                "不保持（用完即断）".to_string()
            } else {
                format!("{} 分钟", base_idle)
            },
            if session_idle == 0 {
                "已关闭".to_string()
            } else {
                format!("{} 分钟", session_idle)
            }
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
            // 3. 堡垒机模式：停止空闲回收并断开全部堡垒机会话（与 4 的兜底
            //    任务一样，本函数是同步的，须经 async_runtime spawn）。
            if kind == McpKind::Ssh {
                tauri::async_runtime::spawn(async move {
                    bastion::stop_all().await;
                });
            }
            // 4. 兜底任务：3 秒后仍未退出则 abort（abort 与 join 解耦，join 被
            //    timeout 消费后仍可 abort）。
            let join = h.join;
            let abort = h.abort;
            // 注意：本函数是同步的（经同步 Tauri 命令在主线程调用，无 tokio 运行时
            // 上下文），不能直接用 tokio::spawn（会 panic 导致整个程序崩溃）。
            // tauri::async_runtime::spawn 内部会先 enter 运行时，任意线程均可安全调用。
            tauri::async_runtime::spawn(async move {
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

/// 运行中热切换绑定的资源（会话配置 / 终端标签页），立即生效无需重启。
///
/// 由 `commands::mcp::mcp_rebind` 调用：`bound_source` 已规范化（"config"|"terminal"），
/// `resource_id` 已做存在性校验。服务未运行时返回错误（此时只需保存配置，
/// 启动时自然生效）。
pub fn rebind_mcp(kind: McpKind, bound_source: &str, resource_id: &str) -> AppResult<()> {
    let guard = SERVERS.lock();
    match guard.get(&kind) {
        Some(h) => {
            *h.shared.bound_source.lock() = bound_source.to_string();
            *h.shared.bound_resource_id.lock() = resource_id.to_string();
            log::info!(
                "[mcp] {} 热切换绑定：来源 {}, 资源 {}",
                kind.label(),
                bound_source,
                resource_id
            );
            Ok(())
        }
        None => Err(AppError::InvalidInput(format!(
            "{} 未运行，无需热切换（配置已保存，启动时生效）",
            kind.label()
        ))),
    }
}

/// 运行中热切换多机模式的机器集合，立即生效无需重启。
///
/// 由 `commands::mcp::mcp_rebind_multi` 调用（ids 已做存在性校验）。
/// 热切换后 tools/list 会按新集合生成 target 枚举；已建立的客户端会话可能
/// 缓存旧 schema，其旧 target 会执行失败并收到可用目标清单，模型可自行纠正。
/// 服务未运行或非 multi 模式时返回错误。
pub fn rebind_mcp_multi(kind: McpKind, resource_ids: &[String]) -> AppResult<()> {
    let guard = SERVERS.lock();
    match guard.get(&kind) {
        Some(h) => {
            if h.shared.resource_mode != "multi" {
                return Err(AppError::InvalidInput(format!(
                    "{} 当前不在多机模式运行（切换模式需重启服务）",
                    kind.label()
                )));
            }
            *h.shared.bound_resource_ids.lock() = resource_ids.to_vec();
            log::info!(
                "[mcp] {} 多机模式热切换：{} 台机器",
                kind.label(),
                resource_ids.len()
            );
            Ok(())
        }
        None => Err(AppError::InvalidInput(format!(
            "{} 未运行，无需热切换（配置已保存，启动时生效）",
            kind.label()
        ))),
    }
}

// ===========================================================================
// 共享状态
// ===========================================================================

/// 每个 SSE 连持有一个发送端：用于把 JSON-RPC 响应推回该连接。
///
/// 有界 channel（容量 [`SSE_CHANNEL_CAPACITY`]）：客户端不读时发送端
/// `send().await` 阻塞而不是无限堆积内存。
type SseSender = mpsc::Sender<String>;

/// 路由共享状态。
struct SharedState {
    app: AppHandle,
    state: AppState,
    /// 该实例是 SSH MCP 还是 DB MCP（决定对外暴露哪个工具）。
    kind: McpKind,
    /// 资源模式："bound"（绑定本地资源）| "client"（客户端直连，目标/凭据来自参数）
    /// | "multi"（多机模式，仅 SSH：绑定一组会话，由外部 AI 按 target 参数自选目标）。
    /// 决定工具定义、目标解析与确认请求的脱敏行为。
    resource_mode: String,
    /// 绑定来源："config"（绑定会话配置，执行时新建短连接，默认）|
    /// "terminal"（绑定已打开的终端标签页，命令写入该终端 PTY 执行，支持
    /// A→B→C 跳板嵌套场景）。仅 SSH kind + bound 模式有效。
    /// 用 Mutex 包裹支持运行中热切换（`mcp_rebind`）。
    bound_source: Mutex<String>,
    /// 绑定的资源 id：SSH 会话 id / DB profile id（bound_source=config）或
    /// 终端实例 id（bound_source=terminal）。工具执行时按此 id 解析目标，
    /// 外部客户端无需传连接名。client 模式下为空串。
    bound_resource_id: Mutex<String>,
    /// 多机模式（multi）绑定的 SSH 会话 id 集合（其它模式为空）。用 Mutex 包裹
    /// 支持运行中热切换（`mcp_rebind_multi`）；执行时按 target 展示名在其中路由。
    bound_resource_ids: Mutex<Vec<String>>,
    /// 绑定的具体数据库名（仅 kind=Db 有效）。设置后 exec_sql 只针对该库。
    bound_database: Option<String>,
    token: String,
    /// sessionId -> 该 SSE 连接的 mpsc 发送端（旧 HTTP+SSE 传输用）。
    clients: Arc<Mutex<HashMap<String, SseSender>>>,
    /// sessionId -> 客户端名（initialize 的 clientInfo.name，两种传输共用）。
    ///
    /// 用于确认卡片与执行日志标注来源客户端（Cursor / Claude Desktop 等）；
    /// SSE 断开时随 [`SseCleanupGuard`] 清理。
    client_names: Arc<Mutex<HashMap<String, String>>>,
    /// Streamable HTTP 会话表：session_id -> ()（仅记录存在性，spec 2025-03-26）。
    streamable_sessions: Arc<Mutex<HashMap<String, ()>>>,
    /// 服务停止信号：`stop_mcp_server` 通知后所有 SSE 流结束（关闭长连接）。
    sse_shutdown: Arc<tokio::sync::Notify>,
    /// 并发工具调用信号量：有界任务数（见 [`MAX_CONCURRENT_CALLS`]）。
    call_slots: Arc<Semaphore>,
    /// SSE 长连接数信号量：有界连接数（见 [`MAX_SSE_CLIENTS`]）。
    sse_slots: Arc<Semaphore>,
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

    // SSE 连接数上限：恶意客户端可低成本建立海量 SSE 长连接（每个连接
    // 占一个后台任务 + 一个 channel 缓冲），超限直接拒绝。
    let sse_permit = match shared.sse_slots.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            log::warn!(
                "[mcp] SSE 连接数已达上限（{}），拒绝新连接",
                MAX_SSE_CLIENTS
            );
            return (
                StatusCode::TOO_MANY_REQUESTS,
                format!("SSE 连接数已达上限（{}）", MAX_SSE_CLIENTS),
            )
                .into_response();
        }
    };

    let session_id = uuid::Uuid::new_v4().to_string();
    // 有界 channel：客户端不读时响应堆积被限制在 SSE_CHANNEL_CAPACITY 条内。
    let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);

    shared.clients.lock().insert(session_id.clone(), tx);

    log::info!("[mcp] SSE 客户端连接: {}", session_id);

    // 先推 endpoint 事件（告知客户端消息端点）。
    let endpoint_url = format!("/messages?sessionId={}", session_id);
    let initial = Some(Event::default().event("endpoint").data(endpoint_url));

    // cleanup guard：stream 被 drop 时（客户端断开）自动清理 clients map、
    // 客户端名表并释放连接名额。
    let cleanup = SseCleanupGuard {
        clients: shared.clients.clone(),
        client_names: shared.client_names.clone(),
        session_id: session_id.clone(),
        _sse_permit: sse_permit,
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
/// 自动从 `clients` / `client_names` map 中移除对应的 session 条目，
/// 并释放 SSE 连接名额。
struct SseCleanupGuard {
    clients: Arc<Mutex<HashMap<String, SseSender>>>,
    client_names: Arc<Mutex<HashMap<String, String>>>,
    session_id: String,
    /// 持有 SSE 连接名额，drop 时自动归还（见 [`SharedState::sse_slots`]）。
    _sse_permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for SseCleanupGuard {
    fn drop(&mut self) {
        self.clients.lock().remove(&self.session_id);
        self.client_names.lock().remove(&self.session_id);
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

    // 4.5 解析客户端名：initialize 时登记 clientInfo.name，后续请求按会话查表。
    let client_name = if req.method == "initialize" {
        let name = client_name_from_initialize(&req.params);
        shared
            .client_names
            .lock()
            .insert(session_id.clone(), name.clone());
        name
    } else {
        shared
            .client_names
            .lock()
            .get(&session_id)
            .cloned()
            .unwrap_or_else(|| "unknown-client".into())
    };

    // 5. 异步执行并推送响应（不阻塞 POST 响应）。
    // 并发信号量：每个调用最长可阻塞 5 分钟（等人工确认），无界 spawn 会被
    // 恶意客户端低成本打爆任务数；满时直接回 busy 错误。
    let shared_clone = shared.clone();
    let req_clone = req.clone();
    let slots = shared.call_slots.clone();
    let sid_for_log = session_id.clone();
    tokio::spawn(async move {
        let _permit = match slots.try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                let busy = JsonRpcResponse::error(
                    req_clone.id.clone(),
                    -32000,
                    &format!("服务繁忙，请稍后重试（并发调用数已达上限 {}）", MAX_CONCURRENT_CALLS),
                );
                let _ = tx
                    .send(serde_json::to_string(&busy).unwrap_or_default())
                    .await;
                return;
            }
        };
        let response = dispatch(&shared_clone, &req_clone, &client_name).await;
        let json_str = match serde_json::to_string(&response) {
            Ok(s) => s,
            Err(e) => {
                log::error!("[mcp] 序列化 JSON-RPC 响应失败: {}", e);
                return;
            }
        };
        // 有界 channel：客户端不读时此处阻塞而非无限堆积内存。
        if tx.send(json_str).await.is_err() {
            log::warn!("[mcp] SSE 会话 {} 已断开，响应未送达", sid_for_log);
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

    // 3. initialize：生成 session，响应头带 mcp-session-id；登记 clientInfo.name。
    if req.method == "initialize" {
        let session_id = uuid::Uuid::new_v4().to_string();
        {
            let mut sessions = shared.streamable_sessions.lock();
            // 防止长期运行后 map 无限增长：超过上限时清空重建。
            // 该 map 仅用于诊断日志（宽松放行），清空不影响功能。
            if sessions.len() >= 1024 {
                sessions.clear();
                shared.client_names.lock().clear();
            }
            sessions.insert(session_id.clone(), ());
        }
        let client_name = client_name_from_initialize(&req.params);
        shared
            .client_names
            .lock()
            .insert(session_id.clone(), client_name.clone());
        log::info!(
            "[mcp] Streamable HTTP 客户端初始化: session={} client={}",
            session_id,
            client_name
        );

        // 并发信号量：dispatch（tools/call 等）最长可阻塞 5 分钟，超限直接拒绝。
        let permit = match shared.call_slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    format!("服务繁忙，请稍后重试（并发调用数已达上限 {}）", MAX_CONCURRENT_CALLS),
                )
                    .into_response();
            }
        };
        let response = dispatch(&shared, &req, &client_name).await;
        drop(permit);
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
    // 按会话查客户端名（initialize 时登记），确认卡片与日志标注来源。
    let mut client_name = "unknown-client".to_string();
    if let Some(sid) = headers.get(MCP_SESSION_ID) {
        if let Ok(s) = sid.to_str() {
            let sessions = shared.streamable_sessions.lock();
            if !sessions.contains_key(s) {
                log::warn!("[mcp] 未知 mcp-session-id: {}（宽松放行）", s);
            }
            if let Some(name) = shared.client_names.lock().get(s).cloned() {
                client_name = name;
            }
        }
    }

    // 6. 执行并返回 application/json。
    // 并发信号量：与 initialize 分支一致，tools/call 最长阻塞 5 分钟（等人工确认），
    // 超限直接返回 429，避免无界并发任务。
    let permit = match shared.call_slots.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                format!("服务繁忙，请稍后重试（并发调用数已达上限 {}）", MAX_CONCURRENT_CALLS),
            )
                .into_response();
        }
    };
    let response = dispatch(&shared, &req, &client_name).await;
    drop(permit);
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
/// - `tools/list`：返回该实例的工具定义。
/// - `tools/call`：执行工具（list_* 直接；exec_* 经人工确认）。
/// - 其它方法：返回 method not found 错误。
///
/// `client_name` 是发起请求的 MCP 客户端名（initialize 的 clientInfo.name，
/// 由传输层按会话解析），用于确认卡片与执行日志标注来源。
async fn dispatch(shared: &Arc<SharedState>, req: &JsonRpcRequest, client_name: &str) -> JsonRpcResponse {
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
            // 多机模式：机器清单现算（热切换后立即反映新集合），驱动 target 枚举。
            // 宽松解析：个别会话被删除时跳过（其余机器照常暴露）。
            let machines = if shared.kind == McpKind::Ssh && shared.resource_mode == "multi" {
                exec::multi_machines_lenient(
                    &shared.state,
                    &shared.bound_resource_ids.lock(),
                    false,
                )
                .map(|(ms, _)| ms)
                .unwrap_or_default()
            } else {
                Vec::new()
            };
            let defs = tools::tool_defs(
                shared.kind,
                &shared.resource_mode,
                &shared.bound_source.lock(),
                shared.bound_database.as_deref(),
                &machines,
            );
            JsonRpcResponse::success(req.id.clone(), json!({ "tools": defs }))
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
            let content = handle_tool_call(shared, &name, &arguments, client_name).await;
            JsonRpcResponse::success(req.id.clone(), content)
        }
        other => JsonRpcResponse::error(req.id.clone(), -32601, &format!("未知方法: {}", other)),
    }
}

/// 从 initialize 请求参数解析客户端名（`params.clientInfo.name`）。
///
/// MCP 规范要求客户端在 initialize 中自报 clientInfo（如 "cursor" /
/// "claude-ai"/ "cline"）；缺失时回退 "unknown-client"。
fn client_name_from_initialize(params: &Option<Value>) -> String {
    params
        .as_ref()
        .and_then(|p| p.get("clientInfo"))
        .and_then(|c| c.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown-client")
        .to_string()
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
/// 按 `shared.kind` 分派到该实例的工具（工具集合见 [`tools::kind_supports_tool`]）。
/// 目标资源由模式决定：
/// - bound 模式：`shared.bound_resource_id`（用户在页面手动绑定），不从 arguments 读
///   连接名——外部客户端只需传 command / sql。
/// - client 模式：目标与凭据从 arguments 解析（host/port/username/password），
///   确认请求中的参数副本会剔除 password（防明文出现在前端浮层/事件）。
///
/// 写操作仍经人工确认；`client_name` 用于确认卡片与日志标注来源客户端。
async fn handle_tool_call(
    shared: &Arc<SharedState>,
    name: &str,
    arguments: &Value,
    client_name: &str,
) -> Value {
    // 校验：工具名必须在该实例 kind 允许的工具集合内。
    if !tools::kind_supports_tool(shared.kind, name) {
        return tool_text_result(Err(AppError::InvalidInput(format!(
            "{} 不提供工具 `{}`",
            shared.kind.label(),
            name
        ))));
    }

    // 堡垒机工具仅在堡垒机模式（启动时固化）下提供。
    if name.starts_with("bastion_") && shared.resource_mode != "bastion" {
        return tool_text_result(Err(AppError::InvalidInput(
            "堡垒机工具仅在 SSH MCP 的「堡垒机模式」下可用（切换模式需重启服务）".into(),
        )));
    }

    // 反向守卫：堡垒机模式只允许 bastion_* 工具。
    // 若放行 exec_ssh / 文件三件套，resolve_target 会解析成绑定的堡垒机会话配置，
    // 命令将**直接落在堡垒机服务器本机**执行，绕过"先进资产主机"的会话模型。
    // tools/list 虽不暴露它们，但客户端可能缓存旧 schema 或自定义调用，故显式拒绝。
    if shared.resource_mode == "bastion" && !name.starts_with("bastion_") {
        return tool_text_result(Err(AppError::InvalidInput(format!(
            "SSH MCP 当前为「堡垒机模式」，只能使用 bastion_* 会话工具（`{}` 不可用）。\
请先用 bastion_create_session 进入资产主机，再用 bastion_session_exec 在该主机执行命令",
            name
        ))));
    }

    // 只读工具（list_files / list_machines / bastion_list_*）跳过人工确认，直接执行。
    if tools::is_readonly_tool(name) {
        // list_machines（多机模式专用）：无 target 参数、无需解析执行目标，
        // 直接返回授权机器清单（非 multi 模式调用则拒绝，防越权枚举）。
        if name == "list_machines" {
            if shared.kind != McpKind::Ssh || shared.resource_mode != "multi" {
                return tool_text_result(Err(AppError::InvalidInput(
                    "list_machines 仅在多机模式下可用".into(),
                )));
            }
            let machines =
                exec::multi_machines_lenient(&shared.state, &shared.bound_resource_ids.lock(), false)
                    .map(|(ms, _)| ms);
            let res = machines.map(|ms| tools::format_machines(&ms));
            log_execution(shared, name, "[只读] 列出授权机器清单", &res, 0);
            return tool_text_result(res);
        }
        // bastion_list_sessions（堡垒机模式专用）：直接返回活跃会话清单。
        if name == "bastion_list_sessions" {
            let res: AppResult<String> = Ok(bastion::list_sessions().await);
            log_execution(shared, name, "[只读] 列出堡垒机会话", &res, 0);
            return tool_text_result(res);
        }
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
                tools::readonly_detail(name, arguments)
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
    let exec_detail = tools::exec_detail_for(name, arguments);

    // 运行模式判定（与 AI 助手的执行模式语义一致）：
    // - manual：人工确认（默认）；
    // - auto：全部自动执行；
    // - whitelist：白名单内自动放行（SSH=命令白名单、DB=只读 SQL），其余确认。
    // 例外：文件传输工具（upload_file / download_file / bastion_upload_file）
    // 读写**本机任意路径**（localPath 不设沙箱是功能刚需——外部客户端要把本机
    // 任意文件上传到 S3/SFTP/目标主机），无论何种模式都必须人工确认；确认卡片
    // 会展示完整的本地/远端路径与目标，用户可见可控。
    let mode = run_mode_of(shared.kind);
    let is_file_tool = matches!(
        name,
        "upload_file" | "download_file" | "bastion_upload_file"
    );
    let whitelist_hit = mode == "whitelist"
        && !is_file_tool
        && tools::is_whitelist_auto(&shared.state, name, arguments);
    let auto_run = (mode == "auto" || whitelist_hit) && !is_file_tool;
    if auto_run {
        let tag = if whitelist_hit { "白名单放行" } else { "自动放行" };
        log::info!(
            "[mcp] {} {}，直接执行（目标: {}）",
            shared.kind.label(),
            tag,
            resource_name
        );
        let started = std::time::Instant::now();
        let res = run_target(shared, name, &target, arguments).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        log_execution(
            shared,
            name,
            &format!("[{}] {} | {}", tag, resource_name, exec_detail),
            &res,
            elapsed_ms,
        );
        return tool_text_result(res);
    }

    // 人工确认流程（默认路径）。
    let description = tools::describe_tool(shared.kind, name, arguments, &resource_name);
    // client 模式：确认请求的参数副本剔除 password，避免明文密码出现在前端浮层/事件。
    let approval_args = redact_arguments(shared, arguments);
    let approval = ApprovalRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        kind: shared.kind,
        tool_name: name.into(),
        arguments: approval_args,
        description,
        client_name: client_name.to_string(),
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
    /// 终端标签页绑定模式：命令写入该终端 PTY 执行（支持跳板嵌套）。
    Terminal {
        instance_id: String,
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
    /// 展示名：bound 为资源名，terminal 为终端名，client 为 `user@host:port`
    /// （供确认浮层/日志标注来源）。
    fn display_name(&self) -> String {
        match self {
            ResolvedTarget::Bound { display, .. }
            | ResolvedTarget::Terminal { display, .. }
            | ResolvedTarget::Direct { display, .. } => display.clone(),
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
            // 堡垒机模式：目标统一为绑定的堡垒机会话配置（各 bastion_* 工具再按
            // 参数 sessionId 在堡垒机会话表内路由，不走 multi/terminal 分支）。
            if shared.resource_mode == "bastion" {
                let resource_id = shared.bound_resource_id.lock().clone();
                if resource_id.is_empty() {
                    return Err(AppError::InvalidInput(
                        "SSH MCP 未绑定堡垒机会话配置，请先在 X-Term 的 MCP 页面绑定".into(),
                    ));
                }
                let display = exec::session_name_by_id(&shared.state, &resource_id);
                return Ok(ResolvedTarget::Bound {
                    resource_id,
                    display,
                });
            }
            // 多机模式：从工具参数 target（展示名）在授权集合内路由。
            // 不在集合内 / 缺失 target 都报错并回显可用清单（模型可自行纠正）。
            // 宽松解析：个别会话被删除时跳过（其余机器照常可用）。
            if shared.resource_mode == "multi" {
                let ids = shared.bound_resource_ids.lock().clone();
                if ids.is_empty() {
                    return Err(AppError::Config(
                        "多机模式未勾选任何机器：请先在 X-Term 的 MCP 页面勾选".into(),
                    ));
                }
                let (machines, _) = exec::multi_machines_lenient(&shared.state, &ids, false)?;
                let target = arguments
                    .get("target")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if target.is_empty() {
                    return Err(AppError::InvalidInput(format!(
                        "多机模式必须传 target 参数指定目标机器。可用目标：{}",
                        machines
                            .iter()
                            .map(|m| m.display_name.as_str())
                            .collect::<Vec<_>>()
                            .join("、")
                    )));
                }
                let m = exec::multi_resolve(&machines, target)?;
                return Ok(ResolvedTarget::Bound {
                    resource_id: m.id.clone(),
                    display: m.display_name.clone(),
                });
            }
            // 终端标签页绑定：命令写入该终端 PTY 执行（支持 A→B→C 跳板嵌套）。
            if shared.bound_source.lock().as_str() == "terminal" {
                let instance_id = shared.bound_resource_id.lock().clone();
                if instance_id.is_empty() {
                    return Err(AppError::InvalidInput(format!(
                        "{} 未绑定终端标签页",
                        shared.kind.label()
                    )));
                }
                let display = exec::terminal_display_name(&shared.state, &instance_id);
                Ok(ResolvedTarget::Terminal {
                    instance_id,
                    display,
                })
            } else {
                if shared.bound_resource_id.lock().is_empty() {
                    return Err(AppError::InvalidInput(format!(
                        "{} 未绑定资源",
                        shared.kind.label()
                    )));
                }
                let resource_id = shared.bound_resource_id.lock().clone();
                let display = exec::session_name_by_id(&shared.state, &resource_id);
                Ok(ResolvedTarget::Bound {
                    resource_id,
                    display,
                })
            }
        }
        McpKind::Db if client_mode => {
            let host = exec::arg_host(arguments)?;
            // port 省略时按方言缺省（工具 schema 描述声明 MySQL 3306 /
            // PostgreSQL 5432）：沿用 SSH 语义的 22 会让信任描述而省略
            // port 的调用必然连接失败。
            let default_port = if exec::arg_db_kind(arguments) == "postgres" {
                5432
            } else {
                3306
            };
            let port = exec::arg_port_with_default(arguments, default_port)?;
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
            if shared.bound_resource_id.lock().is_empty() {
                return Err(AppError::InvalidInput(format!(
                    "{} 未绑定资源",
                    shared.kind.label()
                )));
            }
            let resource_id = shared.bound_resource_id.lock().clone();
            let display = exec::profile_name_by_id(&shared.state, &resource_id);
            Ok(ResolvedTarget::Bound {
                resource_id,
                display,
            })
        }
        // File MCP 仅支持 bound 模式（绑定 file_account）；client 模式拒绝。
        McpKind::File if client_mode => Err(AppError::InvalidInput(format!(
            "{} 不支持客户端直连模式，请在页面绑定 S3 文件账号",
            shared.kind.label()
        ))),
        McpKind::File => {
            if shared.bound_resource_id.lock().is_empty() {
                return Err(AppError::InvalidInput(format!(
                    "{} 未绑定资源",
                    shared.kind.label()
                )));
            }
            let resource_id = shared.bound_resource_id.lock().clone();
            let display = exec::account_name_by_id(&shared.state, &resource_id);
            Ok(ResolvedTarget::Bound {
                resource_id,
                display,
            })
        }
    }
}
/// 按已解析的目标执行工具：按 kind 路由到对应的子分派函数。
///
/// - [`run_ssh_tool`]：exec_ssh / 文件三件套（bound/terminal/multi/client 四模式）
///   + 堡垒机模式的 6 个 bastion_* 工具；
/// - [`run_db_tool`]：exec_sql（bound / client）；
/// - [`run_file_tool`]：S3 三件套（仅 bound）。
async fn run_target(
    shared: &Arc<SharedState>,
    tool_name: &str,
    target: &ResolvedTarget,
    arguments: &Value,
) -> AppResult<String> {
    match shared.kind {
        McpKind::Ssh => run_ssh_tool(shared, tool_name, target, arguments).await,
        McpKind::Db => run_db_tool(shared, tool_name, target, arguments).await,
        McpKind::File => run_file_tool(shared, tool_name, target, arguments).await,
    }
}

/// SSH kind 的工具执行分派。
///
/// 堡垒机模式下只处理 bastion_* 工具（目标统一为绑定的堡垒机会话配置，
/// 实际目标主机由参数 sessionId 在堡垒机会话表内路由）；其余模式处理
/// exec_ssh 与文件三件套（Bound=会话配置短连接 / Terminal=写入已打开终端
/// PTY / Direct=客户端直连）。
async fn run_ssh_tool(
    shared: &Arc<SharedState>,
    tool_name: &str,
    target: &ResolvedTarget,
    arguments: &Value,
) -> AppResult<String> {
    match tool_name {
        "exec_ssh" => {
            let command = exec::arg_command(arguments)?;
            // 单命令超时：工具参数 timeoutSeconds（默认 30s，上限 300s）。
            let timeout = exec::arg_timeout_seconds(arguments);
            match target {
                ResolvedTarget::Bound { resource_id, .. } => {
                    exec::exec_ssh_by_id(&shared.state, resource_id, &command, timeout).await
                }
                ResolvedTarget::Terminal { instance_id, .. } => {
                    // 终端标签页绑定：命令写入该终端 PTY 执行（支持跳板嵌套）。
                    exec::exec_ssh_terminal(&shared.state, instance_id, &command, timeout).await
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
                        timeout,
                    )
                    .await
                }
            }
        }
        "list_files" => {
            let path = exec::arg_path(arguments)?;
            match target {
                ResolvedTarget::Bound { resource_id, .. } => {
                    exec::list_files_by_id(&shared.state, resource_id, &path).await
                }
                // 终端标签页绑定只支持命令执行，文件操作走 SFTP 短连接不可用。
                ResolvedTarget::Terminal { .. } => Err(AppError::InvalidInput(
                    "终端标签页绑定模式仅支持 exec_ssh；文件操作请改绑会话配置".into(),
                )),
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
        "upload_file" => {
            let local_path = exec::arg_local_path(arguments)?;
            let remote_path = exec::arg_remote_path(arguments)?;
            match target {
                ResolvedTarget::Bound { resource_id, .. } => {
                    exec::upload_file_by_id(&shared.state, resource_id, &local_path, &remote_path)
                        .await
                }
                ResolvedTarget::Terminal { .. } => Err(AppError::InvalidInput(
                    "终端标签页绑定模式仅支持 exec_ssh；文件操作请改绑会话配置".into(),
                )),
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
        "download_file" => {
            let remote_path = exec::arg_remote_path(arguments)?;
            let local_path = exec::arg_local_path(arguments)?;
            match target {
                ResolvedTarget::Bound { resource_id, .. } => {
                    exec::download_file_by_id(&shared.state, resource_id, &remote_path, &local_path)
                        .await
                }
                ResolvedTarget::Terminal { .. } => Err(AppError::InvalidInput(
                    "终端标签页绑定模式仅支持 exec_ssh；文件操作请改绑会话配置".into(),
                )),
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
        // ---- 堡垒机模式（resolve_target 已限定目标为绑定的堡垒机会话配置） ----
        "bastion_list_hosts" => match target {
            ResolvedTarget::Bound { resource_id, .. } => {
                bastion::list_hosts(&shared.state, resource_id).await
            }
            _ => Err(AppError::InvalidInput(
                "bastion_list_hosts 仅在堡垒机模式下可用".into(),
            )),
        },
        "bastion_create_session" => {
            let host = exec::arg_host(arguments)?;
            match target {
                ResolvedTarget::Bound { resource_id, .. } => {
                    bastion::create_session(&shared.state, resource_id, &host).await
                }
                _ => Err(AppError::InvalidInput(
                    "bastion_create_session 仅在堡垒机模式下可用".into(),
                )),
            }
        }
        "bastion_session_exec" => {
            let session_id = bastion::arg_session_id(arguments)?;
            let command = exec::arg_command(arguments)?;
            let timeout = exec::arg_timeout_seconds(arguments);
            match target {
                ResolvedTarget::Bound { .. } => {
                    bastion::session_exec(&session_id, &command, &shared.state, timeout).await
                }
                _ => Err(AppError::InvalidInput(
                    "bastion_session_exec 仅在堡垒机模式下可用".into(),
                )),
            }
        }
        "bastion_close_session" => {
            let session_id = bastion::arg_session_id(arguments)?;
            match target {
                ResolvedTarget::Bound { .. } => bastion::close_session(&session_id).await,
                _ => Err(AppError::InvalidInput(
                    "bastion_close_session 仅在堡垒机模式下可用".into(),
                )),
            }
        }
        "bastion_upload_file" => {
            let session_id = bastion::arg_session_id(arguments)?;
            let local_path = exec::arg_local_path(arguments)?;
            let remote_path = exec::arg_remote_path(arguments)?;
            match target {
                ResolvedTarget::Bound { .. } => {
                    bastion::upload_file(&session_id, &local_path, &remote_path, &shared.state)
                        .await
                }
                _ => Err(AppError::InvalidInput(
                    "bastion_upload_file 仅在堡垒机模式下可用".into(),
                )),
            }
        }
        other => Err(AppError::InvalidInput(format!(
            "{} 不提供工具 `{}`",
            shared.kind.label(),
            other
        ))),
    }
}

/// DB kind 的工具执行分派（exec_sql；Bound=绑定 profile / Direct=客户端直连）。
async fn run_db_tool(
    shared: &Arc<SharedState>,
    tool_name: &str,
    target: &ResolvedTarget,
    arguments: &Value,
) -> AppResult<String> {
    match tool_name {
        "exec_sql" => {
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
                // DB MCP 不会产生 Terminal 目标（resolve_target 已限定），防御性分支。
                ResolvedTarget::Terminal { .. } => Err(AppError::InvalidInput(
                    "数据库 MCP 不支持终端标签页绑定".into(),
                )),
                ResolvedTarget::Direct {
                    host,
                    port,
                    username,
                    password,
                    ..
                } => {
                    // 客户端直连：database / dbKind 取参数（可选），未传则不指定
                    // 默认库 / 按 mysql 处理。
                    let database = exec::arg_database(arguments);
                    let db_kind = exec::arg_db_kind(arguments);
                    exec::exec_sql_direct(
                        &db_kind,
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
        other => Err(AppError::InvalidInput(format!(
            "{} 不提供工具 `{}`",
            shared.kind.label(),
            other
        ))),
    }
}

/// File kind 的工具执行分派（绑定 S3 账号，仅 bound 模式）。
async fn run_file_tool(
    shared: &Arc<SharedState>,
    tool_name: &str,
    target: &ResolvedTarget,
    arguments: &Value,
) -> AppResult<String> {
    // resolve_target 已拒绝 File 的 client/terminal 模式，这里防御性统一拦截。
    let ResolvedTarget::Bound { resource_id, .. } = target else {
        return Err(AppError::InvalidInput(format!(
            "{} 不支持终端绑定/客户端直连模式",
            shared.kind.label()
        )));
    };
    match tool_name {
        "list_files" => {
            let path = exec::arg_path(arguments)?;
            exec::list_files_by_account(&shared.state, resource_id, &path).await
        }
        "upload_file" => {
            let local_path = exec::arg_local_path(arguments)?;
            let remote_path = exec::arg_remote_path(arguments)?;
            exec::upload_file_by_account(&shared.state, resource_id, &local_path, &remote_path)
                .await
        }
        "download_file" => {
            let remote_path = exec::arg_remote_path(arguments)?;
            let local_path = exec::arg_local_path(arguments)?;
            exec::download_file_by_account(&shared.state, resource_id, &remote_path, &local_path)
                .await
        }
        other => Err(AppError::InvalidInput(format!(
            "{} 不提供工具 `{}`",
            shared.kind.label(),
            other
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

/// 只读工具执行前预先解析目标展示名（用于日志，失败时回退占位）。
fn resolve_target_display(shared: &SharedState, arguments: &Value) -> String {
    resolve_target(shared, arguments)
        .map(|t| t.display_name())
        .unwrap_or_else(|_| "(未知目标)".into())
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
