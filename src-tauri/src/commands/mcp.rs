//! MCP（Model Context Protocol）服务端相关的 Tauri 命令。
//!
//! 两个独立 MCP 实例：
//! - SSH MCP（`kind="ssh"`）：对外暴露 `exec_ssh`，绑定到一个 SSH 会话。
//! - DB MCP（`kind="db"`）：对外暴露 `exec_sql`，绑定到一个 DB profile。
//!
//! 两者各自独立启停、监听地址/端口/token/绑定资源，配置持久化在 `mcp.json`。
//!
//! 命令一览：
//! - [`mcp_start`] / [`mcp_stop`] / [`mcp_status`]：按 kind 启停 / 查询。
//! - [`mcp_save_config`] / [`mcp_load_config`]：读写 `mcp.json` 中该 kind 的配置
//!   （绑定资源、host、port、token、enabled）。
//! - [`mcp_generate_token`]：为该 kind 生成随机 token 并持久化。
//! - [`mcp_respond_approval`]：前端回 exec_ssh/exec_sql 的人工确认结果。

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::error::{AppError, AppResult};
use crate::mcp::approval::McpKind;
use crate::mcp::{start_mcp_server, stop_mcp_server, McpServerStatus};
use crate::state::AppState;

/// MCP 配置文件名（位于应用数据目录下）。
const MCP_CONFIG_FILENAME: &str = "mcp.json";

// ===========================================================================
// 配置数据结构
// ===========================================================================

/// 单个 MCP 实例的配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInstanceConfig {
    /// 是否启用（前端开关；当前启动逻辑以显式 mcp_start 为准，此字段记录意图）。
    #[serde(default)]
    pub enabled: bool,
    /// 监听地址，默认 `127.0.0.1`（仅本机）。
    ///
    /// 允许任意地址（含 0.0.0.0 / 局域网 IP），由用户自行承担暴露风险。
    #[serde(default = "default_host")]
    pub host: String,
    /// 监听端口。
    #[serde(default)]
    pub port: u16,
    /// Bearer token（未生成则为 None；启动时必须存在）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// 绑定的资源 id：SSH 会话 id / DB profile id（bound_source="config"）或
    /// 终端实例 id（bound_source="terminal"）。仅 `resource_mode == "bound"`
    /// 时必填；`"client"` 模式下忽略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    /// 资源模式："bound"（绑定本地资源，默认）| "client"（客户端直连，免绑定实例）。
    ///
    /// - bound：启动要求绑定资源（resourceId），工具参数只传 command/sql，目标从绑定解析，
    ///   凭据从本地 vault 解析。
    /// - client：无需绑定资源，工具参数需携带 host/port/username/password 等目标信息，
    ///   凭据即用即弃、不存储不落日志。适合调用方自带账密表的巡检场景。
    #[serde(default = "default_resource_mode")]
    pub resource_mode: String,
    /// 绑定来源（仅 bound 模式有效）："config"（绑定会话配置，默认；执行时新建
    /// 短连接）| "terminal"（绑定已打开的终端标签页，命令写入该终端 PTY 执行，
    /// 支持 A→B→C 跳板嵌套场景）。仅 SSH kind 支持 terminal 来源。
    #[serde(default = "default_bound_source")]
    pub bound_source: String,
    /// 绑定的具体数据库名（仅 db kind 有效）。设置后 exec_sql 只针对该库操作。
    /// 为空则使用 profile 的 default_database。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_database: Option<String>,
    /// 自动放行：开启后 exec_ssh/exec_sql 跳过人工确认直接执行。默认 false。
    #[serde(default)]
    pub auto_approve: bool,
    /// 是否记录执行日志到文本文件（每次启动生成一个日志文件）。默认 true。
    #[serde(default = "default_enable_log")]
    pub enable_log: bool,
}

/// 默认监听地址：仅本机回环。
///
/// 仅为默认值；用户可改为 0.0.0.0 或局域网 IP 以对局域网/外部开放，
/// 风险由用户自行承担。
fn default_host() -> String {
    "127.0.0.1".into()
}

fn default_enable_log() -> bool {
    true
}

/// 默认资源模式：绑定本地资源（向后兼容；老 mcp.json 无此字段即视为 bound）。
fn default_resource_mode() -> String {
    "bound".into()
}

/// 默认绑定来源：会话配置（向后兼容；老 mcp.json 无此字段即视为 config）。
fn default_bound_source() -> String {
    "config".into()
}

