<!--
  SqlConsoleView.vue — SQL 控制台（MySQL / PostgreSQL，多标签）

  功能：
  - 多标签页（参考终端）：每个标签 = 一个独立连接 + 绑定一个库（schema）。
    标签标题 = 库名（未绑定库时为 profile 名），支持关闭/关闭其他/拖拽排序。
  - 左侧表树：点实例节点 → 打开/激活该 profile 的标签；点库节点 → 打开/激活
    该 (profile, 库) 的标签并自动 USE（之后 SQL 无需带库前缀）；点表节点 →
    填入 `SELECT * FROM `表名` LIMIT 100;`（当前库表不带前缀）。
  - 只读 / 读写模式切换（默认只读，写操作需切到读写模式）。
  - SQL 编辑器（CodeMirror，Ctrl+Enter 执行）：执行 / 清空 / AI 优化 / AI 解释。
  - 结果区：动态列 el-table，显示行数、耗时、影响行数；出错显示 error。
  - 每个标签独立订阅 db:query_result 事件，按 queryId 匹配各自等待的查询。
-->
<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch, type Ref } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import {
  dbDeleteProfile,
  dbDefaultTableQuery,
  dbListDatabases,
  dbListProfiles,
  dbListTables,
  dbListGroups,
  dbSaveGroup,
  dbDeleteGroup,
  type DraggedTable,
} from "@/api/db";
import type { DbGroup, DbKind, DbProfile, QueryResult, AiSqlResultEvent } from "@/api/types";
import { listen } from "@tauri-apps/api/event";
import DbProfileDialog from "@/components/DbProfileDialog.vue";
import AiPanel from "@/components/AiPanel.vue";
import { useAiDbStore } from "@/stores/ai";
import { useDbStore, type DbTab } from "@/stores/db";
import { useSettingsStore } from "@/stores/settings";
import { useCodeMirror } from "@/composables/useCodeMirror";
import ResultTableV2 from "@/components/ResultTableV2.vue";
import { stripLeadingComments, useSqlConsole } from "@/composables/useSqlConsole";

// KeepAlive 按 name 匹配缓存本组件（保留 DB 助手面板状态）。
defineOptions({ name: "SqlConsoleView" });

// 用 DB 域 store：aiOptimize/aiExplain 的结果会进右侧 DB 助手面板，与终端助手隔离。
const ai = useAiDbStore();
const db = useDbStore();
const settings = useSettingsStore();
const isDark = computed(() => settings.terminal.theme === "dark");

// --- AI 系统提示词 ----------------------------------------------------------
// 按活动标签连接的数据库类型切换措辞（MySQL / PostgreSQL 方言差异大）。

/** profile.kind 是否为 PostgreSQL（容忍 postgresql/pg 别名）。 */
function isPostgres(kind?: string | null): boolean {
  const k = (kind ?? "").toLowerCase();
  return k === "postgres" || k === "postgresql" || k === "pg";
}

// kind 由标签携带（openTab 时从 profile 归一化），不依赖 profiles 列表加载时序。
const activeKind = computed<DbKind>(() => db.activeKind);

const dbaTitle = computed(() =>
  activeKind.value === "postgres" ? "资深 PostgreSQL DBA" : "资深 MySQL DBA"
);
const dbDialectLabel = computed(() =>
  activeKind.value === "postgres" ? "PostgreSQL SQL" : "MySQL SQL"
);

/** 命令行模式提示符（按数据库类型区分）。 */
const cliPrompt = computed(() => (activeKind.value === "postgres" ? "postgres>" : "mysql>"));

/** 树中隐藏的系统库（按数据库类型）。 */
const SYSTEM_DATABASES: Record<"mysql" | "postgres", string[]> = {
  mysql: ["information_schema", "performance_schema", "mysql", "sys"],
  postgres: ["postgres", "template0", "template1"],
};

function systemDatabasesOf(profile: DbProfile): string[] {
  return isPostgres(profile.kind) ? SYSTEM_DATABASES.postgres : SYSTEM_DATABASES.mysql;
}

/** 按数据库类型生成标识符限定名（MySQL 反引号 / PG 双引号；段间点分隔）。 */
function quoteTableForKind(kind: string | null | undefined, table: string): string {
  const q = isPostgres(kind) ? (s: string) => `"${s}"` : (s: string) => `\`${s}\``;
  return table.split(".").map(q).join(".");
}

const SYSTEM_DIAGNOSE = computed(
  () =>
    `你是一名${dbaTitle.value}。用户会提供一段 SQL，请给出优化建议：` +
    "1) 指出潜在的性能问题（缺索引、全表扫描、N+1、回表等）；" +
    "2) 给出优化后的 SQL（用 ```sql 代码块）；" +
    "3) 必要时建议索引（CREATE INDEX）。简洁、专业、中文。"
);

const SYSTEM_EXPLAIN = computed(
  () =>
    `你是一名${dbaTitle.value}。用户会提供一段 SQL，请用通俗简洁的中文解释：` +
    "1) 这段 SQL 做了什么；" +
    "2) 涉及的关键字/函数/子查询含义；" +
    "3) 可能的注意事项。不要重复 SQL 原文。"
);

// --- profile 列表 -----------------------------------------------------------
const profiles = ref<DbProfile[]>([]);
const dbGroups = ref<DbGroup[]>([]);
const selectedProfileId = ref<string | null>(null);
const loadingProfiles = ref(false);

const selectedProfile = computed(
  () => profiles.value.find((p) => p.id === selectedProfileId.value) ?? null
);

async function loadProfiles() {
  loadingProfiles.value = true;
  try {
    [profiles.value, dbGroups.value] = await Promise.all([
      dbListProfiles(),
      dbListGroups(),
    ]);
    // 同步树根：分组 + 实例层。
    treeData.value = rebuildInstanceNodes();
    // 树重建后按活动标签重新对齐（展开实例 / 高亮当前库）。
    syncTreeToActiveTab();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    ElMessage.error("加载数据库连接失败：" + msg);
  } finally {
    loadingProfiles.value = false;
  }
}

// --- DB 分组管理 ---------------------------------------------------------------
async function createDbGroup() {
  let name: string;
  try {
    const res = await ElMessageBox.prompt("请输入分组名称", "新建分组", {
      confirmButtonText: "创建",
      cancelButtonText: "取消",
      inputValidator: (v) => !!v?.trim() || "名称不能为空",
    });
    name = res.value;
  } catch {
    return;
  }
  try {
    await dbSaveGroup({
      id: crypto.randomUUID(),
      name: name.trim(),
      parentId: null,
      sortOrder: 0,
      createdAt: new Date().toISOString(),
    });
    await loadProfiles();
    ElMessage.success("已创建分组");
  } catch (e) {
    ElMessage.error("创建分组失败: " + String(e));
  }
}

async function renameDbGroup(g: DbGroup) {
  let name: string;
  try {
    const res = await ElMessageBox.prompt("请输入新的分组名称", "重命名分组", {
      inputValue: g.name,
      confirmButtonText: "保存",
      cancelButtonText: "取消",
      inputValidator: (v) => !!v?.trim() || "名称不能为空",
    });
    name = res.value;
  } catch {
    return;
  }
  try {
    await dbSaveGroup({ ...g, name: name.trim() });
    await loadProfiles();
    ElMessage.success("已重命名");
  } catch (e) {
    ElMessage.error("重命名失败: " + String(e));
  }
}

async function deleteDbGroup(g: DbGroup) {
  const hasChild = profiles.value.some((p) => p.groupId === g.id);
  try {
    await ElMessageBox.confirm(
      hasChild
        ? `分组 "${g.name}" 下仍有连接，删除分组后连接将变为无分组。继续？`
        : `确定删除分组 "${g.name}" 吗？`,
      "删除分组",
      { type: "warning", confirmButtonText: "删除", cancelButtonText: "取消" }
    );
  } catch {
    return;
  }
  try {
    await dbDeleteGroup(g.id);
    await loadProfiles();
    ElMessage.success("已删除");
  } catch (e) {
    ElMessage.error("删除失败: " + String(e));
  }
}

/** 树节点右键/下拉菜单命令。 */
function onTreeCommand(cmd: string, data: TreeNode) {
  if (data.type === "instance") {
    const profile = profiles.value.find((p) => p.id === data.value);
    if (!profile) return;
    switch (cmd) {
      case "edit":
        selectedProfileId.value = profile.id;
        openEditProfile();
        break;
      case "delete":
        deleteProfileById(profile);
        break;
    }
  } else if (data.type === "group") {
    const g = dbGroups.value.find((x) => x.id === data.value);
    if (!g) return;
    switch (cmd) {
      case "newChild":
        openCreateProfile(g.id);
        break;
      case "rename":
        renameDbGroup(g);
        break;
      case "delete":
        deleteDbGroup(g);
        break;
    }
  }
}

