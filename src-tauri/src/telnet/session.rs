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
use tokio::sync::{mpsc, oneshot};
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

/// leftover 缓冲上限。增量解析会缓存未闭合的序列（新增行为），防御远端
/// 持续发送未闭合的 SB 序列导致内存无界增长。正常协商序列（WILL/DO/终端
/// 类型等）只有几个字节，远小于此值；超限丢弃并重新同步。
const MAX_IAC_LEFTOVER: usize = 64 * 1024;

// 常用选项码。
const OPT_ECHO: u8 = 1;
const OPT_SUPPRESS_GA: u8 = 3;
const OPT_TERM_TYPE: u8 = 24;
const OPT_NAWS: u8 = 31;

const OUTPUT_BUFFER_CAP: usize = 64 * 1024;

/// 终端输出批量 emit 间隔（毫秒，与 SSH 会话一致）。
const TERMINAL_BATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
/// 终端输出批量 emit 大小阈值（字节）。
const TERMINAL_BATCH_MAX_BYTES: usize = 64 * 1024;

/// 输入消息（与 SshSession 的 InputMsg 对齐）。
enum TelnetInput {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    /// 立即冲刷输出批次（terminal_attach 前调用，见 SshSession::flush_output）。
    FlushOutput(oneshot::Sender<()>),
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
    /// 输出日志（可选，设置开启时由命令层装配）。
    pub output_log: crate::output_log::SharedOutputLog,
}

impl TelnetSession {
    pub fn snapshot(&self, max_bytes: usize) -> String {
        // 0 表示默认 8 KiB（与 SSH/Local 的 snapshot 契约一致；直接透传 0
        // 会让 OutputRing::snapshot 取 0 字节，恒返回空串）。
        let n = if max_bytes == 0 { 8 * 1024 } else { max_bytes };
        match self.output_buffer.lock() {
            Ok(buf) => buf.snapshot(n),
            Err(_) => String::new(),
        }
    }

    /// 取终端输出的完整快照（整个环形缓冲）。与 [`SshSession::full_snapshot`]
    /// 对齐，供 AI 可视化命令的哨兵检测/截取使用。
    pub fn full_snapshot(&self) -> String {
        match self.output_buffer.lock() {
            Ok(buf) => buf.snapshot(usize::MAX),
            Err(_) => String::new(),
        }
    }

    /// 取原始字节快照 + 累计字节数（同一锁内，原子一致）。
    /// 供前端 attach 回放（terminal_attach 命令），见 [`SshSession::attach_snapshot`]。
    pub fn attach_snapshot(&self) -> (Vec<u8>, usize) {
        match self.output_buffer.lock() {
            Ok(buf) => buf.snapshot_raw_with_total(),
            Err(_) => (Vec::new(), 0),
        }
    }

    /// 累计写入字节数（不受环形截断影响），判断"输出是否仍在增长"用。
    pub fn total_output_bytes(&self) -> usize {
        match self.output_buffer.lock() {
            Ok(buf) => buf.total_bytes(),
            Err(_) => 0,
        }
    }

    /// 输出缓冲当前字节数（命令执行前基准）。
    pub fn output_offset(&self) -> usize {
        match self.output_buffer.lock() {
            Ok(buf) => buf.len(),
            Err(_) => 0,
        }
    }