/// 规范化资源模式：仅 `"client"` 视为直连模式，其余一律按 `"bound"` 处理
/// （防配置文件被手动改坏导致意外直连）。
pub(crate) fn normalize_resource_mode(m: &str) -> String {
    if m == "client" {
        "client".into()
    } else {
        "bound".into()
    }
}

/// 规范化绑定来源：仅 `"terminal"` 视为终端标签页绑定，其余一律按 `"config"` 处理。
pub(crate) fn normalize_bound_source(s: &str) -> String {
    if s == "terminal" {
        "terminal".into()
    } else {
        "config".into()
    }
}

/// kind 的默认端口。
fn default_port_for(kind: McpKind) -> u16 {
    match kind {
        McpKind::Ssh => 8765,
        McpKind::Db => 8766,
        McpKind::File => 8767,
    }
}

impl McpInstanceConfig {
    /// 该 kind 的默认配置。
    pub fn default_for(kind: McpKind) -> Self {
        Self {
            enabled: false,
            host: default_host(),
            port: default_port_for(kind),
            token: None,
            resource_id: None,
            resource_mode: default_resource_mode(),
            bound_source: default_bound_source(),
            bound_database: None,
            auto_approve: false,
            enable_log: true,
        }
    }
}

/// `mcp.json` 根结构：三个 kind 各一份配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpConfigFile {
    #[serde(default)]
    ssh: McpInstanceConfig,
    #[serde(default)]
    db: McpInstanceConfig,
    #[serde(default)]
    file: McpInstanceConfig,
}

// ===========================================================================
// 配置读写
// ===========================================================================

fn config_path(state: &AppState) -> std::path::PathBuf {
    state.settings_path.as_path().join(MCP_CONFIG_FILENAME)
}

/// 读取 `mcp.json`；文件不存在或解析失败返回默认双配置。
///
/// 解析失败（损坏）时由 json_store 把损坏文件改名留证（`<path>.corrupt-*`）后
/// 返回默认配置——绝不静默覆盖，token/端口等配置事后可从留证文件恢复。
fn read_config_file(state: &AppState) -> McpConfigFile {
    let path = config_path(state);
    crate::storage::json_store::read_json_or_default(&path).unwrap_or_default()
}

/// 写 `mcp.json`（原子写：复用 json_store 的「先写唯一 .tmp 再 rename」两阶段
/// 替换流程，任意时刻崩溃目标文件要么是旧内容要么是完整新内容——裸
/// `std::fs::write` 会留下半个文件，损坏后下次启动静默回默认配置、已配置的
/// token 全部丢失）。
fn write_config_file(state: &AppState, cfg: &McpConfigFile) -> AppResult<()> {
    let path = config_path(state);
    crate::storage::json_store::write_json(&path, cfg)
}

/// 从配置文件取指定 kind 的配置（保证字段非空：host/port 用默认兜底）。
fn instance_config(state: &AppState, kind: McpKind) -> McpInstanceConfig {
    let file = read_config_file(state);
    let mut c = match kind {
        McpKind::Ssh => file.ssh,
        McpKind::Db => file.db,
        McpKind::File => file.file,
    };
    // 兜底：host 空则默认，port 为 0 则默认。
    if c.host.trim().is_empty() {
        c.host = default_host();
    }
    if c.port == 0 {
        c.port = default_port_for(kind);
    }
    c
}

/// 把指定 kind 的配置写回文件（其余 kind 保持不变）。
fn set_instance_config(state: &AppState, kind: McpKind, c: McpInstanceConfig) -> AppResult<()> {
    let mut file = read_config_file(state);
    match kind {
        McpKind::Ssh => file.ssh = c,
        McpKind::Db => file.db = c,
        McpKind::File => file.file = c,
    }
    write_config_file(state, &file)
}

// ===========================================================================
// 命令
// ===========================================================================

