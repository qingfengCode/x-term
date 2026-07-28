//! OpenAI 兼容协议的 LLM Provider 实现。
//!
//! 适用于 OpenAI 官方以及任何提供 OpenAI 兼容 `/chat/completions` 接口的厂商，
//! 例如 DeepSeek、智谱 GLM、本地 Ollama、以及任意自托管的兼容端点。
//!
//! # 流式协议要点
//!
//! OpenAI 的流式响应使用 SSE（Server-Sent Events）格式，每行形如：
//!
//! ```text
//! data: {"choices":[{"delta":{"content":"Hello"}}]}
//!
//! data: {"choices":[{"delta":{"content":" world"}}]}
//!
//! data: [DONE]
//! ```
//!
//! 本模块按行解析，逐段把 `delta.content` 通过 `ai:chunk` 事件推给前端，
//! 直到读到 `[DONE]` 标记为止。

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::ai::provider::{ChatMessage, ChatWithToolsResult, LlmProvider, ProviderConfig, Role};
use crate::ai::tools::{ToolCall, ToolDef, ToolResult};
use crate::error::{AppError, AppResult};
use crate::events::{self, AiChunkEvent, AiDoneEvent, AiErrorEvent};

/// OpenAI 兼容协议的 Provider。
///
/// 一个实例对应一组固定的 (base_url, api_key, model)；不同模型/Key 请构造多个实例。
pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAiProvider {
    /// 从配置构造实例。`base_url` 末尾的 `/` 会被去除以便后续拼接路径。
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
// 用于构造请求体 / 解析响应的 serde 结构
// ---------------------------------------------------------------------------

/// 请求体中的消息项（字段顺序与 OpenAI 一致，便于阅读抓包）。
#[derive(Debug, Serialize)]
struct ReqMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// 顶层请求体。
#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ReqMessage<'a>>,
    stream: bool,
}

/// 流式 chunk 顶层结构。
#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<Choice>,
}

/// 单个 choice。
#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
}

/// 增量内容。
#[derive(Debug, Default, Deserialize)]
struct Delta {
    /// 首个 chunk 通常没有 content（只有 role），故允许缺省。
    #[serde(default)]
    content: Option<String>,
}

