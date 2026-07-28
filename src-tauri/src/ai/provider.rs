//! Provider 抽象层。
//!
//! 本模块定义与具体厂商解耦的对话抽象：消息结构（[`Role`] / [`ChatMessage`]）、
//! 厂商类型（[`ProviderKind`]）、配置结构（[`ProviderConfig`]）以及统一的
//! [`LlmProvider`] trait。具体实现见 [`crate::ai::openai`] 与 [`crate::ai::claude`]。
//!
//! 调用方典型流程：
//! ```ignore
//! let cfg = ProviderConfig { kind: ProviderKind::DeepSeek, .. };
//! let provider = build_provider(&cfg)?;
//! let full = provider.chat_stream(messages, request_id, app_handle).await?;
//! ```

use async_trait::async_trait;

use crate::ai::tools::{ToolCall, ToolDef, ToolResult};
use crate::error::{AppError, AppResult};

// ===========================================================================
// 对话消息
// ===========================================================================

/// 对话消息的角色。
///
/// 序列化后分别为字符串 `"system"` / `"user"` / `"assistant"` / `"tool"`，符合
/// OpenAI 与 Claude 等主流厂商的 API 约定（`tool` 用于工具调用结果回填）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 系统指令，用于设定模型行为。
    System,
    /// 用户消息。
    User,
    /// 模型（助手）的回复。
    Assistant,
    /// 工具调用结果（Function Calling 回填）。
    Tool,
}

/// 一条对话消息。
///
/// `role` 决定 `content` 在请求体中的归属（system / user / assistant）。
///
/// 工具调用相关字段：
/// - `tool_calls`：仅当 `role=Assistant` 且本轮模型请求了工具调用时存在。具体厂商
///   （OpenAI / Claude）在序列化时会按各自协议转换格式。
/// - `tool_call_id`：仅当 `role=Tool` 时存在，用于把工具结果回填给对应的 tool_call。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// 仅当 role=Assistant 且本轮有工具调用时存在。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// 仅当 role=Tool 时存在（对应的 tool_call id）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// 构造一条普通消息（无工具调用相关字段）。
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

// ===========================================================================
// Provider 类型与配置
// ===========================================================================

/// 支持的 LLM 厂商种类。
///
/// - [`ProviderKind::OpenAi`]、[`ProviderKind::DeepSeek`]、[`ProviderKind::Zhipu`]、
///   [`ProviderKind::Ollama`]、[`ProviderKind::OpenAiCompatible`] 均使用 OpenAI
///   兼容协议，由 [`crate::ai::openai::OpenAiProvider`] 承载。
/// - [`ProviderKind::Anthropic`] 使用 Claude 原生 Messages 协议，由
///   [`crate::ai::claude::ClaudeProvider`] 承载。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
// 序列化值必须与前端 `src/api/types.ts` 的 `ProviderKind` 枚举字符串完全一致：
// openai / anthropic / deepseek / zhipu / ollama / openai_compatible。
// 之前用 `rename_all = "lowercase"` 会把 OpenAiCompatible 序列化成 "openaicompatible"
// （无分隔符），与前端 "openai_compatible" 对不上；而 `snake_case` 又会把 OpenAi
// 转成 "open_ai"（识别大写边界）。因此改为逐变体显式 rename，确保精确匹配前端。
pub enum ProviderKind {
    /// OpenAI 官方（gpt-3.5/gpt-4 系列）。
    #[serde(rename = "openai")]
    OpenAi,
    /// Anthropic Claude（原生 Messages API）。
    #[serde(rename = "anthropic")]
    Anthropic,
    /// DeepSeek（OpenAI 兼容协议）。
    #[serde(rename = "deepseek")]
    DeepSeek,
    /// 智谱 GLM（OpenAI 兼容协议）。
    #[serde(rename = "zhipu")]
    Zhipu,
    /// 本地 Ollama（OpenAI 兼容协议）。
    #[serde(rename = "ollama")]
    Ollama,
    /// 任意 OpenAI 兼容端点（用户自填 base_url）。
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
}

impl ProviderKind {
    /// 返回用于序列化/配置文件的小写字符串。
    ///
    /// 值与 serde 序列化及前端枚举保持一致：`openai_compatible`（下划线）。
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::OpenAi => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::DeepSeek => "deepseek",
            ProviderKind::Zhipu => "zhipu",
            ProviderKind::Ollama => "ollama",
            ProviderKind::OpenAiCompatible => "openai_compatible",
        }
    }

    /// 从字符串解析厂商类型，大小写、首尾空白均容错。
    ///
    /// 注意：这里有意不实现 `std::str::FromStr`，因为该 trait 要求返回 `Result`
    /// 而非 `Option`，与配置解析时「未知种类返回 None 由上层报错」的语义更契合。
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai" => Some(ProviderKind::OpenAi),
            "anthropic" | "claude" => Some(ProviderKind::Anthropic),
            "deepseek" => Some(ProviderKind::DeepSeek),
            "zhipu" | "glm" => Some(ProviderKind::Zhipu),
            "ollama" => Some(ProviderKind::Ollama),
            "openai-compatible" | "openai_compatible" | "compatible" => {
                Some(ProviderKind::OpenAiCompatible)
            }
            _ => None,
        }
    }

    /// 该厂商对应的默认 base_url（`OpenAiCompatible` 无默认值，返回空串）。
    pub fn default_base_url(&self) -> &'static str {
        match self {
            ProviderKind::OpenAi => "https://api.openai.com/v1",
            ProviderKind::Anthropic => "https://api.anthropic.com",
            ProviderKind::DeepSeek => "https://api.deepseek.com/v1",
            ProviderKind::Zhipu => "https://open.bigmodel.cn/api/paas/v4",
            ProviderKind::Ollama => "http://localhost:11434/v1",
            ProviderKind::OpenAiCompatible => "",
        }
    }
}

