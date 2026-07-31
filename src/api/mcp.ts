// MCP（Model Context Protocol）服务端 API。
//
// X-Term 作为 MCP 服务端，对外暴露两类独立 MCP 实例：
// - SSH MCP（kind="ssh"）：对外暴露 exec_ssh，绑定到一个 SSH 会话。
// - DB MCP（kind="db"）：对外暴露 exec_sql，绑定到一个 DB profile。
// 两者各自独立启停、监听地址/端口/token/绑定资源。配置持久化在 mcp.json。

import { invoke } from "@tauri-apps/api/core";

/** MCP 实例种类。 */
export type McpKind = "ssh" | "db";

/** 单个 MCP 实例的配置（持久化在 mcp.json，每个 kind 一份）。 */
export interface McpInstanceConfig {
  /** 是否启用（记录意图；实际启停以 mcpStart/mcpStop 为准）。 */
  enabled: boolean;
  /** 监听地址，默认 0.0.0.0。 */
  host: string;
  /** 监听端口（ssh 默认 8765、db 默认 8766）。 */
  port: number;
  /** Bearer token（未生成则 undefined）。 */
  token?: string;
  /** 绑定的资源 id：SSH 会话 id（ssh）或 DB profile id（db）。 */
  resourceId?: string;
  /** 绑定的具体数据库名（仅 db kind）。设置后 exec_sql 只针对该库。 */
  boundDatabase?: string;
  /** 自动放行：开启后 exec_ssh/exec_sql 跳过人工确认直接执行。默认 false。 */
  autoApprove: boolean;
  /** 是否记录执行日志到文本文件（每次启动生成一个日志文件）。默认 true。 */
  enableLog: boolean;
}

/** MCP 服务端运行状态。 */
export interface McpServerStatus {
  running: boolean;
  host: string;
  port: number;
  /** 客户端连接的 SSE 端点（完整 URL）。 */
  endpoint: string;
}

/** 启动指定 kind 的 MCP 服务端。
 *  host/port 省略时用配置默认值。需已生成 token 且已绑定资源，否则后端报错。 */
export function mcpStart(kind: McpKind, host?: string, port?: number): Promise<McpServerStatus> {
  return invoke<McpServerStatus>("mcp_start", { kind, host, port });
}

/** 停止指定 kind 的 MCP 服务端。 */
export function mcpStop(kind: McpKind): Promise<void> {
  return invoke<void>("mcp_stop", { kind });
}

/** 查询指定 kind 的服务端状态。 */
export function mcpStatus(kind: McpKind): Promise<McpServerStatus> {
  return invoke<McpServerStatus>("mcp_status", { kind });
}

/** 保存指定 kind 的配置（绑定资源 / host / port / enabled / token）。
 *  不直接重启服务；若服务在运行，前端需先 stop 再 start 生效。 */
export function mcpSaveConfig(kind: McpKind, config: McpInstanceConfig): Promise<void> {
  return invoke<void>("mcp_save_config", { kind, config });
}

/** 读取指定 kind 的配置。 */
export function mcpLoadConfig(kind: McpKind): Promise<McpInstanceConfig> {
  return invoke<McpInstanceConfig>("mcp_load_config", { kind });
}

/** 为指定 kind 生成随机 token（写入 mcp.json）并返回。 */
export function mcpGenerateToken(kind: McpKind): Promise<string> {
  return invoke<string>("mcp_generate_token", { kind });
}

/** 外部 MCP 客户端请求执行操作时的确认事件 payload。
 *
 * `arguments` 是工具参数的 JSON 对象（后端 serde_json::Value 原样透传，
 * camelCase 由 serde 自动转换）。 */
export interface McpApprovalRequest {
  requestId: string;
  /** 哪个 MCP 服务端发起的确认（ssh / db）。 */
  kind: McpKind;
  toolName: "exec_ssh" | "exec_sql" | string;
  arguments: Record<string, unknown>;
  description: string;
  clientName: string;
  /** 该 MCP 当前绑定的资源名（SSH 会话名 / DB profile 名）。 */
  resourceName: string;
}

/** 响应 MCP 确认请求（前端浮层后用户点允许/拒绝）。
 *  返回是否命中 pending 请求。 */
export function mcpRespondApproval(requestId: string, approved: boolean): Promise<boolean> {
  return invoke<boolean>("mcp_respond_approval", { requestId, approved });
}

/** 执行日志内容（mcp_log 命令返回）。 */
export interface McpLogContent {
  /** 日志文件名（无日志时为空）。 */
  filename: string;
  /** 日志内容（最近 maxLines 行）。 */
  content: string;
  /** 日志文件是否存在。 */
  exists: boolean;
}

/** 读取指定 kind 的最新日志文件尾部内容（日志面板轮询用）。 */
export function mcpLog(kind: McpKind, maxLines?: number): Promise<McpLogContent> {
  return invoke<McpLogContent>("mcp_log", { kind, maxLines });
}
