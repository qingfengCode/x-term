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
  sortOrder: number;
  createdAt: string;
  updatedAt: string;
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

// --- 启动客户端 ---

/** 启动本地桌面客户端连接目标。 */
export function remoteDesktopLaunch(params: RemoteDesktopParams): Promise<string> {
  return invoke<string>("remote_desktop_launch", { ...params });
}
