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
    /// 单次请求最大输出 tokens（请求体 `max_tokens`；0 表示不发送，由服务端默认）。
    max_output: u32,
    /// 采样温度（`None` 不发送）。
    temperature: Option<f32>,
}

impl OpenAiProvider {
    /// 从配置构造实例。`base_url` 末尾的 `/` 会被去除以便后续拼接路径。
    pub fn new(cfg: &ProviderConfig) -> Self {
        Self {
            // 超时取自模型配置（connect_timeout_secs / read_timeout_secs，0=默认）：
            // connect_timeout 防 DNS/建连挂死；read_timeout 兜底防止连接假死后
            // 流式读取无限阻塞。长思考模型可调大 read_timeout。
            client: reqwest::Client::builder()
                .connect_timeout(crate::ai::provider::timeout_secs(
                    cfg.connect_timeout_secs,
                    crate::ai::provider::DEFAULT_CONNECT_TIMEOUT_SECS,
                ))
                .read_timeout(crate::ai::provider::timeout_secs(
                    cfg.read_timeout_secs,
                    crate::ai::provider::DEFAULT_READ_TIMEOUT_SECS,
                ))
                .build()
                .expect("构造 reqwest Client 失败"),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            max_output: cfg.max_output,
            temperature: cfg.temperature,
        }
    }
}

// ---------------------------------------------------------------------------
// 用于构造请求体 / 解析响应的 serde 结构
// ---------------------------------------------------------------------------

/// 消息 content：纯文本字符串，或「文本 + 图片」块数组（多模态模型）。
/// `#[serde(untagged)]` 使字符串变体序列化为 JSON 字符串、数组变体序列化为数组，
/// 与 OpenAI 的 content 两种合法形态一一对应。
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ReqContent {
    Text(String),
    Parts(Vec<ReqContentPart>),
}

/// content 数组中的单个块（text / image_url 二选一）。
#[derive(Debug, Serialize)]
struct ReqContentPart {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_url: Option<ImageUrl>,
}

/// image_url 块：OpenAI 兼容协议用 data URL 内联图片内容。
#[derive(Debug, Serialize)]
struct ImageUrl {
    url: String,
}

/// 把通用 [`ChatMessage`] 的 content 转成 OpenAI 的 content 形态：
/// 无图片 → 纯文本字符串；有图片 → 文本 + image_url 块数组（图片拼 data URL）。
fn openai_content(m: &ChatMessage) -> ReqContent {
    match &m.images {
        Some(imgs) if !imgs.is_empty() => {
            let mut parts = Vec::with_capacity(imgs.len() + 1);
            if !m.content.is_empty() {
                parts.push(ReqContentPart {
                    kind: "text",
                    text: Some(m.content.clone()),
                    image_url: None,
                });
            }
            for img in imgs {
                parts.push(ReqContentPart {
                    kind: "image_url",
                    text: None,
                    image_url: Some(ImageUrl {
                        url: format!("data:{};base64,{}", img.mime_type, img.data_base64),
                    }),
                });
            }
            ReqContent::Parts(parts)
        }
        _ => ReqContent::Text(m.content.clone()),
    }
}

/// 请求体中的消息项（字段顺序与 OpenAI 一致，便于阅读抓包）。
#[derive(Debug, Serialize)]
struct ReqMessage<'a> {
    role: &'a str,
    content: ReqContent,
}

/// 顶层请求体。
#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ReqMessage<'a>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

/// 流式 chunk 顶层结构。
#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<Choice>,
    /// 用量统计（OpenAI 在流末尾带 finish_reason 的 chunk 上携带）。
    /// 用 `Value` 容错接收：部分兼容服务器的 usage 格式不标准（缺字段/类型
    /// 异常），强类型反序列化失败会连带丢掉整个 chunk 的**文本**——那是不可
    /// 接受的。数字提取见 [`extract_usage`]。
    #[serde(default)]
    usage: Option<serde_json::Value>,
}

