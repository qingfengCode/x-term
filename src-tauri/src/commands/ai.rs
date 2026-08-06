//! AI 助手命令。
//!
//! 入口 [`ai_chat`]：根据 [`crate::config::Settings`] 中配置的 provider，调用
//! 对应的 [`LlmProvider`] 进行对话。
//!
//! # 智能体（工具调用）模式
//!
//! 当 `AiChatRequest::agent_mode == true` 时，[`ai_chat`] 会进入
//! [`run_agent_loop`] 多轮编排循环：
//!
//! 1. 把 [`crate::ai::tools::all_tools`] 一起发给模型。
//! 2. 模型若返回 `tool_calls`，对每个调用：发射 `ai:tool_call` 事件（含危险标记
//!    与人类可读描述）→ 通过 oneshot 阻塞等待前端确认 → 执行工具 → 发射
//!    `ai:tool_result` → 把结果以 role=tool 消息回填。
//! 3. 循环直到模型给出纯文本回复（无 tool_calls）或达到 `max_tool_calls` 上限。
//!
//! `agent_mode == false` 时（翻译/诊断/解释等旧场景）传入空工具集，模型不会
//! 调用工具，行为等价于普通流式对话——但走的是统一的 `chat_with_tools` 通道，
//! 这样 `chat_stream` 可以逐步废弃。
//!
//! 前端通过 [`ai_execute_tool`] / [`ai_cancel_tool`] 把工具确认结果发回。

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tauri::{AppHandle, State};

use crate::ai::provider::{build_provider, ChatMessage, Role};
use crate::ai::tools::{self, ToolApproval, ToolResult};
use crate::config::{
    settings_load_inner, FileAccessSettings, SqlAgentSettings, SshAgentSettings, RUN_MODE_AUTO,
    RUN_MODE_WHITELIST,
};
use crate::error::{AppError, AppResult};
use crate::events::{
    self, AiDoneEvent, AiErrorEvent, AiToolCallEvent, AiToolResultEvent, AI_DONE, AI_ERROR,
    AI_TOOL_CALL, AI_TOOL_RESULT,
};
use crate::state::AppState;

/// 工具确认默认超时（5 分钟）。超时视为拒绝。
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// 对话请求参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiChatRequest {
    /// 客户端分配的请求 id，用于匹配流式事件。
    pub request_id: String,
    /// 对话消息列表（含 system / user / assistant）。
    pub messages: Vec<ChatMessage>,
    /// 是否启用智能体模式（工具调用）。前端 agent 模式时传 true。
    #[serde(default)]
    pub agent_mode: bool,
    /// 当前活动终端 instanceId（保留字段，工具上下文目前由模型自行决定）。
    #[serde(default)]
    pub active_terminal_id: Option<String>,
    /// 当前活动 MySQL 连接 id（保留字段）。
    #[serde(default)]
    pub active_db_conn_id: Option<String>,
    /// 请求所属助手域："ssh"（终端助手）| "db"（数据库助手）。
    /// 文件工具（read_file / write_file / list_files）据此取对应工作目录。
    #[serde(default)]
    pub domain: Option<String>,
}

