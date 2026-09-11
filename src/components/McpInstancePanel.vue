<!--
  McpInstancePanel.vue — 单个 MCP 实例（SSH / DB / File）的配置与管理面板。

  作为 McpView 的三个 Tab 共用组件，按 kind 区分：
  - kind="ssh"：对外暴露 exec_ssh + 文件工具，绑定一个 SSH 会话。
  - kind="db"：对外暴露 exec_sql，绑定一个 DB profile。
  - kind="file"：对外暴露 list_files/upload_file/download_file，绑定一个 S3 文件账号（仅 bound 模式）。

  资源模式（resourceMode）：
  - "bound"（默认）：绑定资源为 SSH 会话 / DB profile / S3 文件账号，工具只传 command/sql/path 等。
  - "client"（客户端直连）：免绑定实例，调用方在工具参数中传
    host/port/username/password，凭据即用即弃、不存储不落日志。
    （File MCP 不支持 client 模式）
  - "multi"（多机模式，仅 SSH）：勾选一组 SSH 会话授权给外部 AI，工具调用必传
    target（授权机器名，重名自动加 #序号 后缀），由 AI 自行决定每次执行的目标
    机器；越权 target 被拒绝。运行中调整勾选可热切换即时生效。

  功能：
  - 资源模式开关（直连模式隐藏绑定 UI 并展示安全提示；File kind 隐藏此开关）。
  - 绑定资源下拉（启动前/后均可改；运行中改后提示"需重启生效"，
    multi 模式的勾选集合变更可热切换）。
  - 监听地址（默认 127.0.0.1；可改为 0.0.0.0 / 局域网 IP 对外开放）与端口可编辑。
  - token 生成 / 复制。
  - 启停按钮 + 运行状态徽标 + SSE 端点。
  - 客户端配置 JSON（一键复制）。
  - 对外监听（0.0.0.0）风险提示。
-->
<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { ElMessage } from "element-plus";
import { VideoPlay, VideoPause, Refresh, CopyDocument, Key } from "@element-plus/icons-vue";
import HelpTip from "@/components/HelpTip.vue";
import { useMcpStore } from "@/stores/mcp";
import { useSessionsStore } from "@/stores/sessions";
import { useTerminalsStore } from "@/stores/terminals";
import { dbListProfiles, dbConnect, dbListDatabases, dbDisconnect } from "@/api/db";
import { fileAccountList } from "@/api/fileBackend";
import type { FileAccount } from "@/api/fileBackend";
import type { McpBoundSource, McpInstanceConfig, McpKind } from "@/api/mcp";
import type { DbProfile, Session } from "@/api/types";

const props = defineProps<{ kind: McpKind }>();

const mcp = useMcpStore();
const sessions = useSessionsStore();
const terminalsStore = useTerminalsStore();

/** 该 kind 的可用资源列表（ssh→SSH 会话；db→DB profile；file→S3 文件账号）。 */
const sshSessions = ref<Session[]>([]);
const dbProfiles = ref<DbProfile[]>([]);
const fileAccounts = ref<FileAccount[]>([]);

/** DB MCP 专用：绑定 profile 后可选的数据库列表。 */
const databases = ref<string[]>([]);
const loadingDbs = ref(false);

const isSsh = computed(() => props.kind === "ssh");
const isFile = computed(() => props.kind === "file");
/** kind 中文标题。 */
const title = computed(() => (isSsh.value ? "SSH MCP" : isFile.value ? "File MCP" : "DB MCP"));
/** 该 kind 对外暴露的工具说明（按资源模式/绑定来源分支）。 */
const toolHint = computed(() => {
  if (isFile.value) {
    return "对外暴露 list_files(path) / upload_file(localPath, remotePath) / download_file(remotePath, localPath)，\
目标即下方绑定的 S3 文件账号。File MCP 仅支持绑定模式。";
  }
  if (clientMode.value) {
    return isSsh.value
      ? "对外暴露 exec_ssh(host/port/username/password/command)：目标服务器由调用方在参数中指定，免绑定本地实例。"
      : "对外暴露 exec_sql(host/port/username/password/database/sql)：目标数据库由调用方在参数中指定，免绑定本地实例。";
  }
  if (multiMode.value) {
    return "对外暴露 exec_ssh(target, command) / list_files / upload_file / download_file（均需 target）\
与只读 list_machines()（机器清单）。target 由外部 AI 从下方勾选的机器中自行选择，\
越权 target 会被拒绝并回显可用清单。";
  }
  if (terminalBound.value) {
    return "对外暴露 exec_ssh(command)：命令写入下方绑定的终端标签页执行（支持 A→B→C 跳板嵌套，\
命令在终端当前所在的远端主机上执行，用户可实时看到执行过程）。";
  }
  return isSsh.value
    ? "对外暴露 exec_ssh(command)，目标服务器即下方绑定的 SSH 会话。"
    : "对外暴露 exec_sql(sql)，目标数据库即下方绑定的连接。";
});

const config = computed(() => {
  if (isSsh.value) return mcp.sshConfig;
  if (isFile.value) return mcp.fileConfig;
  return mcp.dbConfig;
});
const status = computed(() => {
  if (isSsh.value) return mcp.sshStatus;
  if (isFile.value) return mcp.fileStatus;
  return mcp.dbStatus;
});
const loading = computed(() => mcp.loading[props.kind]);

/** 客户端直连模式（免绑定实例）：目标与账密由调用方在工具参数中传入。File 不支持。 */
const clientMode = computed(() => !isFile.value && config.value.resourceMode === "client");

