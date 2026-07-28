<!--
  SqlConsoleView.vue — MySQL SQL 控制台

  功能：
  - 选择 / 新建 / 编辑 / 删除 DB profile，连接 / 断开。
  - 只读 / 读写模式切换（默认只读，写操作需切到读写模式）。
  - 左侧表列表：dbListTables，点击 → DESCRIBE 表结构 + 表名填入 SQL 编辑器。
  - SQL 编辑器（textarea，Ctrl+Enter 执行）：执行 / 清空 / AI 优化 / AI 解释。
  - 结果区：动态列 el-table，显示行数、耗时、影响行数；出错显示 error。
  - 订阅 db:query_result 事件，按 queryId 匹配当前等待的查询。
-->
<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import {
  Refresh,
  VideoPlay,
  VideoPause,
  Plus,
  Edit,
  Delete,
  CaretRight,
  Delete as ClearIcon,
  MagicStick,
  View,
  Connection,
  Coin,
  Document,
} from "@element-plus/icons-vue";
import {
  dbDeleteProfile,
  dbDisconnect,
  dbListDatabases,
  dbListProfiles,
  dbListTables,
  type DraggedTable,
} from "@/api/db";
import type { DbProfile, QueryResult, AiSqlResultEvent } from "@/api/types";
import { listen } from "@tauri-apps/api/event";
import DbProfileDialog from "@/components/DbProfileDialog.vue";
import AiPanel from "@/components/AiPanel.vue";
import { useAiDbStore } from "@/stores/ai";
import { useDbStore } from "@/stores/db";
import { useSettingsStore } from "@/stores/settings";
import { useCodeMirror } from "@/composables/useCodeMirror";

// KeepAlive 按 name 匹配缓存本组件（保留 DB 助手面板状态）。
defineOptions({ name: "SqlConsoleView" });

// 用 DB 域 store：aiOptimize/aiExplain 的结果会进右侧 DB 助手面板，与 SSH 助手隔离。
const ai = useAiDbStore();
const db = useDbStore();
const settings = useSettingsStore();
const isDark = computed(() => settings.terminal.theme === "dark");

// --- AI 系统提示词 ----------------------------------------------------------
const SYSTEM_DIAGNOSE =
  "你是一名资深 MySQL DBA。用户会提供一段 SQL，请给出优化建议：" +
  "1) 指出潜在的性能问题（缺索引、全表扫描、N+1、回表等）；" +
  "2) 给出优化后的 SQL（用 ```sql 代码块）；" +
  "3) 必要时建议索引（CREATE INDEX）。简洁、专业、中文。";

const SYSTEM_EXPLAIN =
  "你是一名资深 MySQL DBA。用户会提供一段 SQL，请用通俗简洁的中文解释：" +
  "1) 这段 SQL 做了什么；" +
  "2) 涉及的关键字/函数/子查询含义；" +
  "3) 可能的注意事项。不要重复 SQL 原文。";

// --- profile 列表 -----------------------------------------------------------
const profiles = ref<DbProfile[]>([]);
const selectedProfileId = ref<string | null>(null);
const loadingProfiles = ref(false);

const selectedProfile = computed(
  () => profiles.value.find((p) => p.id === selectedProfileId.value) ?? null
);

function profileLabel(p: DbProfile): string {
  return `${p.name} (${p.host}:${p.port})`;
}

async function loadProfiles() {
  loadingProfiles.value = true;
  try {
    profiles.value = await dbListProfiles();
    // 同步树根：实例层。
    treeData.value = rebuildInstanceNodes();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    ElMessage.error("加载数据库连接失败：" + msg);
  } finally {
    loadingProfiles.value = false;
  }
}

// --- 连接状态 ---------------------------------------------------------------
// connId 由全局 db store 维护，便于 AI 智能体面板读取活动连接（启用 SQL 工具集）。
const connId = computed(() => db.activeConnId);
const connecting = ref(false);
const isConnected = computed(() => !!connId.value);

const readOnly = ref(true); // 默认只读

async function connect() {
  if (!selectedProfileId.value) {
    ElMessage.warning("请先选择数据库连接");
    return;
  }
  connecting.value = true;
  try {
    const profile = profiles.value.find((p) => p.id === selectedProfileId.value);
    const name = profile?.name ?? selectedProfileId.value;
    await db.connect(selectedProfileId.value, name);
    ElMessage.success("已连接");
    // 重建树并展开当前实例节点（触发懒加载库列表）。
    treeData.value = rebuildInstanceNodes();
    await nextTick();
    const instKey = `inst-${selectedProfileId.value}`;
    treeRef.value?.store?.getNode(instKey)?.expand();
    // 连接后聚焦输入：命令行模式聚焦原生输入框，代码模式挂载/重挂 CodeMirror。
    await nextTick();
    if (editorMode.value === "console") {
      cliInputRef.value?.focus();
    } else {
      remountSqlEditor();
    }
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    ElMessage.error("连接失败：" + msg);
    db.clear();
  } finally {
    connecting.value = false;
  }
}

