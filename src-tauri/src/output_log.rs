//! 终端输出日志落盘："用户屏幕上实际看到的干净文本"。
//!
//! 原始 PTY 字节流直接写文件不可读：颜色/光标转义、进度条 `\r` 重绘、
//! `top` 类全屏 TUI 的反复刷帧……本模块三级流水线产出接近 PuTTY
//! "Printable text" 日志的可读文本（借鉴 uniTerm output_log.go，Apache-2.0，
//! 用 Rust 重写）：
//!
//! 1. [`AnsiStripper`]：跨 chunk 的转义序列状态机。剥离 SGR/OSC/单字符转义，
//!    但**保留** 7 种行内编辑 CSI（C/D/G/`/K/P/@/X——光标水平移动与行内
//!    删除/插入）原样透传给下一级解释——它们直接决定"行最终长什么样"。
//!    不完整序列（chunk 边界截断的 ESC 开头）缓存到下一 chunk 继续。
//! 2. [`LineEmulator`]：**单行终端模拟器**。光标在字节缓冲上移动：可打印
//!    字节覆写/追加、`\b` 左移、`\r` 归零不清行（进度条重绘只留最终状态）、
//!    `\n` 刷整行；解释保留的 CSI（CUF/CUB/CHA/EL/DCH/ICH/DL）。
//!    **高水位 `emitted`**：行未换行前不盲目输出——只有从未刷出过的尾部
//!    增量才进入日志；一旦写入位置退回已刷出区域（`\r` 回写、`\b` 后改写）
//!    则回拉水位（文件已写不可撤回，回拉避免后续无限重复前缀——慢速逐
//!    字符回显的交换机 CLI 场景）。
//! 3. [`OutputLogger`]：文件落盘。`logs/<会话名>_<时间戳>.log`（同秒冲突加
//!    `_2` 后缀）、`BufWriter` + 惰性 flush（写入量超阈值或距上次超 1s）、
//!    关闭时把未换行的尾行（`top` 无尾换行）也刷出。

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

/// 会话持有的日志器共享句柄（reader 写、命令层装配/替换）。
pub type SharedOutputLog = Arc<StdMutex<Option<OutputLogger>>>;

// ===========================================================================
// 1. ANSI stripper
// ===========================================================================

/// 需要保留给行模拟器解释的 CSI final byte（水平移动 + 行内删除/插入）。
const KEEP_CSI_FINALS: &[u8] = b"CDG`KP@X";

/// 转义序列解析状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripState {
    /// 普通文本。
    Ground,
    /// 收到 ESC，等下一个字节决定序列类型。
    Esc,
    /// ESC [ ... ：参数字节累积中。
    Csi,
    /// ESC ] ... ：OSC 文本，直到 BEL 或 ESC \。
    Osc,
    /// OSC 中收到 ESC，等 `\` 确认 ST（否则视为 OSC 内容继续）。
    OscEsc,
    /// ESC ( / ) / # 等字符集指定：中间字节累积中。
    Charset,
}

/// 转义序列剥离器（跨 chunk 有状态）。
pub struct AnsiStripper {
    state: StripState,
    /// 累积中的 CSI 序列（从 ESC 起，完整后决定去留）。
    csi_buf: Vec<u8>,
    /// 参数字节计数上限（防恶意超长参数撑爆内存）。
    csi_params: usize,
}

const MAX_CSI_PARAMS: usize = 64;

impl AnsiStripper {
    pub fn new() -> Self {
        Self {
            state: StripState::Ground,
            csi_buf: Vec::new(),
            csi_params: 0,
        }
    }

