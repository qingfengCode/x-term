//! AI 工具调用（Tool Calling）能力。
//!
//! 本模块定义了 X-Term 向 LLM 暴露的工具集合，以及工具执行器与安全护栏。
//!
//! # 概念
//!
//! - [`ToolDef`]：工具定义，发给模型的 JSON Schema 描述。
//! - [`ToolCall`]：模型返回的工具调用请求（id + name + 已解析的参数）。
//! - [`ToolResult`]：工具执行结果，回填给模型。
//! - [`ToolApproval`]：用户对工具调用的确认/拒绝（前端通过命令发回）。
//!
//! # 工具集
//!
//! - `exec_ssh`：在指定 SSH 会话对应的服务器上执行命令（非交互 `channel.exec`）。
//! - `terminal_snapshot`：取指定终端最近输出。
//! - `exec_sql`：在指定 MySQL 连接上执行 SQL。
//! - `list_db_tables`：列出当前数据库的表。
//! - `describe_table`：描述表结构。
//! - `read_file` / `write_file` / `list_files`：本地文件读写（设置页开启"本地文件
//!   读写"后才下发）。只能在各助手工作目录（沙箱）内操作，详见 [`file_tools`]。
//! - `desktop_screenshot` / `desktop_click` / `desktop_type` / `desktop_key`：操作
//!   内嵌 RDP 远程桌面（多模态模型 + 活动 RDP 会话时启用）。执行体在**前端**
//!   （IronRDP WASM 会话），后端只负责下发与等待回执，详见 [`desktop_tools`]。
//! - `todo_write`：任务清单记账工具（agent 模式**始终**下发，不受上下文裁剪）。
//!   整表替换语义，emit `ai:todo` 事件供前端渲染，详见 [`todo_tool`]。
//! - `load_skill`：按标题加载已启用技能全文（该域存在启用技能时才下发），
//!   配合 agent 提示词里的技能目录摘要按需取用，详见 [`skill_tool`]。
//!
//! # 执行流程
//!
//! 1. 调用方（`commands::ai::run_agent_loop`）把 [`all_tools`] 发给模型。
//! 2. 模型返回 `Vec<ToolCall>`，调用方依次：
//!    - emit `ai:tool_call` 事件（含 [`is_dangerous`] 标记与 [`describe_call`] 描述）；
//!    - 通过 `oneshot` 阻塞等待前端确认；
//!    - 调用 [`execute_tool`] 执行，emit `ai:tool_result`；
//!    - 把结果以 role="tool" 消息回填给模型。
//!
//! # 安全
//!
//! [`is_dangerous`] 是一道静态护栏，对若干"灾难性"模式（rm -rf /、mkfs、fork bomb、
//! DROP/TRUNCATE、无 WHERE 的 DELETE 等）返回 true，前端据此红色高亮 + 二次确认。
//! 这并非沙箱：执行端不做拦截，仍按用户最终决定执行。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::AppHandle;
use tokio::time::timeout;

use crate::ai::provider::ImagePart;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::utils::{format_query_result, strip_ansi};

// ===========================================================================
// 类型定义
// ===========================================================================

/// 一个工具的定义（发给模型的 JSON Schema 描述）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema 描述参数。
    pub parameters: Value,
}

/// 模型返回的工具调用请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// 参数（已解析的 JSON 对象）。
    pub arguments: Value,
}

/// 工具执行结果（回填给模型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub ok: bool,
    pub output: String,
}

impl ToolResult {
    pub fn ok<S: Into<String>>(output: S) -> Self {
        Self {
            ok: true,
            output: output.into(),
        }
    }

    pub fn err<S: Into<String>>(output: S) -> Self {
        Self {
            ok: false,
            output: output.into(),
        }
    }
}

/// 用户对工具调用的确认/拒绝（前端通过命令发回）。
pub struct ToolApproval {
    pub approved: bool,
}

/// 桌面工具（desktop_*）的前端执行回执（通过 `ai_desktop_tool_respond` 命令发回）。
///
/// RDP 会话（IronRDP WASM）活在前端，桌面工具的后端侧无法执行——编排层 emit
/// `ai:tool_call` 后阻塞在 [`crate::state::AppState::pending_desktop_calls`]，
/// 等待前端「批准 + 执行」一体回执：
/// - `approved=false`：用户拒绝（或请求被终止）；
/// - `approved=true`：`ok` / `output` 为执行结果；`image` 仅 `desktop_screenshot`
///   附带（mime + base64），编排层把它作为图片消息回填给模型（role=tool 消息
///   不允许携带图片，需追加一条带图的 user 消息）。
pub struct DesktopToolOutcome {
    pub approved: bool,
    pub ok: bool,
    pub output: String,
    pub image: Option<ImagePart>,
}

/// 模型提问的单个问题（`ask_user_question` 参数项）。
///
/// `id` 必须稳定唯一（前端回传答案时原样带回）；`options` 提供可选项，
/// `multi_select` 允许勾选多个（缺省单选）。与 dsh `tool-ask-user` 的
/// question 结构一致。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestion {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub options: Vec<AskUserOption>,
    #[serde(default)]
    pub multi_select: bool,
}

/// `AskUserQuestion` 的可选项。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskUserOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// 用户对某个问题的回答（前端 `ai_ask_user_respond` 发回）。
///
/// `selected` 为勾选的选项 label；`custom` 为自由输入——单选时覆盖 `selected`，
/// 多选时与 `selected` 并存。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskUserAnswer {
    pub id: String,
    #[serde(default)]
    pub selected: Vec<String>,
    #[serde(default)]
    pub custom: Option<String>,
}

/// `ask_user_question` 工具的前端回答回执。
///
/// 执行体在**前端**（用户填写回答）——编排层 emit `ai:tool_call` 后阻塞在
/// [`crate::state::AppState::pending_ask_user_calls`]，等待前端通过
/// `ai_ask_user_respond` 命令回传答案：
/// - `answered=false`：用户取消 / 请求被终止 / 确认超时；
/// - `answered=true`：`answers` 为逐题回答（按 `id` 与问题对应）。
pub struct AskUserOutcome {
    pub answered: bool,
    pub answers: Vec<AskUserAnswer>,
}

// ===========================================================================
// 常量
// ===========================================================================

/// 单命令执行超时（30 秒）。
const EXEC_TIMEOUT: Duration = Duration::from_secs(30);
/// exec_ssh（独立连接模式）输出截断上限（16 KiB）。
const EXEC_OUTPUT_CAP: usize = 16 * 1024;
/// exec_ssh（终端可视化模式）返回给 AI 的输出截断上限（16 KiB）。
///
/// 与 [`EXEC_OUTPUT_CAP`] 保持一致：两种执行模式对回填给模型的输出大小限制相同。
const MAX_EXEC_OUTPUT_BYTES: usize = 16 * 1024;
/// terminal_snapshot 默认返回字节数（16 KiB）。
const SNAPSHOT_DEFAULT_BYTES: usize = 16 * 1024;
/// terminal_snapshot 允许的最大字节数（与终端输出环形缓冲容量一致）。
///
/// 之前上限被死死卡在默认值（8 KiB），模型想取更多也拿不到；现在允许申请
/// 到环形缓冲全量，作为 exec_ssh 可视化模式截断/失败后的兜底路径。
const SNAPSHOT_MAX_BYTES: usize = crate::ssh::session::OUTPUT_BUFFER_CAP;
/// read_file 单文件读取上限（1 MiB）。超出拒绝，防上下文爆炸。
const MAX_FILE_READ_BYTES: usize = 1024 * 1024;
/// write_file 单次写入上限（10 MiB）。
const MAX_FILE_WRITE_BYTES: usize = 10 * 1024 * 1024;
/// list_files 单目录最多返回条目数。
const MAX_LIST_ENTRIES: usize = 200;
/// load_skill 单条技能内容返回上限（4 KiB）。技能内容由对话总结生成（≤500 字），
/// 该上限仅作防御（防用户手动编辑出超长内容撑爆上下文）。
const MAX_SKILL_BYTES: usize = 4 * 1024;

// ===========================================================================
// 工具集
// ===========================================================================

/// 返回全部工具定义。
///
/// 工具参数用 `serde_json::json!` 构造 JSON Schema；厂商实现按 OpenAI 兼容
/// 协议封装（包成 `function.parameters`）。
/// SSH 上下文工具集（有活动终端时启用）。
///
/// - `exec_ssh`：在服务器执行 shell 命令。
/// - `terminal_snapshot`：读取终端最近输出。
pub fn ssh_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "exec_ssh".into(),
            description: "在指定的 SSH 终端会话对应的服务器上执行一条 shell 命令，\
返回标准输出和标准错误的合并文本。适用于查询系统状态（如 ps、df、netstat、\
cat 配置文件等）。单命令超时 30 秒，输出截断 16KB（超出会附截断提示；\
如需更完整内容可调用 terminal_snapshot）。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "sessionId": {
                        "type": "string",
                        "description": "目标 SSH 终端会话的实例 id"
                    },
                    "command": {
                        "type": "string",
                        "description": "要执行的 shell 命令（单条，非交互）"
                    }
                },
                "required": ["sessionId", "command"]
            }),
        },
        ToolDef {
            name: "terminal_snapshot".into(),
            description: "获取指定 SSH 终端会话最近的屏幕输出（默认 16KB，\
可通过 maxBytes 申请最多 256KB），用于了解用户当前看到了什么、上下文是什么。\
命令输出被截断或缺失时，用它获取终端最近输出。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string" },
                    "maxBytes": {
                        "type": "integer",
                        "description": "最多返回的字节数，可省略（默认 8192）",
                        "minimum": 1
                    }
                },
                "required": ["sessionId"]
            }),
        },
    ]
}

/// MySQL 上下文工具集（有活动数据库连接时启用）。
///
/// - `exec_sql`：执行 SQL（默认只读，写操作需额外确认）。
/// - `list_db_tables`：列出当前库的表。
/// - `describe_table`：查看表结构。
pub fn sql_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "exec_sql".into(),
            description: "在指定的 MySQL 连接上执行 SQL 语句。默认只读\
（SELECT/SHOW/EXPLAIN/DESCRIBE）；写操作（INSERT/UPDATE/DELETE/DDL）需要用户在确认时\
额外批准。返回列名和行（最多 100 行）。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "dbConnId": { "type": "string" },
                    "sql": { "type": "string" },
                    "limit": {
                        "type": "integer",
                        "description": "返回行数上限，默认 100",
                        "minimum": 1
                    }
                },
                "required": ["dbConnId", "sql"]
            }),
        },
        ToolDef {
            name: "list_db_tables".into(),
            description: "列出指定 MySQL 连接当前数据库的所有表名。".into(),
            parameters: json!({
                "type": "object",
                "properties": { "dbConnId": { "type": "string" } },
                "required": ["dbConnId"]
            }),
        },
        ToolDef {
            name: "describe_table".into(),
            description: "返回指定表的列结构（字段名、类型、是否可空、键、默认值、注释）。\
table 可用 `database.table` 限定名（推荐，尤其当连接未指定默认库时），或仅 `table`（取当前默认库）。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "dbConnId": { "type": "string" },
                    "table": {
                        "type": "string",
                        "description": "表名，可用 `database.table` 限定（如 apidoc.api_keys）或仅 table"
                    }
                },
                "required": ["dbConnId", "table"]
            }),
        },
    ]
}