/// 发起一次 AI 对话（流式；agent 模式下走多轮工具调用循环）。
///
/// 该命令在收到请求后立即 spawn 一个后台任务执行实际的网络调用，
/// 命令本身很快返回 `Ok(())`；所有响应通过事件推送。这样前端不必长时间 await。
#[tauri::command]
pub async fn ai_chat(
    req: AiChatRequest,
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<()> {
    // 读设置、构造 provider，全部在命令线程完成（spawn 之前的同步部分）。
    let settings = settings_load_inner(&state)?;
    let provider_cfg = settings
        .ai
        .active_provider()
        .ok_or_else(|| AppError::InvalidInput("未配置 AI provider，请先在设置中添加".into()))?;
    // 智能体循环轮数上限与上下文裁剪预算均来自模型配置（见设置页「模型参数」）。
    let max_tool_calls = provider_cfg.max_tool_calls.max(1);
    let context_budget = provider_cfg
        .context_window
        .saturating_sub(provider_cfg.max_output)
        .max(1) as usize;
    let provider = build_provider(&provider_cfg)?;

    // SSH / SQL 智能体配置：分别克隆一份 move 进 spawned task。
    // SSH 域读 ssh_agent（run_mode / command_whitelist / terminal_visualization），
    // SQL 域读 sql_agent（run_mode / sql_mode / terminal_visualization）。
    // 文件读写配置：启用时下发文件工具，按请求 domain 取工作目录。
    let ssh_cfg = settings.ai.ssh_agent.clone();
    let sql_cfg = settings.ai.sql_agent.clone();
    let file_cfg = settings.ai.file_access.clone();

    // AppState 所有字段均为 Arc，clone 廉价且共享同一份内部数据。
    let task_state = state.inner().clone();

    let request_id = req.request_id.clone();
    let app_clone = app.clone();
    let request_id_for_cleanup = request_id.clone();
    let pending_ai_tasks = state.pending_ai_tasks.clone();

    let join = tokio::spawn(async move {
        let result = run_agent_loop(
            &app_clone,
            task_state,
            req,
            provider,
            ssh_cfg,
            sql_cfg,
            file_cfg,
            max_tool_calls,
            context_budget,
        )
        .await;
        // 任务结束（正常完成或被 abort）后，从 pending_ai_tasks 移除自己。
        pending_ai_tasks.lock().remove(&request_id_for_cleanup);
        if let Err(e) = result {
            events::emit(
                &app_clone,
                AI_ERROR,
                AiErrorEvent {
                    request_id: request_id_for_cleanup,
                    message: e.to_string(),
                },
            );
        }
    });

    // 登记 JoinHandle，供 ai_stop 取出 abort。
    // 同一 request_id 二次登记（前端复用/重试）时先 abort 旧任务，避免旧任务
    // 成为孤儿继续 emit 事件、并在结束时误删新任务的登记项。
    if let Some(old) = state.pending_ai_tasks.lock().insert(request_id, join) {
        old.abort();
    }

    Ok(())
}

/// 确认执行某个工具调用（前端"批准"按钮触发）。
#[tauri::command]
pub async fn ai_execute_tool(tool_call_id: String, state: State<'_, AppState>) -> AppResult<()> {
    if let Some((_, tx)) = state.pending_tool_calls.lock().remove(&tool_call_id) {
        let _ = tx.send(ToolApproval { approved: true });
    }
    Ok(())
}

/// 取消某个工具调用（前端"拒绝"按钮触发）。
#[tauri::command]
pub async fn ai_cancel_tool(tool_call_id: String, state: State<'_, AppState>) -> AppResult<()> {
    if let Some((_, tx)) = state.pending_tool_calls.lock().remove(&tool_call_id) {
        let _ = tx.send(ToolApproval { approved: false });
    }
    Ok(())
}

/// 终止正在进行的 AI 请求（前端"终止"按钮触发）。
///
/// 取出 requestId 对应的后台任务 JoinHandle 调 `abort()`，整个 future 树
/// （包括 `chat_with_tools` 的流式读取、工具执行、工具确认等待）会在最近的
/// await 点被取消。abort 后 spawn 的 future 不会再执行收尾代码，因此本命令
/// 同时负责清理：
/// - `pending_ai_tasks`：移除自身（abort 不会走任务内的 cleanup）；
/// - `pending_tool_calls`：给**属于本请求**的阻塞中工具确认发拒绝信号
///   （按 toolCallId → requestId 映射过滤，不误伤其他并发会话的确认项）。
///
/// 注意：abort 不会发射任何 AI 事件，前端需在调用本命令后自行把 sending 置 false
/// （前端也会订阅 ai:stopped 事件作为统一收尾信号）。
#[tauri::command]
pub async fn ai_stop(
    request_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<()> {
    // 1. 取出并 abort 后台任务。
    if let Some(join) = state.pending_ai_tasks.lock().remove(&request_id) {
        join.abort();
    }
    // 2. 只拒绝属于本请求的阻塞中工具确认（防止 oneshot 泄漏 + 让相关 UI 收尾）。
    //    按 requestId 过滤：`ai_stop` 不该影响其他并发会话正在等待的确认。
    let ids: Vec<String> = {
        let map = state.pending_tool_calls.lock();
        map.iter()
            .filter(|(_, (req_id, _))| req_id == &request_id)
            .map(|(id, _)| id.clone())
            .collect()
    };
    let mut pending = Vec::new();
    {
        let mut map = state.pending_tool_calls.lock();
        for id in &ids {
            if let Some((_, tx)) = map.remove(id) {
                pending.push(tx);
            }
        }
    }
    for tx in pending {
        let _ = tx.send(crate::ai::tools::ToolApproval { approved: false });
    }
    // 3. 发射 ai:stopped 事件，前端据此统一收尾（标记 sending=false 等）。
    events::emit(
        &app,
        crate::events::AI_STOPPED,
        crate::events::AiStoppedEvent {
            request_id: request_id.clone(),
        },
    );
    Ok(())
}

/// 把一条命令前缀加入白名单并持久化（前端卡片"加入白名单并执行"按钮触发）。
///
/// - 读取当前 settings，把 `command` 去空白后加入 `ai.ssh_agent.command_whitelist`（去重），
///   重新保存到 settings.json。
/// - `command` 可以是完整命令（如 `df -h`），函数只取**第一个 token**作为白名单条目
///   （与 `is_whitelisted` 的前缀匹配规则一致）。
#[tauri::command]
pub fn ai_add_to_whitelist(command: String, state: State<'_, AppState>) -> AppResult<()> {
    // 取首个 token 作为白名单前缀。
    let prefix = command.split_whitespace().next().unwrap_or("").trim();
    if prefix.is_empty() {
        return Err(AppError::InvalidInput("命令前缀为空".into()));
    }
    let mut settings = settings_load_inner(&state)?;
    if !settings
        .ai
        .ssh_agent
        .command_whitelist
        .iter()
        .any(|w| w == prefix)
    {
        settings
            .ai
            .ssh_agent
            .command_whitelist
            .push(prefix.to_string());
        let path = state
            .settings_path
            .as_path()
            .join(crate::config::SETTINGS_FILENAME);
        crate::storage::json_store::write_json(&path, &settings)?;
    }
    Ok(())
}

/// 设置某个助手域的工作目录并持久化（设置页「本地文件读写」卡片触发）。
///
/// `domain` 为 "ssh"（终端助手）| "db"（数据库助手）；`path` 为绝对路径。
/// AI 只能在该目录及子目录内读写文件（沙箱）。传空串清除配置。
#[tauri::command]
pub fn set_workspace_dir(
    domain: String,
    path: String,
    state: State<'_, AppState>,
) -> AppResult<()> {
    if domain != "ssh" && domain != "db" {
        return Err(AppError::InvalidInput(format!("无效的助手域: {domain}")));
    }
    let mut settings = settings_load_inner(&state)?;
    if path.trim().is_empty() {
        settings.ai.file_access.workspace_dirs.remove(&domain);
    } else {
        // 校验目录存在（不校验是否为目录内可达，AI 执行期会 canonicalize 复查）。
        let p = std::path::Path::new(path.trim());
        if !p.is_absolute() {
            return Err(AppError::InvalidInput("工作目录必须是绝对路径".into()));
        }
        if !p.exists() {
            return Err(AppError::NotFound(format!("目录不存在: {}", p.display())));
        }
        settings
            .ai
            .file_access
            .workspace_dirs
            .insert(domain, path.trim().to_string());
    }
    let path = state
        .settings_path
        .as_path()
        .join(crate::config::SETTINGS_FILENAME);
    crate::storage::json_store::write_json(&path, &settings)?;
    Ok(())
}

// ===========================================================================
// 智能体编排循环
// ===========================================================================

/// 智能体多轮编排循环。
///
/// 每一轮：调用 `chat_with_tools` → 若有 tool_calls 则逐个走"emit→等待确认→执行
/// →emit 结果"流程 → 把结果回填 → 进入下一轮。直到模型给出无 tool_calls 的纯文本
/// 回复（发射 `ai:done`），或达到 `max_tool_calls` 上限（也发射 `ai:done`）。
///
/// `max_tool_calls` 与 `context_budget` 来自激活模型的配置（设置页可编辑）：
/// - `max_tool_calls`：最大工具调用数，防止模型陷入工具调用死循环；
/// - `context_budget`：上下文预算（context_window - max_output），超出部分的历史
///   消息在发送前被 [`trim_history_for_context`] 丢弃。
async fn run_agent_loop(
    app: &AppHandle,
    state: AppState,
    req: AiChatRequest,
    provider: Box<dyn crate::ai::provider::LlmProvider>,
    ssh_cfg: SshAgentSettings,
    sql_cfg: SqlAgentSettings,
    file_cfg: FileAccessSettings,
    max_tool_calls: usize,
    context_budget: usize,
) -> AppResult<()> {
    // agent_mode 决定是否传入工具集。false 时 tools 为空，等同普通对话。
    // 工具集按活动上下文裁剪（块A）：有活动终端→SSH 工具；有活动 DB 连接→SQL 工具；
    // 设置页开启「本地文件读写」→ 追加文件工具（read_file/write_file/list_files）。
    let mut tools = Vec::new();
    if req.agent_mode {
        tools.extend(tools::tools_for_context(
            req.active_terminal_id.as_deref(),
            req.active_db_conn_id.as_deref(),
        ));
        if file_cfg.enabled {
            tools.extend(tools::file_tools());
        }
    }
    // 文件工具按请求所属域取工作目录（未配置时执行器返回明确错误引导用户）。
    let file_domain = req.domain.as_deref().unwrap_or("");
    let file_workspace = if file_cfg.enabled {
        file_cfg
            .workspace_dirs
            .get(file_domain)
            .map(|p| std::path::PathBuf::from(p))
    } else {
        None
    };
    let allowed = tools::allowed_tool_names(&tools);
    let mut messages = req.messages.clone();
    let mut tool_results: Vec<(String, ToolResult)> = Vec::new();
    let request_id = req.request_id.clone();
    let mut last_text = String::new();

    for _iter in 0..max_tool_calls {
        // 上下文裁剪：估算 tokens 超出预算时丢弃最旧的历史（保留 system 与最近消息），
        // 避免长对话超出模型上下文窗口。裁剪只影响本轮发送，不影响 messages 本身。
        let mut round_messages = messages.clone();
        trim_history_for_context(&mut round_messages, context_budget);
        let resp = provider
            .chat_with_tools(
                round_messages,
                tools.clone(),
                tool_results.clone(),
                request_id.clone(),
                app.clone(),
            )
            .await?;

        last_text = resp.message.clone();

        if resp.tool_calls.is_empty() {
            // 纯文本回复：chat_with_tools 已 emit 所有 chunk，这里收尾。
            events::emit(
                app,
                AI_DONE,
                AiDoneEvent {
                    request_id: request_id.clone(),
                    full_text: resp.message,
                },
            );
            return Ok(());
        }

        // 把本轮 assistant 消息（含 tool_calls）追加进 messages。
        messages.push(ChatMessage {
            role: crate::ai::provider::Role::Assistant,
            content: resp.message,
            tool_calls: Some(resp.tool_calls.clone()),
            tool_call_id: None,
        });

        // 逐个工具调用：emit → 等待确认 → 执行 → emit 结果 → 回填进 messages。
        // 关键：每个 tool_call 的结果必须以 role=tool 消息紧跟在 assistant(tool_calls)
        // 之后追加进 messages，否则 OpenAI/Anthropic 协议会以 400 拒绝
        // （"assistant with tool_calls must be followed by tool messages"）。
        tool_results.clear();
        for call in &resp.tool_calls {
            // 上下文裁剪（块A）：若模型幻觉调用了未 advertised 的工具，直接拒绝并回填。
            if !allowed.contains(&call.name) {
                let msg = format!(
                    "工具 `{}` 在当前上下文不可用（未提供活动终端或数据库连接）",
                    call.name
                );
                events::emit(
                    app,
                    AI_TOOL_RESULT,
                    AiToolResultEvent {
                        request_id: request_id.clone(),
                        tool_call_id: call.id.clone(),
                        ok: false,
                        output: msg.clone(),
                    },
                );
                tool_results.push((
                    call.id.clone(),
                    ToolResult {
                        ok: false,
                        output: msg.clone(),
                    },
                ));
                // 同样把结果作为 role=tool 消息追加进 messages。
                messages.push(ChatMessage {
                    role: crate::ai::provider::Role::Tool,
                    content: msg,
                    tool_calls: None,
                    tool_call_id: Some(call.id.clone()),
                });
                continue;
            }
            let dangerous =
                tools::is_dangerous(&call.name, &call.arguments, file_workspace.as_deref());
            let description = tools::describe_call(&call.name, &call.arguments);

            // === 域分发：按工具名取对应配置 + 计算 whitelisted / auto_run ===
            // exec_ssh：白名单判定 + ssh_cfg.run_mode 决定自动放行。
            // exec_sql：先按 sql_mode 校验是否允许（不允许直接拒绝，不执行）；
            //           只读查询（is_readonly_sql）视作"安全"（前端绿色卡片），
            //           配合 sql_cfg.run_mode 决定自动放行。
            // 文件工具（read_file/write_file/list_files）：启用后自动处理——
            //           读/列自动执行，写文件仅覆盖已有文件（危险）时走确认。
            // 其它工具（terminal_snapshot / list_db_tables / describe_table）：默认安全，
            // 不走确认（auto_run = true，无副作用读操作）。
            let domain = if call.name == "exec_ssh" {
                "ssh"
            } else if call.name == "exec_sql" {
                "sql"
            } else if matches!(
                call.name.as_str(),
                "read_file" | "write_file" | "list_files"
            ) {
                "file"
            } else {
                "other"
            };

            // exec_sql 模式校验：sql_mode 不允许的语句直接回填错误，不进入确认/执行流程。
            if domain == "sql" {
                let sql_text = call
                    .arguments
                    .get("sql")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !tools::sql_allowed_by_mode(sql_text, &sql_cfg.sql_mode) {
                    let msg = format!(
                        "当前 SQL 模式为 `{}`，不允许执行该语句（首个关键字被拦截）",
                        sql_cfg.sql_mode
                    );
                    events::emit(
                        app,
                        AI_TOOL_RESULT,
                        AiToolResultEvent {
                            request_id: request_id.clone(),
                            tool_call_id: call.id.clone(),
                            ok: false,
                            output: msg.clone(),
                        },
                    );
                    tool_results.push((
                        call.id.clone(),
                        ToolResult {
                            ok: false,
                            output: msg.clone(),
                        },
                    ));
                    messages.push(ChatMessage {
                        role: crate::ai::provider::Role::Tool,
                        content: msg,
                        tool_calls: None,
                        tool_call_id: Some(call.id.clone()),
                    });
                    continue;
                }
            }

            // whitelisted / auto_run：按域 + 各自运行模式计算。
            //   - ssh：命令命中 ssh 白名单即 whitelisted=true（前端绿色卡片）；
            //          auto_run 按 ssh_cfg.run_mode：auto=全部自动 / whitelist=白名单
            //          内且非危险 / manual=全部人工确认。
            //   - sql：只读查询 whitelisted=true；auto_run 按 sql_cfg.run_mode：
            //          auto=全部自动 / whitelist=只读且非危险 / manual=全部人工确认。
            //   - file：启用即自动处理（读/列直接执行；写仅覆盖已有文件时确认）。
            //   - 其它：无副作用读操作，whitelisted=false 但 auto_run=true（直接执行）。
            //
            // auto 模式放开"危险永远人工确认"的护栏（用户显式选择无人值守）：
            // 危险操作也自动执行；manual / whitelist 模式下危险操作仍强制人工确认。
            let (whitelisted, auto_run) = match domain {
                "ssh" => {
                    let w = call
                        .arguments
                        .get("command")
                        .and_then(Value::as_str)
                        .map(|c| tools::is_whitelisted(c, &ssh_cfg.command_whitelist))
                        .unwrap_or(false);
                    let auto = match ssh_cfg.run_mode.as_str() {
                        RUN_MODE_AUTO => true,
                        RUN_MODE_WHITELIST => w && !dangerous,
                        _ => false, // manual：全部人工确认
                    };
                    (w, auto)
                }
                "sql" => {
                    let readonly = call
                        .arguments
                        .get("sql")
                        .and_then(Value::as_str)
                        .map(tools::is_readonly_sql)
                        .unwrap_or(false);
                    // 危险 SQL（DROP/TRUNCATE/无 WHERE DELETE）在 auto 模式下自动执行，
                    // manual / whitelist 模式下仍强制人工确认。
                    let auto = match sql_cfg.run_mode.as_str() {
                        RUN_MODE_AUTO => true,
                        RUN_MODE_WHITELIST => readonly && !dangerous,
                        _ => false, // manual：全部人工确认
                    };
                    (readonly, auto)
                }
                "file" => {
                    // 写文件若覆盖已有文件（dangerous）仍走确认，其余自动执行。
                    (false, call.name != "write_file" || !dangerous)
                }
                _ => (false, true),
            };
            events::emit(
                app,
                AI_TOOL_CALL,
                AiToolCallEvent {
                    request_id: request_id.clone(),
                    tool_call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.to_string(),
                    description,
                    dangerous,
                    whitelisted,
                    auto_approved: auto_run,
                },
            );

            // 终端可视化模式：按工具域取各自的可视化开关。
            // - exec_ssh：ssh_cfg.terminal_visualization（命令写进 PTY）
            // - exec_sql：sql_cfg.terminal_visualization（SQL + 结果回显到 SQL 控制台）
            // 两者独立设置，互不影响。
            let visualization = match domain {
                "ssh" => ssh_cfg.terminal_visualization,
                "sql" => sql_cfg.terminal_visualization,
                _ => false,
            };

            let result = if auto_run {
                // 自动放行：不等待人工确认，直接执行。
                let r =
                    tools::execute_tool(app, &state, call, &allowed, visualization, file_domain)
                        .await;
                events::emit(
                    app,
                    AI_TOOL_RESULT,
                    AiToolResultEvent {
                        request_id: request_id.clone(),
                        tool_call_id: call.id.clone(),
                        ok: r.ok,
                        output: r.output.clone(),
                    },
                );
                r
            } else {
                // 注册 oneshot 等待前端确认（带上 requestId，供 ai_stop 精确清理）。
                let (tx, rx) = tokio::sync::oneshot::channel::<ToolApproval>();
                state
                    .pending_tool_calls
                    .lock()
                    .insert(call.id.clone(), (request_id.clone(), tx));

                let approval = tokio::time::timeout(APPROVAL_TIMEOUT, rx).await;
                state.pending_tool_calls.lock().remove(&call.id);

                match approval {
                    Ok(Ok(ToolApproval { approved: true })) => {
                        let r = tools::execute_tool(
                            app,
                            &state,
                            call,
                            &allowed,
                            visualization,
                            file_domain,
                        )
                        .await;
                        events::emit(
                            app,
                            AI_TOOL_RESULT,
                            AiToolResultEvent {
                                request_id: request_id.clone(),
                                tool_call_id: call.id.clone(),
                                ok: r.ok,
                                output: r.output.clone(),
                            },
                        );
                        r
                    }
                    _ => {
                        // 拒绝或超时。
                        events::emit(
                            app,
                            AI_TOOL_RESULT,
                            AiToolResultEvent {
                                request_id: request_id.clone(),
                                tool_call_id: call.id.clone(),
                                ok: false,
                                output: "用户拒绝了该操作或确认超时".into(),
                            },
                        );
                        ToolResult {
                            ok: false,
                            output: "用户拒绝了该操作或确认超时".into(),
                        }
                    }
                }
            };
            tool_results.push((call.id.clone(), result.clone()));
            // 把工具结果作为 role=tool 消息追加进 messages（紧跟 assistant(tool_calls)）。
            messages.push(ChatMessage {
                role: crate::ai::provider::Role::Tool,
                content: result.output.clone(),
                tool_calls: None,
                tool_call_id: Some(call.id.clone()),
            });
        }
        // 继续下一轮：messages 已含完整的 [assistant(tool_calls), tool, tool, ...] 链。
    }

    // 达到 max_tool_calls 上限：以最后一段文本收尾。
    log::warn!(
        "[ai:{}] 智能体循环达到 {} 轮上限，强制结束",
        request_id,
        max_tool_calls
    );
    events::emit(
        app,
        AI_DONE,
        AiDoneEvent {
            request_id,
            full_text: last_text,
        },
    );
    Ok(())
}

// ===========================================================================
// 上下文窗口裁剪
// ===========================================================================

/// 单条消息除正文外的固定协议开销（role、分隔符等），估算时计入。
const MESSAGE_OVERHEAD_TOKENS: usize = 8;

/// 粗略估算一段文本的 token 数。
///
/// 不做精确分词：CJK 字符按 1 token/字（中文分词基本一字一 token），其余按
/// 4 字符/token（英文常见经验值）。用于上下文窗口预算，够用且廉价。
fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for c in text.chars() {
        let cp = c as u32;
        // CJK 统一表意文字（基本区 + 扩展 A），覆盖中/日/韩文。
        if (0x4E00..=0x9FFF).contains(&cp) || (0x3400..=0x4DBF).contains(&cp) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other / 4
}

/// 按上下文预算裁剪历史消息（就地修改）。
///
/// 规则：
/// - system 消息永远保留（携带系统指令）；
/// - 从最旧的非 system 消息开始丢弃，直到估算总 tokens 不超过 `budget`；
/// - 丢弃带 `tool_calls` 的 assistant 消息时，连带其后连续的 tool 结果消息一起丢弃，
///   否则会残留"孤儿 tool 消息"——OpenAI/Anthropic 协议要求 tool 消息必须跟在
///   带 tool_calls 的 assistant 消息之后，否则以 400 拒绝；
/// - 至少保留最后一条非 system 消息（当前用户问题不能被裁掉）。
fn trim_history_for_context(messages: &mut Vec<ChatMessage>, budget: usize) {
    let tokens_of = |m: &ChatMessage| estimate_tokens(&m.content) + MESSAGE_OVERHEAD_TOKENS;
    let mut total: usize = messages.iter().map(tokens_of).sum();
    let mut i = 0;
    while total > budget && i < messages.len() {
        if messages[i].role == Role::System {
            i += 1;
            continue;
        }
        // 计算本轮要丢弃的条数：assistant(tool_calls) 连带其后连续 tool 消息。
        let mut drop = 1;
        let mut drop_tokens = tokens_of(&messages[i]);
        if messages[i].role == Role::Assistant && messages[i].tool_calls.is_some() {
            let mut j = i + 1;
            while j < messages.len() && messages[j].role == Role::Tool {
                drop += 1;
                drop_tokens += tokens_of(&messages[j]);
                j += 1;
            }
        }
        // 保护：若丢弃后不再剩任何非 system 消息，停止（保留最后一条用户消息）。
        let remaining_non_system = messages
            .iter()
            .skip(i + drop)
            .filter(|m| m.role != Role::System)
            .count();
        if remaining_non_system == 0 {
            break;
        }
        total = total.saturating_sub(drop_tokens);
        messages.drain(i..i + drop);
    }
}

// ===========================================================================
// 对话历史持久化（独立 JSON 文件，按 domain 分文件）
// ===========================================================================

/// 可序列化的对话（持久化用）。只保留 id/title/messages，不含运行时状态
/// （activeRequestId/sending 重启后恒为 null/false）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializableConversation {
    pub id: String,
    pub title: String,
    pub messages: Vec<Value>,
}

/// 对话历史文件路径：`<app_data>/ai_conversations_<domain>.json`。
fn conversations_path(domain: &str) -> AppResult<std::path::PathBuf> {
    let dir = crate::storage::json_store::app_data_dir()?;
    Ok(dir.join(format!("ai_conversations_{}.json", domain)))
}

/// 读取指定 domain 的对话历史列表。
#[tauri::command]
pub fn ai_list_conversations(domain: String) -> AppResult<Vec<SerializableConversation>> {
    let path = conversations_path(&domain)?;
    crate::storage::json_store::read_json_or_default(&path)
}

/// 全量保存指定 domain 的对话历史（原子写，覆盖旧文件）。
#[tauri::command]
pub fn ai_save_conversations(
    domain: String,
    conversations: Vec<SerializableConversation>,
) -> AppResult<()> {
    let path = conversations_path(&domain)?;
    crate::storage::json_store::write_json(&path, &conversations)
}