async function deleteProfileById(p: DbProfile) {
  try {
    await ElMessageBox.confirm(`确定删除连接 "${p.name}" 吗？`, "删除连接", {
      type: "warning",
      confirmButtonText: "删除",
      cancelButtonText: "取消",
    });
  } catch {
    return;
  }
  try {
    await dbDeleteProfile(p.id);
    await loadProfiles();
    ElMessage.success("已删除");
  } catch (e) {
    ElMessage.error("删除失败: " + String(e));
  }
}

// --- 表列表 -----------------------------------------------------------------
// --- 左侧导航树（分组 > 实例 > 库 > 表）-------------------------------------------
interface TreeNode {
  type: "group" | "instance" | "database" | "table";
  /** 唯一 key。 */
  key: string;
  /** 显示文本。 */
  label: string;
  /** 分组：group id；实例：profile id；库：库名；表：表名。 */
  value: string;
  /** 表所属的库名（仅 table 节点用）。 */
  database?: string;
  /** 库/表节点所属的 profile id（用于打开对应标签）。 */
  profileId?: string;
  /** 是否已加载子节点（懒加载标记）。 */
  loaded?: boolean;
  /** 子节点。 */
  children?: TreeNode[];
  /** 是否叶子（表是叶子）。 */
  isLeaf?: boolean;
  /** 表的字段名列表（点表时预拉，供 CodeMirror 字段级自动补全）。 */
  columns?: string[];
}

/** 树根：分组 + 实例节点。 */
const treeData = ref<TreeNode[]>([]);
const treeRef = ref<any>(null);

/** 树懒加载缓存：实例层子节点（库）按 profileId，库层子节点（表）按 profileId:库名。
 *
 * 缓存的是 promise 而非结果：用户点击与 syncTreeToActiveTab 的 node.expand()
 * 会并发触发同一节点的 load，缓存 promise 让并发请求合并为一次后端调用。
 */
const treeCache = new Map<string, Promise<TreeNode[]>>();
function cachedOrLoad(key: string, loader: () => Promise<TreeNode[]>): Promise<TreeNode[]> {
  const hit = treeCache.get(key);
  if (hit) return hit;
  const p = loader().catch((e: unknown) => {
    // 失败不缓存：下次展开重新加载（缓存了失败，展开会永远打不开）。
    treeCache.delete(key);
    throw e;
  });
  treeCache.set(key, p);
  return p;
}

/**
 * 连接拓扑变化时重建整树（`:key` 强制重挂载）+ 清掉对应 profile 的懒加载缓存。
 *
 * el-tree 节点一旦 loaded 就不再重新加载（resolve 空数组也会置 loaded）。
 * 断线重连后服务器上的库/表可能已变化，不重建就会一直显示旧数据（展开后
 * 点不存在的库报错）。只对「该 profile 连接数从 1+ 降到 0」触发（断开/全部
 * 关闭标签）：正常开/关标签（1→2、2→1）不重建，避免打断用户展开状态。
 * 0→1（首次连接/重连）不再重建——首次连接时该 profile 的树本就无数据可刷新，
 * 断线重连的刷新已在 1→0 时完成（重建 + 清缓存）；再重建只会打断其他
 * profile 的展开状态。
 */
const treeVersion = ref(0);
let prevConnCounts = new Map<string, number>();
let connCountsFirst = true;
watch(
  () => db.tabs.map((t) => ({ pid: t.profileId, conn: !!t.connId })),
  (list) => {
    const counts = new Map<string, number>();
    for (const { pid, conn } of list) {
      counts.set(pid, (counts.get(pid) ?? 0) + (conn ? 1 : 0));
    }
    if (connCountsFirst) {
      // 首帧只建立基线（keepalive 下可能已有历史标签），不触发重建。
      connCountsFirst = false;
      prevConnCounts = counts;
      return;
    }
    // 同时遍历旧/新两侧的 profile：标签被全部关闭时 pid 会从 counts 中消失，
    // 只遍历 counts 会漏掉 1→0（缓存不失效、树不重建，残留的库/表节点之后
    // 一点又触发 0→1 重建，打断操作）。
    const pids = new Set<string>([...prevConnCounts.keys(), ...counts.keys()]);
    let changed = false;
    for (const pid of pids) {
      const prev = prevConnCounts.get(pid) ?? 0;
      const cnt = counts.get(pid) ?? 0;
      if (prev > 0 && cnt === 0) {
        changed = true;
        // 该 profile 的库/表列表全部失效（重连后数据可能已变化）。
        for (const key of [...treeCache.keys()]) {
          if (key.startsWith(`dbs:${pid}`) || key.startsWith(`tables:${pid}:`)) {
            treeCache.delete(key);
          }
        }
      }
    }
    if (changed) treeVersion.value++;
    prevConnCounts = counts;
  },
);

/** 重建树根：分组节点 + 未分组实例节点。 */
function rebuildInstanceNodes(): TreeNode[] {
  const roots: TreeNode[] = [];
  const groupMap = new Map<string, TreeNode>();

  // 构建分组节点。
  for (const g of dbGroups.value) {
    const node: TreeNode = {
      type: "group",
      key: `grp-${g.id}`,
      label: g.name,
      value: g.id,
      isLeaf: false,
      children: [],
    };
    groupMap.set(g.id, node);
  }
  // 挂载子分组。
  for (const g of dbGroups.value) {
    const node = groupMap.get(g.id)!;
    if (g.parentId && groupMap.has(g.parentId)) {
      groupMap.get(g.parentId)!.children!.push(node);
    } else {
      roots.push(node);
    }
  }

  // 把实例挂到对应分组或根。
  for (const p of profiles.value) {
    const inst: TreeNode = {
      type: "instance",
      key: `inst-${p.id}`,
      label: p.name,
      value: p.id,
      isLeaf: false,
      children: [],
    };
    if (p.groupId && groupMap.has(p.groupId)) {
      groupMap.get(p.groupId)!.children!.push(inst);
    } else {
      roots.push(inst);
    }
  }

  // 排序：分组在前，实例在后；同类按名称。
  roots.sort((a, b) => {
    if (a.type !== b.type) return a.type === "group" ? -1 : 1;
    return a.label.localeCompare(b.label);
  });
  return roots;
}

/** 懒加载子节点（el-tree 的 load 回调）。
 *
 * 失败路径必须用第三个参数 `reject()`：它只结束 loading、不把节点标记为
 * "已加载"，下次展开会重新 load。若用 `resolve([])`，el-tree 会把空结果
 * 永久缓存（`loaded=true`），之后展开永远不再触发 load——表现为"展开没反应"。
 */
async function loadTreeNode(node: any, resolve: (children: TreeNode[]) => void, reject?: () => void) {
  const fail = () => {
    if (reject) reject();
    else resolve([]);
  };
  const data: TreeNode = node.data ?? node;
  // 根节点：el-tree 在 lazy 模式下初始化/重挂载（:key 变化）时会调用根节点的
  // load 回调（TreeStore.initialize 里 loadFn(this.root, ...)），根节点的 data
  // 是 treeData 数组本身、没有 .type。必须直接 resolve 树根数据，否则根节点
  // 被 resolve([]) 置空、整棵树显示 "No Data"（此前 :key 重挂载后必现）。
  if (Array.isArray(data)) {
    resolve(data as TreeNode[]);
    return;
  }
  if (data.type === "group") {
    // 分组节点的子节点（实例）已在 rebuildInstanceNodes 中静态构建。
    resolve(data.children ?? []);
    return;
  }
  if (data.type === "instance") {
    // 展开实例 → 打开/激活该 profile 的标签（无则连接），保证有可用连接列库。
    const profile = profiles.value.find((p) => p.id === data.value);
    if (!profile) {
      fail();
      return;
    }
    try {
      await openTabForProfile(profile);
      // 列库必须用「该 profile 自己的连接」，不能拿活动标签的连接：连接是
      // 异步的，等待期间活动标签可能被切走（用户点了别的标签 / 并发展开两个
      // 实例时另一个先连上把 activeTabId 顶掉），拿活动标签连接要么张冠李戴、
      // 要么因 profileId 不匹配直接放弃——表现为"展开转一下又收起、没反应"。
      const connId = await ensureProfileConn(profile.id);
      if (!connId) {
        // 连接失败：openTabForProfile 已吞掉错误并弹提示。
        fail();
        return;
      }
      const nodes = await cachedOrLoad(`dbs:${profile.id}`, async () => {
        const dbs = await dbListDatabases(connId);
        return dbs
          .filter((d) => !systemDatabasesOf(profile).includes(d))
          .map((d) => ({
            type: "database" as const,
            key: `db-${profile.id}-${d}`,
            label: d,
            value: d,
            profileId: profile.id,
            isLeaf: false,
            children: [],
          }));
      });
      resolve(nodes);
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      ElMessage.error("连接失败：" + msg);
      fail();
    }
    return;
  }
  if (data.type === "database") {
    // 展开库 → 列出表。不能用"活动标签的连接"：活动标签可能正连接中
    // （connId 为 null，点库节点开标签时必现）或属于别的库；改用同 profile
    // 任意已连接标签的连接——同一服务器的连接都能 SHOW TABLES FROM `库`。
    const connId = await ensureProfileConn(data.profileId ?? "");
    if (!connId) {
      fail();
      return;
    }
    try {
      const nodes = await cachedOrLoad(`tables:${data.profileId}:${data.value}`, async () => {
        const tables = await dbListTables(connId, data.value);
        return tables.map((t) => ({
          type: "table" as const,
          key: `tbl-${data.profileId}-${data.value}-${t}`,
          label: t,
          value: t,
          database: data.value,
          profileId: data.profileId,
          isLeaf: true,
        }));
      });
      resolve(nodes);
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      ElMessage.error(`加载表列表失败：${msg}`);
      fail();
    }
    return;
  }
  resolve([]);
}