/// RDP 桌面上下文工具集（有活动内嵌 RDP 会话且激活模型为多模态时启用）。
///
/// - `desktop_screenshot`：截取当前 RDP 桌面整屏画面（PNG），作为**图片**回传给
///   模型（多模态视觉）。坐标与截图共用同一像素坐标系。
/// - `desktop_click`：在截图坐标系的 (x, y) 像素位置点击（左/右/中键，可双击）。
/// - `desktop_type`：把文本输入到远端当前焦点处（Unicode 通道，不支持组合键）。
/// - `desktop_key`：按下一个键（DOM `KeyboardEvent.code`），可带修饰键组合。
///
/// 与 exec_ssh 等后端工具不同：这些工具的执行体在**前端**（IronRDP WASM 会话），
/// 后端只负责 emit `ai:tool_call` 并等待前端通过 `ai_desktop_tool_respond` 回传
/// 结果（见 [`crate::commands::ai::run_agent_loop`]），因此 `execute_tool` 不会
/// 分派到它们。
pub fn desktop_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "desktop_screenshot".into(),
            description: "截取当前活动 RDP 远程桌面的整屏画面（PNG 图片，会直接展示给你）。\
坐标与截图使用同一像素坐标系。执行任何点击/输入前都应先截图了解当前界面，\
操作完成后再截图确认结果。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolDef {
            name: "desktop_click".into(),
            description: "在远端桌面截图的像素坐标系中，点击 (x, y) 位置。\
button 可选 left（默认）/right/middle；double=true 时双击。\
点击前先 desktop_screenshot 确认坐标，点击后用 desktop_screenshot 确认效果。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "x": {
                        "type": "integer",
                        "description": "截图坐标系中的横坐标（像素，从 0 开始）"
                    },
                    "y": {
                        "type": "integer",
                        "description": "截图坐标系中的纵坐标（像素，从 0 开始）"
                    },
                    "button": {
                        "type": "string",
                        "enum": ["left", "right", "middle"],
                        "description": "鼠标按键，默认 left"
                    },
                    "double": {
                        "type": "boolean",
                        "description": "是否双击，默认 false"
                    }
                },
                "required": ["x", "y"]
            }),
        },
        ToolDef {
            name: "desktop_type".into(),
            description: "把文本输入到远端桌面当前焦点位置（如输入框、编辑器、终端），\
通过 Unicode 输入通道发送，等价于用户在远端键入。不支持快捷键/组合键（如 Ctrl+C），\
组合键请用 desktop_key。输入前请确认焦点在目标输入框上。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "要输入的完整文本（单次调用上限 2000 字符）"
                    }
                },
                "required": ["text"]
            }),
        },
        ToolDef {
            name: "desktop_key".into(),
            description: "向远端桌面发送一次按键。code 使用 DOM KeyboardEvent.code 值\
（如 \"Enter\"、\"Escape\"、\"Tab\"、\"Backspace\"、\"F5\"、\"KeyA\"、\"ArrowDown\"）；\
modifiers 为可选修饰键 code 列表（如 [\"ControlLeft\",\"ShiftLeft\",\"AltLeft\"]），\
按下主键后按相反顺序释放。适合快捷键组合、回车确认、方向键等 desktop_type 覆盖不了的输入。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "主键的 KeyboardEvent.code（如 Enter / Escape / F5 / KeyA）"
                    },
                    "modifiers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "可选修饰键 code 列表，如 [\"ControlLeft\"]"
                    }
                },
                "required": ["code"]
            }),
        },
    ]
}


/// 本地文件读写工具集（设置页开启"本地文件读写"后才由编排层下发）。
///
/// 两个助手域（终端助手 / 数据库助手）共用同一组工具定义；执行时按请求所属的
/// domain（"ssh" / "db"）取该域在设置里配置的工作目录，路径参数一律视为
/// **相对工作目录的路径**（如 `data/users.csv`），绝对路径与 `..` 逃逸被拒绝。
///
/// - `read_file`：读取工作目录内文本文件（≤1 MiB，二进制拒绝）。
/// - `write_file`：写入文本到工作目录内文件（覆盖已有文件标记危险，需人工确认）。
/// - `list_files`：列出工作目录/子目录内容（帮助 AI 了解有哪些文件可用）。
pub fn file_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read_file".into(),
            description: "读取本地工作目录内的文本文件内容，返回原始文本。\
path 是相对工作目录的路径（如 data/users.csv），不允许绝对路径或 .. 逃逸。\
单文件上限 1MB；二进制文件会被拒绝。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "相对工作目录的文件路径，如 data/users.csv"
                    }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "write_file".into(),
            description: "把文本内容写入本地工作目录内的文件。path 是相对工作目录的路径，\
不允许绝对路径或 .. 逃逸。父目录需已存在（不会自动创建多级目录）。\
若目标文件已存在会被覆盖（用户会收到危险确认）。适合导出数据、保存脚本等。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "相对工作目录的文件路径，如 out/users.csv"
                    },
                    "content": {
                        "type": "string",
                        "description": "要写入的完整文本内容"
                    }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDef {
            name: "list_files".into(),
            description: "列出工作目录（或相对其的子目录）内的条目：文件名、类型（文件/目录）、\
大小。path 省略时列工作目录根。用于了解有哪些文件可用、确认输出文件是否已存在。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "相对工作目录的目录路径，省略或空串表示工作目录本身"
                    }
                },
                "required": []
            }),
        },
    ]
}

/// 任务清单工具（借鉴 deepseek-harness `tool-todo`）。
///
/// `todo_write(todos: [{content, status}])`：模型**整表替换**当前任务清单，
/// 无部分更新/单条编辑——每次调用都携带完整清单，前端按事件流 last-write-wins
/// 展示。status 取值 `pending`（待办）/ `in_progress`（进行中）/ `completed`（已完成）。
///
/// 该工具是纯记账（无副作用、不需要确认），agent 模式下**始终下发**，不受
/// 活动上下文裁剪影响。清单状态不落后端（前端按会话持有），执行器只负责
/// 校验与 emit `ai:todo` 事件。
pub fn todo_tool() -> ToolDef {
    ToolDef {
        name: "todo_write".into(),
        description: "维护当前任务的任务清单（**整表替换**：每次调用都必须携带当前完整的\
任务清单，而不是只写变化的部分）。适用于多步骤任务：任务开始前列出所有步骤，\
每完成/进行到一步就调用一次更新对应项状态。\
status 取值：pending（待办）、in_progress（进行中）、completed（已完成）。\
任务全部完成或放弃时调用一次，把清单改为空数组 []。"
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "当前完整任务清单（全部步骤）",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "任务步骤描述（非空、不重复）"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "该步骤当前状态"
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        }),
    }
}

/// 技能加载工具（借鉴 deepseek-harness `tool-skill`）。
///
/// agent 模式下系统提示词只注入**技能目录摘要**（标题 + 内容开头），模型需要
/// 完整技能内容时调用本工具按**标题精确匹配**加载。是只读操作，自动放行。
/// 仅当该助手域存在已启用的技能时由编排层下发。
pub fn skill_tool() -> ToolDef {
    ToolDef {
        name: "load_skill".into(),
        description: "加载一条已启用技能的完整内容。name 必须与系统提示词「可用技能」\
目录中的标题**完全一致**。仅当你需要该技能的完整步骤/命令细节时才调用；\
目录摘要已足够理解任务时不必加载。加载后按技能内容执行，不要重复加载同一技能。"
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "技能标题（与目录中的标题完全一致）"
                }
            },
            "required": ["name"]
        }),
    }
}

/// 返回全部工具定义（SSH + SQL）。保留用于测试/兼容；运行时按上下文裁剪请用
/// [`tools_for_context`]。
pub fn all_tools() -> Vec<ToolDef> {
    let mut v = ssh_tools();
    v.extend(sql_tools());
    v
}

/// 向用户提问工具（借鉴 deepseek-harness `tool-ask-user`）。
///
/// 模型在信息不足（端口/目录/方案取舍/需要用户确认）时调用，由前端渲染为
/// 问题表单（选项 + 自由输入），用户的回答作为 tool 结果回填上下文——避免
/// 模型瞎猜或空转。执行体在**前端**（类似桌面工具）：编排层 emit
/// `ai:tool_call` 后等待 `ai_ask_user_respond` 回执。agent 模式无条件下发。
pub fn ask_user_tool() -> ToolDef {
    ToolDef {
        name: "ask_user_question".into(),
        description: "向用户提出一个或多个问题，获取继续执行所需的信息（端口、\
路径、目标主机、方案取舍、确认等）。适用于信息不足或需要用户决策时：\
不要猜测关键信息，直接提问。每个问题需提供稳定的 id、问题文本，可附选项\
（label + 可选说明；推荐项放第一位并在 label 末尾加 (Recommended)）与多选\
开关。一次最多提问 4 个问题。答案会在你的下一轮上下文中以 \
{\"answers\":[{\"id\",\"selected\",\"custom\"}]} 形式返回。"
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "要问的问题列表（1-4 个）",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "稳定且唯一的问题 id（回传答案时原样带回）"
                            },
                            "question": {
                                "type": "string",
                                "description": "问题文本（简明、具体）"
                            },
                            "header": {
                                "type": "string",
                                "description": "可选短标题（≤12 字符）"
                            },
                            "options": {
                                "type": "array",
                                "description": "可选项（缺省为自由输入）",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string", "description": "选项文本" },
                                        "description": { "type": "string", "description": "选项说明（可选）" }
                                    },
                                    "required": ["label"]
                                }
                            },
                            "multi_select": {
                                "type": "boolean",
                                "description": "是否允许多选（缺省单选）"
                            }
                        },
                        "required": ["id", "question"]
                    }
                }
            },
            "required": ["questions"]
        }),
    }
}

/// 校验 ask_user_question 参数（纯函数，便于单元测试）。
///
/// 规则：questions 必须是 1-4 个问题；每题 id / question 非空；options 每项
/// label 非空。返回 Err(可读错误信息)。
pub fn validate_ask_user_questions(args: &Value) -> Result<Vec<AskUserQuestion>, String> {
    let Some(list) = args.get("questions").and_then(Value::as_array) else {
        return Err("ask_user_question 缺少 questions 参数（必须是数组）".into());
    };
    if list.is_empty() {
        return Err("questions 不能为空".into());
    }
    if list.len() > 4 {
        return Err("一次最多提问 4 个问题".into());
    }
    let mut questions: Vec<AskUserQuestion> = Vec::with_capacity(list.len());
    let mut seen: HashSet<String> = HashSet::new();
    for (i, q) in list.iter().enumerate() {
        let id = match q.get("id").and_then(Value::as_str) {
            Some(s) => s.trim().to_string(),
            None => return Err(format!("第 {} 个问题缺少 id", i + 1)),
        };
        if id.is_empty() {
            return Err(format!("第 {} 个问题的 id 为空", i + 1));
        }
        if !seen.insert(id.clone()) {
            return Err(format!("问题 id 重复：{id}"));
        }
        let question = match q.get("question").and_then(Value::as_str) {
            Some(s) => s.trim().to_string(),
            None => return Err(format!("第 {} 个问题缺少 question 文本", i + 1)),
        };
        if question.is_empty() {
            return Err(format!("第 {} 个问题的 question 为空", i + 1));
        }
        let header = q
            .get("header")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let multi_select = q.get("multi_select").and_then(Value::as_bool).unwrap_or(false);
        let mut options: Vec<AskUserOption> = Vec::new();
        if let Some(opts) = q.get("options").and_then(Value::as_array) {
            // 前端表单以 label 作为勾选 key，重复 label 会互相覆盖 → 必须唯一。
            let mut opt_seen: HashSet<String> = HashSet::new();
            for (j, o) in opts.iter().enumerate() {
                let label = match o.get("label").and_then(Value::as_str) {
                    Some(s) => s.trim().to_string(),
                    None => return Err(format!("第 {} 个问题第 {} 个选项缺少 label", i + 1, j + 1)),
                };
                if label.is_empty() {
                    return Err(format!("第 {} 个问题第 {} 个选项的 label 为空", i + 1, j + 1));
                }
                if !opt_seen.insert(label.clone()) {
                    return Err(format!("第 {} 个问题的选项 label 重复：{label}", i + 1));
                }
                options.push(AskUserOption {
                    label,
                    description: o
                        .get("description")
                        .and_then(Value::as_str)
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                });
            }
        }
        questions.push(AskUserQuestion {
            id,
            question,
            header,
            options,
            multi_select,
        });
    }
    Ok(questions)
}

