// TOTP/MFA 验证码管理 API。

import { invoke } from "@tauri-apps/api/core";

/** 一条 TOTP 条目（不含 secret 明文）。 */
export interface TotpEntry {
  id: string;
  issuer: string;
  account: string;
  algorithm: string; // "SHA1" | "SHA256" | "SHA512"
  digits: number; // 通常 6，也有 8
  period: number; // 通常 30
  sortOrder: number;
  createdAt: string;
}

/** 实时生成的验证码 + 剩余时间。 */
export interface TotpCode {
  code: string;
  remainingSeconds: number;
  period: number;
}

/** 添加条目参数。secret 可以是 base32 串或完整的 otpauth:// URI。 */
export interface TotpAddInput {
  issuer: string;
  account: string;
  secret: string;
  algorithm?: string;
  digits?: number;
  period?: number;
}

export function totpList(): Promise<TotpEntry[]> {
  return invoke<TotpEntry[]>("totp_list");
}

export function totpAdd(input: TotpAddInput): Promise<TotpEntry> {
  return invoke<TotpEntry>("totp_add", { input });
}

export function totpDelete(id: string): Promise<void> {
  return invoke<void>("totp_delete", { id });
}

export function totpGenerate(id: string): Promise<TotpCode> {
  return invoke<TotpCode>("totp_generate", { id });
}

/** 临时生成（不存库）——添加对话框预览用。 */
export function totpGenerateForSecret(
  secret: string,
  algorithm: string,
  digits: number,
  period: number
): Promise<TotpCode> {
  return invoke<TotpCode>("totp_generate_for_secret", {
    secret,
    algorithm,
    digits,
    period,
  });
}

/** 生成当前码并填充到指定终端实例（不追加换行）。 */
export function totpFillTerminal(id: string, instanceId: string): Promise<void> {
  return invoke<void>("totp_fill_terminal", { id, instanceId });
}
