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
//! - `read_file` / `write_file` / `list_files`：本地文件读写（设置页开启"本地文件
//!   读写"后才下发）。只能在各助手工作目录（沙箱）内操作，详见 [`file_tools`]。
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
use std::path::{Path, PathBuf};
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::AppHandle;
use tokio::time::timeout;

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::utils::{format_query_result, strip_ansi};

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
/// read_file 单文件读取上限（1 MiB）。超出拒绝，防上下文爆炸。
const MAX_FILE_READ_BYTES: usize = 1024 * 1024;
/// write_file 单次写入上限（10 MiB）。
const MAX_FILE_WRITE_BYTES: usize = 10 * 1024 * 1024;
/// list_files 单目录最多返回条目数。
const MAX_LIST_ENTRIES: usize = 200;

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
cat 配置文件等）。单命令超时 30 秒，输出截断 16KB。"
                .into(),
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
用于了解用户当前看到了什么、上下文是什么。"
                .into(),
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

/// 本地文件读写工具集（设置页开启"本地文件读写"后才由编排层下发）。
///
/// 两个助手域（终端助手 / 数据库助手）共用同一组工具定义；执行时按请求所属的
/// domain（"ssh" / "db"）取该域在设置里配置的工作目录，路径参数一律视为
/// **相对工作目录的路径**（如 `data/users.csv`），绝对路径与 `..` 逃逸被拒绝。
///
/// - `read_file`：读取工作目录内文本文件（≤1 MiB，二进制拒绝）。
/// - `write_file`：写入文本到工作目录内文件（覆盖已有文件标记危险，需人工确认）。
/// - `list_files`：列出工作目录/子目录内容（帮助 AI 了解有哪些文件可用）。
pub fn file_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read_file".into(),
            description: "读取本地工作目录内的文本文件内容，返回原始文本。\
path 是相对工作目录的路径（如 data/users.csv），不允许绝对路径或 .. 逃逸。\
单文件上限 1MB；二进制文件会被拒绝。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "相对工作目录的文件路径，如 data/users.csv"
                    }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "write_file".into(),
            description: "把文本内容写入本地工作目录内的文件。path 是相对工作目录的路径，\
不允许绝对路径或 .. 逃逸。父目录需已存在（不会自动创建多级目录）。\
若目标文件已存在会被覆盖（用户会收到危险确认）。适合导出数据、保存脚本等。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "相对工作目录的文件路径，如 out/users.csv"
                    },
                    "content": {
                        "type": "string",
                        "description": "要写入的完整文本内容"
                    }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDef {
            name: "list_files".into(),
            description: "列出工作目录（或相对其的子目录）内的条目：文件名、类型（文件/目录）、\
大小。path 省略时列工作目录根。用于了解有哪些文件可用、确认输出文件是否已存在。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "相对工作目录的目录路径，省略或空串表示工作目录本身"
                    }
                },
                "required": []
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
/// `file_domain` 是请求所属助手域（"ssh"/"db"/""），文件工具据此取对应工作目录；
/// 非文件工具忽略该参数。
///
/// 任何执行错误都被吞掉并返回 `ToolResult { ok: false, output: <错误信息> }`，
/// 由调用方把错误回填给模型，让模型据此重试或解释给用户。
pub async fn execute_tool(
    app: &AppHandle,
    state: &AppState,
    call: &ToolCall,
    allowed: &HashSet<String>,
    visualization: bool,
    file_domain: &str,
) -> ToolResult {
    if !allowed.contains(&call.name) {
        return ToolResult::err(format!(
            "工具 `{}` 在当前上下文不可用（未提供活动终端或数据库连接）",
            call.name
        ));
    }
    match call.name.as_str() {
        "exec_ssh" => exec_ssh(state, &call.arguments, visualization).await,
        "terminal_snapshot" => terminal_snapshot(state, &call.arguments),
        "exec_sql" => exec_sql(app, state, &call.arguments, visualization).await,
        "list_db_tables" => list_db_tables(state, &call.arguments).await,
        "describe_table" => describe_table(state, &call.arguments).await,
        "read_file" => read_file(state, &call.arguments, file_domain),
        "write_file" => write_file(state, &call.arguments, file_domain),
        "list_files" => list_files(state, &call.arguments, file_domain),
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
async fn exec_ssh(state: &AppState, args: &Value, visualization: bool) -> ToolResult {
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
        if visualization {
            "可视化(写PTY)"
        } else {
            "独立连接"
        }
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
                .ok_or_else(|| AppError::NotFound(format!("会话配置 {session_config_id} 不存在")))?
        };
        let vault = {
            let guard = state.vault_read()?;
            guard
                .as_ref()
                .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
                .clone()
        };
        let conn = state.conn()?;
        let resolved = crate::ssh::session::resolve_credential(&session_config, &vault, &conn)?;
        Ok((session_config, resolved))
    })();
    let (session_config, resolved) = match setup {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("解析 SSH 凭据失败: {e}")),
    };

    // 4. 新建连接 + exec（整体 30s 超时）。
    let state_clone = state.clone();
    let run = async {
        // 连接（这里复用 SshSession::open 用的 connect_direct）。
        let handle = crate::ssh::client::connect_direct(
            &session_config.host,
            session_config.port,
            &session_config.username,
            &session_config.id,
            resolved.auth_method,
            state_clone,
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
/// - `baseline`：命令写入前缓冲的字节数（保留参数，已不用于截取逻辑）；
/// - `sentinel`：哨兵字符串（超时时为空）。
///
/// 哨兵会出现在两处：**命令回显行**（PTY 回显 `cmd; echo SENTINEL`，是第一次出现）
/// 和 **echo 命令的实际输出行**（最后一次出现）。用 `find` 取第一次会出现会把
/// "提示符+命令残片"当作输出，因此这里按行匹配：取第一个含哨兵的行之后、
/// 最后一个含哨兵的行之前的内容，即真正的命令输出。
fn extract_new_output(snapshot: &str, _baseline: usize, sentinel: &str) -> String {
    if sentinel.is_empty() {
        // 超时路径：返回快照全量。
        return snapshot.to_string();
    }
    let lines: Vec<&str> = snapshot.split_inclusive('\n').collect();
    let mut first: Option<usize> = None;
    let mut last: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if line.contains(sentinel) {
            if first.is_none() {
                first = Some(i);
            }
            last = Some(i);
        }
    }
    match (first, last) {
        (Some(f), Some(l)) if l > f => lines[f + 1..l].concat(),
        // 只有一处含哨兵（输出被环形缓冲滚掉等）：取该行之后的内容。
        (Some(f), Some(_)) => lines[f + 1..].concat(),
        // 快照里找不到哨兵：返回全量（调用方按超时/部分输出处理）。
        _ => snapshot.to_string(),
    }
}

/// 截断输出到指定字节数，超出则尾部提示。
fn truncate_output(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let cut = s
        .char_indices()
        .take_while(|(i, _)| *i <= max_bytes)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(max_bytes);
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
async fn exec_sql(
    app: &AppHandle,
    state: &AppState,
    args: &Value,
    visualization: bool,
) -> ToolResult {
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
            Err(e) => (
                Vec::new(),
                Vec::new(),
                0u64,
                Some(format!("SQL 执行失败: {e}")),
            ),
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
            let tables: Vec<String> = qr.rows.into_iter().filter_map(|mut r| r.pop()).collect();
            ToolResult::ok(format!("共 {} 张表：\n{}", tables.len(), tables.join("\n")))
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
// 本地文件读写工具（工作目录沙箱）
// ===========================================================================

/// 取指定助手域的工作目录（设置页配置）。未配置返回 None。
fn workspace_dir_for(state: &AppState, domain: &str) -> Option<String> {
    let settings = crate::config::settings_load_inner(state).ok()?;
    settings.ai.file_access.workspace_dirs.get(domain).cloned()
}

/// 把"相对工作目录"的路径解析为沙箱内绝对路径。
///
/// 安全规则：
/// 1. 拒绝绝对路径与含 `..` 组分的路径；
/// 2. 工作目录 canonicalize（解析 symlink，统一实际大小写）；
/// 3. 目标已存在 → canonicalize 全路径后必须位于工作目录内（防 symlink 逃逸）；
/// 4. 目标不存在（写新文件）→ 父目录 canonicalize 校验，文件名直接拼接。
fn resolve_workspace_path(workspace: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err("path 必须是相对工作目录的路径，不允许绝对路径".into());
    }
    if rel_path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("path 不允许包含 `..`（不能逃逸工作目录）".into());
    }
    let ws = workspace
        .canonicalize()
        .map_err(|e| format!("工作目录不可访问: {e}"))?;
    let joined = ws.join(rel_path);
    // 目标已存在：canonicalize 后校验前缀（可解析 symlink，防逃逸）。
    if joined.exists() {
        let real = joined
            .canonicalize()
            .map_err(|e| format!("解析路径失败: {e}"))?;
        if !real.starts_with(&ws) {
            return Err("路径超出工作目录范围，已拒绝".into());
        }
        return Ok(real);
    }
    // 目标不存在（写新文件）：父目录必须存在且在工作目录内。
    let parent = joined.parent().unwrap_or(&ws);
    if !parent.exists() {
        return Err(format!("父目录不存在: {}", parent.display()));
    }
    let real_parent = parent
        .canonicalize()
        .map_err(|e| format!("解析父目录失败: {e}"))?;
    if !real_parent.starts_with(&ws) {
        return Err("路径超出工作目录范围，已拒绝".into());
    }
    let name = joined
        .file_name()
        .ok_or_else(|| "无效的文件名".to_string())?;
    Ok(real_parent.join(name))
}

/// read_file：读取工作目录内文本文件（≤1 MiB）。
fn read_file(state: &AppState, args: &Value, domain: &str) -> ToolResult {
    let rel = match args.get("path").and_then(Value::as_str) {
        Some(p) => p,
        None => return ToolResult::err("read_file 缺少 path 参数"),
    };
    let ws = match workspace_dir_for(state, domain) {
        Some(w) => w,
        None => {
            return ToolResult::err(
                "当前助手未配置工作目录：请在设置页开启「本地文件读写」并选择工作目录",
            );
        }
    };
    let path = match resolve_workspace_path(Path::new(&ws), rel) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(e),
    };
    if !path.is_file() {
        return ToolResult::err(format!("{} 不是文件", path.display()));
    }
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return ToolResult::err(format!("读取文件信息失败: {e}")),
    };
    if meta.len() > MAX_FILE_READ_BYTES as u64 {
        return ToolResult::err(format!(
            "文件过大（{} 字节，上限 {} 字节）。请先手动拆分/截取后再让 AI 读取",
            meta.len(),
            MAX_FILE_READ_BYTES
        ));
    }
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => return ToolResult::err(format!("读取文件失败: {e}")),
    };
    // 二进制检测：含 NUL 字节即视为二进制，拒绝（防止把乱码/图片内容塞给模型）。
    if data.contains(&0) {
        return ToolResult::err("二进制文件不支持读取，仅支持文本文件");
    }
    let text = String::from_utf8_lossy(&data);
    ToolResult::ok(format!(
        "文件 {}（{} 字节）内容：\n{}",
        path.display(),
        data.len(),
        text
    ))
}

