//! 终端输出日志命令。

use tauri::{Manager, State};

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// 在系统文件管理器中打开日志目录（不存在则创建），返回目录路径。
///
/// 日志文件由会话建立时按设置装配（见 `attach_output_log_if_enabled`），
/// 命名为 `<会话名>_<yyyyMMdd-HHmmss>.log`。
///
/// async + `spawn_blocking`：建目录 + 拉起 explorer/xdg-open 是阻塞系统调用
/// （进程创建在杀软环境下可达数百毫秒），跑在主线程会冻结整个窗口。
#[tauri::command]
pub async fn open_logs_dir(state: State<'_, AppState>) -> AppResult<String> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let dir = state.data_dir.join("logs");
        std::fs::create_dir_all(&dir)?;
        #[cfg(target_os = "windows")]
        let spawned = std::process::Command::new("explorer").arg(&dir).spawn();
        #[cfg(not(target_os = "windows"))]
        let spawned = std::process::Command::new("xdg-open").arg(&dir).spawn();
        match spawned {
            Ok(_) => Ok(dir.to_string_lossy().into_owned()),
            Err(e) => Err(AppError::Ssh(format!("打开日志目录失败: {e}"))),
        }
    })
    .await
    .map_err(|e| AppError::Ssh(format!("打开日志目录任务失败: {}", e)))?
}
