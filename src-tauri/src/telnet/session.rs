//! Telnet 终端会话实现。
//!
//! 用 `tokio::net::TcpStream` 连接目标，后台 reader 任务处理：
//! - 读取远端字节，过滤/响应 IAC 协商，纯数据写入输出缓冲 + emit `terminal:data`
//! - 从 input mpsc 收取用户输入（write/resize），写回 TcpStream
//! - 连接断开时 emit `terminal:closed`

use std::sync::{Arc, Mutex as StdMutex};

use base64::Engine;
use tauri::AppHandle;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{AppError, AppResult};
use crate::events::{emit, TerminalClosedEvent, TerminalDataEvent, TERMINAL_CLOSED, TERMINAL_DATA};
use crate::ssh::session::{OutputRing, SharedOutputRing};

// Telnet IAC 命令字节。
const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250; // 子协商开始
const SE: u8 = 240; // 子协商结束

// 常用选项码。
const OPT_ECHO: u8 = 1;
const OPT_SUPPRESS_GA: u8 = 3;
const OPT_TERM_TYPE: u8 = 24;
const OPT_NAWS: u8 = 31;

const OUTPUT_BUFFER_CAP: usize = 64 * 1024;

/// 输入消息（与 SshSession 的 InputMsg 对齐）。
enum TelnetInput {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

pub struct TelnetSession {
    pub id: String,
    pub session_config_id: String,
    app: AppHandle,
    /// TcpStream 写端（reader 任务持有读端，通过 split）。
    /// 实际上 reader 任务里持有 write_half，input 通过 channel 传给它。
    reader_handle: Option<JoinHandle<()>>,
    input_tx: Option<mpsc::UnboundedSender<TelnetInput>>,
    pub output_buffer: SharedOutputRing,
}

impl TelnetSession {
    pub fn snapshot(&self, max_bytes: usize) -> String {
        match self.output_buffer.lock() {
            Ok(buf) => buf.snapshot(max_bytes),
            Err(_) => String::new(),
        }
    }

    /// 输出缓冲当前字节数（命令执行前基准）。
    pub fn output_offset(&self) -> usize {
        match self.output_buffer.lock() {
            Ok(buf) => buf.len(),
            Err(_) => 0,
        }
    }

    pub fn write(&self, data: Vec<u8>) -> AppResult<()> {
        let tx = self
            .input_tx
            .as_ref()
            .ok_or_else(|| AppError::Ssh("Telnet 会话尚未启动 reader 或已关闭".into()))?;
        tx.send(TelnetInput::Write(data))
            .map_err(|_| AppError::Ssh("Telnet reader 已退出".into()))
    }

    pub fn resize(&self, cols: u32, rows: u32) -> AppResult<()> {
        let tx = self
            .input_tx
            .as_ref()
            .ok_or_else(|| AppError::Ssh("Telnet 会话尚未启动 reader 或已关闭".into()))?;
        tx.send(TelnetInput::Resize {
            cols: cols as u16,
            rows: rows as u16,
        })
        .map_err(|_| AppError::Ssh("Telnet reader 已退出".into()))
    }

    pub fn spawn_reader(&mut self, stream: TcpStream) -> AppResult<()> {
        let app = self.app.clone();
        let session_id = self.id.clone();
        let output_buffer = self.output_buffer.clone();

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<TelnetInput>();
        self.input_tx = Some(input_tx);

        // split TcpStream 为读/写两半。
        let (mut read_half, mut write_half) = stream.into_split();

        let join = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut cols: u16 = 80;
            let mut rows: u16 = 24;

            // 发送初始 NAWS。
            let _ = send_naws(&mut write_half, cols, rows).await;

            loop {
                tokio::select! {
                    // 远端 → 前端。
                    // 注意：不能用 biased —— 远端持续输出时会饿死输入分支，
                    // 用户按键（含 Ctrl+C 等中断）得不到处理。
                    n = read_half.read(&mut buf) => {
                        match n {
                            Ok(0) | Err(_) => break, // 连接关闭
                            Ok(len) => {
                                let data = &buf[..len];
                                let clean = process_iac(data, &mut write_half, &mut cols, &mut rows).await;
                                if !clean.is_empty() {
                                    // 写入输出缓冲。
                                    if let Ok(mut ob) = output_buffer.lock() {
                                        ob.push(&clean);
                                    }
                                    // emit 给前端（base64）。
                                    let b64 = base64::engine::general_purpose::STANDARD.encode(&clean);
                                    emit(
                                        &app,
                                        TERMINAL_DATA,
                                        TerminalDataEvent {
                                            session_id: session_id.clone(),
                                            data: b64,
                                        },
                                    );
                                }
                            }
                        }
                    }
                    // 前端 → 远端。
                    inp = input_rx.recv() => {
                        match inp {
                            Some(TelnetInput::Write(data)) => {
                                if write_half.write_all(&data).await.is_err() {
                                    break;
                                }
                                let _ = write_half.flush().await;
                            }
                            Some(TelnetInput::Resize { cols: c, rows: r }) => {
                                cols = c;
                                rows = r;
                                let _ = send_naws(&mut write_half, cols, rows).await;
                            }
                            None => break,
                        }
                    }
                }
            }
            // 连接断开 → emit closed。
            emit(
                &app,
                TERMINAL_CLOSED,
                TerminalClosedEvent {
                    session_id: session_id.clone(),
                },
            );
            log::info!("[telnet:{}] 会话结束", session_id);
        });

