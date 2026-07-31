//! AI 工具调用（Tool Calling）能力。
//!
//! 本模块定义了 X-Term 向 LLM 暴露的工具集合，以及工具执行器与安全护栏。
//!
//! # 概念
//!
//! - [`ToolDef`]：工具定义，发给模型的 JSON Schema 描述。
//! - [`ToolCall`]：模型返回的工具调用请求（id + name + 已解析的参数）。
//! - [`ToolResult`]：工具执行结果，回填给模型。
//! - [`ToolApproval`]：用户对工具调用的确认/拒绝（前端通过命令发回）。
//!
//! # 工具集
//!
//! - `exec_ssh`：在指定 SSH 会话对应的服务器上执行命令（非交互 `channel.exec`）。
//! - `terminal_snapshot`：取指定终端最近输出。
//! - `exec_sql`：在指定 MySQL 连接上执行 SQL。
//! - `list_db_tables`：列出当前数据库的表。
//! - `describe_table`：描述表结构。
//!
//! # 执行流程
//!
//! 1. 调用方（`commands::ai::run_agent_loop`）把 [`all_tools`] 发给模型。
//! 2. 模型返回 `Vec<ToolCall>`，调用方依次：
//!    - emit `ai:tool_call` 事件（含 [`is_dangerous`] 标记与 [`describe_call`] 描述）；
//!    - 通过 `oneshot` 阻塞等待前端确认；
//!    - 调用 [`execute_tool`] 执行，emit `ai:tool_result`；
//!    - 把结果以 role="tool" 消息回填给模型。
//!
//! # 安全
//!
//! [`is_dangerous`] 是一道静态护栏，对若干"灾难性"模式（rm -rf /、mkfs、fork bomb、
//! DROP/TRUNCATE、无 WHERE 的 DELETE 等）返回 true，前端据此红色高亮 + 二次确认。
//! 这并非沙箱：执行端不做拦截，仍按用户最终决定执行。

use std::collections::HashSet;
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::AppHandle;
use tokio::time::timeout;

use crate::error::{AppError, AppResult};
use crate::utils::{format_query_result, strip_ansi};
use crate::state::AppState;

// ===========================================================================
// 类型定义
// ===========================================================================

/// 一个工具的定义（发给模型的 JSON Schema 描述）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema 描述参数。
    pub parameters: Value,
}

/// 模型返回的工具调用请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// 参数（已解析的 JSON 对象）。
    pub arguments: Value,
}

/// 工具执行结果（回填给模型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub ok: bool,
    pub output: String,
}

impl ToolResult {
    pub fn ok<S: Into<String>>(output: S) -> Self {
        Self {
            ok: true,
            output: output.into(),
        }
    }

    pub fn err<S: Into<String>>(output: S) -> Self {
        Self {
            ok: false,
            output: output.into(),
        }
    }
}

/// 用户对工具调用的确认/拒绝（前端通过命令发回）。
pub struct ToolApproval {
    pub approved: bool,
}

// ===========================================================================
// 常量
// ===========================================================================

/// 单命令执行超时（30 秒）。
const EXEC_TIMEOUT: Duration = Duration::from_secs(30);
/// exec_ssh（独立连接模式）输出截断上限（16 KiB）。
const EXEC_OUTPUT_CAP: usize = 16 * 1024;
/// exec_ssh（终端可视化模式）返回给 AI 的输出截断上限（16 KiB）。
///
/// 与 [`EXEC_OUTPUT_CAP`] 保持一致：两种执行模式对回填给模型的输出大小限制相同。
const MAX_EXEC_OUTPUT_BYTES: usize = 16 * 1024;
/// terminal_snapshot 默认/上限字节数（8 KiB）。
const SNAPSHOT_DEFAULT_BYTES: usize = 8 * 1024;

// ===========================================================================
// 工具集
// ===========================================================================

