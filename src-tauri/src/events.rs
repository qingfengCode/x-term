//! Tauri 事件名常量与事件 payload 定义。
//!
//! 前端通过监听这些事件来接收来自后端的实时通知（终端数据、传输进度、AI 流式响应等）。
//! 所有事件 payload 结构体均使用 `#[serde(rename_all = "camelCase")]`，保证 JS 端拿到的
//! 字段名为驼峰式（如 `sessionId` 而不是 `session_id`），符合前端编码惯例。

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

// ===========================================================================
// 事件名常量
// ===========================================================================

/// 终端输出数据事件。
/// payload: [`TerminalDataEvent`]，`data` 字段是 base64 编码后的二进制数据。
pub const TERMINAL_DATA: &str = "terminal:data";

/// 终端关闭（连接断开）事件。payload: [`TerminalClosedEvent`]。
pub const TERMINAL_CLOSED: &str = "terminal:closed";

/// 终端进程退出事件。payload: [`TerminalExitEvent`]。
pub const TERMINAL_EXIT: &str = "terminal:exit";

/// 文件传输进度事件。payload: [`TransferProgressEvent`]。
pub const TRANSFER_PROGRESS: &str = "transfer:progress";

/// 文件传输完成事件。payload: [`TransferDoneEvent`]。
pub const TRANSFER_DONE: &str = "transfer:done";

/// 文件传输出错事件。payload: [`TransferErrorEvent`]。
pub const TRANSFER_ERROR: &str = "transfer:error";

/// AI 流式响应片段事件。payload: [`AiChunkEvent`]。
pub const AI_CHUNK: &str = "ai:chunk";

/// AI 响应完成事件。payload: [`AiDoneEvent`]。
pub const AI_DONE: &str = "ai:done";

/// AI 出错事件。payload: [`AiErrorEvent`]。
pub const AI_ERROR: &str = "ai:error";

/// AI 请求被用户终止。payload: [`AiStoppedEvent`]。
pub const AI_STOPPED: &str = "ai:stopped";

/// AI 请求执行工具（前端弹确认）。payload: [`AiToolCallEvent`]。
pub const AI_TOOL_CALL: &str = "ai:tool_call";

/// 工具执行完成（前端展示结果）。payload: [`AiToolResultEvent`]。
pub const AI_TOOL_RESULT: &str = "ai:tool_result";

/// AI 智能体更新任务清单（todo_write 工具调用）。payload: [`AiTodoEvent`]。
///
/// 借鉴 deepseek-harness 的 tool-todo 语义：模型每次调用 `todo_write` 时**整表替换**
/// 任务清单，前端按事件流 last-write-wins 展示最新清单（同一请求内多条 todo_write
/// 事件按到达顺序覆盖）。
pub const AI_TODO: &str = "ai:todo";

/// AI 请求的 token 用量统计。payload: [`AiUsageEvent`]。
///
/// 借鉴 deepseek-harness 的 llm/token-meter：流式结束时从响应 usage 字段解析并
/// 上报（OpenAI 兼容协议的 usage 在最后一个 chunk）。前端按会话累计展示。
/// 厂商不返回 usage 时不发该事件。
pub const AI_USAGE: &str = "ai:usage";

/// AI 请求自动重试中。payload: [`AiRetryingEvent`]。
///
/// 可重试错误（建连失败/429/5xx）发生时 provider 内部已 emit 过 ai:error，
/// 前端会按错误收尾（sending=false 等）；随后编排层退避重试。重试开始时
/// emit 本事件，前端据此**恢复**会话的进行中状态——否则重试成功后的
/// 全部输出（chunk/done/工具事件）虽然仍在发送，但会话已被标记结束，
/// 用户体验上等于"报错了但其实又在跑"，结果凭空丢失。
pub const AI_RETRYING: &str = "ai:retrying";

