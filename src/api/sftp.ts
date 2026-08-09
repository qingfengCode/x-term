import { invoke } from "@tauri-apps/api/core";
import type { FileEntry, FileMeta } from "./types";

export function sftpList(sftpId: string, path: string): Promise<FileEntry[]> {
  return invoke<FileEntry[]>("sftp_list", { sftpId, path });
}

export function sftpStat(sftpId: string, path: string): Promise<FileMeta> {
  return invoke<FileMeta>("sftp_stat", { sftpId, path });
}

/** 获取远程会话当前工作目录（绝对路径，后端用 REALPATH 解析 "."）。 */
export function sftpPwd(sftpId: string): Promise<string> {
  return invoke<string>("sftp_pwd", { sftpId });
}

export function sftpMkdir(sftpId: string, path: string): Promise<void> {
  return invoke<void>("sftp_mkdir", { sftpId, path });
}

export function sftpRename(sftpId: string, oldpath: string, newpath: string): Promise<void> {
  return invoke<void>("sftp_rename", { sftpId, oldpath, newpath });
}

export function sftpRemove(sftpId: string, path: string, isDir: boolean): Promise<void> {
  return invoke<void>("sftp_remove", { sftpId, path, isDir });
}

export interface DownloadParams {
  sftpId: string;
  remotePath: string;
  localPath: string;
  taskId: string;
}

export interface UploadParams {
  sftpId: string;
  localPath: string;
  remotePath: string;
  taskId: string;
}

export function sftpDownload(params: DownloadParams): Promise<void> {
  return invoke<void>("sftp_download", { params });
}

export function sftpUpload(params: UploadParams): Promise<void> {
  return invoke<void>("sftp_upload", { params });
}

export function sftpClose(sftpId: string): Promise<void> {
  return invoke<void>("sftp_close", { sftpId });
}
