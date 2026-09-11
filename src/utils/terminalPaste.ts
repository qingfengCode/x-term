/**
 * 安全粘贴的纯函数层（可单测，不依赖组件）。
 *
 * 三件事：
 * 1. 换行规范化：Windows 剪贴板的 `\r\n` → `\n`，孤立 `\r` 丢弃——否则
 *    在 vim/远端 shell 里粘贴会出现双换行。
 * 2. 括号粘贴包裹：仅当**目标会话已探测的** bracketedPasteMode 为真时把
 *    文本包上 `ESC[200~ ... ESC[201~`。该状态由 xterm 解析远端 shell 发来
 *    的 `ESC[?2004h/l` 维护（term.modes.bracketedPasteMode），读真实状态
 *    而非假设开启——vim/less 里包裹可防止逐行自动缩进，而未开启 2004 的
 *    古老 shell 收到包裹序列反而会输出乱码。
 * 3. 危险检测：多行 / 含破坏性命令的粘贴先经用户确认再写入——防止剪贴板
 *    里整段脚本被一次回车全部执行。
 *
 * 借鉴 uniTerm terminalPaste.ts（Apache-2.0），危险检测为 x-term 增强。
 */

/** 换行规范化：`\r\n` → `\n`，剩余孤立 `\r` 丢弃。 */
export function normalizePastedText(text: string): string {
  return text.replace(/\r\n/g, "\n").replace(/\r/g, "");
}

/** 目标会话开启 bracketed paste 时包裹标记序列，否则原样返回。 */
export function wrapBracketedPaste(text: string, enabled: boolean): string {
  return enabled ? `\x1b[200~${text}\x1b[201~` : text;
}

/** 粘贴内容的危险关键词（前缀匹配，覆盖常见破坏性命令）。 */
const DANGEROUS_PATTERNS: RegExp[] = [
  /\brm\s+(-[a-z]*r[a-z]*f?|[a-z]*f[a-z]*r)/i, // rm -rf / rm -fr ...
  /\bmkfs\b/i,
  /\bdd\s+.*of=/i, // dd of=/dev/...
  /\b(shutdown|reboot|halt|poweroff|init\s+[06])\b/i,
  /:\(\)\s*\{\s*:\|\:&\s*\}\s*;?\s*:/, // fork bomb :(){ :|:& };:
  /\bchmod\s+-[a-z]*R[a-z]*\s+777\s+\//i,
  /\b>\s*\/dev\/sd[a-z]/i,
  /\bdrop\s+(table|database)\b/i, // SQL 混入
];

/** 粘贴是否需要确认：多行 或 含危险命令。 */
export function needsPasteConfirm(text: string): boolean {
  const normalized = normalizePastedText(text);
  const lines = normalized.split("\n").filter((l) => l.trim());
  if (lines.length > 1) return true;
  return DANGEROUS_PATTERNS.some((re) => re.test(normalized));
}

/** 生成确认弹窗的消息体：行数 + 内容预览（首行截断，多行显示总行数）。 */
export function pasteConfirmMessage(text: string): string {
  const normalized = normalizePastedText(text);
  const lines = normalized.split("\n").filter((l) => l.trim());
  const multi = lines.length > 1;
  const preview = (lines[0] ?? "").trim();
  const shown =
    preview.length > 60 ? `${preview.slice(0, 60)}…` : preview || "(空行)";
  if (multi) {
    return `将粘贴 ${lines.length} 行内容，可能一次执行多条命令。\n首行：${shown}`;
  }
  return `粘贴内容包含高危命令，确认执行？\n${shown}`;
}
