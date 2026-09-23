//! 全局设置命令。

use tauri::{Manager, State};

use crate::config::Settings;
use crate::error::AppResult;
use crate::state::AppState;

/// 读取设置。
///
/// async：缓存未命中时同步读盘 + 反序列化，放阻塞线程池（同步命令在主线程
/// 执行，磁盘慢/杀软扫描时会冻结整个 UI）。
#[tauri::command]
pub async fn settings_load(state: State<'_, AppState>) -> AppResult<Settings> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        crate::config::settings_load_inner(&state)
    })
    .await
    .map_err(|e| crate::error::AppError::Ssh(format!("设置读取任务失败: {}", e)))?
}

/// 保存设置。
///
/// 写盘成功后失效内存缓存（见 [`crate::config::settings_invalidate_cache`]），
/// 保证后续建连读到的是新设置。
///
/// async：磁盘写（原子替换）放阻塞线程池，避免主线程同步写盘卡 UI。
#[tauri::command]
pub async fn settings_save(settings: Settings, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let path = state
            .settings_path
            .as_path()
            .join(crate::config::SETTINGS_FILENAME);
        crate::storage::json_store::write_json(&path, &settings)?;
        crate::config::settings_invalidate_cache(&state);
        Ok(())
    })
    .await
    .map_err(|e| crate::error::AppError::Ssh(format!("设置保存任务失败: {}", e)))?
}
