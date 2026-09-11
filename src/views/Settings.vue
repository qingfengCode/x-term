<!--
  Settings.vue — 应用设置视图

  两个 Tab：
  1. 终端：主题 / 字体 / 字号 / 行高 / 滚屏 / 选中复制 / 启用WebGL / SSH 空闲断开时间 / SSH 保活间隔 / SSH 连接超时
  2. AI 助手（BYOK 多模型）：管理 provider 列表、设置激活模型

  所有改动通过 settingsStore.save() 持久化；主题切换同时切换 document.documentElement 的 'dark' class。
-->
<script setup lang="ts">
import { computed, h, onMounted, reactive, ref, watch } from "vue";
import { onBeforeRouteLeave } from "vue-router";
import { ElMessage, ElMessageBox } from "element-plus";
import type { FormInstance, FormRules, TabsInstance } from "element-plus";
import { Delete, Plus, Refresh, Folder, Setting, Download, Upload, FolderOpened } from "@element-plus/icons-vue";
import HelpTip from "@/components/HelpTip.vue";
import { open, save } from "@tauri-apps/plugin-dialog";
import { setWorkspaceDir } from "@/api/ai";
import * as backupApi from "@/api/backup";
import type { BackupInfo } from "@/api/backup";
import { openLogsDir } from "@/api/config";
import { localTerminalShells } from "@/api/local";
import type { LocalShellInfo } from "@/api/local";
import { zmodemPickFolder } from "@/api/terminal";
import { useSettingsStore } from "@/stores/settings";
import { useUpdateStore } from "@/stores/update";
import { useVaultStore } from "@/stores/vault";
import { useMcpStore } from "@/stores/mcp";
import { TERMINAL_COLOR_SCHEMES, defaultSchemeFor } from "@/utils/terminalThemes";
import { ProviderKind, PROVIDER_DEFAULTS } from "@/api/types";
import {
  APP_SHORTCUT_METAS,
  RUN_MODE_OPTIONS,
  SQL_MODE_OPTIONS,
  DESKTOP_CLIENT_MODE_OPTIONS,
  defaultAppShortcuts,
} from "@/api/types";
import { eventToCombo, isModifierOnly } from "@/utils/shortcut";
import type {
  AppShortcutAction,
  AppShortcuts,
  ProviderConfig,
  ProviderKind as ProviderKindType,
  ShortcutCommand,
  TerminalSettings,
  ToolRunMode,
} from "@/api/types";

const settings = useSettingsStore();
const updater = useUpdateStore();
const vaultStore = useVaultStore();
const mcpStore = useMcpStore();
const activeTab = ref<
  "terminal" | "shortcuts" | "appShortcuts" | "ai" | "security" | "dataMigration" | "about"
>("terminal");
/** Tab 容器实例（切换 Tab 时把内容区滚动回顶部）。 */
const tabsRef = ref<TabsInstance | null>(null);

// --- 关于 / 更新 -----------------------------------------------------------

/** 字节数格式化为人类可读单位。 */
function formatBytes(n: number): string {
  if (!n) return "0 B";
  // 升级包可能超过 1TB（大体积应用），缺 TB 会把 1.5TB 显示成 "1536.0 GB"。
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}

/** 安装前二次确认（会退出应用）。 */
async function confirmInstall() {
  try {
    await ElMessageBox.confirm(
      "安装将退出当前应用并启动安装程序，未保存的会话将断开。继续？",
      "安装确认",
      { type: "warning", confirmButtonText: "安装并重启", cancelButtonText: "取消" },
    );
  } catch {
    return;
  }
  void updater.install();
}

// 切到关于页时按需加载应用信息与更新源；切到数据迁移页时刷新保险库状态。
watch(activeTab, (tab) => {
  if (tab === "about") loadAbout();
  if (tab === "dataMigration") void vaultStore.refresh();
  // 切换 Tab 后回到内容区顶部，避免停留在上个 Tab 的滚动位置。
  tabsRef.value?.$el.querySelector(".el-tabs__content")?.scrollTo(0, 0);
});
async function loadAbout() {
  await updater.loadInfo();
}

// --- 应用快捷键（独立 tab） ----------------------------------------------
// 本地副本：编辑期间不立即写 store，点"保存"才回写。
const appForm = reactive<AppShortcuts>({ ...defaultAppShortcuts(), ...settings.appShortcuts });
/** 当前正在录键的动作（同一时间只录一个）；null 表示未在录键。 */
const recordingAction = ref<AppShortcutAction | null>(null);
/** 录键时实时显示的提示文本。 */
const recordingText = ref("");

/** 应用快捷键列表（元信息 + 当前绑定 + 冲突标记）。 */
const appRows = computed(() =>
  APP_SHORTCUT_METAS.map((m) => {
    const key = appForm[m.action] ?? "";
    // 冲突检测：同一组合键被多个动作绑定，或与自定义快捷命令的 shortcut 重复。
    const dupAction = APP_SHORTCUT_METAS.some(
      (o) => o.action !== m.action && (appForm[o.action] ?? "") === key && key
    );
    const dupCmd = settings.shortcuts.some((s) => s.shortcut && s.shortcut === key);
    return { ...m, key, conflict: Boolean(key) && (dupAction || dupCmd) };
  })
);

/** 开始为某个动作录键。 */
function startRecord(action: AppShortcutAction) {
  recordingAction.value = action;
  recordingText.value = "按下组合键…（Esc 取消，Backspace 清除）";
}

/** 录键事件处理：绑定到录入控件的 keydown。 */
function onRecordKeydown(e: KeyboardEvent) {
  const action = recordingAction.value;
  if (!action) return;
  e.preventDefault();
  e.stopPropagation();
  // Esc：取消录入，保留原值。
  if (e.key === "Escape") {
    recordingAction.value = null;
    recordingText.value = "";
    return;
  }
  // Backspace（无修饰）：清除该动作绑定。
  if (e.key === "Backspace" && !e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey) {
    appForm[action] = "";
    recordingAction.value = null;
    recordingText.value = "";
    return;
  }
  const combo = eventToCombo(e);
  // 纯修饰键不结束录入，等用户按主键。
  if (isModifierOnly(combo)) {
    recordingText.value = `${combo}+ ?`;
    return;
  }
  appForm[action] = combo;
  recordingAction.value = null;
  recordingText.value = "";
}

/** 保存应用快捷键。 */
/** 终端操作开关（快捷键 tab）：即时写回 store 并保存。 */
function setTermOp(
  key: "copyOnSelect" | "rightClickPaste" | "suggestHistory" | "suggestAi" | "pasteConfirm",
  v: boolean,
) {
  settings.setTerminal({ [key]: v });
  void settings.save().catch(() => {
    /* 保存失败不阻断开关效果（内存已生效），下次保存会补写 */
  });
}

/** 打开终端输出日志目录。 */
async function onOpenLogsDir() {
  try {
    await openLogsDir();
  } catch (e) {
    ElMessage.error("打开日志目录失败: " + String(e));
  }
}

async function saveAppShortcuts() {
  // 冲突提示（不阻断保存，仅提醒）。
  const conflicts = appRows.value.filter((r) => r.conflict);
  if (conflicts.length > 0) {
    try {
      await ElMessageBox.confirm(
        `检测到 ${conflicts.length} 个快捷键冲突，仍要保存吗？`,
        "快捷键冲突",
        { type: "warning", confirmButtonText: "仍然保存", cancelButtonText: "返回修改" }
      );
    } catch {
      return; // 用户选择返回修改。
    }
  }
  // 写回 store。
  for (const m of APP_SHORTCUT_METAS) {
    settings.setAppShortcut(m.action, appForm[m.action] ?? "");
  }
  try {
    await settings.save();
    ElMessage.success("快捷键已保存");
  } catch (e) {
    ElMessage.error(String(e));
  }
}

/** 重置为默认。 */
function resetAppShortcuts() {
  Object.assign(appForm, defaultAppShortcuts());
}

// 终端表单（与 store 解耦的本地副本，点"应用"才写回；desktopClients 嵌套对象单独拷贝，
// 避免改下拉直接改到 store 里，导致"重置"失效）
const termForm = reactive<TerminalSettings>({
  ...settings.terminal,
  desktopClients: { ...settings.terminal.desktopClients },
});

/** 本机可用的本地 shell 列表（本地终端默认 Shell 下拉；不可用项隐藏）。 */
const localShells = ref<LocalShellInfo[]>([]);

/** 可选的本地 shell（过滤掉本机不可用的）。 */
const localShellOptions = computed(() => localShells.value.filter((s) => s.available));

/** 终端字体预设（等宽字体，value 为完整 font-family 栈，label 为显示名）。 */
const FONT_PRESETS: { label: string; value: string }[] = [
  { label: "Consolas（推荐）", value: "Consolas, 'Cascadia Code', 'Courier New', monospace" },
  { label: "Cascadia Code", value: "'Cascadia Code', Consolas, monospace" },
  { label: "Courier New", value: "'Courier New', monospace" },
  { label: "JetBrains Mono", value: "'JetBrains Mono', Consolas, monospace" },
  { label: "Fira Code", value: "'Fira Code', Consolas, monospace" },
  { label: "Source Code Pro", value: "'Source Code Pro', Consolas, monospace" },
  { label: "Menlo", value: "Menlo, Consolas, monospace" },
  { label: "Monaco", value: "Monaco, Consolas, monospace" },
  { label: "DejaVu Sans Mono", value: "'DejaVu Sans Mono', Consolas, monospace" },
];

/** 下拉选项：预设列表 + 旧版输入的自定义字体（不在预设中时保留显示，避免选中态丢失）。 */
const fontOptions = computed(() => {
  if (!termForm.fontFamily || FONT_PRESETS.some((f) => f.value === termForm.fontFamily)) {
    return FONT_PRESETS;
  }
  return [...FONT_PRESETS, { label: `自定义：${termForm.fontFamily}`, value: termForm.fontFamily }];
});

/** 终端字符编码选项（远端服务器 locale；输出前端解码 / 输入后端转码）。 */
const ENCODING_OPTIONS: { label: string; value: string }[] = [
  { label: "UTF-8（默认）", value: "utf-8" },
  { label: "GBK（简体中文）", value: "gbk" },
  { label: "GB18030（简体中文超集）", value: "gb18030" },
  { label: "Big5（繁体中文）", value: "big5" },
  { label: "Shift-JIS（日文）", value: "shift-jis" },
  { label: "EUC-KR（韩文）", value: "euc-kr" },
];

// 添加/编辑 provider 弹窗
const providerDialogVisible = ref(false);
const providerFormRef = ref<FormInstance>();
/** 编辑模式：正在编辑的 provider 下标；null 表示新增。 */
const editingIndex = ref<number | null>(null);
const providerForm = reactive<{
  kind: ProviderKindType;
  baseUrl: string;
  apiKey: string;
  model: string;
  maxOutput: number;
  contextWindow: number;
  maxToolCalls: number;
  temperature: number | null;
  connectTimeoutSecs: number;
  readTimeoutSecs: number;
  multimodal: boolean;
}>({
  kind: ProviderKind.OpenAi,
  baseUrl: defaultBaseUrl(ProviderKind.OpenAi),
  apiKey: "",
  model: "",
  maxOutput: PROVIDER_DEFAULTS.maxOutput,
  contextWindow: PROVIDER_DEFAULTS.contextWindow,
  maxToolCalls: PROVIDER_DEFAULTS.maxToolCalls,
  temperature: null,
  connectTimeoutSecs: PROVIDER_DEFAULTS.connectTimeoutSecs,
  readTimeoutSecs: PROVIDER_DEFAULTS.readTimeoutSecs,
  multimodal: false,
});