    /// 取 `since_total` 累计字节之后的输出窗口（配合
    /// [`Self::total_output_bytes`] 截取命令执行期间的新增输出）。
    /// 返回 (文本, 是否溢出窗口容量)。
    pub fn snapshot_after(&self, since_total: usize) -> (String, bool) {
        match self.output_buffer.lock() {
            Ok(buf) => buf.snapshot_after(since_total),
            Err(_) => (String::new(), false),
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

    /// 通知 reader 立即冲刷输出批次（attach 快照前调用，见 SshSession::flush_output）。
    pub fn flush_output(&self) -> Option<oneshot::Receiver<()>> {
        let tx = self.input_tx.as_ref()?;
        let (ack_tx, ack_rx) = oneshot::channel();
        let _ = tx.send(TelnetInput::FlushOutput(ack_tx));
        Some(ack_rx)
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
        // 输出日志（可选）：reader 输出喂入；退出时 close。
        let output_log = self.output_log.clone();

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<TelnetInput>();
        self.input_tx = Some(input_tx);

        // split TcpStream 为读/写两半。
        let (mut read_half, mut write_half) = stream.into_split();

        let join = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut cols: u16 = 80;
            let mut rows: u16 = 24;
            // IAC 解析状态跨 read 分片保持（协商序列可能被 TCP 切断）。
            let mut parser = TelnetIacParser::new();

            // 发送初始 NAWS。
            let _ = send_naws(&mut write_half, cols, rows).await;

            // 输出批次缓冲（批量 emit 策略与 SSH 一致：16ms/64KiB 冲刷）。
            let mut batch: Vec<u8> = Vec::with_capacity(4096);
            // 批次首字节对应的累计字节数（半开区间起点，供 attach 去重切片）。
            let mut batch_start_total: usize = 0;
            let mut batch_total: usize = 0;
            let mut batch_deadline: Option<tokio::time::Instant> = None;

            let flush_batch =
                |batch: &mut Vec<u8>, start_total: usize, end_total: usize| {
                    if batch.is_empty() {
                        return;
                    }
                    let b64 = base64::engine::general_purpose::STANDARD.encode(batch.as_slice());
                    emit(
                        &app,
                        TERMINAL_DATA,
                        TerminalDataEvent {
                            session_id: session_id.clone(),
                            data: b64,
                            start_total,
                            total: end_total,
                        },
                    );
                    batch.clear();
                };

            loop {
                let flush_fut = async {
                    match batch_deadline {
                        Some(dl) => tokio::time::sleep_until(dl).await,
                        None => std::future::pending::<()>().await,
                    }
                };

                tokio::select! {
                    // 远端 → 前端（先累积进批次）。
                    // 注意：不能用 biased —— 远端持续输出时会饿死输入分支，
                    // 用户按键（含 Ctrl+C 等中断）得不到处理。
                    _ = flush_fut, if !batch.is_empty() => {
                        flush_batch(&mut batch, batch_start_total, batch_total);
                        batch_deadline = None;
                    }
                    n = read_half.read(&mut buf) => {
                        match n {
                            Ok(0) | Err(_) => {
                                // 连接关闭：先冲刷残留批次，避免尾部输出丢失。
                                flush_batch(&mut batch, batch_start_total, batch_total);
                                break;
                            }
                            Ok(len) => {
                                let data = &buf[..len];
                                let clean = process_iac(&mut parser, data, &mut write_half).await;
                                if !clean.is_empty() {
                                    if batch.is_empty() {
                                        batch_start_total = batch_total;
                                    }
                                    // 写入输出缓冲（锁内取追加后的累计字节数，
                                    // 随批次事件 emit 供前端 attach 回放去重）。
                                    // 同步喂输出日志。
                                    if let Ok(mut logger) = output_log.lock() {
                                        if let Some(l) = logger.as_mut() {
                                            l.feed(&clean);
                                        }
                                    }
                                    if let Some(total) = output_buffer.lock().ok().map(|mut ob| {
                                        ob.push(&clean);
                                        ob.total_bytes()
                                    }) {
                                        batch_total = total;
                                    }
                                    batch.extend_from_slice(&clean);
                                    if batch_deadline.is_none() {
                                        batch_deadline = Some(
                                            tokio::time::Instant::now() + TERMINAL_BATCH_INTERVAL,
                                        );
                                    }
                                    if batch.len() >= TERMINAL_BATCH_MAX_BYTES {
                                        flush_batch(&mut batch, batch_start_total, batch_total);
                                        batch_deadline = None;
                                    }
                                }
                            }
                        }
                    }
                    // 前端 → 远端。
                    inp = input_rx.recv() => {
                        match inp {
                            Some(TelnetInput::Write(data)) => {
                                if write_all_timeout(&mut write_half, &data).await.is_err() {
                                    // 写失败/超时 = 连接已断或对端长期不读：
                                    // 冲刷残留批次再退出（无超时的话 write_all
                                    // 挂起会冻结整个 reader 循环：读、resize、
                                    // flush 全部停摆，终端假死）。
                                    flush_batch(&mut batch, batch_start_total, batch_total);
                                    break;
                                }
                            }
                            Some(TelnetInput::Resize { cols: c, rows: r }) => {
                                cols = c;
                                rows = r;
                                let _ = send_naws(&mut write_half, cols, rows).await;
                            }
                            Some(TelnetInput::FlushOutput(ack)) => {
                                flush_batch(&mut batch, batch_start_total, batch_total);
                                batch_deadline = None;
                                let _ = ack.send(());
                            }
                            None => {
                                flush_batch(&mut batch, batch_start_total, batch_total);
                                break;
                            }
                        }
                    }
                }
            }
            // 关闭输出日志（冲刷尾行 + 落盘），再 emit closed。
            if let Ok(mut logger) = output_log.lock() {
                if let Some(l) = logger.as_mut() {
                    l.close();
                }
                *logger = None;
            }
            // 连接断开 → emit closed。
            emit(
                &app,
                TERMINAL_CLOSED,
                TerminalClosedEvent {
                    session_id: session_id.clone(),
                    reason: None,
                },
            );
            log::info!("[telnet:{}] 会话结束", session_id);
        });

        self.reader_handle = Some(join);
        Ok(())
    }
}