/// 返回全部工具定义。
///
/// 工具参数用 `serde_json::json!` 构造 JSON Schema；具体厂商实现（OpenAI / Claude）
/// 会按各自协议封装（OpenAI 包成 `function.parameters`，Claude 用 `input_schema`）。
/// SSH 上下文工具集（有活动终端时启用）。
///
/// - `exec_ssh`：在服务器执行 shell 命令。
/// - `terminal_snapshot`：读取终端最近输出。
pub fn ssh_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "exec_ssh".into(),
            description: "在指定的 SSH 终端会话对应的服务器上执行一条 shell 命令，\
返回标准输出和标准错误的合并文本。适用于查询系统状态（如 ps、df、netstat、\
cat 配置文件等）。单命令超时 30 秒，输出截断 16KB。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "sessionId": {
                        "type": "string",
                        "description": "目标 SSH 终端会话的实例 id"
                    },
                    "command": {
                        "type": "string",
                        "description": "要执行的 shell 命令（单条，非交互）"
                    }
                },
                "required": ["sessionId", "command"]
            }),
        },
        ToolDef {
            name: "terminal_snapshot".into(),
            description: "获取指定 SSH 终端会话最近的屏幕输出（最近 8KB），\
用于了解用户当前看到了什么、上下文是什么。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string" },
                    "maxBytes": {
                        "type": "integer",
                        "description": "最多返回的字节数，可省略（默认 8192）",
                        "minimum": 1
                    }
                },
                "required": ["sessionId"]
            }),
        },
    ]
}

/// MySQL 上下文工具集（有活动数据库连接时启用）。
///
/// - `exec_sql`：执行 SQL（默认只读，写操作需额外确认）。
/// - `list_db_tables`：列出当前库的表。
/// - `describe_table`：查看表结构。
pub fn sql_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "exec_sql".into(),
            description: "在指定的 MySQL 连接上执行 SQL 语句。默认只读\
（SELECT/SHOW/EXPLAIN/DESCRIBE）；写操作（INSERT/UPDATE/DELETE/DDL）需要用户在确认时\
额外批准。返回列名和行（最多 100 行）。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "dbConnId": { "type": "string" },
                    "sql": { "type": "string" },
                    "limit": {
                        "type": "integer",
                        "description": "返回行数上限，默认 100",
                        "minimum": 1
                    }
                },
                "required": ["dbConnId", "sql"]
            }),
        },
        ToolDef {
            name: "list_db_tables".into(),
            description: "列出指定 MySQL 连接当前数据库的所有表名。".into(),
            parameters: json!({
                "type": "object",
                "properties": { "dbConnId": { "type": "string" } },
                "required": ["dbConnId"]
            }),
        },
        ToolDef {
            name: "describe_table".into(),
            description: "返回指定表的列结构（字段名、类型、是否可空、键、默认值、注释）。\
table 可用 `database.table` 限定名（推荐，尤其当连接未指定默认库时），或仅 `table`（取当前默认库）。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "dbConnId": { "type": "string" },
                    "table": {
                        "type": "string",
                        "description": "表名，可用 `database.table` 限定（如 apidoc.api_keys）或仅 table"
                    }
                },
                "required": ["dbConnId", "table"]
            }),
        },
    ]
}

/// 返回全部工具定义（SSH + SQL）。保留用于测试/兼容；运行时按上下文裁剪请用
/// [`tools_for_context`]。
pub fn all_tools() -> Vec<ToolDef> {
    let mut v = ssh_tools();
    v.extend(sql_tools());
    v
}

/// 按当前活动上下文裁剪工具集（块A 核心逻辑）。
///
/// - 提供活动终端 → 启用 SSH 工具；
/// - 提供活动 MySQL 连接 → 启用 SQL 工具；
/// - 两者皆无 → 返回空（agent 模式下模型只能纯文本对话，前端系统提示会告知
///   "未检测到可用上下文"）。
///
/// `active_terminal_id` / `active_db_conn_id` 只要**非空字符串**即视为有该上下文
/// （具体值是否有效由执行期 `execute_tool` 自然校验——找不到对应实例会返回错误）。
pub fn tools_for_context(
    active_terminal_id: Option<&str>,
    active_db_conn_id: Option<&str>,
) -> Vec<ToolDef> {
    let mut tools: Vec<ToolDef> = Vec::new();
    if active_terminal_id.map(|s| !s.is_empty()).unwrap_or(false) {
        tools.extend(ssh_tools());
    }
    if active_db_conn_id.map(|s| !s.is_empty()).unwrap_or(false) {
        tools.extend(sql_tools());
    }
    tools
}

/// 从一组工具定义中提取名称集合，用于执行期校验模型是否幻觉调用了未 advertised 的工具。
pub fn allowed_tool_names(tools: &[ToolDef]) -> HashSet<String> {
    tools.iter().map(|t| t.name.clone()).collect()
}

// ===========================================================================
// 工具执行器
// ===========================================================================