async function disconnect() {
  if (!connId.value) return;
  await db.disconnect();
  ElMessage.success("已断开");
  // 重置树（清除已加载的库/表子节点，保留实例层）。
  treeData.value = rebuildInstanceNodes();
  describeResult.value = null;
  result.value = null;
  // 断开后清除关联库（db.disconnect 已清空 conns，显式再置一次更稳妥）。
  db.setActiveDatabase(null);
}

// --- 表列表 -----------------------------------------------------------------
// --- 左侧导航树（实例 > 库 > 表）-------------------------------------------
interface TreeNode {
  type: "instance" | "database" | "table";
  /** 唯一 key。 */
  key: string;
  /** 显示文本。 */
  label: string;
  /** 实例：profile id；库：库名；表：表名。 */
  value: string;
  /** 表所属的库名（仅 table 节点用）。 */
  database?: string;
  /** 是否已加载子节点（懒加载标记）。 */
  loaded?: boolean;
  /** 子节点。 */
  children?: TreeNode[];
  /** 是否叶子（表是叶子）。 */
  isLeaf?: boolean;
  /** 表的字段名列表（点表时预拉，供 CodeMirror 字段级自动补全）。 */
  columns?: string[];
}

/** 树根：所有 profile 作为实例节点。 */
const treeData = ref<TreeNode[]>([]);
const treeRef = ref<any>(null);

/** 重建实例层：从 profiles 生成根节点。 */
function rebuildInstanceNodes(): TreeNode[] {
  return profiles.value.map((p) => ({
    type: "instance",
    key: `inst-${p.id}`,
    label: p.name,
    value: p.id,
    isLeaf: false,
    children: [],
  }));
}

/** 懒加载子节点（el-tree 的 load 回调）。 */
async function loadTreeNode(node: any, resolve: (children: TreeNode[]) => void) {
  const data: TreeNode = node.data ?? node;
  if (data.type === "instance") {
    // 展开实例 → 列出库。要求已连接（connId 存在）。
    if (!db.activeConnId) {
      resolve([]);
      return;
    }
    try {
      const dbs = await dbListDatabases(db.activeConnId);
      resolve(
        dbs
          .filter((d) => !["information_schema", "performance_schema", "mysql", "sys"].includes(d))
          .map((d) => ({
            type: "database" as const,
            key: `db-${data.value}-${d}`,
            label: d,
            value: d,
            isLeaf: false,
            children: [],
          })),
      );
    } catch {
      resolve([]);
    }
    return;
  }
  if (data.type === "database") {
    // 展开库 → 列出表。
    if (!db.activeConnId) {
      resolve([]);
      return;
    }
    try {
      const tables = await dbListTables(db.activeConnId, data.value);
      resolve(
        tables.map((t) => ({
          type: "table" as const,
          key: `tbl-${data.value}-${t}`,
          label: t,
          value: t,
          database: data.value,
          isLeaf: true,
        })),
      );
    } catch {
      resolve([]);
    }
    return;
  }
  resolve([]);
}

/** 拖拽表节点：把表信息写入 dataTransfer，供 AI 面板/SQL 编辑器接收。 */
function onTableDragStart(e: DragEvent, data: TreeNode) {
  if (data.type !== "table" || !db.activeConnId) return;
  const payload: DraggedTable = {
    connId: db.activeConnId,
    // 父节点是 database 节点；若拿不到库名则传 null（用默认库）。
    database: data.database ?? null,
    table: data.value,
  };
  // 同时写入文本（支持拖到外部/SQL 编辑器）和自定义 JSON（AI 面板解析）。
  e.dataTransfer?.setData("text/plain", payload.table);
  e.dataTransfer?.setData(
    "application/x-xterm-table",
    JSON.stringify(payload),
  );
  if (e.dataTransfer) e.dataTransfer.effectAllowed = "copy";
}

/** 点击树节点。 */
async function onTreeNodeClick(data: TreeNode) {
  if (data.type === "instance") {
    // 点击实例：切换连接（若未连接该 profile）。
    if (selectedProfileId.value !== data.value) {
      selectedProfileId.value = data.value;
      await connect();
    }
    return;
  }
  if (data.type === "database") {
    // 点击库：仅展开/收起（不填 USE，避免与后续 SQL 拼成多语句导致语法错误）。
    // 点表时已用完全限定名 `db`.`table`，无需 USE 切换当前库。
    // 记录关联库，供 AI 助手显示上下文 / 注入 system prompt。
    db.setActiveDatabase(data.value);
    return;
  }
  if (data.type === "table") {
    // 点击表：填入 SELECT 模板 + 记录表名 + 预拉表结构（供补全，但不弹出对话框）。
    // 同时把表所在库设为关联库（与点库行为一致）。
    db.setActiveDatabase(data.database ?? null);
    const db2 = data.database ?? "";
    const qualified = db2 ? `\`${db2}\`.\`${data.value}\`` : `\`${data.value}\``;
    const selectTpl = `SELECT * FROM ${qualified} LIMIT 100;`;
    if (editorMode.value === "console") {
      cliInput.value = selectTpl;
      cliInputRef.value?.focus();
    } else {
      const v = getSqlView();
      if (v) {
        v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: selectTpl } });
        v.focus();
      } else {
        sqlText.value = selectTpl;
      }
    }
    selectedTable.value = data.value;
    // 预拉表结构（不弹层）——composable 存到 describeResult，字段名写入树节点供补全。
    const desc = await loadStructure(data.value);
    if (desc) {
      const cols = desc.rows.map((r) => r[0]).filter(Boolean);
      data.columns = cols;
    }
    return;
  }
}