/// write_file：把文本写入工作目录内文件（覆盖已有文件在上层被标记危险）。
fn write_file(state: &AppState, args: &Value, domain: &str) -> ToolResult {
    let (rel, content) = match (
        args.get("path").and_then(Value::as_str),
        args.get("content").and_then(Value::as_str),
    ) {
        (Some(p), Some(c)) => (p, c),
        _ => return ToolResult::err("write_file 缺少 path 或 content 参数"),
    };
    let ws = match workspace_dir_for(state, domain) {
        Some(w) => w,
        None => {
            return ToolResult::err(
                "当前助手未配置工作目录：请在设置页开启「本地文件读写」并选择工作目录",
            );
        }
    };
    if content.len() > MAX_FILE_WRITE_BYTES {
        return ToolResult::err(format!(
            "内容过大（{} 字节，上限 {} 字节）",
            content.len(),
            MAX_FILE_WRITE_BYTES
        ));
    }
    let path = match resolve_workspace_path(Path::new(&ws), rel) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(e),
    };
    match std::fs::write(&path, content) {
        Ok(()) => ToolResult::ok(format!(
            "已写入 {}（{} 字节）",
            path.display(),
            content.len()
        )),
        Err(e) => ToolResult::err(format!("写入失败: {e}")),
    }
}