/// Telnet IAC 解析器（跨 TCP 分片的增量状态机）。
///
/// TCP 是字节流，IAC 协商序列（`IAC WILL <opt>`、`IAC SB ... IAC SE` 等）随时
/// 可能被分片切断。旧实现按「每次 read 到的 chunk」独立解析，chunk 末尾不完整
/// 的序列直接丢弃，于是协商序列跨分片时：命令字节漏进数据流（乱码）、数据丢失、
/// 该序列对应的选项协商失效（远端以为客户端不响应）。本解析器把未完成的序列
/// 尾部缓存起来，与下一 chunk 拼接后继续解析，并正确处理子协商内的
/// `IAC IAC` 转义。
struct TelnetIacParser {
    /// 上一批数据解析后剩余的不完整序列字节（等待后续数据补齐）。
    leftover: Vec<u8>,
}

/// 一批数据解析后的产物。
struct ParsedTelnet {
    /// 过滤掉协商序列后的纯数据（展示用）。
    data: Vec<u8>,
    /// 需写回远端的协商响应。
    responses: Vec<u8>,
}

impl TelnetIacParser {
    fn new() -> Self {
        Self { leftover: Vec::new() }
    }

    /// 增量解析一批字节：先与 leftover 拼接，解析中遇到不完整序列时把未完成
    /// 的尾部存回 leftover，等待下一批补齐。
    fn push(&mut self, chunk: &[u8]) -> ParsedTelnet {
        let mut buf = std::mem::take(&mut self.leftover);
        buf.extend_from_slice(chunk);

        let mut data = Vec::new();
        let mut responses = Vec::new();
        let mut i = 0usize;
        let mut incomplete_from: Option<usize> = None;

        while i < buf.len() {
            if buf[i] != IAC {
                // 数据模式：一直推进到下一个 IAC（或结尾）。
                let start = i;
                while i < buf.len() && buf[i] != IAC {
                    i += 1;
                }
                data.extend_from_slice(&buf[start..i]);
                continue;
            }
            match buf.get(i + 1).copied() {
                None => {
                    // 孤立的 IAC（下一字节还没到）：整体缓存等待补齐。
                    incomplete_from = Some(i);
                    break;
                }
                Some(IAC) => {
                    // IAC IAC → 转义的数据字节 255。
                    data.push(IAC);
                    i += 2;
                }
                Some(cmd @ (DO | DONT | WILL | WONT)) => {
                    match buf.get(i + 2).copied() {
                        None => {
                            incomplete_from = Some(i);
                            break;
                        }
                        Some(opt) => {
                            // 响应策略。
                            match (cmd, opt) {
                                (WILL, OPT_ECHO) | (WILL, OPT_SUPPRESS_GA) => {
                                    responses.extend_from_slice(&[IAC, DO, opt]); // 接受
                                }
                                (DO, OPT_TERM_TYPE) => {
                                    responses.extend_from_slice(&[IAC, WILL, opt]); // 同意提供终端类型
                                }
                                (DO, OPT_NAWS) => {
                                    responses.extend_from_slice(&[IAC, WILL, opt]); // 同意 NAWS
                                }
                                (WILL, _) => {
                                    responses.extend_from_slice(&[IAC, WONT, opt]); // 拒绝其它 WILL
                                }
                                (DO, _) => {
                                    responses.extend_from_slice(&[IAC, WONT, opt]); // 拒绝其它 DO
                                }
                                _ => {} // DONT/WONT 不响应
                            }
                            i += 3;
                        }
                    }
                }
                Some(SB) => {
                    // 子协商：内容直到 IAC SE 结束；内容中的 IAC IAC 转义为 255。
                    let mut j = i + 2;
                    let mut sub = Vec::new();
                    let mut found_end = false;
                    while j < buf.len() {
                        match (buf[j], buf.get(j + 1).copied()) {
                            (IAC, Some(SE)) => {
                                found_end = true;
                                j += 2;
                                break;
                            }
                            (IAC, Some(IAC)) => {
                                sub.push(IAC);
                                j += 2;
                            }
                            (b, _) => {
                                sub.push(b);
                                j += 1;
                            }
                        }
                    }
                    if !found_end {
                        // SE 尚未到达：整个 SB 序列缓存等待补齐。
                        incomplete_from = Some(i);
                        break;
                    }
                    // 终端类型请求（SB TERM_TYPE ... SE）→ 回 IS + 名称。
                    if sub.first() == Some(&OPT_TERM_TYPE) {
                        let mut r = vec![IAC, SB, OPT_TERM_TYPE, 0]; // 0 = IS
                        r.extend_from_slice(b"xterm-256color");
                        r.extend_from_slice(&[IAC, SE]);
                        responses.extend_from_slice(&r);
                    }
                    i = j;
                }
                Some(_) => {
                    // 其它单字节命令（如 NOP），跳过。
                    i += 2;
                }
            }
        }

        if let Some(from) = incomplete_from {
            self.leftover = buf[from..].to_vec();
        } else {
            self.leftover.clear();
        }
        // 防御：远端持续发送未闭合序列时缓存会无限增长，超限丢弃并重新同步。
        if self.leftover.len() > MAX_IAC_LEFTOVER {
            log::warn!(
                "Telnet IAC 未闭合序列超过 {} 字节，丢弃并重新同步",
                MAX_IAC_LEFTOVER
            );
            self.leftover.clear();
        }
        ParsedTelnet { data, responses }
    }
}

