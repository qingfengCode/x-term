//! Anthropic Claude 原生 Messages API 的 LLM Provider 实现。
//!
//! 与 OpenAI 兼容协议不同，Claude 的 Messages API 把 `system` 提示词放在请求体
//! 顶层，而不是放进 `messages` 数组里。本模块在构造请求时会先把 `role=system`
//! 的消息提取、拼接成 system 字段，剩余 user/assistant 消息原样传入。
//!
//! # 流式协议要点
//!
//! Claude 使用 SSE 多事件类型格式：
//!
//! ```text
//! event: message_start
//! data: {"type":"message_start",...}
//!
//! event: content_block_delta
//! data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}
//!
//! event: message_stop
//! data: {"type":"message_stop"}
//! ```
//!
//! 我们只关心 `event: content_block_delta`（取 `delta.text` 推送 ai:chunk）
//! 与 `event: message_stop`（结束、推送 ai:done）。其它事件忽略即可。

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ai::provider::{ChatMessage, ChatWithToolsResult, LlmProvider, ProviderConfig, Role};
use crate::ai::tools::{ToolCall, ToolDef, ToolResult};
use crate::error::{AppError, AppResult};
use crate::events::{self, AiChunkEvent, AiDoneEvent, AiErrorEvent};

/// Anthropic API 要求附带的本协议版本号。
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 单次响应的 max_tokens 上限。Claude 必传该字段。
///
/// 智能体模式下工具调用 + 较长解释可能超过 2048，故放宽到 8192。
const MAX_TOKENS: u32 = 8192;

/// Claude Provider。
pub struct ClaudeProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl ClaudeProvider {
    pub fn new(cfg: &ProviderConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// 请求 / 响应 serde 结构
// ---------------------------------------------------------------------------

/// 请求体中的对话消息（仅 user / assistant）。
#[derive(Debug, Serialize)]
struct ReqMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// Messages API 顶层请求体。
#[derive(Debug, Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<ReqMessage<'a>>,
    stream: bool,
}

/// `content_block_delta` 事件的 data 载荷。
#[derive(Debug, Default, Deserialize)]
struct ClaudeDelta {
    /// Claude 偶尔会发送不带 delta 字段的辅助事件，故允许缺省。
    #[serde(default)]
    delta: Option<TextDelta>,
}

/// 文本增量。
#[derive(Debug, Default, Deserialize)]
struct TextDelta {
    #[serde(default)]
    text: Option<String>,
}

// ---------------------------------------------------------------------------
// LlmProvider 实现
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmProvider for ClaudeProvider {
    async fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
        request_id: String,
        app: tauri::AppHandle,
    ) -> AppResult<String> {
        // 1) 拆分 system / 对话消息。
        let mut system_parts: Vec<String> = Vec::new();
        let mut conv: Vec<ReqMessage> = Vec::with_capacity(messages.len());
        for m in &messages {
            match m.role {
                Role::System => system_parts.push(m.content.clone()),
                Role::User => conv.push(ReqMessage {
                    role: "user",
                    content: m.content.as_str(),
                }),
                Role::Assistant => conv.push(ReqMessage {
                    role: "assistant",
                    content: m.content.as_str(),
                }),
                // chat_stream（非工具流）不处理 tool 消息；忽略。
                Role::Tool => {}
            }
        }
        let system = if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n\n"))
        };

        let body = MessagesRequest {
            model: &self.model,
            max_tokens: MAX_TOKENS,
            system,
            messages: conv,
            stream: true,
        };