/** 多机模式（仅 SSH）：勾选一组机器授权给外部 AI，由 AI 按工具参数 target 自选目标。 */
const multiMode = computed(() => isSsh.value && config.value.resourceMode === "multi");

/** 多机模式的勾选集合（旧配置无 resourceIds 字段时兜底为空数组，避免 multiple select 收到 undefined）。 */
const selectedIds = computed<string[]>({
  get: () => config.value.resourceIds ?? [],
  set: (v) => {
    config.value.resourceIds = v;
  },
});

/** 绑定来源（兼容旧配置无该字段的情况）。 */
const boundSource = computed<McpBoundSource>(
  () => config.value.boundSource ?? "config",
);

/**
 * 模式 / 绑定方式（下拉框展示用）：
 * - "config"：会话配置绑定（bound + boundSource=config）
 * - "terminal"：终端标签页绑定（bound + boundSource=terminal）
 * - "multi"：多机模式（resourceMode=multi，仅 SSH：勾选一组机器，AI 按 target 自选）
 * - "client"：客户端直连（resourceMode=client，免绑定）
 */
const mode = computed<"config" | "terminal" | "multi" | "client">(() => {
  if (config.value.resourceMode === "client") return "client";
  if (config.value.resourceMode === "multi") return "multi";
  return boundSource.value === "terminal" ? "terminal" : "config";
});

/** 终端标签页绑定模式（SSH kind + bound 模式 + 来源为 terminal）。 */
const terminalBound = computed(
  () => isSsh.value && !clientMode.value && !multiMode.value && boundSource.value === "terminal",
);

/** 模式 / 绑定方式的说明（悬浮帮助）。 */
const modeHint = computed(() => {
  if (multiMode.value) {
    return "勾选一组机器授权给外部 AI：每次工具调用由 AI 按机器名 / 地址 / 标签自行决定目标\
（参数 target）。重名机器的目标名自动加 #序号 后缀；确认卡片与日志都会显示目标机器。\
运行中调整勾选即时生效；切换到/离开本模式需重启服务。";
  }
  if (terminalBound.value) {
    return "命令将写入所选终端执行：终端当前在哪个远端主机（含 A→B→C 跳板嵌套）\
命令就在哪个主机上执行。执行期间请勿手动操作该终端；终端关闭后绑定失效，需重新选择。";
  }
  if (clientMode.value) {
    return "调用方在工具参数中传 host/port/username/password，凭据仅本次调用有效，\
不存储不落日志。适用于调用方自带账密表的巡检场景。";
  }
  return "命令通过新建短连接执行，不打扰终端；但只能到达本机直接连到的服务器，\
无法执行到跳板后的主机（如 A→B→C 中的 C）。需要时切换为「终端标签页」。";
});

/** 多机勾选控件的悬浮帮助。 */
const machinesHint =
  "目标名即会话名（重名自动加 #序号 后缀），会连同地址与标签暴露给外部 AI 做路由决策。\
AI 只能在勾选的机器里选择目标，越权 target 会被拒绝。";

/** 运行模式选项（与 AI 助手的执行模式语义一致）。 */
const RUN_MODE_OPTIONS: {
  value: "manual" | "whitelist" | "auto";
  label: string;
  desc: string;
}[] = [
  {
    value: "manual",
    label: "手动运行",
    desc: "所有调用都弹出确认，批准后才执行",
  },
  {
    value: "whitelist",
    label: "白名单运行",
    desc: "白名单内自动放行，其余弹出确认",
  },
  {
    value: "auto",
    label: "自动运行",
    desc: "全部自动执行（文件传输仍需确认）",
  },
];

/** 当前模式的说明文案（按 kind 展开白名单的具体含义）。 */
const runModeDesc = computed(() => {
  switch (config.value.runMode) {
    case "auto":
      return "外部客户端的所有调用直接执行，不弹确认（文件传输除外）";
    case "whitelist":
      return isSsh.value
        ? "命令白名单内自动执行（同「设置 → AI」的 SSH 白名单），其余弹确认"
        : isFile.value
          ? "读操作自动执行，文件上传/下载仍弹确认"
          : "只读 SQL（SELECT/SHOW/EXPLAIN/DESCRIBE）自动执行，其余弹确认";
    default:
      return "外部客户端的每次调用都需你在 X-Term 中确认后执行";
  }
});

/** 运行模式的说明（悬浮帮助）。 */
const runModeHint =
  "与 AI 助手的执行模式语义一致：手动运行 = 所有调用人工确认；\
白名单运行 = 白名单内自动放行（SSH 为「设置 → AI」的命令白名单、DB 为只读 SQL、\
文件为读操作），其余人工确认；自动运行 = 全部自动执行。\
例外：upload_file / download_file 读写本机任意路径，任何模式下都强制人工确认。";

/** 记录执行日志开关的说明（悬浮帮助）。 */
const logHint =
  "开启后，每次启动服务会生成一个文本日志文件（位于应用数据目录 mcp-logs/），\
记录每次工具调用的时间、命令/SQL、结果与耗时。重启服务后生效。";

/** 已打开的终端标签页（含断开的，断开的禁选；连接中的无 instanceId 不列出）。 */
const terminalOptions = computed(() =>
  terminalsStore.tabs
    .filter((t) => t.instanceId)
    .map((t) => ({
      instanceId: t.instanceId,
      session: t.session,
      disconnected: t.disconnected,
      connecting: t.connecting,
    })),
);