/** 拖拽表节点：把表信息写入 dataTransfer，供 AI 面板/SQL 编辑器接收。 */
function onTableDragStart(e: DragEvent, data: TreeNode) {
  if (data.type !== "table") return;
  // 用「表所属 profile 的连接」而非活动标签连接：表可能属于另一台服务器
  // （展开 A 实例后切到 B 标签再拖 A 的表），否则 AiPanel 会用当前活动连接
  // 去查错服务器/错库。
  const tab = db.tabs.find((t) => t.profileId === data.profileId && t.connId);
  if (!tab?.connId) return;
  const payload: DraggedTable = {
    connId: tab.connId,
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

/** 打开/激活某 profile 的标签（失败弹提示；返回是否成功，供调用方决定后续动作）。 */
async function openTabForProfile(profile: DbProfile): Promise<boolean> {
  try {
    await db.openTab(profile, null);
    await focusActiveInput();
    return true;
  } catch (e: unknown) {
    ElMessage.error("连接失败：" + String(e));
    return false;
  }
}

/** 打开/激活某 (profile, 库) 的标签并自动 USE（失败弹提示；返回是否成功）。 */
async function openTabForDatabase(profile: DbProfile, database: string): Promise<boolean> {
  try {
    await db.openTab(profile, database);
    await focusActiveInput();
    return true;
  } catch (e: unknown) {
    ElMessage.error("连接失败：" + String(e));
    return false;
  }
}

/**
 * 找指定 profile 的可用连接 id：同 profile 已连接标签优先；没有则打开实例标签
 * 等连接完成。树节点（库/表）可能属于非活动 profile——操作（列表/DESCRIBE/拖拽）
 * 必须落在表自己的连接上，否则会用活动连接查错服务器/错库。
 */
async function ensureProfileConn(profileId: string): Promise<string | null> {
  let tab = db.tabs.find((t) => t.profileId === profileId && t.connId);
  if (!tab) {
    const profile = profiles.value.find((p) => p.id === profileId);
    if (!profile) return null;
    await openTabForProfile(profile);
    tab = db.tabs.find((t) => t.profileId === profileId && t.connId);
  }
  return tab?.connId ?? null;
}

/** 连接完成后聚焦输入框（仅命令行模式）。 */
async function focusActiveInput() {
  await nextTick();
  const s = activeState.value;
  if (s && s.editorMode.value === "console") s.cliInputRef.value?.focus();
}

/** 点击树节点。 */
async function onTreeNodeClick(data: TreeNode) {
  if (data.type === "group") {
    return;
  }
  if (data.type === "instance") {
    // 点实例：打开/激活该 profile 的标签（展开由 el-tree 处理）。
    const profile = profiles.value.find((p) => p.id === data.value);
    if (profile) await openTabForProfile(profile);
    return;
  }
  if (data.type === "database") {
    // 点库：打开/激活该库的标签并自动 USE——每个 SQL 执行界面绑定一个库，
    // 之后该标签里的 SQL 无需带库前缀（如 `SELECT * FROM `表名``）。
    const profile = profiles.value.find((p) => p.id === data.profileId);
    if (!profile) return;
    await openTabForDatabase(profile, data.value);
    return;
  }
  if (data.type === "table") {
    // 点表：填入 SELECT 模板 + 记录表名 + 预拉表结构（供补全，但不弹对话框）。
    // 与点库语义一致：先确保活动标签是「表所属 profile + 库」的标签。
    // 注意不能只比库名——两台服务器都有同名库时，`sameDb` 会误判为同库，
    // 无前缀模板填进另一台服务器的标签，执行时静默查到另一张同名表。
    // 跨服务器（profileId 不同）时限定名也救不了，必须切到表所属标签。
    const active = db.activeTab;
    const dbName = data.database ?? "";
    const profile = profiles.value.find((p) => p.id === data.profileId);
    const sameTab =
      !!dbName &&
      !!active &&
      active.profileId === data.profileId &&
      active.database === dbName;
    if (!sameTab && profile && dbName) {
      // 切到表所属库的标签（连接成功时活动标签已切换、自动 USE）。
      const ok = await openTabForDatabase(profile, dbName);
      if (!ok) return; // 连接失败已弹提示；不填模板，避免执行落在错连接上
    }
    // 活动标签即表所属库标签（已自动 USE）→ 用不带前缀的表名。
    // SELECT 模板由后端按方言生成（浏览模式）：MySQL/SQLite 反引号、PG 双引号
    // ——前端不再手拼（旧实现对 PG 也用反引号，是已知小瑕疵）。
    let selectTpl = "";
    try {
      const tplConnId = db.activeTab?.connId ?? "";
      selectTpl =
        tplConnId && data.database !== undefined
          ? await dbDefaultTableQuery(tplConnId, data.value, 100, 0)
          : `SELECT * FROM \`${data.value}\` LIMIT 100;`;
    } catch {
      selectTpl = `SELECT * FROM \`${data.value}\` LIMIT 100;`;
    }
    const s = activeState.value;
    if (s) {
      if (s.editorMode.value === "console") {
        s.cliInput.value = selectTpl;
        s.cliInputRef.value?.focus();
      } else {
        const v = getSqlView();
        if (v) {
          v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: selectTpl } });
          v.focus();
        } else {
          s.sqlText.value = selectTpl;
        }
      }
    }
    // 预拉表结构（不弹层）——MySQL 的 DESCRIBE 需 `db`.`table` 限定（同 profile
    // 开多个库标签时连接当前库未必是表所在库）；PG 的表名已带 schema 前缀，
    // 库由连接层决定，不拼接库名。
    const connId = db.activeTab?.connId ?? (await ensureProfileConn(data.profileId ?? ""));
    const descDb = isPostgres(profile?.kind) ? undefined : dbName;
    const desc =
      connId && s ? await s.console.loadStructure(data.value, connId, descDb) : null;
    if (desc) {
      const cols = desc.rows.map((r) => r[0]).filter(Boolean);
      data.columns = cols;
    }
    return;
  }
}

/** 树与活动标签对齐：展开活动 profile 的实例节点、高亮当前库节点。 */
function syncTreeToActiveTab() {
  const tab = db.activeTab;
  const tree = treeRef.value;
  if (!tab || !tree) return;
  const instKey = `inst-${tab.profileId}`;
  const node = tree.store?.nodesMap?.[instKey];
  if (node && !node.expanded) node.expand();
  if (tab.database) {
    tree.setCurrentKey(`db-${tab.profileId}-${tab.database}`);
  } else {
    tree.setCurrentKey(instKey);
  }
}

// 切标签时：树对齐 + 重挂 CodeMirror（内容随活动标签变化）。
watch(() => db.activeTabId, () => {
  syncTreeToActiveTab();
  remountSqlEditor();
});
// 活动标签的绑定库变化（点库/拖表/执行 USE）时同步树高亮。
watch(() => db.activeDatabase, () => syncTreeToActiveTab());

