//! 本地终端会话实现。
//!
//! 用 portable-pty（Windows ConPTY）在本机启动 shell 子进程（cmd / PowerShell /
//! Git Bash），与 SSH / Telnet 会话共用同一套事件通道与输出环形缓冲：
//! - 后台阻塞 reader 线程从 PTY master 读输出 → 输出缓冲 + emit `terminal:data`
//! - 写输入：`spawn_blocking` 里写 PTY 写端并回 ack（避免大粘贴阻塞运行时）
//! - 子进程退出（读端 EOF）→ 取退出码 → emit `terminal:exit` + `terminal:closed`

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use base64::Engine;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tauri::AppHandle;

use super::LocalShellInfo;
use crate::error::{AppError, AppResult};
use crate::events::{
    emit, TerminalClosedEvent, TerminalDataEvent, TerminalExitEvent, TERMINAL_CLOSED,
    TERMINAL_DATA, TERMINAL_EXIT,
};
use crate::ssh::session::{OutputRing, SharedOutputRing, OUTPUT_BUFFER_CAP};

// ===========================================================================
// Shell 检测与解析
// ===========================================================================

/// 解析好的 shell 启动信息。
struct ResolvedShell {
    /// 展示名（"cmd" / "PowerShell" / "Git Bash"）。
    label: String,
    /// 可执行文件路径。
    program: PathBuf,
    /// 启动参数（Git Bash 加 `--login` 走登录 shell）。
    args: Vec<String>,
}

/// 检测本机可用的 shell 列表（真实环境：搜索根目录 + PATH）。
pub(crate) fn detect_shells() -> Vec<LocalShellInfo> {
    detect_shells_inner(&default_search_roots(), &path_entries())
}

/// 纯逻辑版检测：显式传入搜索根目录与 PATH 条目，便于环境无关的单元测试。
fn detect_shells_inner(roots: &[PathBuf], path_entries: &[PathBuf]) -> Vec<LocalShellInfo> {
    vec![
        LocalShellInfo {
            id: "cmd".into(),
            label: "cmd".into(),
            available: cmd_exe().is_file(),
        },
        LocalShellInfo {
            id: "powershell".into(),
            label: "PowerShell".into(),
            available: powershell_exe(roots, path_entries).is_some(),
        },
        LocalShellInfo {
            id: "git-bash".into(),
            label: "Git Bash".into(),
            available: git_bash_exe(roots, path_entries).is_some(),
        },
    ]
}

/// 解析 shell id 为启动信息；返回 None 表示该 shell 本机不可用。
fn resolve_shell(shell_id: &str) -> Option<ResolvedShell> {
    resolve_shell_inner(shell_id, &default_search_roots(), &path_entries())
}

/// 纯逻辑版解析（单测用）。
fn resolve_shell_inner(
    shell_id: &str,
    roots: &[PathBuf],
    path_entries: &[PathBuf],
) -> Option<ResolvedShell> {
    match shell_id {
        // cmd：Windows 上 COMSPEC 兜底，恒可用；其它平台回退 sh。
        "cmd" => Some(ResolvedShell {
            label: "cmd".into(),
            program: cmd_exe(),
            args: vec![],
        }),
        "powershell" => powershell_exe(roots, path_entries).map(|program| ResolvedShell {
            label: "PowerShell".into(),
            program,
            args: vec![],
        }),
        "git-bash" => git_bash_exe(roots, path_entries).map(|program| ResolvedShell {
            label: "Git Bash".into(),
            program,
            args: vec!["--login".into()],
        }),
        _ => None,
    }
}

/// cmd 可执行文件：Windows 用 COMSPEC（缺失回退标准路径）；其它平台回退 sh。
#[cfg(windows)]
fn cmd_exe() -> PathBuf {
    std::env::var_os("COMSPEC")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"))
}

#[cfg(not(windows))]
fn cmd_exe() -> PathBuf {
    PathBuf::from("/bin/sh")
}

/// PowerShell：标准安装路径优先，其次查 PATH。
#[cfg(windows)]
fn powershell_exe(_roots: &[PathBuf], path_entries: &[PathBuf]) -> Option<PathBuf> {
    let standard = PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
    if standard.is_file() {
        return Some(standard);
    }
    find_in_path("powershell.exe", path_entries)
}

