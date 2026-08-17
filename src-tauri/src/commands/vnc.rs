//! VNC 桥接命令：为终端页内嵌 VNC 查看器开/关 WebSocket↔TCP 桥接。

use serde::Serialize;
use tauri::State;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// VNC 桥接启动结果（前端 noVNC 直连信息）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VncBridgeInfo {
    /// 桥接实例 id（前端 tab 标识）。
    pub instance_id: String,
    /// 前端 noVNC 直连的 WebSocket 地址。
    pub ws_url: String,
}

/// 启动一个 VNC 桥接，返回桥接实例 id 与 ws 地址。
///
/// 桥接仅监听 127.0.0.1 临时端口并带随机 token（见 [`crate::vnc`]），
/// 口令等 VNC 认证信息不经过后端（由前端 noVNC 直接与目标协商）。
#[tauri::command]
pub async fn vnc_bridge_start(
    host: String,
    port: u16,
    state: State<'_, AppState>,
) -> AppResult<VncBridgeInfo> {
    if host.trim().is_empty() {
        return Err(AppError::InvalidInput("VNC 主机不能为空".into()));
    }
    if port == 0 {
        return Err(AppError::InvalidInput("VNC 端口无效".into()));
    }

    let bridge = crate::vnc::VncBridge::start(&host, port).await?;
    let info = VncBridgeInfo {
        instance_id: bridge.id.clone(),
        ws_url: bridge.ws_url.clone(),
    };
    state.vnc_bridges.lock().insert(bridge.id.clone(), bridge);
    log::info!("[vnc] 桥接启动: {} -> {}:{}", info.instance_id, host, port);
    Ok(info)
}

/// 停止并回收一个 VNC 桥接（tab 关闭 / 断线清理时调用）。
#[tauri::command]
pub fn vnc_bridge_stop(instance_id: String, state: State<'_, AppState>) -> AppResult<()> {
    // remove 后 Drop 兜底执行 stop（关监听 + 断连接）。
    state
        .vnc_bridges
        .lock()
        .remove(&instance_id)
        .map(|_| ())
        .ok_or_else(|| AppError::NotFound(format!("VNC 桥接 {} 不存在", instance_id)))
}
