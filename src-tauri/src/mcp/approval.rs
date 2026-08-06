//! MCP 工具调用的人工确认护栏。
//!
//! 外部 MCP 客户端（AI）调用 `exec_ssh` / `exec_sql` 时，必须先经 X-Term 用户在前端
//! 确认，避免外部 AI 在无防护下直接操作生产服务器/数据库。
//!
//! 流程：
//! 1. MCP 服务端在执行 exec_* 前调用 [`ApprovalRegistry::request_approval`]，
//!    它会 emit 一个 [`MCP_APPROVAL_REQUEST`] 事件给前端，并注册一个 oneshot，
//!    阻塞等待（最长 [`APPROVAL_TIMEOUT`]）。
//! 2. 前端收到事件后展示确认弹窗，用户点"允许"/"拒绝"后调用
//!    `mcp_respond_approval(requestId, approved)` 命令。
//! 3. 该命令调用 [`ApprovalRegistry::respond`]，把结果通过 oneshot 发回，
//!    MCP 服务端据此决定是否执行。
//!
//! list_ssh_sessions / list_db_profiles 是只读元数据查询，不走确认。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter};
use tokio::sync::{oneshot, Mutex};

use crate::error::{AppError, AppResult};

/// 确认请求等待超时（5 分钟）。超时视为拒绝。
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// MCP 确认请求事件名（emit 给前端）。
pub const MCP_APPROVAL_REQUEST: &str = "mcp:approval_request";

/// MCP 确认请求过期事件名（超时后 emit 给前端，通知移除对应浮层卡片）。
pub const MCP_APPROVAL_EXPIRED: &str = "mcp:approval_expired";

/// MCP 服务端种类：SSH MCP（暴露 exec_ssh + 文件工具）/ DB MCP（暴露 exec_sql）/
/// File MCP（暴露 list_files / upload_file / download_file，基于绑定的 S3 账号）。
///
/// 每个 kind 各自独立的监听实例、端口、token、绑定的资源。序列化为小写字符串
/// "ssh" / "db" / "file"（serde rename_all 在 enum 上对单元变体取小写名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpKind {
    Ssh,
    Db,
    File,
}

impl McpKind {
    /// 解析字符串为 McpKind；非法输入回退到 Ssh。
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "db" => McpKind::Db,
            "file" => McpKind::File,
            _ => McpKind::Ssh,
        }
    }

    /// 中文展示名。
    pub fn label(self) -> &'static str {
        match self {
            McpKind::Ssh => "SSH MCP",
            McpKind::Db => "DB MCP",
            McpKind::File => "File MCP",
        }
    }
}

impl Default for McpKind {
    /// 默认 Ssh（用于 ApprovalRequest.kind 的 serde default 等）。
    fn default() -> Self {
        McpKind::Ssh
    }
}

/// 一条 MCP 确认请求（前端展示用）。
///
/// `arguments` 是工具参数的 JSON 对象原样透传，前端可解析展示详情。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    /// 唯一请求 id（前端回结果时携带）。
    pub request_id: String,
    /// 哪个 MCP 服务端发起的确认（ssh / db）。
    #[serde(default)]
    pub kind: McpKind,
    /// 工具名：`exec_ssh` / `exec_sql`。
    pub tool_name: String,
    /// 工具参数（JSON 对象）。
    pub arguments: Value,
    /// 人类可读简述（如 "执行命令: df -h"）。
    pub description: String,
    /// 发起请求的 MCP 客户端标识（从 User-Agent 或 session 推断，可能为空）。
    pub client_name: String,
    /// 该 MCP 当前绑定的资源名（SSH 会话名 / DB profile 名），供前端浮层标注来源。
    #[serde(default)]
    pub resource_name: String,
}

/// 全局确认注册表。
///
/// 维护 `requestId -> oneshot::Sender<bool>` 的 pending map。每个待确认请求占一条；
/// 前端回结果或超时后移除。
///
/// 用 `tokio::sync::Mutex`（不是 `parking_lot`）：因为 oneshot 的 `Sender` 要跨
/// `.await` 边界在 [`request_approval`] 内被 await，需要持有的锁是 `Send` 的。
/// `parking_lot::Mutex` 实现是 non-async，guard 跨 await 不安全。

/// 确认请求过期事件的 payload（超时后通知前端移除浮层卡片）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalExpiredPayload {
    pub request_id: String,
}

#[derive(Default)]
pub struct ApprovalRegistry {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
}

impl ApprovalRegistry {
    /// 创建空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 发起一次确认请求。
    ///
    /// 1. 生成 oneshot 并登记到 pending map。
    /// 2. emit [`MCP_APPROVAL_REQUEST`] 事件（payload = `req`）给前端。
    /// 3. 阻塞等待 oneshot（最长 [`APPROVAL_TIMEOUT`]）。
    ///
    /// 返回：
    /// - `Ok(true)`：用户批准。
    /// - `Ok(false)`：用户拒绝，或前端未在超时内回应。
    /// - `Err(_)`：emit 事件失败（极端情况，一般不会发生）。
    pub async fn request_approval(&self, req: ApprovalRequest, app: &AppHandle) -> AppResult<bool> {
        let (tx, rx) = oneshot::channel::<bool>();
        {
            let mut map = self.pending.lock().await;
            map.insert(req.request_id.clone(), tx);
        }

        // emit 给前端；失败时清理 pending 并返回错误。
        if let Err(e) = app.emit(MCP_APPROVAL_REQUEST, req.clone()) {
            self.pending.lock().await.remove(&req.request_id);
            return Err(AppError::Tauri(e));
        }

        // 等待结果；超时则视为拒绝，并清理。
        match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
            Ok(Ok(approved)) => Ok(approved),
            Ok(Err(_)) => {
                // sender 被 drop（理论上不会发生，因为只有 respond 取出并 drop）。
                self.pending.lock().await.remove(&req.request_id);
                Ok(false)
            }
            Err(_) => {
                self.pending.lock().await.remove(&req.request_id);
                log::warn!(
                    "[mcp] 确认请求 {} 超时（{}s），视为拒绝",
                    req.request_id,
                    APPROVAL_TIMEOUT.as_secs()
                );
                // 通知前端移除对应的确认浮层卡片（避免残留）。
                let _ = app.emit(
                    MCP_APPROVAL_EXPIRED,
                    ApprovalExpiredPayload {
                        request_id: req.request_id.clone(),
                    },
                );
                Ok(false)
            }
        }
    }

    /// 前端回结果。取出对应 requestId 的 oneshot 发送端并发送结果。
    ///
    /// 返回是否命中（true = 找到并发送成功；false = requestId 不存在或已超时清理）。
    pub async fn respond(&self, request_id: &str, approved: bool) -> bool {
        let entry = self.pending.lock().await.remove(request_id);
        match entry {
            Some(tx) => tx.send(approved).is_ok(),
            None => {
                log::warn!("[mcp] 收到未知/过期的确认结果: {}", request_id);
                false
            }
        }
    }
}

/// 给 AppState 用的便捷类型别名。
pub type SharedApprovalRegistry = Arc<ApprovalRegistry>;
