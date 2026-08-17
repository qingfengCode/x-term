import { invoke } from "@tauri-apps/api/core";

/** 桌面连接（RDP/VNC），独立于终端 sessions。 */
export interface Desktop {
  id: string;
  name: string;
  protocol: "rdp" | "vnc";
  host: string;
  port: number;
  username: string | null;
  credentialId: string | null;
  /** 所属桌面分组（null = 未分组）。 */
  groupId: string | null;
  /** 上次使用的 RDP 分辨率（"宽x高"，null = 未记忆）。 */
  desktopSize: string | null;
  sortOrder: number;
  createdAt: string;
  updatedAt: string;
}

/** 桌面分组（与终端会话分组完全独立）。 */
export interface DesktopGroup {
  id: string;
  name: string;
  parentId: string | null;
  sortOrder: number;
  createdAt: string;
}

/** 远程桌面启动参数。 */
export interface RemoteDesktopParams {
  protocol: "rdp" | "vnc";
  host: string;
  port: number;
  username?: string;
  password?: string;
}

// --- CRUD ---

export function desktopList(): Promise<Desktop[]> {
  return invoke<Desktop[]>("desktop_list");
}

export function desktopSave(desktop: Desktop): Promise<void> {
  return invoke<void>("desktop_save", { desktop });
}

export function desktopDelete(id: string): Promise<void> {
  return invoke<void>("desktop_delete", { id });
}

/** 记录断开时最后一次使用的 RDP 分辨率（"宽x高"），重连时恢复。 */
export function desktopSaveSize(id: string, size: string | null): Promise<void> {
  return invoke<void>("desktop_save_size", { id, size });
}

// --- 分组 CRUD ---

export function desktopGroupList(): Promise<DesktopGroup[]> {
  return invoke<DesktopGroup[]>("desktop_group_list");
}

export function desktopGroupSave(group: DesktopGroup): Promise<void> {
  return invoke<void>("desktop_group_save", { group });
}

export function desktopGroupDelete(id: string): Promise<void> {
  return invoke<void>("desktop_group_delete", { id });
}

// --- 启动客户端 ---

/** 启动本地桌面客户端连接目标。 */
export function remoteDesktopLaunch(params: RemoteDesktopParams): Promise<string> {
  return invoke<string>("remote_desktop_launch", { ...params });
}
