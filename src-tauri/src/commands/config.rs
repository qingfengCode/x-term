//! 全局设置命令。

use tauri::State;

use crate::config::Settings;
use crate::error::AppResult;
use crate::state::AppState;

/// 读取设置。
#[tauri::command]
pub fn settings_load(state: State<'_, AppState>) -> AppResult<Settings> {
    crate::config::settings_load_inner(&state)
}

/// 保存设置。
///
/// 写盘成功后失效内存缓存（见 [`crate::config::settings_invalidate_cache`]），
/// 保证后续建连读到的是新设置。
#[tauri::command]
pub fn settings_save(settings: Settings, state: State<'_, AppState>) -> AppResult<()> {
    let path = state
        .settings_path
        .as_path()
        .join(crate::config::SETTINGS_FILENAME);
    crate::storage::json_store::write_json(&path, &settings)?;
    crate::config::settings_invalidate_cache(&state);
    Ok(())
}
