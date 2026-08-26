import { invoke } from "@tauri-apps/api/core";

export function terminalWrite(instanceId: string, data: string): Promise<void> {
  return invoke<void>("terminal_write", { instanceId, data });
}

export function terminalResize(instanceId: string, cols: number, rows: number): Promise<void> {
  return invoke<void>("terminal_resize", { instanceId, cols, rows });
}

/** terminal:data 事件载荷（data 为 base64 字节；[startTotal,total) 为半开区间，
 *  批量 emit 时一段数据可能含多个 TCP 块，按该区间对快照基线做精确去重）。 */
export interface TerminalDataPayload {
  sessionId: string;
  data: string;
  /** data 首字节对应的累计输出字节数（区间起点；旧事件无此字段为 0）。 */
  startTotal?: number;
  /** data 末字节追加后的累计输出字节数（区间终点，单调递增）。 */
  total?: number;
}

/** terminal_attach 返回：缓冲原始字节（base64）+ 累计字节基线。 */
export interface TerminalAttachResult {
  data: string;
  total: number;
}

/**
 * 取终端输出缓冲快照（attach 回放）。
 *
 * 后端 reader 在 connect 返回前就开始推 terminal:data，前端监听注册之前的
 * 输出会丢（首屏空白）。挂载时先注册监听并缓存事件，再调用本命令取快照，
 * 按 total 基线去重后合并渲染——既不丢字节也不重复。
 */
export function terminalAttach(instanceId: string): Promise<TerminalAttachResult> {
  return invoke<TerminalAttachResult>("terminal_attach", { instanceId });
}
