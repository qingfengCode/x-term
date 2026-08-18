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
//! 2. 模型若返回 `tool_calls`，先预计算每个调用的执行计划，再**全部**发射
//!    `ai:tool_call` 事件（含危险标记与人类可读描述，前端所有确认卡片同时出现），
//!    确认通道轮初统一登记、按 [`MAX_PARALLEL_TOOL_CALLS`] 限制并发：多个工具
//!    并行等待前端确认并并行执行 → 按调用原顺序发射 `ai:tool_result` → 把结果
//!    以 role=tool 消息回填。
//!    例外：终端可视化开启时 exec_ssh 写的是**共享 PTY**（忙锁 try-acquire，
//!    忙时立即报错），同轮按原顺序**串行**执行（见 [`build_serial_gates`]），
//!    其余调用照常并行。
//! 3. 循环直到模型给出纯文本回复（无 tool_calls）或达到 `max_tool_calls` 上限。
//!
//! `agent_mode == false` 时（翻译/诊断/解释等旧场景）传入空工具集，模型不会
//! 调用工具，行为等价于普通流式对话——但走的是统一的 `chat_with_tools` 通道，
//! 这样 `chat_stream` 可以逐步废弃。
//!
//! 前端通过 [`ai_execute_tool`] / [`ai_cancel_tool`] 把工具确认结果发回。

use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tauri::{AppHandle, State};

use crate::ai::provider::{build_provider, ChatMessage, ChatWithToolsResult, LlmProvider, Role};
use crate::ai::tools::{
    self, DesktopToolOutcome, ToolApproval, ToolDef, ToolResult,
};
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

/// 同一轮 tool_calls 的并发执行上限（借鉴 dsh `maxParallelToolCalls` 的默认 10，
/// 取 6 兼顾"多命令并行提速"与"同时打开的 SSH 连接 / 确认卡片不过量"）。
/// 超过上限的调用排队等待，前序调用结束后立即执行（信号量语义）。
const MAX_PARALLEL_TOOL_CALLS: usize = 6;

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
    /// 当前活动内嵌 RDP 会话的桥接实例 id（桌面助手用，启用 desktop_* 工具）。
    ///
    /// 桌面工具要求激活模型为多模态（否则模型看不懂截图），`ai_chat` 入口会
    /// 对非多模态模型强制置空本字段，从而不下发桌面工具。
    #[serde(default)]
    pub active_desktop_id: Option<String>,
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
    // 该请求域（ssh/db/desktop）是否存在已启用技能：存在时 agent 模式追加
    // load_skill 工具（配合系统提示词里的技能目录摘要按需加载）。
    let req_domain = req.domain.clone().unwrap_or_default();
    let has_skills = settings
        .ai
        .skills
        .iter()
        .any(|s| s.enabled && s.domain == req_domain);
    // 多模态兜底：非多模态模型不发送图片字段。前端发送时已按激活模型过滤，
    // 但旧对话历史 / 其它调用方仍可能带图——文本模型（如 DeepSeek）遇到
    // image_url / image 块会直接 400，这里在入口统一剥离，双保险。
    let mut req = req;
    filter_images_for_model(&mut req.messages, provider_cfg.multimodal);
    // 桌面工具依赖多模态视觉：非多模态模型收到截图会 400，入口直接门控——
    // 不传 active_desktop_id 即不下发 desktop_* 工具（前端系统提示会告知用户）。
    if !provider_cfg.multimodal {
        req.active_desktop_id = None;
    }
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
            has_skills,
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

/// 桌面工具（desktop_*）的前端「批准/拒绝 + 执行结果」一体回执。
///
/// RDP 会话（IronRDP WASM）活在前端，桌面工具的确认与执行都在前端完成：
/// - 用户拒绝：`approved=false`（其余字段被忽略）；
/// - 用户批准：前端先在 RDP 会话上执行，再把结果随 `ok`/`output` 回传；
///   `desktop_screenshot` 额外附带 `image_mime` + `image_base64`（PNG），
///   编排层把它作为图片消息回填给多模态模型。
#[tauri::command]
pub async fn ai_desktop_tool_respond(
    tool_call_id: String,
    approved: bool,
    ok: bool,
    output: String,
    image_mime: Option<String>,
    image_base64: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let outcome = DesktopToolOutcome {
        approved,
        ok,
        output,
        image: match (image_mime, image_base64) {
            (Some(mime), Some(data)) if approved => Some(crate::ai::provider::ImagePart {
                mime_type: mime,
                data_base64: data,
            }),
            _ => None,
        },
    };
    if let Some((_, tx)) = state.pending_desktop_calls.lock().remove(&tool_call_id) {
        let _ = tx.send(outcome);
    }
    Ok(())
}

/// 回传 ask_user_question 的用户回答（前端问题表单「提交/取消」触发）。
///
/// 提问的执行体在**前端**（用户填写回答）：编排循环 emit `ai:tool_call` 后阻塞
/// 在 `pending_ask_user_calls`，本命令把逐题回答回传。`answered=false`（取消）
/// 时其余字段被忽略。
#[tauri::command]
pub async fn ai_ask_user_respond(
    tool_call_id: String,
    answered: bool,
    answers: Vec<crate::ai::tools::AskUserAnswer>,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let outcome = crate::ai::tools::AskUserOutcome { answered, answers };
    if let Some((_, tx)) = state.pending_ask_user_calls.lock().remove(&tool_call_id) {
        let _ = tx.send(outcome);
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
/// - `pending_tool_calls` / `pending_desktop_calls`：给**属于本请求**的阻塞中工具
///   确认/前端执行等待发拒绝信号（按 toolCallId → requestId 映射过滤，
///   不误伤其他并发会话的确认项）。
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
    // 2.5 桌面工具（desktop_*）的前端执行等待同样按 requestId 清理：
    //     发送「拒绝」回执，让编排循环以拒绝结果收尾而不是挂到超时。
    let desktop_ids: Vec<String> = {
        let map = state.pending_desktop_calls.lock();
        map.iter()
            .filter(|(_, (req_id, _))| req_id == &request_id)
            .map(|(id, _)| id.clone())
            .collect()
    };
    let mut desktop_pending = Vec::new();
    {
        let mut map = state.pending_desktop_calls.lock();
        for id in &desktop_ids {
            if let Some((_, tx)) = map.remove(id) {
                desktop_pending.push(tx);
            }
        }
    }
    for tx in desktop_pending {
        let _ = tx.send(DesktopToolOutcome {
            approved: false,
            ok: false,
            output: "请求已终止".into(),
            image: None,
        });
    }
    // 2.6 ask_user_question（向用户提问）的等待同样按 requestId 清理：
    //     发送「未回答」回执，让编排循环以取消结果收尾而不是挂到超时。
    let ask_ids: Vec<String> = {
        let map = state.pending_ask_user_calls.lock();
        map.iter()
            .filter(|(_, (req_id, _))| req_id == &request_id)
            .map(|(id, _)| id.clone())
            .collect()
    };
    let mut ask_pending = Vec::new();
    {
        let mut map = state.pending_ask_user_calls.lock();
        for id in &ask_ids {
            if let Some((_, tx)) = map.remove(id) {
                ask_pending.push(tx);
            }
        }
    }
    for tx in ask_pending {
        let _ = tx.send(crate::ai::tools::AskUserOutcome {
            answered: false,
            answers: Vec::new(),
        });
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

/// 一轮中某个 tool_call 的预计算执行计划。
///
/// 所有决策（域分发 / 危险 / 白名单 / 自动放行 / SQL 模式拦截 / 可视化开关）在
/// 进入并发阶段前一次性算好，便于把同一轮的多个调用**并行**确认与执行
/// （借鉴 dsh `maxParallelToolCalls`）。`rejected` 非 None 的调用（如 SQL 模式
/// 拦截）不进入确认/执行阶段，直接回填错误。
struct PendingCall<'a> {
    call: &'a crate::ai::tools::ToolCall,
    domain: &'static str,
    description: String,
    dangerous: bool,
    whitelisted: bool,
    auto_run: bool,
    visualization: bool,
    /// 同轮需按原顺序串行执行的调用（当前仅可视化模式的 exec_ssh——写共享
    /// PTY，忙锁忙时立即报错，并行会自相冲突）。编排层据此挂链式闸门，
    /// 见 [`build_serial_gates`]。
    serial: bool,
    /// 预拒绝原因：非 None 时该调用不进入批准/执行阶段。
    rejected: Option<String>,
}

impl std::fmt::Debug for PendingCall<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 只为测试断言提供可读输出（call 借用 ToolCall，不展开其内容）。
        f.debug_struct("PendingCall")
            .field("name", &self.call.name)
            .field("domain", &self.domain)
            .field("dangerous", &self.dangerous)
            .field("whitelisted", &self.whitelisted)
            .field("auto_run", &self.auto_run)
            .field("visualization", &self.visualization)
            .field("serial", &self.serial)
            .field("rejected", &self.rejected)
            .finish()
    }
}

/// 一轮中单个 tool_call 的处置结果（预计算阶段产出，与调用**原始顺序**对位）。
///
/// `Rejected`：不进入确认/执行（未 advertised 的幻觉调用 / SQL 模式拦截 /
/// ask_user_question 参数非法），轮末按原顺序回填错误；`Run`：进入
/// 「emit 卡片 → 并发确认/执行」流程。
///
/// 之所以要对位记录而不是"预拒绝的先回填、执行的完成后回填"：role=tool
/// 结果消息必须与 assistant(tool_calls) 里的调用顺序一致（部分兼容网关
/// 会校验 tool 消息与 tool_calls 的对应顺序），分两段回填会把交错的原始
/// 顺序重排成「先全部被拒、后全部执行」，可能 400，且干扰模型对
/// "哪个结果属于哪次调用"的对应关系。
enum RoundItem<'a> {
    Rejected {
        call: &'a crate::ai::tools::ToolCall,
        msg: String,
    },
    Run(PendingCall<'a>),
}

