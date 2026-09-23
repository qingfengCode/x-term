//! MCP（Model Context Protocol）服务端。
//!
//! 把 X-Term 管理的 SSH 服务器和 MySQL 数据库通过标准 MCP（JSON-RPC 2.0 over
//! HTTP + SSE）暴露给外部 MCP 客户端（Claude Desktop、Cursor 等）。
//!
//! 每个 kind（SSH / DB）支持以下资源模式（`resource_mode`）：
//! - `bound`（默认）：绑定一个本地资源（SSH 会话 / DB profile），工具参数只传
//!   command/sql，目标与凭据从本地解析。
//! - `client`（客户端直连）：无需绑定实例，目标与凭据（host/port/username/password）
//!   由调用方在工具参数中传入，凭据即用即弃、不存储不落日志。
//! - `multi`（多机模式，仅 SSH）：绑定一组 SSH 会话（用户勾选授权），工具必传
//!   `target`（授权机器的展示名，schema 附 enum 约束，重名自动加 `#序号` 后缀），
//!   由外部 AI 自行决定每次调用落在哪台机器；另暴露只读 `list_machines`（机器
//!   地址/标签清单）辅助路由。越权 target 直接拒绝并回显可用清单。
//! - `bastion`（堡垒机模式，仅 SSH）：绑定一个堡垒机 SSH 会话配置（如
//!   JumpServer），以「会话」为单位按需进出资产主机——`bastion_list_hosts`
//!   （资产菜单）/ `bastion_create_session`（进入目标主机，返回 sessionId）/
//!   `bastion_session_exec`（会话内执行）/ `bastion_upload_file`（经会话 shell
//!   以 base64 分块上传本地文件到目标主机）/ `bastion_close_session` /
//!   `bastion_list_sessions`。连接模型为「基础连接 + 多 channel」：MFA 只在
//!   建立基础连接时输一次，之后各目标主机复用该已认证连接开 channel；支持
//!   配置「登录后命令」（如 `sudo su -`），会话空闲自动回收，停止服务时全断。
//!
//! # 子模块
//! - [`approval`]：人工确认护栏。`exec_ssh` / `exec_sql` 必须先经 X-Term 用户在前端
//!   确认（emit `mcp:approval_request` → 前端回 `mcp_respond_approval`）才会执行。
//! - [`bastion`]：堡垒机模式的会话管理（建连/哨兵执行/空闲回收）。
//! - [`exec`]：不依赖活跃终端实例的执行实现（按"连接名"解析配置后建立短连接执行；
//!   直连模式从工具参数解析目标后执行）。
//! - [`server`]：基于 axum 0.7 的 HTTP + SSE 传输，实现 `initialize` / `tools/list`
//!   / `tools/call` 三个 JSON-RPC 方法。
//! - [`tools`]：工具定义（tools/list 的 inputSchema）与元信息（名称校验 / 只读性 /
//!   白名单语义 / 日志与确认卡片文案），纯函数层。
//! - [`upload`]：堡垒机会话文件上传的纯逻辑（base64 分块 / 远端命令拼装 / 输出解析）。
//!
//! # 安全
//! - 监听地址默认 127.0.0.1（仅本机）；允许配置为 0.0.0.0 / 局域网 IP 对外
//!   开放（如局域网内其他机器上的 MCP 客户端直连），暴露风险由用户自行承担。
//! - 所有请求需携带 Bearer token（`Authorization: Bearer <token>` 或 `?token=`）。
//! - 写/执行类工具默认经过人工确认；用户可将运行模式切换为「白名单运行」
//!   （SSH=命令白名单 / DB=只读 SQL 自动放行）或「自动运行」（全部自动执行）；
//!   **文件传输工具（upload_file / download_file）例外**：本地路径不做沙箱
//!   （外部客户端把本机文件传到 S3/SFTP 是功能刚需），故无论何种运行模式
//!   都强制人工确认——确认卡片展示完整的本地/远端路径与目标，用户可见可控。
//! - client 模式下密码不落日志、不写入确认事件（确认浮层参数副本已剔除 password）。

pub mod approval;
pub mod bastion;
pub mod exec;
pub mod server;
pub mod tools;
pub mod upload;

pub use approval::McpKind;
pub use server::{mcp_server_status, start_mcp_server, stop_mcp_server, McpServerStatus};
