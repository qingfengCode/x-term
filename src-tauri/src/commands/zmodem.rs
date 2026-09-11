//! ZMODEM（rz 上传 / sz 下载）专用文件对话框命令。
//!
//! 与 tauri-plugin-dialog 的 JS API 唯一区别：这里**不设置父窗口（owner）**。
//!
//! 原因：Windows 上带 owner 的模态 `IFileDialog::Show(hwnd)` 会禁用宿主窗口，
//! 禁用期间 WebView2（Chromium）会把进程光标置为隐藏——表现为弹窗内鼠标
//! 完全不可见但点击有效，关闭弹窗后恢复（WebView2Feedback #4518 同族
//! bug，微软标记 tracked 尚无修复，亦无浏览器参数可规避，--disable-gpu /
//! --disable-direct-composition 均实测无效）。
//!
//! 无 owner 的对话框不会禁用主窗口，从触发链上绕开该问题，同时保留系统
//! 原生对话框观感。代价是对话框不是应用模态（主窗口仍可点击），但 ZMODEM
//! 传输期间前端本来就会门控终端输入，无实际影响。

use tauri::AppHandle;
use tauri_plugin_dialog::DialogExt;

/// 弹出原生多选文件框（rz 上传）。
///
/// 返回选中文件的绝对路径列表；用户取消返回空数组。
/// 异步命令运行在 tokio 工作线程（非主线程），blocking API 安全。
#[tauri::command]
pub async fn zmodem_pick_files(app: AppHandle, title: Option<String>) -> Vec<String> {
    let mut builder = app.dialog().file();
    if let Some(t) = title {
        builder = builder.set_title(t);
    }
    builder
        .blocking_pick_files()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.to_string())
        .collect()
}

/// 弹出原生保存文件框（sz 下载）。
///
/// 返回完整保存路径；用户取消返回 null。defaultName 预填文件名
/// （对话框落在系统记忆的上次目录）。
#[tauri::command]
pub async fn zmodem_save_file(
    app: AppHandle,
    title: Option<String>,
    default_name: Option<String>,
) -> Option<String> {
    let mut builder = app.dialog().file();
    if let Some(t) = title {
        builder = builder.set_title(t);
    }
    if let Some(n) = default_name {
        builder = builder.set_file_name(n);
    }
    builder.blocking_save_file().map(|p| p.to_string())
}

/// 弹出原生目录选择框（设置 ZMODEM 默认下载目录）。
///
/// 返回选中目录的绝对路径；用户取消返回 null。
#[tauri::command]
pub async fn zmodem_pick_folder(app: AppHandle, title: Option<String>) -> Option<String> {
    let mut builder = app.dialog().file();
    if let Some(t) = title {
        builder = builder.set_title(t);
    }
    builder.blocking_pick_folder().map(|p| p.to_string())
}