// selectedTable / describeResult / showStructure 由 useSqlConsole composable 提供（见下方）。
// 历史抽屉（命令行模式下历史不再常驻侧栏，改为抽屉按需打开）。
const historyDrawer = ref(false);
// 表结构弹层（默认不展示，点顶部按钮或点表时弹出）。
const structureVisible = ref(false);
// 编辑模式：console=命令行模式（Enter 执行，结果累积）；code=代码模式（多行编辑器+结果区）。
const editorMode = ref<"console" | "code">("console");
// 左侧数据库树宽度（可拖拽调整）。
const sidebarWidth = ref(200);
function startResize(e: MouseEvent) {
  e.preventDefault();
  const startX = e.clientX;
  const startW = sidebarWidth.value;
  const onMove = (ev: MouseEvent) => {
    const w = startW + (ev.clientX - startX);
    sidebarWidth.value = Math.max(140, Math.min(480, w));
  };
  const onUp = () => {
    document.removeEventListener("mousemove", onMove);
    document.removeEventListener("mouseup", onUp);
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
  };
  document.body.style.cursor = "col-resize";
  document.body.style.userSelect = "none";
  document.addEventListener("mousemove", onMove);
  document.addEventListener("mouseup", onUp);
}
// 当前选中 profile 名（控制台顶部显示）。
const selectedProfileName = computed(() => {
  const p = profiles.value.find((x) => x.id === selectedProfileId.value);
  return p ? `${p.name} (${p.host}:${p.port})` : "";
});

// --- SQL 编辑器（CodeMirror）----------------------------------------------
const sqlText = ref("");
const sqlEditorRef = ref<HTMLElement | null>(null);
// 从已加载的树节点收集 表→字段 映射，供 CodeMirror SQL 自动补全（表名 + 字段名）。
// 字段来自点表时预拉的 DESCRIBE；未点过的表字段为空（仅补表名）。
const tableSchema = computed(() => {
  const map: Record<string, string[]> = {};
  for (const inst of treeData.value) {
    for (const dbNode of inst.children ?? []) {
      for (const tbl of dbNode.children ?? []) {
        if (tbl.type === "table") map[tbl.value] = tbl.columns ?? [];
      }
    }
  }
  return map;
});
const { mount: mountSqlEditor, remount: remountSqlEditor, getView: getSqlView } = useCodeMirror(
  sqlEditorRef,
  sqlText,
  tableSchema,
  () => void execute(),
  isDark,
  () => void execute(), // Enter 直接执行（Shift+Enter 换行）
);

// 模式切换时重新挂载 CodeMirror（容器 DOM 因 v-if 切换而变化，需 destroy 后重建）。
watch(editorMode, () => {
  remountSqlEditor();
});

/** 在编辑器末尾插入文本（用于点击表名快速插入查询模板）。 */
function insertTextAtCursor(text: string) {
  const v = getSqlView();
  if (!v) {
    sqlText.value += text;
    return;
  }
  const docLen = v.state.doc.length;
  v.dispatch({
    changes: { from: docLen, insert: text },
    selection: { anchor: docLen + text.length },
  });
  v.focus();
}

function clearSql() {
  const v = getSqlView();
  if (v) {
    v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: "" } });
  } else {
    sqlText.value = "";
  }
}

// --- SQL 命令行控制台（逻辑抽到 useSqlConsole composable） ---
import { useSqlConsole } from "@/composables/useSqlConsole";

const {
  entries: consoleEntries,
  scrollRef: consoleScrollRef,
  bottomAnchorRef: consoleBottomAnchor,
  executing,
  lastResult: result,
  clear: clearConsole,
  execute: runSql,
  onInputKeydown,
  setup: setupConsole,
  destroy: destroyConsole,
  history,
  loadHistory,
  useHistory: applyHistory,
  clearHistory,
  selectedTable,
  describeResult,
  loadStructure,
  pushExternal,
} = useSqlConsole(connId, sqlText, async (kw, noWhere) => {
  try {
    await ElMessageBox.confirm(
      `检测到危险操作：${kw}${noWhere ? "（DELETE 无 WHERE）" : ""}。确认继续吗？`,
      "危险操作确认",
      { type: "warning", confirmButtonText: "确认执行", cancelButtonText: "取消", confirmButtonClass: "el-button--danger" },
    );
    return true;
  } catch {
    return false;
  }
});