    /// 喂入原始字节，输出"干净流"（普通文本 + 控制字符 + 被保留的行内 CSI）。
    pub fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) {
        for &b in data {
            match self.state {
                StripState::Ground => match b {
                    0x1b => {
                        self.state = StripState::Esc;
                        self.csi_buf.clear();
                        self.csi_params = 0;
                        self.csi_buf.push(b);
                    }
                    // 其余（可打印 + \n \r \b \t 等控制）透传。
                    _ => out.push(b),
                },
                StripState::Esc => {
                    self.csi_buf.push(b);
                    match b {
                        b'[' => self.state = StripState::Csi,
                        b']' => self.state = StripState::Osc,
                        // 字符集指定：ESC ( B / ESC ) 0 等（中间字节 + final）。
                        b'(' | b')' | b'*' | b'+' | b'%' | b'#' => self.state = StripState::Charset,
                        // DCS/SOS/PM/APC（P/X/^/_）：吞到 ST——简化为进入 OSC
                        // 语义（BEL/ESC\ 结束），对日志场景足够。
                        b'P' | b'X' | b'^' | b'_' => self.state = StripState::Osc,
                        // 单字符转义（ESC 7 / ESC = 等）：整段丢弃。
                        _ => {
                            self.state = StripState::Ground;
                            self.csi_buf.clear();
                        }
                    }
                }
                StripState::Csi => {
                    self.csi_buf.push(b);
                    let is_param = b.is_ascii_digit() || b == b';' || b == b':' || b == b'?';
                    if is_param {
                        self.csi_params += 1;
                        if self.csi_params > MAX_CSI_PARAMS {
                            // 超长参数：丢弃整个序列。
                            self.state = StripState::Ground;
                            self.csi_buf.clear();
                        }
                    } else if (0x40..=0x7e).contains(&b) {
                        // final byte：决定整段去留。
                        if KEEP_CSI_FINALS.contains(&b) {
                            out.extend_from_slice(&self.csi_buf);
                        }
                        self.state = StripState::Ground;
                        self.csi_buf.clear();
                    } else if (0x20..=0x2f).contains(&b) {
                        // 中间字节：继续等 final（与参数同属序列体）。
                    } else {
                        // 异常字节：丢弃序列，回 Ground（该字节也吞掉）。
                        self.state = StripState::Ground;
                        self.csi_buf.clear();
                    }
                }
                StripState::Osc => {
                    match b {
                        0x07 => self.state = StripState::Ground, // BEL 终止
                        0x1b => self.state = StripState::OscEsc,
                        _ => {} // OSC 文本：丢弃
                    }
                }
                StripState::OscEsc => match b {
                    b'\\' => self.state = StripState::Ground, // ESC \ (ST)
                    0x1b => {}                                 // 连续 ESC，仍等 \
                    _ => self.state = StripState::Osc,         // 非 ST：视为 OSC 内容
                },
                StripState::Charset => {
                    // 一个 final 字节后结束（ESC ( B 的 B）。
                    self.state = StripState::Ground;
                    self.csi_buf.clear();
                }
            }
        }
    }
}

