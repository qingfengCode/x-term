<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { ElMessage, ElMessageBox, ElNotification } from "element-plus";
import { marked } from "marked";
import DOMPurify from "dompurify";
import { open, save } from "@tauri-apps/plugin-dialog";
import { readFile, writeTextFile } from "@tauri-apps/plugin-fs";
import { homeDir, join } from "@tauri-apps/api/path";
import { useAiSshStore, useAiDbStore, useAiDesktopStore, type AiMessage } from "@/stores/ai";
import { useSettingsStore } from "@/stores/settings";
import { useTerminalsStore } from "@/stores/terminals";
import { useDbStore } from "@/stores/db";
import { useUiStore } from "@/stores/ui";
import { useDesktopTabsStore } from "@/stores/desktopTabs";
import { dbShowCreateTable, type DraggedTable } from "@/api/db";
import type { AskUserAnswer } from "@/api/db";
import { executeDesktopTool, aiDesktopToolRespond, getDesktopControl } from "@/api/desktopControl";
import type { ToolCallItem } from "@/stores/ai";
import { bytesToBase64 } from "@/utils/binary";
import type { ImagePart, ToolRunMode } from "@/api/types";
import { RUN_MODE_OPTIONS } from "@/api/types";
import SkillDialog from "@/components/SkillDialog.vue";
import SkillManagerDialog from "@/components/SkillManagerDialog.vue";
import type { SkillConfig } from "@/api/types";

/** 助手域：ssh=终端助手（终端页）；db=数据库助手（SQL 页）；desktop=桌面助手（桌面页）。
 *  决定用哪个 store、暴露哪些工具。 */
const props = defineProps<{ domain: "ssh" | "db" | "desktop" }>();

// 按 domain 选 store：三个 store 是同一 factory 产出的独立实例，状态完全隔离。
const ai =
  props.domain === "ssh"
    ? useAiSshStore()
    : props.domain === "db"
      ? useAiDbStore()
      : useAiDesktopStore();
const settings = useSettingsStore();
// SSH 域需要读 terminals；DB 域需要读 db；desktop 域需要读 desktopTabs。
// 都实例化（取用时按域判断），开销可忽略。
const terminals = useTerminalsStore();
const db = useDbStore();
const desktopTabs = useDesktopTabsStore();
const ui = useUiStore();

// --- 折叠 / 展开 ---------------------------------------------------------
// collapsed 与全局 uiStore 同步：全局快捷键（toggleAi）通过 uiStore 控制，
// 本组件点击竖条也走 ui.toggleAi，保证单一数据源。
const collapsed = computed({
  get: () => ui.aiCollapsed,
  set: (v) => ui.setAiCollapsed(v),
});
const COLLAPSED_WIDTH = 40;

/** 展开态宽度：按 domain 从 uiStore 取（拖拽调整后记忆）。 */
const panelWidth = computed(() =>
  collapsed.value ? COLLAPSED_WIDTH : ui.aiWidths[props.domain]
);

function toggle() {
  ui.toggleAi();
}

// --- 拖拽调整宽度 ---------------------------------------------------------
// 拖动面板左缘竖条改变宽度（写法沿用 SQL 控制台左侧树 sidebar-resizer 惯例：
// mousedown 后挂 document 级 mousemove/mouseup，期间锁定 body 光标与选区）。
const DEFAULT_WIDTH = 340;
const MIN_WIDTH = 240;

const dragging = ref(false);
let dragStartX = 0;
let dragStartWidth = 0;

function startResize(e: MouseEvent) {
  e.preventDefault();
  dragging.value = true;
  dragStartX = e.clientX;
  dragStartWidth = ui.aiWidths[props.domain];
  document.body.style.cursor = "col-resize";
  document.body.style.userSelect = "none";
  document.addEventListener("mousemove", onResizeMove);
  document.addEventListener("mouseup", onResizeEnd);
  // 鼠标在窗口外释放（拖出窗口边缘 / Alt-Tab 切走）时 mouseup 不触发，
  // 监听器与 col-resize 光标、userSelect:none 会永久残留——用 window blur 兜底清理。
  window.addEventListener("blur", onResizeEnd);
}

function onResizeMove(e: MouseEvent) {
  // 面板在右缘：向左拖（clientX 减小）→ 宽度增大。
  const delta = dragStartX - e.clientX;
  const max = Math.max(MIN_WIDTH, Math.round(window.innerWidth * 0.6));
  const w = Math.min(max, Math.max(MIN_WIDTH, dragStartWidth + delta));
  ui.setAiWidth(props.domain, w);
}

function onResizeEnd() {
  dragging.value = false;
  document.body.style.cursor = "";
  document.body.style.userSelect = "";
  document.removeEventListener("mousemove", onResizeMove);
  document.removeEventListener("mouseup", onResizeEnd);
  window.removeEventListener("blur", onResizeEnd);
}

/** 双击竖条：恢复默认宽度。 */
function resetWidth() {
  ui.setAiWidth(props.domain, DEFAULT_WIDTH);
}

onBeforeUnmount(() => {
  if (dragging.value) onResizeEnd();
  if (composerHeightDragging.value) onComposerResizeEnd();
});

// --- 模式 ----------------------------------------------------------------
// 按域定制：SSH 域聚焦服务器运维，DB 域聚焦数据库，desktop 域聚焦 RDP 桌面操作。
// 三套模式与提示词独立。
type SshMode = "chat" | "translate" | "diagnose" | "explain" | "agent";
type DbMode = "chat" | "optimize" | "explain" | "agent";
type DesktopMode = "chat" | "agent";
type Mode = SshMode | DbMode | DesktopMode;

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
    "你是一名资深数据库 DBA 助手（按用户当前连接的 MySQL / PostgreSQL 方言作答），" +
    "专注于 SQL 优化、表结构设计、索引、事务、性能调优。" +
    "回答简洁专业；SQL 用 markdown ```sql 代码块给出。",
  optimize:
    "用户会提供一段 SQL（MySQL / PostgreSQL 方言以当前连接为准）。请给出优化建议：索引、重写、执行计划推测。" +
    "优化后的 SQL 用 ```sql 代码块给出，并附简短说明。",
  explain:
    "用户会提供一段 SQL 或查询结果（MySQL / PostgreSQL 方言以当前连接为准）。请用通俗简洁的中文解释其含义、潜在问题。",
  agent:
    "你是一名可执行操作的数据库智能体。你可以调用工具在用户的数据库（MySQL / PostgreSQL，以当前连接为准）上执行 SQL" +
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

const DESKTOP_PROMPTS: Record<DesktopMode, string> = {
  chat:
    "你是一名 Windows 桌面操作助手，熟悉 Windows 系统、常用软件操作与故障排查。" +
    "回答简洁专业；涉及具体操作时给出清晰步骤。",
  agent:
    "你是一名可操作远程 Windows 桌面（RDP）的智能体。你可以调用工具查看桌面截图" +
    "（desktop_screenshot）、点击（desktop_click）、输入文本（desktop_type）和发送按键" +
    "（desktop_key）来完成任务。\n" +
    "规则：\n" +
    "1. 动手前必须先调用 desktop_screenshot 查看当前界面，不要凭空猜测界面内容。\n" +
    "2. 每次只做一小步：截图 → 判断 → 一次点击/输入 → 再截图确认效果，然后决定下一步。\n" +
    "3. 点击坐标以最近一次截图的像素坐标系为准（左上角为 (0,0)，与截图同尺寸）。\n" +
    "4. 每一步都先告诉用户你要做什么，再调用工具；点击/输入会经用户确认后执行。\n" +
    "5. 输入普通文本用 desktop_type（先确认焦点在目标输入框上）；快捷键、回车、\n" +
    "   方向键等组合输入用 desktop_key（code 如 Enter / F5 / KeyA，修饰键如 ControlLeft）。\n" +
    "6. 无法确定如何操作或操作有风险（删除数据、修改系统设置等）时，先向用户说明再动手。\n" +
    "7. 完成后用中文简洁总结。",
};

const SYSTEM_PROMPTS = (
  props.domain === "ssh" ? SSH_PROMPTS : props.domain === "db" ? DB_PROMPTS : DESKTOP_PROMPTS
) as Record<Mode, string>;

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
const DESKTOP_MODES: { label: string; value: DesktopMode }[] = [
  { label: "智能体", value: "agent" },
  { label: "对话", value: "chat" },
];
const MODE_OPTIONS =
  props.domain === "ssh" ? SSH_MODES : props.domain === "db" ? DB_MODES : DESKTOP_MODES;

const mode = ref<Mode>("agent");

/** 面板标题：按域显示。 */
const panelTitle = computed(() =>
  props.domain === "ssh" ? "终端助手" : props.domain === "db" ? "数据库助手" : "桌面助手"
);

/** 当前活动会话的任务清单（todo_write 工具最新整表；无则空数组）。 */
const activeTodos = computed(() => ai.activeConversation?.todos ?? []);

/** 任务清单统计（完成数/总数/完成百分比）。 */
const todoStats = computed(() => {
  const total = activeTodos.value.length;
  const completed = activeTodos.value.filter((t) => t.status === "completed").length;
  return {
    total,
    completed,
    pct: total > 0 ? Math.round((completed / total) * 100) : 0,
  };
});

/** 任务清单条折叠状态（项数超过阈值时可手动收起）。 */
const todoCollapsed = ref(false);
/** 超过该条数时显示折叠按钮。 */
const TODO_COLLAPSE_THRESHOLD = 6;

/** 当前活动会话最近一次模型请求的 token 用量（ai:usage 事件覆盖；无则 0）。 */
const activeUsage = computed(() => ai.activeConversation?.usage ?? { prompt: 0, completion: 0 });

/** 用量展示文本："1.2k / 3M tokens"（千分位友好；整数千位不带小数，避免 "1.0k"）。 */
function formatTokens(n: number): string {
  if (n < 1000) return String(n);
  const fmt = (v: number) => (Number.isInteger(v) ? String(v) : v.toFixed(1));
  if (n < 1_000_000) return `${fmt(n / 1000)}k`;
  return `${fmt(n / 1_000_000)}M`;
}

/** 系统注入的提醒消息（如重复调用守卫的 [重复调用提醒]）：以灰色提示渲染，
 *  不伪装成用户自己发送的消息。 */
function isSystemReminder(m: AiMessage): boolean {
  return m.role === "user" && m.content.startsWith("[重复调用提醒]");
}

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

// --- 会话级终端绑定（手动指定 AI 操作哪台终端） ----------------------------
// 痛点：默认跟随"当前活动终端"——对话期间切换终端 tab，AI 的操作对象跟着
// 漂移（上一条命令在 A 执行、下一条跑到了 B）。绑定后整个会话固定操作该
// 终端，不随 tab 切换变化；"auto" = 维持跟随行为。
// 按对话独立记忆（组件内存，不持久化——重启后回到跟随模式）。
const AUTO_TERMINAL = "auto";
const boundTerminals = ref<Record<string, string>>({});

/** 仅终端页签：监控页签虽在同一 tab 列表，但没有终端实例，AI 无法在其上执行
 *  命令，故绑定下拉 / 上下文解析 / 跟随目标一律排除。 */
const terminalTabs = computed(() => terminals.tabs.filter((t) => t.kind === "terminal"));

/**
 * 当前对话的绑定值（"auto" 或终端 tab 的**稳定 id**）。
 * 用 tab.id 而非 instanceId：重连会更换 instanceId，稳定 id 让绑定在
 * 断开→重连后依然有效；tab 被关闭后才回退 auto。断开状态允许绑定
 * （重连后继续生效，选项上标记）。
 */
const boundTerminalId = computed(() => {
  const bound = boundTerminals.value[ai.activeCid ?? ""];
  if (bound && bound !== AUTO_TERMINAL) {
    if (terminalTabs.value.some((t) => t.id === bound)) return bound;
  }
  return AUTO_TERMINAL;
});

/**
 * 发送时快照的终端 instanceId：响应期间 resolvedTerminalTab 固定用它。
 *
 * 痛点修复：agent 模式下后端一轮请求内所有工具调用都锁定在**发送时**传入的
 * sessionId（ai_chat 入参只带一次）。若响应期间用户切了 tab，auto 跟随的
 * computed 会漂到新终端——面板提示显示新终端，而 AI 实际操作的是发送时的
 * 旧终端，两者不一致（用户感知"AI 用了旧终端"）。锁定后响应期间提示与
 * 实际操作一致；响应结束（ai.sending=false）自动恢复跟随新活动终端。
 */
const sentTerminalId = ref<string | null>(null);

// 响应结束（sending true→false）显式清空快照：resolvedTerminalTab 的锁定分支
// 本就会因 ai.sending=false 跳过，这里兜底清掉残留值，杜绝任何路径读到旧终端。
watch(
  () => ai.sending,
  (sending) => {
    if (!sending) sentTerminalId.value = null;
  },
);

// 响应期间用户切换终端 tab：AI 本轮仍操作发送时的终端，提示用户避免误判
// （实际锁定已由 resolvedTerminalTab 的 sentTerminalId 分支保证，这里只告知）。
watch(
  () => terminals.activeId,
  (id, old) => {
    if (
      id &&
      id !== old &&
      ai.sending &&
      sentTerminalId.value &&
      id !== sentTerminalId.value
    ) {
      ElMessage.info("AI 响应中，本轮仍操作发送时的终端（响应结束后自动跟随）");
    }
  },
);

/** 绑定 / 跟随的目标 tab（绑定 tab 未连接或已断开未重连时为 null——
 *  断开 tab 的 instanceId 指向后端已销毁的会话，注入上下文只会让 AI 每轮
 *  exec_ssh 必败）。 */
const resolvedTerminalTab = computed(() => {
  // 响应期间：锁定发送时的终端，不随 tab 切换 / activeId 变化漂移
  // （中途断开也保持——本轮后端本就以该 sessionId 执行，断开由执行错误呈现）。
  if (ai.sending && sentTerminalId.value) {
    return terminalTabs.value.find((t) => t.instanceId === sentTerminalId.value) ?? null;
  }
  if (boundTerminalId.value === AUTO_TERMINAL) {
    const id = activeTerminalId.value;
    const t = id ? terminalTabs.value.find((x) => x.instanceId === id) : undefined;
    return t && !t.disconnected ? t : null;
  }
  const t = terminalTabs.value.find((x) => x.id === boundTerminalId.value);
  return t && !t.disconnected ? t : null;
});

/** 可绑定的终端选项（仅终端页签；断开的标记，重连后绑定自动恢复）。 */
const terminalOptions = computed(() =>
  terminalTabs.value.map((t, i) => ({
    id: t.id,
    label:
      (terminalOptions_dup(t.session.name) ? `${t.session.name} #${i + 1}` : t.session.name) +
      (t.disconnected ? "（已断开）" : t.instanceId ? "" : "（连接中…）"),
  })),
);
function terminalOptions_dup(name: string): boolean {
  return terminalTabs.value.filter((t) => t.session.name === name).length > 1;
}

function setBoundTerminal(v: string) {
  // 响应期间锁定终端绑定：AI 本轮正按发送时的终端操作，中途切换会造成
  // 提示与实际操作不一致（且后端单轮内不会改用新 sessionId）。
  if (ai.sending) {
    ElMessage.warning("AI 响应中，暂不能切换终端（响应结束后可切换）");
    return;
  }
  const cid = ai.activeCid;
  if (!cid) return;
  if (v === AUTO_TERMINAL) delete boundTerminals.value[cid];
  else boundTerminals.value[cid] = v;
}

