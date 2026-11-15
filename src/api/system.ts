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

/**
 * 用系统默认浏览器打开外部 URL。
 *
 * 前端所有可点击链接都必须走这里：Tauri webview 里 `<a href>` 的默认行为
 * 是在应用内导航（整个窗口被外部网页替换，未保存状态全丢）。后端只放行
 * http/https/mailto。
 */
export function openExternalUrl(url: string): Promise<void> {
  return invoke<void>("open_external_url", { url });
}
