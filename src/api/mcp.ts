// MCP（Model Context Protocol）服务端 API。
//
// X-Term 作为 MCP 服务端，对外暴露三类独立 MCP 实例：
// - SSH MCP（kind="ssh"）：对外暴露 exec_ssh + 文件工具，绑定到一个 SSH 会话。
// - DB MCP（kind="db"）：对外暴露 exec_sql，绑定到一个 DB profile。
// - File MCP（kind="file"）：对外暴露 list_files/upload_file/download_file，
//   绑定到一个 S3 文件账号（仅 bound 模式）。
// 三者各自独立启停、监听地址/端口/token/绑定资源。配置持久化在 mcp.json。

import { invoke } from "@tauri-apps/api/core";

/** MCP 实例种类。 */
export type McpKind = "ssh" | "db" | "file";

/**
 * MCP 绑定来源（仅 bound 模式有效）。
 * - "config"（默认）：绑定会话配置，执行时新建短连接。
 * - "terminal"：绑定已打开的终端标签页，命令写入该终端 PTY 执行
 *   （支持 A→B→C 跳板嵌套场景）。仅 SSH kind 支持。
 */
export type McpBoundSource = "config" | "terminal";

/** 单个 MCP 实例的配置（持久化在 mcp.json，每个 kind 一份）。 */
export interface McpInstanceConfig {
  /** 是否启用（记录意图；实际启停以 mcpStart/mcpStop 为准）。 */
  enabled: boolean;
  /** 监听地址，默认 127.0.0.1（仅本机）；可配置 0.0.0.0 / 局域网 IP 对外开放。 */
  host: string;
  /** 监听端口（ssh 默认 8765、db 默认 8766）。 */
  port: number;
  /** Bearer token（未生成则 undefined）。 */
  token?: string;
  /**
   * 绑定的资源 id：SSH 会话 id / DB profile id（boundSource="config"）或
   * 终端实例 id（boundSource="terminal"）。仅 bound 模式必填。
   */
  resourceId?: string;
  /**
   * 多机模式（resourceMode="multi"，仅 SSH）勾选的 SSH 会话 id 集合。
   */
  resourceIds?: string[];
  /**
   * 资源模式："bound"（绑定本地资源，默认）| "client"（客户端直连，免绑定实例）
   * | "multi"（多机模式，仅 SSH：勾选一组机器，由外部 AI 按工具参数 target 自选目标）
   * | "bastion"（堡垒机模式，仅 SSH：绑定堡垒机会话配置，以「会话」为单位按需
   *   进出资产主机——bastion_list_hosts / bastion_create_session / bastion_session_exec
   *   / bastion_upload_file / bastion_close_session / bastion_list_sessions）。
   * client 模式下调用方需在工具参数中传 host/port/username/password，
   * 凭据仅本次调用有效、不存储不落日志。
   */
  resourceMode: "bound" | "client" | "multi" | "bastion";
  /**
   * 绑定来源（仅 bound 模式）："config"（会话配置，默认）| "terminal"（终端标签页）。
   * terminal 来源下命令写入该终端 PTY 执行，支持 A→B→C 跳板嵌套。
   */
  boundSource: McpBoundSource;
  /** 绑定的具体数据库名（仅 db kind）。设置后 exec_sql 只针对该库。 */
  boundDatabase?: string;
  /**
   * 运行模式（与 AI 助手的执行模式语义一致）：
   * - "manual"（默认）：所有写/执行类调用人工确认；
   * - "whitelist"：白名单内自动放行（SSH=命令白名单、DB=只读 SQL），其余确认；
   * - "auto"：全部自动执行（文件传输工具仍强制人工确认）。
   */
  runMode: "manual" | "whitelist" | "auto";
  /** 是否记录执行日志到文本文件（每次启动生成一个日志文件）。默认 true。 */
  enableLog: boolean;
  /** 堡垒机目标主机会话空闲自动回收时长（分钟，仅 ssh + bastion 模式）。0 = 不回收。默认 15。 */
  idleTimeoutMinutes: number;
  /**
   * 堡垒机「登录后命令」（仅 ssh + bastion 模式）：进入目标主机后自动执行一次，
   * 之后的命令都在其上下文（如 `sudo su -` 的 root 登录 shell）中执行。
   * 空 / 未配置 = 不执行。要求无需交互输入（提权请配免密 sudo）。
   */
  postLoginCommand?: string;
  /**
   * 堡垒机基础连接（完成 MFA 的那条）空闲保持时长（分钟，仅 ssh + bastion 模式）。
   * 期间新请求复用该连接、不重复要求 MFA；0 = 不保持（最后一个会话关闭即断开）。默认 30。
   */
  baseIdleMinutes: number;
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

/**
 * 运行中热切换绑定的资源（会话配置 / 终端标签页），立即生效无需重启。
 *
 * - `boundSource`："config"（会话配置）| "terminal"（终端标签页，仅 ssh kind）。
 * - `resourceId`：会话配置 id 或终端实例 id。
 *
 * 同时持久化到 mcp.json。服务未运行时只保存配置（启动时生效）。
 */
export function mcpRebind(
  kind: McpKind,
  boundSource: McpBoundSource,
  resourceId: string,
): Promise<void> {
  return invoke<void>("mcp_rebind", { kind, boundSource, resourceId });
}

/**
 * 运行中热切换多机模式的机器集合（仅 ssh kind + multi 模式）。
 * 返回 true = 已热切换即时生效；false = 未热切换（服务未运行或以其它模式
 * 运行），配置已保存、（重）启动时生效。同时持久化到 mcp.json。
 */
export function mcpRebindMulti(kind: McpKind, resourceIds: string[]): Promise<boolean> {
  return invoke<boolean>("mcp_rebind_multi", { kind, resourceIds });
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
