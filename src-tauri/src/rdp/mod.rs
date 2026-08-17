//! 内嵌 RDP 客户端的「迷你网关」桥接模块。
//!
//! 前端 IronRDP 官方 WASM 客户端（@devolutions/iron-remote-desktop-rdp）通过
//! WebSocket 连接本模块在 127.0.0.1 上开出的临时端口。桥接实现 Devolutions
//! Gateway 协议（RDCleanPath）的最小可用子集：解析握手 PDU → TCP 建连 →
//! X.224 握手 → 桥接端终止 TLS → 回传证书链，之后纯字节透传（见
//! [`bridge`] 与 [`rdcp`]）。
//!
//! 安全性：监听仅绑定回环地址（与 MCP 服务一致，本机可用），每个桥接
//! 带随机 uuid token 作为 URL 路径，未知 token 直接 404。
//!
//! 注意：桥接不校验 RDP 服务器证书（与官方 Gateway 的 dangerous_connect
//! 一致），敏感环境建议在设置中改用系统 mstsc。

pub mod bridge;
pub mod rdcp;

pub use bridge::RdpBridge;