/// list_files：列出工作目录（或相对其的子目录）内的条目。
fn list_files(state: &AppState, args: &Value, domain: &str) -> ToolResult {
    let rel = args.get("path").and_then(Value::as_str).unwrap_or("");
    let ws = match workspace_dir_for(state, domain) {
        Some(w) => w,
        None => {
            return ToolResult::err(
                "当前助手未配置工作目录：请在设置页开启「本地文件读写」并选择工作目录",
            );
        }
    };
    let dir = match resolve_workspace_path(Path::new(&ws), rel) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(e),
    };
    if !dir.is_dir() {
        return ToolResult::err(format!("{} 不是目录", dir.display()));
    }
    let rd = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) => return ToolResult::err(format!("列出目录失败: {e}")),
    };
    let mut lines: Vec<String> = Vec::new();
    let mut count = 0usize;
    for entry in rd {
        if count >= MAX_LIST_ENTRIES {
            lines.push(format!("…（条目过多，仅显示前 {MAX_LIST_ENTRIES} 项）"));
            break;
        }
        let Ok(e) = entry else { continue };
        count += 1;
        let name = e.file_name().to_string_lossy().to_string();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            lines.push(format!("[目录] {name}/"));
        } else {
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            lines.push(format!("{name}（{} 字节）", size));
        }
    }
    if lines.is_empty() {
        lines.push("（空目录）".into());
    }
    ToolResult::ok(format!("目录 {}：\n{}", dir.display(), lines.join("\n")))
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
        r":\(\)\s*\{", // fork bomb :(){:|:&};:
        r"(?i)>\s*/dev/(?:sd|nvme|hd|vd|xvd)",
        r"(?i)chmod\s+-R\s+777\s+/(?:\s|$)",
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap_or_else(|e| panic!("无效正则 {p}: {e}")))
    .collect()
});

