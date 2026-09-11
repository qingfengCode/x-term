import { invoke } from "@tauri-apps/api/core";

/**
 * 向终端实例写入数据（base64 字节流）。
 *
 * @param raw true = 原样写入（ZMODEM 协议/文件二进制的旁路，不做编码转换）；
 *            默认 false = 键盘文本输入，后端按设置的终端编码转码（GBK 等）。
 */
export function terminalWrite(instanceId: string, data: string, raw = false): Promise<void> {
  return invoke<void>("terminal_write", { instanceId, data, raw });
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

/**
 * ZMODEM 专用：无父窗口的原生多选文件框（rz 上传）。
 *
 * 与 tauri-plugin-dialog 的 JS API 不同，后端不设置 owner——带 owner 的
 * 模态对话框会禁用主窗口，期间 WebView2 会把进程光标置为隐藏（弹窗内
 * 鼠标不可见但可点击）。取消时返回空数组。
 */
export function zmodemPickFiles(title: string): Promise<string[]> {
  return invoke<string[]>("zmodem_pick_files", { title });
}

/**
 * ZMODEM 专用：无父窗口的原生保存文件框（sz 下载）。
 *
 * 返回完整保存路径；取消时返回 null。defaultName 预填文件名。
 */
export function zmodemSaveFile(title: string, defaultName: string): Promise<string | null> {
  return invoke<string | null>("zmodem_save_file", { title, defaultName });
}

/**
 * ZMODEM 专用：无父窗口的原生目录选择框（设置默认下载目录）。
 *
 * 返回选中目录的绝对路径；取消时返回 null。
 */
export function zmodemPickFolder(title: string): Promise<string | null> {
  return invoke<string | null>("zmodem_pick_folder", { title });
}
