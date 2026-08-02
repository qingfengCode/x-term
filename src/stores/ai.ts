import { defineStore } from "pinia";
import { computed, ref } from "vue";
import * as aiApi from "@/api/ai";
import * as dbApi from "@/api/db";
import type { ChatMessage, ChatRole, ToolCall, ToolResult } from "@/api/types";

/**
 * AI 对话状态。
 *
 * 一条"对话"由若干消息组成。发起请求后，流式片段通过 ai:chunk 事件累积到
 * 当前进行中的助手消息上；ai:done 时标记完成；ai:error 时记录错误。
 *
 * 智能体（agent）模式下，模型会通过 ai:tool_call 事件请求执行工具（操作 SSH/SQL），
 * 此时该工具调用进入 pendingToolCalls 待用户确认；用户点击执行/拒绝后，
 * 通过 ai:tool_result 事件回填结果。
 */

/** 对话中的一个工具调用项（用于在消息流中渲染卡片）。 */
export interface ToolCallItem {
  toolCallId: string;
  name: string;
  arguments: Record<string, unknown>;
  description: string;
  dangerous: boolean;
  /** exec_ssh 命令是否落在白名单内（前端据此显示绿色卡片 + 默认放行 UX）。 */
  whitelisted: boolean;
  /** 是否已被自动放行（白名单模式 + 命中白名单）。true 时卡片直接显示"已自动执行"终态。 */
  autoApproved: boolean;
  /** pending=待确认；approved=已执行；rejected=已拒绝；done=已有结果。 */
  status: "pending" | "approved" | "rejected" | "done";
  result?: ToolResult;
}

/**
 * 助手消息的有序片段。
 *
 * 一条助手消息可能由若干「文本段」和「工具调用」交替组成（多轮智能体场景：
 * 说要做X → 调工具 → 总结 → 再调工具 → 最终总结）。用 `parts` 按事件到达顺序记录，
 * 模板据此渲染，使工具卡片出现在正文中间的实际位置，而非全堆在顶部。
 *
 * `tool` 片段的 `item` 直接引用 `toolCalls` 数组里的同一个 `ToolCallItem` 对象
 * （JS 引用共享），所以批准/拒绝/结果等状态更新照旧改 `toolCalls` 里的对象即可，
 * `parts` 里的卡片会自动同步——不存在两份状态不同步的风险。
 */
export type AiMessagePart =
  | { kind: "text"; text: string }
  | { kind: "tool"; item: ToolCallItem };

export interface AiMessage {
  id: string;
  role: ChatRole;
  content: string;
  toolCallId?: string;
  /** 是否正在流式接收中。 */
  streaming: boolean;
  error?: string;
  /** 该助手消息产生的工具调用（用于在气泡内渲染卡片）。 */
  toolCalls?: ToolCallItem[];
  /**
   * 助手消息的有序片段（文本 + 工具调用）。仅助手消息使用；模板渲染的唯一事实来源。
   * `content` / `toolCalls` 字段保留供历史回放与状态扫描使用，与此字段并行维护。
   */
  parts?: AiMessagePart[];
}

/**
 * AI 助手 store 工厂。
 *
 * 拆分为两个完全隔离的助手（各自独立的对话列表 / 多会话 / requestToCid 路由表）：
 * - `useAiSshStore`（id "ai:ssh"）：终端助手（终端页）
 * - `useAiDbStore`（id "ai:db"）：SQL 页数据库助手
 *
 * 事件路由：MainLayout 把 `ai:*` 事件同时分发给两个 store，每个 store 的
 * `convForRequest(requestId)` 只会在自己的 `requestToCid` 里命中——对方 store
 * 找不到该 requestId 即静默 return，天然实现隔离，无需给事件加 domain 字段。
 */
