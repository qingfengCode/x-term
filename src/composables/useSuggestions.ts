/**
 * 终端智能补全建议（历史命令 + 快捷命令 + AI 懒触发）。
 *
 * 三级混合：用户击键后 150ms 防抖聚合建议项——
 * 1. 历史命令：前缀精确匹配优先，再做 fzf 风格子序列模糊匹配（带 matchIndices
 *    供 UI 高亮命中字符）；
 * 2. 快捷命令：来自 settings.shortcuts（终端底部按钮栏的同一份数据）；
 * 3. AI 占位项：只显示"AI 补全…"，**方向键选中才真正调用模型**——避免每次
 *    击键都烧 token（懒触发）。
 *
 * 历史缓存为模块级：多个终端标签页共享同一份（与 shell 历史的全局语义一致），
 * 写入走后端 SQLite（同命令去重、删旧插新），无需前端防抖落盘。
 *
 * 借鉴自 uniTerm 的 useSuggestions（Apache-2.0），存储层换成 x-term 的
 * history_* Tauri 命令。
 */
import { ref } from "vue";
import { historyAdd, historyDelete, historyRecent, type HistoryEntry } from "@/api/history";
import { aiCompleteCommand } from "@/api/ai";
import { useSettingsStore } from "@/stores/settings";

export interface SuggestionItem {
  type: "history" | "quick-command" | "ai-preview" | "ai-result";
  label: string;
  value: string;
  /** 历史条目的数据库 id（删除用）。 */
  id?: number;
  /** label 中命中输入的字符下标（高亮用）。 */
  matchIndices?: number[];
  /** 快捷命令：command 文本中的命中下标（label 与 command 都参与匹配时）。 */
  commandMatchIndices?: number[];
}

export interface SuggestionsState {
  visible: boolean;
  items: SuggestionItem[];
  selectedIndex: number;
  loading: boolean;
}

/** 内存缓存上限（与后端 history_recent 的取用量一致）。 */
const MAX_HISTORY = 500;
/** 单条命令最大长度（超长不参与补全）。 */
const MAX_COMMAND_LENGTH = 200;

// --- 模块级共享状态（跨终端标签页） ----------------------------------------
const historyCache = new Map<string, HistoryEntry>(); // key = command
let historyLoaded = false;
const historyVersion = ref(0); // 触发依赖更新的版本号

