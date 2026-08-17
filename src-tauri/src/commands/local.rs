//! 本地终端命令：打开本地 shell 标签页、检测本机可用 shell。

use tauri::State;

use crate::error::AppResult;
use crate::local::{self, LocalShellInfo};
use crate::state::AppState;

/// 打开一个本地终端标签页，返回终端实例 id（前端 tab 标识）。
///
/// `shell` 为 shell 标识（"cmd" / "powershell" / "git-bash"）；省略时使用
/// 设置里的默认 shell（`TerminalSettings.local_shell`）。标识非法或本机不可用时
/// 回退到 cmd（`LocalSession::spawn` 内部处理）。
#[tauri::command]
pub fn connect_local_terminal(
    shell: Option<String>,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> AppResult<String> {
    // 读取默认 shell 设置；设置缺失/损坏时回退 "cmd"。
    let shell_id = shell.unwrap_or_else(|| {
        crate::config::settings_load_inner(&state)
            .map(|s| s.terminal.local_shell)
            .unwrap_or_else(|_| "cmd".into())
    });

    let session = crate::local::LocalSession::spawn(&shell_id, &app)?;
    let id = session.id.clone();
    state
        .terminals
        .lock()
        .insert(id.clone(), crate::state::TerminalSession::Local(session));
    Ok(id)
}

/// 列出本机可用的本地 shell（供设置页下拉展示；`available` 为 false 的项前端隐藏）。
#[tauri::command]
pub fn local_terminal_shells() -> AppResult<Vec<LocalShellInfo>> {
    Ok(local::session::detect_shells())
}
