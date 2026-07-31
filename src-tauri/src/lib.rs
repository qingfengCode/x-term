//! X-Term 后端库入口。
//!
//! 由 `main.rs` 调用 [`run`] 启动 Tauri 应用。setup 阶段初始化：
//! - 应用数据目录；
//! - SQLite 连接池（运行迁移）；
//! - [`AppState`] 注入到 Tauri；
//! - 注册所有命令。

pub mod ai;
pub mod commands;
pub mod config;
pub mod database;
pub mod error;
pub mod events;
pub mod mcp;
pub mod ssh;
pub mod state;
pub mod storage;
pub mod telnet;
pub mod totp;
pub mod utils;

use state::AppState;
use storage::db;
use storage::json_store;
use tauri::Manager;

/// 启动 Tauri 应用。
pub fn run() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_os::init())
        .setup(|app| {
            // 1. 数据目录。
            let data_dir = json_store::app_data_dir()?;
            log::info!("应用数据目录: {}", data_dir.display());

            // 2. 数据库。
            let pool = db::init_pool(&data_dir)?;

            // 3. 设置路径（与 data_dir 相同）。
            let settings_path = data_dir.clone();

            // 4. 注入状态。
            let state = AppState::new(data_dir, pool, settings_path);
            app.manage(state);

            Ok(())
        });

    // 注册命令（命令列表见 commands::register）。
    let builder = commands::register(builder);

    builder
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}
