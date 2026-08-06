import { defineStore } from "pinia";
import { ref } from "vue";
import * as configApi from "@/api/config";
import {
  defaultAppShortcuts,
  PROVIDER_DEFAULTS,
  type AppShortcutAction,
  type AppShortcuts,
  type FileAccessSettings,
  type Settings,
  type ShortcutCommand,
  type SkillConfig,
  type SshAgentSettings,
  type SqlAgentSettings,
  type TerminalSettings,
} from "@/api/types";

const defaultSshAgent: SshAgentSettings = {
  commandWhitelist: ["ls", "pwd", "whoami", "date", "uptime", "df", "free", "ps", "cat", "grep"],
  runMode: "manual",
  terminalVisualization: false,
};
const defaultSqlAgent: SqlAgentSettings = {
  sqlMode: "readonly",
  runMode: "manual",
  terminalVisualization: false,
};
const defaultFileAccess: FileAccessSettings = {
  enabled: false,
  workspaceDirs: {},
};

const defaultTerminal: TerminalSettings = {
  theme: "dark",
  fontFamily: "Consolas, 'Cascadia Code', 'Courier New', monospace",
  fontSize: 14,
  lineHeight: 1.2,
  scrollback: 10000,
  copyOnSelect: true,
  enableWebgl: true,
  sshIdleTimeoutMinutes: 30,
};