/** 该 kind 是否有可用资源可选。 */
const hasResources = computed(() => {
  if (isSsh.value) {
    // 终端绑定模式：有已打开的终端即可；否则看会话配置。
    return terminalBound.value
      ? terminalOptions.value.some((t) => t.instanceId && !t.disconnected)
      : sshSessions.value.length > 0;
  }
  if (isFile.value) return fileAccounts.value.length > 0;
  return dbProfiles.value.length > 0;
});

/**
 * 与后端 disambiguate_display_names 同规则生成展示名：
 * 唯一名保持原名；重名组内按 id 字典序加 `#k`（k 从 1 起）。
 * **必须按已勾选集合计算**（后端同样只对勾选集合编号），全量集合算出的
 * 编号可能不同（如 3 台同名只勾了 2 台）。
 */
function displayNamesOf(sessions: { id: string; name: string }[]): Map<string, string> {
  const byName = new Map<string, string[]>();
  for (const s of sessions) {
    const list = byName.get(s.name) ?? [];
    list.push(s.id);
    byName.set(s.name, list);
  }
  const out = new Map<string, string>();
  for (const s of sessions) {
    const ids = (byName.get(s.name) ?? []).slice().sort();
    out.set(s.id, ids.length === 1 ? s.name : `${s.name}#${ids.indexOf(s.id) + 1}`);
  }
  return out;
}

/** 绑定资源的展示名（用于运行状态/提示）。 */
const boundResourceName = computed(() => {
  if (multiMode.value) {
    const ids = config.value.resourceIds ?? [];
    if (ids.length === 0) return "";
    // 按勾选集合计算带后缀的目标名（与确认卡片 / tools/list enum 完全一致）。
    const selected = ids
      .map((id) => sshSessions.value.find((s) => s.id === id))
      .filter((s): s is Session => !!s);
    const names = displayNamesOf(selected);
    const labels = ids.map((id) => names.get(id) ?? "(会话已删除)");
    return `多机模式 · 已授权 ${ids.length} 台：${labels.join("、")}`;
  }
  const id = config.value.resourceId;
  if (!id) return "";
  if (isSsh.value && terminalBound.value) {
    const t = terminalsStore.tabs.find((tab) => tab.instanceId === id);
    if (!t) return "(终端已关闭)";
    return `终端 · ${t.session.name} (${t.session.host}:${t.session.port})`;
  }
  if (isSsh.value) {
    return sshSessions.value.find((s) => s.id === id)?.name ?? "(会话已删除)";
  }
  if (isFile.value) {
    return fileAccounts.value.find((a) => a.id === id)?.name ?? "(账号已删除)";
  }
  return dbProfiles.value.find((p) => p.id === id)?.name ?? "(连接已删除)";
});

/** 完整 MCP 端点 URL（Streamable HTTP 用 Authorization 头鉴权，URL 不带 token）。 */
const fullUrl = computed(() => {
  if (!status.value.running) return "";
  // 绑定地址是 0.0.0.0（对所有网卡监听）时，0.0.0.0 不是合法客户端目的地
  // （多数系统上连接 0.0.0.0 失败或行为未定义），展示为 127.0.0.1 才是实际可用地址。
  const host = status.value.host === "0.0.0.0" ? "127.0.0.1" : status.value.host;
  return `http://${host}:${status.value.port}/mcp`;
});

/** 客户端配置 JSON（一键复制）。 */
const clientConfig = computed(() => {
  if (!fullUrl.value) return "";
  const serverKey = isSsh.value ? "x-term-ssh" : isFile.value ? "x-term-file" : "x-term-db";
  const cfg: Record<string, unknown> = {
    mcpServers: {
      [serverKey]: { url: fullUrl.value },
    },
  };
  if (config.value.token) {
    (cfg.mcpServers as Record<string, Record<string, unknown>>)[serverKey].headers = {
      Authorization: `Bearer ${config.value.token}`,
    };
  }
  return JSON.stringify(cfg, null, 2);
});

/** 绑定资源 / 地址 / 端口 改动后，若服务在运行，需提示重启。 */
const needsRestart = computed(() => status.value.running);

async function loadResources() {
  if (isSsh.value) {
    if (!sessions.loaded) {
      try {
        await sessions.load();
      } catch {
        /* ignore */
      }
    }
    sshSessions.value = sessions.sessions.filter(
      (s) => s.protocol === "ssh" || !s.protocol,
    );
  } else if (isFile.value) {
    try {
      fileAccounts.value = await fileAccountList();
    } catch (e) {
      ElMessage.error("加载 S3 文件账号列表失败：" + String(e));
    }
  } else {
    try {
      dbProfiles.value = await dbListProfiles();
    } catch (e) {
      ElMessage.error("加载数据库连接列表失败：" + String(e));
    }
  }
}

/** DB MCP：选择 profile 后临时连接获取数据库列表。
 *  快速切换 profile 时用序号丢弃过期响应，避免先发起的慢请求覆盖新选择的结果。 */
let dbListSeq = 0;
async function loadDatabases(profileId: string) {
  const seq = ++dbListSeq;
  if (!profileId) {
    if (seq === dbListSeq) databases.value = [];
    return;
  }
  loadingDbs.value = true;
  try {
    const connId = await dbConnect(profileId);
    try {
      const dbs = await dbListDatabases(connId);
      if (seq === dbListSeq) databases.value = dbs;
    } finally {
      await dbDisconnect(connId).catch(() => {});
    }
  } catch {
    if (seq !== dbListSeq) return;
    // 连接失败（服务不可达等）：清空列表，用户可手动输入。
    databases.value = [];
  } finally {
    if (seq === dbListSeq) loadingDbs.value = false;
  }
}

