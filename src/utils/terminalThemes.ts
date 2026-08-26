/**
 * terminalThemes.ts — 终端 ANSI 16 色配色方案预设。
 *
 * xterm.js 的 theme 需要显式提供 ANSI 16 色，缺失时全部回退 foreground，
 * 导致 ls / vim / 语法高亮等彩色输出显示为纯色。
 * 这里按名称内置多套流行配色（深浅各半），供设置页选择。
 */
import type { ITheme } from "@xterm/xterm";

export interface TerminalColorScheme {
  /** 唯一 id（持久化值）。 */
  id: string;
  /** 显示名。 */
  label: string;
  /** 所属明暗：跟随应用主题时的默认值选取依据。 */
  scheme: "dark" | "light";
  /** xterm 完整主题（含 ANSI 16 色）。 */
  theme: ITheme;
}

/** Catppuccin Mocha（与旧版纯色主题背景一致，平滑升级）。 */
const catppuccinMocha: ITheme = {
  background: "#1e1e2e",
  foreground: "#cdd6f4",
  cursor: "#f5e0dc",
  cursorAccent: "#1e1e2e",
  selectionBackground: "#585b7088",
  black: "#45475a",
  red: "#f38ba8",
  green: "#a6e3a1",
  yellow: "#f9e2af",
  blue: "#89b4fa",
  magenta: "#f5c2e7",
  cyan: "#94e2d5",
  white: "#bac2de",
  brightBlack: "#585b70",
  brightRed: "#f38ba8",
  brightGreen: "#a6e3a1",
  brightYellow: "#f9e2af",
  brightBlue: "#89b4fa",
  brightMagenta: "#f5c2e7",
  brightCyan: "#94e2d5",
  brightWhite: "#a6adc8",
};

const oneDark: ITheme = {
  background: "#282c34",
  foreground: "#abb2bf",
  cursor: "#61afef",
  cursorAccent: "#282c34",
  selectionBackground: "#3e4451cc",
  black: "#3f4451",
  red: "#e05c61",
  green: "#8cc265",
  yellow: "#d18f52",
  blue: "#4aa5f0",
  magenta: "#c162de",
  cyan: "#42b3c2",
  white: "#d7dae0",
  brightBlack: "#4f5666",
  brightRed: "#ff616e",
  brightGreen: "#a5e075",
  brightYellow: "#f0a45d",
  brightBlue: "#4dc4ff",
  brightMagenta: "#de73ff",
  brightCyan: "#4cd1e0",
  brightWhite: "#e6e6e6",
};

const dracula: ITheme = {
  background: "#282a36",
  foreground: "#f8f8f2",
  cursor: "#bf9fff",
  cursorAccent: "#282a36",
  selectionBackground: "#44475acc",
  black: "#21222c",
  red: "#ff5555",
  green: "#50fa7b",
  yellow: "#f1fa8c",
  blue: "#bd93f9",
  magenta: "#ff79c6",
  cyan: "#8be9fd",
  white: "#f8f8f2",
  brightBlack: "#6272a4",
  brightRed: "#ff6e6e",
  brightGreen: "#69ff94",
  brightYellow: "#ffffa5",
  brightBlue: "#d6acff",
  brightMagenta: "#ff92df",
  brightCyan: "#a4ffff",
  brightWhite: "#ffffff",
};

const nord: ITheme = {
  background: "#2e3440",
  foreground: "#d8dee9",
  cursor: "#d8dee9",
  cursorAccent: "#2e3440",
  selectionBackground: "#434c5ecc",
  black: "#3b4252",
  red: "#bf616a",
  green: "#a3be8c",
  yellow: "#ebcb8b",
  blue: "#81a1c1",
  magenta: "#b48ead",
  cyan: "#88c0d0",
  white: "#e5e9f0",
  brightBlack: "#4c566a",
  brightRed: "#bf616a",
  brightGreen: "#a3be8c",
  brightYellow: "#ebcb8b",
  brightBlue: "#81a1c1",
  brightMagenta: "#b48ead",
  brightCyan: "#8fbcbb",
  brightWhite: "#eceff4",
};

const tokyoNight: ITheme = {
  background: "#1a1b26",
  foreground: "#a9b1d6",
  cursor: "#c0caf5",
  cursorAccent: "#1a1b26",
  selectionBackground: "#33467ccc",
  black: "#32344a",
  red: "#f7768e",
  green: "#9ece6a",
  yellow: "#e0af68",
  blue: "#7aa2f7",
  magenta: "#ad8ee6",
  cyan: "#7dcfff",
  white: "#787c99",
  brightBlack: "#444b6a",
  brightRed: "#ff7a93",
  brightGreen: "#b9f27c",
  brightYellow: "#ff9e64",
  brightBlue: "#7da6ff",
  brightMagenta: "#bb9af7",
  brightCyan: "#0db9d7",
  brightWhite: "#acb0d0",
};