/// 工具执行器入口。按 `call.name` 分派到具体实现。
///
/// `allowed` 是本轮 agent loop 实际下发给模型的工具名集合（上下文裁剪后）。
/// 若 `call.name` 不在其中，说明模型幻觉调用了未 advertised 的工具，直接拒绝——
/// 防止"只给了 SSH 工具，模型却调 exec_sql"这类越权。
///
/// 任何执行错误都被吞掉并返回 `ToolResult { ok: false, output: <错误信息> }`，
/// 由调用方把错误回填给模型，让模型据此重试或解释给用户。
pub async fn execute_tool(
    app: &AppHandle,
    state: &AppState,
    call: &ToolCall,
    allowed: &HashSet<String>,
    visualization: bool,
) -> ToolResult {
    if !allowed.contains(&call.name) {
        return ToolResult::err(format!(
            "工具 `{}` 在当前上下文不可用（未提供活动终端或数据库连接）",
            call.name
        ));
    }
    match call.name.as_str() {
        "exec_ssh" => exec_ssh(app, state, &call.arguments, visualization).await,
        "terminal_snapshot" => terminal_snapshot(state, &call.arguments),
        "exec_sql" => exec_sql(app, state, &call.arguments, visualization).await,
        "list_db_tables" => list_db_tables(state, &call.arguments).await,
        "describe_table" => describe_table(state, &call.arguments).await,
        other => ToolResult::err(format!("未知工具: {other}")),
    }
}

/// exec_ssh：在指定 SSH 会话对应服务器上执行命令。
///
/// **两种执行模式**：
/// - **终端可视化**（`visualization = true`）：把命令写入用户活动终端的 PTY
///   （`SshSession::write`），命令和输出实时显示在用户的 xterm 里。返回"已写入终端"
///   确认；模型若需读取结果可继续调 `terminal_snapshot`。这是最贴近"AI 在终端里操作"
///   的体验。
/// - **独立连接**（`visualization = false`，默认）：不复用终端会话已有的 `Handle`
///   （russh 0.45 的 `Handle` 未实现 `Clone`），而是基于该会话的 `session_config_id`
///   新建一条独立 SSH 连接，用 `channel.exec` 执行命令，读完输出后断开。输出干净
///   （无 PTY 转义污染）、隔离性好、规避所有权问题，但用户在终端里看不到。
async fn exec_ssh(
    app: &AppHandle,
    state: &AppState,
    args: &Value,
    visualization: bool,
) -> ToolResult {
    // 1. 解析参数。
    let (session_id, command) = match (
        args.get("sessionId").and_then(Value::as_str),
        args.get("command").and_then(Value::as_str),
    ) {
        (Some(s), Some(c)) => (s.to_string(), c.to_string()),
        _ => return ToolResult::err("exec_ssh 缺少 sessionId 或 command 参数"),
    };

    if command.trim().is_empty() {
        return ToolResult::err("command 不能为空");
    }

    log::info!(
        "[agent] exec_ssh 开始：会话 {session_id}，模式 {}，命令：{command}",
        if visualization { "可视化(写PTY)" } else { "独立连接" }
    );

    // 1.5 终端可视化模式：把命令写入用户活动终端的 PTY（命令实时显示在 xterm），
    //     并通过"哨兵 echo"检测命令执行完成，截取执行期间的新增输出返回给 AI。
    //     这样终端可视化（看到 AI 敲命令）与 AI 拿到真实结果两者兼得。
    if visualization {
        return exec_ssh_visual(state, &session_id, &command).await;
    }

    // 2. 从 terminals 取 session_config_id。
    let session_config_id = {
        let terminals = state.terminals.lock();
        terminals
            .get(&session_id)
            .map(|s| s.session_config_id().to_string())
    };
    let session_config_id = match session_config_id {
        Some(id) => id,
        None => {
            return ToolResult::err(format!("找不到终端会话 {session_id}"));
        }
    };

    // 3. 加载会话配置 + 解析凭据（同步操作；多次获取短生命 DB 连接）。
    let setup: AppResult<(
        crate::storage::sessions_repo::Session,
        crate::ssh::session::ResolvedCredential,
    )> = (|| {
        let session_config = {
            let conn = state.conn()?;
            crate::storage::sessions_repo::get_session(&conn, &session_config_id)?
                .ok_or_else(|| {
                    AppError::NotFound(format!("会话配置 {session_config_id} 不存在"))
                })?
        };
        let vault = {
            let guard = state.vault_read()?;
            guard
                .as_ref()
                .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
                .clone()
        };
        let conn = state.conn()?;
        let resolved = crate::ssh::session::resolve_credential(
            &session_config,
            &vault,
            &conn,
        )?;
        Ok((session_config, resolved))
    })();
    let (session_config, resolved) = match setup {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("解析 SSH 凭据失败: {e}")),
    };

    // 4. 新建连接 + exec（整体 30s 超时）。
    let app_clone = app.clone();
    let run = async {
        // 连接（这里复用 SshSession::open 用的 connect_direct）。
        let handle = crate::ssh::client::connect_direct(
            &session_config.host,
            session_config.port,
            &session_config.username,
            resolved.auth_method,
            app_clone,
        )
        .await?;

        // 打开 session channel。
        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|e| AppError::Ssh(format!("打开 channel 失败: {e}")))?;

        // 请求 exec（want_reply=false）。
        channel
            .exec(false, command.as_str())
            .await
            .map_err(|e| AppError::Ssh(format!("exec 失败: {e}")))?;

        // 循环 channel.wait() 收集 Data / ExtendedData。
        let mut raw: Vec<u8> = Vec::new();
        let mut exit_code: Option<u32> = None;
        use russh::ChannelMsg;
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { ref data }) => {
                    raw.extend_from_slice(data.as_ref());
                }
                Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                    raw.extend_from_slice(data.as_ref());
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exit_code = Some(exit_status);
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            }
            // 软截断：超出上限就停。
            if raw.len() >= EXEC_OUTPUT_CAP {
                raw.truncate(EXEC_OUTPUT_CAP);
                break;
            }
        }

        // 断开（忽略错误）。
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "bye", "en")
            .await;

        Ok::<_, AppError>((raw, exit_code))
    };

    match timeout(EXEC_TIMEOUT, run).await {
        Ok(Ok((raw, exit_code))) => {
            let text = strip_ansi(&String::from_utf8_lossy(&raw));
            let truncated = if text.len() > EXEC_OUTPUT_CAP {
                let mut s: String = text.chars().take(EXEC_OUTPUT_CAP).collect();
                s.push_str("\n... [输出已截断]");
                s
            } else {
                text
            };
            let code_suffix = match exit_code {
                Some(0) | None => String::new(),
                Some(c) => format!("\n[exit: {c}]"),
            };
            ToolResult::ok(format!("{truncated}{code_suffix}"))
        }
        Ok(Err(e)) => ToolResult::err(format!("exec_ssh 失败: {e}")),
        Err(_) => ToolResult::err("exec_ssh 执行超时（30s）"),
    }
}