/// 启动指定 kind 的 MCP 服务端。
///
/// - `host` / `port` 可选，省略时用配置文件中的值（默认 127.0.0.1 / 8765|8766）。
/// - host 允许任意监听地址（0.0.0.0 / 局域网 IP 亦可），暴露风险由用户自行承担。
/// - token 必须已生成（配置文件中存在），否则返回 Config 错误。
/// - 资源校验按 `resource_mode` 分支：
///   - bound（默认）：必须已绑定资源（resourceId 非空），否则返回错误；
///   - client：无需绑定资源，目标与凭据由调用方在工具参数中传入。
#[tauri::command]
pub async fn mcp_start(
    kind: String,
    host: Option<String>,
    port: Option<u16>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<McpServerStatus> {
    let kind = McpKind::parse(&kind);
    let cfg = instance_config(state.inner(), kind);
    let resource_mode = normalize_resource_mode(&cfg.resource_mode);

    let host = host.unwrap_or(cfg.host);
    let port = port.unwrap_or(cfg.port);
    let token = cfg.token.ok_or_else(|| {
        AppError::Config(format!(
            "未配置 {} 的 token：请先在 MCP 页面生成 token",
            kind.label()
        ))
    })?;
    // bound 模式要求绑定资源；client 模式（客户端直连）不要求。
    let bound_source = normalize_bound_source(&cfg.bound_source);
    // 终端标签页绑定仅 SSH kind 支持；其它 kind 强制回退会话配置绑定。
    let bound_source = match (bound_source.as_str(), kind) {
        ("terminal", McpKind::Ssh) => "terminal",
        ("terminal", _) => {
            log::warn!(
                "[mcp] {} 不支持终端标签页绑定，已回退为会话配置绑定",
                kind.label()
            );
            "config"
        }
        _ => "config",
    };
    let bound_resource_id = if resource_mode == "client" {
        None
    } else if bound_source == "terminal" {
        // 终端标签页绑定：校验终端存在且为 SSH 终端（启动时校验，避免每次调用都报错）。
        let id = cfg.resource_id.ok_or_else(|| {
            AppError::Config(
                "SSH MCP 未绑定终端标签页：请先在 MCP 页面选择一个已打开的终端标签页".into(),
            )
        })?;
        if !crate::mcp::exec::ssh_terminal_exists(state.inner(), &id) {
            return Err(AppError::Config(format!(
                "绑定的终端标签页不存在或已断开（终端可能已关闭），请重新绑定（当前 id: {}）",
                id
            )));
        }
        Some(id)
    } else {
        Some(cfg.resource_id.ok_or_else(|| {
            AppError::Config(format!(
                "{} 未绑定资源：请先在 MCP 页面选择一个{}，或开启「客户端直连模式」",
                kind.label(),
                match kind {
                    McpKind::Ssh => "SSH 会话",
                    McpKind::Db => "数据库连接",
                    McpKind::File => "S3 文件账号",
                }
            ))
        })?)
    };

    start_mcp_server(
        kind,
        app,
        state.inner().clone(),
        host,
        port,
        token,
        bound_resource_id,
        bound_source.to_string(),
        cfg.bound_database,
        resource_mode,
        cfg.auto_approve,
        cfg.enable_log,
    )
    .await?;
    Ok(crate::mcp::mcp_server_status(kind))
}

/// 停止指定 kind 的 MCP 服务端。
#[tauri::command]
pub fn mcp_stop(kind: String) -> AppResult<()> {
    stop_mcp_server(McpKind::parse(&kind))
}

/// 查询指定 kind 的 MCP 服务端状态。
#[tauri::command]
pub fn mcp_status(kind: String) -> AppResult<McpServerStatus> {
    Ok(crate::mcp::mcp_server_status(McpKind::parse(&kind)))
}

/// 保存指定 kind 的配置（绑定资源 / host / port / enabled / token / auto_approve）。
///
/// 前端在用户改了绑定、地址、端口、开关后调用。**不直接重启服务**——若服务在运行，
/// 前端应先 mcp_stop 再 mcp_start 生效（本命令只持久化配置）。
/// 例外：`auto_approve` 改动**立即生效**（更新运行时开关，无需重启）。
#[tauri::command]
pub fn mcp_save_config(
    kind: String,
    config: McpInstanceConfig,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let kind = McpKind::parse(&kind);
    // host 不做地址限制：允许 0.0.0.0 / 局域网 IP 等任意监听地址（空值允许——
    // 启动时回退默认 127.0.0.1）。
    // auto_approve 立即生效（无需重启服务）。
    crate::mcp::server::set_auto_approve(kind, config.auto_approve);
    set_instance_config(state.inner(), kind, config)
}

/// 读取指定 kind 的配置。
#[tauri::command]
pub fn mcp_load_config(kind: String, state: State<'_, AppState>) -> AppResult<McpInstanceConfig> {
    Ok(instance_config(state.inner(), McpKind::parse(&kind)))
}

/// 运行中热切换绑定的资源（会话配置 / 终端标签页），立即生效无需重启。
///
/// - `bound_source`："config"（会话配置）| "terminal"（终端标签页，仅 SSH kind）。
/// - `resource_id`：会话配置 id 或终端实例 id。
///
/// 同时把新绑定持久化到 mcp.json（与 `mcp_save_config` 各自独立，二者都会落盘）。
/// 服务未运行时只保存配置（启动时生效）。
#[tauri::command]
pub async fn mcp_rebind(
    kind: String,
    bound_source: String,
    resource_id: String,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let kind = McpKind::parse(&kind);
    let bound_source = normalize_bound_source(&bound_source);

    // 终端标签页绑定仅 SSH kind 支持。
    if bound_source == "terminal" && kind != McpKind::Ssh {
        return Err(AppError::InvalidInput(format!(
            "{} 不支持终端标签页绑定，请绑定{}",
            kind.label(),
            match kind {
                McpKind::Ssh => "SSH 会话",
                McpKind::Db => "数据库连接",
                McpKind::File => "S3 文件账号",
            }
        )));
    }
    // 终端绑定要求终端当前存在且为 SSH 终端。
    if bound_source == "terminal" && !crate::mcp::exec::ssh_terminal_exists(state.inner(), &resource_id)
    {
        return Err(AppError::NotFound(format!(
            "终端标签页不存在或已断开（终端可能已关闭），请重新选择"
        )));
    }

    // 热切换运行中的实例（未运行时仅提示，不视为错误——配置已保存，启动时生效）。
    match crate::mcp::server::rebind_mcp(kind, &bound_source, &resource_id) {
        Ok(()) => {}
        Err(e) => log::info!("[mcp] {} 热切换跳过：{}", kind.label(), e),
    }

    // 持久化配置。
    let mut cfg = instance_config(state.inner(), kind);
    cfg.bound_source = bound_source;
    cfg.resource_id = Some(resource_id);
    set_instance_config(state.inner(), kind, cfg)
}

/// 为指定 kind 生成随机 token（uuid 去横线），写入配置文件并返回。
#[tauri::command]
pub fn mcp_generate_token(kind: String, state: State<'_, AppState>) -> AppResult<String> {
    let kind = McpKind::parse(&kind);
    let mut cfg = instance_config(state.inner(), kind);
    let token = uuid::Uuid::new_v4().simple().to_string();
    cfg.token = Some(token.clone());
    set_instance_config(state.inner(), kind, cfg)?;
    Ok(token)
}

/// 前端回确认结果（exec_ssh/exec_sql 的人工确认）。
///
/// `approved=true` 允许执行；`false` 拒绝。返回是否命中 pending 请求。
#[tauri::command]
pub async fn mcp_respond_approval(
    request_id: String,
    approved: bool,
    state: State<'_, AppState>,
) -> AppResult<bool> {
    let hit = state.approval_registry.respond(&request_id, approved).await;
    Ok(hit)
}

// ===========================================================================
// 执行日志
// ===========================================================================

/// 日志内容返回结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpLogContent {
    /// 日志文件名（无日志时为空）。
    pub filename: String,
    /// 日志内容（最近 `max_lines` 行）。
    pub content: String,
    /// 日志文件是否存在。
    pub exists: bool,
}

