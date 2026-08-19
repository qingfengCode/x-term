import { defineStore } from "pinia";
import { computed, ref } from "vue";
import * as aiApi from "@/api/ai";
import * as dbApi from "@/api/db";
import type { AskUserAnswer } from "@/api/db";
import { useSettingsStore } from "@/stores/settings";
import type { ChatMessage, ChatRole, ImagePart, ToolCall, ToolResult } from "@/api/types";

/**
 * AI 对话状态。
 *
 * 一条"对话"由若干消息组成。发起请求后，流式片段通过 ai:chunk 事件累积到
 * 当前进行中的助手消息上；ai:done 时标记完成；ai:error 时记录错误。
 *
 * 智能体（agent）模式下，模型会通过 ai:tool_call 事件请求执行工具（操作 SSH/SQL），
 * 此时该工具调用进入 pendingToolCalls 待用户确认；用户点击执行/拒绝后，
 * 通过 ai:tool_result 事件回填结果。
 *
 * 任务清单（todo，借鉴 dsh tool-todo）：agent 模式下模型通过 todo_write 工具
 * 维护当前任务的步骤清单，后端经 ai:todo 事件推送整表，前端按会话持有并渲染
 * 在消息列表上方。新用户消息（新回合）开始时清空旧清单（standing-plan 语义）。
 */

/** 任务清单中的一项（todo_write 工具写入，状态以后端事件为准）。 */
export interface AiTodoItem {
  content: string;
  /** pending=待办；in_progress=进行中；completed=已完成。 */
  status: "pending" | "in_progress" | "completed";
}

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
  /** 桌面工具（desktop_*）绑定的 RDP 桥接实例 id（请求发起时的活动桌面）。 */
  desktopId?: string | null;
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
  /** 用户消息附带的多模态图片（展示用 data URL 由组件端拼接）。 */
  images?: ImagePart[];
  /** 是否正在流式接收中。 */
  streaming: boolean;
  error?: string;
  /** 该助手消息产生时的 agent 模式（"重新生成"据此重建上下文，而非当前面板 mode）。 */
  agent?: boolean;
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
 * 拆分为三个完全隔离的助手（各自独立的对话列表 / 多会话 / requestToCid 路由表）：
 * - `useAiSshStore`（id "ai:ssh"）：终端助手（终端页）
 * - `useAiDbStore`（id "ai:db"）：SQL 页数据库助手
 * - `useAiDesktopStore`（id "ai:desktop"）：桌面助手（桌面页，desktop_* 工具）
 *
 * 事件路由：MainLayout 把 `ai:*` 事件同时分发给三个 store，每个 store 的
 * `convForRequest(requestId)` 只会在自己的 `requestToCid` 里命中——对方 store
 * 找不到该 requestId 即静默 return，天然实现隔离，无需给事件加 domain 字段。
 */
