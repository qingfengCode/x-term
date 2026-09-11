import { invoke } from "@tauri-apps/api/core";

// 与后端 events.rs / monitor.rs 的 payload 对应（camelCase）。

export interface MonitorHost {
  hostname: string;
  kernel: string;
  os: string;
}

export interface MonitorCpu {
  usagePct?: number | null;
  cores: number;
}

export interface MonitorMem {
  totalKb: number;
  availableKb: number;
  usedKb: number;
  usedPct: number;
  swapTotalKb: number;
  swapFreeKb: number;
  swapUsedPct?: number | null;
}

export interface MonitorLoad {
  m1: number;
  m5: number;
  m15: number;
}

export interface MonitorNet {
  rxKbps?: number | null;
  txKbps?: number | null;
  rxTotal: number;
  txTotal: number;
}

export interface MonitorDisk {
  fs: string;
  mount: string;
  totalKb: number;
  usedKb: number;
  availKb: number;
  usedPct: number;
}

export interface MonitorProcess {
  user: string;
  pid: number;
  cpuPct: number;
  memPct: number;
  comm: string;
}

/** monitor:data 事件载荷（一帧）。 */
export interface MonitorDataEvent {
  monitorId: string;
  ts: number;
  host?: MonitorHost | null;
  cpu: MonitorCpu;
  mem: MonitorMem;
  load?: MonitorLoad | null;
  uptimeSecs: number;
  net: MonitorNet;
  disks: MonitorDisk[];
  processes: MonitorProcess[];
}

/** monitor:closed 事件载荷。 */
export interface MonitorClosedEvent {
  monitorId: string;
  reason: string;
}

/** 启动服务器监控，返回 monitorId（事件按它过滤）。 */
export function monitorStart(sessionConfigId: string): Promise<string> {
  return invoke<string>("monitor_start", { sessionConfigId });
}

/** 停止服务器监控。 */
export function monitorStop(monitorId: string): Promise<void> {
  return invoke<void>("monitor_stop", { monitorId });
}
