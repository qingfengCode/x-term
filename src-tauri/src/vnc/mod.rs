//! 内嵌 VNC 查看器的 WebSocket ↔ TCP 桥接模块。
//!
//! 前端 noVNC 通过 WebSocket 连接本模块在 127.0.0.1 上开出的临时端口，
//! 桥接把 WS 字节流转发到目标 VNC 服务端（RFB over TCP），本身对协议透明
//! （认证/口令由前端 noVNC 处理）。
//!
//! 安全性：监听仅绑定回环地址（与 MCP 服务一致，本机可用），每个桥接
//! 带随机 uuid token 作为 URL 路径，未知 token 直接 404。

pub mod bridge;

pub use bridge::VncBridge;