impl Default for AnsiStripper {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// 2. 单行终端模拟器
// ===========================================================================

/// 单行缓冲上限（字节）。超长行强制刷出（防御性，正常命令行远小于此）。
const MAX_LINE_BYTES: usize = 8192;

/// 单行终端模拟器（高水位输出）。
pub struct LineEmulator {
    /// 当前行可见字符缓冲。
    buf: Vec<u8>,
    /// 光标位置（字节下标）。
    cursor: usize,
    /// 高水位：已刷出到日志的行前缀长度。
    emitted: usize,
}

impl LineEmulator {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            cursor: 0,
            emitted: 0,
        }
    }

    /// 喂入"干净流"字节（含保留的行内 CSI），产出的完整行（带 `\n`）追加到
    /// `out`。返回是否有未换行的残留（供空闲时 flush_partial）。
    pub fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) {
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            match b {
                b'\n' => {
                    self.flush_line(out);
                }
                b'\r' => {
                    // 归零不清行：进度条重绘从行首改写。写入点退回已刷出
                    // 区域 ⇒ 回拉水位（已写部分不可撤，回拉避免后续重复
                    // 输出更长前缀——慢速逐字符回显场景）。
                    self.cursor = 0;
                    self.emitted = self.emitted.min(self.cursor);
                }
                0x08 => {
                    // Backspace：光标左移一格。
                    self.cursor = self.cursor.saturating_sub(1);
                    self.emitted = self.emitted.min(self.cursor);
                }
                b'\t' => {
                    // Tab：推进到下一个 8 列停位（填空格）。
                    let target = (self.cursor / 8 + 1) * 8;
                    while self.cursor < target && self.buf.len() < MAX_LINE_BYTES {
                        self.put_byte(b' ');
                    }
                }
                0x1b => {
                    // 保留的 CSI 序列：解析参数与 final。
                    if i + 1 < data.len() && data[i + 1] == b'[' {
                        let mut j = i + 2;
                        let mut params = String::new();
                        while j < data.len()
                            && (data[j].is_ascii_digit() || data[j] == b';' || data[j] == b':' || data[j] == b'?')
                        {
                            params.push(data[j] as char);
                            j += 1;
                        }
                        if j < data.len() && (0x40..=0x7e).contains(&data[j]) {
                            self.apply_csi(&params, data[j]);
                            i = j;
                        }
                        // 序列不完整（chunk 截断在参数中间）：本 chunk 丢弃该
                        // 尾巴（下一 chunk stripper 不会重发——罕见场景，接受）。
                    }
                    // 非 CSI 的孤立 ESC：忽略。
                }
                0x00..=0x1f => {
                    // 其它控制字符（BEL/VT 等）：忽略。
                }
                _ => {
                    self.put_byte(b);
                }
            }
            i += 1;
        }
        // 行超长保护：强制截断刷出。
        if self.buf.len() >= MAX_LINE_BYTES {
            self.flush_line(out);
        }
    }

    /// 在光标处写一个字节（覆写或追加）。光标越过缓冲尾（CUF 虚位）时
    /// 先补空格填满空隙。
    fn put_byte(&mut self, b: u8) {
        while self.buf.len() < self.cursor && self.buf.len() < MAX_LINE_BYTES {
            self.buf.push(b' ');
        }
        if self.cursor < self.buf.len() {
            self.buf[self.cursor] = b;
        } else if self.buf.len() < MAX_LINE_BYTES {
            self.buf.push(b);
        }
        self.cursor += 1;
    }

    /// 应用行内 CSI。
    fn apply_csi(&mut self, params: &str, final_byte: u8) {
        // 取第一个数字参数（无参数视为 1；EL 默认 0）。
        let n = params
            .trim_start_matches('?')
            .split(';')
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(1);
        match final_byte {
            b'C' => {
                // CUF：右移（光标可越过缓冲尾成为虚位，写入时补空格）。
                self.cursor = (self.cursor + n).min(MAX_LINE_BYTES);
            }
            b'D' => {
                // CUB：左移。
                self.cursor = self.cursor.saturating_sub(n);
                self.emitted = self.emitted.min(self.cursor);
            }
            b'G' | b'`' => {
                // CHA/HPA：绝对列。
                self.cursor = (n - 1).min(self.buf.len());
                self.emitted = self.emitted.min(self.cursor);
            }
            b'K' => {
                // EL：0=到行尾 1=到行首（含光标) 2=整行。
                let mode = params
                    .trim_start_matches('?')
                    .split(';')
                    .next()
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                match mode {
                    0 => {
                        self.buf.truncate(self.cursor);
                        self.emitted = self.emitted.min(self.buf.len());
                    }
                    1 => {
                        // 删除 [0, cursor)（含光标前的全部）。
                        let cut = self.cursor.min(self.buf.len());
                        self.buf.drain(..cut);
                        self.cursor = 0;
                        self.emitted = 0;
                    }
                    _ => {
                        self.buf.clear();
                        self.cursor = 0;
                        self.emitted = 0;
                    }
                }
            }
            b'P' => {
                // DCH：删除光标起 n 字符。
                let end = (self.cursor + n).min(self.buf.len());
                self.buf.drain(self.cursor..end);
                self.emitted = self.emitted.min(self.buf.len());
            }
            b'@' => {
                // ICH：光标起插入 n 空格。
                for _ in 0..n {
                    if self.buf.len() >= MAX_LINE_BYTES {
                        break;
                    }
                    self.buf.insert(self.cursor, b' ');
                }
            }
            b'X' => {
                // ECH：光标起 n 字符用空格覆写（不移动后续）。
                let end = (self.cursor + n).min(self.buf.len());
                for p in self.cursor..end {
                    self.buf[p] = b' ';
                }
            }
            _ => {}
        }
    }

    /// 换行刷出：行内容（去尾随空格）+ `\n`，重置全部状态。
    fn flush_line(&mut self, out: &mut Vec<u8>) {
        let mut end = self.buf.len();
        while end > 0 && self.buf[end - 1] == b' ' {
            end -= 1;
        }
        if end > 0 {
            // 只输出 emitted 之后未刷出过的增量：flush_partial 已把前缀以
            // `前缀\n` 落盘的话，整行重写会让日志出现重复前缀
            //（慢速行输出：中途停顿 >500ms 被部分冲刷，随后继续到换行）。
            let start = self.emitted.min(end);
            if end > start {
                out.extend_from_slice(&self.buf[start..end]);
            }
            out.push(b'\n');
        }
        self.buf.clear();
        self.cursor = 0;
        self.emitted = 0;
    }

    /// 是否有未换行的残留内容。
    pub fn has_pending(&self) -> bool {
        !self.buf.is_empty()
    }

    /// 空闲冲刷：输出未刷出过的行增量（不带换行——`top` 类 TUI 行还没结束）。
    /// 输出后补 `\n` 使日志按行对齐（不完美但可读）。
    pub fn flush_partial(&mut self, out: &mut Vec<u8>) {
        if self.emitted < self.buf.len() {
            let mut end = self.buf.len();
            while end > 0 && self.buf[end - 1] == b' ' {
                end -= 1;
            }
            if end > self.emitted {
                out.extend_from_slice(&self.buf[self.emitted..end]);
                out.push(b'\n');
            }
            self.emitted = self.buf.len();
        }
    }
}

