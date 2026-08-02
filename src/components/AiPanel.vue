<script setup lang="ts">
import { computed, nextTick, ref, watch } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { Promotion, Delete, ChatDotRound, DArrowRight, Connection, Tools, ArrowDown, Plus, Close, CopyDocument, RefreshRight, VideoPause, Document, Loading, Download } from "@element-plus/icons-vue";
import { marked } from "marked";
import DOMPurify from "dompurify";
import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";
import { homeDir, join } from "@tauri-apps/api/path";
import { useAiSshStore, useAiDbStore, type AiMessage } from "@/stores/ai";
import { useSettingsStore } from "@/stores/settings";
import { useTerminalsStore } from "@/stores/terminals";
import { useDbStore } from "@/stores/db";
import { useUiStore } from "@/stores/ui";
import { dbShowCreateTable, type DraggedTable } from "@/api/db";
import type { ToolCallItem } from "@/stores/ai";

/** 助手域：ssh=终端助手（终端页）；db=数据库助手（SQL 页）。决定用哪个 store、暴露哪些工具。 */
const props = defineProps<{ domain: "ssh" | "db" }>();

// 按 domain 选 store：两个 store 是同一 factory 产出的独立实例，状态完全隔离。
const ai = props.domain === "ssh" ? useAiSshStore() : useAiDbStore();
const settings = useSettingsStore();
// SSH 域需要读 terminals；DB 域需要读 db。两个都实例化（取用时按域判断），开销可忽略。
const terminals = useTerminalsStore();
const db = useDbStore();
const ui = useUiStore();

// --- 折叠 / 展开 ---------------------------------------------------------
// collapsed 与全局 uiStore 同步：全局快捷键（toggleAi）通过 uiStore 控制，
// 本组件点击竖条也走 ui.toggleAi，保证单一数据源。
const collapsed = computed({
  get: () => ui.aiCollapsed,
  set: (v) => ui.setAiCollapsed(v),
});
const EXPANDED_WIDTH = 340;
const COLLAPSED_WIDTH = 40;

function toggle() {
  ui.toggleAi();
}

// --- 模式 ----------------------------------------------------------------
// 按域定制：SSH 域聚焦服务器运维，DB 域聚焦数据库。两套模式与提示词独立。
type SshMode = "chat" | "translate" | "diagnose" | "explain" | "agent";
type DbMode = "chat" | "optimize" | "explain" | "agent";
type Mode = SshMode | DbMode;

const SSH_PROMPTS: Record<SshMode, string> = {
  chat:
    "你是一名资深 Linux/Unix 运维工程师助手，专注于服务器运维、网络、容器、Shell 脚本等。" +
    "回答简洁专业；若涉及命令，用 markdown ```bash 代码块给出，并附一行简短说明；危险操作前提醒用户。",
  translate:
    "用户用自然语言描述一个想完成的任务，请直接给出可在 Linux 终端执行的 shell 命令" +
    "（用 ```bash 代码块），紧跟一行简短说明。如操作有风险（如删除、重启、修改系统文件），" +
    "必须额外加 ⚠️ 提醒。不要多余解释。",
  diagnose:
    "用户会粘贴一段报错/命令输出。请分析错误根因，给出修复步骤，必要时用 ```bash 给出命令。" +
    "简洁、可直接执行。",
  explain:
    "用户会粘贴一段命令输出。请用通俗简洁的中文解释这段输出的含义、关键数字/字段、是否异常。",
  agent:
    "你是一名可执行操作的 SSH 运维智能体。你可以调用工具在用户的服务器上执行命令（exec_ssh）" +
    "和读取终端输出（terminal_snapshot）来完成任务。\n" +
    "规则：\n" +
    "1. 需要了解服务器状态或执行命令时，调用 exec_ssh 工具（提供 sessionId 和 command）。" +
    "不要用「据我所知」「通常」等措辞凭空回答系统状态——必须实际执行命令查看。\n" +
    "2. 不确定当前终端状态时，先调用 terminal_snapshot 查看最近输出。\n" +
    "3. 每一步都先告诉用户你要做什么，再调用工具。\n" +
    "4. 拿到结果后用中文简洁总结，必要时继续下一步。\n" +
    "5. 危险操作（删除、重启、修改系统文件等）必须明确告知风险后再调用，用户会再次确认。\n" +
    "6. 默认只查询不修改；如需修改，明确说明并调用工具。\n" +
    "工具的 sessionId 是终端实例 id（前端会提供）。",
};

const DB_PROMPTS: Record<DbMode, string> = {
  chat:
    "你是一名资深 MySQL DBA 助手，专注于 SQL 优化、表结构设计、索引、事务、性能调优。" +
    "回答简洁专业；SQL 用 markdown ```sql 代码块给出。",
  optimize:
    "用户会提供一段 MySQL SQL。请给出优化建议：索引、重写、执行计划推测。" +
    "优化后的 SQL 用 ```sql 代码块给出，并附简短说明。",
  explain:
    "用户会提供一段 MySQL SQL 或查询结果。请用通俗简洁的中文解释其含义、潜在问题。",
  agent:
    "你是一名可执行操作的 MySQL 数据库智能体。你可以调用工具在用户的数据库上执行 SQL" +
    "（exec_sql）、列出表（list_db_tables）、查看表结构（describe_table）来完成任务。\n" +
    "规则：\n" +
    "1. 需要查询数据时，调用 exec_sql（提供 dbConnId 和 sql）。默认只读查询（SELECT/SHOW/EXPLAIN）。" +
    "不要凭空猜测表结构或数据——必须实际执行 SQL 查看。\n" +
    "2. 不了解库结构时，先调用 list_db_tables 查看有哪些表，再用 describe_table 看字段。\n" +
    "3. 每一步都先告诉用户你要做什么，再调用工具。\n" +
    "4. 拿到结果后用中文简洁总结，必要时继续下一步。\n" +
    "5. 写操作（INSERT/UPDATE/DELETE/DDL）必须明确告知影响后再调用，用户会再次确认。\n" +
    "6. 默认只查询不修改；如需修改，明确说明并调用工具。\n" +
    "工具的 dbConnId 是数据库连接 id（前端会提供）。",
};

const SYSTEM_PROMPTS = (props.domain === "ssh" ? SSH_PROMPTS : DB_PROMPTS) as Record<Mode, string>;

