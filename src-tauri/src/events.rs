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

// ===========================================================================
// 事件 payload 结构体
// ===========================================================================

/// 终端输出数据。
///
/// `data` 使用 base64 字符串而不是原始 `Vec<u8>`，以避免 JSON 将字节序列化为 number[]
/// 带来的体积膨胀。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalDataEvent {
    pub session_id: String,
    pub data: String,
}

/// 终端关闭。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalClosedEvent {
    pub session_id: String,
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
