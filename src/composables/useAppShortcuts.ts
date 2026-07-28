// 应用级快捷键的全局分发器。
//
// 在 MainLayout 挂载时调用 useAppShortcuts()，它会注册一个 window keydown 监听，
// 根据当前 settings.appShortcuts 把组合键映射到对应动作（newTab/closeTab/...），
// 并通过传入的 handlers 字典执行。
//
// 与各组件原有的"自定义命令快捷键"（settings.shortcuts[].shortcut）互不干扰：
// 本分发器只处理 APP_SHORTCUT_METAS 中定义的应用动作。

import { onBeforeUnmount, onMounted } from "vue";
import { useSettingsStore } from "@/stores/settings";
import type { AppShortcutAction } from "@/api/types";
import { matchesCombo } from "@/utils/shortcut";

export interface AppShortcutHandlers {
  newTab?: () => void;
  closeTab?: () => void;
  nextTab?: () => void;
  prevTab?: () => void;
  copy?: () => void;
  paste?: () => void;
  toggleAi?: () => void;
  search?: () => void;
  focusSessions?: () => void;
}

/** 是否在可编辑元素中（输入框/textarea/CodeMirror 等），此类元素中一般不触发应用快捷键。 */
function isEditableTarget(e: KeyboardEvent): boolean {
  const t = e.target as HTMLElement | null;
  if (!t) return false;
  const tag = t.tagName;
  if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return true;
  // CodeMirror / contenteditable。
  if (t.isContentEditable) return true;
  if (t.closest && t.closest(".CodeMirror, .cm-editor")) return true;
  return false;
}

/**
 * 注册全局应用快捷键监听。
 * @param handlers 各动作的处理函数；缺失的动作不会被触发。
 * @param options.editablePassthrough 在可编辑元素中是否放行（默认 true，即不拦截）。
 *        注意：少数动作（如 toggleAi、focusSessions）即使在输入框中也希望生效，
 *        由各 handler 自行判断；这里统一放行可编辑元素以避免误吞输入。
 */
export function useAppShortcuts(
  handlers: AppShortcutHandlers,
  options: { editablePassthrough?: boolean } = {}
) {
  const settings = useSettingsStore();
  const editablePassthrough = options.editablePassthrough ?? true;

  function onKeydown(e: KeyboardEvent) {
    // 可编辑元素中默认放行（避免误吞 SQL/会话名输入）。
    if (editablePassthrough && isEditableTarget(e)) return;

    const map = settings.appShortcuts as Partial<Record<AppShortcutAction, string>>;
    for (const action of Object.keys(map) as AppShortcutAction[]) {
      const combo = map[action];
      if (!combo) continue;
      if (matchesCombo(e, combo)) {
        const fn = handlers[action];
        if (fn) {
          e.preventDefault();
          e.stopPropagation();
          fn();
          return;
        }
      }
    }
  }

  onMounted(() => window.addEventListener("keydown", onKeydown, true));
  onBeforeUnmount(() => window.removeEventListener("keydown", onKeydown, true));
}
