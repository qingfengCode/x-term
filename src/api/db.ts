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

/** 切换连接的当前库（schema）。之后该连接上的查询自动带 USE，SQL 无需库前缀。 */
export function dbUseDatabase(connId: string, database: string | null): Promise<void> {
  return invoke<void>("db_use_database", { connId, database });
}

/**
 * 执行 SQL，结果通过 db:query_result 事件推送（含 queryId）。
 * 注意：本 invoke 会等到后端 SQL 执行完才 resolve；事件先于 invoke resolve 到达。
 * @param readOnly 只读模式：后端强制校验（前端判定可被绕过，必须由后端兜底）。
 */
export function dbExecSql(
  connId: string,
  sql: string,
  queryId: string,
  readOnly = false
): Promise<void> {
  return invoke<void>("db_exec_sql", { connId, sql, queryId, readOnly });
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

// ---------------------------------------------------------------------------
// 多厂商扩展（能力开关 / 浏览模式 / 脚本执行）
// ---------------------------------------------------------------------------

/** 连接的方言能力（UI 显隐的单一事实来源，前端不写 if kind ===）。 */
export interface DbCapabilities {
  supportsComment: boolean;
  supportsAutoIncrement: boolean;
  supportsCreateDatabase: boolean;
  multipleDatabases: boolean;
  /** 类型建议列表（建表/改列 UI 用）。 */
  columnTypes: string[];
}

/** 连接的方言能力。 */
export function dbCapabilities(connId: string): Promise<DbCapabilities> {
  return invoke<DbCapabilities>("db_capabilities", { connId });
}

/**
 * 生成分页浏览 SELECT（只生成文本不执行）：方言分页差异（LIMIT/OFFSET vs
 * FETCH）收敛在后端。点表浏览/翻页时调用。
 */
export function dbDefaultTableQuery(
  connId: string,
  table: string,
  limit: number,
  offset: number
): Promise<string> {
  return invoke<string>("db_default_table_query", { connId, table, limit, offset });
}

/** 脚本中一条语句的执行结果。 */
export interface ScriptStmtResult {
  /** 语句起始行号（1-based，失败定位）。 */
  line: number;
  /** 语句前 200 字符预览。 */
  sqlPreview: string;
  affected: number;
  /** 出错信息；null = 成功。 */
  error: string | null;
}

/** 按方言切分 SQL 脚本并逐条执行（首错即停）。 */
export function dbExecuteScript(
  connId: string,
  script: string,
  readOnly: boolean
): Promise<ScriptStmtResult[]> {
  return invoke<ScriptStmtResult[]>("db_execute_script", { connId, script, readOnly });
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

/** ask_user_question 的单个回答（提交问题表单时逐题回传）。 */
export interface AskUserAnswer {
  id: string;
  /** 勾选的选项 label（无选项/未勾选为空数组）。 */
  selected?: string[];
  /** 自由输入文本（单选时覆盖 selected）。 */
  custom?: string | null;
}

/**
 * 回传 ask_user_question 的用户回答（前端问题表单「提交/取消」触发）。
 * `answered=false`（取消）时 answers 应为空数组。
 */
export function aiAskUserRespond(
  toolCallId: string,
  answered: boolean,
  answers: AskUserAnswer[],
): Promise<void> {
  return invoke<void>("ai_ask_user_respond", { toolCallId, answered, answers });
}

/**
 * 把一条命令前缀加入白名单并持久化到 settings.json。
 * 卡片"加入白名单并执行"按钮触发；后端只取首个 token 作为白名单条目。
 */
export function aiAddToWhitelist(command: string): Promise<void> {
  return invoke<void>("ai_add_to_whitelist", { command });
}
