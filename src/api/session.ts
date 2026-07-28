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

export function disconnectSession(instanceId: string): Promise<void> {
  return invoke<void>("disconnect_session", { instanceId });
}

export function openSftpForSession(sessionConfigId: string): Promise<string> {
  return invoke<string>("open_sftp_for_session", { sessionConfigId });
}