/** 适配视图：执行（带只读模式判定，代码模式从 CodeMirror 读 sqlText）。 */
async function execute() {
  await runSql(readOnly.value);
}

// --- 命令行模式专用输入（原生 textarea，非 CodeMirror） ---
const cliInputRef = ref<HTMLTextAreaElement | null>(null);
const cliInput = ref("");

/** 命令行回车：执行 cliInput 内容，成功后清空输入框（mysql CLI 风格）。 */
async function onCliKeydown(e: KeyboardEvent) {
  if (e.key !== "Enter") return;
  if (e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return; // Shift+Enter 换行
  e.preventDefault();
  const sql = cliInput.value.trim();
  if (!sql || executing.value) return;
  const ok = await runSql(readOnly.value, sql);
  if (ok) {
    cliInput.value = ""; // 清空输入框，等待下一条
    await nextTick();
    cliInputRef.value?.focus();
  }
}

/** 适配视图：历史条目填入当前活动输入（命令行→cliInput，代码模式→CodeMirror）。 */
function useHistoryItem(item: { sql: string; ts: number; elapsedMs?: number }) {
  applyHistory(item);
  if (editorMode.value === "console") {
    cliInput.value = item.sql;
    cliInputRef.value?.focus();
  } else {
    const v = getSqlView();
    if (v) {
      v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: item.sql } });
      v.focus();
    }
  }
}

/** 顶部表结构按钮：拉取结构（composable）+ 弹出展示。 */
async function showStructure() {
  if (!selectedTable.value) return;
  await loadStructure(selectedTable.value);
  structureVisible.value = true;
}