/** 发送时实际使用的终端 instanceId：绑定优先（需已连接），未绑定跟随活动终端。 */
const resolvedTerminalId = computed(() => resolvedTerminalTab.value?.instanceId || null);
/** agent 模式当前可用的活动内嵌 RDP 会话（仅 rdp 协议；VNC 不注册控制句柄）。 */
const activeDesktopId = computed(() => {
  const tab = desktopTabs.tabs.find((t) => t.instanceId === desktopTabs.activeId);
  return tab && tab.protocol === "rdp" ? tab.instanceId : null;
});
/** agent 模式上下文提示文字（按域显示）。 */
const contextTip = computed(() => {
  if (mode.value !== "agent") return "";
  const parts: string[] = [];
  if (props.domain === "ssh") {
    if (resolvedTerminalId.value) {
      const tab = terminals.tabs.find((t) => t.instanceId === resolvedTerminalId.value);
      if (tab) {
        // 响应期间 AI 固定操作发送时的终端：标注"本轮固定"，避免用户误以为
        // 已跟随到新切换的 tab（发送后 contextTip 立即显示锁定结果）。
        parts.push(
          ai.sending && sentTerminalId.value
            ? `${tab.session.name}（本轮固定）`
            : boundTerminalId.value === AUTO_TERMINAL
              ? `${tab.session.name}（跟随当前）`
              : `${tab.session.name}（已绑定）`,
        );
      }
    }
  } else if (props.domain === "db") {
    if (db.activeConnId) {
      const tab = db.activeTab;
      parts.push(`数据库: ${tab?.profileName ?? db.activeConnId}`);
      // 追加当前绑定库（点库/表或拖表时设置）。
      if (db.activeDatabase) parts.push(`库: ${db.activeDatabase}`);
    }
  } else {
    // desktop 域
    const tab = activeDesktopId.value
      ? desktopTabs.tabs.find((t) => t.instanceId === activeDesktopId.value)
      : undefined;
    if (tab) parts.push(`桌面: ${tab.name}`);
  }
  // 本地文件读写：仅 ssh/db 域启用（桌面助手不下发文件工具），未设置则提示。
  if (settings.fileAccess.enabled && props.domain !== "desktop") {
    const dir = settings.fileAccess.workspaceDirs[props.domain as "ssh" | "db"];
    parts.push(dir ? `文件: ${dir}` : "⚠ 文件读写已开启，未设置工作目录");
  }
  if (props.domain === "ssh") {
    if (parts.length) return parts.join("、");
    // 当前页签是服务器监控（与终端同一 tab 列表）时，别把原因说成"未连接"——
    // 用户可能开着终端，只是停在了监控页签上。
    const onMonitorTab = terminals.tabs.some(
      (t) => t.kind === "monitor" && t.id === terminals.activeTabId,
    );
    return onMonitorTab
      ? "当前页签是服务器监控，不是终端。请切到终端页签，或用上方下拉绑定一个终端"
      : "无可用活动终端（未连接或已断开），请连接/重连后 AI 才能操作";
  }
  if (props.domain === "db") {
    return parts.length
      ? `已附加: ${parts.join(" · ")}`
      : "未连接数据库，请先在 SQL 控制台连接后 AI 才能操作";
  }
  return parts.length
    ? `已附加: ${parts.join(" · ")}`
    : "未打开内嵌 RDP 桌面，请先在左侧列表连接后 AI 才能操作";
});