/// 系统注入的编排层提示（如重复调用守卫的提醒）。payload: [`AiSystemNoteEvent`]。
///
/// 后端把它作为 user 消息注入模型上下文的同时 emit 一份给前端，前端以灰色
/// 居中提示渲染（不伪装成用户消息，见 AiPanel 的 isSystemReminder），让用户
/// 能感知"模型正在空转/已收到纠偏提醒"。
pub const AI_SYSTEM_NOTE: &str = "ai:system_note";

/// exec_sql 终端可视化：AI 执行 SQL 时把 SQL + 结构化结果回显到 SQL 控制台。
/// payload: [`AiSqlResultEvent`]。仅在 `sql_agent.terminal_visualization` 开启时 emit。
pub const AI_SQL_RESULT: &str = "ai:sql_result";

/// SQL 控制台查询结果。payload: [`DbQueryResultEvent`]。
pub const DB_QUERY_RESULT: &str = "db:query_result";

/// 应用更新下载进度。payload: [`UpdateProgressEvent`]。
pub const UPDATE_PROGRESS: &str = "update:progress";

/// SSH 二次认证挑战（keyboard-interactive）。payload: [`SshAuthChallengeEvent`]。
///
/// 后端在认证过程中收到服务器发来的键盘交互挑战（如 OTP/验证码）时 emit，
/// 前端弹出输入框收集用户输入后通过 `ssh_auth_respond` 命令回传。
pub const SSH_AUTH_CHALLENGE: &str = "ssh:auth_challenge";

/// SSH 主机公钥变更确认。payload: [`SshHostKeyEvent`]。
///
/// 后端在 [`crate::ssh::client::ClientHandler::check_server_key`] 检测到主机公钥
/// 与 known_hosts 记录不符时 emit，前端弹窗展示新旧指纹对比，用户选择后通过
/// `ssh_host_key_respond` 命令回传决策（接受并更新 / 仅本次接受 / 拒绝）。
pub const SSH_HOST_KEY_CHALLENGE: &str = "ssh:host_key_challenge";

/// 端口转发运行状态变化事件。payload: [`ForwardStateEvent`]。
///
/// 后端在隧道启动、停止，以及监控任务发现 SSH 连接断开（隧道异常退出）时 emit，
/// 前端据此实时更新"运行中/已停止"标签，不再依赖进页面时的一次性拉取。
pub const FORWARD_STATE: &str = "forward:state";

/// 服务器监控数据事件（每 3s 一帧）。payload: [`MonitorDataEvent`]。
pub const MONITOR_DATA: &str = "monitor:data";

/// 服务器监控结束（停止/断开/连续失败）。payload: [`MonitorClosedEvent`]。
pub const MONITOR_CLOSED: &str = "monitor:closed";

// ===========================================================================
// 事件 payload 结构体
// ===========================================================================

/// 终端输出数据。
///
/// `data` 使用 base64 字符串而不是原始 `Vec<u8>`，以避免 JSON 将字节序列化为 number[]
/// 带来的体积膨胀。
///
/// 批量 emit 时事件可能包含多个 TCP 块的拼接，`data` 对应输出缓冲中的
/// 半开区间 `[start_total, total)`：前端按快照基线 `b` 去重时——
/// - `total <= b`：整段已含在快照里，跳过；
/// - `start_total < b < total`：只取尾部 `total - b` 字节；
/// - `start_total >= b`：整段渲染。
/// 这保证了「快照与批次跨界」时既不丢字节也不重复（见 terminal_attach）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalDataEvent {
    pub session_id: String,
    pub data: String,
    /// `data` 首字节对应的累计输出字节数（半开区间起点）。
    #[serde(default)]
    pub start_total: usize,
    /// `data` 末字节追加后的累计输出字节数（半开区间终点，单调递增）。
    #[serde(default)]
    pub total: usize,
}

/// 终端关闭。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalClosedEvent {
    pub session_id: String,
    /// 断开原因（SSH 会话时由 handler 的 `disconnected` 回调记录：服务器
    /// DISCONNECT 的原因文字 / 保活超时等）；本地会话为 `None`。
    #[serde(default)]
    pub reason: Option<String>,
}