/// 把用户回答格式化为回填给模型的 tool 结果文本（dsh 规范：
/// `{"answers":[{id, selected, custom}]}`，compact JSON 单行）。
pub fn format_ask_user_answers(answers: &[AskUserAnswer]) -> String {
    serde_json::json!({ "answers": answers }).to_string()
}

/// 按当前活动上下文裁剪工具集（块A 核心逻辑）。
///
/// - 提供活动终端 → 启用 SSH 工具；
/// - 提供活动 MySQL 连接 → 启用 SQL 工具；
/// - 提供活动内嵌 RDP 会话 → 启用桌面工具（调用方还需保证激活模型为多模态，
///   否则模型看不懂截图——见 [`crate::commands::ai::ai_chat`] 的门控）；
/// - 皆无 → 返回空（agent 模式下模型只能纯文本对话，前端系统提示会告知
///   "未检测到可用上下文"）。
///
/// `active_terminal_id` / `active_db_conn_id` / `active_desktop_id` 只要**非空字符串**
/// 即视为有该上下文（具体值是否有效由执行期自然校验——SSH/SQL 工具找不到对应实例
/// 会返回错误；桌面工具由前端执行，找不到 RDP 控制句柄同样回填错误）。
pub fn tools_for_context(
    active_terminal_id: Option<&str>,
    active_db_conn_id: Option<&str>,
    active_desktop_id: Option<&str>,
) -> Vec<ToolDef> {
    let mut tools: Vec<ToolDef> = Vec::new();
    if active_terminal_id.map(|s| !s.is_empty()).unwrap_or(false) {
        tools.extend(ssh_tools());
    }
    if active_db_conn_id.map(|s| !s.is_empty()).unwrap_or(false) {
        tools.extend(sql_tools());
    }
    if active_desktop_id.map(|s| !s.is_empty()).unwrap_or(false) {
        tools.extend(desktop_tools());
    }
    tools
}

/// 判断一个工具是否为桌面工具（RDP 控制，执行体在前端）。
///
/// 编排层据此把执行路径从「后端 `execute_tool`」切换到「emit + 等待前端回执」。
pub fn is_desktop_tool(name: &str) -> bool {
    matches!(
        name,
        "desktop_screenshot" | "desktop_click" | "desktop_type" | "desktop_key"
    )
}

/// 从一组工具定义中提取名称集合，用于执行期校验模型是否幻觉调用了未 advertised 的工具。
pub fn allowed_tool_names(tools: &[ToolDef]) -> HashSet<String> {
    tools.iter().map(|t| t.name.clone()).collect()
}

// ===========================================================================
// 工具执行器
// ===========================================================================

/// 工具执行器入口。按 `call.name` 分派到具体实现。
///
/// `allowed` 是本轮 agent loop 实际下发给模型的工具名集合（上下文裁剪后）。
/// 若 `call.name` 不在其中，说明模型幻觉调用了未 advertised 的工具，直接拒绝——
/// 防止"只给了 SSH 工具，模型却调 exec_sql"这类越权。
///
/// `file_domain` 是请求所属助手域（"ssh"/"db"/""），文件工具据此取对应工作目录，
/// `load_skill` 据此过滤技能所属域；非文件工具忽略该参数。
///
/// `request_id` 是当前请求 id：`todo_write` 的 `ai:todo` 事件据此路由到前端会话。
///
/// 任何执行错误都被吞掉并返回 `ToolResult { ok: false, output: <错误信息> }`，
/// 由调用方把错误回填给模型，让模型据此重试或解释给用户。
pub async fn execute_tool(
    app: &AppHandle,
    state: &AppState,
    call: &ToolCall,
    allowed: &HashSet<String>,
    visualization: bool,
    file_domain: &str,
    request_id: &str,
) -> ToolResult {
    if !allowed.contains(&call.name) {
        return ToolResult::err(format!(
            "工具 `{}` 在当前上下文不可用（未提供活动终端或数据库连接）",
            call.name
        ));
    }
    match call.name.as_str() {
        "exec_ssh" => exec_ssh(state, &call.arguments, visualization).await,
        "terminal_snapshot" => terminal_snapshot(state, &call.arguments),
        "exec_sql" => exec_sql(app, state, &call.arguments, visualization).await,
        "list_db_tables" => list_db_tables(state, &call.arguments).await,
        "describe_table" => describe_table(state, &call.arguments).await,
        "read_file" => read_file(state, &call.arguments, file_domain),
        "write_file" => write_file(state, &call.arguments, file_domain),
        "list_files" => list_files(state, &call.arguments, file_domain),
        // todo_write：纯记账工具，校验后 emit ai:todo 事件（前端按事件流展示清单）。
        "todo_write" => execute_todo_write(app, request_id, &call.arguments),
        // load_skill：从设置读取技能全文（只读，无副作用）。
        "load_skill" => execute_load_skill(state, &call.arguments, file_domain),
        // 桌面工具的执行体在**前端**（IronRDP WASM 会话）：编排层不应把它们
        // 送到这里，若到达说明路由有误，给出明确错误而不是静默失败。
        name if is_desktop_tool(name) => ToolResult::err(format!(
            "工具 `{name}` 应由前端 RDP 会话执行，后端无法直接执行（执行路由异常）"
        )),
        other => ToolResult::err(format!("未知工具: {other}")),
    }
}

/// exec_ssh：在指定 SSH 会话对应服务器上执行命令。
///
/// **两种执行模式**：
/// - **终端可视化**（`visualization = true`）：把命令写入用户活动终端的 PTY
///   （`SshSession::write`），命令和输出实时显示在用户的 xterm 里。返回"已写入终端"
///   确认；模型若需读取结果可继续调 `terminal_snapshot`。这是最贴近"AI 在终端里操作"
///   的体验。
/// - **独立连接**（`visualization = false`，默认）：不复用终端会话已有的 `Handle`
///   （russh 0.45 的 `Handle` 未实现 `Clone`），而是基于该会话的 `session_config_id`
///   新建一条独立 SSH 连接，用 `channel.exec` 执行命令，读完输出后断开。输出干净
///   （无 PTY 转义污染）、隔离性好、规避所有权问题，但用户在终端里看不到。
async fn exec_ssh(state: &AppState, args: &Value, visualization: bool) -> ToolResult {
    // 1. 解析参数。
    let (session_id, command) = match (
        args.get("sessionId").and_then(Value::as_str),
        args.get("command").and_then(Value::as_str),
    ) {
        (Some(s), Some(c)) => (s.to_string(), c.to_string()),
        _ => return ToolResult::err("exec_ssh 缺少 sessionId 或 command 参数"),
    };

    if command.trim().is_empty() {
        return ToolResult::err("command 不能为空");
    }

    log::info!(
        "[agent] exec_ssh 开始：会话 {session_id}，模式 {}，命令：{command}",
        if visualization {
            "可视化(写PTY)"
        } else {
            "独立连接"
        }
    );

    // 1.5 终端可视化模式：把命令写入用户活动终端的 PTY（命令实时显示在 xterm），
    //     并通过"哨兵 echo"检测命令执行完成，截取执行期间的新增输出返回给 AI。
    //     这样终端可视化（看到 AI 敲命令）与 AI 拿到真实结果两者兼得。
    if visualization {
        return exec_ssh_visual(state, &session_id, &command).await;
    }

    // 2. 从 terminals 取 session_config_id。
    let session_config_id = {
        let terminals = state.terminals.lock();
        terminals
            .get(&session_id)
            .map(|s| s.session_config_id().to_string())
    };
    let session_config_id = match session_config_id {
        Some(id) => id,
        None => {
            return ToolResult::err(format!("找不到终端会话 {session_id}"));
        }
    };

    // 3. 加载会话配置 + 解析凭据（同步操作；多次获取短生命 DB 连接）。
    let setup: AppResult<(
        crate::storage::sessions_repo::Session,
        crate::ssh::session::ResolvedCredential,
    )> = (|| {
        let session_config = {
            let conn = state.conn()?;
            crate::storage::sessions_repo::get_session(&conn, &session_config_id)?
                .ok_or_else(|| AppError::NotFound(format!("会话配置 {session_config_id} 不存在")))?
        };
        let vault = {
            let guard = state.vault_read()?;
            guard
                .as_ref()
                .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
                .clone()
        };
        let conn = state.conn()?;
        let resolved = crate::ssh::session::resolve_credential(&session_config, &vault, &conn)?;
        Ok((session_config, resolved))
    })();
    let (session_config, resolved) = match setup {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("解析 SSH 凭据失败: {e}")),
    };

    // 4. 新建连接 + exec（整体 30s 超时）。
    let state_clone = state.clone();
    let run = async {
        // 连接（这里复用 SshSession::open 用的 connect_direct）。
        let handle = crate::ssh::client::connect_direct(
            &session_config.host,
            session_config.port,
            &session_config.username,
            &session_config.id,
            resolved.auth_method,
            state_clone,
        )
        .await?;

        // 打开 session channel。
        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|e| AppError::Ssh(format!("打开 channel 失败: {e}")))?;

        // 请求 exec（want_reply=false）。
        channel
            .exec(false, command.as_str())
            .await
            .map_err(|e| AppError::Ssh(format!("exec 失败: {e}")))?;

        // 循环 channel.wait() 收集 Data / ExtendedData。
        let mut raw: Vec<u8> = Vec::new();
        let mut exit_code: Option<u32> = None;
        use russh::ChannelMsg;
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { ref data }) => {
                    raw.extend_from_slice(data.as_ref());
                }
                Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                    raw.extend_from_slice(data.as_ref());
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exit_code = Some(exit_status);
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            }
            // 软截断：超出上限就停。
            if raw.len() >= EXEC_OUTPUT_CAP {
                raw.truncate(EXEC_OUTPUT_CAP);
                break;
            }
        }

        // 断开（忽略错误）。
        let _ = handle
            .disconnect(russh::Disconnect::ByApplication, "bye", "en")
            .await;

        Ok::<_, AppError>((raw, exit_code))
    };

    match timeout(EXEC_TIMEOUT, run).await {
        Ok(Ok((raw, exit_code))) => {
            let text = strip_ansi(&String::from_utf8_lossy(&raw));
            let truncated = if text.len() > EXEC_OUTPUT_CAP {
                let mut s: String = text.chars().take(EXEC_OUTPUT_CAP).collect();
                s.push_str("\n... [输出已截断]");
                s
            } else {
                text
            };
            let code_suffix = match exit_code {
                Some(0) | None => String::new(),
                Some(c) => format!("\n[exit: {c}]"),
            };
            ToolResult::ok(format!("{truncated}{code_suffix}"))
        }
        Ok(Err(e)) => ToolResult::err(format!("exec_ssh 失败: {e}")),
        Err(_) => ToolResult::err("exec_ssh 执行超时（30s）"),
    }
}