/// 从 OpenAI usage 字段手动提取 `(prompt_tokens, completion_tokens)`。
/// 字段缺失按 0 计；两者都取不到（或均为 0）返回 None（不发事件）。
fn extract_usage(u: &serde_json::Value) -> Option<(u64, u64)> {
    let prompt = u.get("prompt_tokens").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let completion = u.get("completion_tokens").and_then(serde_json::Value::as_u64).unwrap_or(0);
    if prompt == 0 && completion == 0 {
        None
    } else {
        Some((prompt, completion))
    }
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
        // 构造请求体。借用 messages 的 role，content 按需 clone（图片块要拼 data URL）。
        let req_messages: Vec<ReqMessage> = messages
            .iter()
            .map(|m| ReqMessage {
                role: role_str(m.role),
                content: openai_content(m),
            })
            .collect();
        let body = ChatRequest {
            model: &self.model,
            messages: req_messages,
            stream: true,
            max_tokens: (self.max_output > 0).then_some(self.max_output),
            temperature: self.temperature,
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
        // token 用量（token-meter）：兼容服务器可能在多个 chunk 携带 usage（中间
        // 的可能是部分累计值），只保留**最后一次**、流结束时统一发射一次，避免
        // 前端重复累计；不返回 usage 的服务器则完全不发。
        let mut last_usage: Option<(u64, u64)> = None;

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
                    // 流结束：发射最后捕获的 usage 后收尾（直接 return，
                    // 循环末尾的补发块不会执行）。
                    if let Some((p, c)) = last_usage {
                        emit_usage(&app, &request_id, p, c);
                    }
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
                // usage 只在流末尾的 chunk 出现（token-meter，借鉴 dsh llm/token-meter）。
                // 用 Value 容错提取并记住最后一次，流结束时统一发射（见 [DONE] /
                // 循环末尾），不在此处立即发射——兼容服务器可能多次携带 usage，
                // 立即发射会导致前端重复累计。
                if let Some(u) = parsed.usage.as_ref().and_then(extract_usage) {
                    last_usage = Some(u);
                }
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
        // （[DONE] 分支已直接 return，不存在双发。）
        if let Some((p, c)) = last_usage {
            emit_usage(&app, &request_id, p, c);
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
            .filter_map(|m| m.tool_call_id)
            .map(str::to_string)
            .collect();
        for (tool_call_id, result) in &tool_results {
            if existing_ids.contains(tool_call_id) {
                continue;
            }
            oa_messages.push(OpenAiMessage {
                role: "tool",
                content: ReqContent::Text(result.output.clone()),
                tool_calls: None,
                tool_call_id: Some(tool_call_id.as_str()),
            });
        }

        // 2. 构造请求体。
        let body = ChatToolsRequest {
            model: &self.model,
            messages: oa_messages,
            stream: true,
            max_tokens: (self.max_output > 0).then_some(self.max_output),
            temperature: self.temperature,
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
                log::error!("[ai:{}:openai/tools] {msg}", request_id);
                return Err(AppError::Ai(msg));
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let msg = format!("LLM 返回错误状态 {status}: {}", truncate(&text, 500));
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
        // token 用量（token-meter）：与 chat_stream 同策略——记住最后一次、
        // 流结束时统一发射一次（兼容服务器可能多次携带 usage）。
        let mut last_usage: Option<(u64, u64)> = None;
        let mut usage_emitted = false;
        // [DONE] 是流结束标记，但只出现在某一行内：需跳出内层行循环后
        // 再跳出外层 chunk 循环，否则残留 buffer 会被继续解析。
        let mut done = false;

        while let Some(chunk_res) = stream.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    let msg = format!("读取流式响应失败: {e}");
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
                    if !usage_emitted {
                        if let Some((p, c)) = last_usage {
                            emit_usage(&app, &request_id, p, c);
                        }
                        usage_emitted = true;
                    }
                    done = true;
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
                // token 用量（token-meter）：Value 容错提取 + 记住最后一次，
                // 流结束时统一发射（见 [DONE] / 循环末尾）。
                if let Some(u) = parsed.usage.as_ref().and_then(extract_usage) {
                    last_usage = Some(u);
                }
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

            if done {
                break;
            }
        }

        // 流自然结束（未收到 [DONE]）：补发 usage（若未发过）。
        if !usage_emitted {
            if let Some((p, c)) = last_usage {
                emit_usage(&app, &request_id, p, c);
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

/// 发射 ai:usage 事件（单次请求的 token 用量，前端按会话累计）。
fn emit_usage(app: &tauri::AppHandle, request_id: &str, prompt_tokens: u64, completion_tokens: u64) {
    events::emit(
        app,
        events::AI_USAGE,
        crate::events::AiUsageEvent {
            request_id: request_id.to_string(),
            prompt_tokens,
            completion_tokens,
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
struct OpenAiMessage<'a> {
    role: &'a str,
    content: ReqContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiAssistantToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

impl<'a> OpenAiMessage<'a> {
    /// 把通用 `ChatMessage` 转成 OpenAI 消息。
    fn from_chat(m: &'a ChatMessage) -> Self {
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
            role,
            content: openai_content(m),
            tool_calls,
            tool_call_id: m.tool_call_id.as_deref(),
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
    messages: Vec<OpenAiMessage<'a>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
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
    /// 用量统计（流末尾 chunk 携带；Value 容错，同 [`StreamChunk::usage`]）。
    #[serde(default)]
    usage: Option<serde_json::Value>,
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

// ---------------------------------------------------------------------------
// 单元测试：多模态 content 序列化
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::provider::{ChatMessage, ImagePart, Role};

    fn img() -> ImagePart {
        ImagePart {
            mime_type: "image/png".into(),
            data_base64: "AAAA".into(),
        }
    }

    /// 无图片：content 保持纯文本字符串。
    #[test]
    fn plain_text_content() {
        let m = ChatMessage::new(Role::User, "hello");
        assert_eq!(serde_json::to_string(&openai_content(&m)).unwrap(), r#""hello""#);
    }

    /// 带图片：content 序列化为 text + image_url 块数组（data URL）。
    #[test]
    fn image_content_parts() {
        let m = ChatMessage {
            role: Role::User,
            content: "看看这张图".into(),
            tool_calls: None,
            tool_call_id: None,
            images: Some(vec![img()]),
        };
        let json = serde_json::to_string(&openai_content(&m)).unwrap();
        assert!(json.contains(r#"{"type":"text","text":"看看这张图"}"#), "{json}");
        assert!(
            json.contains(r#"{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}"#),
            "{json}"
        );
    }

    /// 完整请求体：多模态消息转成 OpenAI /chat/completions 请求体。
    #[test]
    fn full_request_body() {
        let m = ChatMessage {
            role: Role::User,
            content: "这是什么".into(),
            tool_calls: None,
            tool_call_id: None,
            images: Some(vec![img()]),
        };
        let body = ChatRequest {
            model: "gpt-4o",
            messages: vec![ReqMessage {
                role: "user",
                content: openai_content(&m),
            }],
            stream: true,
            max_tokens: Some(100),
            temperature: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains(r#""content":[{"type":"text""#), "{json}");
        assert!(json.contains(r#""type":"image_url""#), "{json}");
    }

    /// 流末尾 chunk 的 usage 可解析（token-meter 数据源）。
    #[test]
    fn parses_stream_usage() {
        let json = r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":34,"total_tokens":46}}"#;
        let parsed: StreamChunk = serde_json::from_str(json).unwrap();
        let (p, c) = parsed.usage.as_ref().and_then(extract_usage).expect("usage 应被解析");
        assert_eq!((p, c), (12, 34));
    }

    /// 厂商不返回 usage 时缺省为 None（不 panic、不报错）。
    #[test]
    fn stream_usage_optional() {
        let json = r#"{"choices":[{"delta":{"content":"hi"}}]}"#;
        let parsed: StreamChunk = serde_json::from_str(json).unwrap();
        assert!(parsed.usage.is_none());
    }

    /// usage 格式异常（缺字段/类型不对）时**不影响文本 chunk 解析**（Value 容错），
    /// 提取按缺省 0 计，全 0 时返回 None。
    #[test]
    fn malformed_usage_never_kills_chunk() {
        // usage 是数组（完全非标准）：chunk 仍能解析出文本。
        let json = r#"{"choices":[{"delta":{"content":"hi"}}],"usage":[1,2]}"#;
        let parsed: StreamChunk = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.choices.len(), 1);
        // usage 缺 completion_tokens：只提取到 prompt。
        let parsed: StreamChunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":5}}"#,
        ).unwrap();
        assert_eq!(
            parsed.usage.as_ref().and_then(extract_usage),
            Some((5, 0))
        );
        // usage 为 null：视为无。
        let parsed: StreamChunk = serde_json::from_str(r#"{"choices":[],"usage":null}"#).unwrap();
        assert!(parsed.usage.is_none());
    }

    /// tools 版 chunk 同样携带 usage（agent 多轮场景的用量来源）。
    #[test]
    fn parses_stream_tools_usage() {
        let json = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"x"}]}}],"usage":{"prompt_tokens":5,"completion_tokens":7}}"#;
        let parsed: StreamChunkTools = serde_json::from_str(json).unwrap();
        let (p, c) = parsed.usage.as_ref().and_then(extract_usage).expect("usage 应被解析");
        assert_eq!((p, c), (5, 7));
        // usage 在流末尾 chunk，与 choices 并存时互不影响。
        assert_eq!(parsed.choices.len(), 1);
    }
}