/// 可视化模式执行 SSH 命令：写入活动终端 PTY + 哨兵检测完成 + 截取新输出。
///
/// 流程：
/// 1. 生成唯一哨兵标记，把命令包装为 `<cmd>; echo <SENTINEL>` 写入 PTY
///    （终端里用户能看到 AI 实际敲的命令和输出）。
/// 2. 轮询终端输出环形缓冲，直到出现哨兵（命令执行完毕）或超时。
/// 3. 截取"哨兵行之前、命令回显之后"的新增输出，去 ANSI 后返回给 AI。
///
/// 这样既保留了"终端可视化"（命令在 xterm 实时显示），又让 AI 拿到真实执行结果。
async fn exec_ssh_visual(state: &AppState, session_id: &str, command: &str) -> ToolResult {
    use rand::Rng;

    // 生成唯一哨兵（避免与正常输出撞车）。
    let nonce: u64 = rand::thread_rng().gen();
    let sentinel = format!("__XTERM_DONE_{nonce:x}__");

    // 记录写入前的输出基准长度，用于截取新增部分。
    let baseline = {
        let terminals = state.terminals.lock();
        match terminals.get(session_id) {
            Some(ssh) => ssh.output_offset(),
            None => return ToolResult::err(format!("终端会话 {session_id} 不存在")),
        }
    };

    // 构造实际执行的命令：原命令 + 哨兵 echo。
    // 用 `;` 连接（无论原命令成功与否哨兵都会输出），保证能检测到完成。
    // 注意：原命令末尾的换行已去掉。
    let cmd = command.trim_end_matches(['\n', '\r']);
    let wrapped = format!("{cmd}; echo {sentinel}\n");

    // 写入 PTY（mpsc send，非阻塞）。
    {
        let terminals = state.terminals.lock();
        match terminals.get(session_id) {
            Some(ssh) => {
                if let Err(e) = ssh.write(wrapped.into_bytes()) {
                    return ToolResult::err(format!("写入终端失败: {e}"));
                }
            }
            None => return ToolResult::err(format!("终端会话 {session_id} 不存在")),
        }
    }

    // 轮询等待哨兵出现（最长 30 秒）。
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    #[allow(unused_assignments)]
    let mut snapshot = String::new();
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        snapshot = {
            let terminals = state.terminals.lock();
            match terminals.get(session_id) {
                Some(ssh) => ssh.snapshot(0),
                None => return ToolResult::err("终端会话已断开"),
            }
        };
        if snapshot.contains(&sentinel) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            // 超时：返回目前已收集到的输出（可能命令还在跑或卡住等输入）。
            let partial = extract_new_output(&snapshot, baseline, "");
            let cleaned = strip_ansi(&partial);
            return ToolResult::ok(format!(
                "命令已写入终端执行，但 30 秒内未检测到完成（可能仍在运行或等待输入）。\n\
                 目前输出：\n{}",
                truncate_output(&cleaned, MAX_EXEC_OUTPUT_BYTES)
            ));
        }
    }

    // 截取哨兵之前的新增输出（去掉命令回显、哨兵本身）。
    let new_output = extract_new_output(&snapshot, baseline, &sentinel);
    let cleaned = strip_ansi(&new_output);
    let result = truncate_output(&cleaned, MAX_EXEC_OUTPUT_BYTES);
    ToolResult::ok(result)
}