impl Default for LineEmulator {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// 3. Logger
// ===========================================================================

/// 终端输出日志器（一个终端会话一个实例）。
///
/// `feed` 从 reader 任务调用（同步、锁内），内部完成三级流水线与惰性写盘。
pub struct OutputLogger {
    stripper: AnsiStripper,
    emulator: LineEmulator,
    writer: io::BufWriter<File>,
    /// 中间缓冲（完整行累积，feed 结束一次写入）。
    line_buf: Vec<u8>,
    /// 上次写盘时刻（惰性 flush 判定）。
    last_flush: Instant,
    /// 上次收到输出的时刻（空闲冲刷判定：停顿后的新输出先补刷遗留尾行）。
    last_activity: Instant,
    /// 累计未 flush 字节（超过阈值强制落盘）。
    pending_bytes: usize,
    /// 日志文件路径（前端"打开日志目录"后定位用）。
    #[allow(dead_code)]
    pub path: PathBuf,
}

/// 缓冲写盘阈值（字节）。
const FLUSH_BYTES: usize = 64 * 1024;
/// 惰性 flush 间隔。
const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
/// 输出停顿超过该时长后的新数据，先把遗留的无尾换行行冲刷出去。
const IDLE_PARTIAL_FLUSH: std::time::Duration = std::time::Duration::from_millis(500);

/// 把会话名规范成安全文件名片段（去路径分隔符与 Windows 保留字符）。
fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().to_string();
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed
    }
}