/// 处理一批远端字节的 IAC 协商，返回纯数据字节。
///
/// `parser` 保存跨 TCP 分片的解析状态（见 [`TelnetIacParser`]）；协商响应
/// 直接写回 `write_half`。
async fn process_iac(
    parser: &mut TelnetIacParser,
    data: &[u8],
    write_half: &mut tokio::net::tcp::OwnedWriteHalf,
) -> Vec<u8> {
    let parsed = parser.push(data);
    if !parsed.responses.is_empty() {
        // 带超时：对端接收窗口为 0 时协商写挂起会冻结整个 reader 循环。
        let _ = write_all_timeout(write_half, &parsed.responses).await;
    }
    parsed.data
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
    write_all_timeout(write_half, &msg).await
}

/// 写超时：TCP 发送缓冲满（对端停止读取）时 `write_all` 会无限挂起，且
/// 它运行在 reader 的 select 分支内——挂起即冻结整个循环（读、Resize、
/// FlushOutput 全部停摆，终端假死）。与 SSH 侧的分块写超时语义对齐。
const TELNET_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 带超时的 write_all + flush；任一超时或失败均返回 Err。
async fn write_all_timeout(
    write_half: &mut tokio::net::tcp::OwnedWriteHalf,
    data: &[u8],
) -> std::io::Result<()> {
    let timed = |e| std::io::Error::new(std::io::ErrorKind::TimedOut, e);
    tokio::time::timeout(TELNET_WRITE_TIMEOUT, write_half.write_all(data))
        .await
        .map_err(|_| timed("telnet write timeout"))??;
    tokio::time::timeout(TELNET_WRITE_TIMEOUT, write_half.flush())
        .await
        .map_err(|_| timed("telnet flush timeout"))??;
    Ok(())
}

