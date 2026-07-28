/**
 * SQL 命令行控制台核心逻辑（与视图解耦）。
 *
 * 封装 mysql CLI 风格的执行调度、输出累积、事件路由、查询历史。
 * SqlConsoleView 只负责布局/样式，逻辑全在这里。
 *
 * 设计要点：
 * - 输出条目（ConsoleEntry）按执行顺序累积，支持 sql/table/ok/error/info 五种。
 * - execute 推入一条 running 的 sql entry；后端 emit 的 db:query_result 事件
 *   先于 invoke resolve 到达 onQueryResult，由它把 running entry 替换为最终结果。
 *   因此 pendingEntryId 必须在 await 之前设置（见 execute）。
 * - 查询历史持久化到 localStorage，最多 100 条。
 */

import { nextTick, ref, type Ref } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { dbExecSql, dbDescribeTable } from "@/api/db";
import type { DbQueryResultEvent, QueryResult } from "@/api/types";

// ---------------------------------------------------------------------------
// 类型
// ---------------------------------------------------------------------------

export type ConsoleEntry =
  | { id: string; kind: "sql"; sql: string; status: "running" | "done" | "error" }
  | { id: string; kind: "table"; sql: string; columns: string[]; rows: string[][]; elapsedMs: number }
  | { id: string; kind: "ok"; sql: string; affected: number; elapsedMs: number }
  | { id: string; kind: "error"; sql: string; message: string }
  | { id: string; kind: "info"; text: string };

export interface HistoryItem {
  sql: string;
  ts: number;
  elapsedMs?: number;
}

/** 完整结果（含执行元信息，供代码模式/AI 读取）。 */
export interface FullResult {
  columns: string[];
  rows: string[][];
  affected: number;
  elapsedMs: number;
  error: string | null;
}

// 只读/危险关键字（execute 用）——与视图解耦后仍需这些判定。
const WRITE_KEYWORDS = new Set([
  "INSERT",
  "UPDATE",
  "DELETE",
  "DROP",
  "TRUNCATE",
  "ALTER",
  "CREATE",
  "GRANT",
  "REVOKE",
]);
const DANGEROUS_KEYWORDS = new Set(["DROP", "TRUNCATE"]);

function firstKeyword(sql: string): string {
  return (sql.trimStart().split(/\s+/)[0] || "").toUpperCase();
}
function isDeleteWithoutWhere(sql: string): boolean {
  return /^\s*DELETE\s+FROM\s+\S+\s*(;|$)/i.test(sql);
}

// ---------------------------------------------------------------------------
// composable
// ---------------------------------------------------------------------------

/**
 * @param connIdRef 当前 DB 连接 id 的 ref（只读，视图侧传入）
 * @param sqlTextRef SQL 文本的双向绑定 ref（与 CodeMirror 同步）
 * @param confirmDangerous 危险 SQL 确认回调（返回 true 表示用户确认执行）
 */