export function useSuggestions() {
  const settings = useSettingsStore();
  const state = ref<SuggestionsState>({
    visible: false,
    items: [],
    selectedIndex: -1,
    loading: false,
  });

  // Esc 抑制：用户按 Esc 关闭建议后，直到下一条命令（Enter）前不再弹出。
  let suppressedUntilNextCommand = false;
  let debounceTimer: ReturnType<typeof setTimeout> | null = null;

  /** 最新在前的历史数组（Map 迭代序 = 插入序 = 旧→新，reverse 即新→旧）。 */
  function historyEntries(): HistoryEntry[] {
    void historyVersion.value; // 依赖版本号
    return Array.from(historyCache.values()).reverse();
  }

  async function loadHistory(): Promise<void> {
    if (historyLoaded) return;
    try {
      const entries = await historyRecent(MAX_HISTORY);
      for (const e of entries) historyCache.set(e.command, e);
      historyLoaded = true;
      historyVersion.value++;
    } catch {
      // 后端不可用时静默降级（仅快捷命令 + AI 可用）
    }
  }

  /** 历史净化：敏感/无价值命令不进缓存。
   *
   * 密码、token、AI 哨兵包装、会话控制（exit/clear）、危险命令、超长命令。 */
  function shouldSkipCommand(command: string): boolean {
    const trimmed = command.trim();
    if (!trimmed) return true;
    if (trimmed.length <= 1) return true;
    if (trimmed.length > MAX_COMMAND_LENGTH) return true;
    // AI 可视化执行的哨兵包装命令。
    if (trimmed.includes("__XTERM_DONE_")) return true;
    // shell 注释（如 "# apt update"）。
    if (trimmed.startsWith("#")) return true;
    if (/^(exit|logout|clear|history)$/i.test(trimmed)) return true;
    // 系统控制/危险命令。
    if (/^\s*(reboot|shutdown|halt|poweroff)(\s+.*)?$/i.test(trimmed)) return true;
    if (/^\s*init\s+(0|6)\b/i.test(trimmed)) return true;
    // 潜在敏感信息（密码/token/密钥）。
    if (/\b(password|passwd|token|api[_-]?key|secret|private[_-]?key)\s*[=:]\s*\S+/i.test(trimmed))
      return true;
    if (/\b(-p|--password)\s+\S+/.test(trimmed)) return true;
    if (/Authorization:\s*Bearer\s+\S+/i.test(trimmed)) return true;
    return false;
  }

  /** 记录一条命令：内存去重置顶（LRU）+ 后端入库（失败静默）。 */
  function addHistoryCommand(command: string, getSessionId: () => string) {
    if (shouldSkipCommand(command)) return;
    historyCache.delete(command);
    const entry: HistoryEntry = { id: null, sessionId: "", command, exitCode: null, runAt: "" };
    historyCache.set(command, entry);
    if (historyCache.size > MAX_HISTORY) {
      const firstKey = historyCache.keys().next().value;
      if (firstKey !== undefined) historyCache.delete(firstKey);
    }
    historyVersion.value++;
    // 后端 insert 单行很快，无需防抖；失败不影响内存补全。
    // 返回的自增 id 回填条目——弹窗删除按钮对"刚执行还没重载"的命令同样生效。
    const sid = getSessionId();
    historyAdd(sid, command)
      .then((id) => {
        if (id > 0) entry.id = id;
      })
      .catch(() => {});
  }

  /** 删除一条历史（弹窗删除按钮）：内存 + 后端 + 当前建议列表同步清理。 */
  function removeHistoryCommandById(id: number) {
    let key: string | undefined;
    for (const [cmd, entry] of historyCache) {
      if (entry.id === id) {
        key = cmd;
        break;
      }
    }
    if (key === undefined) return;
    historyCache.delete(key);
    historyVersion.value++;
    historyDelete(id).catch(() => {});
    pruneVisibleItems();
  }

  /** 从当前可见建议中剔除已删除的历史项，并收拢选中下标。 */
  function pruneVisibleItems() {
    state.value.items = state.value.items.filter((item) => {
      if (item.type !== "history") return true;
      return item.id == null || historyCache.has(item.value);
    });
    if (state.value.selectedIndex >= state.value.items.length) {
      state.value.selectedIndex = state.value.items.length - 1;
    }
    if (state.value.items.length === 0) state.value.visible = false;
  }

  /** fzf 风格子序列匹配：返回 command 中命中 prefix 各字符的下标，
   * 长度不等（未完全匹配）时返回空数组。 */
  function getFuzzyMatchIndices(command: string, prefix: string): number[] {
    const cmd = command.toLowerCase();
    const pre = prefix.toLowerCase();
    const indices: number[] = [];
    let cmdIdx = 0;
    let preIdx = 0;
    while (cmdIdx < cmd.length && preIdx < pre.length) {
      if (cmd[cmdIdx] === pre[preIdx]) {
        indices.push(cmdIdx);
        preIdx++;
      }
      cmdIdx++;
    }
    return preIdx === pre.length ? indices : [];
  }

  function getHistorySuggestions(prefix: string): SuggestionItem[] {
    if (!prefix) return [];
    const lowerPrefix = prefix.toLowerCase();
    const matches: SuggestionItem[] = [];
    const entries = historyEntries();

    // 第一轮：前缀精确匹配（优先级高）。
    for (const entry of entries) {
      const cmd = entry.command;
      if (cmd.length > MAX_COMMAND_LENGTH) continue;
      if (cmd.toLowerCase().startsWith(lowerPrefix)) {
        const indices: number[] = [];
        for (let i = 0; i < lowerPrefix.length && i < cmd.length; i++) indices.push(i);
        matches.push({
          type: "history",
          label: cmd,
          value: cmd,
          id: entry.id ?? undefined,
          matchIndices: indices,
        });
      }
    }
    // 第二轮：子序列模糊匹配（Ctrl+R 式）。
    for (const entry of entries) {
      const cmd = entry.command;
      if (cmd.length > MAX_COMMAND_LENGTH) continue;
      if (matches.some((m) => m.value === cmd)) continue;
      const indices = getFuzzyMatchIndices(cmd, lowerPrefix);
      if (indices.length > 0) {
        matches.push({
          type: "history",
          label: cmd,
          value: cmd,
          id: entry.id ?? undefined,
          matchIndices: indices,
        });
      }
    }
    return matches.slice(0, 10);
  }

  function getQuickCommandSuggestions(prefix: string): SuggestionItem[] {
    if (!prefix) return [];
    const lowerPrefix = prefix.toLowerCase();
    const matches: SuggestionItem[] = [];
    for (const cmd of settings.shortcuts) {
      if (!cmd.command) continue;
      // 按命令文本去重：不同 label 但同 command 的快捷命令会产生重复的
      // v-for key（`q-${command}`），导致列表项复用错位。保留首个（列表顺序）。
      if (matches.some((m) => m.value === cmd.command)) continue;
      const label = cmd.label || cmd.command;
      const lowerLabel = label.toLowerCase();
      const lowerCmd = cmd.command.toLowerCase();
      if (lowerLabel.includes(lowerPrefix) || lowerCmd.includes(lowerPrefix)) {
        const lblIdx = lowerLabel.indexOf(lowerPrefix);
        const labelIndices: number[] = [];
        if (lblIdx >= 0) {
          for (let i = 0; i < lowerPrefix.length; i++) labelIndices.push(lblIdx + i);
        }
        const cmdIdx = lowerCmd.indexOf(lowerPrefix);
        const cmdIndices: number[] = [];
        if (cmdIdx >= 0) {
          for (let i = 0; i < lowerPrefix.length; i++) cmdIndices.push(cmdIdx + i);
        }
        matches.push({
          type: "quick-command",
          label,
          value: cmd.command,
          matchIndices: labelIndices.length > 0 ? labelIndices : undefined,
          commandMatchIndices: cmdIndices.length > 0 ? cmdIndices : undefined,
        });
      }
    }
    return matches.slice(0, 10);
  }

  /** 调用模型生成 AI 建议（用户选中占位项时触发）。 */
  async function generateAISuggestion(currentInput: string): Promise<void> {
    if (!currentInput.trim() || state.value.loading) return;
    state.value.items = state.value.items.filter((i) => i.type !== "ai-preview");
    state.value.items.push({ type: "ai-preview", label: "AI 生成中…", value: "" });
    state.value.loading = true;
    try {
      const result = await aiCompleteCommand(currentInput);
      const cleaned = result
        .trim()
        .replace(/^```[\w]*\n?/, "")
        .replace(/\n?```$/, "")
        .trim();
      if (cleaned) {
        state.value.items = state.value.items.filter((i) => i.type !== "ai-preview");
        state.value.items.push({ type: "ai-result", label: cleaned, value: cleaned });
        state.value.selectedIndex = state.value.items.length - 1;
      } else {
        removeAiPlaceholder();
      }
    } catch {
      removeAiPlaceholder();
    } finally {
      state.value.loading = false;
    }
  }

  function removeAiPlaceholder() {
    state.value.items = state.value.items.filter(
      (i) => i.type !== "ai-preview" && i.type !== "ai-result",
    );
    if (state.value.selectedIndex >= state.value.items.length) {
      state.value.selectedIndex = state.value.items.length - 1;
    }
    if (state.value.items.length === 0) state.value.visible = false;
  }

  /** 击键后更新建议（150ms 防抖）。aiEnabled = 设置开关 && AI 已配置。 */
  function updateSuggestions(token: string, aiEnabled: boolean) {
    if (debounceTimer) clearTimeout(debounceTimer);
    if (!token || suppressedUntilNextCommand) {
      state.value.visible = false;
      state.value.items = [];
      return;
    }
    debounceTimer = setTimeout(() => {
      debounceTimer = null;
      if (state.value.loading) return;
      const items = [
        ...getQuickCommandSuggestions(token),
        ...getHistorySuggestions(token),
      ];
      if (aiEnabled) {
        items.push({ type: "ai-preview", label: "AI 补全（选中触发）", value: "" });
      }
      state.value.items = items;
      state.value.selectedIndex = -1;
      state.value.visible = items.length > 0;
    }, 150);
  }

  function selectNext() {
    if (state.value.items.length === 0) return;
    // 取消待执行的防抖刷新：防止迟到的刷新重置用户已导航的 selectedIndex。
    if (debounceTimer) {
      clearTimeout(debounceTimer);
      debounceTimer = null;
    }
    if (state.value.selectedIndex < 0) state.value.selectedIndex = 0;
    else state.value.selectedIndex = (state.value.selectedIndex + 1) % state.value.items.length;
  }

  function selectPrev() {
    if (state.value.items.length === 0) return;
    if (debounceTimer) {
      clearTimeout(debounceTimer);
      debounceTimer = null;
    }
    if (state.value.selectedIndex < 0) state.value.selectedIndex = state.value.items.length - 1;
    else
      state.value.selectedIndex =
        (state.value.selectedIndex - 1 + state.value.items.length) % state.value.items.length;
  }

  function getSelectedItem(): SuggestionItem | null {
    if (state.value.items.length === 0 || state.value.selectedIndex < 0) return null;
    return state.value.items[state.value.selectedIndex];
  }

  function close() {
    if (debounceTimer) {
      clearTimeout(debounceTimer);
      debounceTimer = null;
    }
    state.value.visible = false;
    state.value.items = [];
    state.value.selectedIndex = -1;
  }

  /** Esc 关闭并抑制，直到下一条命令（Enter）才恢复。 */
  function suppress() {
    suppressedUntilNextCommand = true;
    close();
  }

  function resetSuppress() {
    suppressedUntilNextCommand = false;
  }

  function isVisible(): boolean {
    return state.value.visible;
  }

  return {
    state,
    loadHistory,
    addHistoryCommand,
    removeHistoryCommandById,
    updateSuggestions,
    generateAISuggestion,
    selectNext,
    selectPrev,
    getSelectedItem,
    close,
    suppress,
    resetSuppress,
    isVisible,
  };
}

export type Suggestions = ReturnType<typeof useSuggestions>;
