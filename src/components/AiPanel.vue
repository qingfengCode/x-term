<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { Promotion, Delete, ChatDotRound, DArrowRight, Connection, Tools, ArrowDown, Plus, Close, CopyDocument, RefreshRight, VideoPause, Document, Loading, Download, MagicStick, Collection, Picture } from "@element-plus/icons-vue";
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
import { executeDesktopTool, aiDesktopToolRespond, getDesktopControl } from "@/api/desktopControl";
import type { ToolCallItem } from "@/stores/ai";
import { bytesToBase64 } from "@/utils/binary";
import type { ImagePart } from "@/api/types";
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
    if (activeTerminalId.value) {
      const tab = terminals.tabs.find((t) => t.instanceId === activeTerminalId.value);
      if (tab) parts.push(`终端: ${tab.session.name}`);
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
    return parts.length
      ? `已附加: ${parts.join("、")}`
      : "未选择活动终端，请先连接后 AI 才能操作";
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

/** 输入框粘贴：剪贴板含图片时附加（不拦截纯文本粘贴）。 */
function onComposerPaste(e: ClipboardEvent) {
  const items = e.clipboardData?.items ?? [];
  const imageItem = Array.from(items).find((it) => it.type.startsWith("image/"));
  const file = imageItem?.getAsFile();
  if (file) {
    e.preventDefault();
    void attachFile(file);
  }
}

/** 拖入文件（图片）：与拖表共用 drop 通道，互不干扰。 */
function onComposerDropFiles(e: DragEvent) {
  const files = Array.from(e.dataTransfer?.files ?? []);
  for (const f of files) void attachFile(f);
}

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
  const hasImageFile = Array.from(e.dataTransfer?.files ?? []).some((f) =>
    f.type.startsWith("image/")
  );
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
  void scrollToBottom();
}

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
    prompt += buildSkillsSection();
    return { prompt, opts: { agent: false, domain: props.domain } };
  }

  // 动态构建系统提示：把当前活动上下文的真实 id 注入，让模型直接填对参数。
  // 按域裁剪：SSH 面板只传 terminalId（后端就只暴露 SSH 工具），
  // DB 面板只传 dbConnId（后端就只暴露 SQL 工具），
  // desktop 面板只传 desktopId（后端就只暴露桌面工具），实现工具集硬隔离。
  let prompt = SYSTEM_PROMPTS.agent;
  const ctxParts: string[] = [];
  let activeTerminal: string | undefined;
  let activeDb: string | undefined;
  let activeDesktop: string | undefined;
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
  } else if (props.domain === "db") {
    if (db.activeConnId) {
      const tab = db.activeTab;
      const name = tab?.profileName ?? "未命名";
      ctxParts.push(
        `当前活动 MySQL 连接：dbConnId="${db.activeConnId}"（${name}）。调用 exec_sql / list_db_tables / describe_table 时直接用这个 dbConnId。`
      );
      activeDb = db.activeConnId;
      // 注入当前绑定库（点库/表或拖表时设置），让 AI 默认在该库 schema 下操作。
      if (db.activeDatabase) {
        ctxParts.push(
          `当前库（schema）为 "${db.activeDatabase}"，该连接已自动 USE 此库。` +
            `执行 SQL 时直接引用表名（如 \`表名\`）即可，不要加库前缀；` +
            `确需跨库时才用 \`${db.activeDatabase}\`.\`表名\` 限定。`
        );
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
  // 已启用的可复用 skill（历史总结沉淀）。
  prompt += buildSkillsSection();
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
  const ctx = buildSendContext(mode.value === "agent");
  await ai.send(text, ctx.prompt, { ...ctx.opts, images });
  // 发送后强制跟随滚动到底部（用户主动发送，应看到自己的消息与回复开始；
  // 若此刻正在上翻浏览，也以发送为准回到最新位置）。
  void scrollToBottom();
  // 发送后清空附加表与图片（下次提问重新拖/选）。
  clearAttachedTables();
  attachedImages.value = [];
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

/** 拼接已启用的 skill 段落（注入 system prompt）。无启用 skill 返回空串。 */
function buildSkillsSection(): string {
  const skills = settings.skills.filter((s) => s.domain === props.domain && s.enabled);
  if (skills.length === 0) return "";
  const blocks = skills.map((s) => `【${s.title}】\n${s.content}`);
  return "\n\n=== 可复用技能（来自历史总结，处理同类任务时请遵循）===\n" + blocks.join("\n\n");
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
  try {
    await ai.regenerate(m.id, ctx.prompt, ctx.opts);
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
          <el-tooltip content="总结成技能" placement="bottom">
            <el-button class="icon-btn" link :disabled="ai.messages.length === 0 || ai.sending || summarizing" :loading="summarizing" @click="handleSummarizeSkill">
              <el-icon v-if="!summarizing"><MagicStick /></el-icon>
            </el-button>
          </el-tooltip>
          <el-tooltip content="技能管理" placement="bottom">
            <el-button class="icon-btn" link @click="skillManagerVisible = true">
              <el-icon><Collection /></el-icon>
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
            <!-- 用户消息：图片（多模态）+ 文本。
                 图片限制最大宽高、按原比例完整展示（不裁剪），点击打开查看器看大图。 -->
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
        <div class="composer-row">
          <!-- 图片上传入口：仅激活模型开启多模态时显示 -->
          <el-button
            v-if="multimodalEnabled"
            link
            :icon="Picture"
            class="attach-img-btn"
            :disabled="ai.sending || configBlocked"
            :loading="imageReading"
            title="附带图片（也可直接粘贴或拖入）"
            @click="pickImage"
          />
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
            <!-- 发送 / 终止：输入框内右下角悬浮图标，发送中变为红色终止图标 -->
            <el-button
              v-if="!ai.sending"
              link
              :icon="Promotion"
              :disabled="(!inputText.trim() && attachedImages.length === 0) || configBlocked"
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

<style scoped>
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
/* 输入框左侧的图片上传按钮。 */
.attach-img-btn {
  font-size: 17px;
  color: var(--el-color-primary);
  padding: 6px;
  margin-bottom: 2px;
  border-radius: 4px;
  flex-shrink: 0;
}
.attach-img-btn:hover:not(:disabled) {
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
