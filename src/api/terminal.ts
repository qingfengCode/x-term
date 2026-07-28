import { invoke } from "@tauri-apps/api/core";

export function terminalWrite(instanceId: string, data: string): Promise<void> {
  return invoke<void>("terminal_write", { instanceId, data });
}

export function terminalResize(instanceId: string, cols: number, rows: number): Promise<void> {
  return invoke<void>("terminal_resize", { instanceId, cols, rows });
}