/// 从 snapshot 中提取"命令执行期间的新增输出"。
///
/// - `baseline`：命令写入前缓冲的字节数；snapshot 是定长环形缓冲，可能已环绕，
///   这里用"哨兵位置"做主要锚点：取从命令回显结束到哨兵之间的内容。
/// - `sentinel`：哨兵字符串（超时时为空）。
fn extract_new_output(snapshot: &str, _baseline: usize, sentinel: &str) -> String {
    // 策略：找到哨兵所在行，取该行之前的所有内容；再尝试去掉第一行（命令回显）。
    let end = if sentinel.is_empty() {
        snapshot.len()
    } else {
        snapshot.find(sentinel).unwrap_or(snapshot.len())
    };
    let before_sentinel = &snapshot[..end];
    // 去掉末尾的空行/哨兵前导。
    let trimmed = before_sentinel.trim_end_matches(['\n', '\r']);
    // 命令回显通常是第一行（用户在 xterm 看到的 `<cmd>; echo SENTINEL`）。
    // 去掉第一行以减少噪音。
    if let Some(nl) = trimmed.find('\n') {
        trimmed[nl + 1..].to_string()
    } else {
        trimmed.to_string()
    }
}

/// 截断输出到指定字节数，超出则尾部提示。
fn truncate_output(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let cut = s.char_indices().take_while(|(i, _)| *i <= max_bytes).last().map(|(i, _)| i).unwrap_or(max_bytes);
    format!("{}…\n(输出已截断，共 {} 字节)", &s[..cut], s.len())
}

/// terminal_snapshot：取指定终端最近输出。
fn terminal_snapshot(state: &AppState, args: &Value) -> ToolResult {
    let session_id = match args.get("sessionId").and_then(Value::as_str) {
        Some(s) => s,
        None => return ToolResult::err("terminal_snapshot 缺少 sessionId"),
    };
    let max_bytes = args
        .get("maxBytes")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(SNAPSHOT_DEFAULT_BYTES);

    let terminals = state.terminals.lock();
    match terminals.get(session_id) {
        Some(s) => {
            let snap = s.snapshot(max_bytes.min(SNAPSHOT_DEFAULT_BYTES));
            ToolResult::ok(snap)
        }
        None => ToolResult::err(format!("找不到终端会话 {session_id}")),
    }
}

