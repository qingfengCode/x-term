// 与后端 Rust 结构对应的 TypeScript 类型。

export enum AuthType {
  Password = "Password",
  PrivateKey = "PrivateKey",
  Agent = "Agent",
}

/** 会话协议。 */
export type Protocol = "ssh" | "telnet" | "rdp" | "vnc";

export interface Session {
  id: string;
  name: string;
  groupId: string | null;
  host: string;
  port: number;
  username: string;
  authType: AuthType;
  credentialId: string | null;
  keyPath: string | null;
  jumpSessionId: string | null;
  startupScript: string | null;
  tags: string | null;
  color: string | null;
  sortOrder: number;
  createdAt: string;
  updatedAt: string;
  /** 协议：ssh/telnet/rdp/vnc，默认 ssh。 */
  protocol: Protocol;
}

export interface Group {
  id: string;
  name: string;
  parentId: string | null;
  sortOrder: number;
  createdAt: string;
}

export interface FileEntry {
  name: string;
  isDir: boolean;
  size: number;
  modified: string | null;
}

export interface FileMeta {
  size: number;
  isDir: boolean;
  modified: string | null;
}

export enum TunnelKind {
  Local = "Local",
  Remote = "Remote",
  Dynamic = "Dynamic",
}

export interface ForwardRule {
  id: string;
  name: string;
  sessionId: string;
  kind: string;
  localHost: string;
  localPort: number;
  remoteHost: string;
  remotePort: number;
  autoStart: boolean;
  createdAt: string;
}

export enum ProviderKind {
  OpenAi = "openai",
  Anthropic = "anthropic",
  DeepSeek = "deepseek",
  Zhipu = "zhipu",
  Ollama = "ollama",
  OpenAiCompatible = "openai_compatible",
}

export interface ProviderConfig {
  kind: ProviderKind;
  baseUrl: string;
  apiKey: string;
  model: string;
}

export interface TerminalSettings {
  theme: string;
  fontFamily: string;
  fontSize: number;
  lineHeight: number;
  scrollback: number;
  copyOnSelect: boolean;
  enableWebgl: boolean;
}

/** SSH 智能体配置（exec_ssh 工具）。 */
export interface SshAgentSettings {
  /** exec_ssh 命令白名单（命令前缀）。命中→绿色卡片。 */
  commandWhitelist: string[];
  /** 白名单内且非危险时自动放行（免确认按钮）。 */
  autoApproveSafe: boolean;
  /** 终端可视化：true 时 exec_ssh 写入活动终端 PTY，命令实时显示。 */
  terminalVisualization: boolean;
}

/** SQL 执行模式。 */
export type SqlExecMode = "readonly" | "restricted" | "full";

/** SQL 智能体配置（exec_sql 工具）。 */
export interface SqlAgentSettings {
  /** SQL 执行模式：readonly 只读 / restricted 允许 DML / full 允许一切。 */
  sqlMode: SqlExecMode;
  /** 只读查询（SELECT/SHOW/EXPLAIN/DESCRIBE）自动放行（免确认按钮）。 */
  autoApproveSafe: boolean;
  /** 终端可视化：true 时 AI 执行的 SQL 及结果回显到 SQL 控制台（命令行模式）。
   *  与 SSH 智能体的 terminalVisualization 独立设置。 */
  terminalVisualization: boolean;
}

export interface AiSettings {
  providers: ProviderConfig[];
  active: string | null;
  /** SSH 智能体配置。 */
  sshAgent: SshAgentSettings;
  /** SQL 智能体配置。 */
  sqlAgent: SqlAgentSettings;
  /** 旧字段（向后兼容读取，不再写出；store 加载时迁移到 sshAgent）。 */
  commandWhitelist?: string[];
  autoApproveWhitelist?: boolean;
  terminalVisualization?: boolean;
}

/** SQL 执行模式选项（设置页用）。 */
export const SQL_MODE_OPTIONS: { value: SqlExecMode; label: string; desc: string }[] = [
  {
    value: "readonly",
    label: "只读",
    desc: "仅允许 SELECT/SHOW/EXPLAIN/DESCRIBE，最安全",
  },
  {
    value: "restricted",
    label: "受限写",
    desc: "只读 + INSERT/UPDATE/DELETE，DDL 仍需确认",
  },
  {
    value: "full",
    label: "自由",
    desc: "允许一切（危险操作仍走人工确认）",
  },
];

/** 一条快捷命令（终端底部按钮栏 + 可选快捷键绑定）。 */
export interface ShortcutCommand {
  id: string;
  /** 按钮显示文字。 */
  label: string;
  /** 要发送的命令（不含换行，执行时自动追加）。 */
  command: string;
  /** 可选快捷键，如 "Ctrl+1" / "F1"。 */
  shortcut?: string | null;
}

export interface ShortcutSettings {
  commands: ShortcutCommand[];
  /** 应用级快捷键绑定（action -> 组合键字符串）。 */
  app?: AppShortcuts;
}

/**
 * 应用级快捷键动作标识。
 *
 * 这些动作由前端全局 keydown 监听器分发，键可在"设置 → 快捷键"中自定义。
 */
