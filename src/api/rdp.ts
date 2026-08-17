import { invoke } from "@tauri-apps/api/core";

/** RDP 桥接启动结果。 */
export interface RdpBridgeInfo {
  /** 桥接实例 id（前端 tab 标识）。 */
  instanceId: string;
  /** 前端 IronRDP WASM 直连的 WebSocket 地址。 */
  wsUrl: string;
}

/**
 * 启动一个 RDP 桥接（迷你网关，仅监听 127.0.0.1 临时端口，带随机 token）。
 * verifyCert：严格模式校验证书链（系统信任根），默认 false 与官方一致不校验。
 */
export function rdpBridgeStart(
  host: string,
  port: number,
  verifyCert = false,
): Promise<RdpBridgeInfo> {
  return invoke<RdpBridgeInfo>("rdp_bridge_start", { host, port, verifyCert });
}

/** 停止并回收一个 RDP 桥接。 */
export function rdpBridgeStop(instanceId: string): Promise<void> {
  return invoke<void>("rdp_bridge_stop", { instanceId });
}