const SSH_MODES: { label: string; value: SshMode }[] = [
  { label: "智能体", value: "agent" },
  { label: "对话", value: "chat" },
  { label: "翻译为命令", value: "translate" },
  { label: "诊断错误", value: "diagnose" },
  { label: "解释输出", value: "explain" },
];
const DB_MODES: { label: string; value: DbMode }[] = [
  { label: "智能体", value: "agent" },
  { label: "对话", value: "chat" },
  { label: "优化 SQL", value: "optimize" },
  { label: "解释 SQL", value: "explain" },
];
const MODE_OPTIONS = props.domain === "ssh" ? SSH_MODES : DB_MODES;

const mode = ref<Mode>("agent");

/** 面板标题：按域显示。 */
const panelTitle = computed(() => (props.domain === "ssh" ? "终端助手" : "数据库助手"));

// --- 对话标签重命名（双击进入编辑） ---
const editingCid = ref<string | null>(null);
const editingTitle = ref("");
const editTabInputRef = ref<HTMLInputElement | null>(null);
function startRename(c: { id: string; title: string }) {
  editingCid.value = c.id;
  editingTitle.value = c.title || "新对话";
  nextTick(() => {
    editTabInputRef.value?.focus();
    editTabInputRef.value?.select();
  });
}
function commitRename(cid: string) {
  if (editingCid.value === cid) {
    ai.renameConversation(cid, editingTitle.value);
    editingCid.value = null;
  }
}
function cancelRename() {
  editingCid.value = null;
}

// --- 智能体上下文 --------------------------------------------------------
/** agent 模式当前可用的活动终端 instanceId。 */
const activeTerminalId = computed(() => terminals.activeId);
/** agent 模式上下文提示文字（按域显示）。 */
const contextTip = computed(() => {
  if (mode.value !== "agent") return "";
  const parts: string[] = [];
  if (props.domain === "ssh") {
    if (activeTerminalId.value) {
      const tab = terminals.tabs.find((t) => t.instanceId === activeTerminalId.value);
      if (tab) parts.push(`终端: ${tab.session.name}`);
    }
  } else {
    // db 域
    if (db.activeConnId) {
      const c = db.conns.find((x) => x.id === db.activeConnId);
      parts.push(`数据库: ${c?.name ?? db.activeConnId}`);
      // 追加当前关联库（点库/表或拖表时设置）。
      if (db.activeDatabase) parts.push(`库: ${db.activeDatabase}`);
    }
  }
  // 本地文件读写：启用时展示工作目录状态（未设置则提示，引导去设置页）。
  if (settings.fileAccess.enabled) {
    const dir = settings.fileAccess.workspaceDirs[props.domain];
    parts.push(dir ? `文件: ${dir}` : "⚠ 文件读写已开启，未设置工作目录");
  }
  if (props.domain === "ssh") {
    return parts.length
      ? `已附加: ${parts.join("、")}`
      : "未选择活动终端，请先连接后 AI 才能操作";
  }
  return parts.length
    ? `已附加: ${parts.join(" · ")}`
    : "未连接数据库，请先在 SQL 控制台连接后 AI 才能操作";
});

// --- 配置状态 ------------------------------------------------------------
const hasProviders = computed(() => (settings.aiProviders?.length ?? 0) > 0);
const hasActive = computed(() => !!settings.aiActive);

const configBlocked = computed(() => !hasProviders.value || !hasActive.value);
const configTip = computed(() => {
  if (!hasProviders.value) return "未配置 AI provider，请到设置页添加";
  if (!hasActive.value) return "未选择激活模型";
  return "";
});

// --- 输入 / 发送 ---------------------------------------------------------
const inputText = ref("");
const scrollbarRef = ref();
const inputRef = ref();

// --- 拖表附加表结构上下文（仅 DB 域） -----------------------------------
// 用户从 SQL 控制台表树拖表到输入框时：
// - 输入框插入表名引用（如 `users`）；
// - 后台拉取该表 SHOW CREATE TABLE 的 DDL，存入 attachedTables；
// - 发送时把这些 DDL 拼进 system prompt，让 AI 拿到完整表结构。
interface AttachedTable {
  /** 完全限定名（库.表 或 表）。 */
  qualified: string;
  /** 库名（可空）。 */
  database: string | null;
  /** 表名。 */
  table: string;
  /** DDL 文本（拉取中为空串）。 */
  ddl: string;
  /** 拉取状态。 */
  loading: boolean;
  /** 拉取失败时的错误信息。 */
  error?: string;
}
const attachedTables = ref<AttachedTable[]>([]);
/** 拖拽悬停时高亮 composer。 */
const dragOver = ref(false);

/** 把拖入的表加入附加列表 + 输入框插入引用 + 后台拉 DDL。 */
async function attachTable(payload: DraggedTable) {
  // 同连接校验：DB 域只接受当前活动连接的表。
  if (props.domain === "db" && payload.connId !== db.activeConnId) {
    ElMessage.warning("只能拖入当前连接数据库的表");
    return;
  }
  // 拖表也算选中该表所在库（与点表行为一致），让助手关联到正确库。
  if (props.domain === "db" && payload.database) {
    db.setActiveDatabase(payload.database);
  }
  const qualified = payload.database
    ? `${payload.database}.${payload.table}`
    : payload.table;
  // 去重：已附加则不重复加。
  if (attachedTables.value.some((t) => t.qualified === qualified)) {
    ElMessage.info("该表已附加");
    return;
  }
  const entry: AttachedTable = {
    qualified,
    database: payload.database,
    table: payload.table,
    ddl: "",
    loading: true,
  };
  attachedTables.value.push(entry);

  // 输入框插入表名引用（用反引号包裹表名，光标位置追加）。
  const insertText = `\`${payload.table}\` `;
  const el = inputRef.value?.$el?.querySelector("textarea") as HTMLTextAreaElement | null;
  if (el) {
    const start = el.selectionStart ?? inputText.value.length;
    const end = el.selectionEnd ?? inputText.value.length;
    inputText.value =
      inputText.value.slice(0, start) + insertText + inputText.value.slice(end);
    nextTick(() => {
      el.focus();
      const pos = start + insertText.length;
      el.setSelectionRange(pos, pos);
    });
  } else {
    inputText.value += insertText;
  }

  // 后台拉 DDL（仅 DB 域有连接时）。
  if (props.domain === "db" && db.activeConnId) {
    try {
      const ddl = await dbShowCreateTable(
        db.activeConnId,
        payload.database,
        payload.table,
      );
      entry.ddl = ddl;
    } catch (e) {
      entry.error = String(e);
    } finally {
      entry.loading = false;
    }
  } else {
    entry.loading = false;
  }
}

