import { invoke } from "@tauri-apps/api/core";

// --- 数据迁移（加密导入/导出）类型，与后端 backup.rs 对应 ---

/** 各数据段条目数。 */
export interface BackupCounts {
  sessions: number;
  groups: number;
  credentials: number;
  dbProfiles: number;
  dbGroups: number;
  forwardRules: number;
  desktops: number;
  totpSecrets: number;
  fileAccounts: number;
  hasSettings: boolean;
  hasMcp: boolean;
}

/** 导入前预览信息（backup_inspect 返回）。 */
export interface BackupInfo {
  version: number;
  appVersion: string;
  createdAt: string;
  counts: BackupCounts;
  hasCredentials: boolean;
  hasTotp: boolean;
}

/** 导出/导入完成后的摘要。 */
export interface BackupSummary {
  path: string;
  counts: BackupCounts;
  /** 导出时因保险库未解锁而跳过的凭据数。 */
  credentialsSkipped: number;
  /** 导出时因保险库未解锁而跳过的 TOTP 数。 */
  totpSkipped: number;
  /** 是否为覆盖导入。 */
  overwritten: boolean;
}

/** 导出全部用户数据为加密备份文件。 */
export function backupExport(path: string, password: string): Promise<BackupSummary> {
  return invoke<BackupSummary>("backup_export", { path, password });
}

/** 解密备份文件返回元数据（导入前预览 / 校验密码），不写入任何数据。 */
export function backupInspect(path: string, password: string): Promise<BackupInfo> {
  return invoke<BackupInfo>("backup_inspect", { path, password });
}

/**
 * 从加密备份文件导入数据。
 * @param mode "merge"（默认，按 id 合并）| "overwrite"（清空后覆盖导入）
 * @param force 覆盖模式且备份不含凭据/TOTP 时，清空本机凭据需显式确认；后端默认拒绝，传 true 放行
 */
export function backupImport(
  path: string,
  password: string,
  mode: "merge" | "overwrite",
  force = false,
): Promise<BackupSummary> {
  return invoke<BackupSummary>("backup_import", { path, password, mode, force });
}
