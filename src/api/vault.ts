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