/** composer 的 drop 处理。 */
function onComposerDrop(e: DragEvent) {
  e.preventDefault();
  dragOver.value = false;
  const raw = e.dataTransfer?.getData("application/x-xterm-table");
  if (!raw) return;
  try {
    const payload = JSON.parse(raw) as DraggedTable;
    void attachTable(payload);
  } catch {
    /* 忽略非法 payload */
  }
}
function onComposerDragOver(e: DragEvent) {
  // 仅当携带表数据时才允许 drop（避免普通文本拖入干扰）。
  const types = e.dataTransfer?.types ?? [];
  if (Array.from(types).includes("application/x-xterm-table")) {
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
    dragOver.value = true;
  }
}
function onComposerDragLeave() {
  dragOver.value = false;
}

/** 移除一个附加表。 */
function removeAttachedTable(qualified: string) {
  attachedTables.value = attachedTables.value.filter((t) => t.qualified !== qualified);
}

/** 清空所有附加表。 */
function clearAttachedTables() {
  attachedTables.value = [];
}

async function scrollToBottom() {
  await nextTick();
  const wrap = scrollbarRef.value?.wrapRef as HTMLElement | undefined;
  if (wrap) wrap.scrollTop = wrap.scrollHeight;
}

watch(() => ai.messages.length, scrollToBottom);
// 流式期间内容增长也要跟随。
watch(
  () => ai.messages.map((m) => m.content).join(""),
  scrollToBottom
);

async function handleSend() {
  const text = inputText.value.trim();
  if (!text || ai.sending || configBlocked.value) return;
  inputText.value = "";
  if (mode.value === "agent") {
    // 动态构建系统提示：把当前活动上下文的真实 id 注入，让模型直接填对参数。
    // 按域裁剪：SSH 面板只传 terminalId（后端就只暴露 SSH 工具），
    // DB 面板只传 dbConnId（后端就只暴露 SQL 工具），实现工具集硬隔离。
    let prompt = SYSTEM_PROMPTS.agent;
    const ctxParts: string[] = [];
    let activeTerminal: string | undefined;
    let activeDb: string | undefined;
    if (props.domain === "ssh") {
      if (activeTerminalId.value) {
        const tab = terminals.tabs.find((t) => t.instanceId === activeTerminalId.value);
        const name = tab?.session.name ?? "未命名";
        ctxParts.push(
          `当前活动 SSH 终端：sessionId="${activeTerminalId.value}"（${name}）。调用 exec_ssh / terminal_snapshot 时直接用这个 sessionId。`
        );
        activeTerminal = activeTerminalId.value;
      } else {
        ctxParts.push(
          "当前没有活动终端。请直接告诉用户：请先连接终端后再让我操作。不要调用任何工具。"
        );
      }
    } else {
      if (db.activeConnId) {
        const c = db.conns.find((x) => x.id === db.activeConnId);
        const name = c?.name ?? "未命名";
        ctxParts.push(
          `当前活动 MySQL 连接：dbConnId="${db.activeConnId}"（${name}）。调用 exec_sql / list_db_tables / describe_table 时直接用这个 dbConnId。`
        );
        activeDb = db.activeConnId;
        // 注入当前关联库（点库/表或拖表时设置），让 AI 默认在该库 schema 下操作。
        if (db.activeDatabase) {
          ctxParts.push(
            `当前关联的库（schema）为 "${db.activeDatabase}"。执行 SQL 时请默认针对该库的表；` +
              `用 database.table 限定名引用表（如 \`${db.activeDatabase}\`.\`表名\`）以避免库歧义。`
          );
        }
      } else {
        ctxParts.push(
          "当前没有连接数据库。请直接告诉用户：请先在 SQL 控制台连接数据库后再让我操作。不要调用任何工具。"
        );
      }
    }
    // 本地文件读写（设置页开启后注入）：告知模型工作目录与工具用法。
    if (settings.fileAccess.enabled) {
      const dir = settings.fileAccess.workspaceDirs[props.domain];
      if (dir) {
        ctxParts.push(
          `本地文件读写已启用，工作目录为 "${dir}"。可用 read_file / write_file / list_files ` +
            `工具（path 为相对工作目录的路径）读取数据文件或导出结果文件。`
        );
      } else {
        ctxParts.push(
          "本地文件读写已开启，但当前助手尚未设置工作目录。请告诉用户去设置页配置，不要调用文件工具。"
        );
      }
    }
    prompt += "\n\n=== 当前可用上下文 ===\n" + ctxParts.join("\n");
    // 附加表结构（拖表产生）：把 DDL 拼进 system prompt。
    prompt += buildAttachedDdlSection();
    await ai.send(text, prompt, {
      agent: true,
      activeTerminalId: activeTerminal,
      activeDbConnId: activeDb,
      domain: props.domain,
    });
  } else {
    let prompt = SYSTEM_PROMPTS[mode.value];
    const ddlSection = buildAttachedDdlSection();
    if (ddlSection) prompt += ddlSection;
    await ai.send(text, prompt);
  }
  // 发送后清空附加表（下次提问重新拖）。
  clearAttachedTables();
}

/** 拼接附加表的 DDL 段落（用于注入 system prompt）。无附加表返回空串。 */
function buildAttachedDdlSection(): string {
  const ready = attachedTables.value.filter((t) => t.ddl && !t.loading);
  if (ready.length === 0) return "";
  const blocks = ready.map((t) => `-- 表: ${t.qualified}\n${t.ddl}`);
  return (
    "\n\n=== 用户附加的表结构（用户拖入的相关表，分析时请参考）===\n" +
    blocks.join("\n\n")
  );
}

/** 终止当前 AI 请求（发送按钮在 sending 时点击触发）。 */
async function handleStop() {
  await ai.stop();
}

