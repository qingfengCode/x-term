import { invoke } from "@tauri-apps/api/core";
import type { Group, Session } from "./types";

export function listSessions(): Promise<Session[]> {
  return invoke<Session[]>("list_sessions");
}

export function getSession(id: string): Promise<Session | null> {
  return invoke<Session | null>("get_session", { id });
}

export function saveSession(session: Session): Promise<void> {
  return invoke<void>("save_session", { session });
}

export function deleteSession(id: string): Promise<void> {
  return invoke<void>("delete_session", { id });
}

export function listGroups(): Promise<Group[]> {
  return invoke<Group[]>("list_groups");
}

export function saveGroup(group: Group): Promise<void> {
  return invoke<void>("save_group", { group });
}

export function deleteGroup(id: string): Promise<void> {
  return invoke<void>("delete_group", { id });
}

export function connectSession(sessionConfigId: string): Promise<string> {
  return invoke<string>("connect_session", { sessionConfigId });
}

/**
 * 认证失败后使用手动输入的密码/验证码重试连接（`connect_session` 的手动认证版）。
 *
 * - `password` 非空时忽略会话配置的认证方式（私钥会话也回退），改用该密码认证；
 * - `otp` 为二次认证验证码（口令码/动态口令等），后端会预填进 keyboard-interactive
 *   首个验证码提示，其余提示仍走 `ssh:auth_challenge` 弹窗；
 * - 传空串或 null 时回退使用会话配置已保存的凭据。
 */
export function connectSessionWithManualAuth(
  sessionConfigId: string,
  password: string | null,
  otp: string | null,
): Promise<string> {
  return invoke<string>("connect_session_with_manual_auth", {
    sessionConfigId,
    password,
    otp,
  });
}

export function disconnectSession(instanceId: string): Promise<void> {
  return invoke<void>("disconnect_session", { instanceId });
}

/**
 * 复制一个已连接的 SSH 终端会话：在同一条已认证连接上打开新 channel
 * （PTY + shell），**不重新认证**——二次认证（口令码）服务器上复制通道
 * 无需再次输入验证码。仅支持 SSH；失败时调用方应回退 connectSession。
 */
export function cloneTerminalSession(instanceId: string): Promise<string> {
  return invoke<string>("clone_terminal_session", { instanceId });
}

export function openSftpForSession(sessionConfigId: string): Promise<string> {
  return invoke<string>("open_sftp_for_session", { sessionConfigId });
}

// ---------------------------------------------------------------------------
// SSH 二次认证（keyboard-interactive）
// ---------------------------------------------------------------------------

/** 二次认证挑战中需要用户输入的单个项（与后端 events::SshAuthPrompt 对应）。 */
export interface SshAuthPrompt {
  /** 服务器给出的提示文本（如 "Password: " / "Verification code: "）。 */
  prompt: string;
  /** 是否回显输入。false 表示密码类输入，前端应用 password 输入框。 */
  echo: boolean;
}

/** 二次认证挑战事件 payload（与后端 events::SshAuthChallengeEvent 对应）。 */
export interface SshAuthChallengeEvent {
  /** 挑战 id（回传 sshAuthRespond 用）。 */
  challengeId: string;
  /** 会话配置 id。 */
  sessionConfigId: string;
  host: string;
  port: number;
  username: string;
  /** 服务器返回的挑战名称。 */
  name: string;
  /** 服务器返回的说明文字（可为空）。 */
  instructions: string;
  /** 需要用户填写的输入项。 */
  prompts: SshAuthPrompt[];
}

/**
 * 回复 SSH 二次认证挑战。
 *
 * `responses` 为数组时与事件中 `prompts` 一一对应（提交）；传 `null` 表示取消。
 */
export function sshAuthRespond(
  challengeId: string,
  responses: string[] | null
): Promise<void> {
  return invoke<void>("ssh_auth_respond", { challengeId, responses });
}

/** SSH 主机公钥变更确认事件 payload（与后端 events::SshHostKeyEvent 对应）。 */
export interface SshHostKeyEvent {
  /** 挑战 id（回传 sshHostKeyRespond 用）。 */
  challengeId: string;
  host: string;
  port: number;
  /** 服务器实际公钥的算法名（如 "ssh-ed25519"）。 */
  keyType: string;
  /** 服务器实际公钥指纹（SHA-256 base64）。 */
  fingerprint: string;
  /** known_hosts 中记录的旧指纹（用于新旧对比）。 */
  knownFingerprint: string;
}

/** 主机公钥变更决策（与后端 ssh::client::HostKeyDecision 对应）。 */
export type HostKeyDecision = "AcceptAndUpdate" | "AcceptOnce" | "Reject";

/** 回复 SSH 主机公钥变更确认。 */
export function sshHostKeyRespond(
  challengeId: string,
  decision: HostKeyDecision
): Promise<void> {
  return invoke<void>("ssh_host_key_respond", { challengeId, decision });
}
