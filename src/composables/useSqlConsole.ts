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

/**
 * 去掉 SQL 开头的注释（`--` 行注释 / `#` 行注释 / `斜杠星号` 块注释，可连续多层）。
 *
 * 不做注释剥离时，`-- 注释\nDROP TABLE users` 的首个关键字是 `--`，
 * 既不在 WRITE_KEYWORDS 里也不在 DANGEROUS_KEYWORDS 里——只读模式的写保护
 * 与危险确认都会被整段绕过（与后端 tools.rs 的 strip_sql_comments 对应）。
 */
function stripLeadingComments(sql: string): string {
  let s = sql;
  for (;;) {
    const t = s.trimStart();
    if (t.startsWith("--") || t.startsWith("#")) {
      const nl = t.indexOf("\n");
      if (nl < 0) return ""; // 纯注释
      s = t.slice(nl + 1);
    } else if (t.startsWith("/*")) {
      const end = t.indexOf("*/");
      if (end < 0) return ""; // 未闭合的块注释
      s = t.slice(end + 2);
    } else {
      return t;
    }
  }
}

function firstKeyword(sql: string): string {
  return (stripLeadingComments(sql).trimStart().split(/\s+/)[0] || "").toUpperCase();
}
function isDeleteWithoutWhere(sql: string): boolean {
  return /^\s*DELETE\s+FROM\s+\S+\s*(;|$)/i.test(stripLeadingComments(sql));
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
  /** 滚动容器 ref（视图侧绑定，供自动滚动用）。 */
  const scrollRef = ref<HTMLElement | null>(null);
  const executing = ref(false);
  /** 最近一次完整结果（供代码模式/AI 等读取）。 */
  const lastResult = ref<FullResult | null>(null);

  /**
   * 滚动逻辑（mysql CLI 风格）：
   *
   * 执行时把"上一条输入"（语句回显行）滚到视口顶端，当前查询从顶端开始，
   * 结果在其下方展开：
   * - 结果未铺满视口 → 输入框自然跟在输出后面（无需滚动，语句保持在顶端）；
   * - 结果铺满视口 → 结果尾部停在视口下边框处，输入框紧跟结果之后
   *   （输入框此时在下边框之外，按 ↓ / 回车后随之下移回到底部）。
   *
   * 更早的历史在上方，向上滚动回看。
   */
  const bottomAnchorRef = ref<HTMLElement | null>(null);
  /** 当前"上一条输入"（最近执行的 SQL 回显行）的 DOM 锚点，视图侧 :ref 回调设置。 */
  const activeSqlEl = ref<HTMLElement | null>(null);
  /** 活跃 SQL entry 的 id（结果到达后不清空，作为滚动测量的起点）。 */
  const activeSqlId = ref<string | null>(null);

  // --- 合并滚动调度 -------------------------------------------------------------
  // 快速 IPC 下结果事件可能在 Vue 渲染之前就到达（onQueryResult 先于渲染执行），
  // 导致滚动回调成堆且互相覆盖、测量时机也不稳。统一调度到 rAF：
  // rAF 在 DOM 更新并布局后触发，同帧内多次请求只保留最后一次意图——
  // 保证"结果到达后的定位"总是赢过"语句入流时的定位"。
  type ScrollIntent = "sql-top" | "after-result";
  let pendingIntent: ScrollIntent | null = null;
  let scheduled = false;

  function scheduleScroll(intent: ScrollIntent) {
    pendingIntent = intent; // 后到覆盖先到
    if (scheduled) return;
    scheduled = true;
    const run = () => {
      scheduled = false;
      const it = pendingIntent;
      pendingIntent = null;
      if (it === "sql-top") {
        doScrollSqlToTop();
      } else if (it === "after-result") {
        doScrollAfterResult();
        // el-table 等组件异步布局（ResizeObserver/mounted 后才定高），
        // 首帧测得的高度可能偏小；延迟再校准一次，避免大结果滚动不足。
        setTimeout(doScrollAfterResult, 80);
      }
    };
    if (typeof requestAnimationFrame === "function") {
      requestAnimationFrame(run);
    } else {
      // 无 rAF 环境兜底（测试等）：nextTick + 微任务延迟，尽量等布局完成。
      nextTick(() => setTimeout(run, 0));
    }
  }

  /** 执行时：把语句回显行滚到视口顶端。 */
  function scrollSqlToTop() {
    scheduleScroll("sql-top");
  }

  /** 结果到达后：输入框跟在结果后面（铺满时结果尾部贴下边框）。 */
  function scrollToBottomIfOverflow() {
    scheduleScroll("after-result");
  }

  function doScrollSqlToTop() {
    const el = activeSqlEl.value;
    if (el) {
      el.scrollIntoView({ block: "start", inline: "nearest", behavior: "auto" });
    } else {
      const c = scrollRef.value;
      if (c) c.scrollTop = 0;
    }
  }

  /**
   * 定位到"结果之后"：目标 scrollTop = 内容总高 - 视口高（输入框贴底），
   * 但不超过"语句行顶端"（保证语句可见、输入框跟在结果后）。
   * 结果未铺满时 maxScroll < sqlTop → 不滚动，内容从顶部自然排列。
   */
  function doScrollAfterResult() {
    const c = scrollRef.value;
    if (!c) return;
    const anchor = activeSqlEl.value;
    const maxScroll = Math.max(0, c.scrollHeight - c.clientHeight); // 输入框贴底
    let target = maxScroll;
    if (anchor) {
      const cRect = c.getBoundingClientRect();
      const aRect = anchor.getBoundingClientRect();
      const sqlTopInContent = aRect.top - cRect.top + c.scrollTop;
      target = Math.min(target, sqlTopInContent); // 语句保持可见
    }
    c.scrollTop = target;
  }

  function pushEntry(e: ConsoleEntry) {
    // 纯数据入流；滚动由调用方决定（SQL 行→置顶，结果→溢出判断）。
    entries.value.push(e);
  }
  function clear() {
    entries.value = [];
    lastResult.value = null;
    activeSqlEl.value = null;
    activeSqlId.value = null;
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
    // 先剥离开头注释再按分号切分：注释内的分号（如 `-- a;b\nDROP TABLE x`）
    // 若直接切分会把 `b\nDROP TABLE x` 当成最后一条语句，绕过关键字判定。
    const stmts = stripLeadingComments(sqlRaw)
      .split(";")
      .map((s) => s.trim())
      .filter((s) => s.length > 0);
    // 编辑器可能含多条语句（分号分隔）；只执行最后一条非空语句，避免多语句语法错误。
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
    // "上一条输入"置顶，当前查询从顶端开始（mysql CLI 风格）。
    activeSqlId.value = entryId;
    scrollSqlToTop();
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
    scrollToBottomIfOverflow();
  }

  /** 在指定 entry 之后插入新条目（mysql CLI：结果插在 SQL 行之后）。 */
  function insertAfter(id: string, e: ConsoleEntry) {
    const idx = entries.value.findIndex((x) => x.id === id);
    if (idx >= 0) {
      entries.value.splice(idx + 1, 0, e);
    } else {
      entries.value.push(e);
    }
    scrollToBottomIfOverflow();
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
    scrollToBottomIfOverflow();
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
  /** 历史浏览游标：null=未浏览（正在编辑新命令），0=最新一条，递增向更旧。 */
  const histCursor = ref<number | null>(null);
  /** 进入历史浏览前的草稿（以便按 ↓ 回到未写完的命令）。 */
  const histDraft = ref("");

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
    // 新命令入历史后重置浏览状态，下次 ↑ 从最新一条开始。
    histCursor.value = null;
    histDraft.value = "";
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

  /**
   * ↑ 浏览上一条历史。返回应填入输入框的文本；已到最旧一条时返回 null（保持不变）。
   * 首次进入浏览时 current 为输入框现值，会被记为草稿。
   */
  function historyOlder(current: string): string | null {
    if (history.value.length === 0) return null;
    if (histCursor.value === null) {
      histDraft.value = current;
      histCursor.value = 0;
    } else if (histCursor.value < history.value.length - 1) {
      histCursor.value += 1;
    } else {
      return null; // 已是最旧
    }
    return history.value[histCursor.value].sql;
  }

  /**
   * ↓ 浏览下一条历史（向最新方向）。返回应填入的文本；
   * 越过最新一条时返回草稿（空字符串=清空输入框），未处于浏览时返回 null。
   */
  function historyNewer(): string | null {
    if (histCursor.value === null) return null; // 未在浏览
    if (histCursor.value > 0) {
      histCursor.value -= 1;
      return history.value[histCursor.value].sql;
    }
    // 回到最新之外 → 恢复草稿并退出浏览。
    histCursor.value = null;
    return histDraft.value;
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
    // 与用户手动执行一致：语句回显置顶。
    activeSqlId.value = sqlId;
    scrollSqlToTop();
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
    activeSqlEl,
    activeSqlId,
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
    historyOlder,
    historyNewer,
    clearHistory,
    // 表结构
    selectedTable,
    describeResult,
    loadStructure,
  };
}