export function useSqlConsole(
  connIdRef: Ref<string | null>,
  sqlTextRef: Ref<string>,
  confirmDangerous: (kw: string, noWhere: boolean) => Promise<boolean>,
) {
  // --- 输出累积 ---
  const entries = ref<ConsoleEntry[]>([]);
  /** 滚动容器 ref（视图侧绑定，供自动滚到底部用）。 */
  const scrollRef = ref<HTMLElement | null>(null);
  const executing = ref(false);
  /** 最近一次完整结果（供代码模式/AI 等读取）。 */
  const lastResult = ref<FullResult | null>(null);

  /**
   * 滚动逻辑（mysql CLI 风格）：
   * - 内容未超出容器高度 → 不滚动（scrollTop 保持 0，内容从顶部自然排列，输入框紧跟输出末尾）
   * - 内容超出容器高度 → 滚到让输入框可见（贴近视口底部）
   *
   * 实现用 scrollIntoView({ block: 'end' })：浏览器自动判断溢出与否——
   * 不溢出时不产生滚动，溢出时滚到刚好让锚点元素（输入框）底部对齐视口底部。
   */
  const bottomAnchorRef = ref<HTMLElement | null>(null);
  function scrollToBottom() {
    nextTick(() => {
      const anchor = bottomAnchorRef.value;
      if (anchor) {
        // block:'end' 让锚点底部对齐视口底部；inline:'nearest' 避免水平滚动。
        anchor.scrollIntoView({ block: "end", inline: "nearest", behavior: "auto" });
      } else {
        // 兜底：无锚点时按溢出判断。
        const el = scrollRef.value;
        if (el && el.scrollHeight > el.clientHeight) {
          el.scrollTop = el.scrollHeight;
        }
      }
    });
  }
  function pushEntry(e: ConsoleEntry) {
    entries.value.push(e);
    scrollToBottom();
  }
  function clear() {
    entries.value = [];
    lastResult.value = null;
  }

  // --- 执行调度 + 事件路由 ---
  const pendingQueryId = ref<string | null>(null);
  const pendingEntryId = ref<string | null>(null);
  let unlistenFn: UnlistenFn | null = null;

  function genId(prefix: string) {
    return `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
  }

  /**
   * 执行 SQL。把 SQL 推入输出流（running 态），发 invoke 给后端，
   * 结果由 db:query_result 事件回填（onQueryResult 替换 running entry）。
   *
   * @param readOnly 只读模式则拒绝写操作
   * @param sqlOverride 可选：直接传入要执行的 SQL（命令行模式用）；
   *                    不传则从 sqlTextRef 读（代码模式用 CodeMirror 内容）。
   * @returns 是否真正发起了执行（false 表示被只读/危险确认拦截）
   */
  async function execute(readOnly: boolean, sqlOverride?: string): Promise<boolean> {
    if (!connIdRef.value) return false;
    const sqlRaw = (sqlOverride ?? sqlTextRef.value).trim();
    if (!sqlRaw) return false;
    // 编辑器可能含多条语句（分号分隔）；只执行最后一条非空语句，避免多语句语法错误。
    const stmts = sqlRaw.split(";").map((s) => s.trim()).filter((s) => s.length > 0);
    const sql = stmts.length > 0 ? stmts[stmts.length - 1] : sqlRaw;
    if (executing.value) return false;

    const kw = firstKeyword(sql);
    // 只读模式保护。
    if (readOnly && WRITE_KEYWORDS.has(kw)) return false;
    // 危险 SQL 二次确认。
    const dangerous = DANGEROUS_KEYWORDS.has(kw) || isDeleteWithoutWhere(sql);
    if (dangerous) {
      const ok = await confirmDangerous(kw, isDeleteWithoutWhere(sql));
      if (!ok) return false;
    }

    const queryId = genId("q");
    const entryId = genId("e");
    pendingQueryId.value = queryId;
    executing.value = true;

    // 推入 running entry。pendingEntryId 必须在 await 之前设置——
    // 后端 emit 的事件会在 invoke resolve 之前到达 onQueryResult。
    pushEntry({ id: entryId, kind: "sql", sql, status: "running" });
    pendingEntryId.value = entryId;
    lastExecutedSql.value = sql;

    try {
      await dbExecSql(connIdRef.value, sql, queryId);
      // 结果已在 onQueryResult 里回填（事件先于 invoke resolve 到达）。
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      replaceEntry(entryId, { id: entryId, kind: "error", sql, message: msg });
      executing.value = false;
      pendingQueryId.value = null;
      pendingEntryId.value = null;
    }
    return true;
  }

  function replaceEntry(id: string, e: ConsoleEntry) {
    const idx = entries.value.findIndex((x) => x.id === id);
    if (idx >= 0) entries.value[idx] = e;
    scrollToBottom();
  }

  /** 在指定 entry 之后插入新条目（mysql CLI：结果插在 SQL 行之后）。 */
  function insertAfter(id: string, e: ConsoleEntry) {
    const idx = entries.value.findIndex((x) => x.id === id);
    if (idx >= 0) {
      entries.value.splice(idx + 1, 0, e);
    } else {
      entries.value.push(e);
    }
    scrollToBottom();
  }

  /** db:query_result 事件处理：把 running entry 替换为最终结果。 */
  function onQueryResult(payload: DbQueryResultEvent) {
    if (payload.queryId !== pendingQueryId.value) return;
    executing.value = false;
    pendingQueryId.value = null;

    lastResult.value = {
      columns: payload.columns ?? [],
      rows: payload.rows ?? [],
      affected: payload.affected ?? 0,
      elapsedMs: payload.elapsedMs ?? 0,
      error: payload.error,
    };

    const entryId = pendingEntryId.value;
    pendingEntryId.value = null;
    const sql = lastExecutedSql.value;
    if (entryId) {
      // mysql CLI 风格：SQL 行（mysql> ...）永远保留，结果作为独立条目插在它后面。
      // 1. 把 running 态的 sql entry 标记为 done（不再转圈），但保留显示。
      replaceEntry(entryId, { id: entryId, kind: "sql", sql, status: "done" });
      // 2. 在 sql entry 之后插入结果条目。
      if (payload.error) {
        insertAfter(entryId, { id: genId("e"), kind: "error", sql, message: payload.error });
      } else if ((payload.columns ?? []).length > 0) {
        insertAfter(entryId, {
          id: genId("e"),
          kind: "table",
          sql,
          columns: payload.columns ?? [],
          rows: payload.rows ?? [],
          elapsedMs: payload.elapsedMs ?? 0,
        });
      } else {
        insertAfter(entryId, {
          id: genId("e"),
          kind: "ok",
          sql,
          affected: payload.affected ?? 0,
          elapsedMs: payload.elapsedMs ?? 0,
        });
      }
    }
    // 记录历史（成功才记）。
    if (!payload.error && lastExecutedSql.value) {
      pushHistory(lastExecutedSql.value, payload.elapsedMs);
      lastExecutedSql.value = "";
    }
    scrollToBottom();
  }

  /** 订阅 db:query_result 事件（视图在 onMounted 调用，onBeforeUnmount 调 destroy）。 */
  async function setup() {
    unlistenFn = await listen<DbQueryResultEvent>("db:query_result", (e) => {
      onQueryResult(e.payload);
    });
  }
  function destroy() {
    if (unlistenFn) {
      unlistenFn();
      unlistenFn = null;
    }
  }

  // --- 命令行输入 keydown 兜底（Enter 执行，Shift+Enter 换行） ---
  function onInputKeydown(e: KeyboardEvent, readOnly: boolean) {
    if (e.key !== "Enter") return;
    if (e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return;
    e.preventDefault();
    void execute(readOnly);
  }

  // --- 查询历史（localStorage 持久化） ---
  const HISTORY_KEY = "xterm.sql.history";
  const HISTORY_MAX = 100;
  const history = ref<HistoryItem[]>([]);
  const lastExecutedSql = ref("");

  function loadHistory() {
    try {
      const raw = localStorage.getItem(HISTORY_KEY);
      if (raw) history.value = JSON.parse(raw) as HistoryItem[];
    } catch {
      /* ignore */
    }
  }
  function pushHistory(sql: string, elapsedMs?: number) {
    const trimmed = sql.trim();
    if (!trimmed) return;
    history.value = history.value.filter((h) => h.sql !== trimmed);
    history.value.unshift({ sql: trimmed, ts: Date.now(), elapsedMs });
    if (history.value.length > HISTORY_MAX) history.value.length = HISTORY_MAX;
    try {
      localStorage.setItem(HISTORY_KEY, JSON.stringify(history.value));
    } catch {
      /* ignore */
    }
  }
  /** 用历史条目填充编辑器（视图侧传入 CodeMirror 操作或直接改 sqlText）。 */
  function useHistory(item: HistoryItem) {
    sqlTextRef.value = item.sql;
  }
  function clearHistory() {
    history.value = [];
    try {
      localStorage.removeItem(HISTORY_KEY);
    } catch {
      /* ignore */
    }
  }

  // --- 外部执行回显（AI 终端可视化：exec_sql 把 SQL + 结果推入输出流） ---
  /**
   * 把一条"外部"执行的 SQL 及其结构化结果推入输出流（mysql CLI 风格）。
   *
   * 用于 AI 智能体的 SQL 终端可视化：AI 调 exec_sql 时，后端 emit ai:sql_result，
   * 视图层（仅命令行模式）调本方法把 SQL 行 + 结果条目回显进控制台，就像用户自己敲的一样。
   *
   * 与 [`execute`] 的区别：不经过 pendingQueryId/executing 状态机——那是用户手动执行
   * （invoke + db:query_result 事件回填）的专用流程；外部回显是独立的"只展示"路径。
   */
  function pushExternal(res: {
    sql: string;
    columns: string[];
    rows: string[][];
    affected: number;
    elapsedMs: number;
    error: string | null;
  }) {
    const sqlId = genId("e");
    // SQL 行（mysql> 前缀，done 态，不转圈）。
    pushEntry({ id: sqlId, kind: "sql", sql: res.sql, status: "done" });
    if (res.error) {
      insertAfter(sqlId, { id: genId("e"), kind: "error", sql: res.sql, message: res.error });
    } else if (res.columns.length > 0) {
      insertAfter(sqlId, {
        id: genId("e"),
        kind: "table",
        sql: res.sql,
        columns: res.columns,
        rows: res.rows,
        elapsedMs: res.elapsedMs,
      });
    } else {
      insertAfter(sqlId, {
        id: genId("e"),
        kind: "ok",
        sql: res.sql,
        affected: res.affected,
        elapsedMs: res.elapsedMs,
      });
    }
  }

  // --- 表结构（点表/按钮触发，预拉供补全 + 弹层展示） ---
  const selectedTable = ref<string | null>(null);
  const describeResult = ref<QueryResult | null>(null);

  /** 拉取表结构（不弹层，供补全 + describeResult）。 */
  async function loadStructure(table: string) {
    if (!connIdRef.value) return null;
    try {
      const desc = await dbDescribeTable(connIdRef.value, table);
      describeResult.value = desc;
      selectedTable.value = table;
      return desc;
    } catch {
      describeResult.value = null;
      return null;
    }
  }

  return {
    // 输出
    entries,
    scrollRef,
    bottomAnchorRef,
    executing,
    lastResult,
    clear,
    // 执行
    execute,
    onInputKeydown,
    setup,
    destroy,
    // 外部执行回显（AI 终端可视化）
    pushExternal,
    // 历史
    history,
    loadHistory,
    useHistory,
    clearHistory,
    // 表结构
    selectedTable,
    describeResult,
    loadStructure,
  };
}
