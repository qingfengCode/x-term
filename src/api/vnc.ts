import { invoke } from "@tauri-apps/api/core";

/** VNC 桥接启动结果。 */
export interface VncBridgeInfo {
  /** 桥接实例 id（前端 tab 标识）。 */
  instanceId: string;
  /** 前端 noVNC 直连的 WebSocket 地址。 */
  wsUrl: string;
}

/** 启动一个 VNC 桥接（仅监听 127.0.0.1 临时端口，带随机 token）。 */
export function vncBridgeStart(host: string, port: number): Promise<VncBridgeInfo> {
  return invoke<VncBridgeInfo>("vnc_bridge_start", { host, port });
}

/** 停止并回收一个 VNC 桥接。 */
export function vncBridgeStop(instanceId: string): Promise<void> {
  return invoke<void>("vnc_bridge_stop", { instanceId });
}