/// 并发执行阶段的确认通道（轮初统一登记，批准可先于该调用的执行批次到达并
/// 缓冲在通道里；轮到它时立即生效）。
enum CallSlot {
    /// 需要前端人工确认的工具调用（批准后再执行）。
    Approval(tokio::sync::oneshot::Receiver<ToolApproval>),
    /// 桌面工具的前端「批准 + 执行」一体回执（执行体在前端 RDP 会话）。
    Desktop(tokio::sync::oneshot::Receiver<DesktopToolOutcome>),
    /// 向用户提问的回执（ask_user_question，执行体在前端表单）。
    AskUser(tokio::sync::oneshot::Receiver<crate::ai::tools::AskUserOutcome>),
}

/// 预计算单个 tool_call 的执行计划（纯函数，便于单元测试锁定安全门控逻辑）。
///
/// 返回 `Err(msg)` 表示该工具未 advertised（不在 `allowed` 内，模型幻觉调用），
/// 调用方应直接回填该错误消息；`Ok(PendingCall)` 表示进入确认/执行阶段，其中
/// `rejected` 非 None 的调用（SQL 模式拦截）只回填错误、不弹确认卡片。
fn plan_call<'a>(
    call: &'a crate::ai::tools::ToolCall,
    allowed: &HashSet<String>,
    ssh_cfg: &SshAgentSettings,
    sql_cfg: &SqlAgentSettings,
    file_workspace: Option<&std::path::Path>,
) -> Result<PendingCall<'a>, String> {
    // 上下文裁剪（块A）：若模型幻觉调用了未 advertised 的工具，直接拒绝并回填。
    if !allowed.contains(&call.name) {
        return Err(format!(
            "工具 `{}` 在当前上下文不可用（未提供活动终端或数据库连接）",
            call.name
        ));
    }
    let dangerous = tools::is_dangerous(&call.name, &call.arguments, file_workspace);
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
    } else if tools::is_desktop_tool(&call.name) {
        "desktop"
    } else if call.name == "todo_write" {
        "todo"
    } else if call.name == "load_skill" {
        "skill"
    } else if call.name == "ask_user_question" {
        "ask"
    } else {
        "other"
    };

    // exec_sql 模式校验：sql_mode 不允许的语句记入 rejected（稍后统一回填
    // 错误，不进入确认/执行流程）。仍计入重复调用守卫计数——模型反复重试
    // 同一被拒调用正是要打断的循环。
    let mut rejected = None;
    if domain == "sql" {
        let sql_text = call
            .arguments
            .get("sql")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !tools::sql_allowed_by_mode(sql_text, &sql_cfg.sql_mode) {
            rejected = Some(format!(
                "当前 SQL 模式为 `{}`，不允许执行该语句（首个关键字被拦截）",
                sql_cfg.sql_mode
            ));
        }
    }
    // ask_user_question 参数校验：问题结构非法（缺 id/文本、id 重复、选项空等）
    // 直接预拒绝——前端不会渲染出畸形表单。
    if domain == "ask" {
        if let Err(e) = tools::validate_ask_user_questions(&call.arguments) {
            rejected = Some(e);
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
        // 桌面工具：截图是纯读取且无副作用（前端收到即自动截图回传）；
        // 点击/输入可能对远端造成副作用，一律人工确认。
        "desktop" => (false, call.name == "desktop_screenshot"),
        // 记账/只读工具（todo_write / load_skill / ask_user_question）：无副作用。
        // ask_user_question 执行体在前端表单（用户必须人工回答），但**不弹确认
        // 卡片**——提问卡片本身就是交互，auto_run=true 仅表示"无需二次确认"。
        "todo" | "skill" | "ask" => (false, true),
        _ => (false, true),
    };

    // 终端可视化模式：按工具域取各自的可视化开关。
    // - exec_ssh：ssh_cfg.terminal_visualization（命令写进 PTY）
    // - exec_sql：sql_cfg.terminal_visualization（SQL + 结果回显到 SQL 控制台）
    // 两者独立设置，互不影响。
    let visualization = match domain {
        "ssh" => ssh_cfg.terminal_visualization,
        "sql" => sql_cfg.terminal_visualization,
        _ => false,
    };

    // 可视化模式的 exec_ssh 写的是**共享 PTY**（exec_ssh_visual 以
    // mcp_terminal_busy try-lock 互斥，忙时立即报错而非排队）：同轮多条命令
    // 若并行执行，除第一条外全部撞锁失败。标记 serial 交由编排层按原顺序
    // 串行执行（见 build_serial_gates）。非可视化模式每次调用开独立 SSH
    // 连接（exec_ssh → connect_direct），无共享状态，保持并行。
    let serial = domain == "ssh" && ssh_cfg.terminal_visualization;

    Ok(PendingCall {
        call,
        domain,
        description,
        dangerous,
        whitelisted,
        auto_run,
        visualization,
        serial,
        rejected,
    })
}

/// 为 serial 调用（同轮需串行执行的调用，当前仅可视化模式的 exec_ssh）构建
/// 链式闸门。
///
/// serial 调用按出现顺序两两成链：前一个**完整结束**（确认等待 + 执行）后
/// send，下一个开始前 await——保证同链调用严格按调用原顺序执行；链与非
/// serial 调用之间、以及不同链之间（当前只有一条链）仍并行。
///
/// 返回 `(wait, signal)`，均与调用下标对位：`wait[i]` 是第 i 个调用的入口
/// 闸门（None 表示无需等待），`signal[i]` 是出口通知（None 表示无人排队）。
/// 非serial 调用两个位置恒为 None；链头只 signal、链尾只 wait。
fn build_serial_gates(
    serial: &[bool],
) -> (
    Vec<Option<tokio::sync::oneshot::Receiver<()>>>,
    Vec<Option<tokio::sync::oneshot::Sender<()>>>,
) {
    let mut wait: Vec<Option<tokio::sync::oneshot::Receiver<()>>> =
        (0..serial.len()).map(|_| None).collect();
    let mut signal: Vec<Option<tokio::sync::oneshot::Sender<()>>> =
        (0..serial.len()).map(|_| None).collect();
    let serial_idx: Vec<usize> = serial
        .iter()
        .enumerate()
        .filter(|(_, s)| **s)
        .map(|(i, _)| i)
        .collect();
    // 相邻 serial 调用（跳过中间穿插的非 serial 调用）两两连接。
    for pair in serial_idx.windows(2) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        signal[pair[0]] = Some(tx);
        wait[pair[1]] = Some(rx);
    }
    (wait, signal)
}