/// exec_sql：在指定 MySQL 连接上执行 SQL。
///
/// 写操作（INSERT/UPDATE/DELETE/DDL）由调用方在确认阶段把关（前端弹二次确认）；
/// 此函数本身只在用户已批准后才会被调用，故直接执行。
///
/// `visualization` 为 true（SQL 终端可视化开启）时，执行后额外 emit
/// `ai:sql_result` 事件，携带结构化结果（columns/rows/affected/elapsed/error），
/// 前端 SQL 控制台据此把 SQL 与结果回显进输出流（命令行模式）。
async fn exec_sql(app: &AppHandle, state: &AppState, args: &Value, visualization: bool) -> ToolResult {
    let (conn_id, sql) = match (
        args.get("dbConnId").and_then(Value::as_str),
        args.get("sql").and_then(Value::as_str),
    ) {
        (Some(c), Some(s)) => (c.to_string(), s.to_string()),
        _ => return ToolResult::err("exec_sql 缺少 dbConnId 或 sql"),
    };
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(100);

    log::info!(
        "[agent] exec_sql 开始：连接 {conn_id}，可视化 {}，SQL：{sql}",
        if visualization { "是" } else { "否" }
    );

    // 取出 conn → 执行 → 放回（MySqlConn 不 Clone）。
    let conn = {
        let mut map = state.mysql_conns.lock();
        match map.remove(&conn_id) {
            Some(c) => c,
            None => {
                return ToolResult::err(format!("找不到 MySQL 连接 {conn_id}"));
            }
        }
    };

    let started = std::time::Instant::now();
    let res = conn.execute(&sql, limit).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    // 放回。
    state.mysql_conns.lock().insert(conn_id, conn);

    // SQL 终端可视化：把结构化结果回显给 SQL 控制台（命令行模式）。
    if visualization {
        let (columns, rows, affected, error) = match &res {
            Ok(qr) => (qr.columns.clone(), qr.rows.clone(), qr.affected, None),
            Err(e) => (Vec::new(), Vec::new(), 0u64, Some(format!("SQL 执行失败: {e}"))),
        };
        crate::events::emit(
            app,
            crate::events::AI_SQL_RESULT,
            crate::events::AiSqlResultEvent {
                request_id: String::new(),
                sql: sql.clone(),
                columns,
                rows,
                affected,
                elapsed_ms,
                error,
            },
        );
    }

    match res {
        Ok(qr) => ToolResult::ok(format_query_result(&qr)),
        Err(e) => ToolResult::err(format!("SQL 执行失败: {e}")),
    }
}

/// list_db_tables：执行 `SHOW TABLES`，返回表名列表。
async fn list_db_tables(state: &AppState, args: &Value) -> ToolResult {
    let conn_id = match args.get("dbConnId").and_then(Value::as_str) {
        Some(c) => c.to_string(),
        None => return ToolResult::err("list_db_tables 缺少 dbConnId"),
    };
    let conn = {
        let mut map = state.mysql_conns.lock();
        match map.remove(&conn_id) {
            Some(c) => c,
            None => return ToolResult::err(format!("找不到 MySQL 连接 {conn_id}")),
        }
    };
    let res = conn.execute("SHOW TABLES", 10_000).await;
    state.mysql_conns.lock().insert(conn_id, conn);

    match res {
        Ok(qr) => {
            let tables: Vec<String> = qr
                .rows
                .into_iter()
                .filter_map(|mut r| r.pop())
                .collect();
            ToolResult::ok(format!(
                "共 {} 张表：\n{}",
                tables.len(),
                tables.join("\n")
            ))
        }
        Err(e) => ToolResult::err(format!("列出表失败: {e}")),
    }
}

/// describe_table：执行 `DESCRIBE <table>`，返回结构化文本。
async fn describe_table(state: &AppState, args: &Value) -> ToolResult {
    let (conn_id, table) = match (
        args.get("dbConnId").and_then(Value::as_str),
        args.get("table").and_then(Value::as_str),
    ) {
        (Some(c), Some(t)) => (c.to_string(), t.to_string()),
        _ => return ToolResult::err("describe_table 缺少 dbConnId 或 table"),
    };
    // 解析表标识符为安全的反引号限定名（支持 `table` 或 `db.table`）。
    let qualified = match crate::database::mysql::qualify_table_identifier(&table) {
        Ok(q) => q,
        Err(e) => return ToolResult::err(format!("{e}")),
    };
    let sql = format!("DESCRIBE {qualified}");

    let conn = {
        let mut map = state.mysql_conns.lock();
        match map.remove(&conn_id) {
            Some(c) => c,
            None => return ToolResult::err(format!("找不到 MySQL 连接 {conn_id}")),
        }
    };
    let res = conn.execute(&sql, 1000).await;
    state.mysql_conns.lock().insert(conn_id, conn);

    match res {
        Ok(qr) => ToolResult::ok(format_query_result(&qr)),
        Err(e) => ToolResult::err(format!("查看表结构失败: {e}")),
    }
}

// ===========================================================================
// 安全护栏
// ===========================================================================

