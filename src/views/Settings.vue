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
import { Delete, Plus, Refresh, Folder, Setting, Download, Upload } from "@element-plus/icons-vue";
import HelpTip from "@/components/HelpTip.vue";
import { open, save } from "@tauri-apps/plugin-dialog";
import { setWorkspaceDir } from "@/api/ai";
import * as backupApi from "@/api/backup";
import type { BackupInfo } from "@/api/backup";
import { useSettingsStore } from "@/stores/settings";
import { useUpdateStore } from "@/stores/update";
import { useVaultStore } from "@/stores/vault";
import { useMcpStore } from "@/stores/mcp";
import { ProviderKind, PROVIDER_DEFAULTS } from "@/api/types";
import {
  APP_SHORTCUT_METAS,
  RUN_MODE_OPTIONS,
  SQL_MODE_OPTIONS,
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

// 终端表单（与 store 解耦的本地副本，点"应用"才写回）
const termForm = reactive<TerminalSettings>({ ...settings.terminal });

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
  { value: ProviderKind.Anthropic, label: "Anthropic" },
  { value: ProviderKind.DeepSeek, label: "DeepSeek" },
  { value: ProviderKind.Zhipu, label: "智谱 (Zhipu)" },
  { value: ProviderKind.Ollama, label: "Ollama" },
  { value: ProviderKind.OpenAiCompatible, label: "OpenAI 兼容" },
];