/// 可视化模式执行 SSH 命令：写入活动终端 PTY + 哨兵检测完成 + 按偏移取新增输出。
///
/// 流程：
/// 1. 生成唯一哨兵标记，把命令包装为 `<cmd>; echo <SENTINEL>` 写入 PTY
///    （终端里用户能看到 AI 实际敲的命令和输出）；写入前在同一把锁内记录
///    累计输出字节数作为基准。
/// 2. 轮询终端输出环形缓冲（**全量**，非限长窗口），直到命令执行完毕：
///    - 哨兵出现 ≥2 次（命令回显行 + echo 实际输出行）即完成——常规 shell；
///    - 仅出现 1 次（无回显 shell / 长命令折行拆断回显行内的哨兵 / 回显行
///      已滚出环形缓冲）时，以"缓冲停止增长"作为完成信号；
///    - 最长等待 30 秒，超时返回已收集输出 + 引导。
/// 3. 用 [`crate::state::TerminalSession::snapshot_after`] 取"基准之后"的输出
///    窗口（只含本次命令的新增输出，不受缓冲中历史内容与折行影响），清理
///    命令回显与哨兵行，去 ANSI 后按 16 KiB 上限截断（超出附截断提示）。
///
/// 注意：**不能**以哨兵首次出现作为完成信号——命令回显行（PTY 回显
/// `cmd; echo SENTINEL`）在写入瞬间就会出现，此时命令可能仍在执行；
/// 若在此时返回，AI 拿到的会是空/部分输出（"终端有输出但 AI 分析不到"
/// 的主要根因）。
async fn exec_ssh_visual(state: &AppState, session_id: &str, command: &str) -> ToolResult {
    // 占用该终端：与 MCP 的 exec_ssh_terminal 共用同一把忙锁（忙时立即返回
    // 引导、不排队等待）。没有这把锁时，两个并发 AI 会话在同一终端上执行会
    // 互相污染：A 写入的哨兵/回显混进 B 的 snapshot_after 窗口，A 的输出
    // 可能回填给 B，命令输出张冠李戴。
    //
    // 占用经 Drop 守卫释放：AI 请求被"终止"（ai_stop → abort）时本函数的
    // future 在 await 点被整体丢弃，函数末尾的释放代码不会执行——守卫的
    // Drop 保证 abort/panic 路径也释放，否则该终端永久"正被占用"。
    let Some(_busy_guard) = state.try_lock_terminal(session_id) else {
        return ToolResult::err(
            "该终端正被占用（同请求的命令按顺序执行中，或另一 AI/MCP 会话正在\
             该终端执行），请等待当前命令完成后重试",
        );
    };
    exec_ssh_visual_unlocked(state, session_id, command).await
    // _busy_guard 在此 drop（正常返回 / 错误 / 超时 / abort / panic 均释放）。
}

/// [`exec_ssh_visual`] 的执行主体（调用方已持有该终端的 busy 占用）。
async fn exec_ssh_visual_unlocked(state: &AppState, session_id: &str, command: &str) -> ToolResult {
    use rand::Rng;

    // 生成唯一哨兵（避免与正常输出撞车）。哨兵只用于完成检测；内容提取
    // 走"写入点偏移"窗口，不依赖哨兵行在快照中的位置。
    let nonce: u64 = rand::thread_rng().gen();
    let sentinel = format!("__XTERM_DONE_{nonce:x}__");

    // 构造实际执行的命令：原命令 + 哨兵 echo。
    // 用 `;` 连接（无论原命令成功与否哨兵都会输出），保证能检测到完成。
    // 注意：原命令末尾的换行已去掉。
    let cmd = command.trim_end_matches(['\n', '\r']);
    let wrapped = format!("{cmd}; echo {sentinel}\n");

    // 写入 PTY 前记录累计字节基准（同一把锁内先记基准再写，保证命令回显
    // 与输出都落在基准之后的窗口里）。
    let base = {
        let terminals = state.terminals.lock();
        match terminals.get(session_id) {
            Some(ssh) => {
                let base = ssh.total_output_bytes();
                if let Err(e) = ssh.write(wrapped.into_bytes()) {
                    return ToolResult::err(format!("写入终端失败: {e}"));
                }
                base
            }
            None => return ToolResult::err(format!("终端会话 {session_id} 不存在")),
        }
    };

    // 轮询等待命令执行完毕（最长 30 秒）。
    // 完成信号：
    // - 哨兵出现 ≥2 次：命令回显行与 echo 输出行都已出现，命令已结束；
    // - 哨兵只出现 1 次且**不在回显行**（无回显 shell、长命令折行拆断回显
    //   行内的哨兵、或回显行已滚出环形缓冲），且输出停止增长一段时间：
    //   echo 输出行即唯一哨兵，输出停止即视为结束；
    // - 其它情况继续等待。哨兵在回显行里的特征：PTY 回显的是包装后的
    //   `cmd; echo SENTINEL`，整行包含 `echo <SENTINEL>`；真正的 echo 输出行
    //   则只是哨兵本身，二者由此区分。
    let echo_marker = format!("echo {sentinel}");
    const POLL_INTERVAL: Duration = Duration::from_millis(200);
    /// 哨兵出现且输出停止后，视为执行完成的静默宽限时间。
    const SILENCE_GRACE: Duration = Duration::from_secs(1);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    #[allow(unused_assignments)]
    let mut snapshot = String::new();
    let mut last_total = 0usize;
    let mut silence_since = std::time::Instant::now();
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let (snap, total) = {
            let terminals = state.terminals.lock();
            match terminals.get(session_id) {
                Some(ssh) => (ssh.full_snapshot(), ssh.total_output_bytes()),
                None => return ToolResult::err("终端会话已断开"),
            }
        };
        snapshot = snap;

        let occurrences = snapshot.matches(&sentinel).count();
        // 常规 shell：回显行 + echo 输出行都出现 → 命令已结束。
        if occurrences >= 2 {
            break;
        }
        // 无回显 shell / 回显行已滚出缓冲：哨兵只出现一次（echo 输出行），
        // 且输出停止增长一段时间 → 命令已结束。
        if occurrences == 1
            && !sentinel_in_echo_line(&snapshot, &sentinel, &echo_marker)
            && total == last_total
            && silence_since.elapsed() >= SILENCE_GRACE
        {
            break;
        }
        // 输出仍在增长 → 重置静默计时。
        if total != last_total {
            silence_since = std::time::Instant::now();
        }
        last_total = total;

        if std::time::Instant::now() >= deadline {
            // 超时：返回基准之后已收集的输出（可能命令还在跑或卡住等输入）。
            let (window, overflow) = {
                let terminals = state.terminals.lock();
                match terminals.get(session_id) {
                    Some(ssh) => ssh.snapshot_after(base),
                    None => return ToolResult::err("终端会话已断开"),
                }
            };
            let cleaned = strip_ansi(&window);
            let mut result = format!(
                "命令已写入终端执行，但 180 秒内未检测到执行完成（命令可能仍在运行、\
                 等待输入，或终端当前不在 shell 提示符）。**不要盲目重发同一命令**\
                 （命令可能仍在终端里跑，重发会重复执行）。\n目前捕获到的输出：\n{}\n\
                 （如需终端当前完整输出，可调用 terminal_snapshot）",
                truncate_output(&cleaned, MAX_EXEC_OUTPUT_BYTES)
            );
            if overflow {
                result.push_str(&output_overflow_note());
            }
            return ToolResult::ok(result);
        }
    }

    // 取"写入点之后"的输出窗口，清掉命令回显与哨兵行，去 ANSI 后截断（附提示）。
    let (window, overflow) = {
        let terminals = state.terminals.lock();
        match terminals.get(session_id) {
            Some(ssh) => ssh.snapshot_after(base),
            None => return ToolResult::err("终端会话已断开"),
        }
    };
    let cleaned = clean_window(&strip_ansi(&window), cmd, &sentinel);
    let mut result = truncate_output(&cleaned, MAX_EXEC_OUTPUT_BYTES);
    if overflow {
        result.push_str(&output_overflow_note());
    }
    ToolResult::ok(result)
}

/// 判断哨兵在快照中的出现位置是否位于"命令回显行"。
///
/// 命令写入 PTY 的瞬间，shell 会把包装后的 `cmd; echo SENTINEL` 回显成一行
/// 输出（该行含 `echo <SENTINEL>` 标记）；而 echo 的实际输出行（哨兵本身）
/// 只在命令执行完毕后出现。据此区分"命令还在跑"与"命令已结束"，
/// 避免把回显行误当作完成信号。`mcp::exec` 的终端绑定执行复用此函数。
pub fn sentinel_in_echo_line(snapshot: &str, sentinel: &str, echo_marker: &str) -> bool {
    let Some(pos) = snapshot.find(sentinel) else {
        return false;
    };
    let line_start = snapshot[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = snapshot[pos..]
        .find('\n')
        .map(|i| pos + i)
        .unwrap_or(snapshot.len());
    snapshot[line_start..line_end].contains(echo_marker)
}

/// 清理命令执行窗口：去掉末尾哨兵行（含其后的提示符）与开头的命令回显。
///
/// [`crate::state::TerminalSession::snapshot_after`] 返回的窗口从"命令写入点"
/// 开始，结构为：
/// `[命令回显(可能折行)] [命令输出] [哨兵输出行] [下一个提示符]`
///
/// - 在最后一个含完整哨兵的位置截断：去掉哨兵行与提示符；若输出无尾随换行
///   （printf/echo -n/进度条），哨兵与输出粘在同一行，截到哨兵之前可保住
///   粘在行内的输出。
/// - 开头回显按"写入原文（跳过折行产生的 CR/LF）"做字符级前缀匹配，匹配
///   成功则整块移除；不匹配（无回显 shell、heredoc 等特殊回显）则保留原样。
pub fn clean_window(window: &str, cmd: &str, sentinel: &str) -> String {
    // 1. 在最后一个完整哨兵处截断（哨兵之后只有提示符，一并去掉）。
    let mut text = match window.rfind(sentinel) {
        Some(pos) => window[..pos].to_string(),
        None => window.to_string(),
    };
    // 2. 回显剥离：把写入原文（cmd; echo SENTINEL）与窗口开头逐字符比对，
    //    折行产生的 CR/LF 跳过；整条比对完即回显块结束，剩余部分为命令输出。
    let expect: Vec<char> = format!("{cmd}; echo {sentinel}").chars().collect();
    let chars: Vec<char> = text.chars().collect();
    let mut idx = 0;
    let mut i = 0;
    while i < chars.len() && idx < expect.len() {
        // 期望串里的换行按"回显里的真实换行"处理，直接跨过（多行命令）。
        if expect[idx] == '\r' || expect[idx] == '\n' {
            idx += 1;
            continue;
        }
        match chars[i] {
            // 窗口里的 CR/LF 可能是折行产生，也可能是回显里的真实换行，跳过。
            '\r' | '\n' => i += 1,
            c if c == expect[idx] => {
                idx += 1;
                i += 1;
            }
            // 回显不匹配（无回显 shell / 特殊回显）：放弃剥离，返回去尾后的窗口。
            _ => return text,
        }
    }
    if idx == expect.len() {
        // 回显块末尾即写入命令结尾的换行，一并跳过。
        while i < chars.len() && (chars[i] == '\r' || chars[i] == '\n') {
            i += 1;
        }
        text = chars[i..].iter().collect();
    }
    text
}

/// 输出超过环形缓冲容量时的提示（附给模型，避免把残缺窗口当完整结果）。
pub fn output_overflow_note() -> String {
    format!(
        "\n[提示：本次输出超过终端环形缓冲容量（{} 字节），窗口开头部分已丢失；\
         如需完整内容可拆分命令或调用 terminal_snapshot]",
        crate::ssh::session::OUTPUT_BUFFER_CAP
    )
}

/// 截断输出到指定字节数，超出则保留开头，并附**面向模型**的截断提示。
///
/// 提示含原始字节数，并引导模型用 `terminal_snapshot` 获取更完整内容——
/// 否则模型会把截断后的输出误当作命令的完整结果。
fn truncate_output(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let cut = s
        .char_indices()
        .take_while(|(i, _)| *i <= max_bytes)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(max_bytes);
    format!(
        "{}…\n[输出已截断：共 {} 字节，仅保留开头部分；如需更完整内容可调用 terminal_snapshot]",
        &s[..cut],
        s.len()
    )
}

/// terminal_snapshot：取指定终端最近输出。
fn terminal_snapshot(state: &AppState, args: &Value) -> ToolResult {
    let session_id = match args.get("sessionId").and_then(Value::as_str) {
        Some(s) => s,
        None => return ToolResult::err("terminal_snapshot 缺少 sessionId"),
    };
    // maxBytes 上限放开到环形缓冲容量（默认 16 KiB），模型可按需申请更多。
    let max_bytes = args
        .get("maxBytes")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).min(SNAPSHOT_MAX_BYTES))
        .unwrap_or(SNAPSHOT_DEFAULT_BYTES);

    let terminals = state.terminals.lock();
    match terminals.get(session_id) {
        Some(s) => {
            let snap = s.snapshot(max_bytes);
            ToolResult::ok(snap)
        }
        None => ToolResult::err(format!("找不到终端会话 {session_id}")),
    }
}

