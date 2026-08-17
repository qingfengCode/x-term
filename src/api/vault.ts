import { invoke } from "@tauri-apps/api/core";

export interface CredentialInput {
  id?: string;
  name: string;
  kind: string; // "password" | "private_key_text"
  value: string;
  passphrase?: string;
}

export function vaultExists(): Promise<boolean> {
  return invoke<boolean>("vault_exists");
}

export function vaultCreate(passphrase: string): Promise<void> {
  return invoke<void>("vault_create", { passphrase });
}

export function vaultUnlock(passphrase: string): Promise<void> {
  return invoke<void>("vault_unlock", { passphrase });
}

export function vaultUnlocked(): Promise<boolean> {
  return invoke<boolean>("vault_unlocked");
}

/** 锁定保险库：清除内存主密钥，之后需重新输入主密码解锁。
 *  不断开已建立的连接（温和锁定）。 */
export function vaultLock(): Promise<void> {
  return invoke<void>("vault_lock");
}

export function credentialSave(input: CredentialInput): Promise<string> {
  return invoke<string>("credential_save", { input });
}

export function credentialGet(id: string): Promise<string> {
  return invoke<string>("credential_get", { id });
}

export function credentialDelete(id: string): Promise<void> {
  return invoke<void>("credential_delete", { id });
}

/** 凭据列表项（不含明文）。 */
export interface CredentialView {
  id: string;
  name: string;
  kind: string; // "password" | "private_key_text"
  createdAt: string;
}

export function credentialList(): Promise<CredentialView[]> {
  return invoke<CredentialView[]>("credential_list");
}

export function credentialRename(id: string, name: string): Promise<void> {
  return invoke<void>("credential_rename", { id, name });
}

// ---------------------------------------------------------------------------
// SSH 密钥对生成
// ---------------------------------------------------------------------------

/** 密钥对生成请求。 */
export interface SshKeyGenerateInput {
  name: string; // 凭据名称（同时用作公钥注释）
  algorithm: "ed25519" | "rsa";
  bits?: number; // 仅 RSA 有效：2048 / 3072 / 4096，默认 3072
  passphrase?: string; // 可选：私钥口令
}

/** 生成结果（不含私钥明文，私钥已加密存入保险库）。 */
export interface GeneratedKeyInfo {
  id: string; // 凭据 id
  publicKey: string; // "ssh-ed25519 AAAA... <名称>"
  fingerprint: string; // "SHA256:xxx"
}

/** 生成 SSH 密钥对并存入凭据保险库。 */
export function sshKeyGenerate(input: SshKeyGenerateInput): Promise<GeneratedKeyInfo> {
  return invoke<GeneratedKeyInfo>("ssh_key_generate", { input });
}
