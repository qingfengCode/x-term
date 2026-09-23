//! 远程桌面（RDP/VNC）启动命令。
//!
//! 不嵌入 webview，而是启动本地系统客户端：
//! - RDP（Windows）: 生成临时 .rdp 文件，mstsc 打开
//! - VNC: 尝试系统已装的 VNC 客户端（vncviewer）

use std::process::Command;

use tauri::{Manager, State};

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// 启动远程桌面客户端。
///
/// - `protocol`: "rdp" 或 "vnc"
/// - `host`/`port`: 目标地址
/// - `username`/`password`: 可选凭据（RDP 写入 .rdp 文件；VNC 多数客户端不支持命令行传密码）
///
/// async + `spawn_blocking`：写临时 .rdp 文件、CredWriteW、启动外部客户端均为
/// 阻塞系统调用（进程创建在杀软环境下可达数百毫秒），跑在主线程会冻结 UI。
#[tauri::command]
pub async fn remote_desktop_launch(
    protocol: String,
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
    _state: State<'_, AppState>,
) -> AppResult<String> {
    tauri::async_runtime::spawn_blocking(move || {
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
    })
    .await
    .map_err(|e| AppError::Ssh(format!("启动远程桌面任务失败: {}", e)))?
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

/// Windows：向凭据管理器预存 RDP 凭据，mstsc 连接时自动使用。
///
/// 直接调用 CredWriteW 而非 `cmdkey /pass:...` 子进程——后者会把密码明文放进
/// 进程命令行，本机任意进程可通过进程枚举（WMI Win32_Process CommandLine）、
/// 事件日志或崩溃转储读到。
#[cfg(target_os = "windows")]
fn store_rdp_credential(addr: &str, username: &str, password: &str) -> AppResult<()> {
    use std::ffi::c_void;

    const CRED_TYPE_GENERIC: u32 = 1;
    const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;

    #[repr(C)]
    struct FILETIME {
        dw_low_date_time: u32,
        dw_high_date_time: u32,
    }

    #[repr(C)]
    struct CREDENTIALW {
        flags: u32,
        type_: u32,
        target_name: *mut u16,
        comment: *mut u16,
        last_written: FILETIME,
        credential_blob_size: u32,
        credential_blob: *mut u8,
        persist: u32,
        attribute_count: u32,
        attributes: *mut c_void,
        target_alias: *mut u16,
        username: *mut u16,
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn CredWriteW(credential: *const CREDENTIALW, flags: u32) -> i32;
    }

    let target = format!("TERMSRV/{addr}");
    let mut target_utf16: Vec<u16> = target.encode_utf16().collect();
    target_utf16.push(0);
    let mut user_utf16: Vec<u16> = username.encode_utf16().collect();
    user_utf16.push(0);
    // 密码以 UTF-16 字节写入 blob（与 cmdkey 行为一致），仅在非空时写入。
    let mut pass_utf16: Vec<u16> = password.encode_utf16().collect();
    pass_utf16.push(0);

    let cred = CREDENTIALW {
        flags: 0,
        type_: CRED_TYPE_GENERIC,
        target_name: target_utf16.as_mut_ptr(),
        comment: std::ptr::null_mut(),
        last_written: FILETIME {
            dw_low_date_time: 0,
            dw_high_date_time: 0,
        },
        credential_blob_size: (pass_utf16.len() * 2) as u32,
        credential_blob: pass_utf16.as_mut_ptr().cast::<u8>(),
        persist: CRED_PERSIST_LOCAL_MACHINE,
        attribute_count: 0,
        attributes: std::ptr::null_mut(),
        target_alias: std::ptr::null_mut(),
        username: user_utf16.as_mut_ptr(),
    };

    if unsafe { CredWriteW(&cred, 0) } == 0 {
        return Err(AppError::Ssh(
            "预存 RDP 凭据失败（Windows 凭据管理器不可用）".into(),
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

// 全部 async + `spawn_blocking`：取连接 + SQLite 语句跑在主线程会冻结整个窗口。

use crate::storage::desktops_repo::{Desktop, DesktopGroup};

#[tauri::command]
pub async fn desktop_list(state: State<'_, AppState>) -> AppResult<Vec<Desktop>> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::desktops_repo::list_desktops(&conn)
    })
    .await
    .map_err(|e| AppError::Storage(format!("读取桌面连接列表任务失败: {}", e)))?
}

#[tauri::command]
pub async fn desktop_save(desktop: Desktop, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::desktops_repo::upsert_desktop(&conn, &desktop)
    })
    .await
    .map_err(|e| AppError::Storage(format!("保存桌面连接任务失败: {}", e)))?
}

#[tauri::command]
pub async fn desktop_delete(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::desktops_repo::delete_desktop(&conn, &id)
    })
    .await
    .map_err(|e| AppError::Storage(format!("删除桌面连接任务失败: {}", e)))?
}

/// 记录断开时最后一次使用的 RDP 分辨率（"宽x高"），重连时恢复。
#[tauri::command]
pub async fn desktop_save_size(
    id: String,
    size: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::desktops_repo::update_desktop_size(&conn, &id, size.as_deref())
    })
    .await
    .map_err(|e| AppError::Storage(format!("保存桌面分辨率任务失败: {}", e)))?
}

#[tauri::command]
pub async fn desktop_group_list(state: State<'_, AppState>) -> AppResult<Vec<DesktopGroup>> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::desktops_repo::list_desktop_groups(&conn)
    })
    .await
    .map_err(|e| AppError::Storage(format!("读取桌面分组任务失败: {}", e)))?
}

#[tauri::command]
pub async fn desktop_group_save(group: DesktopGroup, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::desktops_repo::upsert_desktop_group(&conn, &group)
    })
    .await
    .map_err(|e| AppError::Storage(format!("保存桌面分组任务失败: {}", e)))?
}

#[tauri::command]
pub async fn desktop_group_delete(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::storage::desktops_repo::delete_desktop_group(&conn, &id)
    })
    .await
    .map_err(|e| AppError::Storage(format!("删除桌面分组任务失败: {}", e)))?
}