// --- 工具调用卡片交互 ----------------------------------------------------
/** 复制文本到剪贴板（消息内容 / 工具命令）。 */
async function copyText(text: string) {
  if (!text) return;
  try {
    await navigator.clipboard?.writeText(text);
    ElMessage.success("已复制");
  } catch {
    ElMessage.warning("复制失败（剪贴板未授权）");
  }
}

/** 重新生成某条 assistant 消息。 */
async function regenerateMessage(m: AiMessage) {
  if (ai.sending) return;
  // 重生时根据当前 mode 选系统提示；agent 模式需重建上下文（重新 send 会注入）。
  const prompt = SYSTEM_PROMPTS[mode.value];
  const opts =
    mode.value === "agent"
      ? {
          agent: true,
          activeTerminalId: props.domain === "ssh" ? (activeTerminalId.value ?? undefined) : undefined,
          activeDbConnId: props.domain === "db" ? (db.activeConnId ?? undefined) : undefined,
          domain: props.domain,
        }
      : undefined;
  try {
    await ai.regenerate(m.id, prompt, opts);
  } catch (e) {
    ElMessage.error("重生失败：" + String(e));
  }
}

async function approveTool(tool: ToolCallItem) {
  if (tool.status !== "pending") return;
  await ai.approveToolCall(tool.toolCallId);
}

/** 加入白名单并执行：把命令前缀持久化到白名单，然后正常 approve。 */
async function addToWhitelistAndRun(tool: ToolCallItem) {
  if (tool.status !== "pending") return;
  await ai.addToWhitelistAndApprove(tool.toolCallId);
  ElMessage.success("已加入白名单并执行");
}

async function rejectTool(tool: ToolCallItem) {
  if (tool.status !== "pending") return;
  await ai.rejectToolCall(tool.toolCallId);
}

/** 工具的参数/结果是否展开（每条卡片独立折叠）。 */
const expanded = ref<Record<string, boolean>>({});
function toggleExpand(id: string) {
  expanded.value[id] = !expanded.value[id];
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
    e.preventDefault();
    // 发送中：Enter 触发终止（与按钮行为一致）；否则发送。
    if (ai.sending) handleStop();
    else handleSend();
  }
}

// 自适应高度：2~5 行（由 CSS autosize 控制，这里只清空时复位）。
function autosize() {
  const el = inputRef.value?.ref?.querySelector?.("textarea") as HTMLTextAreaElement | undefined;
  if (!el) return;
  el.style.height = "auto";
  el.style.height = `${Math.min(el.scrollHeight, 5 * 24)}px`;
}

watch(inputText, () => nextTick(autosize));

// --- 清空 ----------------------------------------------------------------
async function handleClear() {
  try {
    await ElMessageBox.confirm("确定清空所有对话记录？", "清空对话", {
      type: "warning",
      confirmButtonText: "清空",
      cancelButtonText: "取消",
    });
  } catch {
    return;
  }
  ai.clear();
  ElMessage.success("已清空");
}

// --- 导出对话 ------------------------------------------------------------
/** 工具调用状态 → 导出文案。 */
const TOOL_STATUS_TEXT: Record<ToolCallItem["status"], string> = {
  pending: "待确认",
  approved: "执行中",
  rejected: "已拒绝",
  done: "已完成",
};

