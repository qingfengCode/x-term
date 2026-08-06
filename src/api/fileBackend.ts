import { invoke } from "@tauri-apps/api/core";
import type { FileEntry, FileMeta } from "./types";

/** 文件后端账号（对应后端 FileAccount）。 */
export interface FileAccount {
  id: string;
  name: string;
  /** 后端种类：当前固定 "s3"。 */
  kind: string;
  endpoint: string;
  region: string;
  bucket: string;
  credentialId: string | null;
  /** 寻址风格：true=path-style（默认），false=virtual-hosted-style。
   *  带端口/路径前缀的自定义 endpoint（MinIO 等）应保持 true。 */
  pathStyle: boolean;
  sortOrder: number;
  createdAt: string;
  updatedAt: string;
}

/** 创建/更新文件账号时的明文凭据输入（前端临时持有，提交后经 vault 加密）。 */
export interface S3CredentialInput {
  accessKey: string;
  secretKey: string;
}

// ---------------------------------------------------------------------------
// 账号 CRUD
// ---------------------------------------------------------------------------

export function fileAccountList(): Promise<FileAccount[]> {
  return invoke<FileAccount[]>("file_account_list");
}

export function fileAccountSave(account: FileAccount): Promise<void> {
  return invoke<void>("file_account_save", { account });
}

export function fileAccountDelete(id: string): Promise<void> {
  return invoke<void>("file_account_delete", { id });
}

// ---------------------------------------------------------------------------
// 连接生命周期
// ---------------------------------------------------------------------------

/** 连接到文件账号，返回 backendId（后续文件操作用它引用）。 */
export function fileConnect(accountId: string): Promise<string> {
  return invoke<string>("file_connect", { accountId });
}

export function fileDisconnect(backendId: string): Promise<void> {
  return invoke<void>("file_disconnect", { backendId });
}

// ---------------------------------------------------------------------------
// 文件操作
// ---------------------------------------------------------------------------

export function fileList(backendId: string, path: string): Promise<FileEntry[]> {
  return invoke<FileEntry[]>("file_list", { backendId, path });
}

export function fileStat(backendId: string, path: string): Promise<FileMeta> {
  return invoke<FileMeta>("file_stat", { backendId, path });
}

export function fileMkdir(backendId: string, path: string): Promise<void> {
  return invoke<void>("file_mkdir", { backendId, path });
}

export function fileRename(
  backendId: string,
  oldpath: string,
  newpath: string,
): Promise<void> {
  return invoke<void>("file_rename", { backendId, oldpath, newpath });
}

export function fileRemove(backendId: string, path: string, isDir: boolean): Promise<void> {
  return invoke<void>("file_remove", { backendId, path, isDir });
}

export interface FileDownloadParams {
  backendId: string;
  remotePath: string;
  localPath: string;
  taskId: string;
}

export interface FileUploadParams {
  backendId: string;
  localPath: string;
  remotePath: string;
  taskId: string;
}

export function fileDownload(params: FileDownloadParams): Promise<void> {
  return invoke<void>("file_download", { params });
}

export function fileUpload(params: FileUploadParams): Promise<void> {
  return invoke<void>("file_upload", { params });
}
