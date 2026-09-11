import { invoke } from "@tauri-apps/api/core";
import type { ChatMessage } from "./types";

export interface AiChatRequest {
  requestId: string;
  messages: ChatMessage[];
  /** 启用智能体模式（工具调用）。前端 agent 模式时传 true。 */
  agentMode?: boolean;
  /** 当前活动终端 instanceId（工具上下文，可选）。 */
  activeTerminalId?: string;
  /** 当前活动数据库连接 id（MySQL / PostgreSQL，可选）。 */
  activeDbConnId?: string;
  /** 当前活动内嵌 RDP 会话的桥接实例 id（桌面助手用，启用 desktop_* 工具）。 */
  activeDesktopId?: string;
  /** 请求所属助手域："ssh"（终端助手）| "db"（数据库助手）| "desktop"（桌面助手）。文件工具据此取工作目录。 */
  domain?: string;
}

export function aiChat(req: AiChatRequest): Promise<void> {
  return invoke<void>("ai_chat", { req });
}

/** 终止正在进行的 AI 请求。 */
export function aiStop(requestId: string): Promise<void> {
  return invoke<void>("ai_stop", { requestId });
}

/** 终端智能补全：让 AI 把当前输入补全为完整命令（一次性、无对话历史）。 */
export function aiCompleteCommand(input: string): Promise<string> {
  return invoke<string>("ai_complete_command", { input });
}

/** 设置某个助手域的工作目录（传空串清除）。 */
export function setWorkspaceDir(domain: string, path: string): Promise<void> {
  return invoke<void>("set_workspace_dir", { domain, path });
}

// ---------------------------------------------------------------------------
// 对话历史持久化（独立 JSON 文件，按 domain 分文件）
// ---------------------------------------------------------------------------

/** 可序列化的对话（持久化用）。只保留 id/title/messages/todos/usage。 */
export interface SerializableConversation {
  id: string;
  title: string;
  /** messages 原样透传（结构同 AiMessage，后端不解释）。 */
  messages: unknown[];
  /** 智能体任务清单（todo_write 维护；旧文件无此字段）。 */
  todos?: { content: string; status: string }[];
  /** 最近一次模型请求的 token 用量（ai:usage 事件覆盖写入，非累计；
   *  旧文件无此字段，或旧值语义为"累计"——会在下一次请求时被覆盖）。 */
  usage?: { prompt: number; completion: number };
  /** 是否已归档（关闭的会话进历史归档；旧文件无此字段 → false）。 */
  archived?: boolean;
  /** 最后活动时间（毫秒时间戳；旧文件无此字段）。 */
  updatedAt?: number;
  /** 归档时间（毫秒时间戳；仅归档会话有；旧文件无此字段）。 */
  archivedAt?: number;
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
