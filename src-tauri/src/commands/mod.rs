//! Tauri 命令模块。
//!
//! 所有 `#[tauri::command]` 都集中在此目录下，按业务领域分文件。
//! 前端通过 `invoke('command_name', { ... })` 调用。
//!
//! 命令的完整列表在 [`register_handlers`] 中通过 `generate_handler!` 宏注册。

pub mod ai;
pub mod backup;
pub mod config;
pub mod db;
pub mod file_backend;
pub mod forward;
pub mod local;
pub mod mcp;
pub mod rdp;
pub mod remote_desktop;
pub mod session;
pub mod sftp;
pub mod ssh_key;
pub mod terminal;
pub mod totp;
pub mod update;
pub mod vault;
pub mod vnc;

/// 把所有命令注册到给定的 [`tauri::Builder`] 上，返回 builder 自身以便链式调用。
pub fn register(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    builder.invoke_handler(tauri::generate_handler![
        // vault
        crate::commands::vault::vault_exists,
        crate::commands::vault::vault_create,
        crate::commands::vault::vault_unlock,
        crate::commands::vault::vault_unlocked,
        crate::commands::vault::vault_lock,
        crate::commands::vault::credential_save,
        crate::commands::vault::credential_list,
        crate::commands::vault::credential_rename,
        crate::commands::vault::credential_get,
        crate::commands::vault::credential_delete,
        // ssh key（密钥对生成）
        crate::commands::ssh_key::ssh_key_generate,
        // local terminal（本机 shell 标签页）
        crate::commands::local::connect_local_terminal,
        crate::commands::local::local_terminal_shells,
        // vnc（内嵌 VNC 查看器的 WS↔TCP 桥接）
        crate::commands::vnc::vnc_bridge_start,
        crate::commands::vnc::vnc_bridge_stop,
        // rdp（内嵌 RDP 客户端的迷你网关桥接）
        crate::commands::rdp::rdp_bridge_start,
        crate::commands::rdp::rdp_bridge_stop,
        // session
        crate::commands::session::list_sessions,
        crate::commands::session::get_session,
        crate::commands::session::save_session,
        crate::commands::session::delete_session,
        crate::commands::session::list_groups,
        crate::commands::session::save_group,
        crate::commands::session::delete_group,
        crate::commands::session::connect_session,
        crate::commands::session::connect_session_with_manual_auth,
        crate::commands::session::disconnect_session,
        crate::commands::session::open_sftp_for_session,
        crate::commands::session::ssh_auth_respond,
        crate::commands::session::ssh_host_key_respond,
        // terminal
        crate::commands::terminal::terminal_write,
        crate::commands::terminal::terminal_resize,
        crate::commands::terminal::terminal_attach,
        crate::commands::terminal::terminal_snapshot,
        // sftp
        crate::commands::sftp::sftp_list,
        crate::commands::sftp::sftp_stat,
        crate::commands::sftp::sftp_pwd,
        crate::commands::sftp::sftp_mkdir,
        crate::commands::sftp::sftp_rename,
        crate::commands::sftp::sftp_remove,
        crate::commands::sftp::sftp_download,
        crate::commands::sftp::sftp_upload,
        crate::commands::sftp::sftp_close,
        crate::commands::sftp::sftp_transfer_cancel,
        // file backend (S3 / 兼容存储)
        crate::commands::file_backend::file_account_list,
        crate::commands::file_backend::file_account_save,
        crate::commands::file_backend::file_account_delete,
        crate::commands::file_backend::file_connect,
        crate::commands::file_backend::file_disconnect,
        crate::commands::file_backend::file_list,
        crate::commands::file_backend::file_stat,
        crate::commands::file_backend::file_mkdir,
        crate::commands::file_backend::file_rename,
        crate::commands::file_backend::file_remove,
        crate::commands::file_backend::file_download,
        crate::commands::file_backend::file_upload,
        // forward
        crate::commands::forward::forward_start,
        crate::commands::forward::forward_stop,
        crate::commands::forward::forward_list_rules,
        crate::commands::forward::forward_list_running,
        crate::commands::forward::forward_save_rule,
        crate::commands::forward::forward_delete_rule,
        // remote desktop
        crate::commands::remote_desktop::remote_desktop_launch,
        crate::commands::remote_desktop::desktop_list,
        crate::commands::remote_desktop::desktop_save,
        crate::commands::remote_desktop::desktop_delete,
        crate::commands::remote_desktop::desktop_save_size,
        crate::commands::remote_desktop::desktop_group_list,
        crate::commands::remote_desktop::desktop_group_save,
        crate::commands::remote_desktop::desktop_group_delete,
        // ai
        crate::commands::ai::ai_chat,
        crate::commands::ai::ai_execute_tool,
        crate::commands::ai::ai_cancel_tool,
        crate::commands::ai::ai_desktop_tool_respond,
        crate::commands::ai::ai_ask_user_respond,
        crate::commands::ai::ai_stop,
        crate::commands::ai::ai_add_to_whitelist,
        crate::commands::ai::set_workspace_dir,
        crate::commands::ai::ai_list_conversations,
        crate::commands::ai::ai_save_conversations,
        // config
        crate::commands::config::settings_load,
        crate::commands::config::settings_save,
        // totp
        crate::commands::totp::totp_list,
        crate::commands::totp::totp_add,
        crate::commands::totp::totp_delete,
        crate::commands::totp::totp_generate,
        crate::commands::totp::totp_generate_for_secret,
        crate::commands::totp::totp_fill_terminal,
        // db (MySQL)
        crate::commands::db::db_list_profiles,
        crate::commands::db::db_save_profile,
        crate::commands::db::db_delete_profile,
        crate::commands::db::db_list_groups,
        crate::commands::db::db_save_group,
        crate::commands::db::db_delete_group,
        crate::commands::db::db_connect,
        crate::commands::db::db_disconnect,
        crate::commands::db::db_use_database,
        crate::commands::db::db_exec_sql,
        crate::commands::db::db_list_tables,
        crate::commands::db::db_list_databases,
        crate::commands::db::db_describe_table,
        crate::commands::db::db_show_create_table,
        // mcp
        crate::commands::mcp::mcp_start,
        crate::commands::mcp::mcp_stop,
        crate::commands::mcp::mcp_status,
        crate::commands::mcp::mcp_save_config,
        crate::commands::mcp::mcp_load_config,
        crate::commands::mcp::mcp_generate_token,
        crate::commands::mcp::mcp_respond_approval,
        crate::commands::mcp::mcp_rebind,
        crate::commands::mcp::mcp_log,
        // update（应用自更新）
        crate::commands::update::update_get_info,
        crate::commands::update::update_get_manifest_url,
        crate::commands::update::update_set_manifest_url,
        crate::commands::update::update_check,
        crate::commands::update::update_download,
        crate::commands::update::update_install_and_exit,
        // backup（数据迁移：加密导入/导出）
        crate::commands::backup::backup_export,
        crate::commands::backup::backup_inspect,
        crate::commands::backup::backup_import,
    ])
}
