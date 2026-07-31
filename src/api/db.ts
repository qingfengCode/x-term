import { invoke } from "@tauri-apps/api/core";
import type { DbGroup, DbProfile, QueryResult } from "./types";

export function dbListProfiles(): Promise<DbProfile[]> {
  return invoke<DbProfile[]>("db_list_profiles");
}

export function dbSaveProfile(profile: DbProfile): Promise<void> {
  return invoke<void>("db_save_profile", { profile });
}

export function dbDeleteProfile(id: string): Promise<void> {
  return invoke<void>("db_delete_profile", { id });
}

// --- DB 分组 ---
export function dbListGroups(): Promise<DbGroup[]> {
  return invoke<DbGroup[]>("db_list_groups");
}

export function dbSaveGroup(group: DbGroup): Promise<void> {
  return invoke<void>("db_save_group", { group });
}

export function dbDeleteGroup(id: string): Promise<void> {
  return invoke<void>("db_delete_group", { id });
}

/** 连接一个 DB profile，返回 connId。 */
export function dbConnect(profileId: string): Promise<string> {
  return invoke<string>("db_connect", { profileId });
}

export function dbDisconnect(connId: string): Promise<void> {
  return invoke<void>("db_disconnect", { connId });
}

/**
 * 执行 SQL，结果通过 db:query_result 事件推送（含 queryId）。
 * 注意：本 invoke 会等到后端 SQL 执行完才 resolve；事件先于 invoke resolve 到达。
 */
export function dbExecSql(connId: string, sql: string, queryId: string): Promise<void> {
  return invoke<void>("db_exec_sql", { connId, sql, queryId });
}

/** 列出表。database 省略时列当前库；指定时列该库（SHOW TABLES FROM <db>）。 */
export function dbListTables(connId: string, database?: string): Promise<string[]> {
  return invoke<string[]>("db_list_tables", { connId, database: database ?? null });
}

/** 列出服务器上所有可访问的数据库（SHOW DATABASES）。 */
export function dbListDatabases(connId: string): Promise<string[]> {
  return invoke<string[]>("db_list_databases", { connId });
}

export function dbDescribeTable(connId: string, table: string): Promise<QueryResult> {
  return invoke<QueryResult>("db_describe_table", { connId, table });
}

/** 获取 SHOW CREATE TABLE 的 DDL 文本（用于 AI 拖表附加表结构上下文）。 */
export function dbShowCreateTable(
  connId: string,
  database: string | null,
  table: string
): Promise<string> {
  return invoke<string>("db_show_create_table", { connId, database, table });
}

/** 拖拽数据传输用的表节点载荷类型。 */
export interface DraggedTable {
  /** 来源连接 id，用于校验是否与 AI 面板当前连接一致。 */
  connId: string;
  /** 库名（可空，表示当前默认库）。 */
  database: string | null;
  /** 表名。 */
  table: string;
}

// AI 工具调用相关命令。

export function aiExecuteTool(toolCallId: string): Promise<void> {
  return invoke<void>("ai_execute_tool", { toolCallId });
}

export function aiCancelTool(toolCallId: string): Promise<void> {
  return invoke<void>("ai_cancel_tool", { toolCallId });
}

/**
 * 把一条命令前缀加入白名单并持久化到 settings.json。
 * 卡片"加入白名单并执行"按钮触发；后端只取首个 token 作为白名单条目。
 */
export function aiAddToWhitelist(command: string): Promise<void> {
  return invoke<void>("ai_add_to_whitelist", { command });
}