/// 终端进程退出。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalExitEvent {
    pub session_id: String,
    pub code: Option<i32>,
}

/// 文件传输进度。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgressEvent {
    pub task_id: String,
    pub transferred: u64,
    pub total: u64,
    /// 速率，单位字节/秒。
    pub speed: u64,
}

/// 文件传输完成。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferDoneEvent {
    pub task_id: String,
    pub transferred: u64,
    pub total: u64,
}

/// 文件传输出错。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferErrorEvent {
    pub task_id: String,
    pub message: String,
}

/// AI 流式响应片段。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiChunkEvent {
    /// 关联的请求/会话标识。
    pub request_id: String,
    /// 本次片段的文本内容。
    pub delta: String,
}

/// AI 响应完成。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiDoneEvent {
    pub request_id: String,
    /// 累计的完整响应文本。
    pub full_text: String,
}

/// AI 出错。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiErrorEvent {
    pub request_id: String,
    pub message: String,
}

/// AI 请求被用户终止。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiStoppedEvent {
    pub request_id: String,
}

/// AI 请求执行工具（用户需确认）。
///
/// `arguments` 是工具参数的 JSON 字符串；前端可解析展示。
/// `description` 是给人类可读的简述（如 "执行命令: df -h"）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiToolCallEvent {
    pub request_id: String,
    pub tool_call_id: String,
    pub name: String,
    pub arguments: String,
    pub description: String,
    /// 是否被安全护栏判定为危险操作（前端据此红色高亮 + 二次确认）。
    pub dangerous: bool,
    /// exec_ssh 命令是否落在用户白名单内。
    ///
    /// - `true`：白名单内命令，前端显示绿色卡片 + "执行"按钮（仍需用户点一下放行）。
    /// - `false`：非白名单或非 exec_ssh，按 `dangerous` 走橙色/红色确认。
    #[serde(default)]
    pub whitelisted: bool,
    /// 是否已被自动放行（白名单模式 + 命中白名单 + 非危险）。
    ///
    /// `true` 时前端卡片直接显示"已自动执行"终态，不显示执行/拒绝按钮。
    /// 这种 tool_call 后端不再等待人工确认，已直接执行。
    #[serde(default)]
    pub auto_approved: bool,
    /// 桌面工具（desktop_*）绑定的内嵌 RDP 桥接实例 id。
    ///
    /// 仅桌面工具非空：执行体在前端（IronRDP WASM 会话），前端据此定位要操作的
    /// 桌面（请求发起时的活动桌面），不依赖执行瞬间的标签状态。其余工具为 None。
    #[serde(default)]
    pub desktop_id: Option<String>,
}

/// 工具执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiToolResultEvent {
    pub request_id: String,
    pub tool_call_id: String,
    pub ok: bool,
    pub output: String,
}

/// 任务清单中的一项（todo_write 工具写入）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiTodoItem {
    /// 任务描述（非空）。
    pub content: String,
    /// 状态：pending（待办）/ in_progress（进行中）/ completed（已完成）。
    pub status: String,
}

/// AI 智能体任务清单更新事件。
///
/// 每次 `todo_write` 工具调用 emit 一次，携带**整表**任务清单（替换语义，
/// 前端以最新事件为准）。`request_id` 用于路由到对应会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiTodoEvent {
    pub request_id: String,
    pub todos: Vec<AiTodoItem>,
}

/// AI 单次请求的 token 用量（流式结束时上报一次）。
///
/// OpenAI 系：`usage.prompt_tokens / completion_tokens`（流末尾 chunk）；
/// 前端按 request_id 路由到会话并**累计**到会话级统计。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiUsageEvent {
    /// 关联的请求/会话标识。
    pub request_id: String,
    /// 输入 token 数（prompt / input tokens）。
    pub prompt_tokens: u64,
    /// 输出 token 数（completion / output tokens）。
    pub completion_tokens: u64,
}

