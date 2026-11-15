//! 系统集成命令（调用操作系统外壳）。
//!
//! 目前只有「在文件管理器中定位文件」一个命令（下载列表的"打开文件位置"）。

use crate::error::{AppError, AppResult};

/// 在系统文件管理器中打开目标所在目录并选中它。
///
/// - Windows：`explorer /select,<path>`（打开父目录并高亮目标）；
/// - macOS：`open -R <path>`；
/// - 其它 Unix：打开父目录（`xdg-open`）。
///
/// 路径不存在时返回 NotFound（前端提示"文件可能已被移动或删除"）——
/// explorer 静默失败会让用户以为按钮点了没反应。
#[tauri::command]
pub async fn reveal_in_folder(path: String) -> AppResult<()> {
    let target = std::path::PathBuf::from(&path);
    if !target.exists() {
        return Err(AppError::NotFound(format!(
            "路径不存在（文件可能已被移动或删除）：{path}"
        )));
    }

    #[cfg(target_os = "windows")]
    // args 分两个传：Windows 命令行拼装后为 `/select, "C:\...\file.txt"`，
    // explorer 需要引号包裹才能正确处理含空格的路径。
    let spawned = std::process::Command::new("explorer")
        .args(["/select,", &path])
        .spawn();

    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open")
        .args(["-R", &path])
        .spawn();

    #[cfg(all(unix, not(target_os = "macos")))]
    let spawned = {
        // 无"选中"语义，退化为打开所在目录。
        let dir = target.parent().unwrap_or(&target);
        std::process::Command::new("xdg-open").arg(dir).spawn()
    };

    spawned.map_err(|e| AppError::Io(std::io::Error::new(e.kind(), format!("打开文件位置失败: {e}"))))?;
    Ok(())
}

/// 用系统默认浏览器打开外部 URL。
///
/// 前端所有"用户点击的链接"都必须走这里，而不是让 webview 自己导航：
/// Tauri 2 中 `<a href="https://...">` 的默认行为是在应用 webview 内导航，
/// 会把整个 X-Term 窗口替换成外部网页（未保存状态全丢，且构成应用内钓鱼）。
///
/// 只允许 `http` / `https`（以及 `mailto`），其它 scheme（`file:`、`javascript:`、
/// `data:` 等）一律拒绝——避免把本地文件或可执行协议交给系统外壳。
#[tauri::command]
pub async fn open_external_url(app: tauri::AppHandle, url: String) -> AppResult<()> {
    let parsed = tauri::Url::parse(url.trim())
        .map_err(|_| AppError::InvalidInput(format!("无效的 URL：{url}")))?;
    match parsed.scheme() {
        "http" | "https" | "mailto" => {}
        other => {
            return Err(AppError::InvalidInput(format!(
                "不允许打开 {other}: 链接（只支持 http/https/mailto）"
            )));
        }
    }
    // shell 插件的 open 在 Rust 侧调用不做 scope 校验（校验只针对前端 JS 调用）。
    #[allow(deprecated)]
    {
        use tauri_plugin_shell::ShellExt;
        app.shell()
            .open(parsed.as_str(), None)
            .map_err(|e| AppError::Io(std::io::Error::other(format!("打开浏览器失败: {e}"))))?;
    }
    Ok(())
}
