//! 本地终端模块（本机 shell：cmd / PowerShell / Git Bash）。
//!
//! 基于 portable-pty（Windows 走 ConPTY），复用 SSH 终端的输出环形缓冲与
//! `terminal:data` / `terminal:exit` / `terminal:closed` 事件通道，前端
//! TerminalPane 零改动即可渲染本地 shell 标签页。

pub mod session;

pub use session::LocalSession;

/// 本机可用的 shell 检测结果（供设置页下拉展示，`available` 为 false 的项前端隐藏）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalShellInfo {
    /// shell 标识："cmd" | "powershell" | "git-bash"。
    pub id: String,
    /// 展示名。
    pub label: String,
    /// 本机是否可用。
    pub available: bool,
}