/**
 * 模式 / 绑定方式切换（下拉框）。
 *
 * 组合映射：
 * - config  → resourceMode="bound" + boundSource="config"
 * - terminal→ resourceMode="bound" + boundSource="terminal"
 * - multi   → resourceMode="multi"（仅 SSH；勾选集合另选）
 * - client  → resourceMode="client"（忽略绑定）
 *
 * 规则：
 * - config ↔ terminal ↔ multi 的 id 空间不同（单选/多选），清空 resourceId 重选；
 *   config ↔ terminal 运行中可热切换（`mcp_rebind`）；multi 的勾选集合变更走
 *   `mcp_rebind_multi` 热切换。
 * - 涉及 client / multi 的模式切换不能热切换（后端 resource_mode 启动时固化），
 *   提示重启。
 */
async function onModeChange(value: string | number | boolean) {
  const next =
    value === "client"
      ? "client"
      : value === "multi"
        ? "multi"
        : value === "terminal"
          ? "terminal"
          : "config";
  if (next === mode.value) return;
  const prev = mode.value;
  config.value.resourceMode =
    next === "client" ? "client" : next === "multi" ? "multi" : "bound";
  config.value.boundSource = next === "terminal" ? "terminal" : "config";
  if (prev !== "client" && next !== "client") {
    // 单选 / 多选的 id 空间不同，清空重选。
    config.value.resourceId = undefined;
  }
  if (next === "client" || prev === "client" || next === "multi" || prev === "multi") {
    // 涉及客户端直连或多机：resource_mode 启动时固化，需重启生效。
    await saveConfigAndMaybeWarn();
  } else {
    await saveConfigAndMaybeWarn({ hotRebind: true });
  }
}

/** 多机模式勾选变更：保存配置；运行中热切换（mcp_rebind_multi）即时生效。 */
async function onMachinesChange() {
  await saveConfigAndMaybeWarn({ hotRebindMulti: true });
}

/** 绑定资源变更处理。 */
async function onResourceChange() {
  // 仅 DB kind：加载该 profile 的数据库列表。
  if (props.kind === "db" && config.value.resourceId) {
    await loadDatabases(config.value.resourceId);
  }
  await saveConfigAndMaybeWarn({ hotRebind: true });
}

/**
 * 绑定/地址/端口改动后保存配置。
 *
 * `hotRebind`：单选绑定类改动在服务运行中尝试热切换（`mcp_rebind`，立即生效
 * 无需重启）；`hotRebindMulti`：多机勾选集合改动走 `mcp_rebind_multi` 热切换；
 * 未传则按原有行为提示"需重启生效"。
 *
 * 保存失败时把内存配置回滚到最近一次成功保存的快照（见 [`rollbackConfig`]），
 * 避免 UI 显示新值而 mcp.json 仍是旧值的前后端不一致。
 *
 * @returns 是否保存成功（false 时调用方应中止后续依赖新配置的操作，如启动）。
 */
async function saveConfigAndMaybeWarn(opts?: {
  hotRebind?: boolean;
  hotRebindMulti?: boolean;
}): Promise<boolean> {
  try {
    await mcp.saveConfig(props.kind);
    lastSaved = { ...config.value };
    if (!needsRestart.value) return true;
    if (opts?.hotRebindMulti && multiMode.value) {
      const ids = config.value.resourceIds ?? [];
      if (ids.length === 0) {
        // 清空勾选：运行中的集合保持旧值，重新勾选后即时生效。
        ElMessage.info("已保存。请勾选机器，勾选后将即时生效。");
        return true;
      }
      try {
        const applied = await mcp.rebindMulti(props.kind, ids);
        // 区分"已即时生效"与"仅保存"（服务未运行或以其它模式运行），
        // 避免在 bound/client 模式运行中调整勾选时误导用户。
        if (applied) ElMessage.success("机器集合已即时生效");
        else ElMessage.info("配置已保存，服务（重）启动后生效");
      } catch (e) {
        ElMessage.warning("热切换失败，重启服务后生效：" + String(e));
      }
      return true;
    }
    if (opts?.hotRebind) {
      if (!config.value.resourceId) {
        // 切换绑定类型后尚未选择新资源：运行中的绑定保持旧值，重选后即时生效。
        ElMessage.info("已保存。请选择新的绑定资源，选择后将即时生效。");
        return true;
      }
      try {
        await mcp.rebind(props.kind, boundSource.value, config.value.resourceId);
        ElMessage.success("绑定已即时生效");
      } catch (e) {
        ElMessage.warning("热切换失败，重启服务后生效：" + String(e));
      }
      return true;
    }
    ElMessage.warning("配置已保存，需重启该 MCP 服务才能生效。");
    return true;
  } catch (e) {
    rollbackConfig();
    ElMessage.error("保存配置失败：" + String(e));
    return false;
  }
}

/** 最近一次成功保存/加载的配置副本：保存失败时用它回滚内存配置。 */
let lastSaved: McpInstanceConfig | null = null;

/** 把内存配置回滚到最近一次成功保存的状态（与 mcp.json 保持一致）。 */
function rollbackConfig() {
  if (lastSaved) Object.assign(config.value, lastSaved);
}

/**
 * 字段值变化统一保存处理器（绑定库 / 监听地址 / 端口 / 执行日志开关）。
 *
 * 这些控件的 @change 会把控件值作为第一个参数传入；若直接把
 * saveConfigAndMaybeWarn 绑到 @change，事件值会被误当作 options 对象，
 * `opts?.hotRebind` 恒为 undefined，所有改动都落入"需重启生效"分支。
 * 这里显式忽略事件值。
 */