export const makeAiStore = (id: string) =>
  defineStore(id, () => {
  // 从 store id 派生 domain（"ai:ssh"→"ssh"），用于持久化文件名。
  const domain = id.split(":")[1] ?? "ssh";
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
    /** 智能体任务清单（todo_write 工具最新整表；新用户消息时清空）。 */
    todos: AiTodoItem[];
    /** 会话累计 token 用量（ai:usage 事件累加，借鉴 dsh llm/token-meter）。 */
    usage: { prompt: number; completion: number };
  }

  /** 空用量（新会话初始值）。 */
  const EMPTY_USAGE = (): { prompt: number; completion: number } => ({ prompt: 0, completion: 0 });

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
      todos: [],
      usage: EMPTY_USAGE(),
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
        todos: [],
        usage: EMPTY_USAGE(),
      });
    }
    if (!activeCid.value) activeCid.value = conversations.value[0].id;
  }

  // --- 持久化（独立 JSON 文件，按 domain 分文件）---------------------------
  let persistTimer: ReturnType<typeof setTimeout> | null = null;

  /** 防抖持久化：把 conversations 映射为可序列化结构后全量写文件。
   *  streaming 字段强制为 false（避免重启后卡在"生成中"）；不存 activeRequestId/sending。 */
  function persist() {
    if (persistTimer) clearTimeout(persistTimer);
    persistTimer = setTimeout(() => {
      const data = conversations.value.map((c) => ({
        id: c.id,
        title: c.title,
        messages: c.messages.map((m) => ({ ...m, streaming: false })),
        todos: c.todos,
        usage: c.usage,
      }));
      aiApi.aiSaveConversations(domain, data).catch(() => {
        /* 持久化失败不阻塞对话（如磁盘满），仅忽略 */
      });
    }, 800);
  }

  /** 启动时从文件加载历史会话，替换默认的空对话。文件为空则保留默认。 */
  async function loadPersisted() {
    try {
      const list = await aiApi.aiListConversations(domain);
      if (list.length > 0) {
        conversations.value = list.map((c) => ({
          id: c.id,
          title: c.title || "新对话",
          // 还原消息，确保 streaming 为 false。
          messages: (c.messages as AiMessage[]).map((m) => ({ ...m, streaming: false })),
          activeRequestId: null,
          sending: false,
          // 旧持久化数据无 todos / usage 字段 → 默认空值。
          todos: (c as unknown as { todos?: AiTodoItem[] }).todos ?? [],
          usage: (c as unknown as { usage?: { prompt: number; completion: number } }).usage ?? {
            prompt: 0,
            completion: 0,
          },
        }));
        activeCid.value = conversations.value[0].id;
      }
    } catch {
      /* 读失败保持默认 */
    }
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
      todos: [],
      usage: EMPTY_USAGE(),
    };
    conversations.value.push(c);
    activeCid.value = c.id;
    persist();
    return c.id;
  }

  function switchConversation(cid: string) {
    if (conversations.value.some((c) => c.id === cid)) activeCid.value = cid;
  }

  function closeConversation(cid: string) {
    const idx = conversations.value.findIndex((c) => c.id === cid);
    if (idx < 0) return;
    const closed = conversations.value[idx];
    conversations.value.splice(idx, 1);
    // 若该对话正在流式生成，先中止后端任务：否则关闭后 token 还在继续消耗、
    // 事件无人接收，且后端 pending_ai_tasks 里的 JoinHandle 会一直挂到自然结束。
    if (closed.activeRequestId) {
      void stop(closed.activeRequestId);
    }
    if (activeCid.value === cid) {
      activeCid.value = conversations.value[0]?.id ?? null;
      if (!activeCid.value) ensureConversation();
    }
    persist();
  }

  /** 当前激活模型是否开启了多模态（决定历史消息里的图片是否随请求发送）。 */
  function activeModelMultimodal(): boolean {
    const s = useSettingsStore();
    const p = s.aiProviders.find((x) => `${x.kind}:${x.model}` === s.aiActive);
    return p?.multimodal ?? false;
  }

  /**
   * 发送一条用户消息并启动 AI 流式回复。
   * @param userText 用户输入
   * @param systemPrompt 系统提示词
   * @param opts.agent 是否启用工具调用（智能体模式）
   * @param opts.activeTerminalId 当前活动终端（agent 模式上下文）
   * @param opts.activeDbConnId 当前活动 MySQL 连接
   * @param opts.activeDesktopId 当前活动内嵌 RDP 会话（桌面助手上下文）
   * @param opts.domain 请求所属助手域（"ssh" | "db" | "desktop"），文件工具据此取工作目录
   * @param opts.images 附带的多模态图片（仅多模态模型下使用）
   */
  async function send(
    userText: string,
    systemPrompt?: string,
    opts?: {
      agent?: boolean;
      activeTerminalId?: string;
      activeDbConnId?: string;
      activeDesktopId?: string;
      domain?: string;
      images?: ImagePart[];
    }
  ) {
    ensureConversation();
    const conv = activeConversation.value!;
    if (conv.sending) return; // 按会话粒度互斥，不同会话可并发
    conv.sending = true;
    // 新用户消息 = 新回合开始：清空上一任务的任务清单（借鉴 dsh tool-todo 的
    // standing-plan 语义——旧清单只属于上一个任务，新回合从空白清单重新开始）。
    conv.todos = [];

    const userMsg: AiMessage = {
      id: genId(),
      role: "user",
      content: userText,
      streaming: false,
      images: opts?.images?.length ? opts.images : undefined,
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
      // 记录生成时的 agent 模式：AiPanel 的"重新生成"据此重建上下文，
      // 而不是用面板当前 mode（否则同一问题重问得到的上下文不一致）。
      agent: opts?.agent ?? false,
    };
    conv.messages.push(assistantMsg);

    const history: ChatMessage[] = [];
    if (systemPrompt) history.push({ role: "system", content: systemPrompt });
    // 图片随请求发送的前置条件：当前激活模型开启了多模态。切到普通模型后，
    // 历史消息里已带过的图片不再重发（文本模型收到图片块会 400），仅保留展示。
    const withImages = activeModelMultimodal();
    for (const m of conv.messages) {
      if (m.id === assistantMsg.id) break;
      history.push({
        role: m.role,
        content: m.content,
        images: withImages && m.images?.length ? m.images : undefined,
        toolCalls: m.toolCalls?.map((t) => ({
          id: t.toolCallId,
          name: t.name,
          arguments: t.arguments,
        })),
        toolCallId: m.toolCallId,
      });
      // 关键：当 assistant 消息带 tool_calls 时，OpenAI 兼容协议要求
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
        activeDesktopId: opts?.activeDesktopId,
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

  /** 找到该会话当前最后一条 assistant 消息（流式目标 / 工具卡片宿主）。
   *
   * 从尾部**向前**搜索，而不是只看最后一条：assistant 占位消息之后可能
   * 追加了其它角色的消息——系统注入的重复调用提醒（onSystemNote 的 user
   * 消息，轮末到达）就是典型。只看最后一条时，提醒之后到达的
   * onDelta/onToolCall/onToolResult 会因找不到 assistant 宿主而**静默丢弃**：
   * 表现为卡片永停"已确认 · 执行中"（结果路由失败）、后续轮次的文本与
   * 确认卡片凭空消失。向前搜索保证无论尾部插了什么，始终命中正确的
   * assistant 消息（一轮请求期间不会产生新的 assistant 消息）。 */
  function lastAssistant(conv: Conversation): AiMessage | null {
    for (let i = conv.messages.length - 1; i >= 0; i--) {
      const m = conv.messages[i];
      if (m.role === "assistant") return m;
    }
    return null;
  }

  /** 往助手消息追加一段文本：content 与 parts 双写保持同步（模板只渲染 parts）。 */
  function appendTextPart(m: AiMessage, text: string) {
    m.content += text;
    const parts = (m.parts ??= []);
    const last = parts[parts.length - 1];
    if (last && last.kind === "text") {
      last.text += text;
    } else {
      parts.push({ kind: "text", text });
    }
  }

  function onChunk(requestId: string, delta: string) {
    const conv = convForRequest(requestId);
    if (!conv) return;
    const m = lastAssistant(conv);
    if (!m) return;
    // 追加进有序 parts：若末尾是文本段就续上，否则新建一段（与上一个工具调用分隔）。
    appendTextPart(m, delta);
  }

  /** 请求结束后的路由表延迟清理（ms）：ai:tool_result 可能晚于 ai:done 到达
   * （多工具/慢命令时），立即 delete 会让晚到的结果路由失败、卡片永远停在
   * "执行中"。保留短暂 TTL 再清理；期间新请求用新 requestId，不受影响。 */
  const REQUEST_CLEANUP_DELAY_MS = 30_000;
  function scheduleRequestCleanup(requestId: string) {
    setTimeout(() => requestToCid.delete(requestId), REQUEST_CLEANUP_DELAY_MS);
  }

  function onDone(requestId: string) {
    const conv = convForRequest(requestId);
    if (!conv) return;
    const m = lastAssistant(conv);
    if (m) m.streaming = false;
    conv.sending = false;
    conv.activeRequestId = null;
    scheduleRequestCleanup(requestId);
    persist();
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
    // 路由用 TTL 延迟清理而不是立即删：可重试错误（建连失败/429/5xx）发生时
    // 后端会自动退避重试，重试成功后的 chunk/done/工具事件仍以同一
    // requestId 发送——立即删会让它们全部静默丢失（"报错了但其实又跑完
    // 了"，token 白花）。onRetrying 会恢复会话状态；最终失败时 error 已展示。
    scheduleRequestCleanup(requestId);
    persist();
  }

  /**
   * 自动重试通知（ai:retrying 事件）：可重试错误触发退避重试时后端发出。
   * 恢复会话的"进行中"状态（onError 已按错误收尾过）——清错误标记、
   * 重置 streaming/sending，重试成功后的输出继续正常流入。
   */
  function onRetrying(requestId: string) {
    const conv = convForRequest(requestId);
    if (!conv) return;
    const m = lastAssistant(conv);
    if (m) {
      m.streaming = true;
      m.error = undefined;
    }
    conv.sending = true;
    conv.activeRequestId = requestId;
  }

  /** AI 请求被用户终止（后端 ai:stopped 事件）。 */
  function onStopped(requestId: string) {
    const conv = convForRequest(requestId);
    if (!conv) return;
    const m = lastAssistant(conv);
    if (m) {
      m.streaming = false;
      // 终止标记必须写入 parts（content 与 parts 双写同步）：模板只渲染
      // parts，旧实现只改 content 导致标记永远不显示；若停止时还没有任何
      // 文本，整个气泡会渲染为空。
      const marker = "_（已终止）_";
      if (m.content.trim()) {
        appendTextPart(m, "\n\n" + marker);
      } else if (!m.error) {
        appendTextPart(m, marker);
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
    persist();
  }

  /** 用户点击"终止"按钮：调用后端 ai_stop。默认停当前对话；关闭对话时传显式 requestId。 */
  async function stop(requestId?: string) {
    const rid = requestId ?? activeConversation.value?.activeRequestId;
    if (!rid) return;
    try {
      await aiApi.aiStop(rid);
    } catch {
      /* 即使后端报错也本地收尾 */
    }
    // 本地立即收尾（不等 ai:stopped 事件，避免按钮卡顿）。
    onStopped(rid);
  }

  // --- 桌面工具（desktop_*，RDP 控制）执行 -----------------------------------
  // 桌面工具的执行体在**前端** RDP 会话里（后端桥接只透传字节），因此「批准即执行」
  // 由本 store 委托给桌面助手面板注册的执行器：执行器在 RDP 会话上截图/点击/输入，
  // 再通过 ai_desktop_tool_respond 把「批准/拒绝 + 执行结果」一体回执发给后端。
  const DESKTOP_TOOL_NAMES = [
    "desktop_screenshot",
    "desktop_click",
    "desktop_type",
    "desktop_key",
  ];
  type DesktopToolExecutor = (
    toolCallId: string,
    approved: boolean,
    name: string,
    args: Record<string, unknown>,
    desktopId: string | null
  ) => Promise<void>;
  let desktopToolExecutor: DesktopToolExecutor | null = null;

  /** 注册桌面工具执行器（桌面助手面板挂载时调用；其它域不注册、永不命中）。 */
  function setDesktopToolExecutor(fn: DesktopToolExecutor) {
    desktopToolExecutor = fn;
  }

  function isDesktopTool(name: string): boolean {
    return DESKTOP_TOOL_NAMES.includes(name);
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
      desktopId?: string | null;
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
    // ask_user_question 的执行体在**前端表单**（用户必须人工回答）：后端
    // auto_run=true 只表示"不弹二次确认卡片"，并不代表已自动执行——卡片必须
    // 保持 pending，否则提交/取消按钮被禁用、状态行误显示"已提交"。
    const isAsk = payload.name === "ask_user_question";
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
      desktopId: payload.desktopId ?? null,
      status: payload.autoApproved && !isAsk ? "approved" : "pending",
    };
    if (!m.toolCalls) m.toolCalls = [];
    m.toolCalls.push(item);
    // 按到达顺序记录到 parts（item 引用共享，卡片状态随 toolCalls 自动同步）。
    (m.parts ??= []).push({ kind: "tool", item });
    // 桌面工具被自动放行（desktop_screenshot）：执行体在前端，需立即执行并回传
    // 结果——不像后端工具那样已由后端执行完。执行器由桌面助手面板注册。
    if (payload.autoApproved && isDesktopTool(payload.name) && desktopToolExecutor) {
      void desktopToolExecutor(
        payload.toolCallId,
        true,
        payload.name,
        parsed,
        payload.desktopId ?? null
      );
    }
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

  /** 任务清单更新（todo_write 工具调用，整表替换语义，last-write-wins）。 */
  function onTodo(requestId: string, todos: AiTodoItem[]) {
    const conv = convForRequest(requestId);
    if (!conv) return;
    conv.todos = todos;
    persist();
  }

  /** token 用量上报（单次请求，ai:usage 事件）：累加到会话级统计。 */
  function onUsage(requestId: string, promptTokens: number, completionTokens: number) {
    const conv = convForRequest(requestId);
    if (!conv) return;
    conv.usage.prompt += promptTokens;
    conv.usage.completion += completionTokens;
    persist();
  }

  /**
   * 系统注入的编排层提示（ai:system_note 事件，如重复调用守卫提醒）。
   * 作为 user 消息插入前端消息流（模板按 "[重复调用提醒]" 前缀渲染为灰色
   * 居中提示，不伪装成用户发言）；与后端注入模型上下文的副本内容一致，
   * 下次请求重建 history 时两边自然对齐，不会重复。
   */
  function onSystemNote(requestId: string, text: string) {
    const conv = convForRequest(requestId);
    if (!conv) return;
    conv.messages.push({
      id: genId(),
      role: "user",
      content: text,
      streaming: false,
    });
    persist();
  }

  /** 用户点击"执行"。本地立即更新卡片状态为 approved，并通知后端。 */
  async function approveToolCall(toolCallId: string) {
    // 桌面工具：批准即执行——执行体在前端 RDP 会话（后端桥接只透传字节），
    // 由桌面助手面板注册的执行器负责执行并把「批准+结果」一体回执发给后端。
    const item = findToolCallItem(toolCallId);
    if (item && isDesktopTool(item.name) && desktopToolExecutor) {
      updateToolCallStatus(toolCallId, "approved");
      await desktopToolExecutor(
        toolCallId,
        true,
        item.name,
        item.arguments,
        item.desktopId ?? null
      );
      return;
    }
    updateToolCallStatus(toolCallId, "approved");
    // 批准下发失败（僵尸卡片：轮次已超时/请求已结束）：回滚状态并告知调用方，
    // 由 UI 提示——否则卡片停在"执行中"但命令永远不会执行。
    const ok = await dbApi
      .aiExecuteTool(toolCallId)
      .then(
        () => true,
        () => false,
      );
    if (!ok) {
      updateToolCallStatus(toolCallId, "pending");
      return false;
    }
    return true;
  }

  /** 用户点击"加入白名单并执行"：先把命令前缀加入白名单（持久化），再正常 approve。
   * 仅对 exec_ssh 工具有意义；其它工具直接 approve。
   * @returns 白名单是否写入成功（false 时 UI 不应提示"已加入白名单"）。 */
  async function addToWhitelistAndApprove(toolCallId: string): Promise<boolean> {
    // 找到该工具调用的命令参数。
    const cmd = findCommandByToolCallId(toolCallId);
    // 非 exec_ssh（无 command 参数）无需白名单，视为成功。
    let whitelisted = true;
    if (cmd) {
      try {
        await dbApi.aiAddToWhitelist(cmd);
      } catch (e) {
        // 白名单失败仍执行（用户已显式批准执行），但把失败结果返回给 UI——
        // 否则按钮显示"已加入白名单并执行"而实际没写入，误导用户。
        whitelisted = false;
        console.error("加入白名单失败:", e);
      }
    }
    await approveToolCall(toolCallId);
    return whitelisted;
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
    // 桌面工具的拒绝同样走前端执行器（approved=false 回执），后端从
    // pending_desktop_calls 取到拒绝结果后以"用户拒绝"收尾该轮。
    const item = findToolCallItem(toolCallId);
    if (item && isDesktopTool(item.name) && desktopToolExecutor) {
      updateToolCallStatus(toolCallId, "rejected");
      await desktopToolExecutor(
        toolCallId,
        false,
        item.name,
        item.arguments,
        item.desktopId ?? null
      );
      return;
    }
    updateToolCallStatus(toolCallId, "rejected");
    await dbApi.aiCancelTool(toolCallId).catch(() => {
      /* ignore */
    });
  }

  /**
   * 提交 ask_user_question 的回答（前端问题表单「提交」按钮）。
   * 后端把答案作为 tool 结果回填，下一轮模型即可看到。
   */
  async function answerAskUser(toolCallId: string, answers: AskUserAnswer[]) {
    updateToolCallStatus(toolCallId, "approved");
    await dbApi.aiAskUserRespond(toolCallId, true, answers).catch(() => {
      /* ignore */
    });
  }

  /** 取消 ask_user_question（「取消」按钮）：后端以"用户取消"回填该轮。 */
  async function cancelAskUser(toolCallId: string) {
    updateToolCallStatus(toolCallId, "rejected");
    await dbApi.aiAskUserRespond(toolCallId, false, []).catch(() => {
      /* ignore */
    });
  }

  /** 在所有会话中按 toolCallId 找到工具调用项（跨会话扫描，与事件路由一致）。 */
  function findToolCallItem(toolCallId: string): ToolCallItem | null {
    for (const conv of conversations.value) {
      for (const m of conv.messages) {
        if (!m.toolCalls) continue;
        const item = m.toolCalls.find((t) => t.toolCallId === toolCallId);
        if (item) return item;
      }
    }
    return null;
  }

  function updateToolCallStatus(toolCallId: string, status: ToolCallItem["status"]) {
    const item = findToolCallItem(toolCallId);
    if (item) item.status = status;
  }

  /** 清空当前活动会话的消息。 */
  function clear() {
    const conv = activeConversation.value;
    if (conv) {
      conv.messages = [];
      conv.title = "新对话";
      conv.todos = [];
      conv.usage = EMPTY_USAGE();
      persist();
    }
  }

  /**
   * 重命名对话（手动覆盖自动标题）。
   * 由于 send 自动标题仅在 title==="新对话" 时触发，手动重命名后不会被覆盖。
   */
  function renameConversation(cid: string, title: string) {
    const conv = conversations.value.find((c) => c.id === cid);
    if (conv) {
      conv.title = title.trim() || "新对话";
      persist();
    }
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
      activeDesktopId?: string;
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
    const userMsg = conv.messages[userIdx];
    // 删掉该 user 消息及之后的所有消息。
    conv.messages.splice(userIdx);
    // 重新发送（原用户消息若带图片，一并重发）。
    await send(userMsg.content, systemPrompt, {
      ...opts,
      images: userMsg.images,
    });
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
    onTodo,
    onUsage,
    onSystemNote,
    onRetrying,
    approveToolCall,
    addToWhitelistAndApprove,
    rejectToolCall,
    answerAskUser,
    cancelAskUser,
    setDesktopToolExecutor,
    clear,
    renameConversation,
    regenerate,
    loadPersisted,
  };
});

export const useAiSshStore = makeAiStore("ai:ssh");
export const useAiDbStore = makeAiStore("ai:db");
export const useAiDesktopStore = makeAiStore("ai:desktop");