const providerRules: FormRules = {
  kind: [{ required: true, message: "请选择类型", trigger: "change" }],
  baseUrl: [{ required: false }],
  apiKey: [{ required: true, message: "请输入 API Key", trigger: "blur" }],
  model: [{ required: true, message: "请输入模型名", trigger: "blur" }],
  maxOutput: [
    { required: true, message: "请输入最大输出", trigger: "blur" },
    { type: "number", min: 1, max: 1000000, message: "1 ~ 1000000", trigger: "blur" },
  ],
  contextWindow: [
    { required: true, message: "请输入上下文大小", trigger: "blur" },
    { type: "number", min: 1, max: 10000000, message: "1 ~ 10000000", trigger: "blur" },
  ],
  maxToolCalls: [
    { required: true, message: "请输入工具调用数", trigger: "blur" },
    { type: "number", min: 1, max: 1000, message: "1 ~ 1000", trigger: "blur" },
  ],
  temperature: [
    { type: "number", min: 0, max: 2, message: "0 ~ 2（留空则不发送）", trigger: "blur" },
  ],
  connectTimeoutSecs: [
    { required: true, message: "请输入建连超时", trigger: "blur" },
    { type: "number", min: 1, max: 3600, message: "1 ~ 3600 秒", trigger: "blur" },
  ],
  readTimeoutSecs: [
    { required: true, message: "请输入读取超时", trigger: "blur" },
    { type: "number", min: 1, max: 3600, message: "1 ~ 3600 秒", trigger: "blur" },
  ],
};

// ProviderKind 选项
const providerKindOptions: { value: ProviderKindType; label: string }[] = [
  { value: ProviderKind.OpenAi, label: "OpenAI" },
  { value: ProviderKind.DeepSeek, label: "DeepSeek" },
  { value: ProviderKind.Zhipu, label: "智谱 (Zhipu)" },
  { value: ProviderKind.Ollama, label: "Ollama" },
  { value: ProviderKind.OpenAiCompatible, label: "OpenAI 兼容" },
];

function defaultBaseUrl(kind: ProviderKindType): string {
  switch (kind) {
    case ProviderKind.OpenAi:
      return "https://api.openai.com/v1";
    case ProviderKind.DeepSeek:
      return "https://api.deepseek.com/v1";
    case ProviderKind.Zhipu:
      return "https://open.bigmodel.cn/api/paas/v4";
    case ProviderKind.Ollama:
      return "http://localhost:11434/v1";
    case ProviderKind.OpenAiCompatible:
    default:
      return "";
  }
}

function kindLabel(kind: string): string {
  return providerKindOptions.find((o) => o.value === kind)?.label ?? kind;
}

// 当切换 provider 类型时，如果 baseUrl 为空或仍是上一个默认值，则自动填新默认值
function onProviderKindChange(kind: ProviderKindType) {
  const prev = providerForm.baseUrl;
  // 若为空，或是任一已知默认值（即用户未自定义），自动更新
  const allDefaults = new Set(providerKindOptions.map((o) => defaultBaseUrl(o.value)));
  if (!prev || allDefaults.has(prev)) {
    providerForm.baseUrl = defaultBaseUrl(kind);
  }
}

function openProviderDialog() {
  editingIndex.value = null;
  providerForm.kind = ProviderKind.OpenAi;
  providerForm.baseUrl = defaultBaseUrl(ProviderKind.OpenAi);
  providerForm.apiKey = "";
  providerForm.model = "";
  providerForm.maxOutput = PROVIDER_DEFAULTS.maxOutput;
  providerForm.contextWindow = PROVIDER_DEFAULTS.contextWindow;
  providerForm.maxToolCalls = PROVIDER_DEFAULTS.maxToolCalls;
  providerForm.temperature = null;
  providerForm.connectTimeoutSecs = PROVIDER_DEFAULTS.connectTimeoutSecs;
  providerForm.readTimeoutSecs = PROVIDER_DEFAULTS.readTimeoutSecs;
  providerForm.multimodal = false;
  providerDialogVisible.value = true;
}

/** 编辑已有 provider：回填表单（apiKey 为明文，方便修改）。 */
function openEditProvider(p: ProviderConfig, index: number) {
  editingIndex.value = index;
  providerForm.kind = p.kind;
  providerForm.baseUrl = p.baseUrl;
  providerForm.apiKey = p.apiKey;
  providerForm.model = p.model;
  providerForm.maxOutput = p.maxOutput ?? PROVIDER_DEFAULTS.maxOutput;
  providerForm.contextWindow = p.contextWindow ?? PROVIDER_DEFAULTS.contextWindow;
  providerForm.maxToolCalls = p.maxToolCalls ?? PROVIDER_DEFAULTS.maxToolCalls;
  providerForm.temperature = p.temperature ?? null;
  providerForm.connectTimeoutSecs = p.connectTimeoutSecs ?? PROVIDER_DEFAULTS.connectTimeoutSecs;
  providerForm.readTimeoutSecs = p.readTimeoutSecs ?? PROVIDER_DEFAULTS.readTimeoutSecs;
  providerForm.multimodal = p.multimodal ?? false;
  providerDialogVisible.value = true;
}

async function submitProvider() {
  if (!providerFormRef.value) return;
  try {
    await providerFormRef.value.validate();
  } catch {
    return;
  }
  const cfg: ProviderConfig = {
    kind: providerForm.kind,
    baseUrl: providerForm.baseUrl.trim(),
    apiKey: providerForm.apiKey.trim(),
    model: providerForm.model.trim(),
    maxOutput: providerForm.maxOutput,
    contextWindow: providerForm.contextWindow,
    maxToolCalls: providerForm.maxToolCalls,
    temperature: providerForm.temperature,
    connectTimeoutSecs: providerForm.connectTimeoutSecs,
    readTimeoutSecs: providerForm.readTimeoutSecs,
    multimodal: providerForm.multimodal,
  };
  if (editingIndex.value !== null) {
    // 编辑模式：按下标覆盖。
    const idx = editingIndex.value;
    const old = settings.aiProviders[idx];
    settings.aiProviders[idx] = cfg;
    // 激活项标识是 `${kind}:${model}`，若编辑后变了则同步更新。
    if (settings.aiActive === `${old.kind}:${old.model}`) {
      settings.aiActive = `${cfg.kind}:${cfg.model}`;
    }
  } else {
    // 新增模式：同 kind+model 视为重复，覆盖 apiKey/baseUrl 及参数
    const idx = settings.aiProviders.findIndex(
      (p) => p.kind === cfg.kind && p.model === cfg.model,
    );
    if (idx >= 0) {
      settings.aiProviders[idx] = cfg;
    } else {
      settings.aiProviders.push(cfg);
    }
    // 若尚无激活模型，自动设为激活
    if (!settings.aiActive) {
      settings.aiActive = `${cfg.kind}:${cfg.model}`;
    }
  }
  try {
    await settings.save();
    ElMessage.success(editingIndex.value !== null ? "已保存修改" : "已添加 provider");
    providerDialogVisible.value = false;
  } catch (e: any) {
    ElMessage.error("保存失败：" + (e?.message ?? String(e)));
  }
}

async function removeProvider(p: ProviderConfig) {
  try {
    await ElMessageBox.confirm(
      `确定删除 ${kindLabel(p.kind)} / ${p.model} 吗？`,
      "删除确认",
      { type: "warning", confirmButtonText: "删除", cancelButtonText: "取消" },
    );
  } catch {
    return;
  }
  const key = `${p.kind}:${p.model}`;
  settings.aiProviders = settings.aiProviders.filter(
    (x) => !(x.kind === p.kind && x.model === p.model),
  );
  if (settings.aiActive === key) {
    settings.aiActive = settings.aiProviders.length
      ? `${settings.aiProviders[0].kind}:${settings.aiProviders[0].model}`
      : null;
  }
  try {
    await settings.save();
    ElMessage.success("已删除");
  } catch (e: any) {
    ElMessage.error("保存失败：" + (e?.message ?? String(e)));
  }
}

async function setActive(p: ProviderConfig) {
  settings.aiActive = `${p.kind}:${p.model}`;
  try {
    await settings.save();
    ElMessage.success(`已设为默认模型：${kindLabel(p.kind)} / ${p.model}`);
  } catch (e: any) {
    ElMessage.error("保存失败：" + (e?.message ?? String(e)));
  }
}

// 终端主题切换：实时切换 documentElement 的 dark class（预览效果）
watch(
  () => termForm.theme,
  (val, old) => {
    toggleDarkClass(val);
    // 明暗切换时，配色方案跟随调整：未选或与切换前明暗同组的方案换成新明暗的默认。
    if (val === old) return;
    const cur = TERMINAL_COLOR_SCHEMES.find((s) => s.id === termForm.colorScheme);
    if (!cur || cur.scheme === old) {
      termForm.colorScheme = defaultSchemeFor(val);
    }
  },
);

function toggleDarkClass(theme: string) {
  const root = document.documentElement;
  if (theme === "dark") root.classList.add("dark");
  else root.classList.remove("dark");
}

// 主题是否已修改但未点"应用"（本地副本与已持久化的 store 值不一致）。
// 主题选择会立即预览（全局切换 dark class），但只有点"应用"才写入
// settings.json——若未应用就离开，下次进入设置页会被真实值覆盖，
// 表现为"设置了浅色又变回深色"。故离开前提醒。
const themePending = computed(() => termForm.theme !== settings.terminal.theme);

// 离开设置页时拦截：存在未应用的主题变更则弹窗确认，防止用户以为
// 预览即已保存。选择"放弃更改"时同时撤销预览残留，恢复为已保存主题，
// 否则整个应用会停留在未保存的预览主题上，下次进入设置页再次"跳变"。
onBeforeRouteLeave(() => {
  if (!themePending.value) return true;
  const pending = termForm.theme === "dark" ? "深色" : "浅色";
  const saved = settings.terminal.theme === "dark" ? "深色" : "浅色";
  return ElMessageBox.confirm(
    `主题已切换为「${pending}」但尚未应用，离开后将恢复为「${saved}」。是否放弃更改？`,
    "主题未应用",
    { type: "warning", confirmButtonText: "放弃更改", cancelButtonText: "留在设置页" },
  )
    .then(() => {
      toggleDarkClass(settings.terminal.theme);
      return true;
    })
    .catch(() => false);
});

async function applyTerminal() {
  settings.setTerminal({ ...termForm });
  toggleDarkClass(termForm.theme);
  try {
    await settings.save();
    ElMessage.success("终端设置已应用");
  } catch (e: any) {
    ElMessage.error("保存失败：" + (e?.message ?? String(e)));
  }
}

function resetTerminal() {
  Object.assign(termForm, settings.terminal);
  termForm.desktopClients = { ...settings.terminal.desktopClients };
  toggleDarkClass(termForm.theme);
}