/** 导出当前会话为 Markdown 文件（含工具调用，便于归档/分享）。 */
async function handleExport() {
  const conv = ai.activeConversation;
  if (!conv || conv.messages.length === 0) return;
  const title = conv.title || "新对话";

  const lines: string[] = [];
  lines.push(`# ${title}`);
  lines.push("");
  lines.push(`> 助手: ${panelTitle.value}`);
  lines.push(`> 导出时间: ${new Date().toLocaleString("zh-CN", { hour12: false })}`);
  lines.push(`> 消息数: ${conv.messages.length}`);
  lines.push("");
  lines.push("---");

  for (const m of conv.messages) {
    lines.push("");
    lines.push(m.role === "user" ? "## 用户" : "## 助手");
    lines.push("");
    if (m.content) lines.push(m.content);
    // 工具调用：参数与执行结果逐条列出，保留完整执行记录。
    if (m.role === "assistant" && m.toolCalls && m.toolCalls.length > 0) {
      for (const t of m.toolCalls) {
        lines.push("");
        lines.push(`### 工具调用: \`${t.name}\``);
        lines.push("");
        lines.push(`**描述**: ${t.description}`);
        lines.push("");
        lines.push(`**状态**: ${TOOL_STATUS_TEXT[t.status] ?? t.status}`);
        lines.push("");
        lines.push("**参数**:");
        lines.push("");
        lines.push("```json");
        lines.push(JSON.stringify(t.arguments, null, 2));
        lines.push("```");
        if (t.result) {
          lines.push("");
          lines.push(t.result.ok ? "**输出**: " : "**失败**: ");
          lines.push("");
          lines.push("```");
          lines.push(t.result.output);
          lines.push("```");
        }
        lines.push("");
      }
    }
    if (m.error) {
      lines.push("");
      lines.push(`> ⚠ ${m.error}`);
    }
    lines.push("");
  }

  // 默认文件名：助手名-对话标题-时间戳.md（去掉 Windows 非法字符）。
  const safeTitle = title.replace(/[\\/:*?"<>|]/g, "_");
  const pad = (n: number) => String(n).padStart(2, "0");
  const now = new Date();
  const stamp = `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}-${pad(now.getHours())}${pad(now.getMinutes())}${pad(now.getSeconds())}`;
  const defaultPath = await join(await homeDir(), `${panelTitle.value}-${safeTitle}-${stamp}.md`);

  const filePath = await save({
    title: `导出${panelTitle.value}对话`,
    defaultPath,
    filters: [{ name: "Markdown", extensions: ["md"] }],
  });
  if (!filePath) return; // 用户取消

  try {
    await writeTextFile(filePath, lines.join("\n"));
    ElMessage.success("对话已导出");
  } catch (e) {
    ElMessage.error("导出失败: " + String(e));
  }
}

// --- Markdown 渲染 -------------------------------------------------------
// 用 marked 解析完整 GFM（标题/列表/粗体/表格/链接/代码块等），DOMPurify 清洗
// 防 XSS（因为通过 v-html 渲染）。marked 配置 GFM + line break，贴近聊天 UI 习惯。
marked.setOptions({
  gfm: true, // GitHub Flavored Markdown：表格、删除线、任务列表等
  breaks: true, // 单换行也转 <br>（聊天场景更自然）
});

const renderedCache = new Map<string, string>();
function renderMarkdown(text: string): string {
  if (!text) return "";
  // 流式过程中同一段文本会被反复渲染，缓存避免重复解析。
  const cached = renderedCache.get(text);
  if (cached !== undefined) return cached;
  let html: string;
  try {
    html = marked.parse(text, { async: false }) as string;
  } catch {
    html = "";
  }
  const clean = DOMPurify.sanitize(html);
  // 缓存上限 200 条，避免长对话内存膨胀。
  if (renderedCache.size > 200) renderedCache.clear();
  renderedCache.set(text, clean);
  return clean;
}
</script>

<template>
  <div class="ai-panel" :class="{ collapsed }" :style="{ width: collapsed ? COLLAPSED_WIDTH + 'px' : EXPANDED_WIDTH + 'px' }">
    <!-- 折叠态：窄竖条 -->
    <div v-if="collapsed" class="rail" @click="toggle" :title="`展开 ${panelTitle}`">
      <el-icon class="rail-icon"><ChatDotRound /></el-icon>
      <span class="rail-text">{{ panelTitle }}</span>
    </div>

    <!-- 展开态 -->
    <div v-else class="body">
      <!-- 顶部 -->
      <div class="header">
        <div class="title">{{ panelTitle }}</div>
        <div class="header-actions">
          <el-tooltip content="收起" placement="bottom">
            <el-button class="icon-btn" link @click="toggle">
              <el-icon><DArrowRight /></el-icon>
            </el-button>
          </el-tooltip>
          <el-tooltip content="导出对话" placement="bottom">
            <el-button class="icon-btn" link :disabled="ai.messages.length === 0" @click="handleExport">
              <el-icon><Download /></el-icon>
            </el-button>
          </el-tooltip>
          <el-tooltip content="清空对话" placement="bottom">
            <el-button class="icon-btn" link :disabled="ai.messages.length === 0" @click="handleClear">
              <el-icon><Delete /></el-icon>
            </el-button>
          </el-tooltip>
        </div>
      </div>

      <!-- 对话标签栏：多会话切换 -->
      <div class="conv-tabs">
        <div
          v-for="c in ai.conversations"
          :key="c.id"
          class="conv-tab"
          :class="{ active: c.id === ai.activeCid }"
          :title="c.title"
          @click="ai.switchConversation(c.id)"
        >
          <input
            v-if="editingCid === c.id"
            ref="editTabInputRef"
            v-model="editingTitle"
            class="conv-tab-edit"
            @click.stop
            @keyup.enter="commitRename(c.id)"
            @blur="commitRename(c.id)"
            @keyup.esc="cancelRename"
          />
          <span
            v-else
            class="conv-tab-title"
            @dblclick="startRename(c)"
          >{{ c.title || '新对话' }}</span>
          <el-icon
            v-if="ai.conversations.length > 1"
            class="conv-tab-close"
            @click.stop="ai.closeConversation(c.id)"
          >
            <Close />
          </el-icon>
        </div>
        <el-tooltip content="新对话" placement="bottom">
          <el-button class="conv-new" link @click="ai.createConversation()">
            <el-icon><Plus /></el-icon>
          </el-button>
        </el-tooltip>
      </div>

      <!-- 模式选择 + 智能体上下文提示已移至底部输入区（composer）上方 -->

      <!-- 配置提示 -->
      <div v-if="configBlocked" class="config-tip">
        <el-alert :title="configTip" type="warning" :closable="false" show-icon />
      </div>

      <!-- 消息列表 -->
      <el-scrollbar ref="scrollbarRef" class="messages">
        <div v-if="ai.messages.length === 0" class="empty-hint">
          <template v-if="mode === 'agent'">
            <template v-if="domain === 'ssh'">
              智能体模式：AI 可在你的服务器上执行 SSH 命令。先连接一个终端，然后描述任务。
            </template>
            <template v-else>
              智能体模式：AI 可在你的数据库上执行 SQL。先在 SQL 控制台连接数据库，然后描述任务。
            </template>
          </template>
          <template v-else>暂无对话。选择模式后输入你的问题。</template>
        </div>
        <div
          v-for="m in ai.messages"
          :key="m.id"
          class="msg"
          :class="m.role === 'user' ? 'msg-user' : 'msg-ai'"
        >
          <div class="bubble" :class="{ 'bubble-error': m.error }">
            <!-- 悬停操作条 -->
            <div class="msg-actions" v-if="!m.streaming">
              <el-tooltip content="复制" placement="top">
                <el-icon class="msg-action" @click="copyText(m.content)"><CopyDocument /></el-icon>
              </el-tooltip>
              <el-tooltip
                v-if="m.role === 'assistant' && m.content && !m.error"
                content="重新生成"
                placement="top"
              >
                <el-icon class="msg-action" @click="regenerateMessage(m)"><RefreshRight /></el-icon>
              </el-tooltip>
            </div>
            <template v-if="m.role === 'assistant'">
              <!--
                按事件到达顺序渲染片段：文本段与工具调用交替出现，使工具卡片落在
                正文中间的真实位置（如「说要做X → 工具卡片 → 总结」），而非全堆顶部。
              -->
              <template v-for="(part, pIdx) in (m.parts ?? [])" :key="pIdx">
                <!-- 文本段：markdown 渲染（空段不输出占位） -->
                <div
                  v-if="part.kind === 'text' && part.text"
                  class="md"
                  v-html="renderMarkdown(part.text)"
                />
                <!-- 工具调用卡片：item 与 m.toolCalls 内为同一引用，状态自动同步 -->
                <div
                  v-else-if="part.kind === 'tool'"
                  class="tool-card"
                  :class="{
                    'tool-danger': part.item.dangerous,
                    'tool-whitelist': part.item.whitelisted && !part.item.dangerous,
                    'tool-done': part.item.status === 'done',
                    'tool-rejected': part.item.status === 'rejected',
                  }"
                >
                  <div class="tool-head" @click="toggleExpand(part.item.toolCallId)">
                    <el-icon class="tool-icon"><Tools /></el-icon>
                    <span class="tool-desc">{{ part.item.description }}</span>
                    <el-tag v-if="part.item.dangerous" type="danger" size="small" effect="dark">危险</el-tag>
                    <el-tag v-else-if="part.item.autoApproved" type="success" size="small" effect="dark">已自动执行</el-tag>
                    <el-tag v-else-if="part.item.whitelisted" type="success" size="small" effect="plain">白名单</el-tag>
                    <el-tag v-else type="warning" size="small" effect="plain">需确认</el-tag>
                    <el-icon
                      v-if="part.item.name === 'exec_ssh' && typeof part.item.arguments.command === 'string'"
                      class="tool-copy"
                      title="复制命令"
                      @click.stop="copyText(part.item.arguments.command as string)"
                    ><CopyDocument /></el-icon>
                    <el-icon class="tool-expand"><ArrowDown /></el-icon>
                  </div>
                  <div v-if="expanded[part.item.toolCallId]" class="tool-detail">
                    <div class="tool-args">
                      <span class="label">参数:</span>
                      <pre>{{ JSON.stringify(part.item.arguments, null, 2) }}</pre>
                    </div>
                    <div v-if="part.item.result" class="tool-output">
                      <span class="label">{{ part.item.result.ok ? '输出:' : '失败:' }}</span>
                      <pre>{{ part.item.result.output }}</pre>
                    </div>
                  </div>
                  <!-- 操作按钮（仅 pending 时显示；自动放行的无按钮） -->
                  <div v-if="part.item.status === 'pending'" class="tool-actions">
                    <el-button
                      size="small"
                      :type="part.item.dangerous ? 'danger' : 'primary'"
                      @click.stop="approveTool(part.item)"
                    >
                      {{ part.item.dangerous ? '确认执行危险操作' : part.item.whitelisted ? '执行' : '确认执行' }}
                    </el-button>
                    <!-- 加入白名单并执行：仅 exec_ssh 非危险非白名单时显示 -->
                    <el-button
                      v-if="part.item.name === 'exec_ssh' && !part.item.dangerous && !part.item.whitelisted"
                      size="small"
                      type="success"
                      plain
                      @click.stop="addToWhitelistAndRun(part.item)"
                    >
                      加入白名单并执行
                    </el-button>
                    <el-button size="small" @click.stop="rejectTool(part.item)">拒绝</el-button>
                  </div>
                  <div v-else-if="part.item.status === 'approved' && !part.item.autoApproved" class="tool-status">执行中…</div>
                  <div v-else-if="part.item.status === 'approved' && part.item.autoApproved" class="tool-status">已自动执行</div>
                  <div v-else-if="part.item.status === 'rejected'" class="tool-status">已拒绝</div>
                </div>
              </template>
              <!-- 流式占位：无任何片段且正在流式 → 省略号；有文本且流式中 → 闪烁光标 -->
              <span v-if="m.streaming && !(m.parts && m.parts.length)" class="dots">...</span>
              <span v-if="m.streaming && m.content" class="cursor" />
              <div v-if="m.error" class="error-text">⚠ {{ m.error }}</div>
            </template>
            <template v-else>{{ m.content }}</template>
          </div>
        </div>
      </el-scrollbar>

      <!-- 底部输入区 -->
      <div
        class="composer"
        :class="{ 'drag-over': dragOver }"
        @drop="onComposerDrop"
        @dragover="onComposerDragOver"
        @dragleave="onComposerDragLeave"
      >
        <!-- 输入框上方的工具栏：模式选择 + 智能体上下文（已附加终端） -->
        <div class="composer-toolbar">
          <el-select v-model="mode" size="small" class="mode-select">
            <el-option
              v-for="opt in MODE_OPTIONS"
              :key="opt.value"
              :label="opt.label"
              :value="opt.value"
            />
          </el-select>
          <span v-if="mode === 'agent' && !configBlocked" class="ctx-tip-inline">
            <el-icon><Connection /></el-icon>
            <span>{{ contextTip }}</span>
          </span>
        </div>
        <!-- 已附加的表（拖入后显示，结构会随问题一起发给 AI） -->
        <div v-if="attachedTables.length > 0" class="attached-tables">
          <span class="attached-label">
            <el-icon><Document /></el-icon>
            表结构:
          </span>
          <el-tag
            v-for="t in attachedTables"
            :key="t.qualified"
            closable
            size="small"
            :type="t.error ? 'danger' : 'info'"
            @close="removeAttachedTable(t.qualified)"
          >
            <el-icon v-if="t.loading" class="is-loading"><Loading /></el-icon>
            {{ t.table }}
          </el-tag>
          <el-button
            link
            size="small"
            class="attached-clear"
            @click="clearAttachedTables"
          >
            清空
          </el-button>
        </div>
        <div class="composer-row">
          <div class="input-wrap">
            <el-input
              ref="inputRef"
              v-model="inputText"
              type="textarea"
              :autosize="{ minRows: 2, maxRows: 5 }"
              :placeholder="
                configBlocked
                  ? configTip
                  : mode === 'translate'
                  ? '描述想做什么...'
                  : mode === 'diagnose'
                  ? '粘贴报错信息...'
                  : mode === 'explain'
                  ? '粘贴命令输出...'
                  : '输入问题，Enter 发送，Shift+Enter 换行'
              "
              :disabled="configBlocked"
              resize="none"
              @keydown="onKeydown"
            />
            <!-- 发送 / 终止：输入框内右下角悬浮图标，发送中变为红色终止图标 -->
            <el-button
              v-if="!ai.sending"
              link
              :icon="Promotion"
              :disabled="!inputText.trim() || configBlocked"
              class="send-inner-btn"
              title="发送 (Enter)"
              @click="handleSend"
            />
            <el-button
              v-else
              link
              :icon="VideoPause"
              class="send-inner-btn stop-btn"
              title="终止生成"
              @click="handleStop"
            />
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.ai-panel {
  height: 100%;
  background: var(--el-bg-color);
  border-left: 1px solid var(--el-border-color-light);
  display: flex;
  flex-direction: column;
  overflow: hidden;
  transition: width 0.2s ease;
  flex-shrink: 0;
}

/* --- 折叠竖条 --- */
.rail {
  width: 40px;
  height: 100%;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: flex-start;
  padding-top: 16px;
  cursor: pointer;
  gap: 12px;
  color: var(--el-text-color-secondary);
  transition: background 0.15s, color 0.15s;
}
.rail:hover {
  background: var(--el-fill-color-light);
  color: var(--el-color-primary);
}
.rail-icon {
  font-size: 18px;
}
.rail-text {
  writing-mode: vertical-rl;
  letter-spacing: 4px;
  font-size: 13px;
  margin-top: 8px;
}

/* --- 展开态 --- */
.body {
  height: 100%;
  display: flex;
  flex-direction: column;
  min-height: 0;
}

.header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 10px 12px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
}
.title {
  font-size: 14px;
  font-weight: 600;
  color: var(--el-text-color-primary);
}
.header-actions {
  display: flex;
  gap: 4px;
}