/// 智能体多轮编排循环。
///
/// 每一轮：调用 `chat_with_tools` → 若有 tool_calls 则预计算执行计划 → 被拦截的
/// 调用（未 advertised 工具 / SQL 模式拒绝）直接回填 → 其余调用**全部先 emit
/// `ai:tool_call`**（前端所有确认卡片同时出现）→ 确认通道轮初统一登记，按
/// [`MAX_PARALLEL_TOOL_CALLS`] 限制并发「等待确认 + 执行」→ 按调用原顺序 emit
/// `ai:tool_result` 并回填 → 进入下一轮。直到模型给出无 tool_calls 的纯文本回复
/// （发射 `ai:done`），或达到 `max_tool_calls` 上限（也发射 `ai:done`）。
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
    has_skills: bool,
    max_tool_calls: usize,
    context_budget: usize,
) -> AppResult<()> {
    // agent_mode 决定是否传入工具集。false 时 tools 为空，等同普通对话。
    // 工具集按活动上下文裁剪（块A）：有活动终端→SSH 工具；有活动 DB 连接→SQL 工具；
    // 有活动内嵌 RDP 会话→桌面工具（多模态门控在 ai_chat 入口完成）；
    // 设置页开启「本地文件读写」→ 对 ssh/db 域追加文件工具（read_file/write_file/
    // list_files；桌面助手不暴露文件工具，保持工具集按域硬隔离）。
    // 另外几个**始终下发**的工具（不依赖活动上下文）：
    // - todo_write：任务清单记账（借鉴 dsh tool-todo，agent 模式无条件可用）；
    // - load_skill：该域存在启用技能时下发（技能正文按需加载，见 skill_tool）；
    // - ask_user_question：信息不足时向用户提问（借鉴 dsh tool-ask-user，
    //   执行体在前端表单，agent 模式无条件可用）。
    let req_domain = req.domain.clone().unwrap_or_default();
    let mut tools = Vec::new();
    if req.agent_mode {
        tools.extend(tools::tools_for_context(
            req.active_terminal_id.as_deref(),
            req.active_db_conn_id.as_deref(),
            req.active_desktop_id.as_deref(),
        ));
        if file_cfg.enabled && (req_domain == "ssh" || req_domain == "db") {
            tools.extend(tools::file_tools());
        }
        tools.push(tools::todo_tool());
        tools.push(tools::ask_user_tool());
        if has_skills {
            tools.push(tools::skill_tool());
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
    // context_budget 在上下文超限降级重试中会被下调（降至 2/3），因此这里取可变副本。
    let mut context_budget = context_budget;
    let mut tool_results: Vec<(String, ToolResult)> = Vec::new();
    let request_id = req.request_id.clone();
    let mut last_text = String::new();
    // 重复工具调用守卫（借鉴 dsh repeat-tool-reminder）：跟踪连续相同调用，
    // 达到阈值时向模型注入提醒，打断死循环。
    let mut repeat_guard = RepeatCallGuard::new();

    for _iter in 0..max_tool_calls {
        // 调用 chat_with_tools，带有限重试（429/5xx/建连失败指数退避；上下文超限
        // 400 降级：预算降至 2/3 + 摘要化旧轮次工具结果），见
        // [`chat_with_tools_with_retry`]。裁剪只影响本轮发送，不影响 messages 本身
        // （降级重试会把摘要化直接写入 messages，那是刻意为之）。
        let resp = chat_with_tools_with_retry(
            provider.as_ref(),
            &mut messages,
            &tools,
            &tool_results,
            &request_id,
            app,
            &mut context_budget,
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
            images: None,
        });

        // 一轮工具调用分四段处理：① 预计算每个调用的处置（同步，预拒绝只记录
        // 不回填）→ ② Run 项先全部 emit ai:tool_call（前端所有确认卡片同时
        // 出现）→ ③ 并发「等待确认 + 执行」（上限 MAX_PARALLEL_TOOL_CALLS，
        // 确认通道轮初统一登记，批准先到先缓冲）→ ④ **按调用原始顺序**统一
        // emit ai:tool_result 并回填（预拒绝的与执行完成的对位交织，见
        // RoundItem 的顺序说明）。
        // 关键：每个 tool_call 的结果必须以 role=tool 消息紧跟在 assistant(tool_calls)
        // 之后追加进 messages，否则 OpenAI 兼容协议会以 400 拒绝
        // （"assistant with tool_calls must be followed by tool messages"）。
        tool_results.clear();
        // 本轮触发的重复调用提醒（轮末统一注入，避免插在 tool 消息序列中间）。
        let mut reminders: Vec<String> = Vec::new();

        // ---- ① 预计算处置（同步，不 await；预拒绝只记录，轮末统一回填）----
        let mut items: Vec<RoundItem> = Vec::with_capacity(resp.tool_calls.len());
        for call in &resp.tool_calls {
            // 重复调用守卫：先观察（被拒绝/被拦截的调用同样计数——模型反复重试
            // 同一被拒调用正是要打断的循环，与 dsh 的 denied-calls-count 一致）。
            if let Some(reminder) = repeat_guard.observe(&call.name, &call.arguments) {
                reminders.push(reminder);
            }
            // plan_call：allowed 校验（幻觉调用直接拒绝）+ 域分发 / SQL 模式拦截 /
            // 白名单与运行模式判定（纯函数，决策逻辑独立于并发编排，见单元测试）。
            match plan_call(call, &allowed, &ssh_cfg, &sql_cfg, file_workspace.as_deref()) {
                // 上下文裁剪（块A）：未 advertised 的工具，记为预拒绝。
                Err(msg) => items.push(RoundItem::Rejected { call, msg }),
                Ok(pc) => match pc.rejected.clone() {
                    // SQL 模式拦截 / ask_user 参数非法：同为预拒绝。
                    Some(msg) => items.push(RoundItem::Rejected { call: pc.call, msg }),
                    None => items.push(RoundItem::Run(pc)),
                },
            }
        }

        // ---- ② 全部先 emit ai:tool_call（前端所有确认卡片同时出现）。----
        let active: Vec<&PendingCall<'_>> = items
            .iter()
            .filter_map(|it| match it {
                RoundItem::Run(pc) => Some(pc),
                _ => None,
            })
            .collect();
        for pc in &active {
            events::emit(
                app,
                AI_TOOL_CALL,
                AiToolCallEvent {
                    request_id: request_id.clone(),
                    tool_call_id: pc.call.id.clone(),
                    name: pc.call.name.clone(),
                    arguments: pc.call.arguments.to_string(),
                    description: pc.description.clone(),
                    dangerous: pc.dangerous,
                    whitelisted: pc.whitelisted,
                    auto_approved: pc.auto_run,
                    // 桌面工具：把请求发起时的活动桌面 id 带给前端执行器（防 agent
                    // 循环期间用户切换标签导致操作落到错误桌面）。
                    desktop_id: if pc.domain == "desktop" {
                        req.active_desktop_id.clone()
                    } else {
                        None
                    },
                },
            );
        }
        // ---- ③ 并发「等待确认 + 执行」----
        // 先为本轮**全部**调用预创建确认通道并登记 sender：用户可在任意时刻批准
        // 任意卡片（包括还没轮到执行批次的），批准会缓冲在通道里，轮到该调用时
        // 立即生效——否则在别的调用执行期间批准后面卡片的操作会被 ai_execute_tool
        // 静默丢弃（sender 尚未登记），5 分钟后白白超时。
        //
        // 例外：serial 调用（可视化模式的 exec_ssh）写共享 PTY，忙锁是 try-acquire
        // 语义、忙时立即报错——同轮并行会自相冲突（第一条之外的命令全部"终端被
        // 占用"）。这些调用按原顺序串行执行（链式闸门见 build_serial_gates），
        // 其余调用照常并行。
        let mut slots: Vec<Option<CallSlot>> = Vec::with_capacity(active.len());
        for pc in &active {
            let slot = if pc.domain == "desktop" {
                // 桌面工具：执行体在**前端**（IronRDP WASM 会话，后端桥接只透传
                // 字节）。无论 auto_run（截图自动放行）与否都要等前端「批准 +
                // 执行」一体回执——auto_approved 只决定前端是否弹确认卡片。
                let (tx, rx) = tokio::sync::oneshot::channel::<DesktopToolOutcome>();
                state
                    .pending_desktop_calls
                    .lock()
                    .insert(pc.call.id.clone(), (request_id.clone(), tx));
                Some(CallSlot::Desktop(rx))
            } else if pc.domain == "ask" {
                // 提问（ask_user_question）：登记 oneshot 等待前端回答——执行体在
                // 前端表单（用户必须人工回答），auto_run 只表示不弹二次确认卡片。
                let (tx, rx) = tokio::sync::oneshot::channel::<crate::ai::tools::AskUserOutcome>();
                state
                    .pending_ask_user_calls
                    .lock()
                    .insert(pc.call.id.clone(), (request_id.clone(), tx));
                Some(CallSlot::AskUser(rx))
            } else if !pc.auto_run {
                // 注册 oneshot 等待前端确认（带上 requestId，供 ai_stop 精确清理）。
                let (tx, rx) = tokio::sync::oneshot::channel::<ToolApproval>();
                state
                    .pending_tool_calls
                    .lock()
                    .insert(pc.call.id.clone(), (request_id.clone(), tx));
                Some(CallSlot::Approval(rx))
            } else {
                // 自动放行：不等待人工确认，直接执行（无需通道）。
                None
            };
            slots.push(slot);
        }
        // 所有调用的确认/执行共享同一截止时间（轮起点 + APPROVAL_TIMEOUT），
        // 避免排在并发队列后面的调用额外获得更长的等待窗口。
        let deadline = tokio::time::Instant::now() + APPROVAL_TIMEOUT;
        // 并发上限：同一时刻最多 MAX_PARALLEL_TOOL_CALLS 个调用在等待确认/执行
        // （防止大批调用同时打开 SSH 连接 / 确认卡片积压）。
        let semaphore =
            std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_PARALLEL_TOOL_CALLS));
        // 串行闸门：serial 调用按出现顺序两两成链（链头无等待、链尾无通知）。
        let (mut wait_gates, mut signal_gates) =
            build_serial_gates(&active.iter().map(|pc| pc.serial).collect::<Vec<bool>>());
        let futures = active.iter().enumerate().map(|(i, pc)| {
            let request_id = request_id.clone();
            let app = app.clone();
            let state = state.clone();
            let allowed = allowed.clone();
            let semaphore = std::sync::Arc::clone(&semaphore);
            let slot = slots[i].take();
            let wait_gate = wait_gates[i].take();
            let signal_gate = signal_gates[i].take();
            let serial = pc.serial;
            async move {
                // 串行闸门：等链上上一个调用完整结束（确认 + 执行）再开始。前序
                // future 被整体取消（ai_stop abort）时 sender 随之 drop，await
                // 立即返回 Err 放行，不会死锁。
                if let Some(gate) = wait_gate {
                    let _ = gate.await;
                }
                // 串行链上的调用从「闸门打开时」起算独立的确认/执行窗口，与旧
                // 串行实现语义一致——否则排在前面的命令会把后面调用的窗口挤占
                // 掉（共享截止时间下，链条越靠后可用时间越少）。并行调用仍用
                // 轮起点的共享截止。
                let deadline = if serial {
                    tokio::time::Instant::now() + APPROVAL_TIMEOUT
                } else {
                    deadline
                };
                // 等待并发名额（permit 持有到本调用结束）。闸门在名额之前：
                // 串行链同一时刻至多占用一个名额，不挤占并行调用。
                let _permit = semaphore.acquire().await.map_err(|_| ()).ok();
                let mut screenshot_image: Option<crate::ai::provider::ImagePart> = None;
                let result = match slot {
                    Some(CallSlot::Desktop(rx)) => {
                        let outcome = tokio::time::timeout_at(deadline, rx).await;
                        state.pending_desktop_calls.lock().remove(&pc.call.id);
                        match outcome {
                            Ok(Ok(out)) if out.approved => {
                                // 成功回传的截图：协议规定 role=tool 消息不能携带
                                // 图片，由组装阶段以 user 消息图片块回填。
                                screenshot_image = out.image;
                                ToolResult {
                                    ok: out.ok,
                                    output: out.output,
                                }
                            }
                            _ => {
                                // 用户拒绝 / 前端未回执（会话关闭等）/ 确认超时。
                                ToolResult {
                                    ok: false,
                                    output: "用户拒绝了该操作或确认超时".into(),
                                }
                            }
                        }
                    }
                    Some(CallSlot::AskUser(rx)) => {
                        let outcome = tokio::time::timeout_at(deadline, rx).await;
                        state.pending_ask_user_calls.lock().remove(&pc.call.id);
                        match outcome {
                            Ok(Ok(out)) if out.answered => {
                                // 用户的逐题回答（JSON），作为 tool 结果回填给模型。
                                ToolResult {
                                    ok: true,
                                    output: crate::ai::tools::format_ask_user_answers(
                                        &out.answers,
                                    ),
                                }
                            }
                            _ => {
                                // 用户取消 / 请求被终止 / 确认超时。
                                ToolResult {
                                    ok: false,
                                    output: "用户取消了提问或确认超时".into(),
                                }
                            }
                        }
                    }
                    Some(CallSlot::Approval(rx)) => {
                        let approval = tokio::time::timeout_at(deadline, rx).await;
                        state.pending_tool_calls.lock().remove(&pc.call.id);
                        match approval {
                            Ok(Ok(ToolApproval { approved: true })) => {
                                tools::execute_tool(
                                    &app,
                                    &state,
                                    pc.call,
                                    &allowed,
                                    pc.visualization,
                                    file_domain,
                                    &request_id,
                                )
                                .await
                            }
                            _ => {
                                // 拒绝或超时。
                                ToolResult {
                                    ok: false,
                                    output: "用户拒绝了该操作或确认超时".into(),
                                }
                            }
                        }
                    }
                    // 自动放行：不等待人工确认，直接执行。
                    None => {
                        tools::execute_tool(
                            &app,
                            &state,
                            pc.call,
                            &allowed,
                            pc.visualization,
                            file_domain,
                            &request_id,
                        )
                        .await
                    }
                };
                // 放行链上下一个调用（本调用的确认/执行已完整结束；结果 emit 由
                // 轮末按原顺序统一处理，与此处无关）。
                if let Some(tx) = signal_gate {
                    let _ = tx.send(());
                }
                (pc.call.id.clone(), result, screenshot_image)
            }
        });
        let outcomes: Vec<(
            String,
            ToolResult,
            Option<crate::ai::provider::ImagePart>,
        )> = futures::future::join_all(futures).await;

        // ---- ④ 按调用原始顺序统一 emit 结果 + 回填 ----
        // 协议要求 role=tool 消息紧跟 assistant(tool_calls) 且顺序与 tool_calls
        // 一致（部分兼容网关会校验该对应关系，乱序可能 400）。预拒绝
        // （Rejected）与执行完成（Run，outcomes 按 active 顺序对位）在这里按
        // items 的原始顺序交织输出。
        let mut outcome_iter = outcomes.into_iter();
        for item in &items {
            let (call_id, result, screenshot_image) = match item {
                RoundItem::Rejected { call, msg } => (
                    call.id.clone(),
                    ToolResult {
                        ok: false,
                        output: msg.clone(),
                    },
                    None,
                ),
                // Run 项的结果已在 ③ 并发执行完成（outcomes 按 active 顺序对位）。
                RoundItem::Run(_) => outcome_iter
                    .next()
                    .expect("outcomes 与 active 数量一致（同一来源过滤）"),
            };
            events::emit(
                app,
                AI_TOOL_RESULT,
                AiToolResultEvent {
                    request_id: request_id.clone(),
                    tool_call_id: call_id.clone(),
                    ok: result.ok,
                    output: result.output.clone(),
                },
            );
            tool_results.push((call_id.clone(), result.clone()));
            // 把工具结果作为 role=tool 消息追加进 messages（紧跟
            // assistant(tool_calls)）。入上下文前压缩到 8 KiB：完整内容已通过
            // ai:tool_result 事件推给前端展示，模型只需要足够理解结果的头部，
            // 全量原文每轮重发会让成本复利放大。
            messages.push(ChatMessage {
                role: crate::ai::provider::Role::Tool,
                content: compress_tool_output_for_context(&result.output),
                tool_calls: None,
                tool_call_id: Some(call_id),
                images: None,
            });
            // 截图回填：role=tool 消息不允许携带图片（image_url 块只能出现在
            // user 消息里），因此把 desktop_screenshot 捕获的
            // 截图作为紧随其后的 user 消息图片块交给多模态模型查看。
            if let Some(img) = screenshot_image {
                messages.push(ChatMessage {
                    role: Role::User,
                    content: "（这是刚才 desktop_screenshot 工具捕获的 RDP 桌面截图，\
请据此分析当前界面并继续下一步操作。）"
                        .into(),
                    tool_calls: None,
                    tool_call_id: None,
                    images: Some(vec![img]),
                });
            }
        }
        // 轮末注入重复调用提醒：作为 user 消息追加在 tool 结果序列之后（不破坏
        // assistant(tool_calls) → tool 消息的协议顺序），下一轮模型即可看到；
        // 同时 emit 一份给前端（灰色提示渲染），用户能感知模型在空转/已被纠偏。
        for reminder in &reminders {
            log::warn!("[ai:{request_id}] {reminder}");
            events::emit(
                app,
                events::AI_SYSTEM_NOTE,
                events::AiSystemNoteEvent {
                    request_id: request_id.clone(),
                    text: reminder.clone(),
                },
            );
            messages.push(ChatMessage::new(Role::User, reminder.clone()));
        }
        // 早于最近 KEEP_FULL_TOOL_ROUNDS 轮的工具结果替换为一行摘要，从根源上
        // 削减「历史工具输出每轮全量重推」的上下文体积（对下一轮及后续都生效）。
        summarize_old_tool_rounds(&mut messages, KEEP_FULL_TOOL_ROUNDS);
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
// LLM 调用重试
// ===========================================================================

/// 单次 `chat_with_tools` 失败的重试类别。
enum ChatErrorKind {
    /// 可重试：429 / 5xx 或建连失败（请求未发出/未收到响应头）。
    /// 流式传输中途失败**不**在此列——部分 chunk 已推给前端，重试会重复文本。
    Retryable,
    /// 上下文超限（400 且响应体含上下文相关关键词）：降级重试一次。
    ContextTooLong,
    /// 不可重试：立即终止。
    Fatal,
}

/// 按 provider 的错误文案分类（AppError::Ai 只有字符串，没有结构化状态码，
/// 只能按已知格式匹配）。
fn classify_chat_error(msg: &str) -> ChatErrorKind {
    let lower = msg.to_ascii_lowercase();
    // HTTP 429 / 5xx（"LLM 返回错误状态 {status}: ..."）。
    if lower.contains("错误状态 429") || lower.contains("错误状态 5") {
        return ChatErrorKind::Retryable;
    }
    // 400 + 上下文关键词 → 上下文超限（OpenAI "maximum context length"、
    // DeepSeek "超出长度" 等文案都覆盖）。
    if lower.contains("错误状态 400") && is_context_error_body(&lower) {
        return ChatErrorKind::ContextTooLong;
    }
    // 建连失败（预流式）：DNS/连接/超时等，重试通常有效。
    if lower.contains("连接 llm 服务失败") {
        return ChatErrorKind::Retryable;
    }
    ChatErrorKind::Fatal
}

/// 400 响应体是否指向上下文超限（关键词小写匹配，宁宽勿漏：误判最多导致
/// 一次无害的降级重试）。
fn is_context_error_body(lower_body: &str) -> bool {
    const KEYWORDS: [&str; 9] = [
        "context",
        "too long",
        "too large",
        "maximum",
        "length",
        "tokens",
        "输入",
        "长度",
        "超限",
    ];
    KEYWORDS.iter().any(|k| lower_body.contains(k))
}

/// 调用一次 `chat_with_tools`，带有限重试：
/// - 429/5xx/建连失败：最多再试 2 次，指数退避（1.5s → 3s）；
/// - 上下文超限 400：预算降至 2/3 并把早于最近一轮的工具结果摘要化（就地修改
///   `messages`，对后续所有轮次生效——这是裁剪保护下限之外唯一能真正缩容的手段），
///   然后重试一次；
/// - 其它错误：直接返回。
///
/// 重试中途不发射 `ai:error`：`chat_with_tools` 本身不再 emit（见 provider 文档），
/// 最终失败由 `ai_chat` 的任务收尾统一发射一次。否则前端收到中间错误会立刻置为
/// 终态（删除 requestToCid），重试成功后的 chunk/done 将无处路由、回复凭空消失。
async fn chat_with_tools_with_retry(
    provider: &dyn LlmProvider,
    messages: &mut Vec<ChatMessage>,
    tools: &[ToolDef],
    tool_results: &[(String, ToolResult)],
    request_id: &str,
    app: &AppHandle,
    context_budget: &mut usize,
) -> AppResult<ChatWithToolsResult> {
    /// 最大尝试次数（首次 + 2 次重试）。
    const MAX_ATTEMPTS: usize = 3;
    /// 重试退避基数（1.5s、3s，最多重试 2 次）。
    const BACKOFF_BASE_MS: u64 = 1500;

    let mut degraded = false;
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        let mut round = messages.clone();
        trim_history_for_context(&mut round, *context_budget);
        match provider
            .chat_with_tools(
                round,
                tools.to_vec(),
                tool_results.to_vec(),
                request_id.to_string(),
                app.clone(),
            )
            .await
        {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                let msg = e.to_string();
                match classify_chat_error(&msg) {
                    ChatErrorKind::Retryable if attempt < MAX_ATTEMPTS => {
                        let delay = Duration::from_millis(BACKOFF_BASE_MS << (attempt - 1));
                        log::warn!(
                            "[ai:{request_id}] 第 {attempt} 次调用失败，{delay:?} 后重试: {msg}"
                        );
                        tokio::time::sleep(delay).await;
                    }
                    ChatErrorKind::ContextTooLong if !degraded => {
                        degraded = true;
                        *context_budget = context_budget.saturating_mul(2) / 3;
                        summarize_old_tool_rounds(messages, 1);
                        log::warn!(
                            "[ai:{request_id}] 上下文超限（{msg}），预算降至 {} 并摘要化旧轮次工具结果后重试",
                            context_budget
                        );
                    }
                    _ => return Err(e),
                }
            }
        }
    }
}

