//! 远程桌面（RDP/VNC）启动命令。
//!
//! 不嵌入 webview，而是启动本地系统客户端：
//! - RDP（Windows）: 生成临时 .rdp 文件，mstsc 打开
//! - VNC: 尝试系统已装的 VNC 客户端（vncviewer）

use std::process::Command;

use tauri::State;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// 启动远程桌面客户端。
///
/// - `protocol`: "rdp" 或 "vnc"
/// - `host`/`port`: 目标地址
/// - `username`/`password`: 可选凭据（RDP 写入 .rdp 文件；VNC 多数客户端不支持命令行传密码）
#[tauri::command]
pub fn remote_desktop_launch(
    protocol: String,
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
    _state: State<'_, AppState>,
) -> AppResult<String> {
    let addr = if (protocol == "rdp" && port == 3389) || (protocol == "vnc" && port == 5900) {
        host.clone()
    } else {
        format!("{}:{}", host, port)
    };

    match protocol.as_str() {
        "rdp" => launch_rdp(&addr, username.as_deref(), password.as_deref()),
        "vnc" => launch_vnc(&addr),
        other => Err(AppError::InvalidInput(format!("不支持的桌面协议: {other}"))),
    }
}

/// Windows RDP：生成临时 .rdp 文件并用 mstsc 打开。
fn launch_rdp(addr: &str, username: Option<&str>, password: Option<&str>) -> AppResult<String> {
    // .rdp 是每行 "key:s:value" 的文本格式，值里带换行会注入伪造配置行
    // （如 redirectclipboard、full address 覆盖），必须拒绝。
    validate_rdp_field(addr, "主机")?;
    if let Some(u) = username {
        validate_rdp_field(u, "用户名")?;
    }

    // 构建 .rdp 文件内容。
    let mut rdp = String::new();
    rdp.push_str("full address:s:");
    rdp.push_str(addr);
    rdp.push('\n');
    rdp.push_str("prompt for credentials:i:0\n");
    if let Some(u) = username {
        rdp.push_str(&format!("username:s:{}\n", u));
    }
    // 密码不写入 .rdp 文件（明文落盘不安全）；Windows 下用 cmdkey 预存到
    // 凭据管理器（系统加密存储），mstsc 连接时自动使用。
    #[cfg(target_os = "windows")]
    if let (Some(u), Some(p)) = (username, password) {
        store_rdp_credential(addr, u, p)?;
    }
    #[cfg(not(target_os = "windows"))]
    let _ = password;

    // 写入临时文件。
    let temp_dir = std::env::temp_dir();
    let rdp_path = temp_dir.join(format!(
        "xterm-{}.rdp",
        chrono::Utc::now().timestamp_millis()
    ));
    std::fs::write(&rdp_path, &rdp).map_err(|e| AppError::Ssh(format!("写 RDP 文件失败: {e}")))?;

    log::info!("[remote-desktop] 启动 mstsc, rdp 文件: {:?}", rdp_path);

    #[cfg(target_os = "windows")]
    {
        Command::new("mstsc")
            .arg(&rdp_path)
            .spawn()
            .map_err(|e| AppError::Ssh(format!("启动 mstsc 失败: {e}")))?;
    }
    #[cfg(target_os = "macos")]
    {
        // macOS 无原生 RDP，提示用户。
        return Err(AppError::Ssh(
            "macOS 请安装 Microsoft Remote Desktop 后手动连接".into(),
        ));
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xfreerdp")
            .arg(format!("/v:{}", addr))
            .spawn()
            .or_else(|_| Command::new("remmina").spawn())
            .map_err(|e| {
                AppError::Ssh(format!(
                    "启动 RDP 客户端失败（请安装 xfreerdp/remmina）: {e}"
                ))
            })?;
    }

    Ok(format!("已启动 RDP 客户端连接 {}", addr))
}

/// 校验 .rdp 文件字段不包含换行符（防止注入伪造配置行）。
fn validate_rdp_field(field: &str, name: &str) -> AppResult<()> {
    if field.contains(['\r', '\n']) {
        return Err(AppError::InvalidInput(format!("{name} 不能包含换行符")));
    }
    Ok(())
}

/// Windows：用 cmdkey 预存 RDP 凭据，mstsc 连接时自动使用。
#[cfg(target_os = "windows")]
fn store_rdp_credential(addr: &str, username: &str, password: &str) -> AppResult<()> {
    let status = Command::new("cmdkey")
        .arg(format!("/generic:TERMSRV/{addr}"))
        .arg(format!("/user:{username}"))
        .arg(format!("/pass:{password}"))
        .status()
        .map_err(|e| AppError::Ssh(format!("cmdkey 预存凭据失败: {e}")))?;
    if !status.success() {
        return Err(AppError::Ssh(
            "cmdkey 预存凭据失败（Windows 凭据管理器不可用）".into(),
        ));
    }
    Ok(())
}

/// VNC：尝试常见 VNC 客户端。
fn launch_vnc(addr: &str) -> AppResult<String> {
    log::info!("[remote-desktop] 启动 VNC 连接 {}", addr);

    #[cfg(target_os = "windows")]
    {
        // Windows: 尝试 vncviewer（RealVNC/TigerVNC/UltraVNC 都可能注册此名）。
        let vnc_addr = if addr.contains(':') {
            addr.replace(':', "::") // vncviewer 用 host::port
        } else {
            addr.to_string()
        };
        Command::new("vncviewer")
            .arg(&vnc_addr)
            .spawn()
            .map_err(|e| AppError::Ssh(format!("启动 vncviewer 失败（请安装 VNC 客户端）: {e}")))?;
    }
    #[cfg(target_os = "macos")]
    {
        // macOS 内置 Screen Sharing。
        let url = if addr.contains(':') {
            format!("vnc://{}", addr)
        } else {
            format!("vnc://{}:5900", addr)
        };
        Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| AppError::Ssh(format!("启动 Screen Sharing 失败: {e}")))?;
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("vncviewer")
            .arg(addr)
            .spawn()
            .map_err(|e| AppError::Ssh(format!("启动 vncviewer 失败（请安装 VNC 客户端）: {e}")))?;
    }

    Ok(format!("已启动 VNC 客户端连接 {}", addr))
}

// ---------------------------------------------------------------------------
// 桌面会话 CRUD（独立于终端 sessions）
// ---------------------------------------------------------------------------

use crate::storage::desktops_repo::Desktop;

#[tauri::command]
pub fn desktop_list(state: State<'_, AppState>) -> AppResult<Vec<Desktop>> {
    let conn = state.conn()?;
    crate::storage::desktops_repo::list_desktops(&conn)
}

#[tauri::command]
pub fn desktop_save(desktop: Desktop, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    crate::storage::desktops_repo::upsert_desktop(&conn, &desktop)
}

#[tauri::command]
pub fn desktop_delete(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    crate::storage::desktops_repo::delete_desktop(&conn, &id)
}