// ---------------------------------------------------------------------------
// LlmProvider 实现
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
        request_id: String,
        app: tauri::AppHandle,
    ) -> AppResult<String> {
        // 构造请求体。借用 messages 的 content，避免无谓 clone。
        let req_messages: Vec<ReqMessage> = messages
            .iter()
            .map(|m| ReqMessage {
                role: role_str(m.role),
                content: m.content.as_str(),
            })
            .collect();
        let body = ChatRequest {
            model: &self.model,
            messages: req_messages,
            stream: true,
        };

        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await;

        let response = match resp {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("连接 LLM 服务失败: {e}");
                emit_error(&app, &request_id, &msg);
                log::error!("[ai:{}:openai] {msg}", request_id);
                return Err(AppError::Ai(msg));
            }
        };

        // 非 2xx：读取错误体一并报出。
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let msg = format!("LLM 返回错误状态 {status}: {}", truncate(&text, 500));
            emit_error(&app, &request_id, &msg);
            log::error!("[ai:{}:openai] {msg}", request_id);
            return Err(AppError::Ai(msg));
        }

        // 流式读取并解析 SSE。
        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut full_text = String::new();

        while let Some(chunk_res) = stream.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    let msg = format!("读取流式响应失败: {e}");
                    emit_error(&app, &request_id, &msg);
                    log::error!("[ai:{}:openai] {msg}", request_id);
                    return Err(AppError::Ai(msg));
                }
            };
            buffer.extend_from_slice(&chunk[..]);

            // 按换行切分；最后一段可能不完整，留在 buffer 中。
            while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
                // 去掉行尾的 \n 与可能存在的 \r。
                let line = strip_line_ending(&line_bytes);
                if line.is_empty() {
                    continue;
                }
                // SSE 行以 "data: " 前缀开头。
                let Some(payload) = line.strip_prefix("data:") else {
                    // 非 data 行（注释、event、id 等），忽略。
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() {
                    continue;
                }
                if payload == "[DONE]" {
                    emit_done(&app, &request_id, &full_text);
                    return Ok(full_text);
                }
                // 解析 JSON。
                let parsed: StreamChunk = match serde_json::from_str(payload) {
                    Ok(c) => c,
                    Err(e) => {
                        // 单行解析失败不应整体崩溃，记录后继续（兼容部分厂商的非标准行）。
                        log::warn!(
                            "[ai:{}:openai] 解析 SSE 行失败: {e}; line={}",
                            request_id,
                            truncate(payload, 200)
                        );
                        continue;
                    }
                };
                if let Some(choice) = parsed.choices.into_iter().next() {
                    if let Some(delta) = choice.delta.content {
                        if !delta.is_empty() {
                            full_text.push_str(&delta);
                            emit_chunk(&app, &request_id, &delta);
                        }
                    }
                }
            }
        }

        // 流自然结束但未收到 [DONE]：只要已经产出内容，视为正常完成。
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
        // 1. 把通用消息转换为 OpenAI 格式。
        // 注意：调用方（run_agent_loop）已把工具结果作为 role=tool 的 ChatMessage
        // 放进 messages 中（紧跟带 tool_calls 的 assistant 消息），这里直接转换即可，
        // 不再从 tool_results 参数追加——否则会重复，且历史轮次会缺失 tool 消息导致 400。
        let mut oa_messages: Vec<OpenAiMessage> = Vec::with_capacity(messages.len());
        for m in &messages {
            oa_messages.push(OpenAiMessage::from_chat(m));
        }
        // 兼容：若调用方仍通过 tool_results 传结果（messages 里没有），则追加。
        // 用 tool_call_id 判断是否已在 messages 中。
        let existing_ids: std::collections::HashSet<String> = oa_messages
            .iter()
            .filter_map(|m| m.tool_call_id.clone())
            .collect();
        for (tool_call_id, result) in &tool_results {
            if existing_ids.contains(tool_call_id) {
                continue;
            }
            oa_messages.push(OpenAiMessage {
                role: "tool".into(),
                content: result.output.clone(),
                tool_calls: None,
                tool_call_id: Some(tool_call_id.clone()),
            });
        }

        // 2. 构造请求体。
        let body = ChatToolsRequest {
            model: &self.model,
            messages: oa_messages,
            stream: true,
            tools: if tools.is_empty() {
                None
            } else {
                Some(
                    tools
                        .iter()
                        .map(|t| OpenAiTool {
                            kind: "function",
                            function: OpenAiFunction {
                                name: t.name.as_str(),
                                description: t.description.as_str(),
                                parameters: &t.parameters,
                            },
                        })
                        .collect(),
                )
            },
            tool_choice: if tools.is_empty() { None } else { Some("auto") },
        };

        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await;

        let response = match resp {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("连接 LLM 服务失败: {e}");
                emit_error(&app, &request_id, &msg);
                log::error!("[ai:{}:openai/tools] {msg}", request_id);
                return Err(AppError::Ai(msg));
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let msg = format!("LLM 返回错误状态 {status}: {}", truncate(&text, 500));
            emit_error(&app, &request_id, &msg);
            log::error!("[ai:{}:openai/tools] {msg}", request_id);
            return Err(AppError::Ai(msg));
        }

        // 3. SSE 流式解析（增量文本 + 增量 tool_calls）。
        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut text_buffer = String::new();
        // index → (id, name, arguments_buffer)
        let mut tool_buffers: BTreeMap<u32, ToolBuf> = BTreeMap::new();
        let mut finish_reason: Option<String> = None;

        while let Some(chunk_res) = stream.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    let msg = format!("读取流式响应失败: {e}");
                    emit_error(&app, &request_id, &msg);
                    log::error!("[ai:{}:openai/tools] {msg}", request_id);
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
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() {
                    continue;
                }
                if payload == "[DONE]" {
                    // 流结束。
                    break;
                }
                let parsed: StreamChunkTools = match serde_json::from_str(payload) {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!(
                            "[ai:{}:openai/tools] 解析 SSE 行失败: {e}; line={}",
                            request_id,
                            truncate(payload, 200)
                        );
                        continue;
                    }
                };
                if let Some(choice) = parsed.choices.into_iter().next() {
                    if let Some(reason) = choice.finish_reason {
                        finish_reason = Some(reason);
                    }
                    let delta = choice.delta;
                    if let Some(content) = delta.content {
                        if !content.is_empty() {
                            text_buffer.push_str(&content);
                            emit_chunk(&app, &request_id, &content);
                        }
                    }
                    if let Some(tcs) = delta.tool_calls {
                        for tc in tcs {
                            let entry = tool_buffers.entry(tc.index).or_default();
                            if let Some(id) = tc.id {
                                entry.id = Some(id);
                            }
                            if let Some(f) = tc.function {
                                if !f.name.is_empty() {
                                    entry.name = Some(f.name);
                                }
                                if !f.arguments.is_empty() {
                                    entry.arguments.push_str(&f.arguments);
                                }
                            }
                        }
                    }
                }
            }
        }

        // 4. 构造 tool_calls（arguments 解析为 JSON Value）。
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        for (_, buf) in tool_buffers {
            let id = buf.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let name = buf.name.unwrap_or_default();
            let arguments = if buf.arguments.is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&buf.arguments).unwrap_or_else(|_| {
                    // 解析失败：把原始字符串包成 {"_raw": ...} 让模型后续自纠。
                    serde_json::json!({ "_raw": buf.arguments })
                })
            };
            tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }

        // finish_reason == "tool_calls" 表示本轮有工具调用；但即使没拿到该字段，
        // 只要 tool_calls 非空也按工具调用处理（兼容各厂商）。
        let _ = finish_reason; // 仅用于日志，不影响逻辑。

        Ok(ChatWithToolsResult {
            message: text_buffer,
            tool_calls,
        })
    }
}

