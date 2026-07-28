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
//! 3. 循环直到模型给出纯文本回复（无 tool_calls）或达到 `MAX_ITER` 上限。
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

use crate::ai::provider::{build_provider, ChatMessage};
use crate::ai::tools::{self, ToolApproval, ToolResult};
use crate::config::{settings_load_inner, SshAgentSettings, SqlAgentSettings};
use crate::error::{AppError, AppResult};
use crate::events::{
    self, AiDoneEvent, AiErrorEvent, AiToolCallEvent, AiToolResultEvent, AI_DONE, AI_ERROR,
    AI_TOOL_CALL, AI_TOOL_RESULT,
};
use crate::state::AppState;

/// 工具确认默认超时（5 分钟）。超时视为拒绝。
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
/// 智能体循环最大迭代次数，防止模型陷入工具调用死循环。
const MAX_ITER: usize = 10;

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
    let provider_cfg = settings.ai.active_provider().ok_or_else(|| {
        AppError::InvalidInput("未配置 AI provider，请先在设置中添加".into())
    })?;
    let provider = build_provider(&provider_cfg)?;

    // SSH / SQL 智能体配置：分别克隆一份 move 进 spawned task。
    // SSH 域读 ssh_agent（command_whitelist / auto_approve_safe / terminal_visualization），
    // SQL 域读 sql_agent（sql_mode / auto_approve_safe）。
    let ssh_cfg = settings.ai.ssh_agent.clone();
    let sql_cfg = settings.ai.sql_agent.clone();

    // state 的共享字段都是 Arc，clone 出来 move 进 spawned task。
    let data_dir = state.data_dir.clone();
    let db = state.db.clone();
    let vault = state.vault.clone();
    let terminals = state.terminals.clone();
    let mysql_conns = state.mysql_conns.clone();
    let pending_tool_calls = state.pending_tool_calls.clone();

    // 构造一个独立的 AppState 句柄（共享同一份内部数据）。
    let task_state = AppState {
        data_dir,
        db,
        vault,
        terminals,
        sftp_sessions: state.sftp_sessions.clone(),
        tunnels: state.tunnels.clone(),
        mysql_conns,
        pending_tool_calls,
        // 任务自身不操作 pending_ai_tasks，但 AppState 字段必须完整；与外部共享同一 Arc。
        pending_ai_tasks: state.pending_ai_tasks.clone(),
        settings_path: state.settings_path.clone(),
    };

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
    state
        .pending_ai_tasks
        .lock()
        .insert(request_id, join);

    Ok(())
}

/// 确认执行某个工具调用（前端"批准"按钮触发）。
#[tauri::command]
pub async fn ai_execute_tool(
    tool_call_id: String,
    state: State<'_, AppState>,
) -> AppResult<()> {
    if let Some(tx) = state.pending_tool_calls.lock().remove(&tool_call_id) {
        let _ = tx.send(ToolApproval { approved: true });
    }
    Ok(())
}