/// AI 请求自动重试事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiRetryingEvent {
    pub request_id: String,
    /// 第几次尝试（含首次失败的这轮，从 1 起）。
    pub attempt: u32,
    /// 最大尝试次数。
    pub max_attempts: u32,
    /// 重试原因（简短错误摘要，供前端提示）。
    pub reason: String,
}

/// 系统注入的编排层提示（重复调用守卫提醒等）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSystemNoteEvent {
    pub request_id: String,
    /// 提示文本（以 `[重复调用提醒]` 等标记开头，前端据此走灰色提示渲染）。
    pub text: String,
}

/// exec_sql 终端可视化回显事件。
///
/// exec_sql 在 `terminal_visualization` 开启时 emit：前端 SQL 控制台（命令行模式）
/// 据此把 SQL 与结构化结果推入输出流，就像用户自己执行一样。
/// `error` 非空表示执行失败（此时 columns/rows 为空）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSqlResultEvent {
    /// 触发执行的 AI 请求 id（便于调试；控制台不依赖它路由）。
    pub request_id: String,
    /// 执行的 SQL 文本。
    pub sql: String,
    /// 结果列名（非查询语句为空）。
    pub columns: Vec<String>,
    /// 结果行（每行按列顺序的字符串值）。
    pub rows: Vec<Vec<String>>,
    /// 非查询语句的影响行数（SELECT 为行数）。
    pub affected: u64,
    /// 结果是否被 limit 截断（查询实际返回超过 limit 行）。
    #[serde(default)]
    pub truncated: bool,
    /// 执行耗时（毫秒）。
    pub elapsed_ms: u64,
    /// 执行错误信息（成功为 None）。
    pub error: Option<String>,
}

/// SQL 查询结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbQueryResultEvent {
    pub query_id: String,
    pub columns: Vec<String>,
    /// 行数据，每行是按列顺序的值（已转为字符串）。
    pub rows: Vec<Vec<String>>,
    /// 非 SELECT 语句的影响行数（SELECT 为 0）。
    pub affected: u64,
    /// 结果是否被 limit 截断（查询实际返回超过 limit 行）。
    #[serde(default)]
    pub truncated: bool,
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

/// 应用更新下载进度。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProgressEvent {
    /// 已下载字节数。
    pub received: u64,
    /// 总字节数（服务端未返回 Content-Length 时为 0）。
    pub total: u64,
    /// 百分比（0~100；total 未知时为 0）。
    pub percent: u8,
}

/// SSH keyboard-interactive 挑战中需要用户输入的单个项。
///
/// 已被后端用保存密码自动填充的提示（如 "Password: "）**不会**出现在事件里，
/// 避免把凭据信息发往前端。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshAuthPrompt {
    /// 服务器给出的提示文本（如 "Password: " / "Verification code: "）。
    pub prompt: String,
    /// 是否回显输入。false 表示密码类输入，前端应用 password 输入框。
    pub echo: bool,
}

/// SSH 二次认证挑战事件。
///
/// 前端按 `challenge_id` 用 `ssh_auth_respond` 命令回传答案：
/// - 提交：`responses` 数组与 `prompts` 一一对应；
/// - 取消：`responses` 传 `null`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshAuthChallengeEvent {
    /// 挑战 id（前端回传用）。
    pub challenge_id: String,
    /// 会话配置 id（用于展示会话名/主机）。
    pub session_config_id: String,
    /// 目标主机。
    pub host: String,
    /// 目标端口。
    pub port: u16,
    /// 登录用户名。
    pub username: String,
    /// 服务器返回的挑战名称（如 "SSH-2.0-keyboard-interactive"）。
    pub name: String,
    /// 服务器返回的说明文字（可为空）。
    pub instructions: String,
    /// 需要用户填写的输入项（已自动填充的密码提示不会出现在这里）。
    pub prompts: Vec<SshAuthPrompt>,
}

