import { invoke } from "@tauri-apps/api/core";
import type { UpdateInfo, UpdateManifest } from "./types";

/** 关于页应用信息（当前版本 / 更新源 / 数据目录 / Tauri 版本）。 */
export function updateGetInfo(): Promise<UpdateInfo> {
  return invoke<UpdateInfo>("update_get_info");
}

/** 读取更新源地址。 */
export function updateGetManifestUrl(): Promise<string> {
  return invoke<string>("update_get_manifest_url");
}

/** 保存更新源地址。 */
export function updateSetManifestUrl(url: string): Promise<void> {
  return invoke<void>("update_set_manifest_url", { url });
}

/** 检查更新：返回可用清单，已是最新时为 null。 */
export function updateCheck(): Promise<UpdateManifest | null> {
  return invoke<UpdateManifest | null>("update_check");
}

/** 下载安装包，返回落地路径。进度经 update:progress 事件推送。 */
export function updateDownload(manifest: UpdateManifest): Promise<string> {
  return invoke<string>("update_download", { manifest });
}

/** 拉起安装器并退出应用（不可逆）。 */
export function updateInstallAndExit(path: string): Promise<void> {
  return invoke<void>("update_install_and_exit", { path });
}
