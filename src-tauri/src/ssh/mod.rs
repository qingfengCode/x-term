//! SSH 核心模块。
//!
//! 本模块组织 X-Term 与远程主机交互的全部能力：
//! - [`client`]: 基于 russh 的底层 SSH 客户端封装（连接、认证、Handler）。
//! - [`session`]: 单个交互式终端会话（PTY + shell）的封装，负责把远程输出
//!   以事件形式推送到前端。
//! - [`sftp`]: SFTP 子系统封装，提供目录列举、文件元信息、上传/下载等能力。
//! - [`tunnel`]: SSH 端口转发（本地/远程/动态）封装。
//!
//! 所有子模块统一使用 [`crate::error::AppError`] / [`crate::error::AppResult`]
//! 进行错误处理，并通过 [`crate::events::emit`] 向前端推送实时事件。

pub mod client;
pub mod session;
pub mod sftp;
pub mod tunnel;
