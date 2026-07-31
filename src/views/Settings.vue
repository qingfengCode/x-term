<!--
  Settings.vue — 应用设置视图

  两个 Tab：
  1. 终端：主题 / 字体 / 字号 / 行高 / 滚屏 / 选中复制 / WebGL 渲染
  2. AI 助手（BYOK 多模型）：管理 provider 列表、设置激活模型

  所有改动通过 settingsStore.save() 持久化；主题切换同时切换 document.documentElement 的 'dark' class。
-->
<script setup lang="ts">
import { computed, onMounted, reactive, ref, watch } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import type { FormInstance, FormRules } from "element-plus";
import { Delete, Plus, Refresh } from "@element-plus/icons-vue";
import { useSettingsStore } from "@/stores/settings";
import { ProviderKind } from "@/api/types";
import { APP_SHORTCUT_METAS, SQL_MODE_OPTIONS, defaultAppShortcuts } from "@/api/types";
import { eventToCombo, isModifierOnly } from "@/utils/shortcut";
import type {
  AppShortcutAction,
  AppShortcuts,
  ProviderConfig,
  ProviderKind as ProviderKindType,
  ShortcutCommand,
  TerminalSettings,
} from "@/api/types";

const settings = useSettingsStore();
const activeTab = ref<"terminal" | "shortcuts" | "appShortcuts" | "ai">("terminal");

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

// AI 激活模型（value 为 `${kind}:${model}` 字符串）
const activeValue = ref<string>("");

// 添加 provider 弹窗
const providerDialogVisible = ref(false);
const providerFormRef = ref<FormInstance>();
const providerForm = reactive<{
  kind: ProviderKindType;
  baseUrl: string;
  apiKey: string;
  model: string;
}>({
  kind: ProviderKind.OpenAi,
  baseUrl: defaultBaseUrl(ProviderKind.OpenAi),
  apiKey: "",
  model: "",
});

