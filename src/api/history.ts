import { invoke } from "@tauri-apps/api/core";

/** 一条命令历史记录（与后端 storage::history_repo::HistoryEntry 对应）。 */
export interface HistoryEntry {
  id: number | null;
  sessionId: string;
  command: string;
  exitCode: number | null;
  runAt: string;
}

/** 新增一条命令历史（后端同命令去重，只保留最新一次执行）。 */
export function historyAdd(sessionId: string, command: string): Promise<number> {
  return invoke<number>("history_add", { sessionId, command });
}

/** 全局最近历史（按命令去重，新→旧），终端补全建议的数据源。 */
export function historyRecent(limit: number): Promise<HistoryEntry[]> {
  return invoke<HistoryEntry[]>("history_recent", { limit });
}

/** 删除一条历史（补全弹窗的删除按钮）。 */
export function historyDelete(id: number): Promise<void> {
  return invoke<void>("history_delete", { id });
}

/** 关键字模糊搜索历史（所有会话范围）。 */
export function historySearch(keyword: string, limit: number): Promise<HistoryEntry[]> {
  return invoke<HistoryEntry[]>("history_search", { keyword, limit });
}
