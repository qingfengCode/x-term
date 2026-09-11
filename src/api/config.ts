import { invoke } from "@tauri-apps/api/core";
import type { Settings } from "./types";

export function settingsLoad(): Promise<Settings> {
  return invoke<Settings>("settings_load");
}

export function settingsSave(settings: Settings): Promise<void> {
  return invoke<void>("settings_save", { settings });
}

/** 打开终端输出日志目录（系统文件管理器），返回目录路径。 */
export function openLogsDir(): Promise<string> {
  return invoke<string>("open_logs_dir");
}