const providerRules: FormRules = {
  kind: [{ required: true, message: "请选择类型", trigger: "change" }],
  baseUrl: [{ required: false }],
  apiKey: [{ required: true, message: "请输入 API Key", trigger: "blur" }],
  model: [{ required: true, message: "请输入模型名", trigger: "blur" }],
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

// provider 列表选项（用于激活模型 select）
const activeOptions = computed(() =>
  settings.aiProviders.map((p) => ({
    value: `${p.kind}:${p.model}`,
    label: `${kindLabel(p.kind)} / ${p.model}`,
  })),
);

// 当前激活模型同步到 select
watch(
  () => [settings.aiActive, settings.aiProviders] as const,
  ([active]) => {
    if (!active) {
      activeValue.value = "";
      return;
    }
    const [kind, model] = active.split(":");
    // 校验是否仍存在于列表中
    const exists = settings.aiProviders.some((p) => p.kind === kind && p.model === model);
    activeValue.value = exists ? active : "";
  },
  { immediate: true },
);

async function onActiveChange(val: string) {
  settings.aiActive = val || null;
  try {
    await settings.save();
    ElMessage.success("已切换激活模型");
  } catch (e: any) {
    ElMessage.error("保存失败：" + (e?.message ?? String(e)));
  }
}

function openProviderDialog() {
  providerForm.kind = ProviderKind.OpenAi;
  providerForm.baseUrl = defaultBaseUrl(ProviderKind.OpenAi);
  providerForm.apiKey = "";
  providerForm.model = "";
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
  };
  // 同 kind+model 视为重复，覆盖 apiKey/baseUrl
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
  try {
    await settings.save();
    ElMessage.success("已添加 provider");
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
    ElMessage.success("已设为激活");
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

/** SSH 开关（白名单自动放行/终端可视化）切换后立即保存。 */
async function saveSshSwitches() {
  try {
    await settings.save();
  } catch (e: unknown) {
    ElMessage.error("保存失败：" + String(e));
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
/** SQL 开关（模式/只读自动放行）切换后立即保存。 */
async function saveSqlSwitches() {
  try {
    await settings.save();
  } catch (e: unknown) {
    ElMessage.error("保存失败：" + String(e));
  }
}
</script>

<template>
  <div class="settings-view">
    <div class="header">
      <h2>设置</h2>
    </div>

    <el-tabs v-model="activeTab" class="settings-tabs">
      <!-- ============ 终端 ============ -->
      <el-tab-pane label="终端" name="terminal">
        <div class="form-card">
          <el-form :model="termForm" label-width="140px" label-position="right">
            <el-form-item label="主题">
              <el-select v-model="termForm.theme" style="width: 220px">
                <el-option label="深色 (dark)" value="dark" />
                <el-option label="浅色 (light)" value="light" />
              </el-select>
            </el-form-item>

            <el-form-item label="字体">
              <el-input
                v-model="termForm.fontFamily"
                placeholder="Consolas, 'Cascadia Code', monospace"
                style="width: 360px"
              />
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

            <el-form-item label="启用 WebGL 渲染">
              <el-switch v-model="termForm.enableWebgl" />
            </el-form-item>

            <el-form-item>
              <el-button type="primary" @click="applyTerminal">应用</el-button>
              <el-button @click="resetTerminal">重置</el-button>
            </el-form-item>
          </el-form>
        </div>
      </el-tab-pane>

      <!-- ============ 快捷命令 / 快捷键 ============ -->
      <el-tab-pane label="快捷命令" name="shortcuts">
        <div class="tab-section">
          <div class="section-desc">
            配置终端底部快捷命令栏与全局快捷键。点击按钮或按下快捷键即向当前活动终端发送命令。
            <br />支持占位符：<code>{"{host}"}</code>、<code>{"{user}"}</code>、<code>{"{port}"}</code>（按当前会话替换）。
          </div>

          <div class="shortcut-list">
            <div
              v-for="sc in settings.shortcuts"
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
        </div>
      </el-tab-pane>

      <!-- ============ 快捷键（应用级） ============ -->
      <el-tab-pane label="快捷键" name="appShortcuts">
        <div class="tab-section">
          <div class="section-desc">
            自定义应用级快捷键。点击右侧输入框开始录制，按下组合键即可绑定；
            <b>Esc</b> 取消录制，<b>Backspace</b> 清除当前绑定。冲突会高亮提示。
          </div>

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
        </div>
      </el-tab-pane>

      <!-- ============ AI 助手 ============ -->
      <el-tab-pane label="AI 助手" name="ai">
        <el-alert
          class="ai-tip"
          type="warning"
          :closable="false"
          show-icon
          title="AI 数据会上传到所选模型服务，请勿输入敏感信息"
        />

        <div class="form-card">
          <el-form label-width="140px" label-position="right">
            <el-form-item label="当前激活模型">
              <el-select
                v-model="activeValue"
                placeholder="未选择"
                style="width: 360px"
                @change="onActiveChange"
              >
                <el-option
                  v-for="opt in activeOptions"
                  :key="opt.value"
                  :label="opt.label"
                  :value="opt.value"
                />
              </el-select>
              <el-button class="ml" type="primary" plain @click="openProviderDialog">
                添加 Provider
              </el-button>
            </el-form-item>
          </el-form>
        </div>

        <div class="form-card no-pad">
          <el-table :data="settings.aiProviders" empty-text="尚未添加任何 provider" stripe>
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

            <el-table-column label="操作" width="180" align="center" fixed="right">
              <template #default="{ row }">
                <el-button
                  size="small"
                  :disabled="settings.aiActive === `${row.kind}:${row.model}`"
                  @click="setActive(row)"
                >
                  设为激活
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
          <div class="card-title">SSH 智能体（终端命令执行）</div>
          <div class="card-desc">
            控制 AI 在 SSH 终端上执行命令的行为：白名单、自动放行、可视化。
          </div>
          <div class="switch-row">
            <div class="switch-label">
              <div>白名单自动放行</div>
              <div class="switch-desc">
                开启后，命令落在白名单内时<strong>免确认按钮</strong>直接执行。
                危险命令（rm/mkfs/dd 等）仍强制人工确认。
              </div>
            </div>
            <el-switch v-model="settings.sshAgent.autoApproveSafe" @change="saveSshSwitches" />
          </div>
          <div class="switch-row">
            <div class="switch-label">
              <div>终端可视化执行</div>
              <div class="switch-desc">
                开启后，AI 执行的命令会<strong>写入活动终端</strong>，命令和输出
                实时显示在终端窗口（像 AI 在终端里敲命令）。关闭则走后台独立执行，
                输出只在 AI 面板。
              </div>
            </div>
            <el-switch v-model="settings.sshAgent.terminalVisualization" @change="saveSshSwitches" />
          </div>
          <div class="card-title" style="margin-top: 16px; font-size: 13px">命令白名单</div>
          <div class="card-desc">
            允许 AI 在服务器上<strong>免确认</strong>执行的命令前缀（每行一条）。
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
          <div class="card-title">SQL 智能体（数据库语句执行）</div>
          <div class="card-desc">
            控制 AI 在 MySQL 上执行 SQL 的行为：执行模式与自动放行。
          </div>
          <div class="switch-row">
            <div class="switch-label">
              <div>SQL 执行模式</div>
              <div class="switch-desc">
                限制 AI 可执行的 SQL 类型。模式越严格越安全。
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
          <div class="mode-desc">
            {{ SQL_MODE_OPTIONS.find((o) => o.value === settings.sqlAgent.sqlMode)?.desc }}
          </div>
          <div class="switch-row" style="margin-top: 12px">
            <div class="switch-label">
              <div>只读查询自动放行</div>
              <div class="switch-desc">
                开启后，SELECT/SHOW/EXPLAIN/DESCRIBE 等<strong>只读查询</strong>免确认直接执行。
                写操作（INSERT/UPDATE/DELETE/DDL）仍需人工确认。
              </div>
            </div>
            <el-switch v-model="settings.sqlAgent.autoApproveSafe" @change="saveSqlSwitches" />
          </div>
          <div class="switch-row" style="margin-top: 12px">
            <div class="switch-label">
              <div>终端可视化执行</div>
              <div class="switch-desc">
                开启后，AI 执行的 SQL 及其结果<strong>回显到 SQL 控制台</strong>（命令行模式），
                就像你手动执行一样。关闭则结果只在 AI 面板。与 SSH 助手的可视化独立设置。
              </div>
            </div>
            <el-switch v-model="settings.sqlAgent.terminalVisualization" @change="saveSqlSwitches" />
          </div>
        </div>

        <!-- 添加 Provider 弹窗 -->
        <el-dialog
          v-model="providerDialogVisible"
          title="添加 Provider"
          width="520px"
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
          </el-form>

          <template #footer>
            <el-button @click="providerDialogVisible = false">取消</el-button>
            <el-button type="primary" @click="submitProvider">添加</el-button>
          </template>
        </el-dialog>
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
}

.settings-tabs {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
}
/* 让 el-tabs 内容区滚动（tab header 固定，内容超出时滚动） */
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
  background: var(--el-bg-color);
  border: 1px solid var(--el-border-color-light);
  border-radius: 8px;
  padding: 24px;
  margin-bottom: 16px;
}

.form-card.no-pad {
  padding: 0;
  overflow: hidden;
}

.ai-tip {
  margin-bottom: 16px;
}

.ml {
  margin-left: 12px;
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
/* SQL 模式说明（选择框下方）。 */
.mode-desc {
  font-size: 11px;
  color: var(--el-text-color-placeholder);
  margin: 4px 0 0 156px; /* 与 switch-label 右侧对齐 */
  line-height: 1.5;
}
.card-desc code {
  background: var(--el-fill-color-light);
  padding: 1px 4px;
  border-radius: 3px;
  font-size: 11px;
}
.switch-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  padding: 8px 0;
  border-top: 1px solid var(--el-border-color-lighter);
}
.switch-row:first-of-type {
  border-top: none;
}
.switch-label > div:first-child {
  font-size: 13px;
  font-weight: 500;
}
.switch-desc {
  font-size: 12px;
  line-height: 1.5;
  color: var(--el-text-color-secondary);
  margin-top: 2px;
}

/* --- 快捷命令 tab --- */
.shortcut-list {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 12px 0;
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
</style>