function onFieldChange() {
  void saveConfigAndMaybeWarn();
}

/** 运行模式切换：保存配置（后端立即生效，无需重启）。 */
async function saveRunMode() {
  try {
    await mcp.saveConfig(props.kind);
    lastSaved = { ...config.value };
    const label =
      RUN_MODE_OPTIONS.find((o) => o.value === config.value.runMode)?.label ?? "手动运行";
    ElMessage.success(`已切换为「${label}」`);
  } catch (e) {
    rollbackConfig();
    ElMessage.error("保存失败：" + String(e));
  }
}

async function start() {
  // 多机模式：勾选集合非空即可（无单选 resourceId）。
  if (multiMode.value) {
    if ((config.value.resourceIds?.length ?? 0) === 0) {
      ElMessage.warning("多机模式请先勾选至少一台机器");
      return;
    }
  } else if (!clientMode.value && !config.value.resourceId) {
    const resName = terminalBound.value
      ? "终端标签页"
      : isSsh.value
        ? "SSH 会话"
        : isFile.value
          ? "S3 文件账号"
          : "数据库连接";
    ElMessage.warning(`请先选择一个${resName}，或选择「客户端直连」模式`);
    return;
  }
  if (!config.value.token) {
    ElMessage.warning("请先生成 token 再启动");
    return;
  }
  // 先把当前 host/port/resourceId/resourceMode/boundSource 落盘，再用配置启动。
  // 保存失败必须中止启动：否则后端用内存里的旧配置启动，UI 却按新配置展示，
  // 前后端不一致。
  const saved = await saveConfigAndMaybeWarn();
  if (!saved) return;
  try {
    const s = await mcp.start(props.kind);
    ElMessage.success(`${title.value} 已启动：${s.host}:${s.port}`);
  } catch (e) {
    ElMessage.error("启动失败：" + String(e));
  }
}

async function stop() {
  try {
    await mcp.stop(props.kind);
    ElMessage.success(`${title.value} 已停止`);
  } catch (e) {
    ElMessage.error("停止失败：" + String(e));
  }
}

async function generateToken() {
  try {
    await mcp.regenerateToken(props.kind);
    ElMessage.success("已生成新 token");
  } catch (e) {
    ElMessage.error("生成 token 失败：" + String(e));
  }
}

async function copy(text: string, label = "已复制") {
  if (!text) return;
  try {
    await navigator.clipboard.writeText(text);
    ElMessage.success(label);
  } catch {
    ElMessage.error("复制失败");
  }
}

onMounted(async () => {
  // 记录已加载的配置作为回滚基准（后续保存失败时恢复到该状态）。
  lastSaved = { ...config.value };
  await loadResources();
  // 仅 DB kind：若已有绑定 profile，加载其数据库列表。
  if (props.kind === "db" && config.value.resourceId) {
    await loadDatabases(config.value.resourceId);
  }
});
</script>