// --- 多标签：每个标签一个独立执行界面状态 --------------------------------------
interface TabState {
  tabId: string;
  /** SQL 文本（代码模式 CodeMirror 内容）。 */
  sqlText: Ref<string>;
  /** 只读模式。 */
  readOnly: Ref<boolean>;
  /** console=命令行模式；code=代码模式。 */
  editorMode: Ref<"console" | "code">;
  /** 命令行模式输入框内容。 */
  cliInput: Ref<string>;
  cliInputRef: Ref<HTMLTextAreaElement | null>;
  /** 表结构弹层。 */
  structureVisible: Ref<boolean>;
  /** 历史抽屉。 */
  historyDrawer: Ref<boolean>;
  /** 该标签的控制台实例（订阅 db:query_result，按 queryId 匹配）。 */
  console: ReturnType<typeof useSqlConsole>;
}

const tabStates = new Map<string, TabState>();

function confirmDangerous(kw: string, noWhere: boolean): Promise<boolean> {
  return ElMessageBox.confirm(
    `检测到危险操作：${kw}${noWhere ? "（DELETE 无 WHERE）" : ""}。确认继续吗？`,
    "危险操作确认",
    { type: "warning", confirmButtonText: "确认执行", cancelButtonText: "取消", confirmButtonClass: "el-button--danger" },
  )
    .then(() => true)
    .catch(() => false);
}

/** 为标签创建独立状态（连接 id 随标签走，标签关闭时 destroy）。 */
function createTabState(tabId: string): TabState {
  const connIdRef = computed<string | null>(
    () => db.tabs.find((t) => t.id === tabId)?.connId ?? null,
  );
  const sqlText = ref("");
  const readOnly = ref(true); // 默认只读
  const editorMode = ref<"console" | "code">("console");
  const cliInput = ref("");
  const cliInputRef = ref<HTMLTextAreaElement | null>(null);
  const structureVisible = ref(false);
  const historyDrawer = ref(false);
  const console = useSqlConsole(connIdRef, sqlText, confirmDangerous);
  void console.setup();
  console.loadHistory();
  return {
    tabId,
    sqlText,
    readOnly,
    editorMode,
    cliInput,
    cliInputRef,
    structureVisible,
    historyDrawer,
    console,
  };
}

// 标签增删时同步创建/销毁状态（keepalive 下的 SQL 页生命周期只跟组件走，
// 因此标签状态必须随 db.tabs 增删管理，不能依赖组件卸载）。
watch(
  () => db.tabs.map((t) => t.id),
  (ids) => {
    const known = new Set(ids);
    for (const id of ids) {
      if (!tabStates.has(id)) tabStates.set(id, createTabState(id));
    }
    for (const [id, st] of [...tabStates]) {
      if (!known.has(id)) {
        st.console.destroy();
        tabStates.delete(id);
      }
    }
  },
  { immediate: true },
);

/** 活动标签状态（仅活动标签的编辑区渲染，其余标签状态保留在内存）。 */
const activeState = computed<TabState | null>(
  () => (db.activeTabId ? tabStates.get(db.activeTabId) ?? null : null),
);

// 活动标签的派生可写别名（供 v-model 使用）。
const readOnlyModel = computed({
  get: () => activeState.value?.readOnly.value ?? true,
  set: (v: boolean) => {
    if (activeState.value) activeState.value.readOnly.value = v;
  },
});
const editorModeModel = computed({
  get: () => activeState.value?.editorMode.value ?? "console",
  set: (v: "console" | "code") => {
    if (activeState.value) activeState.value.editorMode.value = v;
  },
});
const cliInputModel = computed({
  get: () => activeState.value?.cliInput.value ?? "",
  set: (v: string) => {
    if (activeState.value) activeState.value.cliInput.value = v;
  },
});
const historyDrawerModel = computed({
  get: () => activeState.value?.historyDrawer.value ?? false,
  set: (v: boolean) => {
    if (activeState.value) activeState.value.historyDrawer.value = v;
  },
});
const structureVisibleModel = computed({
  get: () => activeState.value?.structureVisible.value ?? false,
  set: (v: boolean) => {
    if (activeState.value) activeState.value.structureVisible.value = v;
  },
});

// 活动标签的派生只读值（模板直接消费）。
const activeLastResult = computed(() => activeState.value?.console.lastResult.value ?? null);
const activeExecuting = computed(() => activeState.value?.console.executing.value ?? false);
const activeHistory = computed(() => activeState.value?.console.history.value ?? []);
const activeSelectedTable = computed(
  () => activeState.value?.console.selectedTable.value ?? null,
);
const activeDescribeResult = computed(
  () => activeState.value?.console.describeResult.value ?? null,
);
const activeResultIsSelect = computed(() => {
  const r = activeLastResult.value;
  return !!r && r.columns.length > 0;
});

// --- 代码模式结果区高度测量（el-table-v2 需要确定 px 高度） ---
// 结果区是 flex 子项高度自适应，ResizeObserver 跟随容器实际高度喂给虚拟表。
// 结果区仅在代码模式渲染，DOM 晚于 onMounted 出现 → watch 挂 ref 后再观察。
const codeResultBodyRef = ref<HTMLElement | null>(null);
const resultBodyHeight = ref(240);
let resultBodyObs: ResizeObserver | null = null;
watch(codeResultBodyRef, (el) => {
  resultBodyObs?.disconnect();
  if (!el) return;
  resultBodyObs = new ResizeObserver((entries) => {
    for (const en of entries) {
      resultBodyHeight.value = Math.max(120, Math.floor(en.contentRect.height));
    }
  });
  resultBodyObs.observe(el);
});
onBeforeUnmount(() => {
  resultBodyObs?.disconnect();
  resultBodyObs = null;
});

// --- 命令行模式输出区高度（结果表自适应基准） ---
// 表格高度不再固定 400px：内容少时贴合实际行数（不留大片空白），
// 内容多时占满输出区可视高度（扣除语句行/元信息/输入框的预留空间）。
const consoleOutputHeight = ref(400);
let consoleOutputObs: ResizeObserver | null = null;
watch(
  () => activeState.value?.console.scrollRef.value ?? null,
  (el) => {
    consoleOutputObs?.disconnect();
    if (!el) return;
    consoleOutputHeight.value = Math.max(160, el.clientHeight);
    consoleOutputObs = new ResizeObserver((entries) => {
      for (const en of entries) {
        consoleOutputHeight.value = Math.max(160, Math.floor(en.contentRect.height));
      }
    });
    consoleOutputObs.observe(el);
  },
);
onBeforeUnmount(() => {
  consoleOutputObs?.disconnect();
  consoleOutputObs = null;
});

/** 命令行模式结果表自适应高度：min(表头+行数×行高, 输出区可视高度-预留)。 */
function consoleTableHeight(rowCount: number): number {
  const content = 36 + rowCount * 34 + 2; // 表头 36 + 数据行 34/行 + 滚动条余量
  const avail = Math.max(120, consoleOutputHeight.value - 105);
  return Math.max(70, Math.min(content, avail));
}

// --- 标签管理（横向标签栏） ----------------------------------------------
/** 标签标题：库名（未绑定库时为 profile 名）。 */
function tabTitle(tab: DbTab) {
  return tab.database ?? tab.profileName;
}

/** 标签区 DOM 引用（滚动控制用）。 */
const dbTabsRef = ref<HTMLElement | null>(null);

/** 标签区滚轮：纵向滚动转为横向滚动。 */
function onTabsWheel(e: WheelEvent) {
  const el = e.currentTarget as HTMLElement;
  el.scrollLeft += e.deltaY;
}

/** 把激活标签滚入视野（标签超出可视区时自动定位，VS Code 溢出策略）。
    scrollIntoView({ inline: "nearest" }) 只在标签不可见时滚动，无跳动。 */
watch(
  () => db.activeTabId,
  async () => {
    await nextTick();
    const wrap = dbTabsRef.value;
    if (!wrap) return;
    const active = wrap.querySelector(".db-tab.active");
    active?.scrollIntoView({ block: "nearest", inline: "nearest", behavior: "smooth" });
  },
);

/** "+" 下拉：选 profile 开新标签 / 新建连接。 */
async function onAddTabCommand(command: string | number | object) {
  if (command === "__new_profile__") {
    openCreateProfile();
    return;
  }
  const profile = profiles.value.find((p) => p.id === String(command));
  if (profile) await openTabForProfile(profile);
}

// --- 标签右键菜单（关闭/关闭其他/关闭全部） --------------------------------

const tabMenu = ref<{ x: number; y: number; tab: DbTab | null }>({ x: 0, y: 0, tab: null });

function openTabMenu(tab: DbTab, e: MouseEvent) {
  tabMenu.value = { x: e.clientX, y: e.clientY, tab };
}

function closeTabMenu() {
  tabMenu.value.tab = null;
}

