//! 统一错误类型。
//!
//! 整个 X-Term 后端使用 [`AppError`] 作为统一的错误类型，并通过 [`AppResult`]`<T>`
//! 作为函数返回值的别名。任何模块产生的错误都可以通过 `?` 运算符自动转换到
//! `AppError` 的相应变体（见下方实现的若干 `From` 转换）。

use serde::Serialize;
use thiserror::Error;

/// 应用统一错误类型。
///
/// 每个变体对应一类错误来源。所有变体都携带可读的字符串信息，便于在前端展示。
#[derive(Debug, Error)]
pub enum AppError {
    #[error("SSH 错误: {0}")]
    Ssh(String),

    #[error("存储错误: {0}")]
    Storage(String),

    #[error("配置错误: {0}")]
    Config(String),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("加密错误: {0}")]
    Crypto(String),

    #[error("AI 错误: {0}")]
    Ai(String),

    #[error("未找到: {0}")]
    NotFound(String),

    #[error("无效输入: {0}")]
    InvalidInput(String),

    #[error("认证错误: {0}")]
    Auth(String),

    #[error("Tauri 错误: {0}")]
    Tauri(#[from] tauri::Error),
}

// ---------------------------------------------------------------------------
// From 转换：使外部库的错误可以通过 `?` 自动转为 AppError
// ---------------------------------------------------------------------------

impl From<rusqlite::Error> for AppError {
    fn from(err: rusqlite::Error) -> Self {
        AppError::Storage(err.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(err: serde_json::Error) -> Self {
        // JSON 序列化/反序列化失败通常发生在配置读写，归入 Config。
        AppError::Config(err.to_string())
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        AppError::Ai(err.to_string())
    }
}

impl From<tokio::task::JoinError> for AppError {
    fn from(err: tokio::task::JoinError) -> Self {
        // Tokio 任务 join 失败（如 panic）归入通用的 IO/运行时错误。
        AppError::Ssh(format!("异步任务失败: {}", err))
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        AppError::Storage(format!("MySQL 错误: {}", e))
    }
}

// ---------------------------------------------------------------------------
// Serialize：前端只能拿到字符串形式的错误信息
// ---------------------------------------------------------------------------

impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// 全局 `Result` 别名。
pub type AppResult<T> = Result<T, AppError>;
