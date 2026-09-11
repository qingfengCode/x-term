// 系统集成（调用操作系统外壳）。
import { invoke } from "@tauri-apps/api/core";

/**
 * 在系统文件管理器中打开目标所在目录并选中它（Windows 为资源管理器）。
 *
 * 路径不存在时后端返回错误（提示文件可能已被移动/删除）。
 */
export function revealInFolder(path: string): Promise<void> {
  return invoke<void>("reveal_in_folder", { path });
}
