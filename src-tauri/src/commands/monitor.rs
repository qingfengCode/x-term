//! 服务器监控命令（启动 / 停止）。
//!
//! 监控循环本体见 [`crate::monitor`]：一条持久 SSH 连接 + 3s 采集，
//! 数据经 `monitor:data` 事件推送，结束经 `monitor:closed` 通知。

use tauri::State;

use crate::error::AppResult;
use crate::monitor;
use crate::state::AppState;

/// 对某会话启动服务器监控，返回 monitorId（事件按它过滤）。
///
/// 前端重开面板前应先 `monitor_stop` 旧的（map 按 monitorId 索引，不按会话
/// 查重——保持登记结构最简）。
#[tauri::command]
pub async fn monitor_start(
    session_config_id: String,
    state: State<'_, AppState>,
) -> AppResult<String> {
    let monitor_id = format!("mon-{}", uuid::Uuid::new_v4());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    state.monitors.lock().insert(monitor_id.clone(), stop_tx);

    let app_state = state.inner().clone();
    let mid = monitor_id.clone();
    // 循环任务结束时自清理登记。
    let monitors = state.monitors.clone();
    tokio::spawn(async move {
        monitor::start_monitor(app_state, session_config_id, mid.clone(), stop_rx).await;
        monitors.lock().remove(&mid);
    });
    Ok(monitor_id)
}

/// 停止服务器监控。
#[tauri::command]
pub fn monitor_stop(monitor_id: String, state: State<'_, AppState>) -> AppResult<()> {
    if let Some(tx) = state.monitors.lock().remove(&monitor_id) {
        let _ = tx.send(true);
    }
    Ok(())
}