function onTabMenuCommand(cmd: "close" | "closeOthers" | "closeAll") {
  const t = tabMenu.value.tab;
  closeTabMenu();
  if (!t) return;
  if (cmd === "close") {
    void db.closeTab(t.id);
  } else if (cmd === "closeOthers") {
    for (const x of [...db.tabs]) {
      if (x.id !== t.id) void db.closeTab(x.id);
    }
  } else {
    for (const x of [...db.tabs]) void db.closeTab(x.id);
  }
}

/** 工具栏"断开"：关闭当前标签。 */
async function closeActiveTab() {
  if (db.activeTabId) await db.closeTab(db.activeTabId);
}

// 左侧数据库树宽度（可拖拽调整）与收起状态（收起后编辑区占满全宽）。
const sidebarWidth = ref(200);
const treeCollapsed = ref(false);
function startResize(e: MouseEvent) {
  e.preventDefault();
  const startX = e.clientX;
  const startW = sidebarWidth.value;
  const onMove = (ev: MouseEvent) => {
    const w = startW + (ev.clientX - startX);
    sidebarWidth.value = Math.max(140, Math.min(480, w));
  };
  const cleanup = () => {
    document.removeEventListener("mousemove", onMove);
    document.removeEventListener("mouseup", onUp);
    window.removeEventListener("blur", onBlur);
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
  };
  const onUp = () => cleanup();
  // 鼠标在窗口外释放（拖出窗口边缘 / Alt-Tab 切走）时 mouseup 不触发，
  // 监听器与 col-resize 光标、userSelect:none 会永久残留——用 window blur 兜底清理。
  const onBlur = () => cleanup();
  document.body.style.cursor = "col-resize";
  document.body.style.userSelect = "none";
  document.addEventListener("mousemove", onMove);
  document.addEventListener("mouseup", onUp);
  window.addEventListener("blur", onBlur);
}

// --- SQL 编辑器（CodeMirror）----------------------------------------------
const sqlEditorRef = ref<HTMLElement | null>(null);
// 共享一个 CodeMirror 实例：model 委托到活动标签的 sqlText（切标签时 remount）。
const activeSqlText = computed({
  get: () => activeState.value?.sqlText.value ?? "",
  set: (v: string) => {
    const s = activeState.value;
    if (s) s.sqlText.value = v;
  },
});
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
  activeSqlText,
  tableSchema,
  () => void execute(),
  isDark,
  () => void execute(), // Enter 直接执行（Shift+Enter 换行）
  activeKind, // SQL 方言按活动标签的数据库类型切换
);

// 模式切换时重新挂载 CodeMirror（容器 DOM 因 v-if 切换而变化，需 destroy 后重建）。
// 切标签的重挂由上方 activeTabId watcher 负责。
watch(() => activeState.value?.editorMode.value, () => {
  remountSqlEditor();
});

function clearSql() {
  const v = getSqlView();
  if (v) {
    v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: "" } });
  } else {
    const s = activeState.value;
    if (s) s.sqlText.value = "";
  }
}

// --- 执行（按活动标签路由） -----------------------------------------------------

/** USE 语句同步：后端已拦截更新 current_db，这里把标签绑定的库同步到前端状态。 */
const USE_RE = /^\s*use\s+`?([A-Za-z0-9_]+)`?\s*;?\s*$/i;
function maybeSyncUse(sql: string) {
  // 与 composable execute 的提取逻辑一致：先剥行首注释（`-- 注释\nUSE x`），
  // 再按分号切分，多语句里最后一个 USE 生效（`USE db; SELECT 1` 也会同步）。
  const stmts = stripLeadingComments(sql)
    .split(";")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  let useDb: string | null = null;
  for (const stmt of stmts) {
    const m = USE_RE.exec(stmt);
    if (m) useDb = m[1];
  }
  if (useDb && db.activeTab) db.activeTab.database = useDb;
}

/** 适配视图：执行（带只读模式判定，代码模式从 CodeMirror 读 sqlText）。 */
async function execute() {
  const s = activeState.value;
  if (!s) return;
  const sql = s.sqlText.value.trim();
  const ok = await s.console.execute(s.readOnly.value);
  if (ok && sql) maybeSyncUse(sql);
}

/**
 * SQL 回显行的 :ref 回调——仅捕获"上一条输入"（activeSqlId）对应的 DOM，
 * 供 composable 滚动逻辑作锚点（语句置顶 / 结果溢出判断）。卸载时以 null 调用则清空。
 */
function bindSqlEntry(s: TabState | null, id: string, el: unknown) {
  if (!s || id !== s.console.activeSqlId.value) return;
  s.console.activeSqlEl.value = (el as HTMLElement | null) ?? null;
}

function bindScrollRef(s: TabState | null, el: unknown) {
  if (s) s.console.scrollRef.value = (el as HTMLElement | null) ?? null;
}

function bindAnchorRef(s: TabState | null, el: unknown) {
  if (s) s.console.bottomAnchorRef.value = (el as HTMLElement | null) ?? null;
}

