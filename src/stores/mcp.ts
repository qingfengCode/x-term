import { defineStore } from "pinia";
import { ref } from "vue";
import * as mcpApi from "@/api/mcp";
import type { McpKind, McpInstanceConfig, McpServerStatus, McpApprovalRequest } from "@/api/mcp";

/**
 * MCP（Model Context Protocol）服务端状态。
 *
 * 三个独立 MCP 实例：
 * - SSH MCP（ssh）：对外暴露 exec_ssh + 文件工具，绑定一个 SSH 会话。
 * - DB MCP（db）：对外暴露 exec_sql，绑定一个 DB profile。
 * - File MCP（file）：对外暴露 list_files/upload_file/download_file，绑定一个 S3 文件账号。
 * 各自独立配置（host/port/token/resourceId）与运行状态。
 *
 * 待确认请求队列（pendingApprovals）由全局浮层 McpApprovalToast 呈现——外部客户端
 * 发起的工具调用与当前页面无关，属全局事件。
 */
export const useMcpStore = defineStore("mcp", () => {
  /** 三个 kind 各自的配置（从 mcp.json 加载）。 */
  const sshConfig = ref<McpInstanceConfig>(makeDefault("ssh"));
  const dbConfig = ref<McpInstanceConfig>(makeDefault("db"));
  const fileConfig = ref<McpInstanceConfig>(makeDefault("file"));

  /** 三个 kind 各自的运行状态。 */
  const sshStatus = ref<McpServerStatus>({ running: false, host: "", port: 0, endpoint: "" });
  const dbStatus = ref<McpServerStatus>({ running: false, host: "", port: 0, endpoint: "" });
  const fileStatus = ref<McpServerStatus>({ running: false, host: "", port: 0, endpoint: "" });

  /** 各 kind 操作的按钮 loading。 */
  const loading = ref<Record<McpKind, boolean>>({ ssh: false, db: false, file: false });

  /** 待确认的请求队列（按到达顺序）。 */
  const pendingApprovals = ref<McpApprovalRequest[]>([]);

  /** 该 kind 的默认配置。 */
  function makeDefault(kind: McpKind): McpInstanceConfig {
    const port = kind === "ssh" ? 8765 : kind === "db" ? 8766 : 8767;
    return {
      enabled: false,
      host: "0.0.0.0",
      port,
      token: undefined,
      resourceId: undefined,
      resourceMode: "bound",
      boundDatabase: undefined,
      autoApprove: false,
      enableLog: true,
    };
  }

  /** 取该 kind 的配置/状态引用。 */
  function configOf(kind: McpKind) {
    if (kind === "ssh") return sshConfig;
    if (kind === "db") return dbConfig;
    return fileConfig;
  }
  function statusOf(kind: McpKind) {
    if (kind === "ssh") return sshStatus;
    if (kind === "db") return dbStatus;
    return fileStatus;
  }

  /** 加载三个 kind 的配置与状态。 */
  async function loadAll() {
    await Promise.all([
      loadConfig("ssh"),
      loadConfig("db"),
      loadConfig("file"),
      refresh("ssh"),
      refresh("db"),
      refresh("file"),
    ]);
  }

  /** 加载该 kind 的配置。 */
  async function loadConfig(kind: McpKind) {
    try {
      configOf(kind).value = await mcpApi.mcpLoadConfig(kind);
    } catch {
      /* 读失败保持默认 */
    }
  }

  /** 保存该 kind 的配置（不重启服务）。 */
  async function saveConfig(kind: McpKind, cfg?: McpInstanceConfig) {
    const c = cfg ?? configOf(kind).value;
    await mcpApi.mcpSaveConfig(kind, c);
    configOf(kind).value = { ...c };
  }

  /** 拉取该 kind 的服务端状态。 */
  async function refresh(kind: McpKind) {
    try {
      statusOf(kind).value = await mcpApi.mcpStatus(kind);
    } catch {
      /* ignore */
    }
  }

  /** 启动该 kind 的服务端（用配置中的 host/port/token/resourceId）。成功后刷新状态。 */
  async function start(kind: McpKind) {
    loading.value[kind] = true;
    try {
      statusOf(kind).value = await mcpApi.mcpStart(kind);
      return statusOf(kind).value;
    } finally {
      loading.value[kind] = false;
    }
  }

  /** 停止该 kind 的服务端。 */
  async function stop(kind: McpKind) {
    loading.value[kind] = true;
    try {
      await mcpApi.mcpStop(kind);
      statusOf(kind).value = { running: false, host: "", port: 0, endpoint: "" };
    } finally {
      loading.value[kind] = false;
    }
  }

  /** 重新生成该 kind 的 token 并写回配置。 */
  async function regenerateToken(kind: McpKind) {
    const t = await mcpApi.mcpGenerateToken(kind);
    configOf(kind).value.token = t;
    return t;
  }

  /** 收到一个确认请求（由 MainLayout 的事件监听转发进来）。 */
  function onApprovalRequest(req: McpApprovalRequest) {
    pendingApprovals.value.push(req);
  }

  /** 确认请求过期（后端超时自动拒绝后 emit），移除对应浮层卡片。 */
  function onApprovalExpired(requestId: string) {
    pendingApprovals.value = pendingApprovals.value.filter((r) => r.requestId !== requestId);
  }

  /** 用户点"允许/拒绝"。向后端回结果并从队列移除。
   *  即使后端返回错误（如请求已超时清理），仍从前端队列移除，避免卡片残留。 */
  async function respond(requestId: string, approved: boolean) {
    try {
      await mcpApi.mcpRespondApproval(requestId, approved);
    } finally {
      pendingApprovals.value = pendingApprovals.value.filter((r) => r.requestId !== requestId);
    }
  }

  return {
    sshConfig,
    dbConfig,
    fileConfig,
    sshStatus,
    dbStatus,
    fileStatus,
    loading,
    pendingApprovals,
    loadAll,
    loadConfig,
    saveConfig,
    refresh,
    start,
    stop,
    regenerateToken,
    onApprovalRequest,
    onApprovalExpired,
    respond,
  };
});