/// 取消某个工具调用（前端"拒绝"按钮触发）。
#[tauri::command]
pub async fn ai_cancel_tool(
    tool_call_id: String,
    state: State<'_, AppState>,
) -> AppResult<()> {
    if let Some(tx) = state.pending_tool_calls.lock().remove(&tool_call_id) {
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
/// - `pending_tool_calls`：发拒绝信号给所有阻塞中的工具确认（避免泄漏 oneshot）。
///
/// 注意：abort 不会发射任何 AI 事件，前端需在调用本命令后自行把 sending 置 false
/// （前端也会订阅 ai:stopped 事件作为统一收尾信号）。
#[tauri::command]
pub async fn ai_stop(request_id: String, app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    // 1. 取出并 abort 后台任务。
    if let Some(join) = state.pending_ai_tasks.lock().remove(&request_id) {
        join.abort();
    }
    // 2. 拒绝所有阻塞中的工具确认（防止 oneshot 泄漏 + 让相关 UI 收尾）。
    //    注意：pending_tool_calls 是按 toolCallId 索引，无法直接按 requestId 过滤，
    //    但同一个 AI 请求的工具调用 id 通常带 requestId 前缀或同时只有一个请求进行中。
    //    这里清空全部 pending（MVP 假设单请求场景），更精确的清理需要 toolCall→requestId 映射。
    let pending: Vec<tokio::sync::oneshot::Sender<crate::ai::tools::ToolApproval>> = std::mem::take(
        &mut *state.pending_tool_calls.lock(),
    )
    .into_values()
    .collect();
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

// ===========================================================================
// 智能体编排循环
// ===========================================================================

/// 智能体多轮编排循环。
///
/// 每一轮：调用 `chat_with_tools` → 若有 tool_calls 则逐个走"emit→等待确认→执行
/// →emit 结果"流程 → 把结果回填 → 进入下一轮。直到模型给出无 tool_calls 的纯文本
/// 回复（发射 `ai:done`），或达到 [`MAX_ITER`] 上限（也发射 `ai:done`）。
async fn run_agent_loop(
    app: &AppHandle,
    state: AppState,
    req: AiChatRequest,
    provider: Box<dyn crate::ai::provider::LlmProvider>,
    ssh_cfg: SshAgentSettings,
    sql_cfg: SqlAgentSettings,
) -> AppResult<()> {
    // agent_mode 决定是否传入工具集。false 时 tools 为空，等同普通对话。
    // 工具集按活动上下文裁剪（块A）：有活动终端→SSH 工具；有活动 DB 连接→SQL 工具。
    let tools = if req.agent_mode {
        tools::tools_for_context(
            req.active_terminal_id.as_deref(),
            req.active_db_conn_id.as_deref(),
        )
    } else {
        Vec::new()
    };
    let allowed = tools::allowed_tool_names(&tools);
    let mut messages = req.messages.clone();
    let mut tool_results: Vec<(String, ToolResult)> = Vec::new();
    let request_id = req.request_id.clone();
    let mut last_text = String::new();

    for _iter in 0..MAX_ITER {
        let resp = provider
            .chat_with_tools(
                messages.clone(),
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
            let dangerous = tools::is_dangerous(&call.name, &call.arguments);
            let description = tools::describe_call(&call.name, &call.arguments);

            // === 域分发：按工具名取对应配置 + 计算 whitelisted / auto_run ===
            // exec_ssh：白名单判定 + ssh_cfg.auto_approve_safe 自动放行。
            // exec_sql：先按 sql_mode 校验是否允许（不允许直接拒绝，不执行）；
            //           只读查询（is_readonly_sql）视作"安全"（前端绿色卡片），
            //           配合 sql_cfg.auto_approve_safe 自动放行。
            // 其它工具（terminal_snapshot / list_db_tables / describe_table）：默认安全，
            // 不走确认（auto_run = true，无副作用读操作）。
            let domain = if call.name == "exec_ssh" {
                "ssh"
            } else if call.name == "exec_sql" {
                "sql"
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

            // whitelisted / auto_run：按域计算。
            //   - ssh：命令命中 ssh 白名单即 whitelisted=true（前端绿色卡片）；
            //          auto_run = auto_approve_safe && whitelisted && !dangerous。
            //   - sql：只读查询 whitelisted=true；auto_run = auto_approve_safe && 只读 && !dangerous。
            //   - 其它：无副作用读操作，whitelisted=false 但 auto_run=true（直接执行）。
            let (whitelisted, auto_run) = match domain {
                "ssh" => {
                    let w = call
                        .arguments
                        .get("command")
                        .and_then(Value::as_str)
                        .map(|c| tools::is_whitelisted(c, &ssh_cfg.command_whitelist))
                        .unwrap_or(false);
                    // 危险命令永远走人工确认（即使 auto_approve_safe 开启，防御性）。
                    (w, ssh_cfg.auto_approve_safe && w && !dangerous)
                }
                "sql" => {
                    let readonly = call
                        .arguments
                        .get("sql")
                        .and_then(Value::as_str)
                        .map(tools::is_readonly_sql)
                        .unwrap_or(false);
                    // 危险 SQL（DROP/TRUNCATE/无 WHERE DELETE）永远走人工确认。
                    (readonly, sql_cfg.auto_approve_safe && readonly && !dangerous)
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
                let r = tools::execute_tool(app, &state, call, &allowed, visualization).await;
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
                // 注册 oneshot 等待前端确认。
                let (tx, rx) = tokio::sync::oneshot::channel::<ToolApproval>();
                state.pending_tool_calls.lock().insert(call.id.clone(), tx);

                let approval = tokio::time::timeout(APPROVAL_TIMEOUT, rx).await;
                state.pending_tool_calls.lock().remove(&call.id);

                match approval {
                    Ok(Ok(ToolApproval { approved: true })) => {
                        let r = tools::execute_tool(app, &state, call, &allowed, visualization)
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

    // 达到 MAX_ITER 上限：以最后一段文本收尾。
    log::warn!(
        "[ai:{}] 智能体循环达到 {} 轮上限，强制结束",
        request_id,
        MAX_ITER
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