/// 读取指定 kind 的**最新**日志文件的尾部内容（前端日志面板轮询用）。
///
/// 在 `mcp-logs/` 目录下按文件名（含时间戳）找到该 kind 最新的 `mcp-<kind>-*.log`，
/// 返回最近 `max_lines` 行（默认 500）。服务未运行 / 无日志时 `exists=false`。
#[tauri::command]
pub fn mcp_log(
    kind: String,
    max_lines: Option<usize>,
    state: State<'_, AppState>,
) -> AppResult<McpLogContent> {
    let kind = McpKind::parse(&kind);
    let kind_str = match kind {
        McpKind::Ssh => "ssh",
        McpKind::Db => "db",
        McpKind::File => "file",
    };
    let log_dir = state.data_dir.join("mcp-logs");
    let prefix = format!("mcp-{}-", kind_str);

    // 收集该 kind 的日志文件，按文件名排序（时间戳定宽，字典序即时间序）。
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&log_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix) && n.ends_with(".log"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();

    match files.pop() {
        Some(path) => {
            let full = std::fs::read_to_string(&path).unwrap_or_default();
            let max = max_lines.unwrap_or(500);
            let lines: Vec<&str> = full.lines().collect();
            let tail = if lines.len() > max {
                lines[lines.len() - max..].join("\n")
            } else {
                full
            };
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            Ok(McpLogContent {
                filename,
                content: tail,
                exists: true,
            })
        }
        None => Ok(McpLogContent {
            filename: String::new(),
            content: String::new(),
            exists: false,
        }),
    }
}