// 标题栏「下载」抽屉可直达修改默认下载目录（不走本表单）：表单值跟随同步，
// 避免用户在设置页打开抽屉改了目录后点「应用」，旧快照把新值整体覆盖回空。
watch(
  () => settings.terminal.downloadDir,
  (v) => {
    termForm.downloadDir = v;
  },
);

/** 选择默认下载目录（无父窗口原生目录框，规避光标消失问题）。 */
async function pickZmodemDownloadDir() {
  try {
    const dir = await zmodemPickFolder("选择默认下载目录");
    if (dir) termForm.downloadDir = dir;
  } catch {
    // 后端错误按取消处理
  }
}

// --- 快捷命令 / 快捷键 ---------------------------------------------------
function addShortcut() {
  settings.addShortcut();
}

/** 快捷命令按分组聚合（空分组归「默认」），用于分组展示。 */
const shortcutGroupsView = computed(() => {
  const map = new Map<string, ShortcutCommand[]>();
  for (const sc of settings.shortcuts) {
    const g = sc.group?.trim() || "";
    if (!map.has(g)) map.set(g, []);
    map.get(g)!.push(sc);
  }
  return [...map.entries()].map(([group, items]) => ({ group, items }));
});

function removeShortcut(id: string) {
  settings.removeShortcut(id);
}

/** 分组选择变更时，若为新分组则自动加入分组列表。 */
function onGroupChange(sc: ShortcutCommand, val: string) {
  if (val && !settings.shortcutGroups.includes(val)) {
    settings.addShortcutGroup(val);
  }
}

async function saveShortcuts() {
  // 校验：label 和 command 不能同时为空。
  const invalid = settings.shortcuts.find((s) => !s.label.trim() && !s.command.trim());
  if (invalid) {
    ElMessage.warning("存在名称和命令均为空的快捷命令，请填写或删除");
    return;
  }
  try {
    await settings.save();
    ElMessage.success("快捷命令已保存");
  } catch (e: any) {
    ElMessage.error("保存失败：" + (e?.message ?? String(e)));
  }
}

/**
 * 在快捷键输入框内按下组合键时，自动捕获并填入。
 * 例如按 Ctrl+Shift+R → 填入 "Ctrl+Shift+R"。
 */
function onShortcutKeyCapture(sc: ShortcutCommand, e: KeyboardEvent) {
  // 单独的修饰键按下不视为完整快捷键。
  const modifiers = ["Control", "Shift", "Alt", "Meta"];
  if (modifiers.includes(e.key)) return;
  const parts: string[] = [];
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.shiftKey) parts.push("Shift");
  if (e.altKey) parts.push("Alt");
  if (e.metaKey) parts.push("Meta");
  // 主键：字母大写、功能键原样、数字原样。
  let main = e.key;
  if (main.length === 1) main = main.toUpperCase();
  parts.push(main);
  e.preventDefault();
  e.stopPropagation();
  sc.shortcut = parts.join("+");
}

onMounted(async () => {
  if (!settings.loaded) {
    try {
      await settings.load();
    } catch {
      /* ignore */
    }
  }
  Object.assign(termForm, settings.terminal);
  termForm.desktopClients = { ...settings.terminal.desktopClients };
  toggleDarkClass(termForm.theme);
  // 同步 SSH 命令白名单到 textarea 文本（每行一条）。
  whitelistText.value = settings.sshAgent.commandWhitelist.join("\n");
  // 同步应用快捷键到本地副本。
  Object.assign(appForm, defaultAppShortcuts(), settings.appShortcuts);
  // 检测本机可用的本地 shell（本地终端默认 Shell 下拉）。
  try {
    localShells.value = await localTerminalShells();
  } catch {
    /* 检测失败则下拉为空，仅影响展示 */
  }
});

// --- SSH 智能体：命令白名单编辑 -------------------------------------------
// 用多行文本编辑：每行一个命令前缀。失焦/保存时同步到 settings store。
const whitelistText = ref("");

/** SSH 开关（运行模式/终端可视化）切换后立即保存。 */
async function saveSshSwitches() {
  try {
    await settings.save();
  } catch (e: unknown) {
    ElMessage.error("保存失败：" + String(e));
  }
}

/** 安全开关（启用锁定）切换后立即保存。 */
async function saveSecuritySwitches() {
  try {
    await settings.save();
    ElMessage.success(
      settings.vaultLockEnabled ? "已启用锁定功能" : "已关闭锁定功能（导航栏隐藏锁定按钮）",
    );
  } catch (e: unknown) {
    ElMessage.error("保存失败：" + String(e));
  }
}

// --- 数据迁移（加密导入/导出） --------------------------------------------
const exportPwd = ref("");
const exportPwdConfirm = ref("");
const exporting = ref(false);
const importPwd = ref("");
const importMode = ref<"merge" | "overwrite">("merge");
const importing = ref(false);

/** 备份文件名日期戳（YYYYMMDD）。 */
function backupDateStamp(): string {
  return new Date().toISOString().slice(0, 10).replaceAll("-", "");
}

/** 把摘要条目数格式化为展示文本。 */
function countsLines(c: BackupInfo["counts"]): string[] {
  const items: [string, number][] = [
    ["SSH/远程会话", c.sessions],
    ["会话分组", c.groups],
    ["凭据（密码/私钥）", c.credentials],
    ["数据库连接", c.dbProfiles],
    ["数据库分组", c.dbGroups],
    ["端口转发规则", c.forwardRules],
    ["桌面连接（RDP/VNC）", c.desktops],
    ["TOTP 验证码", c.totpSecrets],
    ["文件账号（S3）", c.fileAccounts],
  ];
  const lines = items
    .filter(([, n]) => n > 0)
    .map(([name, n]) => `· ${name}：${n} 条`);
  if (c.hasSettings) lines.push("· 应用设置（含 AI 密钥）");
  if (c.hasMcp) lines.push("· MCP 服务配置");
  return lines.length > 0 ? lines : ["（空备份）"];
}

/** 导出：选保存位置 → 后端聚合数据 → 加密写文件。 */
async function doExport() {
  const pwd = exportPwd.value;
  if (pwd.length < 6) {
    ElMessage.warning("备份密码至少 6 位");
    return;
  }
  if (pwd !== exportPwdConfirm.value) {
    ElMessage.warning("两次输入的密码不一致");
    return;
  }
  const picked = await save({
    title: "导出加密备份",
    defaultPath: `x-term-backup-${backupDateStamp()}.xtermbackup`,
    filters: [{ name: "X-Term 加密备份", extensions: ["xtermbackup"] }],
  });
  if (typeof picked !== "string" || !picked) return; // 用户取消

  exporting.value = true;
  try {
    const summary = await backupApi.backupExport(picked, pwd);
    ElMessage.success(`已导出：${countsLines(summary.counts).join("、")}`);
    if (summary.credentialsSkipped > 0 || summary.totpSkipped > 0) {
      ElMessage.warning(
        `保险库未解锁，${summary.credentialsSkipped} 条凭据、${summary.totpSkipped} 条 TOTP 未包含在备份中；解锁后重新导出即可完整备份`,
      );
    }
    exportPwd.value = "";
    exportPwdConfirm.value = "";
  } catch (e: any) {
    ElMessage.error("导出失败：" + (e?.message ?? String(e)));
  } finally {
    exporting.value = false;
  }
}

/** 导入：选文件 → 解密预览（校验密码）→ 确认 → 写入。 */
async function doImport() {
  const pwd = importPwd.value;
  if (!pwd) {
    ElMessage.warning("请输入备份密码");
    return;
  }
  const picked = await open({
    title: "选择备份文件导入",
    multiple: false,
    filters: [{ name: "X-Term 加密备份", extensions: ["xtermbackup"] }],
  });
  if (typeof picked !== "string" || !picked) return; // 用户取消

  // 先解密预览：校验密码 + 展示文件内容（不写入任何数据）。
  let info: BackupInfo;
  try {
    info = await backupApi.backupInspect(picked, pwd);
  } catch (e: any) {
    ElMessage.error("无法读取备份文件：" + (e?.message ?? String(e)));
    return;
  }

  const lines = [
    `文件：${picked}`,
    `创建时间：${info.createdAt ? new Date(info.createdAt).toLocaleString() : "—"}`,
    ...countsLines(info.counts),
  ];
  if (info.hasCredentials || info.hasTotp) {
    lines.push(
      vaultStore.unlocked
        ? "⚠ 包含凭据与 TOTP，将使用当前保险库重新加密后导入"
        : "⚠ 包含凭据与 TOTP，导入前需先解锁（或创建）凭据保险库",
    );
  }
  if (importMode.value === "overwrite") {
    lines.push("⚠ 覆盖模式：将清空本机现有会话/凭据等全部数据后写入，且不可撤销！");
    if (!info.hasCredentials && !info.hasTotp) {
      lines.push(
        "⚠ 该备份不含凭据与 TOTP：覆盖导入将清空本机全部凭据与 TOTP，且无法从本备份恢复！",
      );
    }
  }

  try {
    await ElMessageBox.confirm(
      h(
        "div",
        { style: "line-height:1.9" },
        lines.map((l, i) => h("div", { key: i }, l)),
      ),
      `确认${importMode.value === "overwrite" ? "覆盖导入" : "合并导入"}？`,
      {
        type: importMode.value === "overwrite" ? "warning" : "info",
        confirmButtonText: "开始导入",
        cancelButtonText: "取消",
      },
    );
  } catch {
    return; // 用户取消
  }

  // 覆盖模式 + 备份不含凭据/TOTP：本机凭据将被清空且无法从备份恢复，
  // 需要二次显式确认（对应后端 force=true 门禁）。
  let force = false;
  if (importMode.value === "overwrite" && !info.hasCredentials && !info.hasTotp) {
    try {
      await ElMessageBox.confirm(
        "该备份不含任何凭据与 TOTP（导出时保险库可能未解锁）。\n\n" +
          "覆盖导入会清空本机现有的全部凭据与 TOTP，且无法从该备份恢复。\n\n" +
          "确认仍要清空并继续导入吗？",
        "缺少凭据的备份",
        {
          type: "error",
          confirmButtonText: "确认清空并导入",
          cancelButtonText: "取消",
        },
      );
      force = true;
    } catch {
      return; // 用户取消
    }
  }

  importing.value = true;
  try {
    const summary = await backupApi.backupImport(picked, pwd, importMode.value, force);
    // 刷新本地状态：设置 + MCP 配置（会话/数据库等列表在各自页面下次进入时重新加载）。
    await settings.load();
    await mcpStore.loadAll();
    ElMessage.success(`导入完成：${countsLines(summary.counts).join("、")}`);
    importPwd.value = "";
  } catch (e: any) {
    ElMessage.error("导入失败：" + (e?.message ?? String(e)));
  } finally {
    importing.value = false;
  }
}

