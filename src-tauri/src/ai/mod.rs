//! AI 助手模块：多模型 LLM 接入。
//!
//! 本模块统一封装 X-Term 与各类大语言模型（LLM）的交互。整体设计围绕一个
//! [`provider::LlmProvider`] trait 展开：所有具体的厂商实现都满足该 trait，
//! 上层调用方只需要构造 [`provider::ProviderConfig`] 并通过
//! [`provider::build_provider`] 拿到一个 `Box<dyn LlmProvider>` 即可发起对话，
//! 无需关心底层协议差异。
//!
//! # 子模块组织
//!
//! - `provider`: 定义对话消息结构、Provider 类型枚举、配置结构以及
//!   `LlmProvider` trait 与工厂函数。
//! - `openai`: OpenAI 兼容协议（同时覆盖 DeepSeek、智谱、Ollama、自定义兼容端）
//!   的流式实现。
//! - `claude`: Anthropic Claude 原生 Messages API 的流式实现。
//! - `prompts`: 各业务场景（翻译为命令、诊断报错、解释输出、通用对话）的
//!   中文系统提示词常量。
//!
//! # 事件流
//!
//! 所有 provider 在流式响应过程中会通过 [`crate::events`] 向前端推送三类事件：
//! - `ai:chunk` —— 增量文本片段
//! - `ai:done` —— 响应完成（携带累计的完整文本）
//! - `ai:error` —— 出错时携带可读错误信息

pub mod claude;
pub mod openai;
pub mod prompts;
pub mod provider;
pub mod tools;