/** 导出当前结果为 CSV/JSON（复制到剪贴板）。 */
async function copyToClipboard(text: string) {
  try {
    await navigator.clipboard?.writeText(text);
    ElMessage.success("已复制");
  } catch {
    ElMessage.warning("复制失败（剪贴板未授权）");
  }
}
function exportCsv() {
  if (!result.value || !result.value.columns.length) return;
  const { columns, rows } = result.value;
  const esc = (v: string) => {
    const s = v ?? "";
    return /[",\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
  };
  const lines = [columns.map(esc).join(","), ...rows.map((r) => r.map(esc).join(","))];
  void copyToClipboard(lines.join("\n"));
}
function exportJson() {
  if (!result.value || !result.value.columns.length) return;
  const { columns, rows } = result.value;
  const objs = rows.map((r) => {
    const o: Record<string, string> = {};
    columns.forEach((c, i) => (o[c] = r[i] ?? ""));
    return o;
  });
  void copyToClipboard(JSON.stringify(objs, null, 2));
}

// --- 结果展示 ---------------------------------------------------------------
const resultIsSelect = computed(() => {
  if (!result.value) return false;
  return result.value.columns.length > 0;
});

// --- AI 集成 ----------------------------------------------------------------
function requireSql(): string | null {
  const sql = sqlText.value.trim();
  if (!sql) {
    ElMessage.warning("请先输入 SQL");
    return null;
  }
  return sql;
}

function aiOptimize() {
  const sql = requireSql();
  if (!sql) return;
  if (ai.sending) {
    ElMessage.warning("AI 正在处理中，请稍候");
    return;
  }
  void ai.send(`请优化以下 MySQL SQL：\n\n\`\`\`sql\n${sql}\n\`\`\``, SYSTEM_DIAGNOSE);
  ElMessage.success("已发送给 AI，请在右侧 AI 面板查看");
}

function aiExplain() {
  const sql = requireSql();
  if (!sql) return;
  if (ai.sending) {
    ElMessage.warning("AI 正在处理中，请稍候");
    return;
  }
  void ai.send(`请解释以下 MySQL SQL：\n\n\`\`\`sql\n${sql}\n\`\`\``, SYSTEM_EXPLAIN);
  ElMessage.success("已发送给 AI，请在右侧 AI 面板查看");
}

// --- profile 弹窗 -----------------------------------------------------------
const dialogVisible = ref(false);
const editingProfile = ref<DbProfile | null>(null);

function openCreateProfile() {
  editingProfile.value = null;
  dialogVisible.value = true;
}

function openEditProfile() {
  if (!selectedProfile.value) {
    ElMessage.warning("请先选择一个连接");
    return;
  }
  editingProfile.value = selectedProfile.value;
  dialogVisible.value = true;
}

function onProfileSaved() {
  void loadProfiles();
}

async function deleteProfile() {
  if (!selectedProfile.value) {
    ElMessage.warning("请先选择一个连接");
    return;
  }
  const p = selectedProfile.value;
  try {
    await ElMessageBox.confirm(`确定删除连接「${p.name}」吗？`, "删除确认", {
      type: "warning",
      confirmButtonText: "删除",
      cancelButtonText: "取消",
    });
  } catch {
    return;
  }
  try {
    await dbDeleteProfile(p.id);
    ElMessage.success("已删除");
    if (selectedProfileId.value === p.id) selectedProfileId.value = null;
    await loadProfiles();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    ElMessage.error("删除失败：" + msg);
  }
}

// --- AI SQL 终端可视化：监听 exec_sql 回显事件 ---
// 仅命令行模式把 AI 执行的 SQL + 结构化结果回显进控制台输出流。
// 后端在 sql_agent.terminal_visualization 开启时 emit；代码模式不回显。
let unlistenSqlResult: (() => void) | null = null;

// --- 生命周期 ---------------------------------------------------------------
onMounted(async () => {
  await loadProfiles();
  loadHistory();
  // 订阅 db:query_result 事件（composable 内部处理）。
  await setupConsole();
  // 订阅 ai:sql_result（exec_sql 终端可视化回显）。
  unlistenSqlResult = await listen<AiSqlResultEvent>("ai:sql_result", (e) => {
    // 仅命令行模式回显（代码模式有自己的结果区，不混入输出流）。
    if (editorMode.value !== "console") return;
    pushExternal(e.payload);
  });
});

onBeforeUnmount(() => {
  destroyConsole();
  if (unlistenSqlResult) {
    unlistenSqlResult();
    unlistenSqlResult = null;
  }
  // 切换页面时主动断开连接，避免后端连接泄漏。
  if (connId.value) {
    void dbDisconnect(connId.value).catch(() => {
      /* ignore */
    });
    db.clear();
  }
});
</script>

<template>
  <div class="sql-console">
    <!-- 顶部工具栏 -->
    <div class="toolbar">
      <div class="toolbar-left">
        <el-select
          v-model="selectedProfileId"
          placeholder="选择数据库连接"
          :loading="loadingProfiles"
          :disabled="isConnected"
          filterable
          style="width: 280px"
        >
          <el-option
            v-for="p in profiles"
            :key="p.id"
            :label="profileLabel(p)"
            :value="p.id"
          />
        </el-select>

        <el-button :icon="Plus" :disabled="isConnected" @click="openCreateProfile" />
        <el-button :icon="Edit" :disabled="isConnected || !selectedProfileId" @click="openEditProfile" />
        <el-button
          :icon="Delete"
          type="danger"
          plain
          :disabled="isConnected || !selectedProfileId"
          @click="deleteProfile"
        />

        <el-divider direction="vertical" />

        <template v-if="!isConnected">
          <el-button
            type="primary"
            :icon="VideoPlay"
            :loading="connecting"
            :disabled="!selectedProfileId"
            @click="connect"
          >
            连接
          </el-button>
        </template>
        <template v-else>
          <el-tag type="success" effect="dark" size="default" round>已连接</el-tag>
          <el-button :icon="VideoPause" type="warning" plain @click="disconnect">断开</el-button>
        </template>
      </div>

      <div class="toolbar-right">
        <span class="mode-label">模式</span>
        <el-radio-group v-model="readOnly" size="small">
          <el-radio-button :value="true">只读</el-radio-button>
          <el-radio-button :value="false">读写</el-radio-button>
        </el-radio-group>
      </div>
    </div>

    <!-- 主体 -->
    <div v-if="!isConnected && !connecting" class="placeholder">
      <el-empty description="请选择 DB profile 并连接" />
    </div>
    <div v-else-if="connecting && !isConnected" class="placeholder">
      <el-icon class="is-loading" :size="32"><Refresh /></el-icon>
      <p>正在连接...</p>
    </div>

    <div v-else class="console-body">
      <!-- 左侧：数据库树（宽度可拖拽） -->
      <aside class="table-list" :style="{ width: sidebarWidth + 'px' }">
        <div class="list-header">
          <span>数据库</span>
          <el-button
            :icon="Refresh"
            size="small"
            circle
            @click="treeData = rebuildInstanceNodes()"
          />
        </div>
        <div class="list-body tree-body">
          <el-tree
            ref="treeRef"
            :data="treeData"
            node-key="key"
            :props="{ label: 'label', children: 'children', isLeaf: 'isLeaf' }"
            :load="loadTreeNode"
            :indent="10"
            lazy
            :expand-on-click-node="false"
            highlight-current
            @node-click="onTreeNodeClick"
          >
            <template #default="{ data }">
              <span
                class="tree-node"
                :class="'node-' + data.type + (data.type === 'table' ? ' draggable' : '')"
                :draggable="data.type === 'table'"
                @dragstart="onTableDragStart($event, data)"
              >
                <el-icon v-if="data.type === 'instance'" class="node-icon"><Connection /></el-icon>
                <el-icon v-else-if="data.type === 'database'" class="node-icon"><Coin /></el-icon>
                <el-icon v-else class="node-icon"><Document /></el-icon>
                <span class="node-label">{{ data.label }}</span>
              </span>
            </template>
          </el-tree>
          <div v-if="treeData.length === 0" class="empty-tip">未配置数据库实例</div>
        </div>

        <!-- 查询历史 -->
      </aside>

      <!-- 拖拽分隔条 -->
      <div class="sidebar-resizer" @mousedown="startResize"></div>

      <!-- 右侧：编辑器 + 结果 -->
      <section class="editor-area sql-console-area">
        <!-- 顶部小工具栏 -->
        <div class="console-toolbar">
          <span class="console-prompt">
            <el-icon><Connection /></el-icon>
            {{ selectedProfileName }}
          </span>
          <!-- 模式切换：命令行 / 代码 -->
          <el-radio-group v-model="editorMode" size="small">
            <el-radio-button value="console">命令行</el-radio-button>
            <el-radio-button value="code">代码</el-radio-button>
          </el-radio-group>
          <!-- 表结构按钮（有选中表时可点，弹出展示） -->
          <el-button
            size="small"
            link
            :icon="Coin"
            :disabled="!selectedTable"
            @click="showStructure"
          >
            表结构{{ selectedTable ? `: ${selectedTable}` : "" }}
          </el-button>
          <el-button :icon="Delete" size="small" link @click="clearConsole">清屏</el-button>
          <el-button :icon="MagicStick" size="small" link @click="aiOptimize">AI 优化</el-button>
          <el-button :icon="View" size="small" link @click="aiExplain">AI 解释</el-button>
          <el-button :icon="Plus" size="small" link @click="historyDrawer = true">历史</el-button>
        </div>

        <!-- ============ 命令行模式（mysql CLI 风格：输入在结果流末尾） ============ -->
        <template v-if="editorMode === 'console'">
        <!-- 单一输出流：历史 SQL+结果 交织，末尾是当前输入 prompt -->
        <div ref="consoleScrollRef" class="console-output">
          <div v-if="consoleEntries.length === 0" class="console-empty">
            输入 SQL 后按 Enter 执行（Shift+Enter 换行）。点击左侧表名快速查询。
          </div>
          <template v-for="e in consoleEntries" :key="e.id">
            <!-- 执行的 SQL -->
            <div v-if="e.kind === 'sql'" class="entry entry-sql">
              <span class="entry-prompt">mysql&gt;</span>
              <span class="entry-sql-text">{{ e.sql }}</span>
              <el-icon v-if="e.status === 'running'" class="is-loading entry-spin"><Refresh /></el-icon>
            </div>
            <!-- 表格结果 -->
            <div v-else-if="e.kind === 'table'" class="entry entry-table">
              <el-table :data="e.rows" border stripe size="small" class="result-table" max-height="400">
                <el-table-column type="index" label="#" width="50" fixed />
                <el-table-column
                  v-for="(c, i) in e.columns"
                  :key="i"
                  :prop="String(i)"
                  :label="c"
                  min-width="120"
                  show-overflow-tooltip
                />
              </el-table>
              <div class="entry-meta">{{ e.rows.length }} 行{{ e.elapsedMs ? ` · ${e.elapsedMs}ms` : "" }}</div>
            </div>
            <!-- 非查询成功 -->
            <div v-else-if="e.kind === 'ok'" class="entry entry-ok">
              OK，影响 {{ e.affected }} 行{{ e.elapsedMs ? ` · ${e.elapsedMs}ms` : "" }}
            </div>
            <!-- 错误 -->
            <div v-else-if="e.kind === 'error'" class="entry entry-error">
              ERROR: {{ e.message }}
            </div>
            <!-- 信息（表结构标题等） -->
            <div v-else-if="e.kind === 'info'" class="entry entry-info">-- {{ e.text }}</div>
          </template>
          <!-- 当前输入（在结果流末尾，mysql CLI 风格；原生输入框，非 CodeMirror） -->
          <div ref="consoleBottomAnchor" class="console-input-wrap">
            <span class="input-prompt">mysql&gt;</span>
            <textarea
              ref="cliInputRef"
              v-model="cliInput"
              class="cli-input"
              rows="1"
              placeholder="输入 SQL，Enter 执行，Shift+Enter 换行"
              spellcheck="false"
              @keydown="onCliKeydown"
            />
          </div>
        </div>
        </template>

        <!-- ============ 代码模式（多行编辑器 + 结果区） ============ -->
        <template v-else>
          <div class="code-editor-toolbar">
            <el-button type="primary" size="small" :icon="CaretRight" :loading="executing" @click="execute">
              执行
            </el-button>
            <el-button size="small" :icon="ClearIcon" @click="clearSql">清空</el-button>
          </div>
          <!-- 复用同一个 CodeMirror 实例：命令行/代码模式共享 sqlEditorRef。
               注意：v-if 切换会销毁重建 DOM，CM 需重新挂载，由 connect 后的 mount 保证。 -->
          <div v-if="editorMode === 'code'" ref="sqlEditorRef" class="code-editor" />
          <div class="code-result">
            <div class="code-result-header">
              <span class="result-meta" v-if="result && !result.error">
                {{ resultIsSelect ? `${result.rows.length} 行` : `影响 ${result.affected} 行` }}
                · {{ result.elapsedMs }} ms
              </span>
              <span class="result-meta error" v-else-if="result && result.error">错误</span>
              <div v-if="result && resultIsSelect && !result.error" class="result-actions">
                <el-button size="small" link @click="exportCsv">复制 CSV</el-button>
                <el-button size="small" link @click="exportJson">复制 JSON</el-button>
              </div>
            </div>
            <div v-loading="executing" class="code-result-body">
              <div v-if="!result" class="empty-tip">尚未执行查询</div>
              <el-alert v-else-if="result.error" :title="result.error" type="error" show-icon :closable="false" />
              <el-table
                v-else-if="resultIsSelect"
                :data="result.rows"
                border
                stripe
                size="small"
                height="100%"
                class="result-table"
              >
                <el-table-column type="index" label="#" width="50" fixed />
                <el-table-column
                  v-for="(c, i) in result.columns"
                  :key="i"
                  :prop="String(i)"
                  :label="c"
                  min-width="120"
                  show-overflow-tooltip
                />
              </el-table>
              <el-alert
                v-else
                :title="`执行成功，影响 ${result.affected} 行（${result.elapsedMs} ms）`"
                type="success"
                show-icon
                :closable="false"
              />
            </div>
          </div>
        </template>
      </section>
      <!-- DB 助手面板：仅在 SQL 页显示，与 SSH 助手完全隔离 -->
      <AiPanel domain="db" />
    </div>

    <!-- profile 弹窗 -->
    <DbProfileDialog
      v-model:visible="dialogVisible"
      :profile="editingProfile"
      @saved="onProfileSaved"
    />

    <!-- 历史抽屉 -->
    <el-drawer v-model="historyDrawer" title="查询历史" size="360px" direction="rtl">
      <div class="history-drawer">
        <el-button v-if="history.length" size="small" link type="danger" @click="clearHistory">
          清空历史
        </el-button>
        <div
          v-for="(h, i) in history"
          :key="i"
          class="history-item"
          :title="h.sql"
          @click="useHistoryItem(h); historyDrawer = false"
        >
          <span class="history-sql">{{ h.sql }}</span>
          <span v-if="h.elapsedMs" class="history-meta">{{ h.elapsedMs }}ms</span>
        </div>
        <div v-if="history.length === 0" class="empty-tip">无历史</div>
      </div>
    </el-drawer>

    <!-- 表结构弹层（默认不展示，点顶部按钮或点表时弹出） -->
    <el-dialog
      v-model="structureVisible"
      :title="selectedTable ? `表结构: ${selectedTable}` : '表结构'"
      width="720px"
      append-to-body
    >
      <el-table
        v-if="describeResult"
        :data="describeResult.rows"
        size="small"
        border
        max-height="480"
      >
        <el-table-column
          v-for="(c, i) in describeResult.columns"
          :key="i"
          :prop="String(i)"
          :label="c"
          min-width="110"
          show-overflow-tooltip
        />
      </el-table>
      <div v-else class="empty-tip">未选择表</div>
    </el-dialog>
  </div>
</template>

<style scoped>
.sql-console {
  display: flex;
  flex-direction: column;
  height: 100%;
  width: 100%;
  box-sizing: border-box;
  background: var(--el-bg-color);
  overflow: hidden;
}

/* 工具栏 */
.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 10px 16px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  background: var(--el-bg-color-overlay);
  flex-shrink: 0;
}
.toolbar-left,
.toolbar-right {
  display: flex;
  align-items: center;
  gap: 8px;
}
.mode-label {
  font-size: 13px;
  color: var(--el-text-color-secondary);
}