/** 把 SSH 白名单 textarea 文本同步到 store（去空行/去空白）。 */
async function saveWhitelist() {
  const list = whitelistText.value
    .split(/\r?\n/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  settings.sshAgent.commandWhitelist = list;
  try {
    await settings.save();
    ElMessage.success(`已保存 SSH 白名单（${list.length} 条）`);
  } catch (e: unknown) {
    ElMessage.error("保存失败：" + String(e));
  }
}

/** 恢复内置默认 SSH 白名单。 */
function resetWhitelist() {
  whitelistText.value = [
    "df", "du", "free", "uptime", "uname", "hostname", "date", "id", "who", "w", "pwd",
    "ps", "top", "htop", "ls", "cat", "head", "tail", "less", "stat", "file", "wc",
    "sort", "uniq", "find", "env", "printenv", "netstat", "ss", "ip", "ifconfig",
    "ping", "traceroute", "nslookup", "dig", "host", "grep", "egrep", "fgrep", "awk",
    "sed", "systemctl status", "systemctl list-units", "systemctl list-unit-files",
    "journalctl", "mount", "lsof", "lsblk", "fdisk -l",
    "docker ps", "docker images", "docker logs", "docker inspect", "docker stats",
  ].join("\n");
}

// --- SQL 智能体：执行模式开关 --------------------------------------------
/** SQL 开关（模式/运行模式/可视化）切换后立即保存。 */
async function saveSqlSwitches() {
  try {
    await settings.save();
  } catch (e: unknown) {
    ElMessage.error("保存失败：" + String(e));
  }
}

/** 运行模式针对各智能体的说明文案（下拉下方显示当前模式的具体行为）。 */
function runModeDesc(mode: ToolRunMode, agent: "ssh" | "sql"): string {
  const map: Record<string, Record<ToolRunMode, string>> = {
    ssh: {
      manual: "所有命令执行都需人工确认，批准后才执行。",
      auto: "所有命令自动执行（含 rm/mkfs/dd 等危险命令），完全无人值守，风险自负。",
      whitelist:
        "命令落在白名单内且非危险时免确认直接执行，其余（含危险命令）弹人工确认。",
    },
    sql: {
      manual: "所有 SQL 都需人工确认，批准后才执行。",
      auto: "所有 SQL 自动执行（含 DROP/TRUNCATE 等危险语句），完全无人值守，风险自负。",
      whitelist:
        "只读查询（SELECT/SHOW/EXPLAIN 等）免确认直接执行，写操作与危险语句弹人工确认。",
    },
  };
  return map[agent][mode];
}

/** 运行模式 / SQL 模式当前值的说明（悬浮帮助，随选择动态变化）。 */
const sshRunModeHint = computed(() => runModeDesc(settings.sshAgent.runMode, "ssh"));
const sqlRunModeHint = computed(() => runModeDesc(settings.sqlAgent.runMode, "sql"));
const sqlModeHint = computed(
  () => SQL_MODE_OPTIONS.find((o) => o.value === settings.sqlAgent.sqlMode)?.desc ?? "",
);

// --- 本地文件读写：开关 + 工作目录 ----------------------------------------
/** 文件读写开关切换后立即保存。 */
async function saveFileAccessSwitch() {
  try {
    await settings.save();
    ElMessage.success(
      settings.fileAccess.enabled ? "已启用本地文件读写" : "已关闭本地文件读写"
    );
  } catch (e: unknown) {
    ElMessage.error("保存失败：" + String(e));
  }
}

/** 为某个助手域选择工作目录（弹系统目录选择器）。 */
async function pickWorkspaceDir(domain: "ssh" | "db") {
  const picked = await open({
    title: `选择${domain === "ssh" ? "终端助手" : "数据库助手"}工作目录`,
    directory: true,
    multiple: false,
  });
  if (typeof picked !== "string" || !picked) return; // 用户取消
  try {
    await setWorkspaceDir(domain, picked);
    // 回写本地 store 并持久化（settings.json 由后端写入，这里同步前端状态）。
    settings.fileAccess.workspaceDirs = {
      ...settings.fileAccess.workspaceDirs,
      [domain]: picked,
    };
    await settings.save();
    ElMessage.success("工作目录已设置");
  } catch (e: unknown) {
    ElMessage.error("设置失败：" + String(e));
  }
}

/** 清除某个助手域的工作目录。 */
async function clearWorkspaceDir(domain: "ssh" | "db") {
  try {
    await setWorkspaceDir(domain, "");
    const dirs = { ...settings.fileAccess.workspaceDirs };
    delete dirs[domain];
    settings.fileAccess.workspaceDirs = dirs;
    await settings.save();
    ElMessage.success("已清除工作目录");
  } catch (e: unknown) {
    ElMessage.error("清除失败：" + String(e));
  }
}
</script>

<template>
  <div class="settings-view">
    <div class="header">
      <h2><el-icon><Setting /></el-icon> 设置</h2>
      <span class="header-sub">应用偏好与配置管理</span>
    </div>

    <el-tabs v-model="activeTab" ref="tabsRef" class="settings-tabs">
      <!-- ============ 终端 ============ -->
      <el-tab-pane label="终端" name="terminal">
        <div class="card-grid">
          <div class="form-card">
            <div class="card-title">外观</div>
            <el-form :model="termForm" label-width="100px" label-position="right">
              <el-form-item label="主题">
                <el-select v-model="termForm.theme" style="width: 160px">
                  <el-option label="深色 (dark)" value="dark" />
                  <el-option label="浅色 (light)" value="light" />
                </el-select>
              </el-form-item>

              <el-form-item>
                <template #label>
                  <HelpTip content="终端 ANSI 16 色配色方案，影响 ls / vim 等彩色输出。留空按明暗主题用默认（Catppuccin）">配色方案</HelpTip>
                </template>
                <el-select
                  v-model="termForm.colorScheme"
                  placeholder="跟随主题"
                  clearable
                  style="width: 220px"
                >
                  <el-option-group
                    v-for="g in [{ label: '深色', scheme: 'dark' }, { label: '浅色', scheme: 'light' }]"
                    :key="g.scheme"
                    :label="g.label"
                  >
                    <el-option
                      v-for="s in TERMINAL_COLOR_SCHEMES.filter((x) => x.scheme === g.scheme)"
                      :key="s.id"
                      :label="s.label"
                      :value="s.id"
                    >
                      <span class="scheme-name">{{ s.label }}</span>
                      <span
                        v-for="c in [s.theme.red, s.theme.green, s.theme.yellow, s.theme.blue, s.theme.magenta, s.theme.cyan]"
                        :key="c"
                        class="scheme-dot"
                        :style="{ background: c }"
                      />
                    </el-option>
                  </el-option-group>
                </el-select>
              </el-form-item>

              <el-form-item label="字体">
                <el-select v-model="termForm.fontFamily" style="width: 220px">
                  <el-option v-for="f in fontOptions" :key="f.value" :label="f.label" :value="f.value" />
                </el-select>
              </el-form-item>

              <el-form-item label="字号">
                <el-input-number v-model="termForm.fontSize" :min="10" :max="24" controls-position="right" />
              </el-form-item>

              <el-form-item label="行高">
                <el-input-number
                  v-model="termForm.lineHeight"
                  :min="1.0"
                  :max="2.0"
                  :step="0.1"
                  :precision="1"
                  controls-position="right"
                />
              </el-form-item>

              <el-form-item label="滚屏行数">
                <el-input-number v-model="termForm.scrollback" :min="100" :max="100000" :step="1000" controls-position="right" />
              </el-form-item>

              <el-form-item label="选中即复制">
                <el-switch v-model="termForm.copyOnSelect" />
              </el-form-item>

              <el-form-item label="启用WebGL">
                <el-switch v-model="termForm.enableWebgl" />
              </el-form-item>

              <el-form-item>
                <template #label>
                  <HelpTip content="把终端输出以可读文本记入 logs 目录（去除颜色码，进度条只留最终状态）。对之后新建的会话生效">
                    输出日志
                  </HelpTip>
                </template>
                <div class="output-log-row">
                  <el-switch v-model="termForm.outputLog" />
                  <el-button size="small" plain :icon="'FolderOpened'" @click="onOpenLogsDir">
                    打开日志目录
                  </el-button>
                </div>
              </el-form-item>
            </el-form>
          </div>

          <div class="form-card">
            <div class="card-title">连接</div>
            <el-form :model="termForm" label-width="100px" label-position="right">
              <el-form-item>
                <template #label>
                  <HelpTip content="0 表示永不自动断开">SSH 空闲断开</HelpTip>
                </template>
                <el-input-number
                  v-model="termForm.sshIdleTimeoutMinutes"
                  :min="0"
                  :max="1440"
                  controls-position="right"
                />
                <span class="unit-hint">分钟</span>
              </el-form-item>

              <el-form-item>
                <template #label>
                  <HelpTip content="0 表示不发送保活包（建议保持默认 30）">SSH 保活间隔</HelpTip>
                </template>
                <el-input-number
                  v-model="termForm.sshKeepaliveSecs"
                  :min="0"
                  :max="3600"
                  controls-position="right"
                />
                <span class="unit-hint">秒</span>
              </el-form-item>

              <el-form-item>
                <template #label>
                  <HelpTip content="0 表示永不超时">SSH 连接超时</HelpTip>
                </template>
                <el-input-number
                  v-model="termForm.sshConnectTimeoutSecs"
                  :min="0"
                  :max="600"
                  controls-position="right"
                />
                <span class="unit-hint">秒</span>
              </el-form-item>

              <el-form-item>
                <template #label>
                  <HelpTip content="远端服务器的字符编码（对应其 locale）。连接 GBK 等老服务器出现乱码时切换；对已打开的终端实时生效（本地终端不受影响）">终端编码</HelpTip>
                </template>
                <el-select v-model="termForm.encoding" style="width: 220px">
                  <el-option
                    v-for="e in ENCODING_OPTIONS"
                    :key="e.value"
                    :label="e.label"
                    :value="e.value"
                  />
                </el-select>
              </el-form-item>

              <el-form-item>
                <template #label>
                  <HelpTip content="「本地终端」按钮默认启动的 Shell（本机不可用的 Shell 不显示）">本地终端 Shell</HelpTip>
                </template>
                <el-select v-model="termForm.localShell" style="width: 160px">
                  <el-option
                    v-for="s in localShellOptions"
                    :key="s.id"
                    :label="s.label"
                    :value="s.id"
                  />
                </el-select>
              </el-form-item>

              <el-form-item>
                <template #label>
                  <HelpTip content="程序内嵌：终端页标签页内打开（无需安装客户端）；系统客户端：调用本机 vncviewer">VNC 客户端</HelpTip>
                </template>
                <el-select v-model="termForm.desktopClients.vnc" style="width: 160px">
                  <el-option
                    v-for="m in DESKTOP_CLIENT_MODE_OPTIONS"
                    :key="m.value"
                    :label="m.label"
                    :value="m.value"
                  />
                </el-select>
              </el-form-item>

              <el-form-item>
                <template #label>
                  <HelpTip content="程序内嵌：终端页标签页内打开（需用户名和密码）；系统客户端：调用 Windows 自带 mstsc">RDP 客户端</HelpTip>
                </template>
                <el-select v-model="termForm.desktopClients.rdp" style="width: 160px">
                  <el-option
                    v-for="m in DESKTOP_CLIENT_MODE_OPTIONS"
                    :key="m.value"
                    :label="m.label"
                    :value="m.value"
                  />
                </el-select>
              </el-form-item>

              <el-form-item>
                <template #label>
                  <HelpTip content="程序内嵌连接时是否校验服务器证书链（严格模式用系统信任根验证）。默认关闭，与官方 mstsc 直连行为一致">RDP 证书校验</HelpTip>
                </template>
                <el-switch v-model="termForm.rdpVerifyCert" />
              </el-form-item>

              <el-form-item>
                <template #label>
                  <HelpTip content="下载的默认保存目录（终端 sz、SFTP、对象存储下载共用）。设置后下载不再弹保存对话框，直接落盘到该目录（同名文件自动加 (n) 后缀）；留空则每次下载时选择路径。下载记录可在标题栏的「下载」列表中查看">默认下载目录</HelpTip>
                </template>
                <div class="zmodem-dir-row">
                  <el-input
                    v-model="termForm.downloadDir"
                    placeholder="留空 = 每次下载时选择路径"
                    readonly
                    clearable
                  >
                    <template #prefix>
                      <el-icon><FolderOpened /></el-icon>
                    </template>
                  </el-input>
                  <el-button :icon="FolderOpened" @click="pickZmodemDownloadDir">浏览</el-button>
                </div>
              </el-form-item>
            </el-form>
          </div>
        </div>
        <div class="card-actions">
          <el-button type="primary" @click="applyTerminal">应用</el-button>
          <el-button @click="resetTerminal">重置</el-button>
        </div>
      </el-tab-pane>

      <!-- ============ 快捷命令 / 快捷键 ============ -->
      <el-tab-pane name="shortcuts">
        <template #label>
          <HelpTip>
            <span>快捷命令</span>
            <template #content>
              配置终端底部快捷命令栏与全局快捷键。点击按钮或按下快捷键即向当前活动终端发送命令。
              <br />支持占位符：<code>{"{host}"}</code>、<code>{"{user}"}</code>、<code>{"{port}"}</code>（按当前会话替换）。
            </template>
          </HelpTip>
        </template>
          <!-- 快捷命令表：名称 / 命令 / 快捷键 / 分组 / 操作 -->
          <div class="sc-cmd-list">
            <div class="sc-cmd-row sc-cmd-head">
              <div>名称</div>
              <div>命令</div>
              <div>快捷键</div>
              <div>分组</div>
              <div class="sc-op-col">操作</div>
            </div>
            <template v-for="gv in shortcutGroupsView" :key="gv.group || '__default__'">
              <div v-if="gv.group" class="sc-cmd-group">{{ gv.group }}</div>
              <div v-for="sc in gv.items" :key="sc.id" class="sc-cmd-row">
                <el-input v-model="sc.label" placeholder="显示名称" class="sc-name" />
                <el-input
                  v-model="sc.command"
                  placeholder="命令（不含换行）"
                  class="sc-command mono"
                />
                <el-input
                  v-model="sc.shortcut"
                  placeholder="如 Ctrl+1（可选）"
                  class="sc-key"
                  @keydown="onShortcutKeyCapture(sc, $event)"
                />
                <el-select
                  v-model="sc.group"
                  placeholder="分组"
                  class="sc-group"
                  clearable
                  filterable
                  allow-create
                  default-first-option
                  @change="(val: string) => onGroupChange(sc, val)"
                >
                  <el-option
                    v-for="g in settings.shortcutGroups"
                    :key="g"
                    :label="g"
                    :value="g"
                  />
                </el-select>
                <el-button
                  class="sc-op-col"
                  type="danger"
                  link
                  title="删除"
                  @click="removeShortcut(sc.id)"
                >
                  <el-icon><Delete /></el-icon>
                </el-button>
              </div>
            </template>
            <div v-if="settings.shortcuts.length === 0" class="empty-tip">
              暂无快捷命令，点击右下角「新增快捷命令」添加
            </div>
          </div>

          <div class="table-actions">
            <el-button :icon="Plus" @click="addShortcut">新增快捷命令</el-button>
            <el-button type="primary" @click="saveShortcuts">
              保存快捷命令
            </el-button>
          </div>
      </el-tab-pane>

      <!-- ============ 快捷命令 / 快捷键 ============ -->
      <el-tab-pane name="appShortcuts">
        <template #label>
          <HelpTip>
            <span>快捷键</span>
            <template #content>
              自定义应用级快捷键。点击右侧输入框开始录制，按下组合键即可绑定；
              <b>Esc</b> 取消录制，<b>Backspace</b> 清除当前绑定。冲突会高亮提示。
            </template>
          </HelpTip>
        </template>
          <!-- 快捷键表：三列（标题 / 描述 / 快捷键），卡片化表格 -->
          <div class="app-shortcut-list">
            <div class="app-shortcut-row app-shortcut-head">
              <div class="sc-col-label">标题</div>
              <div class="sc-col-desc">描述</div>
              <div class="sc-col-key">快捷键</div>
            </div>
            <div
              v-for="row in appRows"
              :key="row.action"
              class="app-shortcut-row"
              :class="{ conflict: row.conflict }"
            >
              <div class="sc-col-label app-shortcut-label">{{ row.label }}</div>
              <div class="sc-col-desc app-shortcut-desc">{{ row.description }}</div>
              <div class="sc-col-key">
                <div
                  class="key-input"
                  :class="{ recording: recordingAction === row.action }"
                  tabindex="0"
                  @keydown="recordingAction === row.action ? onRecordKeydown($event) : undefined"
                  @click="startRecord(row.action)"
                >
                  <template v-if="recordingAction === row.action">
                    {{ recordingText || "按下组合键…" }}
                  </template>
                  <template v-else-if="row.key">
                    <kbd>{{ row.key }}</kbd>
                  </template>
                  <template v-else>
                    <span class="unbound">未绑定（点击录制）</span>
                  </template>
                </div>
                <el-button
                  v-if="row.key && recordingAction !== row.action"
                  size="small"
                  link
                  @click="appForm[row.action] = ''"
                >
                  清除
                </el-button>
                <div v-if="row.conflict" class="conflict-tip">冲突</div>
              </div>
            </div>
          </div>

          <div class="table-actions">
            <el-button :icon="Refresh" @click="resetAppShortcuts">
              恢复默认
            </el-button>
            <el-button type="primary" @click="saveAppShortcuts">
              保存快捷键
            </el-button>
          </div>

          <!-- 终端操作行为（鼠标类"快捷操作"，与键位绑定同区管理）：
               即时生效并保存，无需点上面的"保存快捷键"。 -->
          <div class="form-card term-op-card">
            <div class="card-title">终端操作</div>
            <div class="term-op-row">
              <div class="app-shortcut-info">
                <div class="app-shortcut-label">左键选择即复制</div>
                <div class="app-shortcut-desc">在终端中选中文字后立即复制到剪贴板，无需再按复制键</div>
              </div>
              <el-switch
                :model-value="settings.terminal.copyOnSelect"
                @update:model-value="setTermOp('copyOnSelect', $event as boolean)"
              />
            </div>
            <div class="term-op-row">
              <div class="app-shortcut-info">
                <div class="app-shortcut-label">右键粘贴</div>
                <div class="app-shortcut-desc">右键单击直接粘贴剪贴板内容（PuTTY 风格）；关闭时右键弹出上下文菜单</div>
              </div>
              <el-switch
                :model-value="settings.terminal.rightClickPaste"
                @update:model-value="setTermOp('rightClickPaste', $event as boolean)"
              />
            </div>
            <div class="term-op-row">
              <div class="app-shortcut-info">
                <div class="app-shortcut-label">粘贴保护</div>
                <div class="app-shortcut-desc">
                  粘贴多行内容或含高危命令（rm -rf、dd of=、mkfs 等）的文本时先弹窗确认，
                  避免剪贴板里整段脚本被一次回车全部执行
                </div>
              </div>
              <el-switch
                :model-value="settings.terminal.pasteConfirm"
                @update:model-value="setTermOp('pasteConfirm', $event as boolean)"
              />
            </div>
            <div class="term-op-row">
              <div class="app-shortcut-info">
                <div class="app-shortcut-label">智能补全</div>
                <div class="app-shortcut-desc">终端输入时按历史命令与快捷命令弹出建议；↑↓ 选择、Tab 采纳、Esc 关闭</div>
              </div>
              <el-switch
                :model-value="settings.terminal.suggestHistory"
                @update:model-value="setTermOp('suggestHistory', $event as boolean)"
              />
            </div>
            <div class="term-op-row">
              <div class="app-shortcut-info">
                <div class="app-shortcut-label">补全附带 AI 建议</div>
                <div class="app-shortcut-desc">建议列表底部追加 AI 补全项，方向键选中后才调用模型（需已配置 AI 模型）</div>
              </div>
              <el-switch
                :model-value="settings.terminal.suggestAi"
                @update:model-value="setTermOp('suggestAi', $event as boolean)"
              />
            </div>
          </div>
      </el-tab-pane>

      <!-- ============ AI 助手 ============ -->
      <el-tab-pane label="AI 助手" name="ai" class="ai-pane">
        <div class="form-card no-pad span-full">
          <div class="table-head">
            <div class="table-head-left">
              <span class="table-title">模型列表</span>
              <span class="table-warn-tip">
                <el-icon><WarningFilled /></el-icon>
                AI 数据会上传到所选模型服务，请勿输入敏感信息
              </span>
            </div>
            <el-button size="small" type="primary" plain :icon="Plus" @click="openProviderDialog">
              添加 Provider
            </el-button>
          </div>
          <el-table
            :data="settings.aiProviders"
            empty-text="尚未添加任何 provider"
            max-height="460"
          >
            <el-table-column label="模型" min-width="200">
              <template #default="{ row }">
                <span class="mono">{{ row.model }}</span>
                <el-tag v-if="row.multimodal" type="warning" size="small" effect="plain" class="mm-tag">
                  多模态
                </el-tag>
              </template>
            </el-table-column>

            <el-table-column label="状态" width="90" align="center">
              <template #default="{ row }">
                <el-tag
                  v-if="settings.aiActive === `${row.kind}:${row.model}`"
                  type="success"
                  size="small"
                  effect="dark"
                >
                  激活
                </el-tag>
                <span v-else class="muted">-</span>
              </template>
            </el-table-column>

            <el-table-column label="操作" width="240" align="center" fixed="right">
              <template #default="{ row, $index }">
                <el-button
                  size="small"
                  :type="settings.aiActive === `${row.kind}:${row.model}` ? 'success' : 'primary'"
                  :disabled="settings.aiActive === `${row.kind}:${row.model}`"
                  @click="setActive(row)"
                >
                  {{ settings.aiActive === `${row.kind}:${row.model}` ? "已激活" : "设为激活" }}
                </el-button>
                <el-button size="small" @click="openEditProvider(row, $index)">
                  编辑
                </el-button>
                <el-button size="small" type="danger" @click="removeProvider(row)">
                  删除
                </el-button>
              </template>
            </el-table-column>
          </el-table>
        </div>

        <!-- SSH 智能体配置 -->
        <div class="form-card">
          <div class="card-title">
            <HelpTip content="控制 AI 在 SSH 终端上执行命令的行为：运行模式、白名单、可视化。">SSH 智能体（终端命令执行）</HelpTip>
          </div>
          <div class="switch-row">
            <div class="switch-label">
              <div>
                <HelpTip :content="sshRunModeHint">运行模式</HelpTip>
              </div>
            </div>
            <el-select
              v-model="settings.sshAgent.runMode"
              style="width: 140px"
              @change="saveSshSwitches"
            >
              <el-option
                v-for="opt in RUN_MODE_OPTIONS"
                :key="opt.value"
                :label="opt.label"
                :value="opt.value"
              />
            </el-select>
          </div>
          <div class="switch-row">
            <div class="switch-label">
              <div>
                <HelpTip content="开启后，AI 执行的命令会写入活动终端，命令和输出实时显示在终端窗口（像 AI 在终端里敲命令）。关闭则走后台独立执行，输出只在 AI 面板。">终端可视化执行</HelpTip>
              </div>
            </div>
            <el-switch v-model="settings.sshAgent.terminalVisualization" @change="saveSshSwitches" />
          </div>
          <div class="card-title sub">
            <HelpTip content="允许 AI 在服务器上免确认执行的命令前缀（每行一条）。">命令白名单</HelpTip>
          </div>
          <!-- 安全规则：保持直接展示，不收进 tooltip -->
          <div class="card-desc warn">
            含元字符（<code>; &amp; | &gt; &lt; ` $()</code>）的命令 永不算白名单内——防止 <code>ls; rm -rf /</code> 绕过
          </div>
          <el-input
            v-model="whitelistText"
            type="textarea"
            :autosize="{ minRows: 6, maxRows: 16 }"
            placeholder="每行一个命令前缀，例如：&#10;df&#10;free&#10;ps&#10;systemctl status"
            style="margin-top: 8px"
          />
          <div class="table-actions">
            <el-button type="primary" @click="saveWhitelist">保存白名单</el-button>
            <el-button @click="resetWhitelist">恢复默认</el-button>
          </div>
        </div>

        <!-- SQL 智能体配置 -->
        <div class="form-card">
          <div class="card-title">
            <HelpTip content="控制 AI 在数据库（MySQL / PostgreSQL）上执行 SQL 的行为：执行模式与运行模式。">SQL 智能体（数据库语句执行）</HelpTip>
          </div>
            <div class="switch-row">
              <div class="switch-label">
                <div>
                  <HelpTip :content="sqlRunModeHint">运行模式</HelpTip>
                </div>
              </div>
              <el-select
                v-model="settings.sqlAgent.runMode"
                style="width: 140px"
                @change="saveSqlSwitches"
              >
                <el-option
                  v-for="opt in RUN_MODE_OPTIONS"
                  :key="opt.value"
                  :label="opt.label"
                  :value="opt.value"
                />
              </el-select>
            </div>
            <div class="switch-row">
              <div class="switch-label">
                <div>
                  <HelpTip content="开启后，AI 执行的 SQL 及其结果回显到 SQL 控制台（命令行模式），就像你手动执行一样。关闭则结果只在 AI 面板。与终端助手的可视化独立设置。">终端可视化执行</HelpTip>
                </div>
              </div>
              <el-switch v-model="settings.sqlAgent.terminalVisualization" @change="saveSqlSwitches" />
            </div>
            <div class="switch-row">
              <div class="switch-label">
                <div>
                  <HelpTip :content="sqlModeHint">SQL 执行模式</HelpTip>
                </div>
              </div>
              <el-select
                v-model="settings.sqlAgent.sqlMode"
                style="width: 140px"
                @change="saveSqlSwitches"
              >
                <el-option
                  v-for="opt in SQL_MODE_OPTIONS"
                  :key="opt.value"
                  :label="opt.label"
                  :value="opt.value"
                />
              </el-select>
            </div>
        </div>

        <!-- 本地文件读写 -->
        <div class="form-card span-full">
          <div class="card-title">
            <HelpTip content="开启后，AI 可在各助手的工作目录内自动读写文件（读取数据文件、导出查询结果为 CSV/SQL 等）。AI 只能访问工作目录及子目录（沙箱），写文件覆盖已有文件仍需人工确认。关闭时 AI 无法访问本地文件。">本地文件读写</HelpTip>
          </div>
          <div class="switch-row">
            <div class="switch-label">
              <div>
                <HelpTip content="关闭时 AI 行为与之前完全一致。">启用本地文件读写</HelpTip>
              </div>
            </div>
            <el-switch v-model="settings.fileAccess.enabled" @change="saveFileAccessSwitch" />
          </div>
          <template v-if="settings.fileAccess.enabled">
            <div
              v-for="d in (['ssh', 'db'] as const)"
              :key="d"
              class="workspace-row"
            >
              <div class="switch-label">
                <div>
                  <HelpTip content="AI 读写文件的根目录（可读写其子目录）。未设置时 AI 无法使用文件工具。">{{ d === "ssh" ? "终端助手" : "数据库助手" }}工作目录</HelpTip>
                </div>
              </div>
              <div class="workspace-picker">
                <el-tag
                  class="workspace-path"
                  :type="settings.fileAccess.workspaceDirs[d] ? 'info' : 'warning'"
                  effect="plain"
                  :closable="!!settings.fileAccess.workspaceDirs[d]"
                  :close-icon="Delete"
                  @close="clearWorkspaceDir(d)"
                >
                  <span class="path-text">{{ settings.fileAccess.workspaceDirs[d] ?? "未设置" }}</span>
                </el-tag>
                <el-button size="small" :icon="Folder" @click="pickWorkspaceDir(d)">
                  {{ settings.fileAccess.workspaceDirs[d] ? "更改" : "选择目录" }}
                </el-button>
              </div>
            </div>
          </template>
        </div>

        <!-- 添加/编辑 Provider 弹窗 -->
        <el-dialog
          v-model="providerDialogVisible"
          :title="editingIndex !== null ? '编辑 Provider' : '添加 Provider'"
          width="620px"
          :close-on-click-modal="false"
        >
          <el-form
            ref="providerFormRef"
            :model="providerForm"
            :rules="providerRules"
            label-width="120px"
            label-position="right"
          >
            <el-form-item label="类型" prop="kind">
              <el-select
                v-model="providerForm.kind"
                style="width: 100%"
                @change="onProviderKindChange"
              >
                <el-option
                  v-for="o in providerKindOptions"
                  :key="o.value"
                  :label="o.label"
                  :value="o.value"
                />
              </el-select>
            </el-form-item>

            <el-form-item label="Base URL" prop="baseUrl">
              <el-input v-model="providerForm.baseUrl" placeholder="自动填充，可修改" />
            </el-form-item>

            <el-form-item label="API Key" prop="apiKey">
              <el-input v-model="providerForm.apiKey" type="password" show-password placeholder="sk-..." />
            </el-form-item>

            <el-form-item label="Model" prop="model">
              <el-input v-model="providerForm.model" placeholder="例如 gpt-4o-mini" />
            </el-form-item>

            <el-form-item label="多模态">
              <el-switch v-model="providerForm.multimodal" />
              <span class="muted form-hint">开启后终端助手 / 数据库助手可附带图片提问</span>
            </el-form-item>

            <el-divider content-position="left">模型参数</el-divider>

            <div class="provider-params-grid">
              <el-form-item prop="contextWindow">
                <template #label>
                  <HelpTip content="tokens，超出部分的历史消息会被裁剪">上下文大小</HelpTip>
                </template>
                <el-input-number
                  v-model="providerForm.contextWindow"
                  :min="1"
                  :max="10000000"
                  :step="4096"
                  :controls="false"
                  style="width: 100%"
                />
              </el-form-item>

              <el-form-item prop="maxOutput">
                <template #label>
                  <HelpTip content="tokens（请求体 max_tokens）">最大输出</HelpTip>
                </template>
                <el-input-number
                  v-model="providerForm.maxOutput"
                  :min="1"
                  :max="1000000"
                  :step="1024"
                  :controls="false"
                  style="width: 100%"
                />
              </el-form-item>

              <el-form-item prop="maxToolCalls">
                <template #label>
                  <HelpTip content="智能体模式单次对话的最大工具调用数">工具调用数</HelpTip>
                </template>
                <el-input-number
                  v-model="providerForm.maxToolCalls"
                  :min="1"
                  :max="1000"
                  :controls="false"
                  style="width: 100%"
                />
              </el-form-item>

              <el-form-item prop="temperature">
                <template #label>
                  <HelpTip content="采样温度，留空表示不发送">温度</HelpTip>
                </template>
                <el-input-number
                  v-model="providerForm.temperature"
                  :min="0"
                  :max="2"
                  :step="0.1"
                  :precision="1"
                  :controls="false"
                  placeholder="留空由服务端默认"
                  style="width: 100%"
                />
              </el-form-item>

              <el-form-item prop="connectTimeoutSecs">
                <template #label>
                  <HelpTip content="秒，DNS 解析/建连最长等待">建连超时</HelpTip>
                </template>
                <el-input-number
                  v-model="providerForm.connectTimeoutSecs"
                  :min="1"
                  :max="3600"
                  :controls="false"
                  style="width: 100%"
                />
              </el-form-item>

              <el-form-item prop="readTimeoutSecs">
                <template #label>
                  <HelpTip content="秒，流式响应间隔最长等待；长思考模型需调大">读取超时</HelpTip>
                </template>
                <el-input-number
                  v-model="providerForm.readTimeoutSecs"
                  :min="1"
                  :max="3600"
                  :controls="false"
                  style="width: 100%"
                />
              </el-form-item>
            </div>
          </el-form>

          <template #footer>
            <el-button @click="providerDialogVisible = false">取消</el-button>
            <el-button type="primary" @click="submitProvider">
              {{ editingIndex !== null ? "保存修改" : "添加" }}
            </el-button>
          </template>
        </el-dialog>
      </el-tab-pane>

      <!-- ============ 安全 ============ -->
      <el-tab-pane label="安全" name="security">
        <div class="form-card">
          <div class="card-title">
            <HelpTip content="凭据保险库用主密码保护所有保存的密码与私钥，解锁状态保存在应用进程内。">保险库与锁定</HelpTip>
          </div>
          <div class="switch-row">
            <div class="switch-label">
              <div>
                <HelpTip content="开启后，左侧导航栏底部显示「锁定」按钮。点击锁定即清除内存中的主密钥，需重新输入主密码才能解锁。已建立的连接（终端 / SFTP / 隧道 / 数据库）不受影响，可继续使用。">启用锁定功能</HelpTip>
              </div>
            </div>
            <el-switch v-model="settings.vaultLockEnabled" @change="saveSecuritySwitches" />
          </div>
          <div class="mode-desc">
            说明：解锁状态只保存在应用进程内存中——刷新 / 重开窗口<strong>不会</strong>
            要求重新登录；<strong>重启应用</strong>或点击「锁定」后才需重新输入主密码。
            关闭本开关仅隐藏锁定按钮，重启后仍需解锁（主密钥从不落盘）。
          </div>
        </div>
      </el-tab-pane>

      <!-- ============ 数据迁移 ============ -->
      <el-tab-pane label="数据迁移" name="dataMigration">
        <div class="card-grid">
          <!-- 导出 -->
          <div class="form-card">
            <div class="card-title">
              <HelpTip content="将会话、数据库连接、凭据（密码/私钥）、TOTP、转发规则、桌面连接、文件账号、应用设置与 MCP 配置导出为加密备份文件（.xtermbackup）。文件使用独立的备份密码（Argon2id + AES-256-GCM）加密，可安全转移到其他机器。">导出加密备份</HelpTip>
            </div>
            <div class="backup-pwd-row">
              <el-input
                v-model="exportPwd"
                type="password"
                show-password
                placeholder="备份密码（至少 6 位）"
                @keyup.enter="doExport"
              />
              <el-input
                v-model="exportPwdConfirm"
                type="password"
                show-password
                placeholder="确认备份密码"
                @keyup.enter="doExport"
              />
            </div>
            <div v-if="!vaultStore.unlocked" class="mode-desc">
              提示：当前<strong>未解锁保险库</strong>，凭据（密码/私钥）与 TOTP 将
              <strong>不包含</strong>在备份中；如需完整备份，请先在「安全」页解锁保险库。
            </div>
            <div class="backup-actions">
              <el-button type="primary" :loading="exporting" :icon="Download" @click="doExport">
                导出到文件…
              </el-button>
            </div>
          </div>

          <!-- 导入 -->
          <div class="form-card">
            <div class="card-title">
              <HelpTip content="选择之前导出的加密备份文件并输入对应的备份密码。合并导入按 id 覆盖同名条目、保留本机其他数据；覆盖导入会先清空本机相关数据再完整恢复（同一事务，失败自动回滚）。">从备份导入</HelpTip>
            </div>
            <div class="backup-pwd-row">
              <el-input
                v-model="importPwd"
                type="password"
                show-password
                placeholder="备份密码"
                @keyup.enter="doImport"
              />
              <el-radio-group v-model="importMode" class="backup-mode">
                <el-radio value="merge">合并导入</el-radio>
                <el-radio value="overwrite">覆盖导入</el-radio>
              </el-radio-group>
            </div>
            <div class="backup-actions">
              <!-- 覆盖导入是危险操作，按钮随模式切换为红色提示 -->
              <el-button
                :type="importMode === 'overwrite' ? 'danger' : 'primary'"
                :loading="importing"
                :icon="Upload"
                @click="doImport"
              >
                选择文件导入…
              </el-button>
            </div>
          </div>
        </div>
      </el-tab-pane>

      <!-- ============ 关于 ============ -->
      <el-tab-pane label="关于" name="about">
        <div class="about-page">
          <!-- 品牌区 -->
          <div class="about-brand">
            <div class="about-logo">X</div>
            <div class="about-name">X-Term</div>
            <div class="about-slogan">一站式运维工作站</div>
            <div class="about-version">当前版本 v{{ updater.info?.currentVersion ?? "—" }}</div>
          </div>

          <!-- 检查更新卡片 -->
          <div class="form-card about-update">
            <div class="about-card-title">检查更新</div>

            <!-- 空闲 / 错误后重试 -->
            <div v-if="updater.status === 'idle'" class="about-update-body">
              <el-button type="primary" :icon="Refresh" @click="updater.check()">检查更新</el-button>
              <span v-if="updater.skippedVersion" class="about-hint">
                已跳过 v{{ updater.skippedVersion }}，点击检查将重新提示
              </span>
            </div>

            <!-- 检查中 -->
            <div v-else-if="updater.status === 'checking'" class="about-update-body">
              <el-icon class="is-loading"><Refresh /></el-icon>
              <span>正在检查更新…</span>
            </div>

            <!-- 已是最新 -->
            <div v-else-if="updater.status === 'up-to-date'" class="about-update-body">
              <el-tag type="success" effect="light">✓ 当前已是最新版本</el-tag>
              <el-button link @click="updater.reset()">返回</el-button>
            </div>

            <!-- 发现新版本 -->
            <div v-else-if="updater.status === 'update-available'" class="about-update-body column">
              <div class="about-new-ver">
                <el-tag type="warning" effect="dark">发现新版本 v{{ updater.manifest?.version }}</el-tag>
              </div>
              <pre v-if="updater.manifest?.notes" class="about-notes">{{ updater.manifest.notes }}</pre>
              <div class="about-actions">
                <el-button type="primary" @click="updater.download()">立即更新</el-button>
                <el-button @click="updater.skip()">跳过此版本</el-button>
              </div>
            </div>

            <!-- 下载中 -->
            <div v-else-if="updater.status === 'downloading'" class="about-update-body column">
              <el-progress
                :percentage="updater.progress.percent"
                :stroke-width="14"
                :format="(p: number) => `${p}%`"
              />
              <div class="about-dl-meta">
                {{ formatBytes(updater.progress.received) }}
                <template v-if="updater.progress.total"> / {{ formatBytes(updater.progress.total) }}</template>
              </div>
            </div>

            <!-- 下载完成 -->
            <div v-else-if="updater.status === 'downloaded'" class="about-update-body column">
              <el-tag type="success" effect="light">✓ 下载完成</el-tag>
              <div class="about-actions">
                <el-button type="primary" @click="confirmInstall">立即安装并重启</el-button>
              </div>
            </div>

            <!-- 出错 -->
            <div v-else-if="updater.status === 'error'" class="about-update-body column">
              <el-alert :title="updater.error ?? '更新失败'" type="error" :closable="false" show-icon />
              <div class="about-actions">
                <el-button @click="updater.reset()">返回</el-button>
                <el-button type="primary" @click="updater.check()">重试</el-button>
              </div>
            </div>
          </div>

          <!-- 技术信息 -->
          <div class="form-card about-meta">
            <div class="about-card-title">技术信息</div>
            <div class="about-meta-grid">
              <span class="k">平台</span><span class="v">Windows x64</span>
              <span class="k">数据目录</span><span class="v">{{ updater.info?.dataDir ?? "—" }}</span>
              <span class="k">项目地址</span>
              <span class="v">
                <a
                  class="about-link"
                  href="https://github.com/qingfengCode/x-term"
                  target="_blank"
                  rel="noopener"
                >
                  https://github.com/qingfengCode/x-term
                </a>
              </span>
            </div>
          </div>
        </div>
      </el-tab-pane>
    </el-tabs>
  </div>
</template>

<style scoped>
/* ============ 页面骨架：浅底衬托卡片，头部 + 顶部 Tab + 滚动内容 ============ */
.settings-view {
  padding: 18px 28px 0;
  display: flex;
  flex-direction: column;
  height: 100%;
  box-sizing: border-box;
  overflow: hidden;
  background: var(--el-bg-color-page);
}

.header {
  display: flex;
  align-items: center;
  padding-bottom: 14px;
  margin-bottom: 2px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
}

.header h2 {
  margin: 0;
  font-size: 19px;
  font-weight: 600;
  letter-spacing: 0.3px;
  color: var(--el-text-color-primary);
  display: flex;
  align-items: center;
  gap: 10px;
}

.header h2 .el-icon {
  font-size: 21px;
  color: var(--el-color-primary);
}

.header-sub {
  margin-left: 14px;
  padding-left: 14px;
  border-left: 1px solid var(--el-border-color-lighter);
  font-size: 12px;
  color: var(--el-text-color-placeholder);
}

/* 顶部 Tab 导航 + 下方内容区滚动 */
.settings-tabs {
  flex: 1;
  min-height: 0;
  display: flex;
}
.settings-tabs :deep(.el-tabs__header) {
  margin: 0 0 14px;
  flex-shrink: 0;
}
.settings-tabs :deep(.el-tabs__nav-wrap::after) {
  height: 1px;
  background-color: var(--el-border-color-lighter);
}
.settings-tabs :deep(.el-tabs__item) {
  height: 38px;
  line-height: 38px;
  padding: 0 20px;
  font-size: 13.5px;
  color: var(--el-text-color-secondary);
  transition: color 0.15s ease;
}
.settings-tabs :deep(.el-tabs__item:hover) {
  color: var(--el-text-color-primary);
}
.settings-tabs :deep(.el-tabs__item.is-active) {
  color: var(--el-color-primary);
  font-weight: 600;
}
.settings-tabs :deep(.el-tabs__active-bar) {
  height: 3px;
  border-radius: 3px 3px 0 0;
}
.settings-tabs :deep(.el-tabs__content) {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 0 6px 36px 0;
}
/* 内容限宽：宽屏下表单不至于被拉得稀疏 */
.settings-tabs :deep(.el-tab-pane) {
  max-width: 1080px;
}
.settings-tabs :deep(.el-tabs__content)::-webkit-scrollbar {
  width: 8px;
}
.settings-tabs :deep(.el-tabs__content)::-webkit-scrollbar-thumb {
  background: var(--el-border-color-light);
  border-radius: 4px;
}
.settings-tabs :deep(.el-tabs__content)::-webkit-scrollbar-track {
  background: transparent;
}

/* 卡片：浅底页面上的浮层卡片，轻投影 + 圆角 */
.form-card {
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 10px;
  padding: 18px 22px 20px;
  margin-bottom: 16px;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.05);
}

/* 输出日志开关 + 打开目录按钮同行 */
.output-log-row {
  display: flex;
  align-items: center;
  gap: 12px;
}

/* 多卡片网格布局（两列并排，窄屏回退单列）。AI 助手 Tab 复用同一网格。 */
.card-grid,
.ai-pane {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(400px, 1fr));
  gap: 16px;
}
.card-grid {
  margin-bottom: 16px;
}
.card-grid .form-card,
.ai-pane .form-card {
  margin-bottom: 0;
}
/* 操作按钮行：右对齐（应用/重置类动作惯例） */
.card-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding-top: 4px;
}

/* 卡片标题：独立分区头（标题 + 细分隔线） */
.card-title {
  font-size: 13px;
  font-weight: 600;
  letter-spacing: 0.5px;
  color: var(--el-text-color-primary);
  padding-bottom: 10px;
  margin-bottom: 14px;
  border-bottom: 1px solid var(--el-border-color-extra-light);
}
/* 卡片内的二级标题（如命令白名单） */
.card-title.sub {
  margin-top: 18px;
}

/* 表单：统一行距与标签字色，控件宽度视觉对齐 */
.form-card :deep(.el-form-item) {
  margin-bottom: 16px;
}
.form-card :deep(.el-form-item:last-child) {
  margin-bottom: 0;
}
.form-card :deep(.el-form-item__label) {
  font-size: 13px;
  color: var(--el-text-color-regular);
}

/* AI 助手 Tab：SSH / SQL 配置卡片两列并排，其余横跨整行 */
.ai-pane .span-full {
  grid-column: 1 / -1;
}

.form-card.no-pad {
  padding: 0;
  overflow: hidden;
}

/* 表格卡片头部（标题 + 操作按钮） */
.table-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 14px 16px;
  border-bottom: 1px solid var(--el-border-color-lighter);
}
.table-title {
  font-size: 13px;
  font-weight: 600;
  letter-spacing: 0.5px;
  color: var(--el-text-color-primary);
}
/* 表头左侧：标题 + 内联提示 */
.table-head-left {
  display: flex;
  align-items: center;
  gap: 14px;
  min-width: 0;
}
/* 标题右侧的警示提示（替代原独立 alert 条） */
.table-warn-tip {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 12px;
  font-weight: 400;
  letter-spacing: normal;
  color: var(--el-color-warning);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.table-warn-tip .el-icon {
  font-size: 14px;
  flex-shrink: 0;
}

/* 模型列表表格：表头浅底、行悬停反馈 */
.form-card :deep(.el-table) {
  --el-table-header-bg-color: var(--el-fill-color-lighter);
  --el-table-row-hover-bg-color: var(--el-fill-color-light);
}
.form-card :deep(.el-table th.el-table__cell) {
  font-weight: 600;
  color: var(--el-text-color-regular);
}
.form-card :deep(.el-table .el-table__cell) {
  padding: 10px 0;
}

.mono {
  font-family: "Consolas", "Cascadia Code", monospace;
  font-size: 13px;
  color: var(--el-text-color-regular);
}

.secret {
  color: var(--el-text-color-secondary);
  letter-spacing: 1px;
}

.muted {
  color: var(--el-text-color-placeholder);
}

/* 表单行内辅助说明（如多模态开关旁的解释文字）。 */
.form-hint {
  margin-left: 10px;
  font-size: 12px;
}

/* Provider 弹窗：模型参数两列并排（上下文/最大输出、工具调用数/温度、建连超时/读取超时）。 */
.provider-params-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  column-gap: 16px;
}
.provider-params-grid :deep(.el-form-item) {
  margin-bottom: 14px;
}
.provider-params-grid :deep(.el-form-item__label) {
  white-space: nowrap;
  /* el-form-item__label 默认 align-items: flex-start，问号图标会被顶到顶部；
     改为 center 让图标与文字垂直居中对齐 */
  align-items: center;
}
/* el-tooltip 触发器 span 默认行高 32px 且图标基线对齐，会产生偏移；
   改成 inline-flex 后图标以自身高度居中 */
.provider-params-grid :deep(.el-tooltip__trigger) {
  display: inline-flex;
  align-items: center;
}

/* 模型列表中的「多模态」标签。 */
.mm-tag {
  margin-left: 6px;
}

/* 终端表单：输入框后的单位文字。 */
.unit-hint {
  margin-left: 8px;
  color: var(--el-text-color-secondary);
  font-size: 13px;
}

/* ZMODEM 默认下载目录：路径输入 + 浏览按钮同行。 */
.zmodem-dir-row {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
}
.zmodem-dir-row .el-input {
  flex: 1;
}

/* 配色方案下拉项：名称 + 6 色预览点 */
.scheme-name {
  margin-right: 10px;
}
.scheme-dot {
  display: inline-block;
  width: 10px;
  height: 10px;
  border-radius: 50%;
  margin-right: 3px;
  vertical-align: -1px;
  border: 1px solid rgba(0, 0, 0, 0.15);
}

.card-desc {
  font-size: 12px;
  line-height: 1.6;
  color: var(--el-text-color-secondary);
}
/* SQL 模式说明 / 安全提示等段落说明。 */
.mode-desc {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  margin: 8px 0 0;
  line-height: 1.6;
}
.card-desc code {
  background: var(--el-fill-color-light);
  padding: 1px 4px;
  border-radius: 3px;
  font-size: 11px;
}
/* 安全规则类说明：保持直接展示。 */
.card-desc.warn {
  color: var(--el-color-warning);
}
/* 数据迁移：密码输入行与操作按钮行。 */
.backup-pwd-row {
  display: flex;
  align-items: center;
  gap: 12px;
  margin-top: 14px;
}
/* 密码框弹性均分宽度，卡片变窄时自动收缩 */
.backup-pwd-row :deep(.el-input) {
  min-width: 0;
}
.backup-mode {
  flex-shrink: 0;
}
.backup-actions {
  margin-top: 14px;
}
.switch-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  padding: 13px 2px;
}
/* 分隔线只出现在相邻开关行之间（卡片标题后的首行无线） */
.switch-row + .switch-row {
  border-top: 1px solid var(--el-border-color-lighter);
}
.switch-label > div:first-child {
  font-size: 13px;
  font-weight: 500;
  color: var(--el-text-color-primary);
}

/* 本地文件读写：工作目录选择行 */
.workspace-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  padding: 10px 0;
  border-top: 1px solid var(--el-border-color-lighter);
}
.workspace-picker {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
  max-width: 55%;
}
.workspace-path {
  max-width: 320px;
  font-family: "Consolas", "Cascadia Code", monospace;
  font-size: 12px;
}
/* el-tag 是 inline-flex，text-overflow 在它身上不生效；截断放到内部 span。
   限宽 300px 为关闭按钮留空间。 */
.workspace-path .path-text {
  display: block;
  max-width: 300px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* --- 表格类列表（快捷命令 / 快捷键 共用视觉）：单卡片 + 分隔线 + 整行 hover --- */
.sc-cmd-list,
.app-shortcut-list {
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 10px;
  background: var(--el-bg-color-overlay);
  overflow: hidden;
}
.sc-cmd-row,
.app-shortcut-row {
  display: grid;
  align-items: center;
  gap: 14px;
  padding: 9px 18px;
  transition: background-color 0.12s ease;
}
.sc-cmd-row + .sc-cmd-row,
.app-shortcut-row + .app-shortcut-row,
.sc-cmd-group + .sc-cmd-row {
  border-top: 1px solid var(--el-border-color-lighter);
}
.sc-cmd-row:not(.sc-cmd-head):hover,
.app-shortcut-row:not(.app-shortcut-head):hover {
  background: var(--el-fill-color-light);
}
/* 表头行 */
.sc-cmd-row.sc-cmd-head,
.app-shortcut-row.app-shortcut-head {
  padding: 8px 18px;
  background: var(--el-fill-color-lighter);
  font-size: 12px;
  font-weight: 600;
  color: var(--el-text-color-secondary);
  letter-spacing: 0.5px;
}
/* 快捷命令表列宽：名称 / 命令 / 快捷键 / 分组 / 操作 */
.sc-cmd-row {
  grid-template-columns: 150px minmax(0, 1fr) 140px 120px 40px;
}
/* 分组分隔行 */
.sc-cmd-group {
  padding: 6px 18px;
  background: var(--el-fill-color-lighter);
  font-size: 12px;
  font-weight: 600;
  color: var(--el-text-color-secondary);
  border-top: 1px solid var(--el-border-color-lighter);
}
.sc-op-col {
  justify-self: center;
}
.sc-command :deep(input) {
  font-family: var(--el-font-family-mono, "Cascadia Code", Consolas, monospace);
}
/* 表格底部操作行：右对齐（与终端 tab 的应用/重置一致） */
.table-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 12px;
}
.empty-tip {
  padding: 26px 18px;
  text-align: center;
  color: var(--el-text-color-secondary);
  font-size: 13px;
}

/* --- 应用级快捷键 tab --- */
.term-op-card {
  margin-top: 16px;
}
.term-op-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  padding: 10px 0;
}
.term-op-row + .term-op-row {
  border-top: 1px dashed var(--el-border-color-lighter);
}
/* --- 快捷键表（容器/行/表头视觉见上方共用规则） --- */
.app-shortcut-list {
  margin-top: 8px;
}
/* 快捷键表列宽：标题 / 描述 / 快捷键 */
.app-shortcut-row {
  grid-template-columns: 180px minmax(0, 1fr) auto;
  gap: 16px;
}
/* 冲突行：左侧色条 + 浅黄底 */
.app-shortcut-row.conflict {
  background: var(--el-color-warning-light-9);
  box-shadow: inset 3px 0 0 var(--el-color-warning);
}
.app-shortcut-row.conflict:hover {
  background: var(--el-color-warning-light-8);
}
/* 各列 */
.sc-col-key {
  display: flex;
  align-items: center;
  gap: 8px;
  justify-self: end;
}
.app-shortcut-label {
  font-size: 13px;
  font-weight: 500;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.app-shortcut-desc {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  line-height: 1.5;
}
.app-shortcut-key {
  display: flex;
  align-items: center;
  gap: 8px;
}
.key-input {
  min-width: 160px;
  height: 28px;
  padding: 0 10px;
  display: flex;
  align-items: center;
  justify-content: center;
  border: 1px dashed var(--el-border-color);
  border-radius: 6px;
  cursor: pointer;
  font-size: 12px;
  color: var(--el-text-color-regular);
  background: transparent;
  outline: none;
  user-select: none;
  transition: border-color 0.15s ease, background-color 0.15s ease;
}
.key-input:hover {
  border-color: var(--el-color-primary);
  border-style: solid;
}
.key-input.recording {
  border-color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
  color: var(--el-color-primary);
  animation: pulse 1s ease-in-out infinite;
}
@keyframes pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.6; }
}
.key-input kbd {
  font-family: var(--el-font-family-mono, monospace);
  font-size: 12px;
  padding: 2px 6px;
  background: var(--el-fill-color-dark);
  color: var(--el-color-success);
  border-radius: 3px;
}
.key-input .unbound {
  color: var(--el-text-color-placeholder);
}
.conflict-tip {
  font-size: 11px;
  color: var(--el-color-warning);
  white-space: nowrap;
}

/* --- 关于页 --- */
.about-page {
  max-width: 720px;
  margin: 0 auto;
}
.about-brand {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  padding: 24px 0 28px;
}
.about-logo {
  width: 64px;
  height: 64px;
  border-radius: 16px;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 34px;
  font-weight: 700;
  color: var(--el-color-white);
  /* 终点用 dark-2：浅色主题下 light-3 接近白色，白字对比度不足 */
  background: linear-gradient(135deg, var(--el-color-primary), var(--el-color-primary-dark-2));
  margin-bottom: 8px;
}
.about-name {
  font-size: 22px;
  font-weight: 700;
  color: var(--el-text-color-primary);
}
.about-slogan {
  font-size: 13px;
  color: var(--el-text-color-secondary);
}
.about-version {
  font-size: 12px;
  color: var(--el-text-color-placeholder);
  margin-top: 4px;
}
.about-card-title {
  font-size: 14px;
  font-weight: 600;
  color: var(--el-text-color-primary);
  margin-bottom: 14px;
}
.about-update-body {
  display: flex;
  align-items: center;
  gap: 10px;
}
.about-update-body.column {
  flex-direction: column;
  align-items: stretch;
  gap: 12px;
}
.about-new-ver {
  display: flex;
  align-items: center;
}
.about-notes {
  margin: 0;
  padding: 10px 12px;
  font-size: 12px;
  line-height: 1.7;
  white-space: pre-wrap;
  word-break: break-all;
  background: var(--el-fill-color-light);
  border-radius: 6px;
  color: var(--el-text-color-regular);
  max-height: 200px;
  overflow-y: auto;
  font-family: inherit;
}
.about-actions {
  display: flex;
  gap: 10px;
}
.about-dl-meta {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  text-align: center;
}
.about-hint {
  font-size: 12px;
  color: var(--el-text-color-placeholder);
}
.about-meta-grid {
  display: grid;
  grid-template-columns: 90px 1fr;
  row-gap: 8px;
  column-gap: 12px;
  font-size: 13px;
}
.about-meta-grid .k {
  color: var(--el-text-color-secondary);
}
.about-meta-grid .v {
  color: var(--el-text-color-primary);
  word-break: break-all;
}
.about-link {
  color: var(--el-color-primary);
  text-decoration: none;
}
.about-link:hover {
  text-decoration: underline;
}
</style>