function defaultBaseUrl(kind: ProviderKindType): string {
  switch (kind) {
    case ProviderKind.OpenAi:
      return "https://api.openai.com/v1";
    case ProviderKind.Anthropic:
      return "https://api.anthropic.com";
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
  (val) => {
    toggleDarkClass(val);
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
  toggleDarkClass(termForm.theme);
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

function truncate(s: string, n = 36): string {
  if (!s) return "-";
  return s.length > n ? s.slice(0, n) + "…" : s;
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
  toggleDarkClass(termForm.theme);
  // 同步 SSH 命令白名单到 textarea 文本（每行一条）。
  whitelistText.value = settings.sshAgent.commandWhitelist.join("\n");
  // 同步应用快捷键到本地副本。
  Object.assign(appForm, defaultAppShortcuts(), settings.appShortcuts);
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

  importing.value = true;
  try {
    const summary = await backupApi.backupImport(picked, pwd, importMode.value);
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
            </el-form>
          </div>

          <div class="form-card">
            <div class="card-title">连接</div>
            <el-form :model="termForm" label-width="100px" label-position="right">
              <el-form-item label="SSH 空闲断开">
                <el-input-number
                  v-model="termForm.sshIdleTimeoutMinutes"
                  :min="0"
                  :max="1440"
                  controls-position="right"
                />
                <span class="unit-hint">分钟</span>
                <HelpTip content="0 表示永不自动断开" />
              </el-form-item>

              <el-form-item label="SSH 保活间隔">
                <el-input-number
                  v-model="termForm.sshKeepaliveSecs"
                  :min="0"
                  :max="3600"
                  controls-position="right"
                />
                <span class="unit-hint">秒</span>
                <HelpTip content="0 表示不发送保活包（建议保持默认 30）" />
              </el-form-item>

              <el-form-item label="SSH 连接超时">
                <el-input-number
                  v-model="termForm.sshConnectTimeoutSecs"
                  :min="0"
                  :max="600"
                  controls-position="right"
                />
                <span class="unit-hint">秒</span>
                <HelpTip content="0 表示永不超时" />
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
          快捷命令
          <HelpTip :size="12">
            <template #content>
              配置终端底部快捷命令栏与全局快捷键。点击按钮或按下快捷键即向当前活动终端发送命令。
              <br />支持占位符：<code>{"{host}"}</code>、<code>{"{user}"}</code>、<code>{"{port}"}</code>（按当前会话替换）。
            </template>
          </HelpTip>
        </template>
          <div class="shortcut-list">
            <template v-for="gv in shortcutGroupsView" :key="gv.group || '__default__'">
              <div class="shortcut-group-title">{{ gv.group || "默认" }}</div>
              <div
                v-for="sc in gv.items"
                :key="sc.id"
                class="shortcut-row"
              >
              <el-input
                v-model="sc.label"
                placeholder="显示名称"
                size="small"
                style="width: 120px"
              />
              <el-input
                v-model="sc.command"
                placeholder="命令（不含换行）"
                size="small"
                style="flex: 1"
                class="mono"
              />
              <el-input
                v-model="sc.shortcut"
                placeholder="如 Ctrl+1（可选）"
                size="small"
                style="width: 130px"
                @keydown="onShortcutKeyCapture(sc, $event)"
              />
              <el-select
                v-model="sc.group"
                placeholder="分组"
                size="small"
                style="width: 110px"
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
                type="danger"
                size="small"
                link
                @click="removeShortcut(sc.id)"
              >
                <el-icon><Delete /></el-icon>
              </el-button>
            </div>
            </template>
            <div v-if="settings.shortcuts.length === 0" class="empty-tip">
              暂无快捷命令。
            </div>
          </div>

          <div class="shortcut-actions">
            <el-button size="small" :icon="Plus" @click="addShortcut">新增快捷命令</el-button>
            <el-button size="small" type="primary" @click="saveShortcuts">
              保存快捷命令
            </el-button>
          </div>
      </el-tab-pane>

      <!-- ============ 快捷命令 / 快捷键 ============ -->
      <el-tab-pane name="appShortcuts">
        <template #label>
          快捷键
          <HelpTip :size="12">
            <template #content>
              自定义应用级快捷键。点击右侧输入框开始录制，按下组合键即可绑定；
              <b>Esc</b> 取消录制，<b>Backspace</b> 清除当前绑定。冲突会高亮提示。
            </template>
          </HelpTip>
        </template>
          <div class="app-shortcut-list">
            <div
              v-for="row in appRows"
              :key="row.action"
              class="app-shortcut-row"
              :class="{ conflict: row.conflict }"
            >
              <div class="app-shortcut-info">
                <div class="app-shortcut-label">{{ row.label }}</div>
                <div class="app-shortcut-desc">{{ row.description }}</div>
              </div>
              <div class="app-shortcut-key">
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
              </div>
              <div v-if="row.conflict" class="conflict-tip">与其他绑定冲突</div>
            </div>
          </div>

          <div class="shortcut-actions">
            <el-button size="small" :icon="Refresh" @click="resetAppShortcuts">
              恢复默认
            </el-button>
            <el-button size="small" type="primary" @click="saveAppShortcuts">
              保存快捷键
            </el-button>
          </div>
      </el-tab-pane>

      <!-- ============ AI 助手 ============ -->
      <el-tab-pane label="AI 助手" name="ai" class="ai-pane">
        <el-alert
          class="ai-tip"
          type="warning"
          :closable="false"
          show-icon
          title="AI 数据会上传到所选模型服务，请勿输入敏感信息"
        />

        <div class="form-card no-pad span-full">
          <div class="table-head">
            <span class="table-title">模型列表</span>
            <el-button size="small" type="primary" plain :icon="Plus" @click="openProviderDialog">
              添加 Provider
            </el-button>
          </div>
          <el-table
            :data="settings.aiProviders"
            empty-text="尚未添加任何 provider"
            stripe
            max-height="460"
          >
            <el-table-column label="类型" width="160">
              <template #default="{ row }">{{ kindLabel(row.kind) }}</template>
            </el-table-column>

            <el-table-column label="Base URL" min-width="220">
              <template #default="{ row }">
                <span class="mono">{{ truncate(row.baseUrl) }}</span>
              </template>
            </el-table-column>

            <el-table-column label="模型" min-width="160">
              <template #default="{ row }">
                <span class="mono">{{ row.model }}</span>
                <el-tag v-if="row.multimodal" type="warning" size="small" effect="plain" class="mm-tag">
                  多模态
                </el-tag>
              </template>
            </el-table-column>

            <el-table-column label="参数" min-width="220">
              <template #default="{ row }">
                <span class="muted mono">
                  输出 {{ row.maxOutput ?? 16000 }} · 窗口 {{ row.contextWindow ?? 184000 }} ·
                  工具 {{ row.maxToolCalls ?? 200 }} 次
                  <template v-if="row.temperature != null"> · 温度 {{ row.temperature }}</template>
                </span>
              </template>
            </el-table-column>

            <el-table-column label="API Key" width="120">
              <template #default>
                <span class="secret">***</span>
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
            SSH 智能体（终端命令执行）
            <HelpTip content="控制 AI 在 SSH 终端上执行命令的行为：运行模式、白名单、可视化。" />
          </div>
          <div class="switch-row">
            <div class="switch-label">
              <div>
                运行模式
                <HelpTip :content="sshRunModeHint" />
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
                终端可视化执行
                <HelpTip content="开启后，AI 执行的命令会写入活动终端，命令和输出实时显示在终端窗口（像 AI 在终端里敲命令）。关闭则走后台独立执行，输出只在 AI 面板。" />
              </div>
            </div>
            <el-switch v-model="settings.sshAgent.terminalVisualization" @change="saveSshSwitches" />
          </div>
          <div class="card-title" style="margin-top: 16px; font-size: 13px">
            命令白名单
            <HelpTip content="允许 AI 在服务器上免确认执行的命令前缀（每行一条）。" />
          </div>
          <!-- 安全规则：保持直接展示，不收进 tooltip -->
          <div class="card-desc warn">
            注意：含 shell 元字符（<code>; &amp; | &gt; &lt; ` $()</code>）的命令
            <strong>永不</strong>算白名单内——防止 <code>ls; rm -rf /</code> 绕过。
          </div>
          <el-input
            v-model="whitelistText"
            type="textarea"
            :autosize="{ minRows: 6, maxRows: 16 }"
            placeholder="每行一个命令前缀，例如：&#10;df&#10;free&#10;ps&#10;systemctl status"
            style="margin-top: 8px"
          />
          <div style="margin-top: 10px; display: flex; gap: 8px">
            <el-button type="primary" @click="saveWhitelist">保存白名单</el-button>
            <el-button @click="resetWhitelist">恢复默认</el-button>
          </div>
        </div>

        <!-- SQL 智能体配置 -->
        <div class="form-card">
          <div class="card-title">
            SQL 智能体（数据库语句执行）
            <HelpTip content="控制 AI 在 MySQL 上执行 SQL 的行为：执行模式与运行模式。" />
          </div>
            <div class="switch-row">
              <div class="switch-label">
                <div>
                  运行模式
                  <HelpTip :content="sqlRunModeHint" />
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
                  终端可视化执行
                  <HelpTip content="开启后，AI 执行的 SQL 及其结果回显到 SQL 控制台（命令行模式），就像你手动执行一样。关闭则结果只在 AI 面板。与终端助手的可视化独立设置。" />
                </div>
              </div>
              <el-switch v-model="settings.sqlAgent.terminalVisualization" @change="saveSqlSwitches" />
            </div>
            <div class="switch-row">
              <div class="switch-label">
                <div>
                  SQL 执行模式
                  <HelpTip :content="sqlModeHint" />
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
            本地文件读写
            <HelpTip content="开启后，AI 可在各助手的工作目录内自动读写文件（读取数据文件、导出查询结果为 CSV/SQL 等）。AI 只能访问工作目录及子目录（沙箱），写文件覆盖已有文件仍需人工确认。关闭时 AI 无法访问本地文件。" />
          </div>
          <div class="switch-row">
            <div class="switch-label">
              <div>
                启用本地文件读写
                <HelpTip content="关闭时 AI 行为与之前完全一致。" />
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
                  {{ d === "ssh" ? "终端助手" : "数据库助手" }}工作目录
                  <HelpTip content="AI 读写文件的根目录（可读写其子目录）。未设置时 AI 无法使用文件工具。" />
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
          width="560px"
          :close-on-click-modal="false"
        >
          <el-form
            ref="providerFormRef"
            :model="providerForm"
            :rules="providerRules"
            label-width="100px"
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

            <el-form-item prop="maxOutput">
              <template #label>
                最大输出
                <HelpTip content="tokens（请求体 max_tokens）" />
              </template>
              <el-input-number
                v-model="providerForm.maxOutput"
                :min="1"
                :max="1000000"
                :step="1024"
                style="width: 180px"
              />
            </el-form-item>

            <el-form-item prop="contextWindow">
              <template #label>
                上下文大小
                <HelpTip content="tokens，超出部分的历史消息会被裁剪" />
              </template>
              <el-input-number
                v-model="providerForm.contextWindow"
                :min="1"
                :max="10000000"
                :step="4096"
                style="width: 180px"
              />
            </el-form-item>

            <el-form-item prop="maxToolCalls">
              <template #label>
                工具调用数
                <HelpTip content="智能体模式单次对话的最大工具调用数" />
              </template>
              <el-input-number
                v-model="providerForm.maxToolCalls"
                :min="1"
                :max="1000"
                style="width: 180px"
              />
            </el-form-item>

            <el-form-item prop="temperature">
              <template #label>
                温度
                <HelpTip content="采样温度，留空表示不发送" />
              </template>
              <el-input-number
                v-model="providerForm.temperature"
                :min="0"
                :max="2"
                :step="0.1"
                :precision="1"
                :controls="false"
                placeholder="留空由服务端默认"
                style="width: 180px"
              />
            </el-form-item>

            <el-form-item prop="connectTimeoutSecs">
              <template #label>
                建连超时
                <HelpTip content="秒，DNS 解析/建连最长等待" />
              </template>
              <el-input-number
                v-model="providerForm.connectTimeoutSecs"
                :min="1"
                :max="3600"
                style="width: 180px"
              />
            </el-form-item>

            <el-form-item prop="readTimeoutSecs">
              <template #label>
                读取超时
                <HelpTip content="秒，流式响应间隔最长等待；长思考模型需调大" />
              </template>
              <el-input-number
                v-model="providerForm.readTimeoutSecs"
                :min="1"
                :max="3600"
                style="width: 180px"
              />
            </el-form-item>
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
            保险库与锁定
            <HelpTip content="凭据保险库用主密码保护所有保存的密码与私钥，解锁状态保存在应用进程内。" />
          </div>
          <div class="switch-row">
            <div class="switch-label">
              <div>
                启用锁定功能
                <HelpTip content="开启后，左侧导航栏底部显示「锁定」按钮。点击锁定即清除内存中的主密钥，需重新输入主密码才能解锁。已建立的连接（终端 / SFTP / 隧道 / 数据库）不受影响，可继续使用。" />
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
              导出加密备份
              <HelpTip content="将会话、数据库连接、凭据（密码/私钥）、TOTP、转发规则、桌面连接、文件账号、应用设置与 MCP 配置导出为加密备份文件（.xtermbackup）。文件使用独立的备份密码（Argon2id + AES-256-GCM）加密，可安全转移到其他机器。" />
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
              从备份导入
              <HelpTip content="选择之前导出的加密备份文件并输入对应的备份密码。合并导入按 id 覆盖同名条目、保留本机其他数据；覆盖导入会先清空本机相关数据再完整恢复（同一事务，失败自动回滚）。" />
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
            </div>
          </div>
        </div>
      </el-tab-pane>
    </el-tabs>
  </div>
</template>

<style scoped>
.settings-view {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 16px;
  height: 100%;
  box-sizing: border-box;
  overflow: hidden;
}

.header h2 {
  margin: 0;
  font-size: 20px;
  font-weight: 600;
  color: var(--el-text-color-primary);
  display: flex;
  align-items: center;
  gap: 8px;
}

/* 顶部 Tab 导航 + 下方内容区滚动 */
.settings-tabs {
  flex: 1;
  min-height: 0;
  display: flex;
}
.settings-tabs :deep(.el-tabs__header) {
  margin: 0;
  flex-shrink: 0;
}
.settings-tabs :deep(.el-tabs__content) {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 0 4px;
}

.form-card {
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 8px;
  padding: 20px;
  margin-bottom: 16px;
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
.card-actions {
  display: flex;
  gap: 8px;
}

/* AI 助手 Tab：SSH / SQL 配置卡片两列并排，其余横跨整行 */
.ai-pane .ai-tip {
  margin-bottom: 0;
}
.ai-pane .span-full {
  grid-column: 1 / -1;
}

.form-card.no-pad {
  padding: 0;
  overflow: hidden;
}

.ai-tip {
  margin-bottom: 16px;
}

/* 表格卡片头部（标题 + 操作按钮） */
.table-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 16px;
  border-bottom: 1px solid var(--el-border-color-lighter);
}
.table-title {
  font-size: 14px;
  font-weight: 600;
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

.card-title {
  font-size: 14px;
  font-weight: 600;
  margin-bottom: 4px;
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
  padding: 12px 0;
}
/* 分隔线只出现在相邻开关行之间（卡片标题后的首行无线） */
.switch-row + .switch-row {
  border-top: 1px solid var(--el-border-color-lighter);
}
.switch-label > div:first-child {
  font-size: 13px;
  font-weight: 500;
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

/* --- 快捷命令 tab --- */
.shortcut-list {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 12px 0;
}
.shortcut-group-title {
  font-size: 12px;
  font-weight: 600;
  color: var(--el-text-color-secondary);
  margin: 12px 0 2px;
  padding-left: 2px;
}
.shortcut-group-title:first-child {
  margin-top: 0;
}
.shortcut-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.shortcut-row .mono :deep(input) {
  font-family: var(--el-font-family-mono, "Cascadia Code", Consolas, monospace);
}
.shortcut-actions {
  display: flex;
  gap: 8px;
  margin-top: 8px;
}
.empty-tip {
  padding: 16px;
  text-align: center;
  color: var(--el-text-color-secondary);
  font-size: 13px;
}

/* --- 应用级快捷键 tab --- */
.app-shortcut-list {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin-top: 8px;
}
.app-shortcut-row {
  display: grid;
  grid-template-columns: 1fr auto;
  align-items: center;
  gap: 12px;
  padding: 8px 10px;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  position: relative;
}
.app-shortcut-row.conflict {
  border-color: var(--el-color-warning);
  background: var(--el-color-warning-light-9);
}
.app-shortcut-info {
  min-width: 0;
}
.app-shortcut-label {
  font-size: 13px;
  font-weight: 500;
}
.app-shortcut-desc {
  font-size: 11px;
  color: var(--el-text-color-secondary);
  margin-top: 2px;
}
.app-shortcut-key {
  display: flex;
  align-items: center;
  gap: 8px;
}
.key-input {
  min-width: 160px;
  height: 30px;
  padding: 0 10px;
  display: flex;
  align-items: center;
  justify-content: center;
  border: 1px solid var(--el-border-color);
  border-radius: 4px;
  cursor: pointer;
  font-size: 12px;
  color: var(--el-text-color-regular);
  background: var(--el-fill-color-blank);
  outline: none;
  user-select: none;
}
.key-input:hover {
  border-color: var(--el-color-primary);
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
  grid-column: 1 / -1;
  font-size: 11px;
  color: var(--el-color-warning);
  margin-top: 4px;
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
</style>
