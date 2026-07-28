// 快捷键组合字符串的构造与匹配工具。
//
// 组合格式："Ctrl+Shift+V" / "Ctrl+Tab" / "F1" / "Ctrl+1"。
// 修饰键顺序固定为 Ctrl → Alt → Shift → Meta，主键在后；主键字母大写。
// 这与 settings 中存储的字符串、KeyboardEvent 解析出的字符串保持一致。

/** 修饰键的固定展示顺序。 */
const MOD_ORDER = ["Ctrl", "Alt", "Shift", "Meta"] as const;

/** 把一个 KeyboardEvent 规范化为组合字符串（如 "Ctrl+Shift+V"）。 */
export function eventToCombo(e: KeyboardEvent): string {
  const parts: string[] = [];
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  if (e.metaKey) parts.push("Meta");

  // 忽略纯修饰键按下（让用户继续按主键）。
  const key = e.key;
  if (
    key === "Control" ||
    key === "Alt" ||
    key === "Shift" ||
    key === "Meta" ||
    key === "Dead"
  ) {
    return parts.join("+"); // 仅修饰键，调用方可据此显示"按下一个键…"
  }

  // 主键归一化。
  let main: string;
  if (key === " ") main = "Space";
  else if (key === "ArrowUp") main = "Up";
  else if (key === "ArrowDown") main = "Down";
  else if (key === "ArrowLeft") main = "Left";
  else if (key === "ArrowRight") main = "Right";
  else if (key === "Escape") main = "Esc";
  else if (key === "Delete") main = "Delete";
  else if (key === "Backspace") main = "Backspace";
  else if (key === "Enter") main = "Enter";
  else if (key === "Tab") main = "Tab";
  else if (key.length === 1) main = key.toUpperCase();
  else main = key;

  parts.push(main);
  return parts.join("+");
}

/** 判断 combo 是否为"纯修饰键"（用户还在按主键的过程中）。 */
export function isModifierOnly(combo: string): boolean {
  if (!combo) return true;
  const parts = combo.split("+");
  return parts.every((p) => (MOD_ORDER as readonly string[]).includes(p));
}

/** 在 keydown 事件上判断是否匹配某个组合字符串。 */
export function matchesCombo(e: KeyboardEvent, combo: string): boolean {
  if (!combo) return false;
  return eventToCombo(e) === combo;
}