        let url = format!("{}/v1/messages", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await;

        let response = match resp {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("连接 Claude 服务失败: {e}");
                emit_error(&app, &request_id, &msg);
                log::error!("[ai:{}:claude] {msg}", request_id);
                return Err(AppError::Ai(msg));
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let msg = format!("Claude 返回错误状态 {status}: {}", truncate(&text, 500));
            emit_error(&app, &request_id, &msg);
            log::error!("[ai:{}:claude] {msg}", request_id);
            return Err(AppError::Ai(msg));
        }

        // 2) SSE 流解析。Claude 使用多事件格式，需要跟踪最近一行 `event:`。
        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut full_text = String::new();
        let mut current_event: String = String::new();
        let mut done = false;

        while let Some(chunk_res) = stream.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    let msg = format!("读取 Claude 流式响应失败: {e}");
                    emit_error(&app, &request_id, &msg);
                    log::error!("[ai:{}:claude] {msg}", request_id);
                    return Err(AppError::Ai(msg));
                }
            };
            buffer.extend_from_slice(&chunk[..]);

            while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
                let line = strip_line_ending(&line_bytes);
                if line.is_empty() {
                    continue;
                }
                // 注释行（如 ": ping"），忽略。
                if line.starts_with(':') {
                    continue;
                }
                if let Some(ev) = line.strip_prefix("event:") {
                    current_event = ev.trim().to_string();
                    continue;
                }
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() {
                    continue;
                }

                // Claude 在多行 data 的情况下会拆成多条 data: 行；这里采用
                // 「按 event 触发后立即解析当前 data」的简化策略——对于 text_delta
                // 与 message_stop 这两类关键事件，Anthropic 都把 payload 放在单行内，
                // 因此不必额外拼接。
                match current_event.as_str() {
                    "content_block_delta" => {
                        let parsed: ClaudeDelta = match serde_json::from_str(payload) {
                            Ok(d) => d,
                            Err(e) => {
                                log::warn!(
                                    "[ai:{}:claude] 解析 content_block_delta 失败: {e}; data={}",
                                    request_id,
                                    truncate(payload, 200)
                                );
                                continue;
                            }
                        };
                        if let Some(td) = parsed.delta {
                            if let Some(text) = td.text {
                                if !text.is_empty() {
                                    full_text.push_str(&text);
                                    emit_chunk(&app, &request_id, &text);
                                }
                            }
                        }
                    }
                    "message_stop" => {
                        done = true;
                        break;
                    }
                    // message_start / ping / content_block_start / content_block_stop
                    // 等事件对本模块无价值，忽略。
                    _ => {}
                }
            }

            if done {
                break;
            }
        }

        emit_done(&app, &request_id, &full_text);
        Ok(full_text)
    }

    // -----------------------------------------------------------------------
    // chat_with_tools
    // -----------------------------------------------------------------------

    async fn chat_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDef>,
        tool_results: Vec<(String, ToolResult)>,
        request_id: String,
        app: tauri::AppHandle,
    ) -> AppResult<ChatWithToolsResult> {
        // 1. 拆分 system / 对话消息（含 tool_use/tool_result 的多块 content）。
        // 调用方（run_agent_loop）已把工具结果作为 role=tool 的 ChatMessage 放进 messages，
        // 紧跟在带 tool_calls 的 assistant 消息之后。这里把它们合并成 Claude 要求的
        // user 消息（含 tool_result 块）。
        let mut system_parts: Vec<String> = Vec::new();
        let mut conv: Vec<Value> = Vec::with_capacity(messages.len());
        // 收集连续的 tool 结果，合并为一条 user 消息。
        let mut pending_tool_results: Vec<Value> = Vec::new();
        let mut tool_ids_in_messages: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        let flush_tool_results = |conv: &mut Vec<Value>, pending: &mut Vec<Value>| {
            if !pending.is_empty() {
                conv.push(json!({ "role": "user", "content": pending.clone() }));
                pending.clear();
            }
        };

        for m in &messages {
            match m.role {
                Role::System => system_parts.push(m.content.clone()),
                Role::User => {
                    flush_tool_results(&mut conv, &mut pending_tool_results);
                    conv.push(json!({ "role": "user", "content": m.content }));
                }
                Role::Assistant => {
                    flush_tool_results(&mut conv, &mut pending_tool_results);
                    if let Some(tcs) = &m.tool_calls {
                        // assistant 带 tool_use：content 是数组（text + tool_use 块）。
                        let mut blocks: Vec<Value> = Vec::new();
                        if !m.content.is_empty() {
                            blocks.push(json!({ "type": "text", "text": m.content }));
                        }
                        for tc in tcs {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.name,
                                "input": tc.arguments,
                            }));
                        }
                        conv.push(json!({ "role": "assistant", "content": blocks }));
                    } else {
                        conv.push(json!({ "role": "assistant", "content": m.content }));
                    }
                }
                Role::Tool => {
                    // tool 结果：合并进 pending，等遇到下一条非 tool 消息时 flush。
                    if let Some(id) = &m.tool_call_id {
                        tool_ids_in_messages.insert(id.clone());
                        pending_tool_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": m.content,
                        }));
                    }
                }
            }
        }
        // 兼容：若调用方仍通过 tool_results 参数传结果（且不在 messages 里），追加。
        for (id, r) in &tool_results {
            if tool_ids_in_messages.contains(id) {
                continue;
            }
            pending_tool_results.push(json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": r.output,
                "is_error": !r.ok,
            }));
        }
        flush_tool_results(&mut conv, &mut pending_tool_results);
        let system = if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n\n"))
        };

        // 2. tools → Claude 的 input_schema 格式。
        let tools_payload: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();

        let body = MessagesToolsRequest {
            model: &self.model,
            max_tokens: MAX_TOKENS,
            system: system.as_deref(),
            messages: conv,
            stream: true,
            tools: if tools_payload.is_empty() { None } else { Some(tools_payload) },
        };

        let url = format!("{}/v1/messages", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await;

        let response = match resp {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("连接 Claude 服务失败: {e}");
                emit_error(&app, &request_id, &msg);
                log::error!("[ai:{}:claude/tools] {msg}", request_id);
                return Err(AppError::Ai(msg));
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let msg = format!("Claude 返回错误状态 {status}: {}", truncate(&text, 500));
            emit_error(&app, &request_id, &msg);
            log::error!("[ai:{}:claude/tools] {msg}", request_id);
            return Err(AppError::Ai(msg));
        }

        // 3. SSE 流式解析：累积 text 与 tool_use（input_json_delta 分片拼接）。
        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut full_text = String::new();
        // index → 累积中的 tool_use 块。
        let mut tool_blocks: std::collections::BTreeMap<u32, ToolUseBuf> =
            std::collections::BTreeMap::new();
        let mut current_event: String = String::new();
        let mut done = false;

        while let Some(chunk_res) = stream.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    let msg = format!("读取 Claude 流式响应失败: {e}");
                    emit_error(&app, &request_id, &msg);
                    log::error!("[ai:{}:claude/tools] {msg}", request_id);
                    return Err(AppError::Ai(msg));
                }
            };
            buffer.extend_from_slice(&chunk[..]);

            while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
                let line = strip_line_ending(&line_bytes);
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                if let Some(ev) = line.strip_prefix("event:") {
                    current_event = ev.trim().to_string();
                    continue;
                }
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() {
                    continue;
                }

                match current_event.as_str() {
                    "content_block_start" => {
                        let parsed: ContentBlockStart =
                            match serde_json::from_str(payload) {
                                Ok(v) => v,
                                Err(e) => {
                                    log::warn!(
                                        "[ai:{}:claude/tools] 解析 content_block_start 失败: {e}",
                                        request_id
                                    );
                                    continue;
                                }
                            };
                        if parsed.content_block.type_ == "tool_use" {
                            let entry = tool_blocks.entry(parsed.index).or_default();
                            entry.id = parsed.content_block.id;
                            entry.name = parsed.content_block.name;
                        }
                    }
                    "content_block_delta" => {
                        let parsed: ContentBlockDelta =
                            match serde_json::from_str(payload) {
                                Ok(v) => v,
                                Err(e) => {
                                    log::warn!(
                                        "[ai:{}:claude/tools] 解析 content_block_delta 失败: {e}",
                                        request_id
                                    );
                                    continue;
                                }
                            };
                        match parsed.delta.type_.as_str() {
                            "text_delta" => {
                                if let Some(text) = parsed.delta.text {
                                    if !text.is_empty() {
                                        full_text.push_str(&text);
                                        emit_chunk(&app, &request_id, &text);
                                    }
                                }
                            }
                            "input_json_delta" => {
                                if let Some(idx) = tool_blocks.get_mut(&parsed.index) {
                                    if let Some(pj) = parsed.delta.partial_json {
                                        idx.input_buffer.push_str(&pj);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    "message_stop" => {
                        done = true;
                        break;
                    }
                    _ => {}
                }
            }

            if done {
                break;
            }
        }

        // 4. 构造 tool_calls（input JSON 解析）。
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        for (_, buf) in tool_blocks {
            let id = buf.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let name = buf.name.unwrap_or_default();
            let arguments = if buf.input_buffer.is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&buf.input_buffer).unwrap_or_else(|_| {
                    json!({ "_raw": buf.input_buffer })
                })
            };
            tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }

        Ok(ChatWithToolsResult {
            message: full_text,
            tool_calls,
        })
    }
}