/// 单个 Provider 的连接配置。
///
/// 该结构会被持久化到用户配置（前端可编辑），因此所有字段均可序列化。
// `rename_all = "camelCase"`：前端 (`src/api/types.ts` 的 `ProviderConfig`) 使用
// baseUrl / apiKey（camelCase）。之前这里漏了 rename，导致后端期望 base_url/api_key
// （snake_case），前端发来 baseUrl 时 serde 报 "missing field `base_url`"。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    /// 厂商种类，决定路由到哪个具体实现。
    pub kind: ProviderKind,
    /// API base_url，末尾不要带 `/`。若用户未填，可用 [`ProviderKind::default_base_url`]。
    pub base_url: String,
    /// API Key（Ollama 等本地服务可留空）。
    pub api_key: String,
    /// 模型名，如 `gpt-4o-mini`、`claude-3-5-sonnet-20241022`、`deepseek-chat`。
    pub model: String,
}

// ===========================================================================
// 工具调用相关类型
// ===========================================================================

/// `chat_with_tools` 的返回结果。
///
/// - `message`：模型本轮的文本回复（已通过 `ai:chunk` 事件流式推送给前端）。
/// - `tool_calls`：模型本轮请求的工具调用；为空表示本轮是纯文本回复。
#[derive(Debug, Clone, Default)]
pub struct ChatWithToolsResult {
    pub message: String,
    pub tool_calls: Vec<ToolCall>,
}

// ===========================================================================
// Provider trait
// ===========================================================================

/// 统一的 LLM Provider 接口。
///
/// 实现方负责把 `messages` 发往对应厂商的流式接口，并在响应过程中：
/// - 每得到一段增量文本，发射 `ai:chunk` 事件（payload: [`crate::events::AiChunkEvent`]）；
/// - 正常结束时发射 `ai:done` 事件（payload: [`crate::events::AiDoneEvent`]），并返回完整文本；
/// - 任何阶段出错时发射 `ai:error` 事件（payload: [`crate::events::AiErrorEvent`]），并返回 `Err`。
///
/// `request_id` 由调用方生成，用于把事件与原始请求一一对应。
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// 发起一次流式对话，返回完整文本。
    async fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
        request_id: String,
        app: tauri::AppHandle,
    ) -> AppResult<String>;

    /// 带工具的对话。
    ///
    /// 与 `chat_stream` 的区别：
    /// - `tools`：本轮暴露给模型的工具集合。为空时等价于普通对话（模型不会调用工具）。
    /// - `tool_results`：上一轮工具调用的结果（已按 `tool_call_id` 配对）。调用方
    ///   需要把上一轮的 assistant 消息（含 `tool_calls`）append 到 `messages`，
    ///   再把本轮 `tool_results` 传入；本方法负责把结果序列化为厂商约定的
    ///   "role=tool" / "tool_result" 消息塞进请求体。
    ///
    /// 流式过程中每得到一段文本增量都发射 `ai:chunk`（与 `chat_stream` 一致），
    /// 但**不**发射 `ai:done`——`ai:done` 由编排循环在最终（无工具调用）回合发出，
    /// 因为带工具的对话可能跨多轮。
    ///
    /// 返回值：本轮文本 + 工具调用列表。
    async fn chat_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDef>,
        tool_results: Vec<(String, ToolResult)>,
        request_id: String,
        app: tauri::AppHandle,
    ) -> AppResult<ChatWithToolsResult>;
}

// ===========================================================================
// 工厂函数
// ===========================================================================

/// 根据配置构造对应的 Provider 实现。
///
/// 路由规则：
/// - `Anthropic` → [`crate::ai::claude::ClaudeProvider`]
/// - 其它所有种类（OpenAi / DeepSeek / Zhipu / Ollama / OpenAiCompatible）
///   → [`crate::ai::openai::OpenAiProvider`]（OpenAI 兼容协议）
pub fn build_provider(cfg: &ProviderConfig) -> AppResult<Box<dyn LlmProvider>> {
    match cfg.kind {
        ProviderKind::Anthropic => Ok(Box::new(crate::ai::claude::ClaudeProvider::new(cfg))),
        _ => Ok(Box::new(crate::ai::openai::OpenAiProvider::new(cfg))),
    }
}

/// 配置合法性检查的便捷入口（可选使用）。
///
/// 当前实现非常保守：仅校验 base_url/model 非空；本地 Ollama 允许空 key。
#[allow(dead_code)]
pub fn validate_config(cfg: &ProviderConfig) -> AppResult<()> {
    if cfg.base_url.trim().is_empty() && cfg.kind != ProviderKind::OpenAiCompatible {
        return Err(AppError::Config(format!(
            "provider `{}` 缺少 base_url",
            cfg.kind.as_str()
        )));
    }
    if cfg.model.trim().is_empty() {
        return Err(AppError::Config("model 不能为空".into()));
    }
    if cfg.api_key.trim().is_empty() && cfg.kind != ProviderKind::Ollama {
        return Err(AppError::Config(format!(
            "provider `{}` 缺少 api_key",
            cfg.kind.as_str()
        )));
    }
    Ok(())
}