#[cfg(not(windows))]
fn powershell_exe(_roots: &[PathBuf], _path_entries: &[PathBuf]) -> Option<PathBuf> {
    None
}

/// Git Bash：常见安装目录（Program Files / LocalAppData\Programs）优先，其次查 PATH。
#[cfg(windows)]
fn git_bash_exe(roots: &[PathBuf], path_entries: &[PathBuf]) -> Option<PathBuf> {
    for root in roots {
        for sub in ["Git/bin", "Git/cmd"] {
            let candidate = root.join(sub).join("bash.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    find_in_path("bash.exe", path_entries)
}

#[cfg(not(windows))]
fn git_bash_exe(roots: &[PathBuf], path_entries: &[PathBuf]) -> Option<PathBuf> {
    for root in roots {
        let candidate = root.join("bin").join("bash");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    find_in_path("bash", path_entries)
}

/// 默认搜索根目录（Git Bash 常见安装位置）。
#[cfg(windows)]
fn default_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(p) = std::env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os("ProgramFiles(x86)") {
        roots.push(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(p).join("Programs"));
    }
    roots
}

#[cfg(not(windows))]
fn default_search_roots() -> Vec<PathBuf> {
    vec![PathBuf::from("/usr/local"), PathBuf::from("/opt")]
}

/// 当前进程的 PATH 条目。
fn path_entries() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// 在 PATH 各目录下查找可执行文件。
fn find_in_path(name: &str, path_entries: &[PathBuf]) -> Option<PathBuf> {
    path_entries
        .iter()
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

// ===========================================================================
// 本地终端会话
// ===========================================================================

/// 本地终端会话。
///
/// 生命周期：`spawn` 启动 shell 子进程与 reader → 用户交互（write/resize）
/// → 子进程退出时 reader emit 事件 → 前端断开时 `close`（Drop 兜底）。
pub struct LocalSession {
    /// 终端实例 id（前端 tab 标识）。
    pub id: String,
    /// shell 展示名（"cmd" / "PowerShell" / "Git Bash"）。
    pub shell: String,
    app: AppHandle,
    /// PTY master 端：读输出（`try_clone_reader`）、调整窗口大小。
    master: Option<Box<dyn MasterPty + Send>>,
    /// PTY 写端。ConPTY 的 `take_writer` 只能取一次，取出后共享复用。
    writer: Arc<StdMutex<Option<Box<dyn Write + Send>>>>,
    /// 子进程。wait/kill 需要 `&mut`，用 Mutex 包装以便从 `&self` 调用。
    child: Arc<StdMutex<Option<Box<dyn Child + Send + Sync>>>>,
    /// 后台 reader 任务（spawn_blocking 阻塞读）。
    reader_handle: Option<tauri::async_runtime::JoinHandle<()>>,
    /// 共享输出环形缓冲（与 SSH / Telnet 相同类型，AI 上下文感知直接复用）。
    pub output_buffer: SharedOutputRing,
}

impl LocalSession {
    /// 在本机打开一个 shell 终端标签页。
    ///
    /// `shell_id`：shell 标识（"cmd" / "powershell" / "git-bash"）；无法解析或
    /// 本机不可用时回退到 cmd。工作目录为用户主目录，环境变量 `TERM=xterm-256color`。
    pub fn spawn(shell_id: &str, app: &AppHandle) -> AppResult<Self> {
        let resolved = resolve_shell(shell_id).or_else(|| resolve_shell("cmd"));
        let resolved =
            resolved.ok_or_else(|| AppError::Ssh("未找到可用的本地 shell（cmd 不可用）".into()))?;
        log::info!(
            "[local] 启动本地 shell: {} ({})",
            resolved.label,
            resolved.program.display()
        );

        // 打开 PTY（Windows ConPTY），初始尺寸与 SSH PTY 保持一致（80×24）。
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::Ssh(format!("创建本地终端失败: {}", e)))?;

        // 组装启动命令：Git Bash 走登录 shell；cwd=用户主目录；TERM 对齐 xterm.js。
        let mut cmd = CommandBuilder::new(&resolved.program);
        for arg in &resolved.args {
            cmd.arg(arg);
        }
        cmd.env("TERM", "xterm-256color");
        if let Some(home) = dirs::home_dir() {
            cmd.cwd(home);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| AppError::Ssh(format!("启动 {} 失败: {}", resolved.label, e)))?;
        // slave 端在 spawn 后不再需要，先释放（Windows ConPTY 与 master 共享同一
        // 内部句柄，释放无副作用；Unix 上释放 slave fd 是标准做法）。
        drop(pair.slave);

        // take_writer 只能调用一次（ConPTY 内部把写端移出），取出后共享复用。
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| AppError::Ssh(format!("获取终端写端失败: {}", e)))?;

        let mut session = LocalSession {
            id: uuid::Uuid::new_v4().to_string(),
            shell: resolved.label,
            app: app.clone(),
            master: Some(pair.master),
            writer: Arc::new(StdMutex::new(Some(writer))),
            child: Arc::new(StdMutex::new(Some(child))),
            reader_handle: None,
            output_buffer: Arc::new(StdMutex::new(OutputRing::new(OUTPUT_BUFFER_CAP))),
        };
        session.spawn_reader()?;
        Ok(session)
    }

    /// 启动后台 reader：阻塞读 PTY 输出 → 缓冲 + emit；EOF 后取退出码 emit 退出事件。
    fn spawn_reader(&mut self) -> AppResult<()> {
        let app = self.app.clone();
        let session_id = self.id.clone();
        let output_buffer = self.output_buffer.clone();
        let child = self.child.clone();

        let reader = self
            .master
            .as_ref()
            .and_then(|m| m.try_clone_reader().ok())
            .ok_or_else(|| AppError::Ssh("获取终端读端失败".into()))?;

        let join = tauri::async_runtime::spawn_blocking(move || {
            let mut reader = reader;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // 子进程退出 / PTY 关闭
                    Ok(n) => {
                        let data = &buf[..n];
                        // 写入输出环形缓冲（如果锁可用）。
                        if let Ok(mut ob) = output_buffer.lock() {
                            ob.push(data);
                        }
                        // emit 给前端（base64，与 SSH / Telnet 一致）。
                        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
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

            // 读端 EOF：等待子进程结束取退出码（此时进程已退出，wait 立即返回）。
            let exit_code = child
                .lock()
                .ok()
                .and_then(|mut guard| guard.as_mut().and_then(|c| c.wait().ok()))
                .map(|status| status.exit_code() as i32);
            emit(
                &app,
                TERMINAL_EXIT,
                TerminalExitEvent {
                    session_id: session_id.clone(),
                    code: exit_code,
                },
            );
            emit(&app, TERMINAL_CLOSED, TerminalClosedEvent { session_id });
            log::info!("[local] 会话结束，退出码: {:?}", exit_code);
        });

        self.reader_handle = Some(join);
        Ok(())
    }

    /// 向本地 shell 写入数据（前端键盘输入）。
    ///
    /// 同步写：PTY 写端是管道，小数据量（按键/粘贴）写入很快；
    /// 大块数据请走 [`Self::write_with_ack`]（异步 + 背压）。
    pub fn write(&self, data: Vec<u8>) -> AppResult<()> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| AppError::Ssh("本地终端写端不可用".into()))?;
        let writer = guard
            .as_mut()
            .ok_or_else(|| AppError::Ssh("本地终端已关闭".into()))?;
        writer
            .write_all(&data)
            .map_err(|e| AppError::Ssh(format!("写入本地终端失败: {}", e)))?;
        let _ = writer.flush();
        Ok(())
    }

    /// 带写完成确认的写入（背压用）。
    ///
    /// 在 spawn_blocking 里完成写后回 ack，调用方 await 它即获得真实背压，
    /// 避免大粘贴 / ZMODEM 类数据阻塞 Tauri 运行时。语义与 SSH 侧对齐。
    pub fn write_with_ack(&self, data: Vec<u8>) -> AppResult<tokio::sync::oneshot::Receiver<()>> {
        let writer = self.writer.clone();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tauri::async_runtime::spawn_blocking(move || {
            if let Ok(mut guard) = writer.lock() {
                if let Some(w) = guard.as_mut() {
                    let _ = w.write_all(&data);
                    let _ = w.flush();
                }
            }
            // 无论成败都回 ack，避免命令层无限等待；失败时连接即将关闭，
            // 由 TERMINAL_CLOSED 事件兜底。
            let _ = ack_tx.send(());
        });
        Ok(ack_rx)
    }

    /// 通知本地终端窗口大小变化（同步，直接调 ConPTY resize）。
    pub fn resize(&self, cols: u32, rows: u32) -> AppResult<()> {
        let master = self
            .master
            .as_ref()
            .ok_or_else(|| AppError::Ssh("本地终端已关闭".into()))?;
        master
            .resize(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::Ssh(format!("调整终端大小失败: {}", e)))
    }

    /// 取终端最近输出的文本快照（用于 AI 上下文感知）。
    pub fn snapshot(&self, max_bytes: usize) -> String {
        let n = if max_bytes == 0 { 8 * 1024 } else { max_bytes };
        match self.output_buffer.lock() {
            Ok(buf) => buf.snapshot(n),
            Err(_) => String::new(),
        }
    }

    /// 取终端输出的完整快照（整个环形缓冲），与 SSH / Telnet 对齐。
    pub fn full_snapshot(&self) -> String {
        match self.output_buffer.lock() {
            Ok(buf) => buf.snapshot(usize::MAX),
            Err(_) => String::new(),
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

    /// 关闭会话：kill 子进程（读端 EOF → reader 自然退出并 emit closed），
    /// 再 abort reader 兜底。Drop 里同样兜底，防孤儿进程。
    pub fn close(&mut self) -> AppResult<()> {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                let _ = child.kill();
            }
        }
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
        Ok(())
    }
}

impl Drop for LocalSession {
    fn drop(&mut self) {
        // 兜底：kill 子进程并 abort reader，防止本地 shell 成为孤儿进程。
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                let _ = child.kill();
            }
        }
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在系统临时目录创建独立测试目录（测试后清理）。
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("x-term-local-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn git_bash_detected_in_install_roots() {
        let root = temp_dir();
        std::fs::create_dir_all(root.join("Git/bin")).unwrap();
        std::fs::write(root.join("Git/bin/bash.exe"), b"x").unwrap();
        let roots = vec![root.clone()];

        let shells = detect_shells_inner(&roots, &[]);
        let git = shells.iter().find(|s| s.id == "git-bash").unwrap();
        assert!(git.available, "Git Bash 应被检测到");

        let resolved = resolve_shell_inner("git-bash", &roots, &[]).unwrap();
        assert_eq!(resolved.program, root.join("Git/bin/bash.exe"));
        assert_eq!(resolved.args, vec!["--login".to_string()]);
        assert_eq!(resolved.label, "Git Bash");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn git_bash_found_in_path() {
        let root = temp_dir();
        std::fs::write(root.join("bash.exe"), b"x").unwrap();
        let path_entries = vec![root.clone()];

        assert!(resolve_shell_inner("git-bash", &[], &path_entries).is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(windows)]
    #[test]
    fn git_bash_missing_when_no_roots_no_path() {
        // 空搜索根目录 + 空 PATH 时 Git Bash 不可用（环境无关：显式传入空集合）。
        let shells = detect_shells_inner(&[], &[]);
        let git = shells.iter().find(|s| s.id == "git-bash").unwrap();
        assert!(!git.available);
        assert!(resolve_shell_inner("git-bash", &[], &[]).is_none());
    }

    #[test]
    fn cmd_always_resolves() {
        // cmd 在任何平台都应有兜底可执行文件（Windows COMSPEC / 其它平台 sh）。
        let resolved = resolve_shell_inner("cmd", &[], &[]).unwrap();
        assert_eq!(resolved.label, "cmd");
        assert!(resolved.args.is_empty());
    }

    #[test]
    fn unknown_shell_resolves_none() {
        assert!(resolve_shell_inner("no-such-shell", &[], &[]).is_none());
    }
}