/// exec_sql：在指定 MySQL 连接上执行 SQL。
///
/// 写操作（INSERT/UPDATE/DELETE/DDL）由调用方在确认阶段把关（前端弹二次确认）；
/// 此函数本身只在用户已批准后才会被调用，故直接执行。
///
/// `visualization` 为 true（SQL 终端可视化开启）时，执行后额外 emit
/// `ai:sql_result` 事件，携带结构化结果（columns/rows/affected/elapsed/error），
/// 前端 SQL 控制台据此把 SQL 与结果回显进输出流（命令行模式）。
async fn exec_sql(
    app: &AppHandle,
    state: &AppState,
    args: &Value,
    visualization: bool,
) -> ToolResult {
    let (conn_id, sql) = match (
        args.get("dbConnId").and_then(Value::as_str),
        args.get("sql").and_then(Value::as_str),
    ) {
        (Some(c), Some(s)) => (c.to_string(), s.to_string()),
        _ => return ToolResult::err("exec_sql 缺少 dbConnId 或 sql"),
    };
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(100);

    log::info!(
        "[agent] exec_sql 开始：连接 {conn_id}，可视化 {}，SQL：{sql}",
        if visualization { "是" } else { "否" }
    );

    // 取出 conn 句柄（Arc 克隆；并发下不会与用户操作互相 remove/insert 竞争）。
    let conn = {
        let map = state.mysql_conns.lock();
        match map.get(&conn_id) {
            Some(c) => c.clone(),
            None => {
                return ToolResult::err(format!("找不到 MySQL 连接 {conn_id}"));
            }
        }
    };

    // USE 语句拦截：与 db_exec_sql 一致——AI 生成的 `USE xxx` 不能直接发给
    // MySQL（prepared 协议不支持 USE，MySQL 1295），改为更新连接的 current_db。
    if let Some(use_db) = crate::database::mysql::parse_use_statement(&sql) {
        if let Some(db) = &use_db {
            if let Err(e) = crate::database::mysql::validate_database_identifier(db) {
                return ToolResult::err(e.to_string());
            }
        }
        conn.set_current_db(use_db.clone());
        return ToolResult::ok(match use_db {
            Some(db) => format!("已切换到数据库 `{db}`"),
            None => "USE 语句缺少库名".into(),
        });
    }

    let started = std::time::Instant::now();
    // 带当前库执行：AI 工具同样自动落在连接的当前库上（USE 由 execute 自动带上）。
    let cur_db = conn.current_db();
    let res = conn.execute(&sql, limit, cur_db.as_deref()).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    // SQL 终端可视化：把结构化结果回显给 SQL 控制台（命令行模式）。
    if visualization {
        let (columns, rows, affected, truncated, error) = match &res {
            Ok(qr) => (
                qr.columns.clone(),
                qr.rows.clone(),
                qr.affected,
                qr.truncated,
                None,
            ),
            Err(e) => (
                Vec::new(),
                Vec::new(),
                0u64,
                false,
                Some(format!("SQL 执行失败: {e}")),
            ),
        };
        crate::events::emit(
            app,
            crate::events::AI_SQL_RESULT,
            crate::events::AiSqlResultEvent {
                request_id: String::new(),
                sql: sql.clone(),
                columns,
                rows,
                affected,
                truncated,
                elapsed_ms,
                error,
            },
        );
    }

    match res {
        Ok(qr) => ToolResult::ok(format_query_result(&qr)),
        Err(e) => ToolResult::err(format!("SQL 执行失败: {e}")),
    }
}

/// list_db_tables：执行 `SHOW TABLES`，返回表名列表。
async fn list_db_tables(state: &AppState, args: &Value) -> ToolResult {
    let conn_id = match args.get("dbConnId").and_then(Value::as_str) {
        Some(c) => c.to_string(),
        None => return ToolResult::err("list_db_tables 缺少 dbConnId"),
    };
    let conn = {
        let map = state.mysql_conns.lock();
        match map.get(&conn_id) {
            Some(c) => c.clone(),
            None => return ToolResult::err(format!("找不到 MySQL 连接 {conn_id}")),
        }
    };
    // 带当前库执行（SHOW TABLES 即当前库的表）。
    let cur_db = conn.current_db();
    let res = conn.execute("SHOW TABLES", 10_000, cur_db.as_deref()).await;

    match res {
        Ok(qr) => {
            let tables: Vec<String> = qr.rows.into_iter().filter_map(|mut r| r.pop()).collect();
            ToolResult::ok(format!("共 {} 张表：\n{}", tables.len(), tables.join("\n")))
        }
        Err(e) => ToolResult::err(format!("列出表失败: {e}")),
    }
}

/// describe_table：执行 `DESCRIBE <table>`，返回结构化文本。
async fn describe_table(state: &AppState, args: &Value) -> ToolResult {
    let (conn_id, table) = match (
        args.get("dbConnId").and_then(Value::as_str),
        args.get("table").and_then(Value::as_str),
    ) {
        (Some(c), Some(t)) => (c.to_string(), t.to_string()),
        _ => return ToolResult::err("describe_table 缺少 dbConnId 或 table"),
    };
    // 解析表标识符为安全的反引号限定名（支持 `table` 或 `db.table`）。
    let qualified = match crate::database::mysql::qualify_table_identifier(&table) {
        Ok(q) => q,
        Err(e) => return ToolResult::err(format!("{e}")),
    };
    let sql = format!("DESCRIBE {qualified}");

    let conn = {
        let map = state.mysql_conns.lock();
        match map.get(&conn_id) {
            Some(c) => c.clone(),
            None => return ToolResult::err(format!("找不到 MySQL 连接 {conn_id}")),
        }
    };
    // 未限定库名的 DESCRIBE（`DESCRIBE \`table\``）按连接当前库执行。
    let cur_db = conn.current_db();
    let res = conn.execute(&sql, 1000, cur_db.as_deref()).await;

    match res {
        Ok(qr) => ToolResult::ok(format_query_result(&qr)),
        Err(e) => ToolResult::err(format!("查看表结构失败: {e}")),
    }
}

// ===========================================================================
// 本地文件读写工具（工作目录沙箱）
// ===========================================================================

/// 取指定助手域的工作目录（设置页配置）。未配置返回 None。
fn workspace_dir_for(state: &AppState, domain: &str) -> Option<String> {
    let settings = crate::config::settings_load_inner(state).ok()?;
    settings.ai.file_access.workspace_dirs.get(domain).cloned()
}

/// 把"相对工作目录"的路径解析为沙箱内绝对路径。
///
/// 安全规则：
/// 1. 拒绝绝对路径与含 `..` 组分的路径；
/// 2. 工作目录 canonicalize（解析 symlink，统一实际大小写）；
/// 3. 目标已存在 → canonicalize 全路径后必须位于工作目录内（防 symlink 逃逸）；
/// 4. 目标不存在（写新文件）→ 父目录 canonicalize 校验，文件名直接拼接。
fn resolve_workspace_path(workspace: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err("path 必须是相对工作目录的路径，不允许绝对路径".into());
    }
    if rel_path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("path 不允许包含 `..`（不能逃逸工作目录）".into());
    }
    let ws = workspace
        .canonicalize()
        .map_err(|e| format!("工作目录不可访问: {e}"))?;
    let joined = ws.join(rel_path);
    // 目标已存在：canonicalize 后校验前缀（可解析 symlink，防逃逸）。
    if joined.exists() {
        let real = joined
            .canonicalize()
            .map_err(|e| format!("解析路径失败: {e}"))?;
        if !real.starts_with(&ws) {
            return Err("路径超出工作目录范围，已拒绝".into());
        }
        return Ok(real);
    }
    // 目标不存在（写新文件）：父目录必须存在且在工作目录内。
    let parent = joined.parent().unwrap_or(&ws);
    if !parent.exists() {
        return Err(format!("父目录不存在: {}", parent.display()));
    }
    let real_parent = parent
        .canonicalize()
        .map_err(|e| format!("解析父目录失败: {e}"))?;
    if !real_parent.starts_with(&ws) {
        return Err("路径超出工作目录范围，已拒绝".into());
    }
    let name = joined
        .file_name()
        .ok_or_else(|| "无效的文件名".to_string())?;
    Ok(real_parent.join(name))
}

/// read_file：读取工作目录内文本文件（≤1 MiB）。
fn read_file(state: &AppState, args: &Value, domain: &str) -> ToolResult {
    let rel = match args.get("path").and_then(Value::as_str) {
        Some(p) => p,
        None => return ToolResult::err("read_file 缺少 path 参数"),
    };
    let ws = match workspace_dir_for(state, domain) {
        Some(w) => w,
        None => {
            return ToolResult::err(
                "当前助手未配置工作目录：请在设置页开启「本地文件读写」并选择工作目录",
            );
        }
    };
    let path = match resolve_workspace_path(Path::new(&ws), rel) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(e),
    };
    if !path.is_file() {
        return ToolResult::err(format!("{} 不是文件", path.display()));
    }
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return ToolResult::err(format!("读取文件信息失败: {e}")),
    };
    if meta.len() > MAX_FILE_READ_BYTES as u64 {
        return ToolResult::err(format!(
            "文件过大（{} 字节，上限 {} 字节）。请先手动拆分/截取后再让 AI 读取",
            meta.len(),
            MAX_FILE_READ_BYTES
        ));
    }
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => return ToolResult::err(format!("读取文件失败: {e}")),
    };
    // 二进制检测：含 NUL 字节即视为二进制，拒绝（防止把乱码/图片内容塞给模型）。
    if data.contains(&0) {
        return ToolResult::err("二进制文件不支持读取，仅支持文本文件");
    }
    let text = String::from_utf8_lossy(&data);
    ToolResult::ok(format!(
        "文件 {}（{} 字节）内容：\n{}",
        path.display(),
        data.len(),
        text
    ))
}

/// write_file：把文本写入工作目录内文件（覆盖已有文件在上层被标记危险）。
fn write_file(state: &AppState, args: &Value, domain: &str) -> ToolResult {
    let (rel, content) = match (
        args.get("path").and_then(Value::as_str),
        args.get("content").and_then(Value::as_str),
    ) {
        (Some(p), Some(c)) => (p, c),
        _ => return ToolResult::err("write_file 缺少 path 或 content 参数"),
    };
    let ws = match workspace_dir_for(state, domain) {
        Some(w) => w,
        None => {
            return ToolResult::err(
                "当前助手未配置工作目录：请在设置页开启「本地文件读写」并选择工作目录",
            );
        }
    };
    if content.len() > MAX_FILE_WRITE_BYTES {
        return ToolResult::err(format!(
            "内容过大（{} 字节，上限 {} 字节）",
            content.len(),
            MAX_FILE_WRITE_BYTES
        ));
    }
    let path = match resolve_workspace_path(Path::new(&ws), rel) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(e),
    };
    match std::fs::write(&path, content) {
        Ok(()) => ToolResult::ok(format!(
            "已写入 {}（{} 字节）",
            path.display(),
            content.len()
        )),
        Err(e) => ToolResult::err(format!("写入失败: {e}")),
    }
}

/// list_files：列出工作目录（或相对其的子目录）内的条目。
fn list_files(state: &AppState, args: &Value, domain: &str) -> ToolResult {
    let rel = args.get("path").and_then(Value::as_str).unwrap_or("");
    let ws = match workspace_dir_for(state, domain) {
        Some(w) => w,
        None => {
            return ToolResult::err(
                "当前助手未配置工作目录：请在设置页开启「本地文件读写」并选择工作目录",
            );
        }
    };
    let dir = match resolve_workspace_path(Path::new(&ws), rel) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(e),
    };
    if !dir.is_dir() {
        return ToolResult::err(format!("{} 不是目录", dir.display()));
    }
    let rd = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) => return ToolResult::err(format!("列出目录失败: {e}")),
    };
    let mut lines: Vec<String> = Vec::new();
    let mut count = 0usize;
    for entry in rd {
        if count >= MAX_LIST_ENTRIES {
            lines.push(format!("…（条目过多，仅显示前 {MAX_LIST_ENTRIES} 项）"));
            break;
        }
        let Ok(e) = entry else { continue };
        count += 1;
        let name = e.file_name().to_string_lossy().to_string();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            lines.push(format!("[目录] {name}/"));
        } else {
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            lines.push(format!("{name}（{} 字节）", size));
        }
    }
    if lines.is_empty() {
        lines.push("（空目录）".into());
    }
    ToolResult::ok(format!("目录 {}：\n{}", dir.display(), lines.join("\n")))
}