/* 对话标签栏 */
.conv-tabs {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 0 6px 6px;
  flex-shrink: 0;
  overflow-x: auto;
  border-bottom: 1px solid var(--el-border-color-lighter);
}
.conv-tab {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  max-width: 130px;
  padding: 3px 8px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
  background: var(--el-fill-color-light);
  border-radius: 4px;
  cursor: pointer;
  white-space: nowrap;
  transition: background 0.15s, color 0.15s;
}
.conv-tab:hover {
  background: var(--el-fill-color);
}
.conv-tab.active {
  color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
}
.conv-tab-title {
  overflow: hidden;
  text-overflow: ellipsis;
}
.conv-tab-edit {
  flex: 1;
  min-width: 40px;
  max-width: 110px;
  border: 1px solid var(--el-color-primary);
  border-radius: 3px;
  background: var(--el-bg-color);
  color: var(--el-text-color-primary);
  font-size: 12px;
  padding: 1px 4px;
  outline: none;
}
.conv-tab-close {
  font-size: 12px;
  border-radius: 50%;
  padding: 1px;
  flex-shrink: 0;
}
.conv-tab-close:hover {
  background: var(--el-fill-color-dark);
}
.conv-new {
  padding: 4px 6px;
  color: var(--el-text-color-secondary);
  flex-shrink: 0;
}
.conv-new:hover {
  color: var(--el-color-primary);
}
.icon-btn {
  padding: 4px;
  color: var(--el-text-color-secondary);
}
.icon-btn:hover {
  color: var(--el-color-primary);
}