<template>
  <div class="instance-panel">
    <!-- 服务配置 -->
    <div class="form-card">
      <div class="card-head">
        <span class="card-ico"><el-icon><Setting /></el-icon></span>
        <span class="card-title-text">服务配置</span>
        <HelpTip :content="toolHint" />
      </div>

      <!-- 模式 / 绑定方式（SSH 三选一、DB 二选一；File 仅支持绑定模式，隐藏） -->
      <div v-if="!isFile" class="field-row">
        <label class="field-label">
          模式 / 绑定方式
          <HelpTip :content="modeHint" />
        </label>
        <el-select :model-value="mode" class="field-control" @change="onModeChange">
          <el-option
            v-if="isSsh"
            value="terminal"
            label="终端标签页（命令写入终端执行）"
          />
          <el-option
            value="config"
            :label="isSsh ? '会话配置（新建连接执行）' : '会话配置（绑定数据库连接）'"
          />
          <el-option v-if="isSsh" value="multi" label="多机模式（勾选多台，AI 自选目标）" />
          <el-option value="client" label="客户端直连（免绑定，调用方传账密）" />
        </el-select>
      </div>

      <!-- 多机模式说明 -->
      <el-alert v-if="multiMode" type="info" :closable="false" show-icon class="alert-gap">
        <div class="client-mode-alert">
          <div>工具调用需传 <code>target</code> 指定目标机器，外部 AI 按机器名 / 地址 / 标签自行路由。</div>
          <div>重名机器的目标名自动加 <code>#序号</code> 后缀；越权 target 会被直接拒绝。</div>
          <div>运行中调整勾选<strong>即时生效</strong>；切换到 / 离开本模式需<strong>重启 MCP 服务</strong>。</div>
        </div>
      </el-alert>

      <!-- 多机模式：勾选授权机器集合 -->
      <div v-if="multiMode" class="field-row">
        <label class="field-label">
          授权机器（多选）
          <HelpTip :content="machinesHint" />
          <span class="required">*</span>
        </label>
        <el-select
          v-model="selectedIds"
          multiple
          filterable
          placeholder="勾选授权给外部 AI 的机器（至少一台）"
          class="field-control"
          :disabled="!hasResources"
          @change="onMachinesChange"
        >
          <el-option
            v-for="s in sshSessions"
            :key="s.id"
            :label="`${s.name} (${s.host}:${s.port})`"
            :value="s.id"
          />
        </el-select>
        <div v-if="selectedIds.length > 0" class="hint-text">
          已选 {{ selectedIds.length }} 台；重名机器的目标名将自动加 #序号 后缀。
        </div>
      </div>

      <!-- 直连模式安全提示 -->
      <el-alert
        v-if="clientMode"
        type="warning"
        :closable="false"
        show-icon
        class="alert-gap"
      >
        <div class="client-mode-alert">
          <div>调用方需在工具参数中传 <code>host</code> / <code>port</code> /
            <code>username</code> / <code>password</code>（DB 另可传 <code>database</code>）。</div>
          <div>密码<strong>仅本次调用有效</strong>：不存储、不落日志、不显示在确认弹窗中。</div>
          <div>切换本模式后需<strong>重启 MCP 服务</strong>才能生效。</div>
        </div>
      </el-alert>

      <!-- 绑定资源（仅单选绑定模式；多机模式用上方的多选集合） -->
      <div v-if="!clientMode && !multiMode" class="field-row">
        <label class="field-label">
          绑定{{ terminalBound ? "终端标签页" : isSsh ? "SSH 会话" : isFile ? "S3 文件账号" : "数据库连接" }}
          <span class="required">*</span>
        </label>
        <el-select
          v-model="config.resourceId"
          :placeholder="`选择${terminalBound ? '一个已打开的终端标签页' : isSsh ? '一个 SSH 会话' : isFile ? '一个 S3 文件账号' : '一个数据库连接'}`"
          filterable
          class="field-control"
          :disabled="!hasResources"
          @change="onResourceChange"
        >
          <template v-if="isSsh && terminalBound">
            <el-option-group label="已打开的终端（在线）">
              <el-option
                v-for="t in terminalOptions"
                :key="t.instanceId"
                :label="`${t.session.name} (${t.session.host}:${t.session.port})`"
                :value="t.instanceId"
                :disabled="t.disconnected || t.connecting"
              />
            </el-option-group>
          </template>
          <template v-else-if="isSsh">
            <el-option
              v-for="s in sshSessions"
              :key="s.id"
              :label="`${s.name} (${s.host}:${s.port})`"
              :value="s.id"
            />
          </template>
          <template v-else-if="isFile">
            <el-option
              v-for="a in fileAccounts"
              :key="a.id"
              :label="`${a.name} (${a.bucket || a.endpoint})`"
              :value="a.id"
            />
          </template>
          <template v-else>
            <el-option
              v-for="p in dbProfiles"
              :key="p.id"
              :label="`${p.name} (${p.host}:${p.port})`"
              :value="p.id"
            />
          </template>
        </el-select>
      </div>

      <!-- DB MCP：绑定具体数据库（仅 db kind + 绑定模式） -->
      <div v-if="kind === 'db' && !clientMode" class="field-row">
        <label class="field-label">
          绑定数据库（可选）
          <HelpTip content="选择后，exec_sql 将只在该库上执行（外部 AI 工具描述中会注明库名）。若连接不可达，可手动输入库名。" />
        </label>
        <el-select
          v-model="config.boundDatabase"
          placeholder="不选则使用连接默认库；可选择或手动输入"
          filterable
          allow-create
          clearable
          class="field-control"
          :loading="loadingDbs"
          :disabled="!config.resourceId"
          @change="onFieldChange"
        >
          <el-option v-for="db in databases" :key="db" :label="db" :value="db" />
        </el-select>
      </div>
      <el-alert
        v-if="!clientMode && !hasResources"
        type="warning"
        :closable="false"
        show-icon
        class="alert-gap"
        :title="
          terminalBound
            ? '暂无已打开的 SSH 终端，请先在终端页打开一个终端（可嵌套登录后绑定该终端）'
            : `暂无可用${isSsh ? 'SSH 会话' : isFile ? 'S3 文件账号' : '数据库连接'}，请先在对应页面创建${isFile ? '' : '，或选择「客户端直连」模式'}`
        "
      />

      <!-- 监听地址 + 端口 -->
      <div class="addr-row">
        <div class="field-row flex1">
          <label class="field-label">
            监听地址
            <HelpTip content="默认 127.0.0.1（仅本机）。可改为 0.0.0.0 或局域网 IP（如 192.168.x.x），供局域网内其他机器连接。" />
          </label>
          <el-input v-model="config.host" placeholder="127.0.0.1" class="field-control mono-input" @change="onFieldChange" />
        </div>
        <div class="field-row port-field">
          <label class="field-label">端口</label>
          <el-input-number
            v-model="config.port"
            :min="1"
            :max="65535"
            controls-position="right"
            class="field-control"
            @change="onFieldChange"
          />
        </div>
      </div>
      <!-- 对外监听提示：0.0.0.0 对所有网卡开放，局域网内持有 token 者均可调用 -->
      <div v-if="config.host === '0.0.0.0'" class="hint-text warn-inline">
        ⚠ 0.0.0.0 表示监听所有网卡，局域网内其他机器可通过本机 IP 连接（请妥善保管 token）。
      </div>
    </div>

    <!-- 运行状态 + 启停 -->
    <div class="form-card">
      <div class="card-head">
        <span class="card-ico"><el-icon><SwitchButton /></el-icon></span>
        <span class="card-title-text">运行状态</span>
        <div class="head-actions">
          <el-button :icon="Refresh" size="small" @click="mcp.refresh(kind)">刷新</el-button>
          <el-button
            v-if="!status.running"
            type="primary"
            :icon="VideoPlay"
            size="small"
            :loading="loading"
            @click="start"
          >
            启动服务
          </el-button>
          <el-button
            v-else
            type="danger"
            :icon="VideoPause"
            size="small"
            :loading="loading"
            @click="stop"
          >
            停止
          </el-button>
        </div>
      </div>

      <!-- 状态 hero：运行/停止 + 绑定摘要 -->
      <div class="status-hero" :class="status.running ? 'is-on' : 'is-off'">
        <span class="hero-dot" />
        <div class="hero-text">
          <span class="hero-state">{{ status.running ? "运行中" : "已停止" }}</span>
          <span class="hero-desc">
            <template v-if="status.running">
              {{
                clientMode
                  ? "客户端直连模式 · 未绑定实例"
                  : multiMode
                    ? boundResourceName || "多机模式 · 未勾选机器"
                    : `已绑定 · ${boundResourceName || "—"}`
              }}
            </template>
            <template v-else>完成配置后点击「启动服务」对外提供调用</template>
          </span>
        </div>
      </div>

      <!-- 端点地址 -->
      <div v-if="status.running" class="endpoint-row">
        <span class="endpoint-tag">SSE</span>
        <code class="endpoint-url">{{ fullUrl }}</code>
        <el-tooltip content="复制端点地址" placement="top">
          <el-button :icon="CopyDocument" link size="small" @click="copy(fullUrl, '已复制端点地址')" />
        </el-tooltip>
      </div>

      <!-- 运行模式（与 AI 助手执行模式一致：手动 / 白名单 / 自动） -->
      <div class="switch-row">
        <div class="switch-info">
          <div class="switch-title">
            运行模式
            <HelpTip :content="runModeHint" />
          </div>
          <div class="switch-desc">{{ runModeDesc }}</div>
        </div>
        <el-select
          :model-value="config.runMode"
          size="small"
          class="runmode-select"
          style="width: 118px"
          @change="
            (v: 'manual' | 'whitelist' | 'auto') => {
              config.runMode = v;
              saveRunMode();
            }
          "
        >
          <el-option
            v-for="opt in RUN_MODE_OPTIONS"
            :key="opt.value"
            :value="opt.value"
            :label="opt.label"
            :title="opt.desc"
          />
        </el-select>
      </div>

      <!-- 执行日志开关 -->
      <div class="switch-row">
        <div class="switch-info">
          <div class="switch-title">
            记录执行日志
            <HelpTip :content="logHint" />
          </div>
          <div class="switch-desc">每次调用的时间、命令与结果写入本地日志文件</div>
        </div>
        <el-switch v-model="config.enableLog" @change="onFieldChange" />
      </div>
    </div>

    <!-- 访问令牌 -->
    <div class="form-card">
      <div class="card-head">
        <span class="card-ico"><el-icon><Key /></el-icon></span>
        <span class="card-title-text">访问令牌 (Token)</span>
        <HelpTip>
          <template #content>
            外部客户端请求时需在 Header 携带
            <code>Authorization: Bearer &lt;token&gt;</code>，或在 URL 加
            <code>?token=&lt;token&gt;</code>。
          </template>
        </HelpTip>
      </div>
      <div class="token-row">
        <el-input :model-value="config.token ?? ''" placeholder="点击生成 token" readonly class="token-input">
          <template #prefix><el-icon><Key /></el-icon></template>
        </el-input>
        <el-button type="primary" :icon="Refresh" @click="generateToken">
          {{ config.token ? "重新生成" : "生成 Token" }}
        </el-button>
        <el-button v-if="config.token" :icon="CopyDocument" @click="copy(config.token, '已复制 token')">复制</el-button>
      </div>
      <!-- 安全警告：保持直接展示，不收进 tooltip -->
      <div class="token-warn">
        <el-icon class="warn-ico"><WarningFilled /></el-icon>
        <span>妥善保管 token：任何持有该 token 的客户端均可调用本服务执行操作。</span>
      </div>
    </div>

    <!-- 客户端配置示例 -->
    <div class="form-card">
      <div class="card-head">
        <span class="card-ico"><el-icon><Document /></el-icon></span>
        <span class="card-title-text">客户端配置示例</span>
        <HelpTip content="将以下配置加入 Claude Desktop 的 claude_desktop_config.json（或 Cursor 的 MCP 设置）。需先启动服务并生成 token。" />
      </div>
      <div v-if="!status.running || !config.token" class="config-empty">
        <el-alert type="info" :closable="false" show-icon>
          请先完成配置（{{ clientMode ? "直连模式无需绑定实例" : multiMode ? "勾选授权机器" : "绑定资源" }}）、生成 token 并启动服务，配置将自动生成。
        </el-alert>
      </div>
      <div v-else class="json-block">
        <div class="json-head">
          <span class="json-dots"><i class="jd-r" /><i class="jd-y" /><i class="jd-g" /></span>
          <span class="json-name">mcp-client-config.json</span>
          <el-button :icon="CopyDocument" size="small" text bg @click="copy(clientConfig, '已复制配置 JSON')">
            复制配置
          </el-button>
        </div>
        <pre class="config-json">{{ clientConfig }}</pre>
      </div>
    </div>
  </div>