export type AppShortcutAction =
  | "newTab" // 新建终端（连接默认/最近会话）
  | "closeTab" // 关闭当前终端
  | "nextTab" // 下一个终端
  | "prevTab" // 上一个终端
  | "copy" // 复制选中文本
  | "paste" // 粘贴
  | "toggleAi" // 展开/收起 AI 面板
  | "search" // 终端搜索
  | "focusSessions"; // 聚焦会话树搜索框

/** 应用级快捷键绑定：action -> 组合键（如 "Ctrl+T"），值为空表示未绑定。 */
export type AppShortcuts = Partial<Record<AppShortcutAction, string>>;

/** 应用级快捷键动作的元信息（用于设置页展示）。 */
export interface AppShortcutMeta {
  action: AppShortcutAction;
  label: string;
  description: string;
  /** 默认组合键。 */
  defaultKey: string;
}

/** 所有应用级快捷键动作的元信息（顺序即设置页展示顺序）。 */
export const APP_SHORTCUT_METAS: AppShortcutMeta[] = [
  { action: "newTab", label: "新建终端", description: "打开新终端标签页", defaultKey: "Ctrl+T" },
  { action: "closeTab", label: "关闭终端", description: "关闭当前活动终端", defaultKey: "Ctrl+W" },
  { action: "nextTab", label: "下一个终端", description: "切换到右侧标签", defaultKey: "Ctrl+Tab" },
  { action: "prevTab", label: "上一个终端", description: "切换到左侧标签", defaultKey: "Ctrl+Shift+Tab" },
  { action: "copy", label: "复制", description: "复制终端选中文本", defaultKey: "Ctrl+Shift+C" },
  { action: "paste", label: "粘贴", description: "粘贴到终端", defaultKey: "Ctrl+Shift+V" },
  { action: "toggleAi", label: "切换 AI 面板", description: "展开/收起右侧 AI 助手", defaultKey: "Ctrl+I" },
  { action: "search", label: "终端搜索", description: "打开终端搜索", defaultKey: "Ctrl+F" },
  { action: "focusSessions", label: "搜索会话", description: "聚焦会话树过滤框", defaultKey: "Ctrl+P" },
];

/** 默认应用快捷键（从元信息派生）。 */
export function defaultAppShortcuts(): AppShortcuts {
  const out: AppShortcuts = {};
  for (const m of APP_SHORTCUT_METAS) out[m.action] = m.defaultKey;
  return out;
}

export interface Settings {
  terminal: TerminalSettings;
  ai: AiSettings;
  shortcuts: ShortcutSettings;
  firstRun: boolean;
}

export type ChatRole = "system" | "user" | "assistant" | "tool";

export interface ChatMessage {
  role: ChatRole;
  content: string;
  /** assistant 角色且本轮有工具调用时存在。 */
  toolCalls?: ToolCall[];
  /** tool 角色时存在（对应工具调用 id）。 */
  toolCallId?: string;
}

// ---------------------------------------------------------------------------
// AI 工具调用
// ---------------------------------------------------------------------------

export interface ToolCall {
  id: string;
  name: string;
  /** 已解析的参数对象。 */
  arguments: Record<string, unknown>;
}

export interface ToolResult {
  ok: boolean;
  output: string;
}

/** AI 工具调用事件（前端弹确认）。对应后端 ai:tool_call 事件。 */
export interface AiToolCallEvent {
  requestId: string;
  toolCallId: string;
  name: string;
  /** arguments 的 JSON 字符串。 */
  arguments: string;
  description: string;
  dangerous: boolean;
}

/** 工具执行结果事件。对应后端 ai:tool_result 事件。 */
export interface AiToolResultEvent {
  requestId: string;
  toolCallId: string;
  ok: boolean;
  output: string;
}

/** exec_sql 终端可视化回显事件（对应后端 ai:sql_result）。
 *  SQL 控制台（命令行模式）据此把 AI 执行的 SQL 与结构化结果回显进输出流。 */
export interface AiSqlResultEvent {
  /** 触发执行的 AI 请求 id（调试用，控制台不依赖它路由）。 */
  requestId: string;
  /** 执行的 SQL 文本。 */
  sql: string;
  /** 结果列名（非查询语句为空）。 */
  columns: string[];
  /** 结果行（每行按列顺序的字符串值）。 */
  rows: string[][];
  /** 非查询语句的影响行数（SELECT 为行数）。 */
  affected: number;
  /** 执行耗时（毫秒）。 */
  elapsedMs: number;
  /** 执行错误信息（成功为 null）。 */
  error: string | null;
}

// ---------------------------------------------------------------------------
// MySQL DB profile
// ---------------------------------------------------------------------------

export interface DbProfile {
  id: string;
  name: string;
  kind: string; // 目前固定 "mysql"
  host: string;
  port: number;
  username: string;
  defaultDatabase: string | null;
  credentialId: string | null;
  sshSessionConfigId: string | null;
  createdAt: string;
}

export interface QueryResult {
  columns: string[];
  rows: string[][];
  affected: number;
}

/** SQL 控制台查询结果事件。对应后端 db:query_result。 */
export interface DbQueryResultEvent {
  queryId: string;
  columns: string[];
  rows: string[][];
  affected: number;
  error: string | null;
  elapsedMs: number;
}