// --- 桌面工具执行器（仅 desktop 域） ---------------------------------------
// desktop_* 工具的执行体在前端 RDP 会话里（见 @/api/desktopControl）：
// 注册到 store，批准即在本面板的执行器里截图/点击/输入，再把回执发给后端。
// 目标桌面取自 tool_call 事件携带的 desktopId（请求发起时的活动桌面）——
// 不用执行瞬间的 activeId，避免 agent 循环期间用户切换标签导致操作落到错误桌面。
onMounted(() => {
  if (props.domain !== "desktop") return;
  ai.setDesktopToolExecutor(async (toolCallId, approved, name, args, desktopId) => {
    if (!approved) {
      // 拒绝：把 approved=false 回执发给后端（无执行结果）。
      await aiDesktopToolRespond(toolCallId, false).catch(() => {
        /* 后端可能已因超时/终止移除等待项，忽略 */
      });
      return;
    }
    const control = desktopId ? getDesktopControl(desktopId) : undefined;
    const result = await executeDesktopTool(name, args, control);
    await aiDesktopToolRespond(toolCallId, true, result).catch(() => {
      /* 同上，忽略（ai:tool_result 事件不会再来，卡片由 onToolResult 状态兜底） */
    });
  });
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

// --- 底部工具行：模型选择 + 模式选择 + 执行模式 ------------------------------
/** 模型下拉选项：只显示模型名（协议类型对使用者无意义，设置页可查）。 */
const modelOptions = computed(() =>
  settings.aiProviders.map((p) => ({
    value: `${p.kind}:${p.model}`,
    label: p.model,
  })),
);

/** 本域可用技能列表（技能按钮下拉用；启用+禁用都列出，禁用项灰色标注）。 */
const domainSkills = computed(() => settings.skills.filter((s) => s.domain === props.domain));

// --- 执行模式图标（面板过窄时选择框只显示图标，title 提示全名） ----------------
/** 三种执行模式的图标：手动=锁（逐条解锁确认），白名单=钥匙（放行名单内命令），自动=闪电（全自动直通）。 */
const RUN_MODE_ICONS = {
  manual: "Lock",
  whitelist: "Key",
  auto: "Lightning",
} as const;
/** 面板过窄（< 360px）时执行模式选择框切到纯图标显示，省出横向空间防换行。 */
const compactRunMode = computed(() => panelWidth.value < 360);
/** 当前执行模式全名（图标模式下的 title 提示）。 */
const runModeLabel = computed(
  () => RUN_MODE_OPTIONS.find((o) => o.value === runMode.value)?.label ?? "",
);

/** 已选技能（输入框上方简短显示，随问题一起发给 AI；可单个移除）。 */
const selectedSkills = ref<SkillConfig[]>([]);

/** 下拉选择：已选过则忽略，未选则追加到已选列表（不插入输入框文本）。 */
function toggleSkill(s: SkillConfig) {
  const i = selectedSkills.value.findIndex((x) => x.id === s.id);
  if (i >= 0) {
    selectedSkills.value.splice(i, 1);
  } else {
    selectedSkills.value.push(s);
  }
}

function removeSelectedSkill(id: string) {
  selectedSkills.value = selectedSkills.value.filter((x) => x.id !== id);
}

/** 技能下拉命令分发：__manage__ 打开管理弹窗，其余按 id 切换勾选。 */
function onSkillCommand(cmd: string) {
  if (cmd === "__manage__") {
    skillManagerVisible.value = true;
    return;
  }
  const s = domainSkills.value.find((x) => x.id === cmd);
  if (s) toggleSkill(s);
}

/** 当前激活模型 key（`${kind}:${model}`）。 */
const activeModel = computed({
  get: () => settings.aiActive,
  set: async (v: string | null) => {
    if (!v) return;
    settings.aiActive = v;
    await settings.save().catch(() => {});
  },
});

/** 本域的执行模式绑定（ssh 域 → sshAgent.runMode；db 域 → sqlAgent.runMode）。
 *  desktop 域无后端执行工具，不显示。切换即时持久化。 */
const runModeTarget =
  props.domain === "ssh" ? settings.sshAgent : props.domain === "db" ? settings.sqlAgent : null;
const showRunMode = computed(() => runModeTarget !== null);
const runMode = computed({
  get: () => runModeTarget?.runMode ?? "manual",
  set: async (v: ToolRunMode) => {
    if (!runModeTarget) return;
    runModeTarget.runMode = v;
    await settings.save().catch(() => {});
  },
});

// --- 多模态（图片输入） ---------------------------------------------------
/** 当前激活模型（按 `${kind}:${model}` 匹配）。 */
const activeProvider = computed(
  () =>
    settings.aiProviders.find((p) => `${p.kind}:${p.model}` === settings.aiActive) ?? null
);
/** 激活模型是否开启了多模态：开启后才显示图片上传入口。 */
const multimodalEnabled = computed(() => activeProvider.value?.multimodal ?? false);

/** 一张待发送的图片。 */
interface AttachedImage {
  id: string;
  name: string;
  mimeType: string;
  dataBase64: string;
}
/** 待发送图片列表（发送后清空）。 */
const attachedImages = ref<AttachedImage[]>([]);

/** 激活模型切到非多模态：清空待发送图片（避免残留导致发送被拦）。 */
watch(multimodalEnabled, (v) => {
  if (!v && attachedImages.value.length > 0) {
    attachedImages.value = [];
    ElMessage.info("当前模型不支持多模态，已清空待发送图片");
  }
});
/** 正在读取图片文件（弹窗选择 / 拖入 / 粘贴）。 */
const imageReading = ref(false);
/** 单次最多附带的图片数。 */
const MAX_IMAGES = 4;
/** 单张图片大小上限（10MB，防止请求体过大）。 */
const MAX_IMAGE_BYTES = 10 * 1024 * 1024;

/** 生成图片的 data URL（渲染预览用）。 */
function imageDataUrl(img: Pick<ImagePart, "mimeType" | "dataBase64">): string {
  return `data:${img.mimeType};base64,${img.dataBase64}`;
}

// --- 大图查看器（点击消息缩略图打开） -------------------------------------
/** 查看器是否打开。 */
const viewerVisible = ref(false);
/** 查看器图片列表（同一消息的所有图，可左右切换）。 */
const viewerUrls = ref<string[]>([]);
/** 打开时定位到的图片下标。 */
const viewerIndex = ref(0);

/** 点击消息里的缩略图：用该消息的全部图片打开查看器，并定位到点击的这张。 */
function previewImages(m: AiMessage, index: number) {
  if (!m.images?.length) return;
  viewerUrls.value = m.images.map(imageDataUrl);
  viewerIndex.value = index;
  viewerVisible.value = true;
}

/** 按文件名后缀推断 MIME 类型（取不到时默认 image/png）。 */
function mimeFromName(name: string): string {
  const ext = name.split(".").pop()?.toLowerCase() ?? "";
  const map: Record<string, string> = {
    png: "image/png",
    jpg: "image/jpeg",
    jpeg: "image/jpeg",
    gif: "image/gif",
    webp: "image/webp",
    bmp: "image/bmp",
  };
  return map[ext] ?? "image/png";
}

/** 校验并追加一张图片（大小/数量上限），返回是否成功。 */
function addImage(img: Omit<AttachedImage, "id">): boolean {
  if (attachedImages.value.length >= MAX_IMAGES) {
    ElMessage.warning(`最多附带 ${MAX_IMAGES} 张图片`);
    return false;
  }
  attachedImages.value.push({ ...img, id: `img-${Date.now()}-${Math.random().toString(36).slice(2, 6)}` });
  return true;
}

/** 移除一张待发送图片。 */
function removeImage(id: string) {
  attachedImages.value = attachedImages.value.filter((i) => i.id !== id);
}

/** 图片选择按钮：弹系统文件对话框，读取并附加。 */
async function pickImage() {
  if (imageReading.value) return;
  const selected = await open({
    title: "选择图片",
    multiple: true,
    filters: [
      { name: "图片", extensions: ["png", "jpg", "jpeg", "gif", "webp", "bmp"] },
    ],
  });
  if (!selected) return;
  const paths = Array.isArray(selected) ? selected : [selected];
  imageReading.value = true;
  try {
    for (const p of paths) {
      const bytes = await readFile(p);
      if (bytes.byteLength > MAX_IMAGE_BYTES) {
        ElMessage.warning(`${p.split(/[\\/]/).pop()} 超过 10MB，已跳过`);
        continue;
      }
      const name = p.split(/[\\/]/).pop() ?? "image";
      addImage({ name, mimeType: mimeFromName(name), dataBase64: bytesToBase64(bytes) });
    }
  } catch (e) {
    ElMessage.error("读取图片失败：" + String(e));
  } finally {
    imageReading.value = false;
  }
}

/** File（粘贴 / 拖入产生）→ 图片附加。 */
async function attachFile(file: File) {
  if (!file.type.startsWith("image/")) return;
  if (file.size > MAX_IMAGE_BYTES) {
    ElMessage.warning(`${file.name} 超过 10MB，已跳过`);
    return;
  }
  try {
    const dataUrl = await new Promise<string>((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(reader.result as string);
      reader.onerror = () => reject(reader.error);
      reader.readAsDataURL(file);
    });
    // data URL 形如 "data:image/png;base64,...."，拆成 mime + base64 两部分。
    const comma = dataUrl.indexOf(",");
    const mime = dataUrl.slice(5, comma).split(";")[0] || file.type || "image/png";
    addImage({ name: file.name, mimeType: mime, dataBase64: dataUrl.slice(comma + 1) });
  } catch {
    ElMessage.error("读取图片失败：" + file.name);
  }
}

/** 输入框粘贴：剪贴板含图片时附加（不拦截纯文本粘贴）。
 *  仅多模态模型支持图片输入：未开启时不做任何拦截，让默认行为生效
 *  （剪贴板里的文本照常粘贴，图片被忽略）。 */
function onComposerPaste(e: ClipboardEvent) {
  const items = e.clipboardData?.items ?? [];
  const imageItem = Array.from(items).find((it) => it.type.startsWith("image/"));
  const file = imageItem?.getAsFile();
  if (file) {
    if (!multimodalEnabled.value) {
      ElMessage.warning("当前模型未开启多模态，不支持粘贴图片（可在设置中开启）");
      return;
    }
    e.preventDefault();
    void attachFile(file);
  }
}

/** 拖入文件（图片）：与拖表共用 drop 通道，互不干扰。
 *  仅多模态模型支持拖入图片：未开启时忽略图片（一次提示），不阻断拖表。 */
function onComposerDropFiles(e: DragEvent) {
  const files = Array.from(e.dataTransfer?.files ?? []);
  if (!multimodalEnabled.value && files.some((f) => f.type.startsWith("image/"))) {
    ElMessage.warning("当前模型未开启多模态，不支持拖入图片（可在设置中开启）");
    return;
  }
  for (const f of files) void attachFile(f);
}

// --- 输入 / 发送 ---------------------------------------------------------
const inputText = ref("");
const scrollbarRef = ref();
const inputRef = ref();

// --- 输入框高度拖拽（拖动 composer 上边缘调整；仅放大，拖回自然高度恢复自适应） ---
const composerRef = ref<HTMLElement | null>(null);
/** 自定义高度（px）；null = 内容自适应（默认，autosize 2~5 行）。 */
const composerH = ref<number | null>(null);
const composerHeightDragging = ref(false);
let dragStartY = 0;
let dragStartComposerH = 0;
/** 当前自然高度缓存：允许用户从自定义高度逐步下拖缩回，直到自然高才恢复 auto。 */
let naturalComposerH = 0;

function startComposerResize(e: MouseEvent) {
  const el = composerRef.value;
  if (!el) return;
  e.preventDefault();
  composerHeightDragging.value = true;
  dragStartY = e.clientY;
  if (composerH.value == null) {
    naturalComposerH = el.getBoundingClientRect().height;
  }
  dragStartComposerH = composerH.value ?? naturalComposerH;
  document.body.style.cursor = "row-resize";
  document.body.style.userSelect = "none";
  document.addEventListener("mousemove", onComposerResizeMove);
  document.addEventListener("mouseup", onComposerResizeEnd);
  // 鼠标在窗口外释放（拖出窗口边缘 / Alt-Tab 切走）时 mouseup 不触发，
  // 监听器与 row-resize 光标、userSelect:none 会永久残留——用 window blur 兜底清理。
  window.addEventListener("blur", onComposerResizeEnd);
}

function onComposerResizeMove(e: MouseEvent) {
  const el = composerRef.value;
  if (!el) return;
  // 向上拖（clientY 减小）→ 输入区变高；最大不超面板高度的 60%。
  const panel = el.closest(".ai-panel") as HTMLElement | null;
  const maxH = panel ? Math.round(panel.clientHeight * 0.6) : 600;
  const h = dragStartComposerH + (dragStartY - e.clientY);
  if (h <= naturalComposerH) {
    // 拖回自然高度及以下：恢复内容自适应。
    composerH.value = null;
  } else {
    composerH.value = Math.min(maxH, Math.round(h));
  }
}

function onComposerResizeEnd() {
  composerHeightDragging.value = false;
  document.body.style.cursor = "";
  document.body.style.userSelect = "";
  document.removeEventListener("mousemove", onComposerResizeMove);
  document.removeEventListener("mouseup", onComposerResizeEnd);
  window.removeEventListener("blur", onComposerResizeEnd);
}

/** 双击拖拽条：恢复内容自适应。 */
function resetComposerH() {
  composerH.value = null;
}

// --- 会话标签栏：溢出时活动标签自动滚入视野 -------------------------------
const convTabsRef = ref<HTMLElement | null>(null);
// 切换/新建会话后，活动标签可能落在横向滚动区外（标签收缩 + 滚动方案下），
// 平滑滚到最近可见位置，保证用户始终看得到当前会话。
watch(
  () => ai.activeCid,
  async (cid) => {
    if (!cid) return;
    await nextTick();
    convTabsRef.value
      ?.querySelector(".conv-tab.active")
      ?.scrollIntoView({ behavior: "smooth", inline: "nearest", block: "nearest" });
  }
);

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
  // 拖表也算选中该表所在库（与点表行为一致）：把活动标签的绑定库切过去并自动
  // USE，让助手关联到正确库（后端同步，后续 AI 执行的 SQL 落在这个库上）。
  // USE 失败必须中止附加：注入的 prompt 声称"当前库为 X、已自动 USE"，
  // 若后端切换失败还继续附加，AI 的写操作会落在错误的 schema 上。
  if (props.domain === "db" && payload.database && db.activeTabId) {
    try {
      await db.useDatabase(db.activeTabId, payload.database);
    } catch (e) {
      ElMessage.error(`切换到库 "${payload.database}" 失败：${e}`);
      return;
    }
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

/** composer 的 drop 处理：拖表（附加表结构）或拖图片文件（多模态）。 */
function onComposerDrop(e: DragEvent) {
  e.preventDefault();
  dragOver.value = false;
  onComposerDropFiles(e);
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
  // 携带表数据（拖表附加）或图片文件（多模态）时才允许 drop。
  const types = Array.from(e.dataTransfer?.types ?? []);
  const hasTable = types.includes("application/x-xterm-table");
  // 图片仅在多模态模型下可拖入（非多模态不给"可放置"反馈，避免误导）。
  const hasImageFile =
    multimodalEnabled.value &&
    Array.from(e.dataTransfer?.files ?? []).some((f) => f.type.startsWith("image/"));
  if (hasTable || hasImageFile) {
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

// 切换数据库连接（关闭标签/切 tab/断开）时清空附加表：表结构属于旧连接，
// 残留会把 A 连接的表 DDL 拼进 B 连接的系统提示词，误导 AI 在错误库上下文中作答。
watch(
  () => (props.domain === "db" ? db.activeConnId : undefined),
  () => clearAttachedTables(),
);

async function scrollToBottom() {
  await nextTick();
  const wrap = scrollbarRef.value?.wrapRef as HTMLElement | undefined;
  if (wrap) wrap.scrollTop = wrap.scrollHeight;
}

/** 滚动跟随阈值（px）：距底部小于该距离视为"在底部"，流式内容才自动跟随。 */
const FOLLOW_THRESHOLD = 48;

/** 用户是否位于底部附近（上翻浏览历史输出后返回 false，流式不再强制拉回）。 */
function isNearBottom(): boolean {
  const wrap = scrollbarRef.value?.wrapRef as HTMLElement | undefined;
  if (!wrap) return true;
  return wrap.scrollHeight - wrap.scrollTop - wrap.clientHeight < FOLLOW_THRESHOLD;
}

// 消息数量/内容变化（流式增长）时：仅当用户本就位于底部附近才跟随滚动，
// 上翻浏览历史输出时允许内容继续增长而不打断阅读位置；用户主动发送则强制滚动。
watch(() => ai.messages.length, () => {
  if (isNearBottom()) void scrollToBottom();
});
// 内容变化（流式增长）时同样跟随。只看最后一条消息的长度而非全量 join：
// join 每个 chunk 都会拼接整段会话文本（长对话 O(总字符数)/token），改为
// watch 流式更新的目标——末条消息的 content.length，避免无谓的大字符串分配。
watch(
  () => ai.messages[ai.messages.length - 1]?.content.length ?? 0,
  () => {
    if (isNearBottom()) void scrollToBottom();
  }
);

// --- 输入历史（上下方向键填充） ------------------------------------------
/** 已发送过的输入（旧→新），上限 50 条；按面板域独立记忆（会话内共享）。 */
const sendHistory = ref<string[]>([]);
/** 历史浏览游标：-1 = 正常编辑态；>=0 指向 sendHistory 索引。 */
const historyIndex = ref(-1);
/** 进入历史浏览时保存的当前草稿（按向下可恢复）。 */
const historyDraft = ref("");

/** 发送成功后记录输入（相邻去重，置顶由记录顺序保证）。 */
function recordHistory(text: string) {
  const t = text.trim();
  if (!t) return;
  if (sendHistory.value[sendHistory.value.length - 1] !== t) {
    sendHistory.value.push(t);
    if (sendHistory.value.length > 50) sendHistory.value.shift();
  }
  historyIndex.value = -1;
  historyDraft.value = "";
}

/** 把光标移到输入框末尾（Vue 更新 DOM 后执行）。 */
function moveCursorToEnd(el: HTMLTextAreaElement | null) {
  nextTick(() => {
    if (!el) return;
    el.focus();
    el.setSelectionRange(el.value.length, el.value.length);
  });
}

/** 切换对话：切到目标会话并滚到底部（避免残留上一个会话的滚动位置）。 */
function onSwitchConversation(cid: string) {
  ai.switchConversation(cid);
  // 切换会话重置折叠（新会话从底部看最新内容）。
  olderExpanded.value = false;
  void scrollToBottom();
}

// --- 历史会话归档（关闭的会话移入，可恢复或彻底删除） ----------------------
/** 历史面板（el-popover 实例引用：恢复会话后手动关闭面板）。 */
const historyPopRef = ref();

/** 归档列表按归档时间倒序展示（旧持久化数据无 archivedAt 的排最后）。 */
const archiveList = computed(() =>
  [...ai.archives].sort((a, b) => (b.archivedAt ?? 0) - (a.archivedAt ?? 0)),
);

/** 相对时间：刚刚 / x 分钟前 / x 小时前 / x 天前 / 具体日期。 */
function fmtRelTime(ts?: number): string {
  if (!ts) return "";
  const diff = Date.now() - ts;
  if (diff < 60_000) return "刚刚";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)} 小时前`;
  if (diff < 7 * 86_400_000) return `${Math.floor(diff / 86_400_000)} 天前`;
  const d = new Date(ts);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/** 恢复历史会话：移回标签栏并激活（复用切换逻辑：重置折叠 + 滚到底部）。 */
function onRestoreConversation(cid: string) {
  ai.restoreConversation(cid);
  olderExpanded.value = false;
  void scrollToBottom();
  // 恢复后关闭历史面板，让用户直接看到恢复的会话。
  historyPopRef.value?.hide?.();
  ElMessage.success("已恢复会话");
}

/** 彻底删除归档会话（二次确认，不可恢复）。 */
function onDeleteConversation(cid: string, title: string) {
  ElMessageBox.confirm(`彻底删除会话「${title}」？删除后不可恢复。`, "删除历史会话", {
    type: "warning",
    confirmButtonText: "删除",
    cancelButtonText: "取消",
  })
    .then(() => {
      ai.deleteConversation(cid);
      ElMessage.success("已删除");
    })
    .catch(() => {
      /* 用户取消 */
    });
}

/** 清空全部历史归档（二次确认，不可恢复）。 */
function onClearArchives() {
  ElMessageBox.confirm(`清空全部 ${ai.archives.length} 条历史会话？删除后不可恢复。`, "清空历史会话", {
    type: "warning",
    confirmButtonText: "清空",
    cancelButtonText: "取消",
  })
    .then(() => {
      ai.clearArchives();
      ElMessage.success("已清空历史会话");
    })
    .catch(() => {
      /* 用户取消 */
    });
}

// --- 长对话折叠：只渲染最新 N 条，更早的消息折叠为一条提示 ----------
// 长对话 DOM 数百条（每条含 markdown/工具卡片）导致滚动与流式更新卡顿；
// 折叠后旧消息不进 DOM，展开按钮可临时查看全部（切换会话/发送新消息自动折叠）。
const KEEP_RECENT = 8;
const olderExpanded = ref(false);
/** 折叠时可见的窗口起点；-1 = 全量展示（消息不足或已展开）。 */
const visibleStart = computed(() => {
  if (olderExpanded.value) return -1;
  return Math.max(0, ai.messages.length - KEEP_RECENT);
});
/** 实际渲染的消息（折叠时只取最新 KEEP_RECENT 条）。 */
const visibleMessages = computed(() =>
  visibleStart.value < 0 ? ai.messages : ai.messages.slice(visibleStart.value),
);

/** 按 agent / 非 agent 模式构建系统提示与发送选项。
 *  handleSend 与 regenerateMessage 共用，保证"重新生成"的上下文与首次发送一致
 *  （agent 的动态上下文、附加表 DDL、skill 段落都走同一条路径）。 */
function buildSendContext(agent: boolean): {
  prompt: string;
  opts: {
    agent: boolean;
    activeTerminalId?: string;
    activeDbConnId?: string;
    activeDesktopId?: string;
    domain: string;
  };
} {
  if (!agent) {
    let prompt = SYSTEM_PROMPTS[mode.value];
    const ddlSection = buildAttachedDdlSection();
    if (ddlSection) prompt += ddlSection;
    // 非 agent 模式没有工具可调：技能正文全量注入，保证模型能用。
    prompt += buildSkillsSection(false);
    return { prompt, opts: { agent: false, domain: props.domain } };
  }

  // 动态构建系统提示：把当前活动上下文的真实 id 注入，让模型直接填对参数。
  // 按域裁剪：SSH 面板只传 terminalId（后端就只暴露 SSH 工具），
  // DB 面板只传 dbConnId（后端就只暴露 SQL 工具），
  // desktop 面板只传 desktopId（后端就只暴露桌面工具），实现工具集硬隔离。
  let prompt = SYSTEM_PROMPTS.agent;
  const ctxParts: string[] = [];
  // 当前时间上下文（借鉴 dsh time-context）：日志时间对比、证书过期、
  // 定时任务等场景依赖"现在是什么时候"。
  ctxParts.push(
    `当前时间：${new Date().toLocaleString("zh-CN", { hour12: false })}（本地时区）。`
  );
  let activeTerminal: string | undefined;
  let activeDb: string | undefined;
  let activeDesktop: string | undefined;
  if (props.domain === "ssh") {
    if (resolvedTerminalId.value) {
      const tab = terminals.tabs.find((t) => t.instanceId === resolvedTerminalId.value);
      const name = tab?.session.name ?? "未命名";
      // 用户手动绑定的终端在整个会话期间固定（对话/切 tab 不漂移）。
      const bindNote =
        boundTerminalId.value === AUTO_TERMINAL ? "当前活动" : "用户指定";
      ctxParts.push(
        `${bindNote} SSH 终端：sessionId="${resolvedTerminalId.value}"（${name}）。调用 exec_ssh / terminal_snapshot 时直接用这个 sessionId。`
      );
      activeTerminal = resolvedTerminalId.value;
    } else if (boundTerminalId.value !== AUTO_TERMINAL) {
      ctxParts.push(
        "用户绑定的终端尚未连接完成（或已断开未重连）。请告诉用户：绑定的终端未就绪，稍候重试或重新选择终端。不要调用任何工具。"
      );
    } else {
      // 当前停在监控页签（非终端）时给出可执行的指引，别让模型笼统地说
      // "请先连接终端"（用户可能本就有已连接的终端）。
      const onMonitorTab = terminals.tabs.some(
        (t) => t.kind === "monitor" && t.id === terminals.activeTabId,
      );
      ctxParts.push(
        onMonitorTab
          ? "当前页签是服务器监控，不是终端。请告诉用户：切换到终端页签，或在上方下拉里绑定一个终端后再操作。不要调用任何工具。"
          : "当前没有活动终端。请直接告诉用户：请先连接终端后再让我操作。不要调用任何工具。"
      );
    }
  } else if (props.domain === "db") {
    if (db.activeConnId) {
      const tab = db.activeTab;
      const name = tab?.profileName ?? "未命名";
      const isPg = db.activeKind === "postgres";
      const kindLabel = isPg ? "PostgreSQL" : "MySQL";
      ctxParts.push(
        `当前活动 ${kindLabel} 连接：dbConnId="${db.activeConnId}"（${name}）。` +
          `请书写 ${kindLabel} 方言的 SQL。调用 exec_sql / list_db_tables / describe_table 时直接用这个 dbConnId。`
      );
      activeDb = db.activeConnId;
      // 注入当前绑定库（点库/表或拖表时设置），让 AI 默认在该库 schema 下操作。
      if (db.activeDatabase) {
        if (isPg) {
          ctxParts.push(
            `当前数据库为 "${db.activeDatabase}"（该连接已绑定此库）。` +
              `执行 SQL 时直接引用表名（必要时用 "schema"."表名" 双引号限定）即可；` +
              `PostgreSQL 表名常带 schema 前缀（如 public.users），跨 schema 才需要限定。`
          );
        } else {
          ctxParts.push(
            `当前库（schema）为 "${db.activeDatabase}"，该连接已自动 USE 此库。` +
              `执行 SQL 时直接引用表名（如 \`表名\`）即可，不要加库前缀；` +
              `确需跨库时才用 \`${db.activeDatabase}\`.\`表名\` 限定。`
          );
        }
      }
    } else {
      ctxParts.push(
        "当前没有连接数据库。请直接告诉用户：请先在 SQL 控制台连接数据库后再让我操作。不要调用任何工具。"
      );
    }
  } else {
    // desktop 域：桌面工具依赖多模态模型（截图需视觉能力）。
    // 非多模态时后端同样不会下发桌面工具（activeDesktopId 被门控忽略），
    // 这里同步告知模型，避免它以为能调用工具却调不动。
    if (!multimodalEnabled.value) {
      ctxParts.push(
        "当前激活模型未开启多模态，无法使用桌面工具（截图需要视觉能力）。" +
          "请告诉用户：到「设置 → AI」中为当前模型开启多模态后重试。不要调用任何桌面工具。"
      );
    } else if (activeDesktopId.value) {
      const tab = desktopTabs.tabs.find((t) => t.instanceId === activeDesktopId.value);
      const name = tab?.name ?? "未命名";
      ctxParts.push(
        `当前活动内嵌 RDP 桌面：instanceId="${activeDesktopId.value}"（${name}）。` +
          `调用 desktop_screenshot / desktop_click / desktop_type / desktop_key 时直接操作这个桌面。`
      );
      activeDesktop = activeDesktopId.value;
    } else {
      ctxParts.push(
        "当前没有打开内嵌 RDP 桌面。请直接告诉用户：请先在桌面页打开一个 RDP 连接（内嵌模式）后再让我操作。不要调用任何工具。"
      );
    }
  }
  // 本地文件读写（设置页开启后注入，仅 ssh/db 域）：告知模型工作目录与工具用法。
  if (settings.fileAccess.enabled && props.domain !== "desktop") {
    const dir = settings.fileAccess.workspaceDirs[props.domain as "ssh" | "db"];
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
  // 已启用的可复用 skill：agent 模式下只注入目录摘要（正文经 load_skill 按需加载）。
  prompt += buildSkillsSection(true);
  // 任务清单指导（借鉴 dsh tool-todo）：多步任务用 todo_write 维护步骤清单。
  prompt +=
    "\n\n=== 任务清单 ===\n" +
    "多步骤任务开始时，先用 todo_write 工具列出全部步骤（status=pending）；" +
    "每完成/进行到一步就调用 todo_write 更新对应项状态（整表替换，携带完整清单）；" +
    "任务全部完成或放弃时调用 todo_write 传空数组 [] 清空清单。";
  // 向用户提问指导（借鉴 dsh tool-ask-user）：信息不足时直接问，不猜不编。
  prompt +=
    "\n\n=== 向用户提问 ===\n" +
    "当缺少继续执行的关键信息（端口、路径、目标主机、凭据名、方案取舍、需要用户确认等）时，" +
    "调用 ask_user_question 向用户提问，不要猜测或编造。问题要简明具体，可附选项让用户快速选择；" +
    "推荐项放在选项第一位并在 label 末尾加 (Recommended)；需要用户从多个候选中选择时" +
    "把选项列全并设 multi_select=true 允许多选。一次最多提问 4 个问题，不要连发多张提问卡片。";
  return {
    prompt,
    opts: {
      agent: true,
      activeTerminalId: activeTerminal,
      activeDbConnId: activeDb,
      activeDesktopId: activeDesktop,
      domain: props.domain,
    },
  };
}

