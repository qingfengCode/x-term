import { invoke } from "@tauri-apps/api/core";
import type { Settings } from "./types";

export function settingsLoad(): Promise<Settings> {
  return invoke<Settings>("settings_load");
}

export function settingsSave(settings: Settings): Promise<void> {
  return invoke<void>("settings_save", { settings });
}
