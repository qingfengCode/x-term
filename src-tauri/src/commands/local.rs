//! 本地终端命令：打开本地 shell 标签页、检测本机可用 shell。

use tauri::{Manager, State};

use crate::error::AppResult;
use crate::local::{self, LocalShellInfo};
use crate::state::AppState;

/// 打开一个本地终端标签页，返回终端实例 id（前端 tab 标识）。
///
/// `shell` 为 shell 标识（"cmd" / "powershell" / "git-bash"）；省略时使用
/// 设置里的默认 shell（`TerminalSettings.local_shell`）。标识非法或本机不可用时
/// 回退到 cmd（`LocalSession::spawn` 内部处理）。
///
/// async：ConPTY 创建 + 子进程启动是重操作（进程创建、句柄准备），放阻塞
/// 线程池，避免主线程同步执行时卡 UI。
#[tauri::command]
pub async fn connect_local_terminal(
    shell: Option<String>,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> AppResult<String> {
    let state_app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = state_app.state::<AppState>();
        // 读取默认 shell 设置；设置缺失/损坏时回退 "cmd"。
        let shell_id = shell.unwrap_or_else(|| {
            crate::config::settings_load_inner(&state)
                .map(|s| s.terminal.local_shell)
                .unwrap_or_else(|_| "cmd".into())
        });

        let session = crate::local::LocalSession::spawn(&shell_id, &app)?;
        let id = session.id.clone();
        // 输出日志（设置开启时）：本地 shell 以 shell 展示名命名。
        let log_enabled = crate::config::settings_load_inner(&state)
            .map(|s| s.terminal.output_log)
            .unwrap_or(false);
        let terminal = crate::state::TerminalSession::Local(session);
        if log_enabled {
            let name = format!("local-{}", shell_id);
            terminal.attach_output_log(&name, &state.data_dir.join("logs"));
        }
        state.terminals.lock().insert(id.clone(), terminal);
        Ok(id)
    })
    .await
    .map_err(|e| crate::error::AppError::Ssh(format!("本地终端启动任务失败: {}", e)))?
}

/// 列出本机可用的本地 shell（供设置页下拉展示；`available` 为 false 的项前端隐藏）。
///
/// async：检测遍历安装目录 + PATH 做多次 `is_file()`，放阻塞线程池。
#[tauri::command]
pub async fn local_terminal_shells() -> AppResult<Vec<LocalShellInfo>> {
    tauri::async_runtime::spawn_blocking(local::session::detect_shells)
        .await
        .map_err(|e| crate::error::AppError::Ssh(format!("检测本地 shell 任务失败: {}", e)))
}