        self.reader_handle = Some(join);
        Ok(())
    }
}

/// 处理 IAC 协商：解析命令序列，响应必要的选项，返回纯数据字节。
///
/// 遍历字节流，遇到 IAC(255) 起始的命令序列就解析并消费，非命令字节累积为数据返回。
async fn process_iac(
    data: &[u8],
    write_half: &mut tokio::net::tcp::OwnedWriteHalf,
    _cols: &mut u16,
    _rows: &mut u16,
) -> Vec<u8> {
    let mut clean = Vec::with_capacity(data.len());
    let mut i = 0;
    let mut to_send: Vec<u8> = Vec::new(); // 待发送的响应

    while i < data.len() {
        if data[i] == IAC {
            if i + 1 >= data.len() {
                break; // 不完整的 IAC，丢弃
            }
            let cmd = data[i + 1];
            match cmd {
                DO | DONT | WILL | WONT => {
                    if i + 2 >= data.len() {
                        break;
                    }
                    let opt = data[i + 2];
                    // 响应策略。
                    match (cmd, opt) {
                        (WILL, OPT_ECHO) | (WILL, OPT_SUPPRESS_GA) => {
                            to_send.extend_from_slice(&[IAC, DO, opt]); // 接受
                        }
                        (DO, OPT_TERM_TYPE) => {
                            to_send.extend_from_slice(&[IAC, WILL, opt]); // 同意提供终端类型
                        }
                        (DO, OPT_NAWS) => {
                            to_send.extend_from_slice(&[IAC, WILL, opt]); // 同意 NAWS
                        }
                        (WILL, _) => {
                            to_send.extend_from_slice(&[IAC, WONT, opt]); // 拒绝其它 WILL
                        }
                        (DO, _) => {
                            to_send.extend_from_slice(&[IAC, WONT, opt]); // 拒绝其它 DO
                        }
                        _ => {} // DONT/WONT 不响应
                    }
                    i += 3;
                }
                SB => {
                    // 子协商：SB ... SE，找到 SE 消费整个块。
                    // 终端类型请求：SB TERMINAL_TYPE SEND IAC SE → 回 SB TERMINAL_TYPE IS <name> IAC SE
                    let mut j = i + 2;
                    while j < data.len() {
                        if data[j] == IAC && j + 1 < data.len() && data[j + 1] == SE {
                            break;
                        }
                        j += 1;
                    }
                    // 解析子协商内容。
                    if i + 2 < data.len() && data[i + 2] == OPT_TERM_TYPE {
                        // 回终端类型。
                        let name = b"xterm-256color";
                        to_send.extend_from_slice(&[IAC, SB, OPT_TERM_TYPE, 0]); // 0 = IS
                        to_send.extend_from_slice(name);
                        to_send.extend_from_slice(&[IAC, SE]);
                    }
                    i = j + 2;
                }
                IAC => {
                    // IAC IAC → 转义的数据字节 255。
                    clean.push(IAC);
                    i += 2;
                }
                _ => {
                    // 其它单字节命令（如 NOP），跳过。
                    i += 2;
                }
            }
        } else {
            clean.push(data[i]);
            i += 1;
        }
    }

    // 发送响应。
    if !to_send.is_empty() {
        let _ = write_half.write_all(&to_send).await;
        let _ = write_half.flush().await;
    }

    clean
}

/// 发送 NAWS（窗口大小）子协商。
async fn send_naws(
    write_half: &mut tokio::net::tcp::OwnedWriteHalf,
    cols: u16,
    rows: u16,
) -> std::io::Result<()> {
    let mut msg = vec![IAC, SB, OPT_NAWS];
    msg.push((cols >> 8) as u8);
    msg.push((cols & 0xff) as u8);
    msg.push((rows >> 8) as u8);
    msg.push((rows & 0xff) as u8);
    msg.extend_from_slice(&[IAC, SE]);
    write_half.write_all(&msg).await?;
    write_half.flush().await?;
    Ok(())
}

/// 带 stream 的 open（connect_session 调用）：连接 + 创建 session + spawn_reader。
impl TelnetSession {
    pub async fn connect_and_spawn(
        host: &str,
        port: u16,
        session_config_id: String,
        app: AppHandle,
    ) -> AppResult<Self> {
        log::info!("[telnet] 连接 {}:{}...", host, port);
        let stream = TcpStream::connect((host, port))
            .await
            .map_err(|e| AppError::Ssh(format!("Telnet 连接失败 {}: {}", host, e)))?;
        let _ = stream.set_nodelay(true);

        let mut session = TelnetSession {
            id: uuid::Uuid::new_v4().to_string(),
            session_config_id,
            app,
            reader_handle: None,
            input_tx: None,
            output_buffer: Arc::new(StdMutex::new(OutputRing::new(OUTPUT_BUFFER_CAP))),
        };
        session.spawn_reader(stream)?;
        Ok(session)
    }
}

impl Drop for TelnetSession {
    fn drop(&mut self) {
        // abort 后台 reader 任务，关闭连接。
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
    }
}
