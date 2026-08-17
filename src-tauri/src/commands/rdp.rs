//! RDP 桥接命令：为终端页内嵌 RDP 客户端开/关 WebSocket 桥接（迷你网关）。

use serde::Serialize;
use tauri::State;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// RDP 桥接启动结果（前端 IronRDP WASM 直连信息）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RdpBridgeInfo {
    /// 桥接实例 id（前端 tab 标识）。
    pub instance_id: String,
    /// 前端 WASM 客户端直连的 WebSocket 地址。
    pub ws_url: String,
}

/// 启动一个 RDP 桥接，返回桥接实例 id 与 ws 地址。
///
/// 桥接仅监听 127.0.0.1 临时端口并带随机 token（见 [`crate::rdp`]）。
/// host/port 为期望目标（请求 PDU 携带的 destination 优先），用户名/口令
/// 不经过后端（由前端 WASM 的 CredSSP 与目标直接协商）。
#[tauri::command]
pub async fn rdp_bridge_start(
    host: String,
    port: u16,
    verify_cert: Option<bool>,
    state: State<'_, AppState>,
) -> AppResult<RdpBridgeInfo> {
    if host.trim().is_empty() {
        return Err(AppError::InvalidInput("RDP 主机不能为空".into()));
    }
    if port == 0 {
        return Err(AppError::InvalidInput("RDP 端口无效".into()));
    }

    let bridge = crate::rdp::RdpBridge::start(&host, port, verify_cert.unwrap_or(false)).await?;
    let info = RdpBridgeInfo {
        instance_id: bridge.id.clone(),
        ws_url: bridge.ws_url.clone(),
    };
    state.rdp_bridges.lock().insert(bridge.id.clone(), bridge);
    log::info!(
        "[rdp] 桥接启动: {} -> {}:{} (校验证书: {})",
        info.instance_id,
        host,
        port,
        verify_cert.unwrap_or(false)
    );
    Ok(info)
}

/// 停止并回收一个 RDP 桥接（tab 关闭 / 断线清理时调用）。
#[tauri::command]
pub fn rdp_bridge_stop(instance_id: String, state: State<'_, AppState>) -> AppResult<()> {
    // remove 后 Drop 兜底执行 stop（关监听 + 断连接）。
    state
        .rdp_bridges
        .lock()
        .remove(&instance_id)
        .map(|_| ())
        .ok_or_else(|| AppError::NotFound(format!("RDP 桥接 {} 不存在", instance_id)))
}