/// SSH 主机公钥变更确认事件。
///
/// 前端按 `challenge_id` 用 `ssh_host_key_respond` 命令回传决策：
/// `AcceptAndUpdate` / `AcceptOnce` / `Reject`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshHostKeyEvent {
    /// 挑战 id（前端回传用）。
    pub challenge_id: String,
    /// 目标主机。
    pub host: String,
    /// 目标端口。
    pub port: u16,
    /// 服务器实际公钥的算法名（如 `"ssh-ed25519"`）。
    pub key_type: String,
    /// 服务器实际公钥指纹（SHA-256 base64）。
    pub fingerprint: String,
    /// known_hosts 中记录的旧指纹（用于前端展示新旧对比）。
    pub known_fingerprint: String,
}

/// 端口转发运行状态变化。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardStateEvent {
    /// 转发规则 id。
    pub rule_id: String,
    /// 是否运行中。
    pub running: bool,
    /// 状态变化原因（如 "started" / "stopped" / "ssh 连接断开"），用于展示/日志。
    pub reason: String,
}

/// 服务器监控数据（一帧）。字段模型复用 [`crate::monitor`] 的定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorDataEvent {
    pub monitor_id: String,
    /// 采样时刻（epoch 毫秒）。
    pub ts: u64,
    pub host: Option<crate::monitor::MonitorHost>,
    pub cpu: crate::monitor::MonitorCpu,
    pub mem: crate::monitor::MonitorMem,
    pub load: Option<crate::monitor::MonitorLoad>,
    pub uptime_secs: u64,
    pub net: crate::monitor::MonitorNet,
    pub disks: Vec<crate::monitor::MonitorDisk>,
    pub processes: Vec<crate::monitor::MonitorProcess>,
}

/// 服务器监控结束。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorClosedEvent {
    pub monitor_id: String,
    /// 结束原因（已停止 / 连接失败 / 连续采集失败等）。
    pub reason: String,
}

// ===========================================================================
// 辅助函数
// ===========================================================================

/// 向所有前端窗口广播一个事件。
///
/// 包裹 [`AppHandle::emit`]，统一错误处理：发射失败时记录日志但不向上抛出
/// （事件丢失不应导致后端命令失败）。
///
/// # 类型参数
/// - `T`: payload 类型，必须可序列化且可克隆。
pub fn emit<T>(app: &AppHandle, event: &str, payload: T)
where
    T: Serialize + Clone,
{
    if let Err(e) = app.emit(event, payload) {
        log::warn!("发送事件 `{}` 失败: {}", event, e);
    }
}

/// 传输进度事件节流器：距上一帧 ≥100ms 才放行（完成帧恒放行）。
///
/// SFTP / 文件后端按 64KiB 块回调进度，百兆带宽 ≈ 1600 次/秒 IPC 事件，
/// 前端逐事件写响应式状态并触发列表重渲染，传输大文件时会明显拖垮 UI。
/// 节流后事件数降约两个数量级；最终进度由 `transfer:done` 事件兜底，
/// 不依赖节流帧（进度条瞬跳到 100% 与真实完成之间无功能差异）。
///
/// 与 `updater.rs` 的 `PROGRESS_EMIT_INTERVAL` 同一套约定。
#[derive(Debug)]
pub struct ProgressThrottle {
    last: std::sync::Mutex<std::time::Instant>,
}

impl ProgressThrottle {
    /// 新建节流器（初始视为"很久没发过"，首帧恒放行）。
    pub fn new() -> Self {
        Self {
            last: std::sync::Mutex::new(
                std::time::Instant::now() - std::time::Duration::from_secs(10),
            ),
        }
    }

    /// 本帧是否应发送：`done` 表示传输已完成（transferred >= total），恒放行。
    pub fn allow(&self, done: bool) -> bool {
        match self.last.lock() {
            Ok(mut last) => {
                if !done && last.elapsed() < std::time::Duration::from_millis(100) {
                    return false;
                }
                *last = std::time::Instant::now();
                true
            }
            // 锁中毒（极罕见）：宁多发不丢完成帧。
            Err(_) => true,
        }
    }
}

impl Default for ProgressThrottle {
    fn default() -> Self {
        Self::new()
    }
}
