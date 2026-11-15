/**
 * 右键菜单键盘导航。
 *
 * 约定：
 * - 菜单容器：role="menu"、tabindex="-1"、ref 传入本 composable、@keydown="onKeydown"；
 * - 菜单项：role="menuitem"、tabindex="-1"、@keydown="onItemKeydown"。
 *
 * 打开菜单后调用 focusFirst()，↑/↓ 循环聚焦，Home/End 跳转，Enter/空格触发点击，
 * Esc 关闭。鼠标操作路径保持不变。
 */
import { nextTick, type Ref } from "vue";

export function useMenuKeyboard(menuRef: Ref<HTMLElement | null>, close: () => void) {
  function items(): HTMLElement[] {
    const root = menuRef.value;
    if (!root) return [];
    return Array.from(
      root.querySelectorAll<HTMLElement>('[role="menuitem"]:not([disabled]):not([aria-disabled="true"])'),
    );
  }

  async function focusFirst() {
    await nextTick();
    items()[0]?.focus();
  }

  function onKeydown(e: KeyboardEvent) {
    const list = items();
    if (list.length === 0) return;
    const current = document.activeElement as HTMLElement | null;
    const idx = current ? list.indexOf(current) : -1;

    if (e.key === "ArrowDown") {
      e.preventDefault();
      list[(idx + 1 + list.length) % list.length]?.focus();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      // 无聚焦项时（idx === -1）从最后一项开始，而不是倒数第二项
      list[idx <= 0 ? list.length - 1 : idx - 1]?.focus();
    } else if (e.key === "Home") {
      e.preventDefault();
      list[0]?.focus();
    } else if (e.key === "End") {
      e.preventDefault();
      list[list.length - 1]?.focus();
    } else if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  }

  function onItemKeydown(e: KeyboardEvent) {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      (e.currentTarget as HTMLElement).click();
    }
  }

  return { focusFirst, onKeydown, onItemKeydown };
}