.config-tip {
  padding: 8px 12px 0;
  flex-shrink: 0;
}

/* --- 消息列表 --- */
.messages {
  flex: 1;
  min-height: 0;
  padding: 12px;
}
.empty-hint {
  color: var(--el-text-color-secondary);
  font-size: 13px;
  text-align: center;
  margin-top: 32px;
}
.msg {
  display: flex;
  margin-bottom: 12px;
  position: relative;
}
/* 悬停操作条（复制/重生） */
.msg-actions {
  position: absolute;
  top: -4px;
  display: flex;
  gap: 2px;
  opacity: 0;
  transition: opacity 0.15s;
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 4px;
  padding: 2px;
  z-index: 5;
}
.msg:hover .msg-actions {
  opacity: 1;
}
.msg-user .msg-actions {
  right: 0;
}
.msg-ai .msg-actions {
  left: 0;
}
.msg-action {
  padding: 3px;
  font-size: 13px;
  color: var(--el-text-color-secondary);
  cursor: pointer;
  border-radius: 3px;
}
.msg-action:hover {
  background: var(--el-fill-color);
  color: var(--el-color-primary);
}
.msg-user {
  justify-content: flex-end;
}
.msg-ai {
  justify-content: flex-start;
}
.bubble {
  max-width: 85%;
  padding: 8px 12px;
  border-radius: 10px;
  font-size: 13px;
  line-height: 1.5;
  word-break: break-word;
  white-space: pre-wrap;
}
.msg-user .bubble {
  background: var(--el-color-primary);
  color: #fff;
  border-bottom-right-radius: 2px;
}
.msg-ai .bubble {
  background: var(--el-fill-color-light);
  color: var(--el-text-color-primary);
  border-bottom-left-radius: 2px;
}
.bubble-error {
  background: var(--el-color-danger-light-9) !important;
  color: var(--el-color-danger) !important;
  border: 1px solid var(--el-color-danger-light-5);
}
.error-text {
  font-weight: 600;
  margin-top: 4px;
}

/* 流式光标与省略号 */
.dots {
  letter-spacing: 2px;
  opacity: 0.7;
}
.cursor {
  display: inline-block;
  width: 7px;
  height: 14px;
  margin-left: 2px;
  vertical-align: text-bottom;
  background: var(--el-text-color-primary);
  animation: blink 1s step-end infinite;
}
@keyframes blink {
  0%,
  50% {
    opacity: 1;
  }
  51%,
  100% {
    opacity: 0;
  }
}

/* markdown 渲染 */
.md {
  white-space: normal;
  line-height: 1.6;
  font-size: 13px;
  word-break: break-word;
}
.md :deep(pre) {
  background: var(--el-fill-color-darker);
  color: var(--el-color-success);
  padding: 8px 10px;
  border-radius: 6px;
  overflow-x: auto;
  margin: 6px 0;
  font-family: Consolas, "Cascadia Code", "Courier New", monospace;
  font-size: 12px;
  line-height: 1.5;
  white-space: pre;
}
.md :deep(pre code) {
  /* 代码块内的 code 不套行内样式 */
  background: none;
  color: inherit;
  padding: 0;
}
/* 行内 code（非代码块内） */
.md :deep(code:not(pre code)) {
  background: var(--el-fill-color-darker);
  color: var(--el-color-warning);
  padding: 1px 5px;
  border-radius: 3px;
  font-family: Consolas, "Cascadia Code", "Courier New", monospace;
  font-size: 12px;
}
/* 标题 */
.md :deep(h1),
.md :deep(h2),
.md :deep(h3),
.md :deep(h4),
.md :deep(h5),
.md :deep(h6) {
  margin: 10px 0 6px;
  font-weight: 600;
  line-height: 1.3;
}
.md :deep(h1) {
  font-size: 17px;
}
.md :deep(h2) {
  font-size: 15px;
}
.md :deep(h3) {
  font-size: 14px;
}
.md :deep(h4),
.md :deep(h5),
.md :deep(h6) {
  font-size: 13px;
}
/* 段落与列表 */
.md :deep(p) {
  margin: 6px 0;
}
.md :deep(ul),
.md :deep(ol) {
  margin: 6px 0;
  padding-left: 22px;
}
.md :deep(li) {
  margin: 3px 0;
}
.md :deep(li > input[type="checkbox"]) {
  margin-right: 6px;
  vertical-align: middle;
}
/* 引用 */
.md :deep(blockquote) {
  margin: 6px 0;
  padding: 4px 12px;
  border-left: 3px solid var(--el-border-color);
  color: var(--el-text-color-secondary);
  background: var(--el-fill-color-light);
}
/* 表格 */
.md :deep(table) {
  border-collapse: collapse;
  margin: 6px 0;
  font-size: 12px;
  width: auto;
  max-width: 100%;
  display: block;
  overflow-x: auto;
}
.md :deep(th),
.md :deep(td) {
  border: 1px solid var(--el-border-color);
  padding: 4px 8px;
  text-align: left;
}
.md :deep(th) {
  background: var(--el-fill-color-light);
  font-weight: 600;
}
/* 分隔线 */
.md :deep(hr) {
  border: none;
  border-top: 1px solid var(--el-border-color);
  margin: 10px 0;
}
/* 链接 */
.md :deep(a) {
  color: var(--el-color-primary);
  text-decoration: none;
}
.md :deep(a:hover) {
  text-decoration: underline;
}
/* 行内强调 */
.md :deep(strong) {
  font-weight: 600;
}

