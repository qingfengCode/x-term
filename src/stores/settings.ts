import { defineStore } from "pinia";
import { ref } from "vue";
import * as configApi from "@/api/config";
import {
  defaultAppShortcuts,
  type AppShortcutAction,
  type AppShortcuts,
  type Settings,
  type ShortcutCommand,
  type SshAgentSettings,
  type SqlAgentSettings,
  type TerminalSettings,
} from "@/api/types";

const defaultSshAgent: SshAgentSettings = {
  commandWhitelist: ["ls", "pwd", "whoami", "date", "uptime", "df", "free", "ps", "cat", "grep"],
  autoApproveSafe: false,
  terminalVisualization: false,
};
const defaultSqlAgent: SqlAgentSettings = {
  sqlMode: "readonly",
  autoApproveSafe: false,
  terminalVisualization: false,
};

const defaultTerminal: TerminalSettings = {
  theme: "dark",
  fontFamily: "Consolas, 'Cascadia Code', 'Courier New', monospace",
  fontSize: 14,
  lineHeight: 1.2,
  scrollback: 10000,
  copyOnSelect: true,
  enableWebgl: true,
};

export const useSettingsStore = defineStore("settings", () => {
  const terminal = ref<TerminalSettings>({ ...defaultTerminal });
  const aiProviders = ref<Settings["ai"]["providers"]>([]);
  const aiActive = ref<string | null>(null);
  /** SSH 智能体配置（exec_ssh）。 */
  const sshAgent = ref<SshAgentSettings>({ ...defaultSshAgent });
  /** SQL 智能体配置（exec_sql）。 */
  const sqlAgent = ref<SqlAgentSettings>({ ...defaultSqlAgent });
  /** 快捷命令列表（终端底部按钮栏 + 快捷键绑定）。 */
  const shortcuts = ref<ShortcutCommand[]>([]);
  /** 快捷命令有序分组名列表。 */
  const shortcutGroups = ref<string[]>([]);
  /** 应用级快捷键绑定（action -> 组合键）。 */
  const appShortcuts = ref<AppShortcuts>(defaultAppShortcuts());
  const loaded = ref(false);

  async function load() {
    const s = await configApi.settingsLoad();
    terminal.value = { ...defaultTerminal, ...s.terminal };
    aiProviders.value = s.ai.providers ?? [];
    aiActive.value = s.ai.active;
    // sshAgent：优先用新字段；缺失时从旧字段迁移（后端已迁移一次，这里兼容旧前端缓存）。
    if (s.ai.sshAgent) {
      sshAgent.value = { ...defaultSshAgent, ...s.ai.sshAgent };
    } else {
      sshAgent.value = {
        commandWhitelist: s.ai.commandWhitelist ?? defaultSshAgent.commandWhitelist,
        autoApproveSafe: s.ai.autoApproveWhitelist ?? false,
        terminalVisualization: s.ai.terminalVisualization ?? false,
      };
    }
    sqlAgent.value = { ...defaultSqlAgent, ...(s.ai.sqlAgent ?? {}) };
    shortcuts.value = s.shortcuts?.commands ?? [];
    shortcutGroups.value = s.shortcuts?.groups ?? [];
    appShortcuts.value = { ...defaultAppShortcuts(), ...(s.shortcuts?.app ?? {}) };
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
      },
      shortcuts: { commands: shortcuts.value, groups: shortcutGroups.value, app: appShortcuts.value },
      firstRun: false,
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

  return {
    terminal,
    aiProviders,
    aiActive,
    sshAgent,
    sqlAgent,
    shortcuts,
    shortcutGroups,
    appShortcuts,
    loaded,
    load,
    save,
    setTerminal,
    addShortcut,
    updateShortcut,
    removeShortcut,
    addShortcutGroup,
    removeShortcutGroup,
    renameShortcutGroup,
    getAppShortcut,
    setAppShortcut,
    resetAppShortcuts,
  };
});