/** 命令行回车：执行 cliInput 内容，成功后清空输入框（mysql CLI 风格）。 */
async function onCliKeydown(e: KeyboardEvent) {
  const s = activeState.value;
  if (!s) return;
  // ↑ / ↓ 浏览历史命令（mysql CLI 风格）。
  if (e.key === "ArrowUp" || e.key === "ArrowDown") {
    if (e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return; // 带修饰键交给默认行为
    const text =
      e.key === "ArrowUp" ? s.console.historyOlder(s.cliInput.value) : s.console.historyNewer();
    if (text === null) return; // 无可切换项（已到边界 / 未在浏览），保持默认光标移动
    e.preventDefault();
    s.cliInput.value = text;
    // 光标移到末尾，方便继续编辑。
    nextTick(() => {
      const el = s.cliInputRef.value;
      if (el) {
        el.selectionStart = el.selectionEnd = el.value.length;
      }
    });
    return;
  }

  if (e.key !== "Enter") return;
  if (e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return; // Shift+Enter 换行
  e.preventDefault();
  const sql = s.cliInput.value.trim();
  if (!sql || s.console.executing.value) return;
  const ok = await s.console.execute(s.readOnly.value, sql);
  if (ok) {
    if (sql) maybeSyncUse(sql);
    s.cliInput.value = ""; // 清空输入框，等待下一条
    await nextTick();
    s.cliInputRef.value?.focus();
  }
}

/** 适配视图：历史条目填入当前活动输入（命令行→cliInput，代码模式→CodeMirror）。 */
function useHistoryItem(item: { sql: string; ts: number; elapsedMs?: number }) {
  const s = activeState.value;
  if (!s) return;
  s.console.useHistory(item);
  if (s.editorMode.value === "console") {
    s.cliInput.value = item.sql;
    s.cliInputRef.value?.focus();
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
  const s = activeState.value;
  if (!s || !s.console.selectedTable.value) return;
  // 表结构可能来自其他 profile 的连接（点表时按表所属连接拉取）；重新拉取时
  // 沿用上次实际使用的连接与库名，避免用当前标签连接 DESCRIBE 错库。
  await s.console.loadStructure(
    s.console.selectedTable.value,
    s.console.structureConnId.value ?? undefined,
    s.console.structureDatabase.value ?? undefined,
  );
  s.structureVisible.value = true;
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
  const r = activeLastResult.value;
  if (!r || !r.columns.length) return;
  const { columns, rows } = r;
  const esc = (v: string) => {
    const s = v ?? "";
    return /[",\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
  };
  const lines = [columns.map(esc).join(","), ...rows.map((row) => row.map(esc).join(","))];
  void copyToClipboard(lines.join("\n"));
}
function exportJson() {
  const r = activeLastResult.value;
  if (!r || !r.columns.length) return;
  const { columns, rows } = r;
  const objs = rows.map((row) => {
    const o: Record<string, string> = {};
    columns.forEach((c, i) => (o[c] = row[i] ?? ""));
    return o;
  });
  void copyToClipboard(JSON.stringify(objs, null, 2));
}

// --- AI 集成 ----------------------------------------------------------------
function requireSql(): string | null {
  const sql = activeState.value?.sqlText.value.trim() ?? "";
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
  void ai.send(`请优化以下${dbDialectLabel.value}：\n\n\`\`\`sql\n${sql}\n\`\`\``, SYSTEM_DIAGNOSE.value);
  ElMessage.success("已发送给 AI，请在右侧 AI 面板查看");
}

function aiExplain() {
  const sql = requireSql();
  if (!sql) return;
  if (ai.sending) {
    ElMessage.warning("AI 正在处理中，请稍候");
    return;
  }
  void ai.send(`请解释以下${dbDialectLabel.value}：\n\n\`\`\`sql\n${sql}\n\`\`\``, SYSTEM_EXPLAIN.value);
  ElMessage.success("已发送给 AI，请在右侧 AI 面板查看");
}

// --- profile 弹窗 -----------------------------------------------------------
const dialogVisible = ref(false);
const editingProfile = ref<DbProfile | null>(null);
const defaultGroupId = ref<string | null>(null);

function openCreateProfile(groupId: string | null = null) {
  editingProfile.value = null;
  defaultGroupId.value = groupId;
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
// 仅命令行模式把 AI 执行的 SQL + 结构化结果回显进**活动标签**的输出流。
// 后端在 sql_agent.terminal_visualization 开启时 emit；代码模式不回显。
let unlistenSqlResult: (() => void) | null = null;
// 组件销毁标志：listen 未 resolve 前组件可能已卸载，resolve 后据此立即反订阅。
let unmounted = false;

// --- 生命周期 ---------------------------------------------------------------
// 标签右键菜单：点击任意处 / Esc 关闭（挂 window，随组件生命周期注销）。
function onWindowClick() {
  closeTabMenu();
}
function onWindowKeydown(e: KeyboardEvent) {
  if (e.key === "Escape") closeTabMenu();
}

onMounted(async () => {
  await loadProfiles();
  window.addEventListener("click", onWindowClick);
  window.addEventListener("keydown", onWindowKeydown);
  // 订阅 ai:sql_result（exec_sql 终端可视化回显到活动标签）。
  try {
    const fn = await listen<AiSqlResultEvent>("ai:sql_result", (e) => {
      if (unmounted) return;
      // 仅命令行模式回显（代码模式有自己的结果区，不混入输出流）。
      const s = activeState.value;
      if (s && s.editorMode.value === "console") s.console.pushExternal(e.payload);
    });
    if (unmounted) fn();
    else unlistenSqlResult = fn;
  } catch (e) {
    console.error("SQL 结果事件订阅失败:", e);
  }
});

onBeforeUnmount(() => {
  unmounted = true;
  window.removeEventListener("click", onWindowClick);
  window.removeEventListener("keydown", onWindowKeydown);
  if (unlistenSqlResult) {
    unlistenSqlResult();
    unlistenSqlResult = null;
  }
  // 销毁所有标签状态（反订阅事件）。
  for (const st of tabStates.values()) st.console.destroy();
  tabStates.clear();
  // 关闭所有标签并断开后端连接，避免连接泄漏。
  for (const t of [...db.tabs]) {
    void db.closeTab(t.id);
  }
});
</script>

<template>
  <div class="sql-console">
    <!-- 主栏（始终显示）：树开关 + 库标签 + 新建 …… 断开 / 帮助 / 只读读写；
         命令行模式下模式切换、表结构与图标操作也并入本行（两行合一） -->
    <div class="main-bar">
      <el-tooltip :content="treeCollapsed ? '展开数据库树' : '收起数据库树'" placement="bottom">
        <button class="tree-toggle" @click="treeCollapsed = !treeCollapsed">
          <el-icon><component :is="treeCollapsed ? 'Expand' : 'Fold'" /></el-icon>
        </button>
      </el-tooltip>

      <div ref="dbTabsRef" class="db-tabs" @wheel="onTabsWheel">
        <div
          v-for="(tab, i) in db.tabs"
          :key="tab.id"
          class="db-tab"
          :class="{ active: tab.id === db.activeTabId }"
          :title="tab.database ? `${tab.profileName} · ${tab.database}` : tab.profileName"
          @click="db.setActive(tab.id)"
          @mousedown.middle.prevent="db.closeTab(tab.id)"
          @contextmenu.prevent="openTabMenu(tab, $event)"
        >
          <span class="dot" :class="{ connecting: tab.connecting }" />
          <span v-if="i < 9" class="tab-idx">{{ i + 1 }}</span>
          <span class="title">{{ tabTitle(tab) }}</span>
          <el-icon class="close" title="关闭" @click.stop="db.closeTab(tab.id)"><Close /></el-icon>
        </div>
      </div>
      <!-- 标签右键菜单（fixed 浮层，视口钳制） -->
      <div
        v-if="tabMenu.tab"
        class="tab-menu"
        :style="{ left: tabMenu.x + 'px', top: tabMenu.y + 'px' }"
        @click.stop
      >
        <div class="tab-menu-item" @click="onTabMenuCommand('close')">关闭</div>
        <div class="tab-menu-item" @click="onTabMenuCommand('closeOthers')">关闭其他</div>
        <div class="tab-menu-item" @click="onTabMenuCommand('closeAll')">关闭全部</div>
      </div>
      <el-dropdown trigger="click" @command="onAddTabCommand">
        <div class="tab-add" title="新建标签"><el-icon><Plus /></el-icon></div>
        <template #dropdown>
          <el-dropdown-menu>
            <el-dropdown-item v-for="p in profiles" :key="p.id" :command="p.id">
              {{ p.name }}（{{ p.host }}:{{ p.port }}）
            </el-dropdown-item>
            <el-dropdown-item command="__new_profile__" :icon="'Plus'" divided>
              新建连接…
            </el-dropdown-item>
          </el-dropdown-menu>
        </template>
      </el-dropdown>

      <div class="main-bar-right">
        <!-- 命令行模式：模式切换 + 表结构 + 图标操作并入本行 -->
        <template v-if="activeState && activeState.editorMode.value === 'console'">
          <el-radio-group v-model="editorModeModel" size="small">
            <el-radio-button value="console">命令行</el-radio-button>
            <el-radio-button value="code">代码</el-radio-button>
          </el-radio-group>
          <el-tooltip :content="activeSelectedTable ? `表结构: ${activeSelectedTable}` : '选中表后可查看表结构'" placement="bottom">
            <el-button
              size="small"
              link
              :icon="'Coin'"
              :disabled="!activeSelectedTable"
              @click="showStructure"
            >
              表结构
            </el-button>
          </el-tooltip>
          <el-divider direction="vertical" />
          <el-tooltip content="清屏" placement="bottom">
            <el-button :icon="'Delete'" size="small" link @click="activeState.console.clear()" />
          </el-tooltip>
          <el-tooltip content="AI 优化" placement="bottom">
            <el-button :icon="'MagicStick'" size="small" link @click="aiOptimize" />
          </el-tooltip>
          <el-tooltip content="AI 解释" placement="bottom">
            <el-button :icon="'View'" size="small" link @click="aiExplain" />
          </el-tooltip>
          <el-tooltip content="历史" placement="bottom">
            <el-button :icon="'Clock'" size="small" link @click="historyDrawerModel = true" />
          </el-tooltip>
        </template>

        <div class="bar-flex" />

        <!-- 断开当前标签 -->
        <el-tooltip v-if="db.activeTab" content="断开当前标签" placement="bottom">
          <el-button :icon="'VideoPause'" size="small" type="warning" plain @click="closeActiveTab" />
        </el-tooltip>
        <!-- 帮助：悬浮显示使用指南 -->
        <el-popover placement="bottom-end" :width="380" trigger="hover">
          <template #reference>
            <el-button :icon="'QuestionFilled'" size="small" link class="help-btn" title="使用指南" />
          </template>
          <div class="help-content">
            <div class="help-title">SQL 控制台使用指南</div>
            <ul class="help-list">
              <li>每个标签绑定一个库：点击左侧<b>库名</b>会打开/切换到该库的标签并自动 <b>USE</b>，之后 SQL 无需带库前缀（如 <b>SELECT * FROM `表名`</b>）</li>
              <li>点击左侧<b>表名</b>：自动填入 SELECT 模板并加载表结构（当前库的表不带前缀）</li>
              <li>输入 SQL 后按 <b>Enter</b> 执行，<b>Shift+Enter</b> 换行；执行后语句置顶、结果向下展开</li>
              <li><b>↑ / ↓</b> 浏览历史命令（↑ 取上一条，↓ 回最新，可循环翻找）</li>
              <li>顶部<b>标签栏</b>可切换库标签；中键或 × 可关闭标签；<b>+</b> 新建标签</li>
              <li><b>表结构</b>：查看当前选中表的字段定义</li>
              <li><b>清屏</b>：清空输出流；<b>AI 优化 / AI 解释</b>：把当前 SQL 发给右侧 AI 助手</li>
              <li><b>历史</b>：打开查询历史抽屉，点击条目回填执行</li>
              <li><b>只读模式</b>下写操作（INSERT/UPDATE 等）会被拦截，需切换到读写模式</li>
              <li>DROP / TRUNCATE 及无 WHERE 的 DELETE 需二次确认后才执行</li>
            </ul>
          </div>
        </el-popover>
        <span class="mode-label">模式</span>
        <el-radio-group v-model="readOnlyModel" size="small" :disabled="!db.activeTab">
          <el-radio-button :value="true">只读</el-radio-button>
          <el-radio-button :value="false">读写</el-radio-button>
        </el-radio-group>
      </div>
    </div>

    <!-- 主体：始终显示 -->
    <div class="console-body">
      <!-- 左侧：数据库树（宽度可拖拽；收起后不占布局空间） -->
      <aside v-if="!treeCollapsed" class="table-list" :style="{ width: sidebarWidth + 'px' }">
        <div class="list-header">
          <span>数据库</span>
          <div class="list-header-actions">
            <el-tooltip content="新建连接" placement="bottom">
              <el-button :icon="'Plus'" size="small" circle @click="openCreateProfile()" />
            </el-tooltip>
            <el-tooltip content="新建分组" placement="bottom">
              <el-button :icon="'FolderAdd'" size="small" circle @click="createDbGroup" />
            </el-tooltip>
            <el-tooltip content="刷新" placement="bottom">
              <el-button :icon="'Refresh'" size="small" circle @click="loadProfiles" />
            </el-tooltip>
          </div>
        </div>
        <div class="list-body tree-body">
          <el-tree
            ref="treeRef"
            :key="treeVersion"
            :data="treeData"
            node-key="key"
            :props="{ label: 'label', children: 'children', isLeaf: 'isLeaf' }"
            :load="loadTreeNode"
            :indent="10"
            lazy
            :expand-on-click-node="true"
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
                <el-icon v-if="data.type === 'group'" class="node-icon grp"><Folder /></el-icon>
                <el-icon v-else-if="data.type === 'instance'" class="node-icon"><Connection /></el-icon>
                <el-icon v-else-if="data.type === 'database'" class="node-icon"><Coin /></el-icon>
                <el-icon v-else class="node-icon"><Document /></el-icon>
                <span v-if="data.type === 'instance' && isPostgres(data.kind)" class="node-kind">PG</span>
                <span class="node-label">{{ data.label }}</span>

                <!-- 实例/分组悬浮操作菜单 -->
                <el-dropdown
                  v-if="data.type === 'instance' || data.type === 'group'"
                  class="node-menu"
                  size="small"
                  trigger="click"
                  placement="bottom-end"
                  @command="(cmd: string) => onTreeCommand(cmd, data)"
                >
                  <!-- stop 必须放在触发元素上：放在 el-dropdown 上不会阻止事件冒泡到树节点 -->
                  <el-icon class="node-menu-icon" @click.stop><MoreFilled /></el-icon>
                  <template #dropdown>
                    <el-dropdown-menu v-if="data.type === 'instance'">
                      <el-dropdown-item command="edit" :icon="'EditPen'">编辑</el-dropdown-item>
                      <el-dropdown-item command="delete" :icon="'Delete'" divided>删除</el-dropdown-item>
                    </el-dropdown-menu>
                    <el-dropdown-menu v-else>
                      <el-dropdown-item command="newChild" :icon="'Plus'">新建连接</el-dropdown-item>
                      <el-dropdown-item command="rename" :icon="'EditPen'">重命名</el-dropdown-item>
                      <el-dropdown-item command="delete" :icon="'Delete'" divided>删除</el-dropdown-item>
                    </el-dropdown-menu>
                  </template>
                </el-dropdown>
              </span>
            </template>
          </el-tree>
          <div v-if="treeData.length === 0" class="empty-tip">未配置数据库实例</div>
        </div>
      </aside>

      <!-- 拖拽分隔条（收起时隐藏） -->
      <div v-if="!treeCollapsed" class="sidebar-resizer" @mousedown="startResize"></div>

      <!-- 右侧：编辑器 + 结果 -->
      <section class="editor-area sql-console-area">
        <!-- 未连接提示 -->
        <div v-if="!activeState" class="editor-placeholder">
          <el-empty description="展开左侧数据库实例，或点 + 新建标签" :image-size="80" />
        </div>
        <template v-else>
        <!-- ============ 命令行模式（mysql CLI 风格：输入在结果流末尾） ============ -->
        <template v-if="activeState.editorMode.value === 'console'">
        <!-- 单一输出流（顶部输入式终端）：
             输入框常驻内容区顶部（始终可见）；历史输出在输入框上方（溢出内容区，
             靠负滚动 / 向上滚轮回看）；当前内容区始终展示最新一条输出。 -->
        <div :ref="(el) => bindScrollRef(activeState, el)" class="console-output">
          <template v-for="e in activeState.console.entries.value" :key="e.id">
            <!-- 执行的 SQL -->
            <div
              v-if="e.kind === 'sql'"
              class="entry entry-sql"
              :ref="(el) => bindSqlEntry(activeState, e.id, el)"
            >
              <span class="entry-prompt">mysql&gt;</span>
              <span class="entry-sql-text">{{ e.sql }}</span>
              <el-icon v-if="e.status === 'running'" class="is-loading entry-spin"><Refresh /></el-icon>
            </div>
            <!-- 表格结果（虚拟滚动：大结果集只渲染可视行） -->
            <div v-else-if="e.kind === 'table'" class="entry entry-table">
              <ResultTableV2 :rows="e.rows" :columns="e.columns" :height="consoleTableHeight(e.rows.length)" />
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
          <!-- 输入框（mysql CLI 风格：常驻输出流末尾，新输出贴在它上方） -->
          <div :ref="(el) => bindAnchorRef(activeState, el)" class="console-input-wrap">
            <span class="input-prompt">{{ cliPrompt }}</span>
            <textarea
              :ref="(el) => { if (activeState) activeState.cliInputRef.value = (el as HTMLTextAreaElement | null) ?? null }"
              v-model="cliInputModel"
              class="cli-input"
              rows="1"
              placeholder="输入 SQL，Enter 执行，Shift+Enter 换行，↑↓ 切换历史"
              spellcheck="false"
              @keydown="onCliKeydown"
            />
          </div>
        </div>
        </template>

        <!-- ============ 代码模式（多行编辑器 + 结果区） ============ -->
        <template v-else>
          <div class="code-editor-toolbar">
            <el-button type="primary" size="small" :icon="'CaretRight'" :loading="activeExecuting" @click="execute">
              执行
            </el-button>
            <!-- 模式切换：命令行 / 代码 + 表结构（位于清空左侧） -->
            <el-radio-group v-model="editorModeModel" size="small">
              <el-radio-button value="console">命令行</el-radio-button>
              <el-radio-button value="code">代码</el-radio-button>
            </el-radio-group>
            <el-tooltip :content="activeSelectedTable ? `表结构: ${activeSelectedTable}` : '选中表后可查看表结构'" placement="bottom">
              <el-button
                size="small"
                link
                :icon="'Coin'"
                :disabled="!activeSelectedTable"
                @click="showStructure"
              >
                表结构
              </el-button>
            </el-tooltip>
            <el-button size="small" :icon="'Delete'" @click="clearSql">清空</el-button>
            <!-- 辅助操作：从顶部工具栏移到执行行右侧（仅图标） -->
            <div class="code-toolbar-right">
              <el-tooltip content="清屏" placement="bottom">
                <el-button :icon="'Delete'" size="small" link @click="activeState.console.clear()" />
              </el-tooltip>
              <el-tooltip content="AI 优化" placement="bottom">
                <el-button :icon="'MagicStick'" size="small" link @click="aiOptimize" />
              </el-tooltip>
              <el-tooltip content="AI 解释" placement="bottom">
                <el-button :icon="'View'" size="small" link @click="aiExplain" />
              </el-tooltip>
              <el-tooltip content="历史" placement="bottom">
                <el-button :icon="'Clock'" size="small" link @click="historyDrawerModel = true" />
              </el-tooltip>
            </div>
          </div>
          <!-- 复用同一个 CodeMirror 实例：命令行/代码模式共享 sqlEditorRef。
               注意：v-if 切换会销毁重建 DOM，CM 需重新挂载，由 watch 后的 remount 保证。 -->
          <div v-if="activeState.editorMode.value === 'code'" ref="sqlEditorRef" class="code-editor" />
          <div class="code-result">
            <div class="code-result-header">
              <span class="result-meta" v-if="activeLastResult && !activeLastResult.error">
                {{ activeResultIsSelect ? `${activeLastResult.rows.length} 行` : `影响 ${activeLastResult.affected} 行` }}
                · {{ activeLastResult.elapsedMs }} ms
              </span>
              <span class="result-meta error" v-else-if="activeLastResult && activeLastResult.error">错误</span>
              <div v-if="activeLastResult && activeResultIsSelect && !activeLastResult.error" class="result-actions">
                <el-button size="small" link @click="exportCsv">复制 CSV</el-button>
                <el-button size="small" link @click="exportJson">复制 JSON</el-button>
              </div>
            </div>
            <div ref="codeResultBodyRef" v-loading="activeExecuting" class="code-result-body">
              <div v-if="!activeLastResult" class="empty-tip">尚未执行查询</div>
              <el-alert v-else-if="activeLastResult.error" :title="activeLastResult.error" type="error" show-icon :closable="false" />
              <ResultTableV2
                v-else-if="activeResultIsSelect"
                :rows="activeLastResult.rows"
                :columns="activeLastResult.columns"
                :height="resultBodyHeight"
              />
              <el-alert
                v-else
                :title="`执行成功，影响 ${activeLastResult.affected} 行（${activeLastResult.elapsedMs} ms）`"
                type="success"
                show-icon
                :closable="false"
              />
            </div>
          </div>
        </template>
        </template>
      </section>
      <!-- DB 助手面板：仅在 SQL 页显示，与终端助手完全隔离 -->
      <AiPanel domain="db" />
    </div>

    <!-- profile 弹窗 -->
    <DbProfileDialog
      v-model:visible="dialogVisible"
      :profile="editingProfile"
      :default-group-id="defaultGroupId"
      @saved="onProfileSaved"
    />

    <!-- 历史抽屉（活动标签） -->
    <el-drawer v-model="historyDrawerModel" title="查询历史" size="360px" direction="rtl">
      <div class="history-drawer">
        <el-button v-if="activeHistory.length" size="small" link type="danger" @click="activeState?.console.clearHistory()">
          清空历史
        </el-button>
        <div
          v-for="(h, i) in activeHistory"
          :key="i"
          class="history-item"
          :title="h.sql"
          @click="useHistoryItem(h); historyDrawerModel = false"
        >
          <span class="history-sql">{{ h.sql }}</span>
          <span v-if="h.elapsedMs" class="history-meta">{{ h.elapsedMs }}ms</span>
        </div>
        <div v-if="activeHistory.length === 0" class="empty-tip">无历史</div>
      </div>
    </el-drawer>

    <!-- 表结构弹层（默认不展示，点顶部按钮或点表时弹出） -->
    <el-dialog
      v-model="structureVisibleModel"
      :title="activeSelectedTable ? `表结构: ${activeSelectedTable}` : '表结构'"
      width="720px"
      append-to-body
    >
      <el-table
        v-if="activeDescribeResult"
        :data="activeDescribeResult.rows"
        size="small"
        border
        max-height="480"
      >
        <el-table-column
          v-for="(c, i) in activeDescribeResult.columns"
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

<style scoped lang="scss">
.sql-console {
  display: flex;
  flex-direction: column;
  height: 100%;
  width: 100%;
  box-sizing: border-box;
  background: var(--el-bg-color);
  overflow: hidden;
}

/* --- 主栏：树开关 + 库标签 + 新建 …… 断开 / 帮助 / 只读读写（命令行模式含模式切换与图标操作） --- */
.main-bar {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 40px;
  padding: 0 10px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  background: var(--el-bg-color-overlay);
  flex-shrink: 0;
}
/* 树开关按钮 */
.tree-toggle {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 24px;
  height: 24px;
  border: none;
  border-radius: 6px;
  background: transparent;
  color: var(--el-text-color-secondary);
  cursor: pointer;
  flex-shrink: 0;
  transition: background-color 0.15s ease, color 0.15s ease;
}
.tree-toggle:hover {
  color: var(--el-color-primary);
  background: var(--el-fill-color-light);
}
/* 右侧控制组 */
.main-bar-right {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
}
/* 弹性占位（把控制组推到最右） */
.bar-flex {
  flex: 1;
}
.mode-label {
  font-size: 13px;
  color: var(--el-text-color-secondary);
}

/* --- 库标签 --- */
.db-tabs {
  display: flex;
  align-items: center;
  gap: 4px;
  flex: 1;
  min-width: 60px;
  overflow-x: auto;
  /* 滚动条平时隐藏（视觉干净），悬停标签区时渐显细滚动条提示可横滑看更多 */
  scrollbar-width: thin;
  scrollbar-gutter: stable;
}
.db-tabs::-webkit-scrollbar {
  height: 3px;
}
.db-tabs::-webkit-scrollbar-thumb {
  background: transparent;
  border-radius: 2px;
  transition: background 0.2s ease;
}
.db-tabs:hover::-webkit-scrollbar-thumb {
  background: var(--el-border-color);
}
.db-tab {
  position: relative;
  display: flex;
  align-items: center;
  gap: 6px;
  height: 26px;
  padding: 0 8px;
  border-radius: 6px;
  cursor: pointer;
  font-size: 12.5px;
  color: var(--el-text-color-regular);
  flex-shrink: 0;
  max-width: 200px;
  transition: background-color 0.15s ease, color 0.15s ease;
}
.db-tab:hover {
  background: var(--el-fill-color-light);
}
.db-tab.active {
  background: var(--el-color-primary-light-9);
  color: var(--el-color-primary);
  font-weight: 500;
}
.db-tab .title {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.db-tab .dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: var(--el-color-success);
  flex-shrink: 0;
}
.db-tab .dot.connecting {
  background: var(--el-color-warning);
}
.db-tab .tab-idx {
  font-size: 10px;
  color: var(--el-text-color-placeholder);
  min-width: 9px;
  text-align: center;
  flex-shrink: 0;
}
.db-tab.active .tab-idx {
  color: var(--el-color-primary);
}
.db-tab .close {
  font-size: 12px;
  padding: 2px;
  border-radius: 3px;
  color: var(--el-text-color-secondary);
  opacity: 0;
  flex-shrink: 0;
  transition: opacity 0.15s ease;
}
.db-tab:hover .close,
.db-tab.active .close {
  opacity: 1;
}
.db-tab .close:hover {
  color: var(--el-color-danger);
  background: var(--el-fill-color);
}
.tab-add {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 22px;
  height: 22px;
  border-radius: 5px;
  cursor: pointer;
  color: var(--el-text-color-secondary);
  font-size: 14px;
  flex-shrink: 0;
}
.tab-add:hover {
  color: var(--el-color-primary);
  background: var(--el-fill-color);
}
/* 标签右键菜单（fixed 浮层） */
.tab-menu {
  position: fixed;
  z-index: 3000;
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color);
  border-radius: 6px;
  box-shadow: 0 2px 12px rgba(0, 0, 0, 0.15);
  padding: 4px;
  min-width: 110px;
}
.tab-menu-item {
  padding: 6px 12px;
  font-size: 13px;
  cursor: pointer;
  border-radius: 4px;
  color: var(--el-text-color-primary);
}
.tab-menu-item:hover {
  background: var(--el-fill-color);
  color: var(--el-color-primary);
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

/* 编辑器区未连接占位 */
.editor-placeholder {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
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
  flex: 1;
  min-width: 0;
}
/* 分组图标颜色 */
.node-icon.grp {
  color: var(--el-color-primary);
}
/* 悬浮操作菜单（参考终端侧栏） */
.list-header-actions {
  display: flex;
  gap: 2px;
}
.node-menu {
  display: none;
  align-items: center;
  cursor: pointer;
  padding: 2px;
  flex-shrink: 0;
}
.tree-node:hover .node-menu,
.node-menu:focus-within {
  display: flex;
}
.node-menu-icon {
  font-size: 13px;
  color: var(--el-text-color-secondary);
  border-radius: 4px;
}
.node-menu-icon:hover {
  color: var(--el-color-primary);
  background: var(--el-fill-color);
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
/* 帮助浮层内容 */
.help-content {
  font-size: 12px;
  line-height: 1.7;
}
.help-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--el-text-color-primary);
  margin-bottom: 6px;
  padding-bottom: 6px;
  border-bottom: 1px solid var(--el-border-color-lighter);
}
.help-list {
  margin: 0;
  padding-left: 16px;
  color: var(--el-text-color-regular);
}
.help-list li {
  margin-bottom: 3px;
}
.help-list b {
  color: var(--el-color-primary);
  font-weight: 600;
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
/* 结果头部元信息（行数/耗时）；error 变体为执行失败时的状态提示。 */
.result-meta {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.result-meta.error {
  color: var(--el-color-danger);
}
.entry-info {
  color: var(--el-text-color-secondary);
  font-style: italic;
}
/* 当前输入区（常驻输出流末尾，mysql CLI 风格；内联样式，无独立边框） */
.console-input-wrap {
  display: flex;
  align-items: stretch;
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
/* 执行行右侧辅助按钮组（清屏 / AI 优化 / AI 解释 / 历史） */
.code-toolbar-right {
  margin-left: auto;
  display: flex;
  align-items: center;
  gap: 2px;
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
