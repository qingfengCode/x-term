//! 应用自更新相关的 Tauri 命令。
//!
//! 对应设置页「关于」Tab 的更新能力，全部委托给 [`crate::updater`]：
//! - [`update_get_info`]：返回当前版本 / 更新源 / 数据目录，供关于页展示。
//! - [`update_get_manifest_url`] / [`update_set_manifest_url`]：读写更新源地址（app.json）。
//! - [`update_check`]：拉取清单比对版本，返回可用更新（无则 null）。
//! - [`update_download`]：下载安装包（进度经 `update:progress` 事件推送）。
//! - [`update_install_and_exit`]：拉起安装器并退出应用。

use tauri::{AppHandle, Manager, State};

use crate::config::APP_CONFIG_FILENAME;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::updater::UpdateManifest;

/// 关于页展示用的应用信息。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    /// 当前运行版本。
    pub current_version: String,
    /// 更新清单地址（可能为空）。
    pub manifest_url: String,
    /// 应用数据目录（安装包下载位置）。
    pub data_dir: String,
    /// Tauri 框架版本。
    pub tauri_version: String,
}

/// app.json 的完整路径。
fn app_config_path(state: &AppState) -> std::path::PathBuf {
    state.settings_path.as_path().join(APP_CONFIG_FILENAME)
}

/// 当前版本号（取自 Cargo.toml / tauri.conf.json 注入的 package info）。
fn current_version(app: &AppHandle) -> String {
    app.package_info().version.to_string()
}

// 以下命令均 async + `spawn_blocking`：app.json 的读写与安装器进程拉起都是
// 阻塞 IO，跑在主线程（Tauri 非 async 命令的默认行为）会冻结整个窗口。

/// 返回关于页所需的应用信息。
#[tauri::command]
pub async fn update_get_info(app: AppHandle) -> AppResult<UpdateInfo> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let cfg = crate::config::app_config_load_inner(&app_config_path(&state))?;
        Ok(UpdateInfo {
            current_version: current_version(&app),
            manifest_url: cfg.update.manifest_url,
            data_dir: state.data_dir.display().to_string(),
            tauri_version: tauri::VERSION.to_string(),
        })
    })
    .await
    .map_err(|e| AppError::Update(format!("读取应用信息任务失败: {}", e)))?
}

/// 读取更新源地址。
#[tauri::command]
pub async fn update_get_manifest_url(state: State<'_, AppState>) -> AppResult<String> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let cfg = crate::config::app_config_load_inner(&app_config_path(&state))?;
        Ok(cfg.update.manifest_url)
    })
    .await
    .map_err(|e| AppError::Update(format!("读取更新源任务失败: {}", e)))?
}

/// 保存更新源地址。
#[tauri::command]
pub async fn update_set_manifest_url(url: String, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let path = app_config_path(&state);
        let mut cfg = crate::config::app_config_load_inner(&path)?;
        cfg.update.manifest_url = url.trim().to_string();
        crate::config::app_config_save_inner(&path, &cfg)
    })
    .await
    .map_err(|e| AppError::Update(format!("保存更新源任务失败: {}", e)))?
}

/// 检查更新：返回可用清单，若已是最新返回 `None`（前端收到 null）。
#[tauri::command]
pub async fn update_check(
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<Option<UpdateManifest>> {
    let cfg = crate::config::app_config_load_inner(&app_config_path(&state))?;
    let url = cfg.update.manifest_url.trim().to_string();
    if url.is_empty() {
        return Err(AppError::Update(
            "尚未配置更新源地址，请先在下方填写".into(),
        ));
    }
    crate::updater::check(&url, &current_version(&app)).await
}

/// 下载安装包，返回落地文件绝对路径。进度经 `update:progress` 事件推送。
#[tauri::command]
pub async fn update_download(
    app: AppHandle,
    state: State<'_, AppState>,
    manifest: UpdateManifest,
) -> AppResult<String> {
    let dest_dir = state.data_dir.as_path().join("updates");
    let path = crate::updater::download(&app, &manifest, &dest_dir).await?;
    Ok(path.display().to_string())
}

/// 拉起安装器并退出应用（不可逆）。
#[tauri::command]
pub async fn update_install_and_exit(app: AppHandle, path: String) -> AppResult<()> {
    tauri::async_runtime::spawn_blocking(move || {
        let installer = std::path::PathBuf::from(path);
        crate::updater::install_and_exit(&app, &installer)
    })
    .await
    .map_err(|e| AppError::Update(format!("启动安装器任务失败: {}", e)))?
}