/// 危险命令正则集合（exec_ssh 用）。
///
/// 命中任一即视为危险操作；前端会红色高亮 + 二次确认。
static DANGEROUS_CMD_REGEXES: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"(?i)rm\s+-[a-z]*r[a-z]*f[a-z]*\s+/(?:\s|$|\*)",
        r"(?i)rm\s+-[a-z]*f[a-z]*r[a-z]*\s+/(?:\s|$|\*)",
        r"(?i)\brm\s+-rf\s+/\*",
        r"(?i)\bmkfs\b",
        r"(?i)\bdd\s+if=.*of=/dev/(?:sd|nvme|hd|vd|xvd)",
        r"(?i)\b(shutdown|reboot|halt|poweroff)\b",
        r"(?i)\binit\s+[06]\b",
        r":\(\)\s*\{",                       // fork bomb :(){:|:&};:
        r"(?i)>\s*/dev/(?:sd|nvme|hd|vd|xvd)",
        r"(?i)chmod\s+-R\s+777\s+/(?:\s|$)",
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap_or_else(|e| panic!("无效正则 {p}: {e}")))
    .collect()
});

/// 危险 SQL 关键字（exec_sql 用）。
static DROP_TRUNCATE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^\s*(DROP|TRUNCATE)\b").expect("无效正则")
});

/// DELETE 无 WHERE 子句。
static DELETE_NO_WHERE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^\s*DELETE\s+FROM\s+\S+\s*(?:;|$)").expect("无效正则")
});

/// 判断一个工具调用是否危险。
///
/// - exec_ssh：command 命中危险命令模式（rm -rf /、mkfs、dd 写块设备、
///   shutdown/reboot、fork bomb、chmod -R 777 / 等）。
/// - exec_sql：DROP/TRUNCATE 开头，或 DELETE 无 WHERE 子句。
/// - 其它工具默认安全。
pub fn is_dangerous(name: &str, arguments: &Value) -> bool {
    match name {
        "exec_ssh" => arguments
            .get("command")
            .and_then(Value::as_str)
            .map(|cmd| {
                DANGEROUS_CMD_REGEXES
                    .iter()
                    .any(|re| re.is_match(cmd))
            })
            .unwrap_or(false),
        "exec_sql" => arguments
            .get("sql")
            .and_then(Value::as_str)
            .map(|sql| DROP_TRUNCATE_RE.is_match(sql) || DELETE_NO_WHERE_RE.is_match(sql))
            .unwrap_or(false),
        _ => false,
    }
}

/// 取一条 SQL 语句的第一个关键字（trim、不区分大小写）。多语句时只看第一条。
///
/// 用于按 SQL 模式（readonly/restricted/full）做粗粒度放行判定。
fn sql_first_keyword(sql: &str) -> String {
    // 只看第一条语句（按 ; 切分），取首个非空 token。
    let first_stmt = sql.split(';').next().unwrap_or("").trim();
    first_stmt
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase()
}

/// 根据 `sql_mode` 判断一条 SQL 是否被允许执行。
///
/// - `readonly`：只允许 `SELECT` / `SHOW` / `EXPLAIN` / `DESCRIBE` / `DESC` / `WITH`。
/// - `restricted`：上述 + 允许 `INSERT` / `UPDATE` / `DELETE` / `MERGE`（DDL 仍禁止）。
/// - `full`：允许一切（但 `is_dangerous` 仍生效，由上层确认）。
///
/// 按第一条语句的第一个关键字判断（trim、不区分大小写）。未知关键字在非 `full`
/// 模式下一律视为不允许（保守策略）。
pub fn sql_allowed_by_mode(sql: &str, mode: &str) -> bool {
    let kw = sql_first_keyword(sql);
    if kw.is_empty() {
        return false;
    }
    match mode {
        "full" => true,
        "restricted" => matches!(
            kw.as_str(),
            "SELECT"
                | "SHOW"
                | "EXPLAIN"
                | "DESCRIBE"
                | "DESC"
                | "WITH"
                | "INSERT"
                | "UPDATE"
                | "DELETE"
                | "MERGE"
        ),
        // 默认（含 "readonly" 及任何未知值）按只读处理。
        _ => matches!(
            kw.as_str(),
            "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "WITH"
        ),
    }
}

/// 判断一条 SQL 是否为只读查询（用于 `auto_approve_safe` 自动放行判定）。
///
/// 只读 = SELECT / SHOW / EXPLAIN / DESCRIBE / DESC / WITH。与
/// [`sql_allowed_by_mode`] 的 readonly 集合一致。
pub fn is_readonly_sql(sql: &str) -> bool {
    let kw = sql_first_keyword(sql);
    matches!(
        kw.as_str(),
        "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "WITH"
    )
}

