//! MCP（Model Context Protocol）服务端。
//!
//! 把 X-Term 管理的 SSH 服务器和 MySQL 数据库通过标准 MCP（JSON-RPC 2.0 over
//! HTTP + SSE）暴露给外部 MCP 客户端（Claude Desktop、Cursor 等）。
//!
//! 每个 kind（SSH / DB）支持两种资源模式（`resource_mode`）：
//! - `bound`（默认）：绑定一个本地资源（SSH 会话 / DB profile），工具参数只传
//!   command/sql，目标与凭据从本地解析。
//! - `client`（客户端直连）：无需绑定实例，目标与凭据（host/port/username/password）
//!   由调用方在工具参数中传入，凭据即用即弃、不存储不落日志。
//!
//! # 子模块
//! - [`approval`]：人工确认护栏。`exec_ssh` / `exec_sql` 必须先经 X-Term 用户在前端
//!   确认（emit `mcp:approval_request` → 前端回 `mcp_respond_approval`）才会执行。
//! - [`exec`]：不依赖活跃终端实例的执行实现（按"连接名"解析配置后建立短连接执行；
//!   直连模式从工具参数解析目标后执行）。
//! - [`server`]：基于 axum 0.7 的 HTTP + SSE 传输，实现 `initialize` / `tools/list`
//!   / `tools/call` 三个 JSON-RPC 方法。
//!
//! # 安全
//! - 监听地址默认 127.0.0.1（仅本机）；允许配置为 0.0.0.0 / 局域网 IP 对外
//!   开放（如局域网内其他机器上的 MCP 客户端直连），暴露风险由用户自行承担。
//! - 所有请求需携带 Bearer token（`Authorization: Bearer <token>` 或 `?token=`）。
//! - 写/执行类工具必须经过人工确认（除非用户开启"自动放行"）；
//!   **文件传输工具（upload_file / download_file）例外**：本地路径不做沙箱
//!   （外部客户端把本机文件传到 S3/SFTP 是功能刚需），故无论是否开启自动放行
//!   都强制人工确认——确认卡片展示完整的本地/远端路径与目标，用户可见可控。
//! - client 模式下密码不落日志、不写入确认事件（确认浮层参数副本已剔除 password）。

pub mod approval;
pub mod exec;
pub mod server;

pub use approval::McpKind;
pub use server::{mcp_server_status, start_mcp_server, stop_mcp_server, McpServerStatus};