/// DELETE 无 WHERE 子句（非锚定：主语句可能是 CTE 之后的 DELETE，见
/// [`sql_first_keyword`]；同时覆盖 `DELETE t1 FROM t2` 别名形式）。
static DELETE_NO_WHERE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bDELETE\s+(?:[A-Za-z_][A-Za-z0-9_.]*\s+)?FROM\s+\S+\s*(?:;|$)")
        .expect("无效正则")
});

/// 判断一个工具调用是否危险。
///
/// - exec_ssh：command 命中危险命令模式（rm -rf /、mkfs、dd 写块设备、
///   shutdown/reboot、fork bomb、chmod -R 777 / 等）。
/// - exec_sql：有效关键字为 DROP/TRUNCATE，或 DELETE 无 WHERE 子句
///   （含 `WITH ... DELETE` 形式，见 [`sql_first_keyword`]）。
/// - write_file：目标文件已存在（覆盖已有数据）→ 危险。
/// - 其它工具默认安全。
///
/// `workspace` 为文件工具所属助手的工作目录（`write_file` 判断覆盖用）；
/// 非文件工具传 None 即可。
pub fn is_dangerous(name: &str, arguments: &Value, workspace: Option<&Path>) -> bool {
    match name {
        "exec_ssh" => arguments
            .get("command")
            .and_then(Value::as_str)
            .map(|cmd| DANGEROUS_CMD_REGEXES.iter().any(|re| re.is_match(cmd)))
            .unwrap_or(false),
        "exec_sql" => arguments
            .get("sql")
            .and_then(Value::as_str)
            .map(|sql| {
                let kw = sql_first_keyword(sql);
                kw == "DROP"
                    || kw == "TRUNCATE"
                    || (kw == "DELETE" && DELETE_NO_WHERE_RE.is_match(sql))
            })
            .unwrap_or(false),
        "write_file" => {
            // 覆盖已有文件：解析出沙箱路径后检查存在性（解析失败视为不危险，
            // 执行阶段会返回错误，不需要提前标记）。
            match (arguments.get("path").and_then(Value::as_str), workspace) {
                (Some(p), Some(ws)) => resolve_workspace_path(ws, p)
                    .map(|p| p.exists())
                    .unwrap_or(false),
                _ => false,
            }
        }
        _ => false,
    }
}

