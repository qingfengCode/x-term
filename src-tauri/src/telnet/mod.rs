//! Telnet 协议终端会话。
//!
//! 与 [`crate::ssh::session::SshSession`] 对外接口一致（write/resize/snapshot/close/spawn_reader），
//! 但底层用 `tokio::net::TcpStream` + 手写 IAC 协商，而非 russh。
//!
//! IAC 协商策略（最小集，覆盖 95% 路由器/交换机/Unix telnetd）：
//! - 收到 WILL ECHO / WILL SUPPRESS_GO_AHEAD → 回 DO（接受服务端回显控制）
//! - 收到 DO TERMINAL_TYPE → 回 WILL + SB 终端类型 `xterm-256color`
//! - 收到 DO NAWS → 回 WILL，并主动发 NAWS 子协商（窗口大小）
//! - 其它 WILL → WONT；其它 DO → WONT（拒绝不支持的选项）
//! - 过滤掉 IAC 序列，纯数据字节写入输出缓冲 + emit

pub mod session;

pub use session::TelnetSession;