// ===========================================================================
// 任务清单工具（todo_write）
// ===========================================================================

/// 任务状态合法取值（与工具 JSON Schema 的 enum 一致）。
const TODO_STATUSES: [&str; 3] = ["pending", "in_progress", "completed"];

/// todo_write 执行器：校验参数 → emit `ai:todo` 事件（整表）→ 返回 counts 摘要。
///
/// 清单状态本身不落后端（前端按会话持有、按事件流 last-write-wins 更新），
/// 因此本函数无持久副作用；校验失败返回错误，由编排层回填给模型。
fn execute_todo_write(app: &AppHandle, request_id: &str, args: &Value) -> ToolResult {
    let items = match validate_todo_list(args) {
        Ok(items) => items,
        Err(e) => return ToolResult::err(e),
    };
    // 校验通过才 emit（失败的事件不携带，前端清单保持不变）。
    crate::events::emit(
        app,
        crate::events::AI_TODO,
        crate::events::AiTodoEvent {
            request_id: request_id.to_string(),
            todos: items.clone(),
        },
    );
    let counts = |s: &str| items.iter().filter(|t| t.status == s).count();
    let n = items.len();
    let mut text = format!(
        "任务清单已更新（共 {n} 项：待办 {} · 进行中 {} · 已完成 {}）",
        counts("pending"),
        counts("in_progress"),
        counts("completed")
    );
    // 完成自动收敛（借鉴 dsh goal 的 concludeTurn 思想）：清单全部完成（或清空）
    // 时附加收尾提醒，避免模型在任务已完成后继续空转调用工具浪费轮次/token。
    // 两种终态文案区分：全部完成 vs 清空清单（清空也可能是"放弃任务"）。
    let all_done = !items.is_empty() && counts("completed") == n;
    if all_done {
        text.push_str(
            "\n（所有任务已完成。如已无其他工作，请直接给出最终总结并结束，不要再调用工具。）",
        );
    } else if n == 0 {
        text.push_str(
            "\n（任务清单已清空。如已无其他工作，请直接给出最终总结并结束，不要再调用工具。）",
        );
    }
    ToolResult::ok(text)
}

/// 校验 todo_write 参数并归一化为事件项列表（纯函数，便于单元测试）。
///
/// 规则：todos 必须是数组（≤50 项）；每项 content 非空且不重复；status 必须是
/// pending / in_progress / completed 之一。返回 Err(可读错误信息)。
fn validate_todo_list(args: &Value) -> Result<Vec<crate::events::AiTodoItem>, String> {
    let Some(todos) = args.get("todos").and_then(Value::as_array) else {
        return Err("todo_write 缺少 todos 参数（必须是数组）".into());
    };
    if todos.len() > 50 {
        return Err("任务清单过长（上限 50 项）".into());
    }
    let mut items: Vec<crate::events::AiTodoItem> = Vec::with_capacity(todos.len());
    let mut seen: HashSet<String> = HashSet::new();
    for (i, t) in todos.iter().enumerate() {
        let content = match t.get("content").and_then(Value::as_str) {
            Some(c) => c.trim(),
            None => return Err(format!("第 {} 项缺少 content（必须是非空字符串）", i + 1)),
        };
        if content.is_empty() {
            return Err(format!("第 {} 项的 content 为空", i + 1));
        }
        if !seen.insert(content.to_string()) {
            return Err(format!("任务内容重复：{content}"));
        }
        let status = match t.get("status").and_then(Value::as_str) {
            Some(s) if TODO_STATUSES.contains(&s) => s.to_string(),
            Some(s) => {
                return Err(format!(
                    "第 {} 项的 status `{s}` 非法（可选：pending / in_progress / completed）",
                    i + 1
                ));
            }
            None => return Err(format!("第 {} 项缺少 status", i + 1)),
        };
        items.push(crate::events::AiTodoItem {
            content: content.to_string(),
            status,
        });
    }
    Ok(items)
}

// ===========================================================================
// 技能加载工具（load_skill）
// ===========================================================================

/// load_skill 执行器：从设置按「标题精确匹配」加载技能全文。
///
/// 技能与前端 settings.json 同源（`ai.skills`，前端保存、后端读取），按请求
/// 所属 domain + enabled 过滤。只读操作，无副作用。
fn execute_load_skill(state: &AppState, args: &Value, domain: &str) -> ToolResult {
    let name = match args.get("name").and_then(Value::as_str) {
        Some(n) => n.trim(),
        None => return ToolResult::err("load_skill 缺少 name 参数"),
    };
    if name.is_empty() {
        return ToolResult::err("load_skill 的 name 不能为空");
    }
    let settings = match crate::config::settings_load_inner(state) {
        Ok(s) => s,
        Err(e) => return ToolResult::err(format!("读取设置失败: {e}")),
    };
    // 取该域启用的技能；未启用视为不可用（与前端目录注入的过滤规则一致）。
    let mut catalog: Vec<crate::config::SkillConfig> = settings
        .ai
        .skills
        .into_iter()
        .filter(|s| s.enabled && s.domain == domain)
        .collect();
    // 标题精确匹配（trim 后比对；找不到返回引导，告诉模型有哪些可用技能）。
    let skill = catalog
        .iter()
        .position(|s| s.title.trim() == name)
        .map(|idx| catalog.remove(idx))
        .unwrap_or_else(|| {
            // 未命中：把可用标题列表附在错误里，帮模型纠正名称。
            let titles: Vec<String> = catalog.into_iter().map(|s| s.title).collect();
            if titles.is_empty() {
                return crate::config::SkillConfig {
                    id: String::new(),
                    title: name.to_string(),
                    content: String::new(),
                    domain: domain.to_string(),
                    enabled: false,
                };
            }
            // 哨兵：content 置空 + 错误由下方处理。
            crate::config::SkillConfig {
                id: String::new(),
                title: format!("__not_found__:{}", titles.join(" / ")),
                content: String::new(),
                domain: domain.to_string(),
                enabled: false,
            }
        });
    if skill.title.starts_with("__not_found__") {
        let titles = skill.title.trim_start_matches("__not_found__:");
        return ToolResult::err(format!(
            "没有找到标题为「{name}」的技能。当前可用的技能：{titles}"
        ));
    }
    // 防御性截断（技能内容正常 ≤500 字，上限 4 KiB 防手动编辑超长）。
    let content = if skill.content.len() > MAX_SKILL_BYTES {
        let cut = skill
            .content
            .char_indices()
            .take_while(|(i, _)| *i <= MAX_SKILL_BYTES)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(MAX_SKILL_BYTES);
        format!("{}…\n[技能内容过长，已截断]", &skill.content[..cut])
    } else {
        skill.content
    };
    ToolResult::ok(format!(
        "技能「{}」完整内容如下：\n{}",
        skill.title, content
    ))
}

// ===========================================================================
// 安全护栏
// ===========================================================================

/// 危险命令正则集合（exec_ssh 用）。
///
/// 命中任一即视为危险操作；前端会红色高亮 + 二次确认。
/// 覆盖：rm -rf /、mkfs、dd 写块设备、shutdown/reboot、fork bomb、
/// chmod -R 777 /、`sed -i`（静默改写文件）等。
static DANGEROUS_CMD_REGEXES: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"(?i)rm\s+-[a-z]*r[a-z]*f[a-z]*\s+/(?:\s|$|\*)",
        r"(?i)rm\s+-[a-z]*f[a-z]*r[a-z]*\s+/(?:\s|$|\*)",
        r"(?i)\brm\s+-rf\s+/\*",
        r"(?i)\bmkfs\b",
        r"(?i)\bdd\s+if=.*of=/dev/(?:sd|nvme|hd|vd|xvd)",
        r"(?i)\b(shutdown|reboot|halt|poweroff)\b",
        r"(?i)\binit\s+[06]\b",
        r":\(\)\s*\{", // fork bomb :(){:|:&};:
        r"(?i)>\s*/dev/(?:sd|nvme|hd|vd|xvd)",
        r"(?i)chmod\s+-R\s+777\s+/(?:\s|$)",
        // sed -i 静默改写文件（-i、-ri、-i.bak 等组合 flag 均命中）。
        // 默认白名单已不含 sed，此正则兜底拦截用户自行加入白名单的场景。
        r"(?i)\bsed\s+-[a-z.]*i[a-z.]*(?:\s|$)",
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap_or_else(|e| panic!("无效正则 {p}: {e}")))
    .collect()
});

/// DELETE 无 WHERE 子句（非锚定：主语句可能是 CTE 之后的 DELETE，见
/// [`sql_first_keyword`]；同时覆盖 `DELETE t1 FROM t2` 别名形式）。
static DELETE_NO_WHERE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bDELETE\s+(?:[A-Za-z_][A-Za-z0-9_.]*\s+)?FROM\s+\S+\s*(?:;|$)")
        .expect("无效正则")
});

/// 判断一个工具调用是否危险。
///
/// - exec_ssh：command 命中危险命令模式（rm -rf /、mkfs、dd 写块设备、
///   shutdown/reboot、fork bomb、chmod -R 777 /、`sed -i` 等）。
/// - exec_sql：有效关键字为 DROP/TRUNCATE，或 DELETE 无 WHERE 子句
///   （含 `WITH ... DELETE` 形式，见 [`sql_first_keyword`]）。
/// - write_file：目标文件已存在（覆盖已有数据）→ 危险。
/// - 其它工具默认安全。
///
/// `workspace` 为文件工具所属助手的工作目录（`write_file` 判断覆盖用）；
/// 非文件工具传 None 即可。
pub fn is_dangerous(name: &str, arguments: &Value, workspace: Option<&Path>) -> bool {
    match name {
        "exec_ssh" => arguments
            .get("command")
            .and_then(Value::as_str)
            .map(|cmd| DANGEROUS_CMD_REGEXES.iter().any(|re| re.is_match(cmd)))
            .unwrap_or(false),
        "exec_sql" => arguments
            .get("sql")
            .and_then(Value::as_str)
            .map(|sql| {
                let kw = sql_first_keyword(sql);
                kw == "DROP"
                    || kw == "TRUNCATE"
                    || (kw == "DELETE" && DELETE_NO_WHERE_RE.is_match(sql))
            })
            .unwrap_or(false),
        "write_file" => {
            // 覆盖已有文件：解析出沙箱路径后检查存在性（解析失败视为不危险，
            // 执行阶段会返回错误，不需要提前标记）。
            match (arguments.get("path").and_then(Value::as_str), workspace) {
                (Some(p), Some(ws)) => resolve_workspace_path(ws, p)
                    .map(|p| p.exists())
                    .unwrap_or(false),
                _ => false,
            }
        }
        _ => false,
    }
}