const makeAiStore = (id: string) =>
  defineStore(id, () => {
  // --- 多会话状态 ----------------------------------------------------------
  // 每个对话独立持有 messages / activeRequestId / sending。事件通过 requestId
  // 在 requestToCid 映射中定位所属会话，从而支持多个对话并发收发。
  interface Conversation {
    id: string;
    title: string;
    messages: AiMessage[];
    /** 该会话当前在途的请求 id（用于把流式事件路由回来）。 */
    activeRequestId: string | null;
    sending: boolean;
  }

  function genId() {
    return `m-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  }
  function genCid() {
    return `c-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  }

  const conversations = ref<Conversation[]>([
    // 初始即有一个"新对话"，避免 UI 还没调用 ensureConversation 时标签栏为空。
    {
      id: `c-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      title: "新对话",
      messages: [],
      activeRequestId: null,
      sending: false,
    },
  ]);
  const activeCid = ref<string | null>(conversations.value[0].id);
  /** requestId → cid 索引，事件路由用。 */
  const requestToCid = new Map<string, string>();

  /** 启动时确保至少有一个对话。 */
  function ensureConversation() {
    if (conversations.value.length === 0) {
      conversations.value.push({
        id: genCid(),
        title: "新对话",
        messages: [],
        activeRequestId: null,
        sending: false,
      });
    }
    if (!activeCid.value) activeCid.value = conversations.value[0].id;
  }

  /** 当前活动会话对象。 */
  const activeConversation = computed<Conversation | null>(() => {
    if (!activeCid.value) return null;
    return conversations.value.find((c) => c.id === activeCid.value) ?? null;
  });

  // --- 兼容计算属性：代理到活动会话，减少调用方改动 ---
  const messages = computed<AiMessage[]>({
    get: () => activeConversation.value?.messages ?? [],
    set: (v) => {
      if (activeConversation.value) activeConversation.value.messages = v;
    },
  });
  const sending = computed(() => activeConversation.value?.sending ?? false);
  const activeRequestId = computed(() => activeConversation.value?.activeRequestId ?? null);

  function createConversation(): string {
    const c: Conversation = {
      id: genCid(),
      title: "新对话",
      messages: [],
      activeRequestId: null,
      sending: false,
    };
    conversations.value.push(c);
    activeCid.value = c.id;
    return c.id;
  }

  function switchConversation(cid: string) {
    if (conversations.value.some((c) => c.id === cid)) activeCid.value = cid;
  }

  function closeConversation(cid: string) {
    const idx = conversations.value.findIndex((c) => c.id === cid);
    if (idx < 0) return;
    conversations.value.splice(idx, 1);
    if (activeCid.value === cid) {
      activeCid.value = conversations.value[0]?.id ?? null;
      if (!activeCid.value) ensureConversation();
    }
  }

  /**
   * 发送一条用户消息并启动 AI 流式回复。
   * @param userText 用户输入
   * @param systemPrompt 系统提示词
   * @param opts.agent 是否启用工具调用（智能体模式）
   * @param opts.activeTerminalId 当前活动终端（agent 模式上下文）
   * @param opts.activeDbConnId 当前活动 MySQL 连接
   * @param opts.domain 请求所属助手域（"ssh" | "db"），文件工具据此取工作目录
   */
  async function send(
    userText: string,
    systemPrompt?: string,
    opts?: {
      agent?: boolean;
      activeTerminalId?: string;
      activeDbConnId?: string;
      domain?: string;
    }
  ) {
    ensureConversation();
    const conv = activeConversation.value!;
    if (conv.sending) return; // 按会话粒度互斥，不同会话可并发
    conv.sending = true;

    const userMsg: AiMessage = {
      id: genId(),
      role: "user",
      content: userText,
      streaming: false,
    };
    conv.messages.push(userMsg);
    // 用首条用户消息作为对话标题（取前 20 字）。
    if (conv.title === "新对话") {
      conv.title = userText.slice(0, 20) + (userText.length > 20 ? "…" : "");
    }

    const requestId = `r-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    conv.activeRequestId = requestId;
    requestToCid.set(requestId, conv.id);

    const assistantMsg: AiMessage = {
      id: genId(),
      role: "assistant",
      content: "",
      streaming: true,
      parts: [],
    };
    conv.messages.push(assistantMsg);

    const history: ChatMessage[] = [];
    if (systemPrompt) history.push({ role: "system", content: systemPrompt });
    for (const m of conv.messages) {
      if (m.id === assistantMsg.id) break;
      history.push({
        role: m.role,
        content: m.content,
        toolCalls: m.toolCalls?.map((t) => ({
          id: t.toolCallId,
          name: t.name,
          arguments: t.arguments,
        })),
        toolCallId: m.toolCallId,
      });
      // 关键：当 assistant 消息带 tool_calls 时，OpenAI/Anthropic 协议要求
      // 后面必须紧跟每个 tool_call_id 对应的 role=tool 结果消息，否则 400。
      // 把每个工具调用的结果（或拒绝原因）作为独立 tool 消息补上。
      if (m.role === "assistant" && m.toolCalls && m.toolCalls.length > 0) {
        for (const t of m.toolCalls) {
          const output = t.result?.output ?? "(用户未执行该操作)";
          history.push({
            role: "tool",
            content: output,
            toolCallId: t.toolCallId,
          });
        }
      }
    }

    try {
      await aiApi.aiChat({
        requestId,
        messages: history,
        agentMode: opts?.agent ?? false,
        activeTerminalId: opts?.activeTerminalId,
        activeDbConnId: opts?.activeDbConnId,
        domain: opts?.domain,
      });
    } catch (e) {
      assistantMsg.streaming = false;
      assistantMsg.error = String(e);
      conv.sending = false;
      conv.activeRequestId = null;
      requestToCid.delete(requestId);
    }
  }

  /** 根据 requestId 定位会话；找不到返回 null。 */
  function convForRequest(requestId: string): Conversation | null {
    const cid = requestToCid.get(requestId);
    if (!cid) return null;
    return conversations.value.find((c) => c.id === cid) ?? null;
  }

  /** 找到该会话当前最后一条 assistant 消息（流式目标）。 */
  function lastAssistant(conv: Conversation): AiMessage | null {
    const m = conv.messages[conv.messages.length - 1];
    return m && m.role === "assistant" ? m : null;
  }

  function onChunk(requestId: string, delta: string) {
    const conv = convForRequest(requestId);
    if (!conv) return;
    const m = lastAssistant(conv);
    if (!m) return;
    m.content += delta;
    // 同步追加进有序 parts：若末尾是文本段就续上，否则新建一段（与上一个工具调用分隔）。
    const parts = (m.parts ??= []);
    const last = parts[parts.length - 1];
    if (last && last.kind === "text") {
      last.text += delta;
    } else {
      parts.push({ kind: "text", text: delta });
    }
  }

  function onDone(requestId: string) {
    const conv = convForRequest(requestId);
    if (!conv) return;
    const m = lastAssistant(conv);
    if (m) m.streaming = false;
    conv.sending = false;
    conv.activeRequestId = null;
    requestToCid.delete(requestId);
  }

  function onError(requestId: string, message: string) {
    const conv = convForRequest(requestId);
    if (!conv) return;
    const m = lastAssistant(conv);
    if (m) {
      m.streaming = false;
      m.error = message;
    }
    conv.sending = false;
    conv.activeRequestId = null;
    requestToCid.delete(requestId);
  }

  /** AI 请求被用户终止（后端 ai:stopped 事件）。 */
  function onStopped(requestId: string) {
    const conv = convForRequest(requestId);
    if (!conv) return;
    const m = lastAssistant(conv);
    if (m) {
      m.streaming = false;
      // 若助手消息已有部分内容，保留并追加终止标记；否则置一个提示。
      if (m.content.trim()) {
        m.content += "\n\n_（已终止）_";
      } else if (!m.error) {
        m.content = "_（已终止）_";
      }
    }
    // 取消该会话所有待确认的工具调用卡片（pending → rejected）。
    for (const msg of conv.messages) {
      if (msg.toolCalls) {
        for (const t of msg.toolCalls) {
          if (t.status === "pending") t.status = "rejected";
        }
      }
    }
    conv.sending = false;
    conv.activeRequestId = null;
    requestToCid.delete(requestId);
  }

  /** 用户点击"终止"按钮：调用后端 ai_stop。 */
  async function stop() {
    const conv = activeConversation.value;
    const rid = conv?.activeRequestId;
    if (!rid) return;
    try {
      await aiApi.aiStop(rid);
    } catch {
      /* 即使后端报错也本地收尾 */
    }
    // 本地立即收尾（不等 ai:stopped 事件，避免按钮卡顿）。
    onStopped(rid);
  }

  /** 收到一个工具调用请求（前端展示确认卡片）。 */
  function onToolCall(
    requestId: string,
    payload: {
      toolCallId: string;
      name: string;
      arguments: string;
      description: string;
      dangerous: boolean;
      whitelisted: boolean;
      autoApproved: boolean;
    }
  ) {
    const conv = convForRequest(requestId);
    if (!conv) return;
    const m = lastAssistant(conv);
    if (!m) return;
    let parsed: Record<string, unknown> = {};
    try {
      parsed = JSON.parse(payload.arguments);
    } catch {
      parsed = { _raw: payload.arguments };
    }
    const item: ToolCallItem = {
      toolCallId: payload.toolCallId,
      name: payload.name,
      arguments: parsed,
      description: payload.description,
      dangerous: payload.dangerous,
      whitelisted: payload.whitelisted ?? false,
      // 自动放行的 tool_call：后端已直接执行，前端直接显示"已自动执行"终态，
      // 后续 onToolResult 会回填结果。
      autoApproved: payload.autoApproved ?? false,
      status: payload.autoApproved ? "approved" : "pending",
    };
    if (!m.toolCalls) m.toolCalls = [];
    m.toolCalls.push(item);
    // 按到达顺序记录到 parts（item 引用共享，卡片状态随 toolCalls 自动同步）。
    (m.parts ??= []).push({ kind: "tool", item });
  }

  /** 工具执行结果回填（更新对应卡片状态）。 */
  function onToolResult(
    requestId: string,
    payload: { toolCallId: string; ok: boolean; output: string }
  ) {
    const conv = convForRequest(requestId);
    if (!conv) return;
    const m = lastAssistant(conv);
    if (!m || !m.toolCalls) return;
    const item = m.toolCalls.find((t) => t.toolCallId === payload.toolCallId);
    if (item) {
      item.status = "done";
      item.result = { ok: payload.ok, output: payload.output };
    }
  }

  /** 用户点击"执行"。本地立即更新卡片状态为 approved，并通知后端。 */
  async function approveToolCall(toolCallId: string) {
    updateToolCallStatus(toolCallId, "approved");
    await dbApi.aiExecuteTool(toolCallId).catch(() => {
      /* ignore */
    });
  }

  /** 用户点击"加入白名单并执行"：先把命令前缀加入白名单（持久化），再正常 approve。
   * 仅对 exec_ssh 工具有意义；其它工具直接 approve。 */
  async function addToWhitelistAndApprove(toolCallId: string) {
    // 找到该工具调用的命令参数。
    const cmd = findCommandByToolCallId(toolCallId);
    if (cmd) {
      await dbApi.aiAddToWhitelist(cmd).catch(() => {
        /* ignore — 即使加白名单失败也继续 approve */
      });
    }
    await approveToolCall(toolCallId);
  }

  /** 在所有会话中按 toolCallId 找到 exec_ssh 的 command 参数。 */
  function findCommandByToolCallId(toolCallId: string): string | null {
    for (const conv of conversations.value) {
      for (const m of conv.messages) {
        if (!m.toolCalls) continue;
        const item = m.toolCalls.find((t) => t.toolCallId === toolCallId);
        if (item && typeof item.arguments.command === "string") {
          return item.arguments.command;
        }
      }
    }
    return null;
  }

  /** 用户点击"拒绝"。 */
  async function rejectToolCall(toolCallId: string) {
    updateToolCallStatus(toolCallId, "rejected");
    await dbApi.aiCancelTool(toolCallId).catch(() => {
      /* ignore */
    });
  }

  function updateToolCallStatus(toolCallId: string, status: ToolCallItem["status"]) {
    // 工具调用可能位于任意会话（用户可能在另一标签确认），扫描全部。
    for (const conv of conversations.value) {
      for (const m of conv.messages) {
        if (!m.toolCalls) continue;
        const item = m.toolCalls.find((t) => t.toolCallId === toolCallId);
        if (item) {
          item.status = status;
          return;
        }
      }
    }
  }

  /** 清空当前活动会话的消息。 */
  function clear() {
    const conv = activeConversation.value;
    if (conv) {
      conv.messages = [];
      conv.title = "新对话";
    }
  }

  /**
   * 重命名对话（手动覆盖自动标题）。
   * 由于 send 自动标题仅在 title==="新对话" 时触发，手动重命名后不会被覆盖。
   */
  function renameConversation(cid: string, title: string) {
    const conv = conversations.value.find((c) => c.id === cid);
    if (conv) conv.title = title.trim() || "新对话";
  }

  /**
   * 重新生成：找到 messageId 之前最近一条 user 消息，删掉该 user 消息之后的所有内容，
   * 然后基于这条 user 消息重新 send。仅对 assistant 消息有意义。
   * @param messageId 要重生的 assistant 消息 id
   * @param systemPrompt 重发用的系统提示词
   * @param opts 透传给 send 的选项（agent 模式等）
   */
  async function regenerate(
    messageId: string,
    systemPrompt?: string,
    opts?: {
      agent?: boolean;
      activeTerminalId?: string;
      activeDbConnId?: string;
      domain?: string;
    },
  ) {
    const conv = activeConversation.value;
    if (!conv || conv.sending) return;
    // 找到该消息在当前会话的位置。
    const idx = conv.messages.findIndex((m) => m.id === messageId);
    if (idx < 0) return;
    // 往前找最近的 user 消息。
    let userIdx = -1;
    for (let i = idx; i >= 0; i--) {
      if (conv.messages[i].role === "user") {
        userIdx = i;
        break;
      }
    }
    if (userIdx < 0) return;
    const userText = conv.messages[userIdx].content;
    // 删掉该 user 消息及之后的所有消息。
    conv.messages.splice(userIdx);
    // 重新发送。
    await send(userText, systemPrompt, opts);
  }

  return {
    // 多会话状态
    conversations,
    activeCid,
    activeConversation,
    createConversation,
    switchConversation,
    closeConversation,
    // 兼容（代理到活动会话）
    messages,
    sending,
    activeRequestId,
    send,
    onChunk,
    onDone,
    onError,
    onStopped,
    stop,
    onToolCall,
    onToolResult,
    approveToolCall,
    addToWhitelistAndApprove,
    rejectToolCall,
    clear,
    renameConversation,
    regenerate,
  };
});

export const useAiSshStore = makeAiStore("ai:ssh");
export const useAiDbStore = makeAiStore("ai:db");
