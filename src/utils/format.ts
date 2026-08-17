/**
 * 通用格式化工具。
 */

/**
 * 字节数 → 人类可读大小（如 "1.5 MB"）。
 *
 * 此前在 TerminalPane / FileExplorerView / SftpView / TransferQueue 各有一份
 * 几乎相同的实现，统一收进 utils。
 */
export function formatSize(n: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}