/// 剥掉 SQL 里的注释（`--` / `#` 行注释、`/* */` 块注释），替换为空格。
///
/// 防止 `-- 注释\nDELETE FROM t` 这类语句的第一个 token 是注释、绕过关键字判定。
fn strip_sql_comments(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    let n = bytes.len();
    while i < n {
        if i + 1 < n && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            // -- 行注释：到行尾。
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(' ');
        } else if bytes[i] == b'#' {
            // # 行注释：到行尾。
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(' ');
        } else if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // /* */ 块注释。
            i += 2;
            while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            out.push(' ');
        } else {
            let ch = sql[i..].chars().next().unwrap_or_default();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// 取一条 SQL 语句的"有效"关键字（trim、不区分大小写）。多语句时只看第一条。
///
/// - 先剥离前导/行内注释；
/// - `WITH` 引导的 CTE 语句返回其后的主语句关键字（如
///   `WITH cte AS (SELECT 1) DELETE FROM t` → `DELETE`），防止 CTE 携带写语句
///   绕过只读/危险判定（MySQL 8 支持 `WITH ... DELETE/UPDATE/INSERT` 单语句）；
/// - 其余返回首个关键字；空语句或无法判定返回空串（调用方保守拒绝）。
fn sql_first_keyword(sql: &str) -> String {
    let clean = strip_sql_comments(sql);
    let first_stmt = clean.split(';').next().unwrap_or("").trim();
    let tokens: Vec<&str> = first_stmt.split_whitespace().collect();
    let Some(first) = tokens.first() else {
        return String::new();
    };
    let first_up = first.to_ascii_uppercase();
    if first_up != "WITH" {
        return first_up;
    }

    // WITH 引导：跟踪括号深度，取 CTE 定义之后、括号外的第一个语句关键字。
    // 例：`WITH cte AS (SELECT 1) DELETE FROM t` → 括号外第一个关键字是 DELETE。
    let mut depth: i32 = 0;
    for tok in tokens.iter().skip(1) {
        depth += tok.matches('(').count() as i32 - tok.matches(')').count() as i32;
        if depth != 0 {
            continue;
        }
        let up = tok.to_ascii_uppercase();
        if matches!(
            up.as_str(),
            "SELECT"
                | "INSERT"
                | "UPDATE"
                | "DELETE"
                | "MERGE"
                | "REPLACE"
                | "DROP"
                | "TRUNCATE"
                | "CREATE"
                | "ALTER"
                | "CALL"
        ) {
            return up;
        }
    }
    // 未找到主语句关键字（残缺语句）→ 返回 WITH 本身；WITH 不在任何放行集合内，
    // 调用方会按"不允许"处理，属保守拒绝。
    "WITH".to_string()
}

/// 根据 `sql_mode` 判断一条 SQL 是否被允许执行。
///
/// - `readonly`：只允许 `SELECT` / `SHOW` / `EXPLAIN` / `DESCRIBE` / `DESC`。
///   注意 `WITH` 不在集合内——[`sql_first_keyword`] 会把 CTE 语句解析成其主语句
///   关键字（`WITH cte AS (...) SELECT ...` → SELECT，仍放行；而
///   `WITH cte AS (...) DELETE ...` → DELETE，被拦截）。
/// - `restricted`：上述 + 允许 `INSERT` / `UPDATE` / `DELETE` / `MERGE`（DDL 仍禁止）。
/// - `full`：允许一切（但 `is_dangerous` 仍生效，由上层确认）。
///
/// 未知关键字在非 `full` 模式下一律视为不允许（保守策略）。
pub fn sql_allowed_by_mode(sql: &str, mode: &str) -> bool {
    let kw = sql_first_keyword(sql);
    if kw.is_empty() {
        return false;
    }
    match mode {
        "full" => true,
        "restricted" => matches!(
            kw.as_str(),
            "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "INSERT" | "UPDATE" | "DELETE" | "MERGE"
        ),
        // 默认（含 "readonly" 及任何未知值）按只读处理。
        _ => matches!(kw.as_str(), "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC"),
    }
}

/// 判断一条 SQL 是否为只读查询（用于"白名单运行"模式下自动放行判定）。
///
/// 只读 = SELECT / SHOW / EXPLAIN / DESCRIBE / DESC（CTE 语句由
/// [`sql_first_keyword`] 解析成主语句关键字后再判定）。与
/// [`sql_allowed_by_mode`] 的 readonly 集合一致。
pub fn is_readonly_sql(sql: &str) -> bool {
    let kw = sql_first_keyword(sql);
    matches!(kw.as_str(), "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC")
}

/// Shell 元字符正则：命中任一即视为"复合命令"，**不**算白名单内。
///
/// 严格模式禁用：命令分隔符（;、&&、||、|）、命令替换（$()、反引号）、
/// 重定向（>、<、>>）、后台（&）、子 shell（括号）。这样保证白名单内命令是
/// "单一 argv 形式"，无法通过 `cat x; rm -rf /` 这类拼接绕过。
///
/// **`\n`/`\r` 必须拒绝**：换行是 shell 命令分隔符。`"ls\nrm -rf /"` 若只按空白
/// 分词会被拆成 `["ls", "rm", "-rf", "/"]`，前 3 个 token 恰好命中白名单项 `ls`，
/// 导致第二条命令在 whitelist 模式下免确认执行（提示注入可触发）。
static COMMAND_METACHAR_RE: Lazy<Regex> = Lazy::new(|| {
    // 任一元字符出现即匹配。`\x00` 是 NUL（regex crate 不支持 `\0` 写法，
    // 会 panic）；命令字符串不应含 NUL，出现即视为恶意输入拒绝。
    Regex::new(r"[;&|<>`\n\r\x00]|\$\(|&&|\|\||>>").expect("无效元字符正则")
});

/// 判断一条 exec_ssh 命令是否落在白名单内（用于前端绿色卡片 + 默认放行 UX）。
///
/// **严格匹配规则**：
/// 1. 命令含任何 shell 元字符（`;` `&` `|` `>` `<` 反引号 `$(` 等）→ 直接返回 `false`
///    （防 `ls;rm -rf /` 绕过；这类命令必须走人工确认）。
/// 2. 否则取命令的前缀 token 序列（最多前 3 个 token，覆盖 `systemctl status nginx`、
///    `docker ps -a` 这类）逐一与白名单做**前缀匹配**：命令的 trim 后前缀以白名单项开头
///    （大小写不敏感，以空格对齐 token 边界）即视为命中。
///
/// 注意：本函数只决定"是否显示为白名单内（免确认 UX）"，**不**改变执行闭环——
/// 执行仍需用户点确认按钮（见 [`crate::commands::ai::run_agent_loop`]）。
pub fn is_whitelisted(command: &str, whitelist: &[String]) -> bool {
    let cmd = command.trim();
    if cmd.is_empty() {
        return false;
    }
    // 1. 含元字符 → 不算白名单内。
    if COMMAND_METACHAR_RE.is_match(cmd) {
        return false;
    }
    // 2. 取前缀 token（最多 3 个），组合成候选前缀集合逐级匹配。
    //    例：cmd = "systemctl status nginx" → 候选 ["systemctl", "systemctl status", "systemctl status nginx"]
    let lower = cmd.to_ascii_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    let mut prefix = String::new();
    for (i, tok) in tokens.iter().enumerate().take(3) {
        if i > 0 {
            prefix.push(' ');
        }
        prefix.push_str(tok);
        // 精确匹配或白名单项等于当前前缀。
        if whitelist.iter().any(|w| {
            let w = w.trim().to_ascii_lowercase();
            !w.is_empty() && w == prefix
        }) {
            return true;
        }
    }
    // 3. 也支持白名单项是命令的前缀（如白名单 "sys" 命中 "systemctl"）——但为安全起见
    //    要求白名单项至少 2 个字符且后接空格/结尾。
    for w in whitelist {
        let w = w.trim().to_ascii_lowercase();
        if w.len() < 2 {
            continue;
        }
        if lower == w || lower.starts_with(&format!("{w} ")) {
            return true;
        }
    }
    false
}

/// 校验 exec_ssh 命令是否被白名单允许（执行端用，返回错误信息供回填模型）。
///
/// 与 [`is_whitelisted`] 的区别：本函数返回 `Result`，仅用于"是否允许"的判定，
/// 由调用方决定如何反馈。当前 exec_ssh 执行端**不**用白名单拦截（保留人工确认闭环），
/// 此函数保留供未来"白名单内自动执行"模式使用。
#[allow(dead_code)]
pub fn check_command_whitelist(command: &str, whitelist: &[String]) -> Result<(), String> {
    if is_whitelisted(command, whitelist) {
        Ok(())
    } else {
        Err(format!("命令 `{command}` 不在白名单中，需用户人工确认"))
    }
}

// ===========================================================================
// 人类可读描述
// ===========================================================================

/// 生成工具调用的人类可读简述，用于前端确认弹窗。
pub fn describe_call(name: &str, arguments: &Value) -> String {
    match name {
        "exec_ssh" => {
            let cmd = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("执行命令: {cmd}")
        }
        "terminal_snapshot" => {
            let sid = arguments
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("?");
            format!("读取终端 {sid} 最近输出")
        }
        "exec_sql" => {
            let sql = arguments.get("sql").and_then(Value::as_str).unwrap_or("");
            let preview: String = sql.chars().take(60).collect();
            if sql.chars().count() > 60 {
                format!("执行 SQL: {preview}...")
            } else {
                format!("执行 SQL: {preview}")
            }
        }
        "list_db_tables" => "列出数据库表".into(),
        "describe_table" => {
            let t = arguments
                .get("table")
                .and_then(Value::as_str)
                .unwrap_or("?");
            format!("查看表 {t} 结构")
        }
        "read_file" => {
            let p = arguments.get("path").and_then(Value::as_str).unwrap_or("?");
            format!("读取文件: {p}")
        }
        "write_file" => {
            let p = arguments.get("path").and_then(Value::as_str).unwrap_or("?");
            format!("写入文件: {p}")
        }
        "list_files" => {
            let p = arguments
                .get("path")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("工作目录");
            format!("列出目录: {p}")
        }
        "todo_write" => {
            let n = arguments
                .get("todos")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            format!("更新任务清单（{n} 项）")
        }
        "load_skill" => {
            let name = arguments.get("name").and_then(Value::as_str).unwrap_or("?");
            format!("加载技能: {name}")
        }
        "ask_user_question" => {
            let n = arguments
                .get("questions")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            format!("向用户提问（{n} 个问题）")
        }
        "desktop_screenshot" => "截取 RDP 桌面画面".into(),
        "desktop_click" => {
            let x = arguments.get("x").and_then(Value::as_i64).unwrap_or(-1);
            let y = arguments.get("y").and_then(Value::as_i64).unwrap_or(-1);
            let button = arguments
                .get("button")
                .and_then(Value::as_str)
                .unwrap_or("left");
            let double = arguments.get("double").and_then(Value::as_bool).unwrap_or(false);
            format!(
                "点击桌面 ({x}, {y}){}{}",
                if button != "left" {
                    format!("，{button} 键")
                } else {
                    String::new()
                },
                if double { "（双击）" } else { "" }
            )
        }
        "desktop_type" => {
            let text = arguments.get("text").and_then(Value::as_str).unwrap_or("");
            let preview: String = text.chars().take(40).collect();
            if text.chars().count() > 40 {
                format!("输入文本: {preview}...")
            } else {
                format!("输入文本: {preview}")
            }
        }
        "desktop_key" => {
            let code = arguments.get("code").and_then(Value::as_str).unwrap_or("?");
            let mods: Vec<&str> = arguments
                .get("modifiers")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if mods.is_empty() {
                format!("按键: {code}")
            } else {
                format!("按键: {} + {}", mods.join("+"), code)
            }
        }
        other => format!("执行工具: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 白名单元字符正则必须能正常编译（回归：曾用 `\0` 写法导致 regex crate
    /// 拒绝编译，首次使用 `Lazy` 初始化时直接 panic）。访问 `COMMAND_METACHAR_RE`
    /// 即触发编译，编译失败会让本测试（及所有依赖它的用例）直接报错。
    #[test]
    fn metachar_regex_compiles() {
        assert!(COMMAND_METACHAR_RE.is_match("ls; rm -rf /"));
    }

    /// 各类 shell 分隔符/重定向/命令替换必须被判定为"非白名单"（防拼接绕过）。
    #[test]
    fn whitelist_rejects_metachars() {
        let wl: Vec<String> = vec!["ls".into(), "cat".into()];
        // 注释里记录的注入场景：换行拆分后 token 恰好命中白名单项。
        assert!(!is_whitelisted("ls\nrm -rf /", &wl));
        assert!(!is_whitelisted("ls; rm -rf /", &wl));
        assert!(!is_whitelisted("cat a.txt | rm -rf /", &wl));
        assert!(!is_whitelisted("ls && whoami", &wl));
        assert!(!is_whitelisted("ls $(whoami)", &wl));
        assert!(!is_whitelisted("echo `whoami`", &wl));
        assert!(!is_whitelisted("ls > out", &wl));
        assert!(!is_whitelisted("cat x >> log", &wl));
        // NUL 字符（`\x00`）：命令字符串不应含 NUL，出现即拒绝。
        assert!(!is_whitelisted("ls\x00rm -rf /", &wl));
        assert!(!is_whitelisted("cat x &", &wl));
    }

    /// 纯白名单命令（无元字符、前缀命中）应放行；大小写不敏感。
    #[test]
    fn whitelist_accepts_simple_commands() {
        let wl: Vec<String> = vec!["systemctl".into(), "docker ps".into()];
        assert!(is_whitelisted("systemctl status nginx", &wl));
        assert!(is_whitelisted("SYSTEMCTL status", &wl));
        assert!(is_whitelisted("docker ps -a", &wl));
        // 未命中白名单的普通命令：无元字符但不在白名单 → false。
        assert!(!is_whitelisted("reboot now", &wl));
        assert!(!is_whitelisted("", &wl));
        // 白名单前缀必须 ≥2 字符且以空格/结尾对齐，单字符 "s" 不允许。
        let wl_short: Vec<String> = vec!["s".into()];
        assert!(!is_whitelisted("systemctl status", &wl_short));
        // 白名单项是命令前缀时需 token 边界：白名单 "sys" 不命中 "systemctl"。
        let wl_prefix: Vec<String> = vec!["sys".into()];
        assert!(is_whitelisted("sys", &wl_prefix));
        assert!(!is_whitelisted("systemctl status", &wl_prefix));
    }

    /// `sed -i` 静默改写文件，必须被 is_dangerous 标记（前缀白名单无法区分
    /// 只读 sed 与改文件的 sed -i，由危险正则兜底拦截）。
    #[test]
    fn dangerous_detects_sed_inplace() {
        let arg = |cmd: &str| serde_json::json!({ "command": cmd });
        assert!(is_dangerous("exec_ssh", &arg("sed -i 's/a/b/' /etc/hosts"), None));
        assert!(is_dangerous("exec_ssh", &arg("sed -ri 's/a/b/g' app.conf"), None));
        assert!(is_dangerous("exec_ssh", &arg("sed -i.bak 's/a/b/' notes.txt"), None));
        // 只读 sed（无 -i）不危险。
        assert!(!is_dangerous("exec_ssh", &arg("sed -n '1,5p' log.txt"), None));
        assert!(!is_dangerous("exec_ssh", &arg("sed 's/a/b/' log.txt"), None));
    }

    /// 常规窗口：去掉命令回显、哨兵行与随后的提示符，只留命令输出。
    #[test]
    fn clean_window_normal() {
        let win = "ls; echo __DONE__\nfile1\nfile2\n__DONE__\nuser@host:~$ ";
        assert_eq!(clean_window(win, "ls", "__DONE__"), "file1\nfile2\n");
    }

    /// 输出末尾无换行（printf/echo -n）：哨兵与最后一行输出粘在同一行，
    /// 清理时截到哨兵之前，保住粘在行内的输出。
    #[test]
    fn clean_window_glued_sentinel() {
        let win = "printf abc; echo __DONE__\nabc__DONE__\nuser@host:~$ ";
        assert_eq!(clean_window(win, "printf abc", "__DONE__"), "abc");
    }

    /// 长命令折行把回显行里的哨兵拆成两行：快照里只有哨兵输出行一处完整
    /// 哨兵，清理应剥离折行回显、去掉哨兵行与提示符，只留输出（回归：
    /// 折行导致的历史输出污染 bug）。
    #[test]
    fn clean_window_wrapped_echo() {
        let sentinel = "__XTERM_DONE_eec0e70d3a3acc51__";
        let cmd = "free -h && echo '---'";
        let win = format!(
            "{cmd}; echo __XTERM_DONE_eec0e70d3a\n3acc51__\n              total        used\nMem:            15G\n{sentinel}\n[root@ai158 ~]# "
        );
        let out = clean_window(&win, cmd, sentinel);
        assert_eq!(out.trim(), "total        used\nMem:            15G");
    }

    /// 无回显 shell（回显不匹配）：放弃剥离回显，但仍去掉哨兵行与提示符。
    #[test]
    fn clean_window_no_echo_shell() {
        let win = "tail-of-output\n__DONE__\nuser@host:~$ ";
        assert_eq!(clean_window(win, "cat x", "__DONE__"), "tail-of-output\n");
        // 无回显 shell 且输出末尾无换行：输出与哨兵同处一行。
        let glued = "abc__DONE__\n";
        assert_eq!(clean_window(glued, "printf abc", "__DONE__"), "abc");
    }

    /// 回显行判定：含 `echo <SENTINEL>` 的行是命令回显（命令可能仍在执行），
    /// 纯哨兵行是 echo 的实际输出（命令已结束）。
    #[test]
    fn sentinel_echo_line_detection() {
        let sentinel = "__XTERM_DONE_abc__";
        let marker = format!("echo {sentinel}");
        // 命令回显行：行内含 `echo SENTINEL`。
        let snap = "$ df -h; echo __XTERM_DONE_abc__\n";
        assert!(sentinel_in_echo_line(snap, sentinel, &marker));
        // echo 输出行：哨兵独占一行，不判定为回显。
        let done = "Sizes\n__XTERM_DONE_abc__\nuser@host:~$ ";
        assert!(!sentinel_in_echo_line(done, sentinel, &marker));
        // 哨兵尚未出现。
        assert!(!sentinel_in_echo_line("no sentinel yet\n", sentinel, &marker));
    }

    /// 截断提示面向模型：包含原始字节数与 terminal_snapshot 引导。
    #[test]
    fn truncate_notice_guides_model() {
        let long = "x".repeat(100);
        let out = truncate_output(&long, 16);
        assert!(out.contains("已截断"));
        assert!(out.contains("100"));
        assert!(out.contains("terminal_snapshot"));
        // 未超限时原样返回。
        assert_eq!(truncate_output(&long, 200), long);
    }

    /// todo_write 合法清单：归一化、trim、按序保留。
    #[test]
    fn todo_accepts_valid_list() {
        let args = serde_json::json!({
            "todos": [
                { "content": "  检查磁盘占用  ", "status": "pending" },
                { "content": "清理日志", "status": "in_progress" },
                { "content": "重启服务", "status": "completed" }
            ]
        });
        let items = validate_todo_list(&args).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].content, "检查磁盘占用"); // trim
        assert_eq!(items[0].status, "pending");
        assert_eq!(items[1].status, "in_progress");
        assert_eq!(items[2].status, "completed");
        // 空清单合法（任务完成/放弃时清空）。
        assert!(validate_todo_list(&serde_json::json!({ "todos": [] })).unwrap().is_empty());
    }

    /// todo_write 非法清单：缺参数/空内容/重复/非法状态/超长 → 拒绝且带可读错误。
    #[test]
    fn todo_rejects_invalid_lists() {
        // 缺 todos 参数。
        assert!(validate_todo_list(&serde_json::json!({})).is_err());
        // content 为空。
        assert!(validate_todo_list(&serde_json::json!({ "todos": [{ "content": "  ", "status": "pending" }] })).is_err());
        // content 重复。
        assert!(validate_todo_list(&serde_json::json!({ "todos": [
            { "content": "a", "status": "pending" },
            { "content": "a", "status": "completed" }
        ] })).is_err());
        // 非法 status。
        assert!(validate_todo_list(&serde_json::json!({ "todos": [{ "content": "a", "status": "done" }] })).is_err());
        // 缺 status。
        assert!(validate_todo_list(&serde_json::json!({ "todos": [{ "content": "a" }] })).is_err());
        // 超过 50 项。
        let many = serde_json::json!({
            "todos": (0..51).map(|i| serde_json::json!({ "content": format!("任务{i}"), "status": "pending" })).collect::<Vec<_>>()
        });
        assert!(validate_todo_list(&many).is_err());
    }

    /// describe_call 对记账/加载工具给出可读描述。
    #[test]
    fn describe_new_tools() {
        let todo = serde_json::json!({ "todos": [{"content":"x","status":"pending"}] });
        assert_eq!(describe_call("todo_write", &todo), "更新任务清单（1 项）");
        let skill = serde_json::json!({ "name": "磁盘清理" });
        assert_eq!(describe_call("load_skill", &skill), "加载技能: 磁盘清理");
        // is_dangerous：记账/加载工具恒为安全。
        assert!(!is_dangerous("todo_write", &todo, None));
        assert!(!is_dangerous("load_skill", &skill, None));
    }

    /// ask_user_question 合法参数：归一化（trim、可选字段过滤）、多选标记保留。
    #[test]
    fn ask_user_accepts_valid() {
        let args = serde_json::json!({
            "questions": [{
                "id": "port",
                "question": "  目标端口？  ",
                "header": "部署配置",
                "options": [
                    { "label": " 80 (Recommended) ", "description": "HTTP" },
                    { "label": "443" }
                ],
                "multi_select": false
            }]
        });
        let qs = validate_ask_user_questions(&args).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].id, "port");
        assert_eq!(qs[0].question, "目标端口？"); // trim
        assert_eq!(qs[0].options.len(), 2);
        assert_eq!(qs[0].options[0].label, "80 (Recommended)");
        assert_eq!(qs[0].options[0].description.as_deref(), Some("HTTP"));
        assert!(!qs[0].multi_select);
    }

    /// ask_user_question 非法参数：缺 questions / 超 4 个 / id 重复 / 选项 label 重复/空 → 拒绝。
    #[test]
    fn ask_user_rejects_invalid() {
        // 缺 questions。
        assert!(validate_ask_user_questions(&serde_json::json!({})).is_err());
        // 空数组。
        assert!(validate_ask_user_questions(&serde_json::json!({ "questions": [] })).is_err());
        // 超过 4 个问题。
        let many = serde_json::json!({
            "questions": (0..5).map(|i| serde_json::json!({ "id": format!("q{i}"), "question": "?" })).collect::<Vec<_>>()
        });
        assert!(validate_ask_user_questions(&many).is_err());
        // id 重复。
        assert!(validate_ask_user_questions(&serde_json::json!({
            "questions": [
                { "id": "a", "question": "1" },
                { "id": "a", "question": "2" }
            ]
        })).is_err());
        // question 为空。
        assert!(validate_ask_user_questions(&serde_json::json!({
            "questions": [{ "id": "a", "question": "  " }]
        })).is_err());
        // 选项 label 重复（前端表单以 label 为勾选 key，重复会互相覆盖）。
        assert!(validate_ask_user_questions(&serde_json::json!({
            "questions": [{
                "id": "a",
                "question": "选哪个？",
                "options": [{ "label": "x" }, { "label": "x" }]
            }]
        })).is_err());
        // 选项 label 为空。
        assert!(validate_ask_user_questions(&serde_json::json!({
            "questions": [{ "id": "a", "question": "选哪个？", "options": [{ "label": "  " }] }]
        })).is_err());
    }

    /// 用户回答格式化为 dsh 规范 JSON（回填模型的 tool 结果）。
    #[test]
    fn ask_user_answers_roundtrip() {
        let answers = vec![
            crate::ai::tools::AskUserAnswer {
                id: "port".into(),
                selected: vec!["80".into()],
                custom: None,
            },
            crate::ai::tools::AskUserAnswer {
                id: "note".into(),
                selected: vec![],
                custom: Some("运维要求 https".into()),
            },
        ];
        let text = crate::ai::tools::format_ask_user_answers(&answers);
        // 可解析回 camelCase 字段（模型回填读的就是这个文本）。
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["answers"][0]["id"], "port");
        assert_eq!(parsed["answers"][0]["selected"][0], "80");
        assert_eq!(parsed["answers"][1]["custom"], "运维要求 https");
    }
}