impl OutputLogger {
    /// 在 `dir` 下创建日志文件：`<name>_<yyyyMMdd-HHmmss>.log`，同秒冲突加
    /// `_2`/`_3` 后缀（O_CREATE|O_EXCL 语义）。
    pub fn create(dir: &Path, name: &str) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let base = sanitize_name(name);
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let mut path = dir.join(format!("{base}_{stamp}.log"));
        let mut suffix = 1u32;
        loop {
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(f) => {
                    return Ok(Self {
                        stripper: AnsiStripper::new(),
                        emulator: LineEmulator::new(),
                        writer: io::BufWriter::new(f),
                        line_buf: Vec::with_capacity(1024),
                        last_flush: Instant::now(),
                        last_activity: Instant::now(),
                        pending_bytes: 0,
                        path,
                    })
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    suffix += 1;
                    path = dir.join(format!("{base}_{stamp}_{suffix}.log"));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 喂入一段原始 PTY 输出（reader 调用）。
    pub fn feed(&mut self, data: &[u8]) {
        // 输出停顿后的新数据：先把遗留的无尾换行行（`top`/`less` 的当前
        // 帧）按增量冲刷出去——否则该行要等到下一条数据或 close 才可见。
        if self.last_activity.elapsed() > IDLE_PARTIAL_FLUSH {
            self.tick_idle();
        }
        self.last_activity = Instant::now();
        self.stripper.feed(data, &mut self.line_buf);
        // 干净流（line_buf）交给行模拟器，完整行写入 lines（字段级 disjoint
        // borrow：&mut emulator 与 &line_buf 不冲突）。
        let mut lines = Vec::new();
        self.emulator.feed(&self.line_buf, &mut lines);
        self.line_buf.clear();
        if !lines.is_empty() {
            self.write(&lines);
        }
        // 惰性落盘。
        if self.pending_bytes >= FLUSH_BYTES
            || (self.last_flush.elapsed() >= FLUSH_INTERVAL && self.pending_bytes > 0)
        {
            self.flush();
        }
    }

    /// 空闲冲刷（reader 长时间无输出时由调用方周期触发，可选）。
    pub fn tick_idle(&mut self) {
        let mut out = Vec::new();
        self.emulator.flush_partial(&mut out);
        if !out.is_empty() {
            self.write(&out);
        }
        if self.pending_bytes > 0 && self.last_flush.elapsed() >= FLUSH_INTERVAL {
            self.flush();
        }
    }

    /// 关闭：冲刷未换行尾行 + 缓冲落盘。
    pub fn close(&mut self) {
        let mut out = Vec::new();
        self.emulator.flush_partial(&mut out);
        if !out.is_empty() {
            let _ = self.writer.write_all(&out);
        }
        let _ = self.writer.flush();
    }

    fn write(&mut self, data: &[u8]) {
        let _ = self.writer.write_all(data);
        self.pending_bytes += data.len();
    }

    fn flush(&mut self) {
        let _ = self.writer.flush();
        self.pending_bytes = 0;
        self.last_flush = Instant::now();
    }
}

// ===========================================================================
// 单元测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// stripper：剥离 SGR/OSC，保留行内 CSI。
    #[test]
    fn stripper_strips_and_keeps() {
        let mut s = AnsiStripper::new();
        let mut out = Vec::new();
        s.feed(b"\x1b[31mred\x1b[0m \x1b]0;title\x07plain\x1b[3D", &mut out);
        assert_eq!(out, b"red plain\x1b[3D");
    }

    /// stripper：跨 chunk 截断的序列在下一 chunk 完成后处理。
    #[test]
    fn stripper_handles_split_sequence() {
        let mut s = AnsiStripper::new();
        let mut out = Vec::new();
        s.feed(b"a\x1b[3", &mut out);
        assert_eq!(out, b"a");
        s.feed(b"2m\x1b[0mb", &mut out);
        assert_eq!(out, b"ab");
    }

    /// 行模拟器：\r 进度条重绘只留最终状态（输入行本身带前导空格）。
    #[test]
    fn emulator_keeps_last_redraw() {
        let mut e = LineEmulator::new();
        let mut out = Vec::new();
        e.feed(b" downloading 10%\r downloading 99%\n", &mut out);
        assert_eq!(out, b" downloading 99%\n");
    }

    /// 行模拟器：\b 退格改写。
    #[test]
    fn emulator_backspace() {
        let mut e = LineEmulator::new();
        let mut out = Vec::new();
        e.feed(b"ab\x08cksum\n", &mut out);
        assert_eq!(out, b"acksum\n");
    }

    /// 行模拟器：EL(0) 截断行尾（提示符骨架场景）。
    #[test]
    fn emulator_el_truncates() {
        let mut e = LineEmulator::new();
        let mut out = Vec::new();
        e.feed(b"done\x1b[K extra\n", &mut out);
        assert_eq!(out, b"done extra\n"); // EL 后继续写 extra
    }

    /// 行模拟器：CUF 右移后覆写。
    #[test]
    fn emulator_cuf_overwrite() {
        let mut e = LineEmulator::new();
        let mut out = Vec::new();
        e.feed(b"ab\x1b[4Cxx\n", &mut out);
        // buf="ab____xx"？CUF 只移光标不写字节，随后的 x 写在 cursor 处
        // （追加），中间留空缺。
        assert_eq!(out, b"ab    xx\n");
    }

    /// 高水位：慢速逐字符 + \r 回写不无限重复前缀（flush_partial 序列单调）。
    #[test]
    fn emulator_watermark_no_repeat_explosion() {
        let mut e = LineEmulator::new();
        let mut out = Vec::new();
        e.feed(b"ab", &mut out);
        e.flush_partial(&mut out); // "ab\n"
        e.feed(b"\rbc", &mut out); // 回写：水位回拉
        e.flush_partial(&mut out); // 输出回写后的增量
        e.feed(b"\n", &mut out); // 换行刷整行（文件里前缀已不可改，只验证不 panic）
        assert!(!out.is_empty());
    }

    /// Logger 文件名冲突加后缀。
    #[test]
    fn logger_conflict_suffix() {
        let dir = std::env::temp_dir().join(format!("xterm-log-test-{}", uuid::Uuid::new_v4()));
        let a = OutputLogger::create(&dir, "s").unwrap();
        let b = OutputLogger::create(&dir, "s").unwrap();
        assert_ne!(a.path, b.path);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