// ===========================================================================
// 工具结果上下文压缩
// ===========================================================================

/// 单条工具结果进入模型上下文的最大字节数（8 KiB ≈ 2k tokens）。
/// 完整内容仍通过 `ai:tool_result` 事件原样推给前端展示，这里只压缩进入
/// 上下文的副本——否则每条 16 KiB 原文 × 每轮全量重发 × max_tool_calls=200
/// 会让 token 成本复利放大。
const TOOL_OUTPUT_CONTEXT_CAP_BYTES: usize = 8 * 1024;

/// 最近多少轮工具调用的结果保留全文；更早轮次的结果替换为一行摘要。
const KEEP_FULL_TOOL_ROUNDS: usize = 5;

/// 旧轮次工具结果被摘要化后的占位前缀（幂等判断用）。
const TOOL_SUMMARY_PREFIX: &str = "[早前轮次工具结果已省略";

/// 工具结果入上下文前的截断：超过 [`TOOL_OUTPUT_CONTEXT_CAP_BYTES`] 时只保留
/// 头部并附截断说明（按 UTF-8 字符边界切分，避免截出乱码）。
fn compress_tool_output_for_context(output: &str) -> String {
    if output.len() <= TOOL_OUTPUT_CONTEXT_CAP_BYTES {
        return output.to_string();
    }
    let mut cut = TOOL_OUTPUT_CONTEXT_CAP_BYTES;
    while !output.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n…[输出过长已截断：原文 {} 字节，完整内容见对话界面的工具卡片]",
        &output[..cut],
        output.len()
    )
}

