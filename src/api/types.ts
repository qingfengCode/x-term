// 与后端 Rust 结构对应的 TypeScript 类型。

export enum AuthType {
  Password = "Password",
  PrivateKey = "PrivateKey",
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

/** 模型参数默认值（与后端 provider.rs 的 serde default 保持一致）。 */
export const PROVIDER_DEFAULTS = {
  maxOutput: 16000,
  contextWindow: 184000,
  maxToolCalls: 200,
  temperature: null,
  connectTimeoutSecs: 15,
  readTimeoutSecs: 300,
} as const;

export interface ProviderConfig {
  kind: ProviderKind;
  baseUrl: string;
  apiKey: string;
  model: string;
  /** 单次请求最大输出 tokens（请求体 max_tokens）。 */
  maxOutput: number;
  /** 上下文窗口大小（tokens），超出部分的历史消息发送前被裁剪。 */
  contextWindow: number;
  /** 智能体模式最大工具调用数。 */
  maxToolCalls: number;
  /** 采样温度；null 表示不发送，由服务端默认。 */
  temperature: number | null;
  /** 建连超时（秒）；0 表示使用默认值。 */
  connectTimeoutSecs: number;
  /** 读取（流式响应）超时（秒）；0 表示使用默认值。长思考模型需调大。 */
  readTimeoutSecs: number;
}

export interface TerminalSettings {
  theme: string;
  fontFamily: string;
  fontSize: number;
  lineHeight: number;
  scrollback: number;
  copyOnSelect: boolean;
  enableWebgl: boolean;
  /** SSH 空闲断开时间（分钟），0 = 永不自动断开。 */
  sshIdleTimeoutMinutes: number;
}

/** 工具运行模式（SSH / SQL 智能体各自独立设置）。 */
export type ToolRunMode = "manual" | "auto" | "whitelist";

/** 运行模式选项（设置页下拉用，与后端 config.rs 常量保持一致）。 */
export const RUN_MODE_OPTIONS: { value: ToolRunMode; label: string; desc: string }[] = [
  {
    value: "manual",
    label: "手动运行",
    desc: "所有工具调用都弹确认，批准后才执行",
  },
  {
    value: "auto",
    label: "自动运行",
    desc: "所有工具调用自动执行（含危险操作），不弹确认",
  },
  {
    value: "whitelist",
    label: "白名单运行",
    desc: "白名单内（SSH 命令白名单 / SQL 只读查询）自动执行，其余确认",
  },
];

/** SSH 智能体配置（exec_ssh 工具）。 */
export interface SshAgentSettings {
  /** exec_ssh 命令白名单（命令前缀）。命中→绿色卡片；whitelist 模式下自动放行。 */
  commandWhitelist: string[];
  /** 运行模式：manual 手动 / auto 自动 / whitelist 白名单运行。 */
  runMode: ToolRunMode;
  /** 终端可视化：true 时 exec_ssh 写入活动终端 PTY，命令实时显示。 */
  terminalVisualization: boolean;
}

/** SQL 执行模式。 */
export type SqlExecMode = "readonly" | "restricted" | "full";

/** SQL 智能体配置（exec_sql 工具）。 */
export interface SqlAgentSettings {
  /** SQL 执行模式：readonly 只读 / restricted 允许 DML / full 允许一切。 */
  sqlMode: SqlExecMode;
  /** 运行模式：manual 手动 / auto 自动 / whitelist 白名单运行（只读查询自动放行）。 */
  runMode: ToolRunMode;
  /** 终端可视化：true 时 AI 执行的 SQL 及结果回显到 SQL 控制台（命令行模式）。
   *  与 SSH 智能体的 terminalVisualization 独立设置。 */
  terminalVisualization: boolean;
}

/** AI 本地文件读写配置（read_file / write_file / list_files 工具）。 */
export interface FileAccessSettings {
  /** 是否启用本地文件读写。关闭时 AI 行为与之前完全一致。 */
  enabled: boolean;
  /** 各助手域的工作目录：key = "ssh" | "db"，值为绝对路径。 */
  workspaceDirs: Record<string, string>;
}

/** 一条可复用 skill（由历史对话总结生成，注入对应 domain 的 system prompt）。 */
export interface SkillConfig {
  id: string;
  title: string;
  /** skill 内容（直接作为系统提示词片段注入）。 */
  content: string;
  /** 所属助手域："ssh" | "db"。 */
  domain: "ssh" | "db";
  enabled: boolean;
}

export interface AiSettings {
  providers: ProviderConfig[];
  active: string | null;
  /** SSH 智能体配置。 */
  sshAgent: SshAgentSettings;
  /** SQL 智能体配置。 */
  sqlAgent: SqlAgentSettings;
  /** 本地文件读写配置。 */
  fileAccess: FileAccessSettings;
  /** 可复用 skill 列表（注入对应 domain 的 system prompt）。 */
  skills?: SkillConfig[];
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
  /** 所属分组名称，为空表示未分组。 */
  group?: string | null;
}

export interface ShortcutSettings {
  commands: ShortcutCommand[];
  /** 有序分组名列表（用于标签页排序）。 */
  groups?: string[];
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
  { action: "newTab", label: "新建终端", description: "快速连接最近一次成功连接的会话", defaultKey: "Ctrl+T" },
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
  /** 会话侧栏宽度（px）。 */
  sidebarWidth: number;
  /** 最近成功连接的会话 id（最近的在前）。 */
  recentSessionIds: string[];
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
  groupId: string | null;
  createdAt: string;
}

/** 数据库连接分组。 */
export interface DbGroup {
  id: string;
  name: string;
  parentId: string | null;
  sortOrder: number;
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

// ---------------------------------------------------------------------------
// 应用自更新
// ---------------------------------------------------------------------------

/** 更新清单（自建服务器上的 update.json）。对应后端 UpdateManifest。 */
export interface UpdateManifest {
  /** 远端最新版本号。 */
  version: string;
  /** 更新日志 / 发布说明。 */
  notes: string;
  /** 安装包下载地址。 */
  url: string;
  /** 安装包 sha256（可选）。 */
  sha256: string | null;
}

/** 更新下载进度事件。对应后端 update:progress。 */
export interface UpdateProgressEvent {
  received: number;
  total: number;
  percent: number;
}

/** 关于页应用信息。对应后端 UpdateInfo。 */
export interface UpdateInfo {
  currentVersion: string;
  manifestUrl: string;
  dataDir: string;
  tauriVersion: string;
}