/// 带 stream 的 open（connect_session 调用）：连接 + 创建 session + spawn_reader。
impl TelnetSession {
    /// `connect_timeout_secs`：建连超时（复用设置里的 SSH 连接超时；0 = 永不超时）。
    pub async fn connect_and_spawn(
        host: &str,
        port: u16,
        session_config_id: String,
        connect_timeout_secs: u32,
        app: AppHandle,
    ) -> AppResult<Self> {
        log::info!("[telnet] 连接 {}:{}...", host, port);
        // TcpStream::connect 本身无超时，目标不可达/防火墙丢包时可能挂数十秒，
        // 且命令无取消路径——必须包一层超时（与 SSH 侧连接超时行为一致）。
        let connect = TcpStream::connect((host, port));
        let stream = match connect_timeout_secs {
            // 0 = 永不超时。
            0 => connect.await,
            secs => {
                tokio::time::timeout(std::time::Duration::from_secs(u64::from(secs)), connect)
                    .await
                    .map_err(|_| {
                        AppError::Ssh(format!(
                            "Telnet 连接超时（{} 秒，可在设置中调整）",
                            secs
                        ))
                    })?
            }
        }
        .map_err(|e| AppError::Ssh(format!("Telnet 连接失败 {}: {}", host, e)))?;
        let _ = stream.set_nodelay(true);

        let mut session = TelnetSession {
            id: uuid::Uuid::new_v4().to_string(),
            session_config_id,
            app,
            reader_handle: None,
            input_tx: None,
            output_buffer: Arc::new(StdMutex::new(OutputRing::new(OUTPUT_BUFFER_CAP))),
            output_log: Arc::new(StdMutex::new(None)),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 按分片序列喂给解析器，汇总全部纯数据与响应。
    fn parse_all(chunks: &[&[u8]]) -> (Vec<u8>, Vec<u8>) {
        let mut parser = TelnetIacParser::new();
        let mut data = Vec::new();
        let mut responses = Vec::new();
        for c in chunks {
            let p = parser.push(c);
            data.extend_from_slice(&p.data);
            responses.extend_from_slice(&p.responses);
        }
        (data, responses)
    }

    /// 纯数据原样透传，无响应。
    #[test]
    fn passthrough_plain_data() {
        let (data, responses) = parse_all(&[b"hello"]);
        assert_eq!(data, b"hello");
        assert!(responses.is_empty());
    }

    /// IAC IAC 转义为单个 255 数据字节。
    #[test]
    fn iac_iac_escape_is_data() {
        let (data, _) = parse_all(&[&[IAC, IAC, b'a']]);
        assert_eq!(data, vec![IAC, b'a']);
    }

    /// 协商响应：WILL ECHO → DO ECHO；DO 未知 → WONT；DONT 不响应。
    #[test]
    fn negotiation_responses() {
        let (_, r) = parse_all(&[&[IAC, WILL, OPT_ECHO]]);
        assert_eq!(r, vec![IAC, DO, OPT_ECHO]);
        let (_, r) = parse_all(&[&[IAC, DO, 42]]);
        assert_eq!(r, vec![IAC, WONT, 42]);
        let (_, r) = parse_all(&[&[IAC, DONT, OPT_ECHO]]);
        assert!(r.is_empty());
    }

    /// 终端类型子协商 → 回 IS + xterm-256color。
    #[test]
    fn terminal_type_subnegotiation() {
        let (data, r) = parse_all(&[&[IAC, SB, OPT_TERM_TYPE, 1, IAC, SE]]);
        assert!(data.is_empty());
        let mut expect = vec![IAC, SB, OPT_TERM_TYPE, 0];
        expect.extend_from_slice(b"xterm-256color");
        expect.extend_from_slice(&[IAC, SE]);
        assert_eq!(r, expect);
    }

    /// 跨分片：IAC WILL 与选项字节被 TCP 分片切断，必须拼回并正确响应。
    #[test]
    fn split_negotiation_across_chunks() {
        let (data, r) = parse_all(&[&[IAC, WILL], &[OPT_ECHO]]);
        assert!(data.is_empty());
        assert_eq!(r, vec![IAC, DO, OPT_ECHO]);
    }

    /// 跨分片：孤立 IAC 位于 chunk 末尾，下一 chunk 以 IAC 开头 → 转义数据 255。
    #[test]
    fn split_iac_escape_across_chunks() {
        let (data, _) = parse_all(&[&[b'a', IAC], &[IAC, b'b']]);
        assert_eq!(data, vec![b'a', IAC, b'b']);
    }

    /// 跨分片：子协商被切断，前半部分不得漏进数据流，拼全后正常响应。
    #[test]
    fn split_subnegotiation_across_chunks() {
        let (data, r) = parse_all(&[&[b'x', IAC, SB, OPT_TERM_TYPE], &[1, IAC, SE, b'y']]);
        assert_eq!(data, vec![b'x', b'y']);
        let mut expect = vec![IAC, SB, OPT_TERM_TYPE, 0];
        expect.extend_from_slice(b"xterm-256color");
        expect.extend_from_slice(&[IAC, SE]);
        assert_eq!(r, expect);
    }
}
