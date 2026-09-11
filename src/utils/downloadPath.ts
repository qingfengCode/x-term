// 下载落盘路径解析（终端 sz / SFTP / 对象存储共用）。
// ----------------------------------------------------------------------------
// 「默认下载目录」（settings.terminal.downloadDir）非空且可用时，下载直接落盘到
// 该目录并自动避让同名文件（name → name (1).ext），不再弹保存对话框；未设置或
// 目录已失效（被删除/移动）时返回 null，由调用方回退原生对话框。

import { exists } from "@tauri-apps/plugin-fs";
import { join } from "@tauri-apps/api/path";
import { useSettingsStore } from "@/stores/settings";

/** 配置中填写的默认下载目录（未设置返回空串；不做存在性检查）。 */
export function configuredDownloadDir(): string {
  const settings = useSettingsStore();
  return settings.terminal.downloadDir?.trim() ?? "";
}

/**
 * 可用的默认下载目录：未设置、或目录不存在（含权限异常）时返回 null。
 * 返回 null 时调用方应回退"另存为"对话框。
 */
export async function usableDownloadDir(): Promise<string | null> {
  const dir = configuredDownloadDir();
  if (!dir) return null;
  try {
    return (await exists(dir)) ? dir : null;
  } catch {
    return null;
  }
}

/** 在目录内生成不冲突的路径：name → name (1).ext → name (2).ext … */
export async function uniquePathIn(dir: string, name: string): Promise<string> {
  const dot = name.lastIndexOf(".");
  const base = dot > 0 ? name.slice(0, dot) : name;
  const ext = dot > 0 ? name.slice(dot) : "";
  for (let i = 0; i < 1000; i++) {
    const candidate = i === 0 ? name : `${base} (${i})${ext}`;
    const p = await join(dir, candidate);
    if (!(await exists(p))) return p;
  }
  return join(dir, name);
}