/* 占位 */
.placeholder {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 12px;
  color: var(--el-text-color-secondary);
}

/* 主体布局 */
.console-body {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: row;
}

/* 左侧表列表（宽度由 sidebarWidth inline style 控制） */
.table-list {
  flex-shrink: 0;
  border-right: none;
  background: var(--el-bg-color-overlay);
  display: flex;
  flex-direction: column;
  overflow: hidden;
}
/* 拖拽分隔条 */
.sidebar-resizer {
  width: 4px;
  flex-shrink: 0;
  cursor: col-resize;
  background: var(--el-border-color-lighter);
  transition: background 0.15s;
}
.sidebar-resizer:hover {
  background: var(--el-color-primary);
}
.list-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 8px 10px;
  font-size: 12px;
  font-weight: 600;
  color: var(--el-text-color-secondary);
  border-bottom: 1px solid var(--el-border-color-lighter);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}
.list-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
}
/* 数据库导航树 */
.tree-body {
  padding: 4px 0;
}
/* 减小 el-tree 每层缩进（默认 18px 太宽） */
.tree-body :deep(.el-tree-node__content) {
  padding-left: 4px !important;
}
.tree-body :deep(.el-tree-node) {
  white-space: nowrap;
}
.tree-body :deep(.el-tree-node__expand-icon) {
  margin-right: 2px;
}
.tree-node {
  display: flex;
  align-items: center;
  gap: 5px;
  font-size: 13px;
}
/* 表节点可拖拽到 AI 输入框/SQL 编辑器。 */
.tree-node.draggable {
  cursor: grab;
}
.tree-node.draggable:hover .node-label {
  color: var(--el-color-primary);
}
.tree-node.draggable:active {
  cursor: grabbing;
}
.node-icon {
  font-size: 13px;
  color: var(--el-text-color-secondary);
}
.node-instance .node-icon {
  color: var(--el-color-primary);
}
.node-database .node-icon {
  color: var(--el-color-warning);
}
.node-label {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.empty-tip {
  padding: 16px 12px;
  font-size: 12px;
  color: var(--el-text-color-placeholder);
  text-align: center;
}
/* 查询历史 */
.history-item {
  padding: 5px 10px;
  font-size: 12px;
  cursor: pointer;
  border-bottom: 1px solid var(--el-border-color-lighter);
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.history-item:hover {
  background: var(--el-fill-color-light);
}
.history-sql {
  color: var(--el-text-color-primary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  font-family: "Consolas", monospace;
}
.history-meta {
  font-size: 10px;
  color: var(--el-text-color-placeholder);
}

/* 右侧编辑区（命令行控制台） */
.sql-console-area {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}
.console-toolbar {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 12px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  background: var(--el-bg-color-overlay);
  flex-shrink: 0;
}
.console-prompt {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 13px;
  font-weight: 500;
  color: var(--el-color-success);
  margin-right: auto;
}
/* 输出区 */
.console-output {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 8px 12px;
  background: var(--el-bg-color);
  font-family: "Consolas", "Cascadia Code", "JetBrains Mono", monospace;
  font-size: 13px;
}
.console-empty {
  color: var(--el-text-color-placeholder);
  text-align: center;
  padding: 32px 0;
}
.entry {
  margin-bottom: 8px;
}
.entry-sql {
  display: flex;
  align-items: center;
  gap: 6px;
  color: var(--el-text-color-primary);
}
.entry-prompt {
  color: var(--el-color-success);
  flex-shrink: 0;
}
.entry-sql-text {
  white-space: pre-wrap;
  word-break: break-all;
}
.entry-spin {
  color: var(--el-color-primary);
}
.entry-table {
  margin: 4px 0 8px 16px;
}
.entry-table :deep(.result-table) {
  max-height: 400px;
}
.entry-meta {
  font-size: 11px;
  color: var(--el-text-color-secondary);
  margin-top: 2px;
}
.entry-ok {
  color: var(--el-color-success);
  padding-left: 16px;
}
.entry-error {
  color: var(--el-color-danger);
  padding-left: 16px;
  white-space: pre-wrap;
}
.entry-info {
  color: var(--el-text-color-secondary);
  font-style: italic;
}
/* 当前输入区（在输出流末尾，mysql CLI 风格） */
.console-input-wrap {
  display: flex;
  align-items: stretch;
  border-top: 1px solid var(--el-border-color-lighter);
  background: var(--el-bg-color-overlay);
  flex-shrink: 0;
  min-height: 36px;
  max-height: 160px;
}
.input-prompt {
  display: flex;
  align-items: center;
  padding: 0 8px;
  color: var(--el-color-success);
  font-family: "Consolas", monospace;
  font-size: 13px;
  flex-shrink: 0;
}
.console-input {
  flex: 1;
  min-width: 0;
  overflow: auto;
}
/* 命令行原生输入（mysql CLI 风格） */
.cli-input {
  flex: 1;
  min-width: 0;
  border: none;
  outline: none;
  background: transparent;
  color: var(--el-text-color-primary);
  font-family: "Consolas", "Cascadia Code", "JetBrains Mono", monospace;
  font-size: 13px;
  line-height: 1.6;
  padding: 8px 0;
  resize: none;
  overflow: hidden;
  white-space: pre;
}
.console-input :deep(.cm-editor) {
  height: 100%;
  min-height: 36px;
  max-height: 160px;
}
.console-input :deep(.cm-scroller) {
  font-family: "Consolas", "Cascadia Code", monospace;
  font-size: 13px;
  line-height: 1.5;
}
.console-input :deep(.cm-gutters) {
  display: none;
}
.console-input :deep(.cm-content) {
  padding: 8px 0;
}
/* 历史抽屉 */
.history-drawer {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
/* 代码模式（多行编辑器 + 结果区） */
.code-editor-toolbar {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  background: var(--el-bg-color-overlay);
  flex-shrink: 0;
}
.code-editor {
  height: 220px;
  overflow: auto;
  border-bottom: 1px solid var(--el-border-color-lighter);
  background: var(--el-bg-color);
  font-family: "Consolas", "Cascadia Code", monospace;
  font-size: 13px;
  flex-shrink: 0;
}
.code-editor :deep(.cm-editor) {
  height: 100%;
}
.code-editor :deep(.cm-scroller) {
  font-family: inherit;
}
.code-result {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
}
.code-result-header {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 6px 12px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  background: var(--el-bg-color-overlay);
  flex-shrink: 0;
  font-size: 13px;
  min-height: 32px;
}
.code-result-header .result-actions {
  margin-left: auto;
  display: flex;
  gap: 4px;
}
.code-result-body {
  flex: 1;
  min-height: 0;
  overflow: hidden;
  display: flex;
  flex-direction: column;
  padding: 8px;
}
</style>
