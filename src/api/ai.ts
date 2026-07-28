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
}

export function aiChat(req: AiChatRequest): Promise<void> {
  return invoke<void>("ai_chat", { req });
}

/** 终止正在进行的 AI 请求。 */
export function aiStop(requestId: string): Promise<void> {
  return invoke<void>("ai_stop", { requestId });
}