export const useSettingsStore = defineStore("settings", () => {
  const terminal = ref<TerminalSettings>({ ...defaultTerminal });
  const aiProviders = ref<Settings["ai"]["providers"]>([]);
  const aiActive = ref<string | null>(null);
  /** SSH 智能体配置（exec_ssh）。 */
  const sshAgent = ref<SshAgentSettings>({ ...defaultSshAgent });
  /** SQL 智能体配置（exec_sql）。 */
  const sqlAgent = ref<SqlAgentSettings>({ ...defaultSqlAgent });
  /** 本地文件读写配置（read_file / write_file / list_files）。 */
  const fileAccess = ref<FileAccessSettings>({ ...defaultFileAccess, workspaceDirs: {} });
  /** 可复用 skill 列表（注入对应 domain 的 system prompt）。 */
  const skills = ref<SkillConfig[]>([]);
  /** 快捷命令列表（终端底部按钮栏 + 快捷键绑定）。 */
  const shortcuts = ref<ShortcutCommand[]>([]);
  /** 快捷命令有序分组名列表。 */
  const shortcutGroups = ref<string[]>([]);
  /** 应用级快捷键绑定（action -> 组合键）。 */
  const appShortcuts = ref<AppShortcuts>(defaultAppShortcuts());
  /** 会话侧栏宽度（px），拖拽调整后持久化。 */
  const sidebarWidth = ref(240);
  /** 最近成功连接的会话 id（最近的在前，最多 10 个）。 */
  const recentSessionIds = ref<string[]>([]);
  const loaded = ref(false);

  async function load() {
    const s = await configApi.settingsLoad();
    terminal.value = { ...defaultTerminal, ...s.terminal };
    // 旧配置缺模型参数字段时补默认值（后端 serde default 已兜底，这里防御旧前端缓存）。
    aiProviders.value = (s.ai.providers ?? []).map((p) => ({
      ...PROVIDER_DEFAULTS,
      ...p,
    }));
    aiActive.value = s.ai.active;
    // sshAgent：优先用新字段；缺失时从旧字段迁移（后端已迁移一次，这里兼容旧前端缓存）。
    if (s.ai.sshAgent) {
      sshAgent.value = { ...defaultSshAgent, ...s.ai.sshAgent };
    } else {
      sshAgent.value = {
        commandWhitelist: s.ai.commandWhitelist ?? defaultSshAgent.commandWhitelist,
        // 旧"白名单自动放行"开关 → 白名单运行模式。
        runMode: s.ai.autoApproveWhitelist ? "whitelist" : defaultSshAgent.runMode,
        terminalVisualization: s.ai.terminalVisualization ?? false,
      };
    }
    sqlAgent.value = { ...defaultSqlAgent, ...(s.ai.sqlAgent ?? {}) };
    fileAccess.value = {
      enabled: s.ai.fileAccess?.enabled ?? false,
      workspaceDirs: { ...(s.ai.fileAccess?.workspaceDirs ?? {}) },
    };
    skills.value = s.ai.skills ?? [];
    shortcuts.value = s.shortcuts?.commands ?? [];
    shortcutGroups.value = s.shortcuts?.groups ?? [];
    appShortcuts.value = { ...defaultAppShortcuts(), ...(s.shortcuts?.app ?? {}) };
    // 旧配置文件没有这些字段（或后端默认 0）时回退默认值。
    sidebarWidth.value =
      typeof s.sidebarWidth === "number" && s.sidebarWidth >= 120 ? s.sidebarWidth : 240;
    recentSessionIds.value = s.recentSessionIds ?? [];
    loaded.value = true;
  }

  async function save() {
    const s: Settings = {
      terminal: terminal.value,
      ai: {
        providers: aiProviders.value,
        active: aiActive.value,
        sshAgent: sshAgent.value,
        sqlAgent: sqlAgent.value,
        fileAccess: fileAccess.value,
        skills: skills.value,
      },
      shortcuts: { commands: shortcuts.value, groups: shortcutGroups.value, app: appShortcuts.value },
      firstRun: false,
      sidebarWidth: sidebarWidth.value,
      recentSessionIds: recentSessionIds.value,
    };
    await configApi.settingsSave(s);
  }

  /** 读取某个应用快捷键的当前绑定（无则返回默认）。 */
  function getAppShortcut(action: AppShortcutAction): string {
    return appShortcuts.value[action] ?? "";
  }

  /** 设置某个应用快捷键绑定（传空串清除绑定）。 */
  function setAppShortcut(action: AppShortcutAction, combo: string) {
    appShortcuts.value = { ...appShortcuts.value, [action]: combo };
  }

  /** 重置全部应用快捷键为默认。 */
  function resetAppShortcuts() {
    appShortcuts.value = defaultAppShortcuts();
  }

  function setTerminal(patch: Partial<TerminalSettings>) {
    terminal.value = { ...terminal.value, ...patch };
  }

  /** 设置会话侧栏宽度（调用方负责 save() 持久化）。 */
  function setSidebarWidth(w: number) {
    sidebarWidth.value = Math.round(w);
  }

  /** 记录一次成功连接：去重置顶，最多保留 10 个（调用方负责 save() 持久化）。 */
  function recordRecentSession(id: string) {
    recentSessionIds.value = [id, ...recentSessionIds.value.filter((x) => x !== id)].slice(0, 10);
  }

  /** 新增一条快捷命令（返回新 id）。 */
  function addShortcut(group?: string): string {
    const id = `sc-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    shortcuts.value.push({ id, label: "新命令", command: "", shortcut: null, group: group ?? null });
    return id;
  }

  /** 更新快捷命令（按 id）。 */
  function updateShortcut(id: string, patch: Partial<ShortcutCommand>) {
    const s = shortcuts.value.find((x) => x.id === id);
    if (s) Object.assign(s, patch);
  }

  /** 删除快捷命令。 */
  function removeShortcut(id: string) {
    shortcuts.value = shortcuts.value.filter((x) => x.id !== id);
  }

  // --- 快捷命令分组管理 ---------------------------------------------------

  /** 添加分组（若已存在则忽略）。 */
  function addShortcutGroup(name: string) {
    const trimmed = name.trim();
    if (trimmed && !shortcutGroups.value.includes(trimmed)) {
      shortcutGroups.value.push(trimmed);
    }
  }

  /** 删除分组（同时清除该组下命令的 group 字段）。 */
  function removeShortcutGroup(name: string) {
    shortcutGroups.value = shortcutGroups.value.filter((g) => g !== name);
    shortcuts.value.forEach((sc) => {
      if (sc.group === name) sc.group = null;
    });
  }

  /** 重命名分组。 */
  function renameShortcutGroup(oldName: string, newName: string) {
    const idx = shortcutGroups.value.indexOf(oldName);
    if (idx !== -1) shortcutGroups.value[idx] = newName;
    shortcuts.value.forEach((sc) => {
      if (sc.group === oldName) sc.group = newName;
    });
  }

  // --- skill CRUD ---

  /** 新增一条 skill（返回新 id）。 */
  function addSkill(skill: Omit<SkillConfig, "id">): string {
    const id = `skill-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    skills.value.push({ ...skill, id });
    return id;
  }

  /** 更新一条 skill。 */
  function updateSkill(id: string, patch: Partial<SkillConfig>) {
    const idx = skills.value.findIndex((s) => s.id === id);
    if (idx !== -1) skills.value[idx] = { ...skills.value[idx], ...patch };
  }

  /** 删除一条 skill。 */
  function removeSkill(id: string) {
    skills.value = skills.value.filter((s) => s.id !== id);
  }

  /** 切换一条 skill 的启用状态。 */
  function toggleSkill(id: string) {
    const s = skills.value.find((x) => x.id === id);
    if (s) s.enabled = !s.enabled;
  }

  return {
    terminal,
    aiProviders,
    aiActive,
    sshAgent,
    sqlAgent,
    fileAccess,
    skills,
    shortcuts,
    shortcutGroups,
    appShortcuts,
    sidebarWidth,
    recentSessionIds,
    loaded,
    load,
    save,
    setTerminal,
    setSidebarWidth,
    recordRecentSession,
    addShortcut,
    updateShortcut,
    removeShortcut,
    addShortcutGroup,
    removeShortcutGroup,
    renameShortcutGroup,
    getAppShortcut,
    setAppShortcut,
    resetAppShortcuts,
    addSkill,
    updateSkill,
    removeSkill,
    toggleSkill,
  };
});
