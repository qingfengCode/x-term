import { invoke } from "@tauri-apps/api/core";
import type { ChatMessage } from "./types";

export interface AiChatRequest {
  requestId: string;
  messages: ChatMessage[];
  /** 启用智能体模式（工具调用）。前端 agent 模式时传 true。 */
  agentMode?: boolean;
  /** 当前活动终端 instanceId（工具上下文，可选）。 */
  activeTerminalId?: string;
  /** 当前活动 MySQL 连接 id（可选）。 */
  activeDbConnId?: string;
  /** 请求所属助手域："ssh"（终端助手）| "db"（数据库助手）。文件工具据此取工作目录。 */
  domain?: string;
}

export function aiChat(req: AiChatRequest): Promise<void> {
  return invoke<void>("ai_chat", { req });
}

/** 终止正在进行的 AI 请求。 */
export function aiStop(requestId: string): Promise<void> {
  return invoke<void>("ai_stop", { requestId });
}

/** 设置某个助手域的工作目录（传空串清除）。 */
export function setWorkspaceDir(domain: string, path: string): Promise<void> {
  return invoke<void>("set_workspace_dir", { domain, path });
}

// ---------------------------------------------------------------------------
// 对话历史持久化（独立 JSON 文件，按 domain 分文件）
// ---------------------------------------------------------------------------

/** 可序列化的对话（持久化用）。只保留 id/title/messages。 */
export interface SerializableConversation {
  id: string;
  title: string;
  /** messages 原样透传（结构同 AiMessage，后端不解释）。 */
  messages: unknown[];
}

/** 读取指定 domain（"ssh" / "db"）的对话历史。 */
export function aiListConversations(domain: string): Promise<SerializableConversation[]> {
  return invoke<SerializableConversation[]>("ai_list_conversations", { domain });
}

/** 全量保存指定 domain 的对话历史（覆盖旧文件）。 */
export function aiSaveConversations(
  domain: string,
  conversations: SerializableConversation[],
): Promise<void> {
  return invoke<void>("ai_save_conversations", { domain, conversations });
}
