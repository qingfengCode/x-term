//! X-Term 后端库入口。
//!
//! 由 `main.rs` 调用 [`run`] 启动 Tauri 应用。setup 阶段初始化：
//! - 应用数据目录；
//! - SQLite 连接池（运行迁移）；
//! - [`AppState`] 注入到 Tauri；
//! - 注册所有命令。

pub mod ai;
pub mod backup;
pub mod commands;
pub mod config;
pub mod database;
pub mod error;
pub mod encoding;
pub mod events;
pub mod file_backend;
pub mod local;
pub mod mcp;
pub mod monitor;
pub mod output_log;
pub mod rdp;
pub mod ssh;
pub mod state;
pub mod storage;
pub mod telnet;
pub mod totp;
pub mod updater;
pub mod utils;
pub mod vnc;

use state::AppState;
use storage::db;
use storage::json_store;
use tauri::Manager;

/// 启动 Tauri 应用。
pub fn run() {
    // 日志统一由 tauri-plugin-log 管理（stdout + 应用日志目录），并承接前端
    // attachConsole 转发的 webview console 输出。注意：不能再额外初始化
    // env_logger 等全局 logger，否则插件初始化会 panic（logger 已存在）。
    // 支持多开：不注册单实例锁，用户可同时运行多个 X-Term 进程（如分别连
    // 不同环境）。共享资源的跨进程安全由各层保证：
    // - xterm.db：WAL + busy_timeout=5s，写竞争由 SQLite 串行化等待；
    // - 启动迁移：建表/加列均幂等（add_column_if_missing 容忍并发 ALTER 竞态）；
    // - JSON 配置：write_json 用带 pid 的唯一 tmp/bak 名原子替换（最后写入者胜出）；
    // - MCP / SSH 隧道监听端口：用户配置，冲突时后启动的实例报「端口被占用」；
    // - RDP/VNC 桥接、数据库监控端口：绑定 :0 临时端口，互不冲突。
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_os::init())
        .plugin(
            tauri_plugin_log::Builder::new()
                // 开发态全量 Debug 便于排障；发布态只留 Info（桥接生命周期/错误）。
                .level(if cfg!(debug_assertions) {
                    log::LevelFilter::Debug
                } else {
                    log::LevelFilter::Info
                })
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("x-term.log".into()),
                    }),
                ])
                .build(),
        )
        .setup(|app| {
            // 1. 数据目录。
            let data_dir = json_store::app_data_dir()?;
            log::info!("应用数据目录: {}", data_dir.display());

            // 2. 数据库。
            let pool = db::init_pool(&data_dir)?;

            // 3. 设置路径（与 data_dir 相同）。
            let settings_path = data_dir.clone();

            // 4. 注入状态。
            let state = AppState::new(data_dir, pool, settings_path, app.handle().clone());
            app.manage(state);

            Ok(())
        });

    // 注册命令（命令列表见 commands::register）。
    let builder = commands::register(builder);

    builder
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}