const gruvboxDark: ITheme = {
  background: "#282828",
  foreground: "#ebdbb2",
  cursor: "#ebdbb2",
  cursorAccent: "#282828",
  selectionBackground: "#504945cc",
  black: "#282828",
  red: "#cc241d",
  green: "#98971a",
  yellow: "#d79921",
  blue: "#458588",
  magenta: "#b16286",
  cyan: "#689d6a",
  white: "#a89984",
  brightBlack: "#928374",
  brightRed: "#fb4934",
  brightGreen: "#b8bb26",
  brightYellow: "#fabd2f",
  brightBlue: "#83a598",
  brightMagenta: "#d3869b",
  brightCyan: "#8ec07c",
  brightWhite: "#ebdbb2",
};

/** 浅色：Catppuccin Latte。 */
const catppuccinLatte: ITheme = {
  background: "#eff1f5",
  foreground: "#4c4f69",
  cursor: "#dc8a78",
  cursorAccent: "#eff1f5",
  selectionBackground: "#acb0be88",
  black: "#5c5f77",
  red: "#d20f39",
  green: "#40a02b",
  yellow: "#df8e1d",
  blue: "#1e66f5",
  magenta: "#8839ef",
  cyan: "#179299",
  white: "#acb0be",
  brightBlack: "#6c6f85",
  brightRed: "#d20f39",
  brightGreen: "#40a02b",
  brightYellow: "#df8e1d",
  brightBlue: "#1e66f5",
  brightMagenta: "#8839ef",
  brightCyan: "#179299",
  brightWhite: "#bcc0cc",
};

const solarizedLight: ITheme = {
  background: "#fdf6e3",
  foreground: "#657b83",
  cursor: "#586e75",
  cursorAccent: "#fdf6e3",
  selectionBackground: "#eee8d588",
  black: "#073642",
  red: "#dc322f",
  green: "#859900",
  yellow: "#b58900",
  blue: "#268bd2",
  magenta: "#d33682",
  cyan: "#2aa198",
  white: "#eee8d5",
  brightBlack: "#002b36",
  brightRed: "#cb4b16",
  brightGreen: "#586e75",
  brightYellow: "#657b83",
  brightBlue: "#839496",
  brightMagenta: "#6c71c4",
  brightCyan: "#93a1a1",
  brightWhite: "#fdf6e3",
};

const oneLight: ITheme = {
  background: "#fafafa",
  foreground: "#383a42",
  cursor: "#526fff",
  cursorAccent: "#fafafa",
  selectionBackground: "#e5e5e6aa",
  black: "#383a42",
  red: "#e45649",
  green: "#50a14f",
  yellow: "#c18401",
  blue: "#4078f2",
  magenta: "#a626a4",
  cyan: "#0184bc",
  white: "#a0a1a7",
  brightBlack: "#4f525e",
  brightRed: "#e06c75",
  brightGreen: "#98c379",
  brightYellow: "#e5c07b",
  brightBlue: "#61afef",
  brightMagenta: "#c678dd",
  brightCyan: "#56b6c2",
  brightWhite: "#ffffff",
};

/** 全部预设（顺序即设置页下拉顺序）。 */
export const TERMINAL_COLOR_SCHEMES: TerminalColorScheme[] = [
  { id: "catppuccin-mocha", label: "Catppuccin Mocha", scheme: "dark", theme: catppuccinMocha },
  { id: "one-dark", label: "One Dark", scheme: "dark", theme: oneDark },
  { id: "dracula", label: "Dracula", scheme: "dark", theme: dracula },
  { id: "nord", label: "Nord", scheme: "dark", theme: nord },
  { id: "tokyo-night", label: "Tokyo Night", scheme: "dark", theme: tokyoNight },
  { id: "gruvbox-dark", label: "Gruvbox Dark", scheme: "dark", theme: gruvboxDark },
  { id: "catppuccin-latte", label: "Catppuccin Latte", scheme: "light", theme: catppuccinLatte },
  { id: "one-light", label: "One Light", scheme: "light", theme: oneLight },
  { id: "solarized-light", label: "Solarized Light", scheme: "light", theme: solarizedLight },
];

/** 旧配置默认值：深色 → Catppuccin Mocha，浅色 → Catppuccin Latte。 */
export function defaultSchemeFor(appTheme: string): string {
  return appTheme === "dark" ? "catppuccin-mocha" : "catppuccin-latte";
}

/** 按 id 取方案；未配置或 id 无效时按应用明暗回退默认方案。 */
export function resolveScheme(id: string | undefined, appTheme: string): TerminalColorScheme {
  const byId = TERMINAL_COLOR_SCHEMES.find((s) => s.id === id);
  if (byId) return byId;
  const fallback = defaultSchemeFor(appTheme);
  return TERMINAL_COLOR_SCHEMES.find((s) => s.id === fallback) ?? TERMINAL_COLOR_SCHEMES[0];
}