/// 把早于最近 `keep_rounds` 轮的工具结果替换为一行摘要（就地修改）。
///
/// 从末尾往前数 assistant(tool_calls) 消息作为「轮次」：每遇一组，其后所属的
/// tool 消息若所在轮次早于保留范围，内容换成 `[早前轮次工具结果已省略：<工具名>，
/// 原文 N 字节]`。摘要带工具名，帮助模型理解被省略的上下文。幂等：已摘要化的
/// 内容（前缀匹配）不会重复处理。
fn summarize_old_tool_rounds(messages: &mut [ChatMessage], keep_rounds: usize) {
    // 先收集 tool_call_id → 工具名。
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in messages.iter() {
        if let Some(tcs) = &m.tool_calls {
            for tc in tcs {
                names.insert(tc.id.clone(), tc.name.clone());
            }
        }
    }
    let mut round = 0usize;
    for m in messages.iter_mut().rev() {
        if m.role == Role::Assistant && m.tool_calls.is_some() {
            round += 1;
            continue;
        }
        if m.role != Role::Tool {
            continue;
        }
        if round < keep_rounds || m.content.starts_with(TOOL_SUMMARY_PREFIX) {
            continue;
        }
        let name = m
            .tool_call_id
            .as_deref()
            .and_then(|id| names.get(id))
            .map(String::as_str)
            .unwrap_or("");
        let bytes = m.content.len();
        m.content = if name.is_empty() {
            format!("{TOOL_SUMMARY_PREFIX}（原文 {bytes} 字节）")
        } else {
            format!("{TOOL_SUMMARY_PREFIX}：{name}，原文 {bytes} 字节")
        };
    }
}

// ===========================================================================
// 上下文窗口裁剪
// ===========================================================================