// ---------------------------------------------------------------------------
// 辅助函数
// ---------------------------------------------------------------------------

/// 去掉行尾的 `\r\n` / `\n`，返回 UTF-8 字符串（非法字节 lossy 转换）。
fn strip_line_ending(bytes: &[u8]) -> String {
    let mut end = bytes.len();
    while end > 0 {
        match bytes[end - 1] {
            b'\n' | b'\r' => end -= 1,
            _ => break,
        }
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// 截断字符串到最多 `max` 字符（按 char），超出加省略号。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn emit_chunk(app: &tauri::AppHandle, request_id: &str, delta: &str) {
    events::emit(
        app,
        events::AI_CHUNK,
        AiChunkEvent {
            request_id: request_id.to_string(),
            delta: delta.to_string(),
        },
    );
}

fn emit_done(app: &tauri::AppHandle, request_id: &str, full_text: &str) {
    events::emit(
        app,
        events::AI_DONE,
        AiDoneEvent {
            request_id: request_id.to_string(),
            full_text: full_text.to_string(),
        },
    );
}

fn emit_error(app: &tauri::AppHandle, request_id: &str, message: &str) {
    events::emit(
        app,
        events::AI_ERROR,
        AiErrorEvent {
            request_id: request_id.to_string(),
            message: message.to_string(),
        },
    );
}

// ---------------------------------------------------------------------------
// chat_with_tools 专用的请求 / 流式响应 serde 结构
// ---------------------------------------------------------------------------

/// Messages API 顶层请求体（带工具）。
///
/// 与 `chat_stream` 的 `MessagesRequest` 区别：messages 是任意 JSON（user/assistant
/// 的 content 可能是字符串或数组），并多了 `tools` 字段。直接用 `serde_json::Value`
/// 构造，避免为 Claude 的复杂 content 结构建模。
#[derive(Debug, Serialize)]
struct MessagesToolsRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
}

/// `content_block_start` 事件的 data 载荷。
#[derive(Debug, Deserialize)]
struct ContentBlockStart {
    #[serde(default)]
    index: u32,
    #[serde(rename = "content_block", default)]
    content_block: ContentBlockInfo,
}

#[derive(Debug, Default, Deserialize)]
struct ContentBlockInfo {
    #[serde(rename = "type", default)]
    type_: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// `content_block_delta` 事件的 data 载荷。
#[derive(Debug, Deserialize)]
struct ContentBlockDelta {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    delta: DeltaInfo,
}

#[derive(Debug, Default, Deserialize)]
struct DeltaInfo {
    #[serde(rename = "type", default)]
    type_: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
}

/// 累积中的 tool_use 块。
#[derive(Default)]
struct ToolUseBuf {
    id: Option<String>,
    name: Option<String>,
    input_buffer: String,
}