/// 剥掉 SQL 里的注释（`--` / `#` 行注释、`/* */` 块注释），替换为空格。
///
/// 防止 `-- 注释\nDELETE FROM t` 这类语句的第一个 token 是注释、绕过关键字判定。
fn strip_sql_comments(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    let n = bytes.len();
    while i < n {
        if i + 1 < n && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            // -- 行注释：到行尾。
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(' ');
        } else if bytes[i] == b'#' {
            // # 行注释：到行尾。
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(' ');
        } else if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // /* */ 块注释。
            i += 2;
            while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            out.push(' ');
        } else {
            let ch = sql[i..].chars().next().unwrap_or_default();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// 取一条 SQL 语句的"有效"关键字（trim、不区分大小写）。多语句时只看第一条。
///
/// - 先剥离前导/行内注释；
/// - `WITH` 引导的 CTE 语句返回其后的主语句关键字（如
///   `WITH cte AS (SELECT 1) DELETE FROM t` → `DELETE`），防止 CTE 携带写语句
///   绕过只读/危险判定（MySQL 8 支持 `WITH ... DELETE/UPDATE/INSERT` 单语句）；
/// - 其余返回首个关键字；空语句或无法判定返回空串（调用方保守拒绝）。
fn sql_first_keyword(sql: &str) -> String {
    let clean = strip_sql_comments(sql);
    let first_stmt = clean.split(';').next().unwrap_or("").trim();
    let tokens: Vec<&str> = first_stmt.split_whitespace().collect();
    let Some(first) = tokens.first() else {
        return String::new();
    };
    let first_up = first.to_ascii_uppercase();
    if first_up != "WITH" {
        return first_up;
    }

    // WITH 引导：跟踪括号深度，取 CTE 定义之后、括号外的第一个语句关键字。
    // 例：`WITH cte AS (SELECT 1) DELETE FROM t` → 括号外第一个关键字是 DELETE。
    let mut depth: i32 = 0;
    for tok in tokens.iter().skip(1) {
        depth += tok.matches('(').count() as i32 - tok.matches(')').count() as i32;
        if depth != 0 {
            continue;
        }
        let up = tok.to_ascii_uppercase();
        if matches!(
            up.as_str(),
            "SELECT"
                | "INSERT"
                | "UPDATE"
                | "DELETE"
                | "MERGE"
                | "REPLACE"
                | "DROP"
                | "TRUNCATE"
                | "CREATE"
                | "ALTER"
                | "CALL"
        ) {
            return up;
        }
    }
    // 未找到主语句关键字（残缺语句）→ 返回 WITH 本身；WITH 不在任何放行集合内，
    // 调用方会按"不允许"处理，属保守拒绝。
    "WITH".to_string()
}

/// 根据 `sql_mode` 判断一条 SQL 是否被允许执行。
///
/// - `readonly`：只允许 `SELECT` / `SHOW` / `EXPLAIN` / `DESCRIBE` / `DESC`。
///   注意 `WITH` 不在集合内——[`sql_first_keyword`] 会把 CTE 语句解析成其主语句
///   关键字（`WITH cte AS (...) SELECT ...` → SELECT，仍放行；而
///   `WITH cte AS (...) DELETE ...` → DELETE，被拦截）。
/// - `restricted`：上述 + 允许 `INSERT` / `UPDATE` / `DELETE` / `MERGE`（DDL 仍禁止）。
/// - `full`：允许一切（但 `is_dangerous` 仍生效，由上层确认）。
///
/// 未知关键字在非 `full` 模式下一律视为不允许（保守策略）。
pub fn sql_allowed_by_mode(sql: &str, mode: &str) -> bool {
    let kw = sql_first_keyword(sql);
    if kw.is_empty() {
        return false;
    }
    match mode {
        "full" => true,
        "restricted" => matches!(
            kw.as_str(),
            "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "INSERT" | "UPDATE" | "DELETE" | "MERGE"
        ),
        // 默认（含 "readonly" 及任何未知值）按只读处理。
        _ => matches!(kw.as_str(), "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC"),
    }
}

/// 判断一条 SQL 是否为只读查询（用于"白名单运行"模式下自动放行判定）。
///
/// 只读 = SELECT / SHOW / EXPLAIN / DESCRIBE / DESC（CTE 语句由
/// [`sql_first_keyword`] 解析成主语句关键字后再判定）。与
/// [`sql_allowed_by_mode`] 的 readonly 集合一致。
pub fn is_readonly_sql(sql: &str) -> bool {
    let kw = sql_first_keyword(sql);
    matches!(kw.as_str(), "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC")
}

/// Shell 元字符正则：命中任一即视为"复合命令"，**不**算白名单内。
///
/// 严格模式禁用：命令分隔符（;、&&、||、|）、命令替换（$()、反引号）、
/// 重定向（>、<、>>）、后台（&）、子 shell（括号）。这样保证白名单内命令是
/// "单一 argv 形式"，无法通过 `cat x; rm -rf /` 这类拼接绕过。
///
/// **`\n`/`\r` 必须拒绝**：换行是 shell 命令分隔符。`"ls\nrm -rf /"` 若只按空白
/// 分词会被拆成 `["ls", "rm", "-rf", "/"]`，前 3 个 token 恰好命中白名单项 `ls`，
/// 导致第二条命令在 whitelist 模式下免确认执行（提示注入可触发）。
static COMMAND_METACHAR_RE: Lazy<Regex> = Lazy::new(|| {
    // 任一元字符出现即匹配。`\x00` 是 NUL（regex crate 不支持 `\0` 写法，
    // 会 panic）；命令字符串不应含 NUL，出现即视为恶意输入拒绝。
    Regex::new(r"[;&|<>`\n\r\x00]|\$\(|&&|\|\||>>").expect("无效元字符正则")
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
        Err(format!("命令 `{command}` 不在白名单中，需用户人工确认"))
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
            let sql = arguments.get("sql").and_then(Value::as_str).unwrap_or("");
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
        "read_file" => {
            let p = arguments.get("path").and_then(Value::as_str).unwrap_or("?");
            format!("读取文件: {p}")
        }
        "write_file" => {
            let p = arguments.get("path").and_then(Value::as_str).unwrap_or("?");
            format!("写入文件: {p}")
        }
        "list_files" => {
            let p = arguments
                .get("path")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("工作目录");
            format!("列出目录: {p}")
        }
        other => format!("执行工具: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 白名单元字符正则必须能正常编译（回归：曾用 `\0` 写法导致 regex crate
    /// 拒绝编译，首次使用 `Lazy` 初始化时直接 panic）。访问 `COMMAND_METACHAR_RE`
    /// 即触发编译，编译失败会让本测试（及所有依赖它的用例）直接报错。
    #[test]
    fn metachar_regex_compiles() {
        assert!(COMMAND_METACHAR_RE.is_match("ls; rm -rf /"));
    }

    /// 各类 shell 分隔符/重定向/命令替换必须被判定为"非白名单"（防拼接绕过）。
    #[test]
    fn whitelist_rejects_metachars() {
        let wl: Vec<String> = vec!["ls".into(), "cat".into()];
        // 注释里记录的注入场景：换行拆分后 token 恰好命中白名单项。
        assert!(!is_whitelisted("ls\nrm -rf /", &wl));
        assert!(!is_whitelisted("ls; rm -rf /", &wl));
        assert!(!is_whitelisted("cat a.txt | rm -rf /", &wl));
        assert!(!is_whitelisted("ls && whoami", &wl));
        assert!(!is_whitelisted("ls $(whoami)", &wl));
        assert!(!is_whitelisted("echo `whoami`", &wl));
        assert!(!is_whitelisted("ls > out", &wl));
        assert!(!is_whitelisted("cat x >> log", &wl));
        // NUL 字符（`\x00`）：命令字符串不应含 NUL，出现即拒绝。
        assert!(!is_whitelisted("ls\x00rm -rf /", &wl));
        assert!(!is_whitelisted("cat x &", &wl));
    }

    /// 纯白名单命令（无元字符、前缀命中）应放行；大小写不敏感。
    #[test]
    fn whitelist_accepts_simple_commands() {
        let wl: Vec<String> = vec!["systemctl".into(), "docker ps".into()];
        assert!(is_whitelisted("systemctl status nginx", &wl));
        assert!(is_whitelisted("SYSTEMCTL status", &wl));
        assert!(is_whitelisted("docker ps -a", &wl));
        // 未命中白名单的普通命令：无元字符但不在白名单 → false。
        assert!(!is_whitelisted("reboot now", &wl));
        assert!(!is_whitelisted("", &wl));
        // 白名单前缀必须 ≥2 字符且以空格/结尾对齐，单字符 "s" 不允许。
        let wl_short: Vec<String> = vec!["s".into()];
        assert!(!is_whitelisted("systemctl status", &wl_short));
        // 白名单项是命令前缀时需 token 边界：白名单 "sys" 不命中 "systemctl"。
        let wl_prefix: Vec<String> = vec!["sys".into()];
        assert!(is_whitelisted("sys", &wl_prefix));
        assert!(!is_whitelisted("systemctl status", &wl_prefix));
    }
}