/// 单条消息除正文外的固定协议开销（role、分隔符等），估算时计入。
const MESSAGE_OVERHEAD_TOKENS: usize = 8;
/// 每张图片的 token 估算值（与分辨率相关，取主流视觉模型的中间值）。
const IMAGE_TOKENS: usize = 1_000;

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
///   否则会残留"孤儿 tool 消息"——OpenAI 兼容协议要求 tool 消息必须跟在
///   带 tool_calls 的 assistant 消息之后，否则以 400 拒绝；
/// - 紧跟 assistant(tool_calls) 组之前的 user 消息不可单独丢弃：若丢，裁剪后首条
///   非 system 消息会变成 assistant(tool_calls)——协议同样以 400 拒绝，对话永久失败；
/// - 至少保留最后一条非 system 消息（当前用户问题不能被裁掉）。
fn trim_history_for_context(messages: &mut Vec<ChatMessage>, budget: usize) {
    let tokens_of = |m: &ChatMessage| {
        // tool_calls 的序列化 JSON（工具名 + 参数）同样占上下文，必须计入预算，
        // 否则参数很长的调用会让估算严重偏低、超限 400。
        let tool_calls_chars: usize = m
            .tool_calls
            .as_ref()
            .map(|tcs| {
                serde_json::to_string(tcs)
                    .map(|s| s.len())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        estimate_tokens(&m.content)
            + tool_calls_chars / 4
            + MESSAGE_OVERHEAD_TOKENS
            + m.images
                .as_ref()
                .map(|imgs| imgs.len() * IMAGE_TOKENS)
                .unwrap_or(0)
    };
    let mut total: usize = messages.iter().map(tokens_of).sum();
    let mut i = 0;
    while total > budget && i < messages.len() {
        if messages[i].role == Role::System {
            i += 1;
            continue;
        }
        // 保护：紧跟 assistant(tool_calls) 组之前的 user 消息与组视为不可分割，
        // 整体跳过（组本身在下一轮迭代按「连带 tool 结果」规则整组丢弃）。
        if messages[i].role == Role::User
            && i + 1 < messages.len()
            && messages[i + 1].role == Role::Assistant
            && messages[i + 1].tool_calls.is_some()
        {
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
// 重复工具调用守卫（借鉴 dsh repeat-tool-reminder）
// ===========================================================================

/// 重复工具调用守卫。
///
/// 观察 agent 循环里的工具调用流：以 `(工具名, 规范化参数)` 为链条键统计**连续**
/// 相同调用次数，达到阈值时生成提醒文本（由编排层作为 user 消息注入 messages，
/// 下一轮模型可见）。是建议性的循环打断器——不拦截、不改写任何调用，决定权
/// 始终在模型：它可以选择换一种方式重试、收集更多信息或直接给出结论。
///
/// 与 dsh 的语义对齐：
/// - 规范化参数：serde_json 默认 `Map` 是 `BTreeMap`，`to_string` 输出天然按键
///   排序——参数对象仅属性顺序不同的调用视为相同；
/// - 排除工具（记账/加载类）既不累计也不重置链条：`grep X → todo_write → grep X`
///   仍算两次连续 `grep X`；
/// - 被拒绝/被拦截的调用同样计数（模型反复重试同一被拒调用正是要打断的循环）；
/// - 按请求实例隔离（每次 `run_agent_loop` 新建），无跨请求状态。
struct RepeatCallGuard {
    /// 上一次跟踪的调用键 `(工具名, 规范化参数)`。
    last_key: Option<(String, String)>,
    /// 连续相同调用次数。
    consecutive: usize,
    /// 已触发的阈值（每个阈值只提醒一次）。
    fired: HashSet<usize>,
}

impl RepeatCallGuard {
    /// 触发提醒的连续次数阈值（升序）。
    const THRESHOLDS: [usize; 3] = [3, 5, 8];
    /// 不参与链条跟踪的工具：记账/加载类调用穿插在循环里不该"洗白"链条，
    /// 也不该自己触发提醒（见 dsh 的 exclude 语义）。
    const EXCLUDED: [&'static str; 2] = ["todo_write", "load_skill"];
    /// 详细提醒里引用的参数预览上限（字符）。
    const ARGS_PREVIEW_CHARS: usize = 200;

    fn new() -> Self {
        Self {
            last_key: None,
            consecutive: 0,
            fired: HashSet::new(),
        }
    }

    /// 观察一次工具调用；命中阈值返回提醒文本，否则返回 None。
    fn observe(&mut self, name: &str, arguments: &serde_json::Value) -> Option<String> {
        if Self::EXCLUDED.contains(&name) {
            return None;
        }
        let key = (name.to_string(), arguments.to_string());
        if self.last_key.as_ref() == Some(&key) {
            self.consecutive += 1;
        } else {
            // 链条重置：新的一串连续调用是新的循环（值得再次在阈值处提醒），
            // 因此同时清空已触发的阈值集合。
            self.last_key = Some(key);
            self.consecutive = 1;
            self.fired.clear();
        }
        // 阈值按连续次数触发，且每个阈值在一串连续调用内只提醒一次（避免每轮都啰嗦）。
        let threshold = Self::THRESHOLDS.iter().copied().find(|&t| t == self.consecutive)?;
        if !self.fired.insert(self.consecutive) {
            return None;
        }
        Some(self.build_reminder(name, arguments, self.consecutive, threshold))
    }

    /// 生成提醒文本。首个阈值（3 次）用简短通用提醒；后续阈值带工具名、
    /// 连续次数与参数预览（头截断，防止循环的巨型参数骑进提醒文本）。
    fn build_reminder(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        count: usize,
        threshold: usize,
    ) -> String {
        if threshold == Self::THRESHOLDS[0] {
            return format!(
                "[重复调用提醒] 你已连续 {count} 次调用工具 `{name}` 且参数完全相同，\
但结果并未改变。请停止重复相同调用：重新阅读上一次的工具结果，\
要么改变策略后再调用，要么直接基于已有信息给出结论。"
            );
        }
        let preview = arguments.to_string();
        let preview = if preview.chars().count() > Self::ARGS_PREVIEW_CHARS {
            let cut: String = preview.chars().take(Self::ARGS_PREVIEW_CHARS).collect();
            format!("{cut}…（参数过长，已省略 {} 字符）", preview.chars().count() - Self::ARGS_PREVIEW_CHARS)
        } else {
            preview
        };
        format!(
            "[重复调用提醒] 你已连续 {count} 次调用工具 `{name}` 且参数完全相同：{preview}\n\
重复相同调用不会得到新信息。请重新阅读上一次的工具结果，换一种方式（调整参数、\
拆分问题或换工具），或直接基于已有信息给出结论。"
        )
    }
}

// ===========================================================================
// 对话历史持久化（独立 JSON 文件，按 domain 分文件）
// ===========================================================================

/// 可序列化的对话（持久化用）。只保留 id/title/messages/todos，不含运行时状态
/// （activeRequestId/sending 重启后恒为 null/false）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializableConversation {
    pub id: String,
    pub title: String,
    pub messages: Vec<Value>,
    /// 智能体任务清单（todo_write 维护；旧文件无此字段 → 默认空）。
    #[serde(default)]
    pub todos: Vec<crate::events::AiTodoItem>,
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

/// 非多模态模型不发送图片字段：把 `messages` 里的图片全部剥离。
///
/// 多模态开关在模型配置上，普通模型（如 DeepSeek 等 OpenAI 兼容文本模型）
/// 不识别 `image_url` / `image` 块，带上会直接 400。前端发送时已按激活模型
/// 过滤，此函数在 `ai_chat` 入口兜底（旧对话历史或其它调用方仍可能带图）。
fn filter_images_for_model(messages: &mut [ChatMessage], multimodal: bool) {
    if !multimodal {
        for m in messages {
            m.images = None;
        }
    }
}

// ===========================================================================
// 单元测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::provider::{ChatMessage, ImagePart, Role};
    use crate::ai::tools::ToolCall;

    fn msg_with_image() -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: "看图".into(),
            tool_calls: None,
            tool_call_id: None,
            images: Some(vec![ImagePart {
                mime_type: "image/png".into(),
                data_base64: "AAAA".into(),
            }]),
        }
    }

    fn tool_msg(id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Tool,
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(id.into()),
            images: None,
        }
    }

    fn assistant_tool_calls(ids: &[&str]) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: Some(
                ids.iter()
                    .map(|id| ToolCall {
                        id: id.to_string(),
                        name: "exec_ssh".into(),
                        arguments: serde_json::json!({}),
                    })
                    .collect(),
            ),
            tool_call_id: None,
            images: None,
        }
    }

    /// 非多模态模型：剥离所有消息的图片字段，文本内容不受影响。
    #[test]
    fn strips_images_for_non_multimodal() {
        let mut messages = vec![msg_with_image(), ChatMessage::new(Role::User, "第二问")];
        filter_images_for_model(&mut messages, false);
        assert!(messages.iter().all(|m| m.images.is_none()));
        assert_eq!(messages[0].content, "看图");
    }

    /// 多模态模型：图片字段原样保留。
    #[test]
    fn keeps_images_for_multimodal() {
        let mut messages = vec![msg_with_image()];
        filter_images_for_model(&mut messages, true);
        assert!(messages[0].images.is_some());
    }

    /// 短输出原样保留；超长输出截断并附说明。
    #[test]
    fn compress_short_and_truncate_long_tool_output() {
        let short = "free -h 输出很短".to_string();
        assert_eq!(compress_tool_output_for_context(&short), short);

        let long = "x".repeat(TOOL_OUTPUT_CONTEXT_CAP_BYTES * 2);
        let compressed = compress_tool_output_for_context(&long);
        assert!(compressed.contains("已截断"));
        assert!(compressed.contains(&format!("原文 {} 字节", long.len())));
        assert!(compressed.len() < long.len());
    }

    /// 截断点落在多字节字符中间时向前退到字符边界，不产生乱码（越界 panic 即失败）。
    #[test]
    fn compress_cuts_at_char_boundary() {
        // "汉" 占 3 字节：CAP-1 个 'a' 后接 "汉"，截断点 CAP 落在字符中间。
        let mut out = "a".repeat(TOOL_OUTPUT_CONTEXT_CAP_BYTES - 1);
        out.push('汉');
        out.push_str("剩余内容");
        let compressed = compress_tool_output_for_context(&out);
        // 截断后内容为纯 ASCII 头部 + 说明（"汉" 被整体切掉，未产生半个字符）。
        assert!(!compressed.contains('汉'));
        assert!(compressed.contains("已截断"));
    }

    /// 保留最近 N 轮全文，更早轮次替换为一行摘要；幂等。
    #[test]
    fn summarize_old_tool_rounds_keeps_recent_and_idempotent() {
        let mut msgs = vec![
            ChatMessage::new(Role::User, "问1"),
            assistant_tool_calls(&["c1"]),
            tool_msg("c1", "第一轮输出"),
            ChatMessage::new(Role::User, "问2"),
            assistant_tool_calls(&["c2"]),
            tool_msg("c2", "第二轮输出"),
            ChatMessage::new(Role::User, "问3"),
            assistant_tool_calls(&["c3"]),
            tool_msg("c3", "第三轮输出"),
        ];
        summarize_old_tool_rounds(&mut msgs, 2);
        // 第 1 轮（早于保留范围）摘要化并带工具名；第 2、3 轮保留全文。
        assert!(msgs[2].content.starts_with(TOOL_SUMMARY_PREFIX));
        assert!(msgs[2].content.contains("exec_ssh"));
        assert_eq!(msgs[5].content, "第二轮输出");
        assert_eq!(msgs[8].content, "第三轮输出");
        // 幂等：重复执行不改变已摘要化的内容。
        let once = msgs[2].content.clone();
        summarize_old_tool_rounds(&mut msgs, 2);
        assert_eq!(msgs[2].content, once);
    }

    /// 边界回归：末尾就是 tool 组时，预算再小也不能把组前的 user 裁掉——
    /// 否则首条非 system 消息变成 assistant(tool_calls)，协议直接 400。
    #[test]
    fn trim_keeps_user_before_trailing_tool_group() {
        let mut msgs = vec![
            ChatMessage::new(Role::User, "问"),
            assistant_tool_calls(&["c1"]),
            tool_msg("c1", "输出"),
        ];
        trim_history_for_context(&mut msgs, 1);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, Role::User);
    }

    /// tool_calls 的序列化参数计入预算；裁剪后不残留孤儿 tool 消息、首条为 user。
    #[test]
    fn trim_counts_tool_calls_and_drops_group_as_whole() {
        // 仅按正文估算总 tokens ≈ 38 < 60（不裁）；计入 tool_calls（约 +60）后超预算。
        let big_args = serde_json::json!({ "command": "x".repeat(200) });
        let mut msgs = vec![
            ChatMessage::new(Role::User, "问"),
            ChatMessage {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: Some(vec![ToolCall {
                    id: "c1".into(),
                    name: "exec_ssh".into(),
                    arguments: big_args,
                }]),
                tool_call_id: None,
                images: None,
            },
            tool_msg("c1", "输出"),
            ChatMessage::new(Role::User, "第二问"),
        ];
        trim_history_for_context(&mut msgs, 60);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].content, "第二问");
    }

    /// 错误分类：429/5xx/建连失败可重试；400 + 上下文关键词触发降级；流中途失败不可重试。
    #[test]
    fn classify_chat_errors() {
        assert!(matches!(
            classify_chat_error("LLM 返回错误状态 429 Too Many Requests: ..."),
            ChatErrorKind::Retryable
        ));
        assert!(matches!(
            classify_chat_error("LLM 返回错误状态 503 Service Unavailable: ..."),
            ChatErrorKind::Retryable
        ));
        assert!(matches!(
            classify_chat_error("连接 LLM 服务失败: dns error"),
            ChatErrorKind::Retryable
        ));
        assert!(matches!(
            classify_chat_error("LLM 返回错误状态 400 Bad Request: {\"message\":\"request too large for model\"}"),
            ChatErrorKind::ContextTooLong
        ));
        assert!(matches!(
            classify_chat_error("LLM 返回错误状态 400 Bad Request: maximum context length exceeded"),
            ChatErrorKind::ContextTooLong
        ));
        assert!(matches!(
            classify_chat_error("读取流式响应失败: connection reset"),
            ChatErrorKind::Fatal
        ));
        assert!(matches!(
            classify_chat_error("LLM 返回错误状态 401 Unauthorized: ..."),
            ChatErrorKind::Fatal
        ));
    }

    // --- 重复工具调用守卫（RepeatCallGuard） ---

    /// 参数对象仅属性顺序不同 → 规范化后视为相同调用（serde_json 的 Map 按键排序）。
    #[test]
    fn guard_canonicalizes_argument_order() {
        let mut g = RepeatCallGuard::new();
        let a = serde_json::json!({ "command": "df -h", "sessionId": "s1" });
        let b = serde_json::json!({ "sessionId": "s1", "command": "df -h" });
        // 第 1、2 次不触发（阈值 3）。
        assert!(g.observe("exec_ssh", &a).is_none());
        assert!(g.observe("exec_ssh", &b).is_none());
        // 第 3 次（属性顺序不同但内容相同）触发提醒。
        let r = g.observe("exec_ssh", &a).expect("应在第 3 次触发");
        assert!(r.contains("连续 3 次"));
        assert!(r.contains("exec_ssh"));
    }

    /// 阈值 [3, 5, 8]：每个阈值只提醒一次；中间穿插不同调用会重置链条。
    #[test]
    fn guard_fires_at_thresholds_and_resets_on_change() {
        let mut g = RepeatCallGuard::new();
        let args = serde_json::json!({ "sql": "SELECT 1" });
        // 3 次连续 → 第 3 次触发（简短提醒，不含参数预览）。
        assert!(g.observe("exec_sql", &args).is_none());
        assert!(g.observe("exec_sql", &args).is_none());
        let r3 = g.observe("exec_sql", &args).unwrap();
        assert!(r3.contains("连续 3 次"));
        assert!(!r3.contains("SELECT 1")); // 首个阈值是通用简短提醒
        // 4、5 次：第 5 次触发详细提醒（含工具名 + 参数预览）。
        assert!(g.observe("exec_sql", &args).is_none());
        let r5 = g.observe("exec_sql", &args).unwrap();
        assert!(r5.contains("连续 5 次"));
        assert!(r5.contains("SELECT 1"));
        // 同一阈值不重复提醒。
        assert!(g.observe("exec_sql", &args).is_none());
        assert!(g.observe("exec_sql", &args).is_none());
        // 第 8 次触发（第二次进入详细提醒路径）。
        let r8 = g.observe("exec_sql", &args).unwrap();
        assert!(r8.contains("连续 8 次"));
        // 之后不再提醒。
        assert!(g.observe("exec_sql", &args).is_none());

        // 换工具 → 链条重置（连续次数重新从 1 计，已触发阈值也重置——新的
        // 一串连续调用是新循环，值得再次提醒）。
        let other = serde_json::json!({ "path": "a.txt" });
        assert!(g.observe("read_file", &other).is_none());
        assert!(g.observe("read_file", &other).is_none());
        // 同样内容换成另一个工具：也重置（键含工具名）。
        assert!(g.observe("exec_sql", &args).is_none());
        assert!(g.observe("exec_sql", &args).is_none());
        assert!(g.observe("exec_sql", &args).unwrap().contains("连续 3 次"));
    }

    /// 排除工具（todo_write / load_skill）既不触发提醒也不重置链条：
    /// grep X → todo_write → grep X 仍算两次连续 grep X。
    #[test]
    fn guard_excluded_tools_are_transparent() {
        let mut g = RepeatCallGuard::new();
        let grep = serde_json::json!({ "command": "grep error /var/log/app.log" });
        let todo = serde_json::json!({ "todos": [] });
        assert!(g.observe("exec_ssh", &grep).is_none());
        // 记账调用穿插：不累计、不重置。
        assert!(g.observe("todo_write", &todo).is_none());
        assert!(g.observe("todo_write", &todo).is_none());
        // 第 2 次 grep（中间隔了排除调用）→ 仍按连续第 2 次计。
        assert!(g.observe("exec_ssh", &grep).is_none());
        // 第 3 次 grep → 触发（链条未被 todo_write 洗白）。
        assert!(g.observe("exec_ssh", &grep).unwrap().contains("连续 3 次"));
    }

    /// 参数预览截断：超长参数被截断并附省略说明，防止巨型参数骑进提醒文本。
    /// （断言用字符数而非字节数：提醒正文为中文，UTF-8 下每字 3 字节。）
    #[test]
    fn guard_truncates_long_argument_preview() {
        let mut g = RepeatCallGuard::new();
        let big = serde_json::json!({ "command": "x".repeat(500) });
        assert!(g.observe("exec_ssh", &big).is_none());
        assert!(g.observe("exec_ssh", &big).is_none());
        let r = g.observe("exec_ssh", &big).unwrap();
        // 第 3 次是简短通用提醒（不含参数预览）。
        assert!(r.contains("连续 3 次"));
        assert!(!r.contains("xxx"));
        // 再补两次到第 5 次：详细版应截断参数。
        assert!(g.observe("exec_ssh", &big).is_none());
        let r5 = g.observe("exec_ssh", &big).unwrap();
        assert!(r5.contains("已省略"));
        assert!(r5.chars().count() < 400);
    }

    // =========================================================================
    // plan_call：单轮工具调用的执行计划（域分发 / SQL 模式拦截 / 运行模式门控）
    // =========================================================================

    fn tc(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: args,
        }
    }

    fn allowed(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn ssh_cfg(run_mode: &str) -> SshAgentSettings {
        SshAgentSettings {
            run_mode: run_mode.into(),
            ..Default::default()
        }
    }

    fn sql_cfg(run_mode: &str, sql_mode: &str) -> SqlAgentSettings {
        SqlAgentSettings {
            run_mode: run_mode.into(),
            sql_mode: sql_mode.into(),
            ..Default::default()
        }
    }

    /// 未 advertised 工具（模型幻觉调用）→ Err（调用方直接回填拒绝）。
    #[test]
    fn plan_rejects_unadvertised_tool() {
        let a = allowed(&["exec_sql"]);
        let c = tc("exec_ssh", serde_json::json!({ "command": "df -h" }));
        let err = plan_call(&c, &a, &ssh_cfg("manual"), &sql_cfg("manual", "readonly"), None)
            .unwrap_err();
        assert!(err.contains("不可用"));
    }

    /// 域分发：工具名 → 正确的域（决定确认/放行策略与配置来源）。
    #[test]
    fn plan_dispatches_domains() {
        let a = allowed(&[
            "exec_ssh",
            "exec_sql",
            "read_file",
            "write_file",
            "desktop_screenshot",
            "desktop_click",
            "todo_write",
            "load_skill",
            "ask_user_question",
            "terminal_snapshot",
        ]);
        let cases: &[(&str, &str)] = &[
            ("exec_ssh", "ssh"),
            ("exec_sql", "sql"),
            ("read_file", "file"),
            ("write_file", "file"),
            ("desktop_screenshot", "desktop"),
            ("desktop_click", "desktop"),
            ("todo_write", "todo"),
            ("load_skill", "skill"),
            ("ask_user_question", "ask"),
            ("terminal_snapshot", "other"),
        ];
        for (name, domain) in cases {
            let c = tc(name, serde_json::json!({}));
            let p = plan_call(&c, &a, &ssh_cfg("manual"), &sql_cfg("manual", "readonly"), None)
                .unwrap();
            assert_eq!(p.domain, *domain, "工具 {name} 应属于域 {domain}");
        }
    }

    /// SQL 模式拦截：readonly 下 UPDATE 被预拒绝；SELECT 放行（auto 模式直接执行）。
    /// sql_mode 是硬边界——auto 模式也不能绕过。
    #[test]
    fn plan_sql_mode_gates_statements() {
        let a = allowed(&["exec_sql"]);
        let cfg = sql_cfg("auto", "readonly");
        let c = tc("exec_sql", serde_json::json!({ "sql": "SELECT 1" }));
        let p = plan_call(&c, &a, &ssh_cfg("manual"), &cfg, None).unwrap();
        assert!(p.rejected.is_none(), "SELECT 不应被 readonly 拦截");
        assert!(p.auto_run, "auto 模式 + 只读查询应自动执行");
        let c = tc("exec_sql", serde_json::json!({ "sql": "UPDATE t SET a=1" }));
        let p = plan_call(&c, &a, &ssh_cfg("manual"), &cfg, None).unwrap();
        assert!(p.rejected.is_some(), "readonly 模式应拦截 UPDATE");
        assert!(p.rejected.unwrap().contains("readonly"));
    }

    /// SQL 危险判定：full 模式放行 DELETE，但 manual 模式下仍强制人工确认。
    #[test]
    fn plan_sql_dangerous_stays_manual() {
        let a = allowed(&["exec_sql"]);
        let cfg = sql_cfg("manual", "full");
        let c = tc("exec_sql", serde_json::json!({ "sql": "DELETE FROM users" }));
        let p = plan_call(&c, &a, &ssh_cfg("manual"), &cfg, None).unwrap();
        assert!(p.rejected.is_none(), "full 模式放行 DELETE");
        assert!(p.dangerous, "无 WHERE DELETE 应标记危险");
        assert!(!p.auto_run, "manual 模式下危险 SQL 必须人工确认");
    }

    /// SSH 运行模式门控：manual / whitelist / auto 三档的自动放行边界。
    #[test]
    fn plan_ssh_run_modes() {
        let a = allowed(&["exec_ssh"]);
        let base = ssh_cfg("manual");
        // manual：白名单内命令也不自动（仍需人工确认）。
        let cfg = SshAgentSettings {
            run_mode: "manual".into(),
            command_whitelist: vec!["df".into()],
            ..base.clone()
        };
        let c = tc("exec_ssh", serde_json::json!({ "command": "df -h" }));
        let p = plan_call(&c, &a, &cfg, &sql_cfg("manual", "readonly"), None).unwrap();
        assert!(p.whitelisted, "命中白名单应标记 whitelisted");
        assert!(!p.auto_run, "manual 模式一律人工确认");
        // whitelist：白名单内且非危险 → 自动执行。
        let cfg = SshAgentSettings {
            run_mode: RUN_MODE_WHITELIST.into(),
            command_whitelist: vec!["df".into()],
            ..base.clone()
        };
        let c = tc("exec_ssh", serde_json::json!({ "command": "df -h" }));
        let p = plan_call(&c, &a, &cfg, &sql_cfg("manual", "readonly"), None).unwrap();
        assert!(p.whitelisted && p.auto_run, "白名单内非危险命令应自动执行");
        // whitelist：危险命令即使命中白名单前缀也不自动。
        let cfg = SshAgentSettings {
            run_mode: RUN_MODE_WHITELIST.into(),
            command_whitelist: vec!["shutdown".into()],
            ..base.clone()
        };
        let c = tc("exec_ssh", serde_json::json!({ "command": "shutdown -h now" }));
        let p = plan_call(&c, &a, &cfg, &sql_cfg("manual", "readonly"), None).unwrap();
        assert!(p.dangerous, "shutdown 应标记危险");
        assert!(!p.auto_run, "白名单内危险命令仍需确认");
        // auto：全部自动执行（含白名单外命令）。
        let cfg = SshAgentSettings {
            run_mode: RUN_MODE_AUTO.into(),
            command_whitelist: vec![],
            ..base
        };
        let c = tc("exec_ssh", serde_json::json!({ "command": "df -h" }));
        let p = plan_call(&c, &a, &cfg, &sql_cfg("manual", "readonly"), None).unwrap();
        assert!(p.auto_run, "auto 模式全部自动执行");
    }

    /// 记账/只读工具（todo_write / load_skill）：无副作用，恒自动执行。
    #[test]
    fn plan_bookkeeping_tools_auto_run() {
        let a = allowed(&["todo_write", "load_skill"]);
        let c = tc("todo_write", serde_json::json!({ "todos": [] }));
        let p = plan_call(&c, &a, &ssh_cfg("manual"), &sql_cfg("manual", "readonly"), None)
            .unwrap();
        assert!(p.auto_run);
        assert_eq!(p.domain, "todo");
        let c = tc("load_skill", serde_json::json!({ "name": "x" }));
        let p = plan_call(&c, &a, &ssh_cfg("manual"), &sql_cfg("manual", "readonly"), None)
            .unwrap();
        assert!(p.auto_run);
        assert_eq!(p.domain, "skill");
    }

    /// 可视化开启时 exec_ssh 写共享 PTY（忙锁 try-acquire 互斥）→ 标记 serial
    /// 同轮串行；关闭时走独立 SSH 连接可并行；exec_sql 不受 ssh 可视化开关影响。
    #[test]
    fn plan_serializes_visual_ssh() {
        let a = allowed(&["exec_ssh", "exec_sql"]);
        let mut cfg = ssh_cfg("auto");
        // 可视化开：exec_ssh 必须串行。
        cfg.terminal_visualization = true;
        let c = tc("exec_ssh", serde_json::json!({ "command": "df -h" }));
        let p = plan_call(&c, &a, &cfg, &sql_cfg("manual", "readonly"), None).unwrap();
        assert!(p.visualization);
        assert!(p.serial, "可视化模式 exec_ssh 应串行");
        // 可视化关：独立连接，无共享状态，可并行。
        cfg.terminal_visualization = false;
        let p = plan_call(&c, &a, &cfg, &sql_cfg("manual", "readonly"), None).unwrap();
        assert!(!p.visualization);
        assert!(!p.serial);
        // exec_sql 走连接池，不受 ssh 可视化开关影响。
        cfg.terminal_visualization = true;
        let c = tc("exec_sql", serde_json::json!({ "sql": "SELECT 1" }));
        let p = plan_call(&c, &a, &cfg, &sql_cfg("auto", "readonly"), None).unwrap();
        assert!(!p.serial);
    }

    /// 串行闸门：serial 调用按出现顺序两两成链（跳过中间穿插的非 serial 调用），
    /// 链头只 signal、链尾只 wait、非 serial 两个位置恒为 None。
    #[test]
    fn serial_gates_chain_in_order() {
        // [ssh, sql, ssh, ssh, sql] → ssh 链 0 → 2 → 3。
        let (wait, signal) = build_serial_gates(&[true, false, true, true, false]);
        assert!(wait[0].is_none() && signal[0].is_some(), "链头只通知");
        assert!(wait[1].is_none() && signal[1].is_none(), "非 serial 无闸门");
        assert!(wait[2].is_some() && signal[2].is_some(), "链中既等又通知");
        assert!(wait[3].is_some() && signal[3].is_none(), "链尾只等待");
        assert!(wait[4].is_none() && signal[4].is_none());
        // 至多一个 serial 调用：不成链（无闸门，等效并行/立即执行）。
        let (wait, signal) = build_serial_gates(&[false, true, false]);
        assert!(wait.iter().all(|g| g.is_none()));
        assert!(signal.iter().all(|g| g.is_none()));
        // 空轮：无闸门。
        let (wait, signal) = build_serial_gates(&[]);
        assert!(wait.is_empty() && signal.is_empty());
    }

    /// 提问工具：合法参数不弹确认卡片（auto_run），非法问题结构被预拒绝。
    #[test]
    fn plan_ask_user_gating() {
        let a = allowed(&["ask_user_question"]);
        // 合法：两个问题（id 唯一、带选项）→ 进入执行阶段，不弹二次确认。
        let c = tc(
            "ask_user_question",
            serde_json::json!({
                "questions": [
                    { "id": "port", "question": "目标端口？", "options": [{ "label": "22" }] },
                    { "id": "confirm", "question": "确认继续？" }
                ]
            }),
        );
        let p = plan_call(&c, &a, &ssh_cfg("manual"), &sql_cfg("manual", "readonly"), None)
            .unwrap();
        assert_eq!(p.domain, "ask");
        assert!(p.rejected.is_none());
        assert!(p.auto_run, "提问不弹二次确认卡片");
        // 非法：id 重复 → 预拒绝（前端不渲染畸形表单）。
        let c = tc(
            "ask_user_question",
            serde_json::json!({
                "questions": [
                    { "id": "q", "question": "a" },
                    { "id": "q", "question": "b" }
                ]
            }),
        );
        let p = plan_call(&c, &a, &ssh_cfg("manual"), &sql_cfg("manual", "readonly"), None)
            .unwrap();
        assert!(p.rejected.is_some(), "重复 id 应被预拒绝");
        assert!(p.rejected.unwrap().contains("重复"));
    }

    /// validate_ask_user_questions：边界校验（空问题 / 超 4 个 / 缺文本 / 空选项）。
    #[test]
    fn validate_ask_user_questions_edges() {
        // 空 questions → 报错。
        assert!(crate::ai::tools::validate_ask_user_questions(&serde_json::json!({ "questions": [] }))
            .is_err());
        // 5 个问题 → 超上限。
        let many: Vec<serde_json::Value> = (0..5)
            .map(|i| serde_json::json!({ "id": format!("q{i}"), "question": "x" }))
            .collect();
        let err = crate::ai::tools::validate_ask_user_questions(&serde_json::json!({ "questions": many }))
            .unwrap_err();
        assert!(err.contains("4"));
        // 缺 question 文本 → 报错。
        assert!(crate::ai::tools::validate_ask_user_questions(&serde_json::json!({
            "questions": [{ "id": "q1" }]
        }))
        .is_err());
        // 选项缺 label → 报错。
        assert!(crate::ai::tools::validate_ask_user_questions(&serde_json::json!({
            "questions": [{ "id": "q1", "question": "x", "options": [{ "description": "无 label" }] }]
        }))
        .is_err());
        // 合法（含多选 + 选项说明）→ Ok，字段归一化正确。
        let ok = crate::ai::tools::validate_ask_user_questions(&serde_json::json!({
            "questions": [{
                "id": "q1",
                "question": "选择方案？",
                "header": "方案",
                "options": [{ "label": "A", "description": "方案 A" }],
                "multi_select": true
            }]
        }))
        .unwrap();
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].id, "q1");
        assert!(ok[0].multi_select);
        assert_eq!(ok[0].options[0].label, "A");
    }

    /// 回答格式化：与 dsh 规范一致（answers JSON，字段名 camelCase）。
    #[test]
    fn format_ask_user_answers_roundtrip() {
        let out = crate::ai::tools::format_ask_user_answers(&[
            crate::ai::tools::AskUserAnswer {
                id: "q1".into(),
                selected: vec!["22".into()],
                custom: None,
            },
            crate::ai::tools::AskUserAnswer {
                id: "q2".into(),
                selected: vec![],
                custom: Some("自由输入".into()),
            },
        ]);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let answers = parsed["answers"].as_array().unwrap();
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0]["id"], "q1");
        assert_eq!(answers[0]["selected"][0], "22");
        assert_eq!(answers[1]["custom"], "自由输入");
    }
}