// ---------------------------------------------------------------------------
// 辅助函数
// ---------------------------------------------------------------------------

/// 返回 Role 对应的 OpenAI 协议字符串字面量。
fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// 去掉行尾的 `\r\n` / `\n`，返回 UTF-8 字符串（非法字节 lossy 转换，避免崩溃）。
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

/// 把字符串截断到最多 `max` 个字符（按 char 计数），超出加省略号。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// 发射 ai:chunk 事件。
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

/// 发射 ai:done 事件。
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

/// 发射 ai:error 事件。
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

/// OpenAI 协议消息（含工具调用相关字段）。
///
/// 把通用 [`ChatMessage`] 转换成 OpenAI 要求的格式：
/// - assistant 带 tool_calls 时，序列化为 `{role, content, tool_calls: [{id, type:"function", function:{name, arguments}}]}`。
/// - tool 结果时，序列化为 `{role:"tool", content, tool_call_id}`。
#[derive(Debug, Serialize)]
struct OpenAiMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiAssistantToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl OpenAiMessage {
    /// 把通用 `ChatMessage` 转成 OpenAI 消息。
    fn from_chat(m: &ChatMessage) -> Self {
        let role = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let tool_calls = m.tool_calls.as_ref().map(|tcs| {
            tcs.iter()
                .map(|tc| OpenAiAssistantToolCall {
                    id: tc.id.clone(),
                    kind: "function",
                    function: OpenAiFunctionCall {
                        name: tc.name.clone(),
                        // arguments 必须是 JSON 字符串。
                        arguments: tc.arguments.to_string(),
                    },
                })
                .collect::<Vec<_>>()
        });
        OpenAiMessage {
            role: role.into(),
            content: m.content.clone(),
            tool_calls,
            tool_call_id: m.tool_call_id.clone(),
        }
    }
}

/// assistant 消息中的工具调用项。
#[derive(Debug, Serialize)]
struct OpenAiAssistantToolCall {
    id: String,
    /// 固定 "function"。
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiFunctionCall,
}

#[derive(Debug, Serialize)]
struct OpenAiFunctionCall {
    name: String,
    /// JSON 字符串。
    arguments: String,
}

/// 顶层带工具的请求体。
#[derive(Debug, Serialize)]
struct ChatToolsRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAiTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
}

/// 顶层 `tools` 数组元素。
#[derive(Debug, Serialize)]
struct OpenAiTool<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    function: OpenAiFunction<'a>,
}

#[derive(Debug, Serialize)]
struct OpenAiFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

// --- 流式响应 ---

/// 流式 chunk（tools 版，含 tool_calls 增量）。
#[derive(Debug, Deserialize)]
struct StreamChunkTools {
    #[serde(default)]
    choices: Vec<ChoiceTools>,
}

#[derive(Debug, Deserialize)]
struct ChoiceTools {
    #[serde(default)]
    delta: DeltaTools,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DeltaTools {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct DeltaToolCall {
    /// 在累积数组中的下标。
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Debug, Default, Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

/// 累积中的单个工具调用缓冲。
#[derive(Default)]
struct ToolBuf {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}