</template>

<style scoped>
.instance-panel {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 2px 2px 8px;
}

/* --- 卡片 --- */
.form-card {
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 10px;
  padding: 14px 16px 16px;
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.03);
}

/* 卡片头：图标徽标 + 标题（+ 右侧动作） */
.card-head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding-bottom: 10px;
  margin-bottom: 14px;
  border-bottom: 1px solid var(--el-border-color-lighter);
}
.card-ico {
  width: 24px;
  height: 24px;
  border-radius: 6px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 13px;
  flex-shrink: 0;
  color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
}
.card-title-text {
  font-size: 13.5px;
  font-weight: 600;
  letter-spacing: 0.2px;
  color: var(--el-text-color-primary);
}
.head-actions {
  margin-left: auto;
  display: flex;
  gap: 8px;
}

/* --- 表单字段 --- */
.field-row {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin-bottom: 14px;
}
.field-label {
  font-size: 12.5px;
  color: var(--el-text-color-regular);
  font-weight: 500;
  display: flex;
  align-items: center;
  gap: 4px;
}
.required {
  color: var(--el-color-danger);
}
.field-control {
  width: 100%;
}
.mono-input :deep(input) {
  font-family: var(--app-font-mono);
  font-size: 12.5px;
}
.addr-row {
  display: flex;
  gap: 12px;
}
.addr-row .flex1 {
  flex: 1;
}
.addr-row .field-row {
  margin-bottom: 10px;
}
.port-field {
  max-width: 160px;
}