/// Shell 元字符正则：命中任一即视为"复合命令"，**不**算白名单内。
///
/// 严格模式禁用：命令分隔符（;、&&、||、|）、命令替换（$()、反引号）、
/// 重定向（>、<、>>）、后台（&）、子 shell（括号）。这样保证白名单内命令是
/// "单一 argv 形式"，无法通过 `cat x; rm -rf /` 这类拼接绕过。
static COMMAND_METACHAR_RE: Lazy<Regex> = Lazy::new(|| {
    // 任一元字符出现即匹配。
    Regex::new(r"[;&|<>`]|\$\(|&&|\|\||>>").expect("无效元字符正则")
});

/// 判断一条 exec_ssh 命令是否落在白名单内（用于前端绿色卡片 + 默认放行 UX）。
///
/// **严格匹配规则**：
/// 1. 命令含任何 shell 元字符（`;` `&` `|` `>` `<` 反引号 `$(` 等）→ 直接返回 `false`
///    （防 `ls;rm -rf /` 绕过；这类命令必须走人工确认）。
/// 2. 否则取命令的前缀 token 序列（最多前 3 个 token，覆盖 `systemctl status nginx`、
///    `docker ps -a` 这类）逐一与白名单做**前缀匹配**：命令的 trim 后前缀以白名单项开头
///    （大小写不敏感，以空格对齐 token 边界）即视为命中。
///
/// 注意：本函数只决定"是否显示为白名单内（免确认 UX）"，**不**改变执行闭环——
/// 执行仍需用户点确认按钮（见 [`crate::commands::ai::run_agent_loop`]）。
pub fn is_whitelisted(command: &str, whitelist: &[String]) -> bool {
    let cmd = command.trim();
    if cmd.is_empty() {
        return false;
    }
    // 1. 含元字符 → 不算白名单内。
    if COMMAND_METACHAR_RE.is_match(cmd) {
        return false;
    }
    // 2. 取前缀 token（最多 3 个），组合成候选前缀集合逐级匹配。
    //    例：cmd = "systemctl status nginx" → 候选 ["systemctl", "systemctl status", "systemctl status nginx"]
    let lower = cmd.to_ascii_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    let mut prefix = String::new();
    for (i, tok) in tokens.iter().enumerate().take(3) {
        if i > 0 {
            prefix.push(' ');
        }
        prefix.push_str(tok);
        // 精确匹配或白名单项等于当前前缀。
        if whitelist.iter().any(|w| {
            let w = w.trim().to_ascii_lowercase();
            !w.is_empty() && w == prefix
        }) {
            return true;
        }
    }
    // 3. 也支持白名单项是命令的前缀（如白名单 "sys" 命中 "systemctl"）——但为安全起见
    //    要求白名单项至少 2 个字符且后接空格/结尾。
    for w in whitelist {
        let w = w.trim().to_ascii_lowercase();
        if w.len() < 2 {
            continue;
        }
        if lower == w || lower.starts_with(&format!("{w} ")) {
            return true;
        }
    }
    false
}

/// 校验 exec_ssh 命令是否被白名单允许（执行端用，返回错误信息供回填模型）。
///
/// 与 [`is_whitelisted`] 的区别：本函数返回 `Result`，仅用于"是否允许"的判定，
/// 由调用方决定如何反馈。当前 exec_ssh 执行端**不**用白名单拦截（保留人工确认闭环），
/// 此函数保留供未来"白名单内自动执行"模式使用。
#[allow(dead_code)]
pub fn check_command_whitelist(command: &str, whitelist: &[String]) -> Result<(), String> {
    if is_whitelisted(command, whitelist) {
        Ok(())
    } else {
        Err(format!(
            "命令 `{command}` 不在白名单中，需用户人工确认"
        ))
    }
}

// ===========================================================================
// 人类可读描述
// ===========================================================================

/// 生成工具调用的人类可读简述，用于前端确认弹窗。
pub fn describe_call(name: &str, arguments: &Value) -> String {
    match name {
        "exec_ssh" => {
            let cmd = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("执行命令: {cmd}")
        }
        "terminal_snapshot" => {
            let sid = arguments
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("?");
            format!("读取终端 {sid} 最近输出")
        }
        "exec_sql" => {
            let sql = arguments
                .get("sql")
                .and_then(Value::as_str)
                .unwrap_or("");
            let preview: String = sql.chars().take(60).collect();
            if sql.chars().count() > 60 {
                format!("执行 SQL: {preview}...")
            } else {
                format!("执行 SQL: {preview}")
            }
        }
        "list_db_tables" => "列出数据库表".into(),
        "describe_table" => {
            let t = arguments
                .get("table")
                .and_then(Value::as_str)
                .unwrap_or("?");
            format!("查看表 {t} 结构")
        }
        other => format!("执行工具: {other}"),
    }
}

