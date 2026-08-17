import { invoke } from "@tauri-apps/api/core";

/** 本机可用的 shell 检测结果。 */
export interface LocalShellInfo {
  /** shell 标识："cmd" | "powershell" | "git-bash"。 */
  id: string;
  /** 展示名。 */
  label: string;
  /** 本机是否可用。 */
  available: boolean;
}

/** 打开一个本地终端标签页，返回终端实例 id（前端 tab 标识）。 */
export function connectLocalTerminal(shell?: string): Promise<string> {
  return invoke<string>("connect_local_terminal", { shell: shell ?? null });
}

/** 列出本机可用的本地 shell（不可用的项前端隐藏）。 */
export function localTerminalShells(): Promise<LocalShellInfo[]> {
  return invoke<LocalShellInfo[]>("local_terminal_shells");
}