async function handleSend() {
  const text = inputText.value.trim();
  // 图片模式下允许"只发图片不带文字"；但两者都空则不发。
  const images = attachedImages.value.map(({ mimeType, dataBase64 }) => ({
    mimeType,
    dataBase64,
  }));
  if ((!text && images.length === 0) || ai.sending || configBlocked.value) return;
  if (images.length > 0 && !multimodalEnabled.value) {
    ElMessage.warning("当前模型未开启多模态，无法附带图片（可在设置中开启）");
    return;
  }
  inputText.value = "";
  recordHistory(text);
  // 发送新消息回到最新窗口：旧消息重新折叠（用户关注新回复）。
  olderExpanded.value = false;
  const ctx = buildSendContext(mode.value === "agent");
  // 快照发送时的终端：agent 响应期间锁定它（不随 tab 切换漂移），
  // 响应结束（ai.sending=false）后自动恢复跟随当前活动终端。
  if (props.domain === "ssh" && ctx.opts.agent) {
    sentTerminalId.value = ctx.opts.activeTerminalId ?? null;
  }
  // 立即清空附加项（内容已快照进 images 与 ctx.prompt）：原先在 await
  // send() 之后才清理——agent 响应长达数十秒，期间用户新粘贴的图片/新勾选
  // 的技能/新拖入的表会被这段收尾误清。
  clearAttachedTables();
  attachedImages.value = [];
  selectedSkills.value = [];
  await ai.send(text, ctx.prompt, { ...ctx.opts, images });
  // 发送后强制跟随滚动到底部（用户主动发送，应看到自己的消息与回复开始；
  // 若此刻正在上翻浏览，也以发送为准回到最新位置）。
  void scrollToBottom();
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

/** 拼接已启用的 skill 段落（注入 system prompt）。无启用 skill 返回空串。
 *
 * 用户在下拉中显式选择的技能（selectedSkills）总是注入；其余已启用技能：
 * agent 模式只注入目录摘要（借鉴 deepseek-harness 的 tool-skill，技能多了
 * 之后全量注入会持续膨胀上下文，完整正文由模型按需调 load_skill 加载）；
 * 非 agent 模式（无工具可调）全量注入，保证技能内容对模型可见可用。 */
function buildSkillsSection(agent: boolean): string {
  // 显式选择的技能优先（即使全局 enabled=false 也注入——用户手动勾选即意图）。
  const picked = selectedSkills.value.filter((s) => s.domain === props.domain);
  const rest = settings.skills.filter(
    (s) =>
      s.domain === props.domain &&
      s.enabled &&
      !picked.some((p) => p.id === s.id),
  );
  if (picked.length === 0 && rest.length === 0) return "";
  const fullInject = (list: SkillConfig[]) =>
    list.map((s) => `【${s.title}】\n${s.content}`).join("\n\n");
  if (!agent) {
    // 非 agent：全部全量注入（无工具可调）。
    return (
      "\n\n=== 可复用技能（来自历史总结，处理同类任务时请遵循）===\n" +
      fullInject([...picked, ...rest])
    );
  }
  const parts: string[] = [];
  if (picked.length > 0) {
    parts.push(
      "\n\n=== 用户指定技能（处理本次任务时必须遵循）===\n" + fullInject(picked),
    );
  }
  if (rest.length > 0) {
    // 目录摘要：标题 + 内容开头（≤120 字），配合 load_skill 按需加载全文。
    const entries = rest.map((s) => {
      const preview = s.content.length > 120 ? s.content.slice(0, 120) + "…" : s.content;
      return `- ${s.title}：${preview}`;
    });
    parts.push(
      "\n\n=== 可用技能（来自历史总结，处理同类任务时请遵循）===\n" +
        entries.join("\n") +
        "\n需要某条技能的完整步骤/命令细节时，调用 load_skill 工具（name 用上述标题，必须完全一致）" +
        "加载全文；目录摘要已足够理解任务时不必加载，且不要重复加载同一技能。",
    );
  }
  return parts.join("");
}

// --- skill 总结与管理 -----------------------------------------------------
const summarizing = ref(false);
const skillDialogVisible = ref(false);
const editingSkill = ref<SkillConfig | null>(null);
const skillManagerVisible = ref(false);

/** 把当前会话总结成 skill：把对话文本喂给 AI，让它生成结构化标题+内容，用户编辑后保存。 */
async function handleSummarizeSkill() {
  const conv = ai.activeConversation;
  // 流式中不允许总结：send 会静默跳过，且读到的 lastMsg 是半截内容。
  if (!conv || conv.messages.length === 0 || summarizing.value || conv.sending) return;

  // 复用 handleExport 的渲染逻辑，把会话拼成纯文本。
  const transcript = conv.messages
    .map((m) => {
      const role = m.role === "user" ? "用户" : "助手";
      let block = `【${role}】\n${m.content}`;
      if (m.role === "assistant" && m.toolCalls) {
        for (const t of m.toolCalls) {
          block += `\n  （工具 ${t.name}: ${JSON.stringify(t.arguments)} → ${t.result?.ok ? "成功" : "失败"}）`;
        }
      }
      return block;
    })
    .join("\n\n");

  summarizing.value = true;
  // 用非 agent 模式发一条总结请求（不调工具，纯文本输出）。
  const prompt =
    "你是运维经验沉淀助手。请把以下运维对话总结成一条可复用的 skill，供未来处理同类任务时参考。\n" +
    "要求：\n" +
    "1. 提炼这类任务的标准处理步骤、关键命令/SQL、常见坑和注意事项。\n" +
    "2. 内容简洁实用，可直接作为系统提示词片段使用，控制在 500 字以内。\n" +
    "3. 第一行输出标题（不超过 20 字，不要加书名号或引号），第二行起输出 skill 内容。\n\n" +
    `=== 待总结的对话 ===\n${transcript}`;

  try {
    // 临时切到 chat 模式发请求（不进 agent 循环）。
    await ai.send("请总结这段对话为可复用 skill", prompt);
    // 等待本次请求的流式完成。必须用捕获的 conv（send 时活跃的会话）判断
    // sending，而不能用 ai.activeConversation——等待期间用户切走对话会让
    // activeConversation 变成别的会话，读取它的 sending/lastMsg 都会错。
    // 同理 rid 要读 conv.activeRequestId（send 同步把它写在本会话上），
    // 读全局 ai.activeRequestId 会拿到别的会话的 id 或 null：
    // null 时跳过轮询，把仍在流式中的半截消息当成总结结果。
    const rid = conv.activeRequestId;
    if (rid) {
      // 等待流式完成：watch 该会话的 sending 状态翻转为 false 即 resolve
      // （响应式回调，替代此前 500ms×600 次的轮询空转）；5 分钟兜底超时，
      // 防事件丢失时永久挂起。
      await new Promise<void>((resolve) => {
        const stop = watch(
          () => conv.sending,
          (sending) => {
            if (!sending) {
              stop();
              clearTimeout(timer);
              resolve();
            }
          }
        );
        const timer = setTimeout(() => {
          stop();
          resolve();
        }, 5 * 60 * 1000);
        // send 已同步收尾（如立即失败）时直接完成。
        if (!conv.sending) {
          stop();
          clearTimeout(timer);
          resolve();
        }
      });
    }
    const msgs = conv.messages;
    const lastMsg = msgs[msgs.length - 1];
    if (lastMsg && lastMsg.role === "assistant" && lastMsg.content.trim()) {
      const text = lastMsg.content.trim();
      // 解析：第一行为标题，其余为内容。
      const nlIdx = text.indexOf("\n");
      const title = nlIdx > 0 ? text.slice(0, nlIdx).trim() : "新技能";
      const content = nlIdx > 0 ? text.slice(nlIdx + 1).trim() : text;
      // 弹出编辑弹窗，用户确认后保存。
      editingSkill.value = {
        id: "",
        title,
        content,
        domain: props.domain,
        enabled: true,
      };
      skillDialogVisible.value = true;
    } else {
      ElMessage.warning("未能生成技能内容，请重试");
    }
  } catch (e) {
    ElMessage.error("总结失败: " + String(e));
  } finally {
    summarizing.value = false;
  }
}

/** SkillDialog 保存回调：新建或更新 skill。 */
async function onSkillSaved(s: SkillConfig) {
  if (s.id) {
    settings.updateSkill(s.id, { title: s.title, content: s.content, enabled: s.enabled });
  } else {
    settings.addSkill({
      title: s.title,
      content: s.content,
      enabled: s.enabled,
      domain: props.domain,
    });
  }
  await settings.save().catch(() => {});
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
  // 用消息生成时的 agent 模式（而非面板当前 mode）重建上下文，保证"重问"
  // 得到的上下文与首次发送一致；附加表 DDL / skill 段落同样按当前状态注入。
  const ctx = buildSendContext(m.agent === true);
  // 与 handleSend 一致：重生也是新请求，快照当前终端供响应期间锁定。
  if (props.domain === "ssh" && ctx.opts.agent) {
    sentTerminalId.value = ctx.opts.activeTerminalId ?? null;
  }
  try {
    await ai.regenerate(m.id, ctx.prompt, ctx.opts);
  } catch (e) {
    ElMessage.error("重生失败：" + String(e));
  }
}

/** 删除一条消息（用户请求或助手响应）。删除后从持久化历史中移除，
 *  后续请求重建上下文时不再包含该消息。 */
async function deleteMessage(m: AiMessage) {
  if (ai.sending || m.streaming) return;
  // 确认文案按消息角色区分，删除不可恢复需用户显式确认。
  const roleText = m.role === "user" ? "这条请求消息" : "这条响应消息";
  try {
    await ElMessageBox.confirm(`确定删除${roleText}？删除后不可恢复。`, "删除消息", {
      type: "warning",
      confirmButtonText: "删除",
      cancelButtonText: "取消",
    });
  } catch {
    return;
  }
  if (ai.deleteMessage(m.id)) {
    ElMessage.success("已删除");
  } else {
    ElMessage.warning("会话发送中，暂不能删除消息");
  }
}

async function approveTool(tool: ToolCallItem) {
  if (tool.status !== "pending") {
    // 点击时卡片已不是"待确认"（被终止/已处理）：明确告知而非静默无反应，
    // 否则用户会以为"点确定没反应"。
    ElMessage.warning(
      tool.status === "rejected"
        ? "该命令已被拒绝或请求已终止，无法再执行；请重新发送请求"
        : "该命令当前状态不可执行（可能已在执行或已结束）"
    );
    return;
  }
  const ok = await ai.approveToolCall(tool.toolCallId);
  if (!ok) {
    // 僵尸卡片（轮次已超时/请求已结束）或桌面工具执行失败（RDP 会话不可用
    // 等）：命令不会执行，明确告知而非让用户对着"执行中"的卡片干等。
    ElMessage.warning("命令未执行（确认已超时、请求已结束或执行失败）；请重试或重新发送请求");
  }
}

/** 加入白名单并执行：把命令前缀持久化到白名单，然后正常 approve。 */
async function addToWhitelistAndRun(tool: ToolCallItem) {
  if (tool.status !== "pending") return;
  const whitelisted = await ai.addToWhitelistAndApprove(tool.toolCallId);
  if (whitelisted) {
    ElMessage.success("已加入白名单并执行");
  } else {
    // 白名单写入失败但执行已批准：如实告知，避免"已加入白名单"的误导。
    ElMessage.warning("已执行，但加入白名单失败（可到设置页手动添加）");
  }
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

// --- ask_user_question 问题表单状态 -------------------------------------
/** 问题结构（与后端 ToolDef schema 对应）。 */
interface AskUserQuestion {
  id: string;
  question: string;
  header?: string | null;
  options?: { label: string; description?: string | null }[];
  multi_select?: boolean;
}

/** toolCallId -> 问题 id -> { 勾选标签, 自由输入 }（惰性初始化）。 */
const askUserSelections = ref<
  Record<string, Record<string, { selected: string[]; custom: string }>>
>({});

/** 取某张提问卡片某个问题的表单状态（首次访问惰性建表）。 */
function askSel(tool: ToolCallItem, qid: string) {
  const byQ = (askUserSelections.value[tool.toolCallId] ??= {});
  return (byQ[qid] ??= { selected: [], custom: "" });
}

/**
 * 从卡片 arguments 解析问题列表（onToolCall 已 parse 成对象）。
 * 防御性过滤：缺 id/question 的问题、label 为空的选项直接丢弃
 * （后端 plan_call 虽已预拒绝畸形结构，双保险避免渲染期 undefined key）。
 */
function askQuestions(tool: ToolCallItem): AskUserQuestion[] {
  const raw = Array.isArray(tool.arguments.questions) ? tool.arguments.questions : [];
  return raw
    .filter(
      (q): q is AskUserQuestion =>
        !!q &&
        typeof q.id === "string" &&
        q.id.trim() !== "" &&
        typeof q.question === "string" &&
        q.question.trim() !== ""
    )
    .map((q) => ({
      ...q,
      options: Array.isArray(q.options)
        ? q.options.filter((o) => o && typeof o.label === "string" && o.label.trim() !== "")
        : undefined,
    }));
}

/** 是否推荐项：提示词要求模型把推荐项放第一位并在 label 末尾加 (Recommended)。 */
function isRecommended(label: string): boolean {
  return /\(Recommended\)$/i.test(label.trim());
}

/** 选项展示文案：剥离 (Recommended) 后缀（由「推荐」标签替代，避免中英混杂）。 */
function optLabel(label: string): string {
  return label.trim().replace(/\s*\(Recommended\)$/i, "");
}

/**
 * 已提交回答的摘要 chips（勾选的选项 + 自由输入）。
 * 仅同会话内存中有记录时非空（重载恢复的历史卡片无记录，自然不显示）。
 */
function askAnswerChips(tool: ToolCallItem): string[] {
  const byQ = askUserSelections.value[tool.toolCallId];
  if (!byQ) return [];
  const chips: string[] = [];
  for (const q of askQuestions(tool)) {
    const st = byQ[q.id];
    if (!st) continue;
    chips.push(...st.selected.map(optLabel));
    const custom = st.custom.trim();
    if (custom) chips.push(custom);
  }
  return chips;
}

/** 单选/多选切换。 */
function toggleAskOption(tool: ToolCallItem, q: AskUserQuestion, label: string) {
  const st = askSel(tool, q.id);
  if (q.multi_select) {
    const i = st.selected.indexOf(label);
    if (i >= 0) st.selected.splice(i, 1);
    else st.selected.push(label);
  } else {
    st.selected = [label];
  }
}

/** 提交回答：组装 answers（dsh 规范字段）→ store 回传后端。 */
async function submitAskUser(tool: ToolCallItem) {
  if (tool.status !== "pending") return;
  const questions = askQuestions(tool);
  // 全空校验：一个问题都没填就提交，等于把空答案甩给模型（相当于无声跳过），
  // 更可能是用户漏填——给出提示而非直接提交（想跳过请点「取消」）。
  const allEmpty = questions.every((q) => {
    const st = askSel(tool, q.id);
    return st.selected.length === 0 && !st.custom.trim();
  });
  if (allEmpty) {
    ElMessage.warning("请至少回答一个问题，或点击「取消」跳过提问");
    return;
  }
  const answers: AskUserAnswer[] = questions.map((q) => {
    const st = askSel(tool, q.id);
    const custom = st.custom.trim();
    return { id: q.id, selected: [...st.selected], custom: custom || null };
  });
  await ai.answerAskUser(tool.toolCallId, answers);
}

/** 取消提问（后端以"用户取消"回填该轮）。 */
function cancelAskUser(tool: ToolCallItem) {
  if (tool.status !== "pending") return;
  ai.cancelAskUser(tool.toolCallId);
}

// --- 折叠时提问提醒 -------------------------------------------------------
// 提问表单必须用户人工填写，面板折叠时用户看不到 → 300s 后超时（模型被迫
// 按"用户取消"继续）。检测到**新出现**的 ask 卡片且面板折叠时：自动展开 +
// 轻通知。用"已通知 toolCallId 集合"精确判定新增——会话切换导致签名重算
// 时历史卡片不会误判为新增；组件挂载时已有卡片也不触发（首帧 prev 为空）。
const notifiedAskIds = new Set<string>();
watch(
  () =>
    (ai.messages ?? [])
      .flatMap((m) => m.toolCalls ?? [])
      .filter((t) => t.name === "ask_user_question")
      .map((t) => `${t.toolCallId}:${t.status}`)
      .join("|"),
  () => {
    // 只提醒**仍待回答**的新卡片：已超时/取消（rejected/done）的旧提问
    // 在切回会话时不应再弹"向你提问"通知（问题早已过期）。
    const fresh = (ai.messages ?? [])
      .flatMap((m) => m.toolCalls ?? [])
      .filter(
        (t) =>
          t.name === "ask_user_question" &&
          t.status === "pending" &&
          !notifiedAskIds.has(t.toolCallId)
      );
    if (fresh.length === 0) return;
    for (const t of fresh) notifiedAskIds.add(t.toolCallId);
    if (collapsed.value) {
      toggle();
      ElNotification({
        title: `${panelTitle.value}向你提问`,
        message: "AI 需要你回答几个问题，请查看面板填写表单",
        type: "warning",
        duration: 5000,
      });
    }
  },
  { flush: "post" }
);

function onKeydown(e: KeyboardEvent) {
  const el = e.target instanceof HTMLTextAreaElement ? e.target : null;

  // 上方向键浏览输入历史：已在历史浏览态时始终上翻；
  // 正常编辑态需光标在行首（或输入为空）才进入历史。
  if (e.key === "ArrowUp" && !e.shiftKey && !e.isComposing) {
    const inHistory = historyIndex.value !== -1;
    const atStart = el ? el.selectionStart === 0 : true;
    if ((inHistory || atStart) && sendHistory.value.length > 0) {
      if (!inHistory) {
        historyDraft.value = inputText.value;
        historyIndex.value = sendHistory.value.length - 1;
      } else if (historyIndex.value > 0) {
        historyIndex.value -= 1;
      } else {
        return; // 已是最旧一条
      }
      e.preventDefault();
      inputText.value = sendHistory.value[historyIndex.value];
      moveCursorToEnd(el);
    }
    return;
  }

  // 下方向键：历史浏览态下前进；到最后一条后再按恢复进入前的草稿。
  if (e.key === "ArrowDown" && !e.shiftKey && !e.isComposing) {
    if (historyIndex.value !== -1) {
      const atEnd = el ? el.selectionStart === el.value.length : true;
      if (atEnd) {
        e.preventDefault();
        if (historyIndex.value < sendHistory.value.length - 1) {
          historyIndex.value += 1;
          inputText.value = sendHistory.value[historyIndex.value];
        } else {
          // 回到草稿，退出历史浏览。
          historyIndex.value = -1;
          inputText.value = historyDraft.value;
        }
        moveCursorToEnd(el);
      }
    }
    return;
  }

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

/** 顶部"更多"下拉的命令分发（低频操作集中收纳，见 header 模板注释）。 */
function onHeaderCommand(cmd: string) {
  switch (cmd) {
    case "export":
      void handleExport();
      break;
    case "summarize":
      void handleSummarizeSkill();
      break;
    case "skills":
      skillManagerVisible.value = true;
      break;
    case "clear":
      void handleClear();
      break;
  }
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

  // 任务清单（todo_write 维护，导出时保留最终状态）。
  if (conv.todos && conv.todos.length > 0) {
    lines.push("");
    lines.push("## 任务清单");
    for (const t of conv.todos) {
      const mark =
        t.status === "completed" ? "[x]" : t.status === "in_progress" ? "[~]" : "[ ]";
      lines.push(`- ${mark} ${t.content}`);
    }
    lines.push("");
  }

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
/**
 * 是否为流式消息中**正在增长**的文本段（流式中 + 末段文本）。
 * 该段用纯文本渲染（见模板）：流式期间每个 delta 都以"全新全文"为 key，
 * markdown 缓存永远不命中——每个 chunk 都重跑 marked + DOMPurify 并整体
 * 重建 innerHTML，O(n²) 开销让回复越长越卡（提交后长时间无响应的元凶）。
 * 纯文本插值开销低两个数量级；流结束（streaming=false）自动恢复 markdown，
 * 历史消息与已完成段落不受影响。
 */
function isGrowingPart(
  m: AiMessage,
  part: { kind: "text"; text: string } | { kind: "tool"; item: ToolCallItem },
  pIdx: number,
): boolean {
  return m.streaming && part.kind === "text" && pIdx === (m.parts?.length ?? 0) - 1;
}

/**
 * 给每个代码块（`<pre>`）套一层容器并插入「复制」按钮。
 *
 * 正文是 v-html 注入的裸 HTML，没有组件实例，按钮只能用内联元素 + 事件委托
 * （见 [`onMdClick`]）。按钮放在 `pre` **外层**：放进 pre 内部会让
 * `pre.textContent` 把按钮文字一起复制出去。
 *
 * 在 sanitize 之后执行，注入的标记不受 DOMPurify 策略限制。
 */
function decorateCodeBlocks(html: string): string {
  // 无代码块时（绝大多数段落）直接返回，省掉一次 DOM 解析。
  if (!html.includes("<pre")) return html;
  const doc = new DOMParser().parseFromString(html, "text/html");
  doc.querySelectorAll("pre").forEach((pre) => {
    const wrap = doc.createElement("div");
    wrap.className = "md-code";
    const btn = doc.createElement("button");
    btn.type = "button";
    btn.className = "md-code-copy";
    btn.textContent = "复制";
    pre.replaceWith(wrap);
    wrap.append(btn, pre);
  });
  return doc.body.innerHTML;
}

/**
 * 代码块「复制」按钮的点击处理（事件委托）。
 *
 * v-html 注入的按钮无法绑定 `@click`，故在 `.md` 容器上统一监听。取同级 `pre`
 * 的 `textContent`——它天然保留原始缩进与换行，且不含按钮文字。
 */
function onMdClick(e: MouseEvent) {
  const btn = (e.target as HTMLElement | null)?.closest(".md-code-copy");
  if (!btn) return;
  const code = btn.parentElement?.querySelector("pre")?.textContent ?? "";
  if (code) void copyText(code);
}

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
  // 代码块加容器与「复制」按钮：随最终字符串一起进缓存，每条消息只处理一次。
  const decorated = decorateCodeBlocks(clean);
  // 缓存上限 200 条，避免长对话内存膨胀。
  if (renderedCache.size > 200) renderedCache.clear();
  renderedCache.set(text, decorated);
  return decorated;
}
</script>

<template>
  <div
    class="ai-panel"
    :class="{ collapsed, dragging }"
    :style="{ width: panelWidth + 'px' }"
  >
    <!-- 拖拽竖条：展开态下拖动调整面板宽度，双击恢复默认 -->
    <div
      v-if="!collapsed"
      class="resize-handle"
      title="拖动调整宽度，双击恢复默认"
      @mousedown="startResize"
      @dblclick="resetWidth"
    />
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
          <!-- 收起是高频操作，保留独立按钮；低频/破坏性操作收进"更多"下拉，
               既缓解窄面板的拥挤，也避免"清空对话"误触。 -->
          <el-tooltip content="收起" placement="bottom">
            <el-button class="icon-btn" link @click="toggle">
              <el-icon><DArrowRight /></el-icon>
            </el-button>
          </el-tooltip>
          <!-- 历史会话：关闭归档的会话列表，面板在按钮下方弹出 -->
          <el-popover
            ref="historyPopRef"
            placement="bottom-end"
            :width="280"
            trigger="click"
            popper-class="ai-history-popper"
          >
            <template #reference>
              <el-button class="icon-btn" link title="历史会话">
                <el-icon><Clock /></el-icon>
              </el-button>
            </template>
            <div class="arch-panel">
              <div class="arch-head">
                <span>历史会话</span>
                <el-button
                  v-if="archiveList.length > 0"
                  size="small"
                  link
                  type="danger"
                  @click="onClearArchives"
                >
                  清空
                </el-button>
              </div>
              <div class="arch-list">
                <div
                  v-for="c in archiveList"
                  :key="c.id"
                  class="arch-item"
                  @click="onRestoreConversation(c.id)"
                >
                  <div class="arch-item-main">
                    <div class="arch-item-title" :title="c.title">{{ c.title || '新对话' }}</div>
                    <div class="arch-item-meta">
                      <span>{{ c.messages.length }} 条消息</span>
                      <span v-if="fmtRelTime(c.archivedAt)">关闭于 {{ fmtRelTime(c.archivedAt) }}</span>
                    </div>
                  </div>
                  <el-tooltip content="恢复会话" placement="top">
                    <el-icon class="arch-act restore" @click.stop="onRestoreConversation(c.id)">
                      <RefreshRight />
                    </el-icon>
                  </el-tooltip>
                  <el-tooltip content="彻底删除" placement="top">
                    <el-icon class="arch-act delete" @click.stop="onDeleteConversation(c.id, c.title)">
                      <Delete />
                    </el-icon>
                  </el-tooltip>
                </div>
                <div v-if="archiveList.length === 0" class="arch-empty">
                  暂无历史会话（关闭的对话会归档到这里）
                </div>
              </div>
            </div>
          </el-popover>
          <el-dropdown
            trigger="click"
            popper-class="ai-header-dropdown"
            @command="onHeaderCommand"
          >
            <el-button class="icon-btn" link title="更多">
              <el-icon><MoreFilled /></el-icon>
            </el-button>
            <template #dropdown>
              <el-dropdown-menu>
                <el-dropdown-item command="export" :disabled="ai.messages.length === 0">
                  <el-icon><Download /></el-icon>导出对话
                </el-dropdown-item>
                <el-dropdown-item
                  command="summarize"
                  :disabled="ai.messages.length === 0 || ai.sending || summarizing"
                >
                  <el-icon><MagicStick /></el-icon>
                  {{ summarizing ? "总结成技能（生成中…）" : "总结成技能" }}
                </el-dropdown-item>
                <el-dropdown-item command="skills">
                  <el-icon><Collection /></el-icon>技能管理
                </el-dropdown-item>
                <el-dropdown-item
                  command="clear"
                  divided
                  :disabled="ai.messages.length === 0"
                  class="danger-item"
                >
                  <el-icon><Delete /></el-icon>清空对话
                </el-dropdown-item>
              </el-dropdown-menu>
            </template>
          </el-dropdown>
        </div>
      </div>

      <!-- 对话标签栏：多会话切换（标签收缩 + 横向滚动，见 .conv-tabs 样式注释） -->
      <div ref="convTabsRef" class="conv-tabs">
        <div
          v-for="c in ai.conversations"
          :key="c.id"
          class="conv-tab"
          :class="{ active: c.id === ai.activeCid }"
          :title="c.title"
          @click="onSwitchConversation(c.id)"
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

      <!-- 智能体任务清单条（todo_write 维护；新回合自动清空）。
           放在滚动区之外：始终固定在消息列表顶部，不随消息滚动被滚走。 -->
      <div v-if="activeTodos.length > 0" class="todo-strip">
        <div class="todo-strip-title" @click="todoCollapsed = !todoCollapsed">
          <el-icon><Collection /></el-icon>
          <span>任务清单</span>
          <span class="todo-stats">{{ todoStats.completed }}/{{ todoStats.total }} 完成</span>
          <div class="todo-progress" :class="{ done: todoStats.completed === todoStats.total }">
            <div class="todo-progress-bar" :style="{ width: todoStats.pct + '%' }" />
          </div>
          <el-icon
            v-if="activeTodos.length > TODO_COLLAPSE_THRESHOLD"
            class="todo-collapse"
            :title="todoCollapsed ? '展开' : '收起'"
          >{{ todoCollapsed ? 'ArrowDown' : 'ArrowUp' }}</el-icon>
        </div>
        <div v-show="!todoCollapsed" class="todo-items">
          <div
            v-for="(t, i) in activeTodos"
            :key="i"
            class="todo-item"
            :class="`todo-${t.status}`"
          >
            <el-icon v-if="t.status === 'completed'" class="todo-icon done"><CircleCheckFilled /></el-icon>
            <el-icon v-else-if="t.status === 'in_progress'" class="todo-icon in-progress is-loading"><Loading /></el-icon>
            <el-icon v-else class="todo-icon"><Clock /></el-icon>
            <span class="todo-content" :class="{ 'todo-content-done': t.status === 'completed' }">
              {{ t.content }}
            </span>
          </div>
        </div>
      </div>

      <!-- 消息列表 -->
      <el-scrollbar ref="scrollbarRef" class="messages">
        <div v-if="ai.messages.length === 0" class="empty-hint">
          <template v-if="mode === 'agent'">
            <template v-if="domain === 'ssh'">
              智能体模式：AI 可在你的服务器上执行 SSH 命令。先连接一个终端，然后描述任务。
            </template>
            <template v-else-if="domain === 'db'">
              智能体模式：AI 可在你的数据库上执行 SQL。先在 SQL 控制台连接数据库，然后描述任务。
            </template>
            <template v-else>
              智能体模式：AI 可查看并操作你的内嵌 RDP 桌面（截图 / 点击 / 输入）。
              先在左侧连接一个 RDP 桌面（需多模态模型），然后描述任务。
            </template>
          </template>
          <template v-else>暂无对话。选择模式后输入你的问题。</template>
        </div>
        <!-- 长对话折叠条：更早的消息未渲染，点击展开全部（性能：旧消息不进 DOM） -->
        <div v-if="visibleStart >= 0" class="older-fold">
          <el-button size="small" text bg @click="olderExpanded = true">
            展开更早的 {{ visibleStart }} 条消息
          </el-button>
        </div>
        <div
          v-for="m in visibleMessages"
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
              <!-- 删除消息：请求/响应均可删（生成中的消息不显示操作条，天然禁删） -->
              <el-tooltip content="删除" placement="top">
                <el-icon
                  class="msg-action msg-action-danger"
                  @click="deleteMessage(m)"
                ><Delete /></el-icon>
              </el-tooltip>
            </div>
            <template v-if="m.role === 'assistant'">
              <!--
                按事件到达顺序渲染片段：文本段与工具调用交替出现，使工具卡片落在
                正文中间的真实位置（如「说要做X → 工具卡片 → 总结」），而非全堆顶部。
              -->
              <template v-for="(part, pIdx) in (m.parts ?? [])" :key="pIdx">
                <!-- 文本段：已完成的走 markdown（有缓存）；流式中的增长段走
                     纯文本（isGrowingPart 说明），流结束后自动切回 markdown。 -->
                <div v-if="part.kind === 'text' && part.text" class="md" @click="onMdClick">
                  <template v-if="isGrowingPart(m, part, pIdx)">
                    <span class="md-raw">{{ part.text }}</span><span class="stream-cursor" />
                  </template>
                  <template v-else>
                    <div v-html="renderMarkdown(part.text)" />
                  </template>
                </div>
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
                    <!-- 头部状态 tag：**生命周期状态优先**于固有属性标签——
                         批准（approved）/拒绝（rejected）/完成（done）后，卡片
                         不能再顶着"需确认"（固有属性 dangerous/whitelisted 不随
                         用户操作变化，折叠态下头部 tag 是唯一可见状态）。 -->
                    <el-tag v-if="part.item.name === 'todo_write'" type="primary" size="small" effect="dark">任务清单</el-tag>
                    <el-tag v-else-if="part.item.name === 'load_skill'" type="info" size="small" effect="plain">技能</el-tag>
                    <el-tag
                      v-else-if="part.item.name === 'ask_user_question'"
                      :type="part.item.status === 'rejected' ? 'info' : 'warning'"
                      size="small"
                      effect="plain"
                    >
                      {{ part.item.status === "rejected" ? "已取消" : part.item.status === "approved" ? "已提交" : "提问" }}
                    </el-tag>
                    <el-tag v-else-if="part.item.status === 'done'" type="success" size="small" effect="plain">已执行</el-tag>
                    <el-tag v-else-if="part.item.status === 'rejected'" type="danger" size="small" effect="plain">已拒绝</el-tag>
                    <!-- approved 拆分：自动放行（白名单/只读）没有任何人"确认"过，
                         显示"已确认"与底部"已自动执行"自相矛盾；人工确认的才叫已确认。 -->
                    <el-tag v-else-if="part.item.status === 'approved' && part.item.autoApproved" type="success" size="small" effect="plain">自动执行中</el-tag>
                    <el-tag v-else-if="part.item.status === 'approved'" type="primary" size="small" effect="plain">已确认 · 执行中</el-tag>
                    <!-- pending：按固有属性展示（危险/自动执行/白名单/需确认）。 -->
                    <el-tag v-else-if="part.item.dangerous" type="danger" size="small" effect="dark">危险</el-tag>
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
                  <!--
                    load_skill / ask_user_question 卡片默认展开（技能全文、问题表单
                    用户通常想直接看到），只有用户显式收起（expanded 置 false）后才
                    折叠；其余工具卡片默认折叠，点击展开。
                  -->
                  <div
                    v-if="
                      part.item.name === 'load_skill' || part.item.name === 'ask_user_question'
                        ? expanded[part.item.toolCallId] !== false
                        : expanded[part.item.toolCallId]
                    "
                    class="tool-detail"
                  >
                    <!-- ask_user_question：问题表单（选项 + 自由输入 + 提交/取消），
                         不展示原始参数 JSON。问题卡片化、选项为可点选卡片
                         （自绘单选/多选标记），提交后整体置灰并显示回答摘要。 -->
                    <div
                      v-if="part.item.name === 'ask_user_question'"
                      class="ask-form"
                      :class="{ 'is-done': part.item.status !== 'pending' }"
                    >
                      <div
                        v-for="(q, qi) in askQuestions(part.item)"
                        :key="q.id"
                        class="ask-q"
                      >
                        <div class="ask-q-head">
                          <span v-if="askQuestions(part.item).length > 1" class="ask-q-index">
                            {{ qi + 1 }}/{{ askQuestions(part.item).length }}
                          </span>
                          <span v-if="q.header" class="ask-header">{{ q.header }}</span>
                          <span v-if="q.multi_select" class="ask-multi-tag">可多选</span>
                        </div>
                        <div class="ask-question">{{ q.question }}</div>
                        <div v-if="q.options && q.options.length" class="ask-options">
                          <label
                            v-for="opt in q.options"
                            :key="opt.label"
                            class="ask-option"
                          >
                            <input
                              :type="q.multi_select ? 'checkbox' : 'radio'"
                              :name="'ask-' + part.item.toolCallId + '-' + q.id"
                              :disabled="part.item.status !== 'pending'"
                              :checked="askSel(part.item, q.id).selected.includes(opt.label)"
                              @change="toggleAskOption(part.item, q, opt.label)"
                            />
                            <span class="ask-opt-box" :class="{ multi: q.multi_select }" />
                            <span class="ask-opt-body">
                              <span class="ask-opt-label">
                                {{ optLabel(opt.label) }}
                                <span v-if="isRecommended(opt.label)" class="ask-opt-rec">推荐</span>
                              </span>
                              <span v-if="opt.description" class="ask-opt-desc">
                                {{ opt.description }}
                              </span>
                            </span>
                          </label>
                        </div>
                        <el-input
                          v-model="askSel(part.item, q.id).custom"
                          type="textarea"
                          :rows="2"
                          size="small"
                          class="ask-input"
                          :disabled="part.item.status !== 'pending'"
                          :placeholder="
                            q.options && q.options.length ? '补充说明（可选）' : '请输入…'
                          "
                        />
                      </div>
                      <!-- 已提交的回答摘要（同会话内存中有勾选/输入记录时展示） -->
                      <div v-if="askAnswerChips(part.item).length" class="ask-answered">
                        <span
                          v-for="(chip, ci) in askAnswerChips(part.item)"
                          :key="ci"
                          class="ask-answered-chip"
                        >
                          {{ chip }}
                        </span>
                      </div>
                      <div class="ask-actions">
                        <el-button
                          size="small"
                          :disabled="part.item.status !== 'pending'"
                          @click.stop="cancelAskUser(part.item)"
                        >
                          取消
                        </el-button>
                        <el-button
                          size="small"
                          type="primary"
                          :disabled="part.item.status !== 'pending'"
                          @click.stop="submitAskUser(part.item)"
                        >
                          提交回答
                        </el-button>
                      </div>
                    </div>
                    <template v-else>
                      <div class="tool-args">
                        <span class="label">参数:</span>
                        <pre>{{ JSON.stringify(part.item.arguments, null, 2) }}</pre>
                      </div>
                      <div v-if="part.item.result" class="tool-output">
                        <span class="label">{{ part.item.result.ok ? '输出:' : '失败:' }}</span>
                        <pre>{{ part.item.result.output }}</pre>
                      </div>
                    </template>
                  </div>
                  <!-- 操作按钮（仅 pending 时显示；自动放行与提问表单无确认按钮） -->
                  <div
                    v-if="
                      part.item.status === 'pending' &&
                      part.item.name !== 'ask_user_question'
                    "
                    class="tool-actions"
                  >
                    <!-- 文案精简：面板宽度有限，长文案会把"拒绝"挤出可视区；
                         完整语义放 title 悬停提示（危险属性卡片头部已有红色
                         "危险"标记，按钮红色即可，无需文案复述）。 -->
                    <el-button
                      size="small"
                      :type="part.item.dangerous ? 'danger' : 'primary'"
                      :title="part.item.dangerous ? '确认执行该危险操作' : '确认执行'"
                      @click.stop="approveTool(part.item)"
                    >
                      执行
                    </el-button>
                    <!-- 加入白名单并执行：仅 exec_ssh 非危险非白名单时显示 -->
                    <el-button
                      v-if="part.item.name === 'exec_ssh' && !part.item.dangerous && !part.item.whitelisted"
                      size="small"
                      type="success"
                      plain
                      title="把该命令前缀加入白名单并执行，以后同类命令自动放行"
                      @click.stop="addToWhitelistAndRun(part.item)"
                    >
                      白名单执行
                    </el-button>
                    <el-button size="small" title="拒绝执行该操作" @click.stop="rejectTool(part.item)">拒绝</el-button>
                  </div>
                  <div v-else-if="part.item.status === 'approved' && part.item.name === 'ask_user_question'" class="tool-status">已提交，等待模型继续</div>
                  <div v-else-if="part.item.status === 'approved' && !part.item.autoApproved" class="tool-status">执行中…</div>
                  <div v-else-if="part.item.status === 'approved' && part.item.autoApproved" class="tool-status">自动执行中…</div>
                  <!-- done：结果已回填（展开可见），底部给出明确终态，避免空白无下文。 -->
                  <div
                    v-else-if="part.item.status === 'done'"
                    class="tool-status"
                    :class="{ 'tool-status-fail': part.item.result && !part.item.result.ok }"
                  >
                    {{ part.item.result && !part.item.result.ok ? "执行失败（展开查看）" : "已完成" }}
                  </div>
                  <div v-else-if="part.item.status === 'rejected'" class="tool-status">
                    {{ part.item.name === 'ask_user_question' ? '已取消' : '已拒绝' }}
                  </div>
                </div>
              </template>
              <!-- 流式占位：无任何片段且正在流式 → 省略号；有文本且流式中 → 闪烁光标 -->
              <span v-if="m.streaming && !(m.parts && m.parts.length)" class="dots">...</span>
              <span v-if="m.streaming && m.content" class="cursor" />
              <div v-if="m.error" class="error-text">⚠ {{ m.error }}</div>
            </template>
            <!-- 用户消息：图片（多模态）+ 文本。
                 图片限制最大宽高、按原比例完整展示（不裁剪），点击打开查看器看大图。 -->
            <template v-else>
              <!-- 系统注入的提醒（重复调用守卫等）：灰色居中提示，不伪装成用户消息 -->
              <div v-if="isSystemReminder(m)" class="system-note">{{ m.content }}</div>
              <template v-else>
                <div v-if="m.images && m.images.length" class="msg-images">
                  <img
                    v-for="(img, i) in m.images"
                    :key="i"
                    :src="imageDataUrl(img)"
                    class="msg-img"
                    alt="图片"
                    title="点击查看大图"
                    @click="previewImages(m, i)"
                  />
                </div>
                <template v-if="m.content">{{ m.content }}</template>
              </template>
            </template>
          </div>
        </div>
      </el-scrollbar>

      <!-- 输入框高度拖拽条（composer 上边缘）：向上拖放大输入区，双击恢复自动 -->
      <div
        class="composer-resizer"
        :class="{ active: composerHeightDragging }"
        title="拖拽调整输入框高度（双击恢复自动）"
        @mousedown="startComposerResize"
        @dblclick="resetComposerH"
      />

      <!-- 底部输入区 -->
      <div
        ref="composerRef"
        class="composer"
        :class="{ 'drag-over': dragOver, 'has-custom-height': composerH != null }"
        :style="composerH != null ? { height: composerH + 'px' } : undefined"
        @drop="onComposerDrop"
        @dragover="onComposerDragOver"
        @dragleave="onComposerDragLeave"
      >
        <!-- 输入框上方的信息栏：智能体上下文（已附加终端）+ token 用量 -->
        <div class="composer-toolbar">
          <!-- ssh 域：提示条即终端绑定入口（点击弹出终端菜单，选择结果体现在
               提示文案"xxx（已绑定/跟随当前）"里）——不额外占用工具栏宽度。 -->
          <el-dropdown
            v-if="props.domain === 'ssh' && mode === 'agent' && !configBlocked"
            trigger="click"
            popper-class="ai-term-bind-dropdown"
            :disabled="ai.sending"
            @command="setBoundTerminal"
          >
            <span
              class="ctx-tip-inline ctx-tip-link"
              :title="ai.sending ? 'AI 响应中，本轮固定操作该终端，响应结束后可切换' : '点击选择 AI 固定操作的终端'"
            >
              <el-icon><Connection /></el-icon>
              <span>{{ contextTip }}</span>
              <el-icon class="ctx-caret"><ArrowDown /></el-icon>
            </span>
            <template #dropdown>
              <el-dropdown-menu>
                <el-dropdown-item
                  :command="AUTO_TERMINAL"
                  :class="{ 'is-active-bind': boundTerminalId === AUTO_TERMINAL }"
                >
                  跟随当前终端
                </el-dropdown-item>
                <el-dropdown-item
                  v-for="t in terminalOptions"
                  :key="t.id"
                  :command="t.id"
                  :class="{ 'is-active-bind': boundTerminalId === t.id }"
                >
                  {{ t.label }}
                </el-dropdown-item>
                <div v-if="terminalOptions.length === 0" class="term-bind-empty">
                  暂无已打开的终端
                </div>
              </el-dropdown-menu>
            </template>
          </el-dropdown>
          <span v-else-if="mode === 'agent' && !configBlocked" class="ctx-tip-inline">
            <el-icon><Connection /></el-icon>
            <span>{{ contextTip }}</span>
          </span>
          <!-- token 用量（最近一次模型请求；取最后一次返回的 usage，不累计） -->
          <span
            v-if="activeUsage.prompt > 0 || activeUsage.completion > 0"
            class="usage-meter"
            :title="`最近一次请求：输入 ${activeUsage.prompt.toLocaleString()} tokens，输出 ${activeUsage.completion.toLocaleString()} tokens`"
          >
            <el-icon><Odometer /></el-icon>
            {{ formatTokens(activeUsage.prompt + activeUsage.completion) }} tokens
          </span>
          <!-- 图片上传（仅多模态模型显示）：置于工具栏最右，点击选图，也可直接粘贴/拖入 -->
          <el-button
            v-if="multimodalEnabled"
            link
            :icon="'Picture'"
            class="attach-img-btn"
            :disabled="ai.sending || configBlocked"
            :loading="imageReading"
            title="附带图片（也可直接粘贴或拖入）"
            @click="pickImage"
          />
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
        <!-- 已附加的图片（多模态模型下显示，会随问题一起发给 AI） -->
        <div v-if="attachedImages.length > 0" class="attached-images">
          <span class="attached-label">
            <el-icon><Picture /></el-icon>
            图片:
          </span>
          <div v-for="img in attachedImages" :key="img.id" class="attached-img-item">
            <img :src="imageDataUrl(img)" class="attached-img-thumb" alt="预览" />
            <el-icon class="attached-img-remove" @click="removeImage(img.id)"><Close /></el-icon>
            <span class="attached-img-name">{{ img.name }}</span>
          </div>
          <el-button
            link
            size="small"
            class="attached-clear"
            @click="attachedImages = []"
          >
            清空
          </el-button>
        </div>
        <!-- 已选技能（输入框上方简短显示，随问题一起发给 AI） -->
        <div v-if="selectedSkills.length > 0" class="selected-skills">
          <span class="attached-label">
            <el-icon><Collection /></el-icon>
            技能:
          </span>
          <el-tag
            v-for="s in selectedSkills"
            :key="s.id"
            closable
            size="small"
            @close="removeSelectedSkill(s.id)"
          >
            {{ s.title }}
          </el-tag>
          <el-button link size="small" class="attached-clear" @click="selectedSkills = []">
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
                  : multimodalEnabled
                  ? '输入问题，Enter 发送，Shift+Enter 换行（可粘贴/拖入图片）'
                  : '输入问题，Enter 发送，Shift+Enter 换行'
              "
              :disabled="configBlocked"
              resize="none"
              @keydown="onKeydown"
              @paste="onComposerPaste"
            />
            <!-- 输入框内底部工具行：左=技能/模式/执行模式，右=模型选择+发送（同一行） -->
            <div class="composer-inline-bar">
              <!-- 技能按钮（+）：下拉勾选本域技能，已选的在输入框上方显示（向上弹出） -->
              <el-dropdown
                trigger="click"
                popper-class="ai-skill-dropdown"
                placement="top-start"
                @command="onSkillCommand"
              >
                <button class="skill-btn" title="选择技能">
                  <el-icon><Plus /></el-icon>
                </button>
                <template #dropdown>
                  <el-dropdown-menu>
                    <el-dropdown-item
                      v-for="s in domainSkills"
                      :key="s.id"
                      :command="s.id"
                      :title="s.content"
                    >
                      <span class="skill-item-title">{{ s.title }}</span>
                      <el-icon v-if="selectedSkills.some((x) => x.id === s.id)" class="skill-check">
                        <CircleCheckFilled />
                      </el-icon>
                    </el-dropdown-item>
                    <div v-if="domainSkills.length === 0" class="skill-empty">
                      暂无技能，可从「总结成技能」生成
                    </div>
                    <el-dropdown-item divided command="__manage__">
                      <el-icon><Collection /></el-icon>管理技能
                    </el-dropdown-item>
                  </el-dropdown-menu>
                </template>
              </el-dropdown>
              <el-select
                v-model="mode"
                size="small"
                placement="top-start"
                popper-class="composer-pop"
                class="inline-select inline-mode-select"
              >
                <el-option
                  v-for="opt in MODE_OPTIONS"
                  :key="opt.value"
                  :label="opt.label"
                  :value="opt.value"
                />
              </el-select>
              <!-- 执行模式（智能体模式才有工具执行；ssh→命令、db→SQL）
                   面板过窄时切为纯图标显示：手动=锁 / 白名单=钥匙 / 自动=闪电 -->
              <el-select
                v-if="showRunMode && mode === 'agent'"
                v-model="runMode"
                size="small"
                placement="top-start"
                :offset="88"
                popper-class="composer-pop runmode-pop"
                class="inline-select inline-runmode-select"
                :class="[`runmode-${runMode}`, { 'is-compact': compactRunMode }]"
                :title="`执行模式：${runModeLabel}`"
              >
                <template #label>
                  <span v-if="compactRunMode" class="runmode-icon-label">
                    <el-icon :size="14">
                      <component :is="RUN_MODE_ICONS[runMode]" />
                    </el-icon>
                  </span>
                  <template v-else>{{ runModeLabel }}</template>
                </template>
                <el-option
                  v-for="opt in RUN_MODE_OPTIONS"
                  :key="opt.value"
                  :value="opt.value"
                  :label="opt.label"
                >
                  <div class="runmode-option">
                    <span>{{ opt.label }}</span>
                    <span class="runmode-desc">{{ opt.desc }}</span>
                  </div>
                </el-option>
              </el-select>
              <span class="inline-spacer" />
              <!-- 模型选择：只显示模型名（top-end 右对齐，防超出窗口右缘） -->
              <el-select
                v-model="activeModel"
                size="small"
                filterable
                :disabled="modelOptions.length === 0"
                placement="top-end"
                popper-class="composer-pop"
                class="inline-select inline-model-select"
                title="当前模型"
              >
                <el-option
                  v-for="m in modelOptions"
                  :key="m.value"
                  :value="m.value"
                  :label="m.label"
                />
              </el-select>
              <!-- 发送 / 终止：发送中变为红色终止图标 -->
              <el-button
                v-if="!ai.sending"
                link
                :icon="'Promotion'"
                :disabled="(!inputText.trim() && attachedImages.length === 0) || configBlocked"
                class="inline-send-btn"
                title="发送 (Enter)"
                @click="handleSend"
              />
              <el-button
                v-else
                link
                :icon="'VideoPause'"
                class="inline-send-btn stop-btn"
                title="终止生成"
                @click="handleStop"
              />
            </div>
          </div>
        </div>
      </div>
    </div>
    <!-- 技能编辑弹窗（总结生成后或从管理器新建时弹出） -->
    <SkillDialog
      v-model:visible="skillDialogVisible"
      :skill="editingSkill"
      :domain="domain"
      @saved="onSkillSaved"
    />
    <!-- 技能管理弹窗（列表 / 启停 / 删除 / 新建） -->
    <SkillManagerDialog
      v-model:visible="skillManagerVisible"
      :domain="domain"
    />
    <!-- 大图查看器：点击消息缩略图打开（teleported 到 body，缩放/旋转/翻页/滚轮） -->
    <el-image-viewer
      v-if="viewerVisible"
      :url-list="viewerUrls"
      :initial-index="viewerIndex"
      hide-on-click-modal
      teleported
      @close="viewerVisible = false"
    />
  </div>
</template>

<style scoped lang="scss">
.ai-panel {
  position: relative;
  height: 100%;
  background: var(--el-bg-color);
  border-left: 1px solid var(--el-border-color-light);
  display: flex;
  flex-direction: column;
  overflow: hidden;
  transition: width 0.2s ease;
  flex-shrink: 0;
}

/* 拖拽中禁用宽度过渡，保证跟手 */
.ai-panel.dragging {
  transition: none;
}

/* --- 拖拽竖条（面板左缘，悬浮于终端区之上） --- */
.resize-handle {
  position: absolute;
  left: -4px;
  top: 0;
  bottom: 0;
  width: 8px;
  cursor: col-resize;
  z-index: 10;
}
/* 悬停/拖拽时显示一条主色竖线提示可拖 */
.resize-handle::after {
  content: "";
  position: absolute;
  left: 3px;
  top: 0;
  bottom: 0;
  width: 2px;
  background: transparent;
  transition: background 0.15s;
}
.resize-handle:hover::after,
.ai-panel.dragging .resize-handle::after {
  background: var(--el-color-primary);
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

/* 对话标签栏：标签允许收缩（min-width:0 是 flex 收缩的关键，否则 nowrap
   文本的 min-content 会撑住宽度不缩），溢出横向滚动兜底（细滚动条弱化视觉）。 */
.conv-tabs {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 0 6px 6px;
  flex-shrink: 0;
  overflow-x: auto;
  border-bottom: 1px solid var(--el-border-color-lighter);
  scrollbar-width: thin;
}
.conv-tabs::-webkit-scrollbar {
  height: 3px;
}
.conv-tabs::-webkit-scrollbar-thumb {
  background: var(--el-border-color);
  border-radius: 2px;
}
.conv-tabs::-webkit-scrollbar-track {
  background: transparent;
}
.conv-tab {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  /* 收缩区间：窄面板下可压到 56px（约 2 字标题 + 关闭钮），宽面板放到 130px */
  min-width: 56px;
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
  /* flex item 内文本收缩 + ellipsis 的必要条件 */
  min-width: 0;
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

/* --- 历史会话面板（header 按钮下方弹出的 popover 内容） ---
   popover 挂 body，但插槽内容带本组件 data-v 属性，scoped 样式依然生效；
   面板外壳（padding 等）用下方 :global(.ai-history-popper) 微调。 */
:global(.ai-history-popper.el-popover) {
  padding: 8px;
}
.arch-panel {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.arch-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  font-size: 13px;
  font-weight: 600;
  color: var(--el-text-color-primary);
  padding-bottom: 4px;
  border-bottom: 1px solid var(--el-border-color-lighter);
}
.arch-list {
  display: flex;
  flex-direction: column;
  gap: 6px;
  /* 归档很多时限高滚动，面板不撑出屏幕。 */
  max-height: 320px;
  overflow-y: auto;
}
.arch-item {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 10px;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  cursor: pointer;
  transition: border-color 0.15s, background 0.15s;
}
.arch-item:hover {
  border-color: var(--el-color-primary-light-5);
  background: var(--el-fill-color-light);
}
.arch-item-main {
  flex: 1;
  min-width: 0;
}
.arch-item-title {
  font-size: 13px;
  font-weight: 500;
  color: var(--el-text-color-primary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.arch-item-meta {
  display: flex;
  gap: 10px;
  margin-top: 2px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.arch-act {
  flex-shrink: 0;
  font-size: 14px;
  color: var(--el-text-color-secondary);
}
.arch-act.restore:hover {
  color: var(--el-color-primary);
}
.arch-act.delete:hover {
  color: var(--el-color-danger);
}
.arch-empty {
  padding: 32px 0;
  text-align: center;
  font-size: 13px;
  color: var(--el-text-color-placeholder);
}
.icon-btn {
  padding: 4px;
  color: var(--el-text-color-secondary);
}
.icon-btn:hover {
  color: var(--el-color-primary);
}

/* 顶部"更多"下拉（element-plus 下拉挂 body，需全局样式而非 scoped） */
/* 终端绑定下拉（挂 body，全局样式）：当前绑定项高亮 */
:global(.ai-term-bind-dropdown .el-dropdown-menu__item.is-active-bind) {
  color: var(--el-color-primary);
  font-weight: 600;
}
:global(.ai-term-bind-dropdown .term-bind-empty) {
  padding: 6px 16px;
  font-size: 12px;
  color: var(--el-text-color-placeholder);
}

/* 输入框内工具行的下拉（挂 body，全局样式）：限制宽度/高度，
   弹出方向由各 select 的 placement 属性控制（向上弹）。 */
:global(.composer-pop) {
  max-width: 280px;
}
:global(.composer-pop .el-select-dropdown__list) {
  max-height: 220px;
}
/* 执行模式下拉项是两行内容（标签+说明）：覆盖默认 34px 定高/nowrap 裁切 */
:global(.runmode-pop .el-select-dropdown__item) {
  height: auto;
  line-height: 1.4;
  white-space: normal;
  padding: 4px 20px;
}
/* 执行模式下拉收窄：上浮到输入框上方后不宜过宽，说明文字自然换行 */
:global(.runmode-pop) {
  max-width: 210px;
}
/* "/技能"下拉（挂 body，全局样式）：限宽 + 技能名截断 */
:global(.ai-skill-dropdown) {
  max-width: 280px;
}
:global(.ai-skill-dropdown .el-dropdown-menu__item) {
  max-width: 260px;
}
:global(.ai-skill-dropdown .skill-item-title) {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
:global(.ai-skill-dropdown .skill-empty) {
  padding: 8px 16px;
  font-size: 12px;
  color: var(--el-text-color-placeholder);
}
:global(.ai-header-dropdown .el-dropdown-menu__item .el-icon) {
  margin-right: 6px;
}
:global(.ai-header-dropdown .el-dropdown-menu__item.danger-item) {
  color: var(--el-color-danger);
}
:global(.ai-header-dropdown .el-dropdown-menu__item.danger-item:not(.is-disabled):hover) {
  color: var(--el-color-danger);
  background: var(--el-color-danger-light-9);
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
/* 长对话折叠条（居中弱化，点击展开全部旧消息） */
.older-fold {
  display: flex;
  justify-content: center;
  padding: 6px 0 10px;
}
/* --- 智能体任务清单条（todo_write 维护） --- */
.todo-strip {
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  background: var(--el-fill-color-light);
  padding: 8px 10px;
  /* 固定条：不参与 flex 压缩；左右留白对齐消息区 padding（12px）。 */
  flex-shrink: 0;
  margin: 12px 12px 0;
  font-size: 12px;
  /* 清单条目很多（展开态）时限高内部滚动，避免把消息区挤没。 */
  max-height: 40%;
  overflow-y: auto;
}
.todo-strip-title {
  display: flex;
  align-items: center;
  gap: 6px;
  color: var(--el-text-color-secondary);
  font-weight: 600;
  margin-bottom: 4px;
  cursor: pointer;
  user-select: none;
}
.todo-strip-title:hover {
  color: var(--el-text-color-primary);
}
.todo-stats {
  font-weight: 400;
  font-size: 11px;
  flex-shrink: 0;
}
/* 进度条：细长灰底 + 主色填充；全部完成变绿 */
.todo-progress {
  flex: 1;
  min-width: 40px;
  height: 4px;
  border-radius: 2px;
  background: var(--el-fill-color-darker);
  overflow: hidden;
}
.todo-progress.done .todo-progress-bar {
  background: var(--el-color-success);
}
.todo-progress-bar {
  height: 100%;
  border-radius: 2px;
  background: var(--el-color-primary);
  transition: width 0.3s ease;
}
.todo-collapse {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  flex-shrink: 0;
}
.todo-items {
  max-height: 220px;
  overflow-y: auto;
}
.todo-item {
  display: flex;
  align-items: flex-start;
  gap: 6px;
  padding: 2px 0;
  color: var(--el-text-color-primary);
  line-height: 1.5;
}
.todo-icon {
  flex-shrink: 0;
  margin-top: 2px;
  font-size: 13px;
  color: var(--el-text-color-placeholder);
}
.todo-item.todo-in_progress .todo-icon {
  color: var(--el-color-primary);
}
.todo-item.todo-completed .todo-icon.done {
  color: var(--el-color-success);
}
.todo-content {
  word-break: break-word;
}
.todo-content-done {
  color: var(--el-text-color-secondary);
  text-decoration: line-through;
}
.msg {
  display: flex;
  margin-bottom: 12px;
  position: relative;
}
/* 悬停操作条（复制/重生/删除） */
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
/* 删除按钮：悬停红色以示破坏性操作 */
.msg-action-danger:hover {
  color: var(--el-color-danger);
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
  /* 深色主题主色为亮青，白字对比度不足，用主题化对比色（见 main.css） */
  color: var(--app-accent-contrast, #fff);
  border-bottom-right-radius: 2px;
}
.msg-ai .bubble {
  background: var(--el-fill-color-light);
  color: var(--el-text-color-primary);
  border-bottom-left-radius: 2px;
}
/* 特异性高于 .msg-user .bubble / .msg-ai .bubble，无需 !important */
.msg .bubble.bubble-error {
  background: var(--el-color-danger-light-9);
  color: var(--el-color-danger);
  border: 1px solid var(--el-color-danger-light-5);
}
.error-text {
  font-weight: 600;
  margin-top: 4px;
}
.md-raw {
  white-space: pre-wrap;
  word-break: break-word;
}
/* 流式光标：随纯文本增长的末尾闪烁条 */
.stream-cursor {
  display: inline-block;
  width: 2px;
  height: 1em;
  margin-left: 2px;
  vertical-align: -0.15em;
  background: var(--el-color-primary);
  animation: stream-blink 1s steps(1) infinite;
}
@keyframes stream-blink {
  50% {
    opacity: 0;
  }
}
/* 系统注入的提醒（重复调用守卫等）：全宽灰色居中提示，与用户/助手气泡区分开 */
.system-note {
  width: 100%;
  text-align: center;
  font-size: 12px;
  line-height: 1.5;
  color: var(--el-text-color-secondary);
  background: var(--el-fill-color-light);
  border: 1px dashed var(--el-border-color-lighter);
  border-radius: 6px;
  padding: 4px 10px;
  word-break: break-word;
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
/* 代码块容器：承载右上角悬停显示的「复制」按钮 */
.md :deep(.md-code) {
  position: relative;
  margin: 6px 0;
}
/* 外边距统一交给容器，避免 pre 的 margin 让悬停区与代码块错开 */
.md :deep(.md-code pre) {
  margin: 0;
}
.md :deep(.md-code-copy) {
  position: absolute;
  top: 8px;
  right: 8px;
  /* 低于消息悬停操作条的 z-index:5，消息首行即代码块时不遮挡它 */
  z-index: 1;
  padding: 2px 8px;
  font-family: inherit;
  font-size: 11px;
  line-height: 1.5;
  color: var(--el-text-color-secondary);
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color);
  border-radius: 4px;
  opacity: 0;
  transition: opacity 0.15s;
  cursor: pointer;
}
.md :deep(.md-code:hover .md-code-copy) {
  opacity: 1;
}
.md :deep(.md-code-copy:hover) {
  color: var(--el-color-primary);
  border-color: var(--el-color-primary);
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

/* --- 输入框高度拖拽条（composer 上边缘） ---
   flex 布局中占独立一行：常态为一条细横线把手，hover / 拖拽中主色高亮。 */
.composer-resizer {
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
  height: 7px;
  cursor: row-resize;
  user-select: none;
}
.composer-resizer::before {
  content: "";
  width: 44px;
  height: 3px;
  border-radius: 2px;
  background: var(--el-border-color);
  transition: background-color 0.15s ease, width 0.15s ease;
}
.composer-resizer:hover::before,
.composer-resizer.active::before {
  width: 64px;
  background: var(--el-color-primary);
}

/* --- 输入区 --- */
.composer {
  border-top: 1px solid var(--el-border-color-lighter);
  padding: 10px 12px;
  flex-shrink: 0;
  transition: background 0.15s, box-shadow 0.15s;
}
/* 自定义高度（用户拖高输入区）：纵向弹性布局，输入框填满剩余空间并内部滚动。
   默认（无此 class）保持 autosize 2~5 行内容自适应，行为不变。 */
.composer.has-custom-height {
  display: flex;
  flex-direction: column;
  overflow: hidden;
}
.composer.has-custom-height .composer-row {
  flex: 1;
  min-height: 0;
}
.composer.has-custom-height .input-wrap {
  height: 100%;
  display: flex;
  flex-direction: column;
}
/* el-input textarea：flex 拉伸填满容器（autosize 内联高度不生效），超长内容内部滚动。 */
.composer.has-custom-height :deep(.el-textarea) {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
}
.composer.has-custom-height :deep(.el-textarea__inner) {
  flex: 1;
  min-height: 0;
  height: auto;
  overflow-y: auto;
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
/* 已选技能标签条（输入框上方简短显示，复用附加表视觉）。 */
.selected-skills {
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
/* 已附加的图片条（多模态）：缩略图 + 文件名 + 移除角标。 */
.attached-images {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 6px;
  margin-bottom: 6px;
  padding: 6px;
  background: var(--el-fill-color-light);
  border-radius: 4px;
  font-size: 12px;
}
.attached-img-item {
  position: relative;
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 2px;
  background: var(--el-bg-color);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 4px;
}
.attached-img-thumb {
  width: 36px;
  height: 36px;
  object-fit: cover;
  border-radius: 3px;
  display: block;
}
.attached-img-remove {
  position: absolute;
  top: -6px;
  right: -6px;
  font-size: 12px;
  padding: 2px;
  border-radius: 50%;
  background: var(--el-color-danger);
  color: #fff;
  cursor: pointer;
}
.attached-img-name {
  max-width: 90px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--el-text-color-secondary);
  font-size: 11px;
}
/* 图片上传按钮（信息栏最右侧，终端选择右边）：仅多模态模型显示。 */
.attach-img-btn {
  font-size: 15px;
  color: var(--el-text-color-secondary);
  padding: 2px;
  height: 22px;
  margin-left: auto; /* 靠右贴边 */
  border-radius: 4px;
  flex-shrink: 0;
}
.attach-img-btn:hover:not(:disabled) {
  color: var(--el-color-primary);
  background: var(--el-fill-color-light);
}
/* 用户消息中的图片：限制最大宽高（宽 220 / 高 160），按原比例完整展示、
   不裁剪也不留白；点击缩略图打开查看器看原图。 */
.msg-images {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-bottom: 6px;
}
.msg-img {
  max-width: 220px;
  max-height: 160px;
  width: auto;
  height: auto;
  border-radius: 6px;
  cursor: zoom-in;
  display: block;
  transition: transform 0.15s, box-shadow 0.15s;
}
.msg-img:hover {
  transform: scale(1.03);
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.25);
}
/* 输入框上方信息栏：智能体上下文（已附加终端）在左，token 用量在右 */
.composer-toolbar {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 6px;
  min-height: 24px;
}
/* 可点击的上下文提示（ssh 域 = 终端绑定入口）：hover 提示可交互 */
.ctx-tip-link {
  cursor: pointer;
  border-radius: 3px;
  padding: 1px 4px;
  margin-left: -4px;
}
.ctx-tip-link:hover {
  background: var(--el-fill-color);
  color: var(--el-color-primary);
}
.ctx-caret {
  font-size: 10px;
  color: var(--el-text-color-placeholder);
}
.ctx-tip-inline {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-size: 11px;
  color: var(--el-text-color-secondary);
  /* 占满中间剩余空间（usage-meter 靠右不被挤出） */
  flex: 1;
  min-width: 0;
  /* 截断过长的会话名，避免撑宽面板 */
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
/* 最近一次模型请求的 token 用量（ai:usage 事件覆盖写入，非累计） */
.usage-meter {
  display: inline-flex;
  align-items: center;
  gap: 3px;
  margin-left: auto; /* 靠右，不挤占上下文提示 */
  font-size: 11px;
  color: var(--el-text-color-secondary);
  white-space: nowrap;
  cursor: default;
}
.usage-meter .el-icon {
  font-size: 12px;
}
.composer-row {
  display: flex;
  align-items: flex-end;
}
/* 输入框容器：工具行悬浮在内部底部 */
.input-wrap {
  position: relative;
  flex: 1;
}
.composer-row :deep(.el-textarea__inner) {
  resize: none;
  /* 底部给内嵌工具行（模式/执行模式/模型/发送）留出空间 */
  padding-bottom: 38px;
}
/* --- 输入框内底部工具行：左=模式/执行模式，右=模型+发送，同一行 ---
   面板过窄时 flex-wrap 自动换行（模型+发送掉到第二行），不溢出。 */
.composer-inline-bar {
  position: absolute;
  left: 5px;
  right: 5px;
  bottom: 5px;
  display: flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
}
/* 中部弹性占位：把模型选择+发送推到右侧 */
.inline-spacer {
  flex: 1;
}
/* 技能按钮（+）：与 select 同高的无边框图标按钮 */
.skill-btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 24px;
  height: 24px;
  padding: 0;
  border: none;
  border-radius: var(--el-border-radius-base);
  background: transparent;
  color: var(--el-text-color-secondary);
  font-size: 14px;
  cursor: pointer;
  flex-shrink: 0;
  transition: background-color 0.15s ease, color 0.15s ease;
}
.skill-btn:hover {
  background: var(--el-fill-color);
  color: var(--el-color-primary);
}
/* 下拉中已选技能项的对勾 */
:global(.ai-skill-dropdown .skill-check) {
  color: var(--el-color-primary);
  margin-left: auto;
}
/* 选择框统一尺寸：small 高度 + 紧凑内边距；无边框融入输入框 */
.inline-select :deep(.el-select__wrapper) {
  min-height: 24px;
  padding: 0 2px 0 4px;
  gap: 2px;
  background: transparent;
  box-shadow: none !important;
  border-radius: var(--el-border-radius-base);
  transition: background-color 0.15s ease;
}
.inline-select :deep(.el-select__wrapper:hover),
.inline-select :deep(.el-select__wrapper.is-hovering) {
  background: var(--el-fill-color);
}
.inline-select :deep(.el-select__wrapper.is-focused) {
  background: var(--el-fill-color-light);
}
.inline-select :deep(.el-select__caret) {
  font-size: 12px;
}
/* 模式选择：紧凑定宽（最长选项"智能体"三字 + 箭头） */
.inline-mode-select {
  width: 72px;
  flex-shrink: 0;
}
/* 执行模式：与模式选择同宽（最长选项"白名单运行"五字收窄内边距后可容纳） */
.inline-runmode-select {
  width: 76px;
  flex-shrink: 0;
}
/* 面板过窄时执行模式切为纯图标：收窄为图标+箭头位宽并居中 */
.inline-runmode-select.is-compact {
  width: 34px;
}
.inline-runmode-select.is-compact :deep(.el-select__wrapper) {
  padding: 0 3px;
  justify-content: center;
}
.runmode-icon-label {
  display: inline-flex;
  align-items: center;
}
/* 模型选择：固定小宽度（面板默认 340px 时整行放得下），模型名超长省略 */
.inline-model-select {
  width: 104px;
  flex-shrink: 0;
}
.inline-model-select :deep(.el-select__selected-item) {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
/* 执行模式按当前值着色：白名单=主色、自动=警示橙（手动=默认色） */
.inline-runmode-select.runmode-whitelist :deep(.el-select__wrapper) {
  color: var(--el-color-primary);
}
.inline-runmode-select.runmode-auto :deep(.el-select__wrapper) {
  color: var(--el-color-warning);
  font-weight: 600;
}
/* 发送/终止按钮：工具行内的紧凑图标按钮 */
.inline-send-btn {
  font-size: 16px;
  color: var(--el-color-primary);
  padding: 2px;
  border-radius: 4px;
  flex-shrink: 0;
}
.inline-send-btn:disabled {
  color: var(--el-text-color-placeholder);
  background: transparent;
  cursor: not-allowed;
}
.inline-send-btn:hover:not(:disabled) {
  background: var(--el-fill-color-light);
}
.inline-send-btn.stop-btn {
  color: var(--el-color-danger);
}
/* --- 下拉项：执行模式（标签 + 简短说明纵向两行） ----------------------------
   el-option 插槽内容由本组件渲染，scoped 可命中；popper 挂 body 的部分用 :global。 */
.runmode-option {
  display: flex;
  flex-direction: column;
  line-height: 1.4;
  padding: 4px 0;
}
.runmode-option .runmode-desc {
  font-size: 11px;
  color: var(--el-text-color-placeholder);
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
  flex-wrap: wrap;
  gap: 4px;
  padding: 4px 8px 6px;
}
/* Element Plus 相邻按钮自带 margin-left:12px，与 gap 叠加后实际间距 18px——
   窄面板下三个按钮放不下，"拒绝"被挤出可视区。间距统一交给上面的 gap。 */
.tool-actions .el-button + .el-button {
  margin-left: 0;
}
.tool-status {
  padding: 4px 8px 6px;
  font-size: 11px;
  color: var(--el-text-color-secondary);
}
.tool-status-fail {
  color: var(--el-color-danger);
}
/* ask_user_question 问题表单：问题卡片化（序号/标题/多选标记），选项为可点选
   卡片（原生 input 视觉隐藏 + 自绘单选圆点/多选方框），提交后整体置灰并显示
   回答摘要 chips。 */
.ask-form {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.ask-q {
  padding: 8px 10px;
  background: var(--el-fill-color-light);
  border-radius: 8px;
}
/* 问题元信息行：序号 + 短标题 + 多选标记 */
.ask-q-head {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-bottom: 4px;
  min-height: 18px;
}
.ask-q-index {
  font-size: 11px;
  color: var(--el-text-color-secondary);
  background: var(--el-fill-color);
  border-radius: 4px;
  padding: 0 6px;
  line-height: 18px;
  flex-shrink: 0;
}
.ask-header {
  font-size: 11px;
  font-weight: 600;
  color: var(--el-color-warning);
  background: var(--el-color-warning-light-9);
  border-radius: 4px;
  padding: 0 6px;
  line-height: 18px;
  max-width: 60%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ask-multi-tag {
  font-size: 11px;
  color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
  border: 1px dashed var(--el-color-primary-light-5);
  border-radius: 4px;
  padding: 0 6px;
  line-height: 16px;
  flex-shrink: 0;
}
.ask-question {
  font-size: 13px;
  font-weight: 500;
  color: var(--el-text-color-primary);
  line-height: 1.5;
  white-space: pre-wrap;
  word-break: break-word;
}
.ask-options {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin: 8px 0 2px;
}
/* 选项卡片：整卡可点，选中高亮边框 + 主色浅底 */
.ask-option {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  font-size: 12px;
  cursor: pointer;
  padding: 6px 8px;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  background: var(--el-fill-color-blank);
  transition:
    border-color 0.15s ease,
    background-color 0.15s ease;
}
.ask-option:hover {
  border-color: var(--el-color-primary-light-5);
}
.ask-option:has(input:checked) {
  border-color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
}
/* 键盘焦点环（input 视觉隐藏后由卡片承接焦点可见性） */
.ask-option:has(input:focus-visible) {
  outline: 2px solid var(--el-color-primary-light-5);
  outline-offset: 1px;
}
/* 原生 input 移出视觉流（保留可点/可聚焦/表单语义，宽度塌缩不影响布局） */
.ask-option input {
  position: absolute;
  width: 0;
  height: 0;
  margin: 0;
  opacity: 0;
}
/* 自绘勾选标记：单选 = 圆点，多选 = 方框对勾 */
.ask-opt-box {
  flex-shrink: 0;
  position: relative;
  width: 14px;
  height: 14px;
  margin-top: 2px;
  border: 1px solid var(--el-border-color);
  border-radius: 50%;
  background: var(--el-fill-color-blank);
  transition:
    border-color 0.15s ease,
    background-color 0.15s ease;
}
.ask-opt-box.multi {
  border-radius: 4px;
}
.ask-option input:checked + .ask-opt-box {
  border-color: var(--el-color-primary);
}
/* 单选选中：内部实心圆点 */
.ask-option input:checked + .ask-opt-box::after {
  content: "";
  position: absolute;
  inset: 3px;
  border-radius: 50%;
  background: var(--el-color-primary);
}
/* 多选选中：主色底 + 白色对勾 */
.ask-option input:checked + .ask-opt-box.multi {
  background: var(--el-color-primary);
}
.ask-option input:checked + .ask-opt-box.multi::after {
  content: "✓";
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 10px;
  line-height: 1;
  color: #fff;
}
.ask-opt-body {
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.ask-opt-label {
  color: var(--el-text-color-primary);
  line-height: 1.5;
  word-break: break-word;
}
/* 推荐标记：提示词要求推荐项放第一位并在 label 末尾加 (Recommended)，
   展示为绿色小标签替代英文后缀 */
.ask-opt-rec {
  font-size: 10px;
  color: var(--el-color-success);
  background: var(--el-color-success-light-9);
  border: 1px solid var(--el-color-success-light-7);
  border-radius: 3px;
  padding: 0 4px;
  margin-left: 4px;
  vertical-align: 1px;
}
.ask-opt-desc {
  color: var(--el-text-color-secondary);
  font-size: 11px;
  line-height: 1.4;
  word-break: break-word;
}
.ask-input {
  margin-top: 6px;
}
/* 提交/取消后的终态：问题区置灰只读（inputs 已 disabled），摘要 chips 展示已选 */
.ask-form.is-done .ask-q {
  opacity: 0.75;
}
.ask-form.is-done .ask-option {
  cursor: default;
}
.ask-answered {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
}
.ask-answered-chip {
  font-size: 11px;
  color: var(--el-color-success);
  background: var(--el-color-success-light-9);
  border: 1px solid var(--el-color-success-light-7);
  border-radius: 10px;
  padding: 1px 8px;
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ask-actions {
  display: flex;
  gap: 6px;
  justify-content: flex-end;
}
</style>