.client-mode-alert {
  font-size: 12px;
  line-height: 1.8;
}
.client-mode-alert code {
  background: var(--el-fill-color-light);
  padding: 1px 4px;
  border-radius: 3px;
  font-family: var(--app-font-mono);
}
.hint-text {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  margin-bottom: 4px;
}
.warn-inline {
  color: var(--el-color-danger);
}
/* el-alert 与上方字段行间距：负上边距抵消 .field-row 的 margin-bottom，得到紧凑的 10px 间隙 */
.alert-gap {
  margin-top: -4px;
  margin-bottom: 14px;
}

/* --- 状态 hero --- */
.status-hero {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 12px 14px;
  border-radius: 8px;
  margin-bottom: 12px;
  border: 1px solid var(--el-border-color-lighter);
  background: var(--el-fill-color-light);
  transition: background 0.25s, border-color 0.25s;
}
.status-hero.is-on {
  border-color: var(--el-color-success-light-7);
  background: var(--el-color-success-light-9);
}
.hero-dot {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  background: var(--el-text-color-disabled);
  flex-shrink: 0;
}
.is-on .hero-dot {
  background: var(--el-color-success);
  animation: hero-pulse 2s ease-out infinite;
}
@keyframes hero-pulse {
  0% { box-shadow: 0 0 0 0 color-mix(in srgb, var(--el-color-success) 45%, transparent); }
  70% { box-shadow: 0 0 0 8px transparent; }
  100% { box-shadow: 0 0 0 0 transparent; }
}
.hero-text {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}
.hero-state {
  font-size: 14px;
  font-weight: 600;
  line-height: 1.2;
  color: var(--el-text-color-primary);
}
.is-on .hero-state {
  color: var(--el-color-success);
}
.hero-desc {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  line-height: 1.4;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* --- 端点行 --- */
.endpoint-row {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 14px;
  min-width: 0;
}
.endpoint-tag {
  font-size: 10px;
  font-weight: 700;
  letter-spacing: 0.5px;
  padding: 2px 6px;
  border-radius: 4px;
  flex-shrink: 0;
  font-family: var(--app-font-mono);
  color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
  border: 1px solid var(--el-color-primary-light-8);
  user-select: none;
}
.endpoint-url {
  font-family: var(--app-font-mono);
  font-size: 12.5px;
  color: var(--el-color-primary);
  word-break: break-all;
  min-width: 0;
}

/* --- 开关行 --- */
.switch-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  padding: 11px 0;
  border-top: 1px dashed var(--el-border-color-lighter);
}
.switch-row:first-of-type {
  border-top: none;
}
.switch-info {
  min-width: 0;
}
.switch-title {
  font-size: 13px;
  font-weight: 500;
  color: var(--el-text-color-primary);
  display: flex;
  align-items: center;
  gap: 4px;
}
.switch-desc {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  margin-top: 2px;
  line-height: 1.4;
}

/* --- 令牌 --- */
.token-row {
  display: flex;
  gap: 8px;
  align-items: center;
}
.token-input {
  flex: 1;
  min-width: 0;
}
.token-input :deep(input) {
  font-family: var(--app-font-mono);
  font-size: 12.5px;
}
.token-warn {
  display: flex;
  align-items: flex-start;
  gap: 6px;
  margin-top: 10px;
  padding: 8px 10px;
  border-radius: 6px;
  font-size: 12px;
  line-height: 1.6;
  color: var(--el-color-warning);
  background: var(--el-color-warning-light-9);
  border: 1px solid var(--el-color-warning-light-8);
}
.warn-ico {
  font-size: 14px;
  flex-shrink: 0;
  margin-top: 2px;
}

/* --- 客户端配置 JSON 块（迷你终端窗口） --- */
.config-empty {
  margin-top: 4px;
}
.json-block {
  border-radius: 8px;
  overflow: hidden;
  border: 1px solid #2a3442;
  margin-top: 2px;
}
.json-head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 10px;
  background: #1c2330;
  border-bottom: 1px solid #2a3442;
}
.json-dots {
  display: inline-flex;
  gap: 5px;
  flex-shrink: 0;
}
.json-dots i {
  width: 9px;
  height: 9px;
  border-radius: 50%;
}
.jd-r { background: #ff5f57; }
.jd-y { background: #febc2e; }
.jd-g { background: #28c840; }
.json-name {
  font-family: var(--app-font-mono);
  font-size: 11px;
  color: #7d8590;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.json-head .el-button {
  margin-left: auto;
  color: #adbac7;
  background: #2a3442;
  border-color: #3a4656;
}
.json-head .el-button:hover {
  color: #e6edf3;
  background: #344050;
  border-color: #46536a;
}
.config-json {
  background: #161b22;
  color: #7ee787;
  padding: 12px 14px;
  font-size: 12px;
  line-height: 1.6;
  overflow: auto;
  margin: 0;
  max-height: 260px;
  font-family: var(--app-font-mono);
}
</style>