/* --- 输入区 --- */
.composer {
  border-top: 1px solid var(--el-border-color-lighter);
  padding: 10px 12px;
  flex-shrink: 0;
  transition: background 0.15s, box-shadow 0.15s;
}
/* 拖表悬停时高亮整个 composer，提示可放置。 */
.composer.drag-over {
  background: var(--el-color-primary-light-9);
  box-shadow: inset 0 0 0 2px var(--el-color-primary);
}
/* 已附加的表标签条。 */
.attached-tables {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 4px 6px;
  margin-bottom: 6px;
  padding: 4px 6px;
  background: var(--el-fill-color-light);
  border-radius: 4px;
  font-size: 12px;
}
.attached-label {
  display: inline-flex;
  align-items: center;
  gap: 3px;
  color: var(--el-text-color-secondary);
  margin-right: 2px;
}
.attached-clear {
  margin-left: auto;
  font-size: 11px;
}
/* 输入框上方工具栏：模式选择在左，智能体上下文（已附加终端）在右 */
.composer-toolbar {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 6px;
  min-height: 24px;
}
.composer-toolbar .mode-select {
  width: auto;
  min-width: 96px;
}
.ctx-tip-inline {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-size: 11px;
  color: var(--el-text-color-secondary);
  /* 截断过长的会话名，避免撑宽面板 */
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.composer-row {
  display: flex;
  align-items: flex-end;
}
/* 输入框容器：发送图标悬浮在右下角（不占布局空间） */
.input-wrap {
  position: relative;
  flex: 1;
}
.composer-row :deep(.el-textarea__inner) {
  resize: none;
  /* 右下角给悬浮图标留出空间，避免文字被遮挡 */
  padding-right: 36px;
}
/* 输入框内右下角的纯图标发送/终止按钮（link 无边框背景） */
.send-inner-btn {
  position: absolute;
  right: 4px;
  bottom: 4px;
  font-size: 16px;
  color: var(--el-color-primary);
  padding: 3px;
  border-radius: 4px;
}
.send-inner-btn:disabled {
  color: var(--el-text-color-placeholder);
  background: transparent;
  cursor: not-allowed;
}
.send-inner-btn:hover:not(:disabled) {
  background: var(--el-fill-color-light);
}
.send-inner-btn.stop-btn {
  color: var(--el-color-danger);
}

/* --- 智能体上下文提示（已合并进 composer-toolbar，见 .ctx-tip-inline） --- */

/* --- 工具调用卡片 ---
   注意：卡片不再包裹在 .tool-calls 容器里，而是按 parts 顺序直接插入消息流
   （可能夹在文本段之间）。用纵向 margin 给它与文本/相邻卡片的间距；
   首个/最后一个元素用 :first-child 折叠多余外边距，避免气泡上下出现空白。 */
.tool-card {
  border: 1px solid var(--el-border-color);
  border-radius: 6px;
  background: var(--el-fill-color-blank);
  overflow: hidden;
  font-size: 12px;
  margin: 6px 0;
}
.tool-card:first-child {
  margin-top: 0;
}
.tool-card:last-child {
  margin-bottom: 0;
}
.tool-card.tool-danger {
  border-color: var(--el-color-danger);
  background: var(--el-color-danger-light-9);
}
/* 白名单内命令：绿色边框，提示"可放心执行"。 */
.tool-card.tool-whitelist {
  border-color: var(--el-color-success-light-5);
  background: var(--el-color-success-light-9);
}
.tool-card.tool-done {
  border-color: var(--el-color-success-light-5);
}
.tool-card.tool-rejected {
  opacity: 0.6;
}
.tool-head {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 8px;
  cursor: pointer;
  user-select: none;
}
.tool-head:hover {
  background: var(--el-fill-color-light);
}
.tool-icon {
  color: var(--el-color-primary);
  flex-shrink: 0;
}
.tool-danger .tool-icon {
  color: var(--el-color-danger);
}
.tool-desc {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.tool-expand {
  font-size: 10px;
  color: var(--el-text-color-secondary);
}
.tool-copy {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  cursor: pointer;
  padding: 2px;
  border-radius: 3px;
}
.tool-copy:hover {
  background: var(--el-fill-color);
  color: var(--el-color-primary);
}
.tool-detail {
  padding: 4px 8px 8px;
  border-top: 1px dashed var(--el-border-color-lighter);
}
.tool-args,
.tool-output {
  margin-top: 4px;
}
.tool-args .label,
.tool-output .label {
  font-size: 11px;
  color: var(--el-text-color-secondary);
}
.tool-detail pre {
  margin: 2px 0 6px;
  padding: 4px 6px;
  background: var(--el-fill-color-dark);
  color: var(--el-color-success);
  border-radius: 4px;
  font-size: 11px;
  max-height: 160px;
  overflow: auto;
  white-space: pre-wrap;
  word-break: break-all;
}
.tool-actions {
  display: flex;
  gap: 6px;
  padding: 4px 8px 6px;
}
.tool-status {
  padding: 4px 8px 6px;
  font-size: 11px;
  color: var(--el-text-color-secondary);
}
</style>
