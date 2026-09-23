//! MCP 工具定义与元信息（tools/list 的 inputSchema + 校验/描述/白名单判定）。
//!
//! 从 server.rs 迁出的「纯函数」层：不持有连接与状态机，只按
//! `(kind, resource_mode, bound_source, bound_database, machines)` 生成工具
//! 定义，或按工具名给出只读性 / 白名单语义 / 日志与确认卡片文案。
//! 以后新增工具只需改本模块（定义 + 元信息）与 server.rs 的执行分派。
//!
//! 白名单语义（与 AI 助手的执行模式一致）：
//! - `exec_sql`：只读 SQL 自动放行；
//! - `exec_ssh` / `bastion_session_exec`：命中「设置 → AI」SSH 命令白名单放行；
//! - `bastion_create_session` / `bastion_close_session`：连接生命周期操作，
//!   不在目标主机执行命令，白名单模式直接放行。

use serde_json::{json, Value};

use crate::mcp::approval::McpKind;
use crate::mcp::exec::{self, MultiMachine};
use crate::state::AppState;

// ===========================================================================
// 名称 / 只读性 / 白名单
// ===========================================================================

/// 该 kind 的 MCP 实例是否支持指定工具。
///
/// SSH MCP 暴露 exec_ssh + 3 个文件工具（多机模式另含 list_machines；堡垒机
/// 模式另含 6 个 bastion_* 工具）；DB MCP 仅暴露 exec_sql。
pub(crate) fn kind_supports_tool(kind: McpKind, name: &str) -> bool {
    matches!(
        (kind, name),
        (McpKind::Ssh, "exec_ssh")
            | (McpKind::Ssh, "list_files")
            | (McpKind::Ssh, "upload_file")
            | (McpKind::Ssh, "download_file")
            | (McpKind::Ssh, "list_machines")
            | (McpKind::Ssh, "bastion_list_hosts")
            | (McpKind::Ssh, "bastion_create_session")
            | (McpKind::Ssh, "bastion_session_exec")
            | (McpKind::Ssh, "bastion_close_session")
            | (McpKind::Ssh, "bastion_list_sessions")
            | (McpKind::Ssh, "bastion_upload_file")
            | (McpKind::Db, "exec_sql")
            | (McpKind::File, "list_files")
            | (McpKind::File, "upload_file")
            | (McpKind::File, "download_file")
    )
}

/// 工具是否为只读（跳过人工确认）。`list_files` / `list_machines`（多机元数据）
/// 与 `bastion_list_hosts`（读资产菜单）/ `bastion_list_sessions`（会话元数据）只读。
pub(crate) fn is_readonly_tool(name: &str) -> bool {
    matches!(
        name,
        "list_files" | "list_machines" | "bastion_list_hosts" | "bastion_list_sessions"
    )
}

/// 白名单运行模式下，本次调用是否落在白名单内（可自动放行）。
///
/// - `exec_sql`：只读 SQL（SELECT / SHOW / EXPLAIN / DESCRIBE / DESC，
///   与 AI SQL 助手的"白名单运行"同一判定）；
/// - `exec_ssh` / `bastion_session_exec`：命令命中「设置 → AI」的 SSH 命令白名单
///   （与 AI SSH 助手共用同一份白名单，含元字符/换行防拼接绕过的严格匹配）；
/// - `bastion_create_session` / `bastion_close_session`：连接生命周期操作（不在
///   目标主机执行命令），白名单模式自动放行；
/// - 其它工具：一律不自动（走人工确认）。
pub(crate) fn is_whitelist_auto(state: &AppState, name: &str, arguments: &Value) -> bool {
    match name {
        "exec_sql" => arguments
            .get("sql")
            .and_then(Value::as_str)
            .map(crate::ai::tools::is_readonly_sql)
            .unwrap_or(false),
        "bastion_create_session" | "bastion_close_session" => true,
        "exec_ssh" | "bastion_session_exec" => {
            let cmd = match arguments.get("command").and_then(Value::as_str) {
                Some(c) => c,
                None => return false,
            };
            // 白名单复用 AI 助手的设置（含内存缓存；读取失败按无白名单处理）。
            let whitelist = crate::config::settings_load_inner(state)
                .map(|s| s.ai.ssh_agent.command_whitelist)
                .unwrap_or_default();
            crate::ai::tools::is_whitelisted(cmd, &whitelist)
        }
        _ => false,
    }
}

// ===========================================================================
// 日志详情与确认卡片文案
// ===========================================================================

/// 只读工具的日志详情（按工具名提取关键字段）。
pub(crate) fn readonly_detail(name: &str, arguments: &Value) -> String {
    match name {
        "list_files" => {
            let path = arguments.get("path").and_then(Value::as_str).unwrap_or("");
            format!("list_files: {}", path)
        }
        "bastion_list_hosts" => "读取堡垒机资产菜单".to_string(),
        "bastion_list_sessions" => "列出堡垒机会话".to_string(),
        _ => name.to_string(),
    }
}

/// 按工具名生成执行内容原文（用于日志，不含密码）。
pub(crate) fn exec_detail_for(name: &str, arguments: &Value) -> String {
    match name {
        "exec_ssh" => {
            let cmd = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("command: {}", cmd)
        }
        "exec_sql" => {
            let sql = arguments.get("sql").and_then(Value::as_str).unwrap_or("");
            format!("sql: {}", sql)
        }
        "list_files" => {
            let path = arguments.get("path").and_then(Value::as_str).unwrap_or("");
            format!("path: {}", path)
        }
        "upload_file" | "download_file" => {
            let local = arguments
                .get("localPath")
                .and_then(Value::as_str)
                .unwrap_or("");
            let remote = arguments
                .get("remotePath")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("{}: {} ⇄ {}", name, local, remote)
        }
        "bastion_create_session" => {
            let host = arguments.get("host").and_then(Value::as_str).unwrap_or("");
            format!("连接目标主机: {}", host)
        }
        "bastion_session_exec" => {
            let sid = arguments
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let cmd = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("session {} | command: {}", sid, cmd)
        }
        "bastion_close_session" => {
            let sid = arguments
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("关闭会话: {}", sid)
        }
        "bastion_upload_file" => {
            let sid = arguments
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let local = arguments
                .get("localPath")
                .and_then(Value::as_str)
                .unwrap_or("");
            let remote = arguments
                .get("remotePath")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("session {} | 上传: {} → {}", sid, local, remote)
        }
        other => other.to_string(),
    }
}

/// 生成 exec_ssh 的可读描述（用于确认弹窗）。`resource` 为绑定的 SSH 会话名。
fn describe_exec_ssh(arguments: &Value, resource: &str) -> String {
    let cmd = arguments
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("");
    format!("[SSH MCP · {}] 执行命令: {}", resource, cmd)
}

/// 生成 exec_sql 的可读描述（用于确认弹窗）。`resource` 为绑定的 DB profile 名。
fn describe_exec_sql(arguments: &Value, resource: &str) -> String {
    let sql = arguments.get("sql").and_then(Value::as_str).unwrap_or("");
    let preview: String = sql.chars().take(80).collect();
    format!("[DB MCP · {}] {}", resource, preview)
}

/// MCP 实例的中文标签（确认卡片/日志前缀用）。
fn mcp_label(kind: McpKind) -> &'static str {
    match kind {
        McpKind::Ssh => "SSH MCP",
        McpKind::Db => "DB MCP",
        McpKind::File => "File MCP",
    }
}

/// 按工具名生成确认弹窗的可读描述（前缀按 kind 取，File MCP 的文件工具
/// 不会误显示成 SSH MCP）。
pub(crate) fn describe_tool(kind: McpKind, name: &str, arguments: &Value, resource: &str) -> String {
    let label = mcp_label(kind);
    match name {
        "exec_ssh" => describe_exec_ssh(arguments, resource),
        "exec_sql" => describe_exec_sql(arguments, resource),
        "bastion_create_session" => {
            let host = arguments.get("host").and_then(Value::as_str).unwrap_or("");
            format!("[{} · 堡垒机 {}] 连接目标主机: {}", label, resource, host)
        }
        "bastion_session_exec" => {
            let sid = arguments
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let cmd = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!(
                "[{} · 堡垒机 {} · 会话 {}] 执行命令: {}",
                label,
                resource,
                sid.chars().take(8).collect::<String>(),
                cmd
            )
        }
        "bastion_close_session" => {
            let sid = arguments
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!(
                "[{} · 堡垒机 {}] 关闭会话: {}",
                label,
                resource,
                sid.chars().take(8).collect::<String>()
            )
        }
        "bastion_upload_file" => {
            let sid = arguments
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let local = arguments
                .get("localPath")
                .and_then(Value::as_str)
                .unwrap_or("");
            let remote = arguments
                .get("remotePath")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!(
                "[{} · 堡垒机 {} · 会话 {}] 上传文件: {} → {}",
                label,
                resource,
                sid.chars().take(8).collect::<String>(),
                local,
                remote
            )
        }
        "upload_file" => {
            let local = arguments
                .get("localPath")
                .and_then(Value::as_str)
                .unwrap_or("");
            let remote = arguments
                .get("remotePath")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("[{} · {}] 上传文件: {} → {}", label, resource, local, remote)
        }
        "download_file" => {
            let local = arguments
                .get("localPath")
                .and_then(Value::as_str)
                .unwrap_or("");
            let remote = arguments
                .get("remotePath")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("[{} · {}] 下载文件: {} → {}", label, resource, remote, local)
        }
        _ => format!("[{} · {}] {}", label, resource, name),
    }
}

/// 把机器清单格式化为给模型看的文本（list_machines 工具输出）。
///
/// 每台一行：`目标名 | user@host:port | 标签`。target 必须与目标名完全一致。
pub(crate) fn format_machines(machines: &[MultiMachine]) -> String {
    let mut out = format!(
        "当前授权机器共 {} 台（工具参数 target 必须与「目标名」完全一致）：\n",
        machines.len()
    );
    for m in machines {
        out.push_str(&format!(
            "- 目标名: {} | {}@{}:{}{}\n",
            m.display_name,
            m.username,
            m.host,
            m.port,
            m.tags
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .map(|t| format!(" | 标签: {}", t))
                .unwrap_or_default()
        ));
    }
    out
}

// ===========================================================================
// 工具定义（MCP inputSchema）
// ===========================================================================

/// 把 target 参数写入工具属性（multi 模式专用；target 为 None 时不动）。
///
/// 注意 JSON 对象属性顺序无语义（serde_json 的 Map 默认按 key 排序），
/// 这里只负责写入，不保证位置。
fn insert_target(props: &mut Value, target: Option<&Value>) {
    if let (Some(obj), Some(t)) = (props.as_object_mut(), target) {
        obj.insert("target".into(), t.clone());
    }
}

/// exec_ssh / bastion_session_exec 的可选 timeoutSeconds 属性（两种工具共用）。
fn timeout_prop() -> Value {
    json!({
        "type": "integer",
        "description": format!(
            "单命令执行超时（秒），默认 {}，最大 {}。长耗时命令（如软件包升级）可适当调大",
            exec::DEFAULT_EXEC_TIMEOUT_SECS,
            exec::MAX_EXEC_TIMEOUT_SECS
        ),
        "minimum": 1,
        "maximum": exec::MAX_EXEC_TIMEOUT_SECS
    })
}

/// 返回该 kind 实例对外暴露的工具定义列表 `{name, description, inputSchema}`。
///
/// - SSH MCP 暴露 `exec_ssh` + `list_files` / `upload_file` / `download_file`（文件
///   级运维，基于 SFTP 短连接）。
/// - DB MCP 暴露 `exec_sql`（若绑定了具体库，描述中注明）。
///
/// 按 `resource_mode` 分支：
/// - `"bound"`（默认）：目标由绑定资源决定，工具参数只传 command/sql/path 等。
/// - `"client"`（客户端直连）：目标与凭据由调用方在参数中传入
///   （host/port/username/password），工具描述中注明密码不存储不落日志。
/// - `"multi"`（多机，仅 SSH）：绑定一组会话，工具必传 `target`（授权机器的
///   展示名，schema 附 enum 约束），另暴露只读 `list_machines` 供模型路由。
/// - `"bastion"`（堡垒机，仅 SSH）：绑定堡垒机会话配置，只暴露 5 个
///   `bastion_*` 会话式工具。
///
/// `machines` 是 multi 模式当前的授权机器清单（其它模式忽略；由调用方现算，
/// 热切换后立即反映新集合）。
pub(crate) fn tool_defs(
    kind: McpKind,
    resource_mode: &str,
    bound_source: &str,
    bound_database: Option<&str>,
    machines: &[MultiMachine],
) -> Vec<Value> {
    match kind {
        McpKind::Ssh => tool_defs_ssh(resource_mode, bound_source, machines),
        McpKind::Db => tool_defs_db(resource_mode, bound_database),
        McpKind::File => tool_defs_file(),
    }
}

/// SSH kind 的工具定义（按资源模式分支：bastion / client / multi / bound）。
fn tool_defs_ssh(resource_mode: &str, bound_source: &str, machines: &[MultiMachine]) -> Vec<Value> {
    let client_mode = resource_mode == "client";
    let multi_mode = resource_mode == "multi";

    // 堡垒机模式：只暴露 6 个 bastion_* 工具（以「会话」为单位按需进出
    // 资产主机；exec_ssh / 文件工具不适用于该模式，避免误在堡垒机本机执行）。
    if resource_mode == "bastion" {
        return vec![
            json!({
                "name": "bastion_list_hosts",
                "description": "列出当前 SSH MCP 绑定的堡垒机（如 JumpServer）授权可访问的主机清单\
            （读取登录后的资产菜单回显）。只读操作，无需人工确认。\
            如果清单为空或不含目标，也可直接向 bastion_create_session 传已知的资产 IP / 名称。",
                "inputSchema": { "type": "object", "properties": {}, "required": [] }
            }),
            json!({
                "name": "bastion_create_session",
                "description": "在堡垒机上进入一台目标主机：复用堡垒机的已认证连接开一条会话通道，\
            连接就绪后返回 sessionId（后续 bastion_session_exec / bastion_close_session 使用）。\
            host 为目标主机的资产 IP 或资产名称（可先用 bastion_list_hosts 查看）。\
            若管理员配置了「登录后命令」（如 sudo su -），会自动执行一次，之后 bastion_session_exec \
            的命令都在其上下文（如 root 登录 shell）中执行。\
            复用连接不会重复要求动态口令；会话空闲超过配置时长会被自动回收，届时需重新创建。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string", "description": "目标主机的资产 IP 或资产名称" }
                    },
                    "required": ["host"]
                }
            }),
            json!({
                "name": "bastion_session_exec",
                "description": "在 bastion_create_session 建立的会话对应的目标主机上执行一条 shell 命令\
            （非交互），返回标准输出和标准错误的合并文本。输出截断 16KB。\
            执行前需要 X-Term 用户人工确认。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": { "type": "string", "description": "bastion_create_session 返回的会话 id" },
                        "command": { "type": "string", "description": "要执行的 shell 命令（单条，非交互）" },
                        "timeoutSeconds": timeout_prop()
                    },
                    "required": ["sessionId", "command"]
                }
            }),
            json!({
                "name": "bastion_close_session",
                "description": "关闭一个不再使用的堡垒机会话（断开到目标主机的连接）。\
            建议任务完成后主动关闭，避免占满堡垒机的并发连接数。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": { "type": "string", "description": "bastion_create_session 返回的会话 id" }
                    },
                    "required": ["sessionId"]
                }
            }),
            json!({
                "name": "bastion_list_sessions",
                "description": "列出当前活跃的堡垒机会话（sessionId / 目标主机 / 存活与空闲时长）。\
            只读操作，无需人工确认。",
                "inputSchema": { "type": "object", "properties": {}, "required": [] }
            }),
            json!({
                "name": "bastion_upload_file",
                "description": "把 X-Term 所在主机的一个本地文件上传到会话所在的目标主机。\
            实现方式：在会话 shell 里把文件按 base64 分块写入远端临时文件，再解码到 remotePath 并校验\
            （校验字节数，远端有 sha256sum 时再比对哈希）——因为到目标主机只有 shell 通道，没有 SFTP。\
            会**覆盖**目标路径上已存在的同名文件；目标目录必须已存在且可写。\
            依赖目标机的 base64（coreutils / busybox 自带）。单文件上限 32 MiB，整体超时 15 分钟\
            （受会话通道往返限制，比 SFTP 慢，适合配置、脚本、几 MiB 的二进制）。\
            传输期间该会话不能执行其它命令。\
            执行前需要 X-Term 用户人工确认（会展示完整的本地与远端路径）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sessionId": { "type": "string", "description": "bastion_create_session 返回的会话 id" },
                        "localPath": { "type": "string", "description": "X-Term 所在主机的本地文件路径（必须已存在）" },
                        "remotePath": { "type": "string", "description": "目标主机上的目标路径，如 /opt/app/x.bin（父目录需已存在）" }
                    },
                    "required": ["sessionId", "localPath", "remotePath"]
                }
            }),
        ];
    }

    // 连接参数（client 模式专用）。
    let conn_props = if client_mode {
        json!({
            "host": { "type": "string", "description": "目标服务器 IP 或域名" },
            "port": {
                "type": "integer",
                "description": "SSH 端口，默认 22",
                "minimum": 1,
                "maximum": 65535
            },
            "username": { "type": "string", "description": "登录用户名" },
            "password": {
                "type": "string",
                "description": "登录密码（敏感字段，仅本次调用有效）"
            }
        })
    } else {
        json!({})
    };
    let conn_required = if client_mode {
        vec!["host", "username", "password"]
    } else {
        vec![]
    };
    // 多机模式：target 参数（enum 约束为授权机器的展示名，帮助模型精确路由）。
    // 机器清单为空（全部会话被删）时不加 enum——空 enum 是非法 JSON Schema，
    // 描述改为告知无可机器，模型会转告用户重新勾选。
    let (target_prop, has_target): (Option<Value>, bool) = if multi_mode {
        let desc = if machines.is_empty() {
            "目标机器名。当前无可用的授权机器（会话可能已被删除），请告知用户到 X-Term 的 MCP 页面重新勾选，不要调用任何工具。".to_string()
        } else {
            format!(
                "目标机器名（必须是用户授权的机器之一，当前可用：{}）。可用 list_machines 查看各机器的地址与标签",
                machines
                    .iter()
                    .map(|m| format!("{} ({}@{})", m.display_name, m.username, m.host))
                    .collect::<Vec<_>>()
                    .join("、")
            )
        };
        let mut prop = json!({ "type": "string", "description": desc });
        if !machines.is_empty() {
            prop["enum"] = Value::Array(
                machines
                    .iter()
                    .map(|m| Value::String(m.display_name.clone()))
                    .collect(),
            );
        }
        (Some(prop), true)
    } else {
        (None, false)
    };

    let mut tools = Vec::with_capacity(5);

    // exec_ssh
    let mut exec_props = conn_props.clone();
    if let Some(obj) = exec_props.as_object_mut() {
        obj.insert(
            "command".into(),
            json!({
                "type": "string",
                "description": "要执行的 shell 命令（单条，非交互）"
            }),
        );
        obj.insert("timeoutSeconds".into(), timeout_prop());
    }
    insert_target(&mut exec_props, target_prop.as_ref());
    let mut exec_required = conn_required.clone();
    exec_required.push("command");
    if has_target {
        exec_required.insert(0, "target");
    }
    // 终端标签页绑定：命令写入用户已打开的终端执行（支持跳板嵌套）。
    let terminal_bound = !client_mode && bound_source == "terminal";
    let exec_desc = if client_mode {
        "在调用方指定的服务器上执行一条 shell 命令（非交互），返回标准输出和标准错误的合并文本。\
目标服务器由参数 host/port/username 指定，password 为登录密码（敏感字段，仅本次调用有效，\
X-Term 不存储、不落日志）。输出截断 16KB。执行前需要 X-Term 用户人工确认。"
    } else if multi_mode {
        "在 target 指定的机器上执行一条 shell 命令（非交互），返回标准输出和标准错误的合并文本。\
target 必须从用户授权的机器中选择（可用 list_machines 查看地址与标签再决定）。\
输出截断 16KB。执行前需要 X-Term 用户人工确认。"
    } else if terminal_bound {
        "在 X-Term 中绑定的**终端标签页**里执行一条 shell 命令（非交互）。\
命令写入该终端 PTY 执行：终端当前在哪个远端主机（含 A→B→C 跳板嵌套），\
命令就在哪个主机上执行，用户可实时看到命令与输出。\
输出截断 16KB。执行前需要 X-Term 用户人工确认。"
    } else {
        "在当前 SSH MCP 绑定的服务器上执行一条 shell 命令（非交互），\
返回标准输出和标准错误的合并文本。输出截断 16KB。\
目标服务器由 X-Term 用户在页面绑定（无需传连接名）。执行前需要 X-Term 用户人工确认。"
    };
    tools.push(json!({
        "name": "exec_ssh",
        "description": exec_desc,
        "inputSchema": {
            "type": "object",
            "properties": exec_props,
            "required": exec_required
        }
    }));

    // 终端标签页绑定：只暴露 exec_ssh。list_files / upload_file / download_file
    // 走 SFTP 短连接、拿不到终端所在的远端主机，执行必然失败——不暴露以免模型
    // 反复试错（执行侧同样有兜底拒绝）。
    if terminal_bound {
        return tools;
    }

    // list_files（只读，免确认）
    let mut list_props = conn_props.clone();
    if let Some(obj) = list_props.as_object_mut() {
        obj.insert(
            "path".into(),
            json!({
                "type": "string",
                "description": "要列举的远端目录绝对路径，如 /home/user、/var/log"
            }),
        );
    }
    insert_target(&mut list_props, target_prop.as_ref());
    let mut list_required = conn_required.clone();
    list_required.push("path");
    if has_target {
        list_required.insert(0, "target");
    }
    let list_desc = if client_mode {
        "列出调用方指定服务器上某目录的内容（基于 SFTP）。\
返回 JSON 数组，元素含 name / isDir / size / modified。\
目标由 host/port/username 指定，password 为登录密码（敏感字段，仅本次调用有效）。\
只读操作，无需人工确认。"
    } else if multi_mode {
        "列出 target 指定机器上某目录的内容（基于 SFTP）。\
返回 JSON 数组，元素含 name / isDir / size / modified。只读操作，无需人工确认。"
    } else {
        "列出当前 SSH MCP 绑定服务器上某目录的内容（基于 SFTP）。\
返回 JSON 数组，元素含 name / isDir / size / modified。只读操作，无需人工确认。"
    };
    tools.push(json!({
        "name": "list_files",
        "description": list_desc,
        "inputSchema": {
            "type": "object",
            "properties": list_props,
            "required": list_required
        }
    }));

    // upload_file（写，需确认）
    let mut up_props = conn_props.clone();
    if let Some(obj) = up_props.as_object_mut() {
        obj.insert(
            "localPath".into(),
            json!({
                "type": "string",
                "description": "X-Term 所在主机的本地文件路径（必须已存在）"
            }),
        );
        obj.insert(
            "remotePath".into(),
            json!({
                "type": "string",
                "description": "远端目标路径（绝对路径）"
            }),
        );
    }
    insert_target(&mut up_props, target_prop.as_ref());
    let mut up_required = conn_required.clone();
    up_required.push("localPath");
    up_required.push("remotePath");
    if has_target {
        up_required.insert(0, "target");
    }
    let up_desc = if client_mode {
        "把 X-Term 所在主机的一个本地文件上传到调用方指定服务器的远端路径（基于 SFTP）。\
目标服务器由 host/port/username 指定，password 为登录密码（敏感字段，仅本次调用有效）。\
单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。\
提示：可先用 exec_ssh 配合 shell 命令把内容写入本地文件（如 heredoc），再调用本工具上传。"
    } else if multi_mode {
        "把 X-Term 所在主机的一个本地文件上传到 target 指定机器的远端路径（基于 SFTP）。\
单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。\
提示：可先用 exec_ssh 配合 shell 命令把内容写入本地文件（如 heredoc），再调用本工具上传。"
    } else {
        "把 X-Term 所在主机的一个本地文件上传到当前 SSH MCP 绑定服务器的远端路径（基于 SFTP）。\
单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。\
提示：可先用 exec_ssh 配合 shell 命令把内容写入本地文件（如 heredoc），再调用本工具上传。"
    };
    tools.push(json!({
        "name": "upload_file",
        "description": up_desc,
        "inputSchema": {
            "type": "object",
            "properties": up_props,
            "required": up_required
        }
    }));

    // download_file（写本地，需确认）
    let mut dl_props = conn_props.clone();
    if let Some(obj) = dl_props.as_object_mut() {
        obj.insert(
            "remotePath".into(),
            json!({
                "type": "string",
                "description": "要下载的远端文件路径（绝对路径）"
            }),
        );
        obj.insert(
            "localPath".into(),
            json!({
                "type": "string",
                "description": "X-Term 所在主机的本地保存路径（父目录必须存在）"
            }),
        );
    }
    insert_target(&mut dl_props, target_prop.as_ref());
    let mut dl_required = conn_required.clone();
    dl_required.push("remotePath");
    dl_required.push("localPath");
    if has_target {
        dl_required.insert(0, "target");
    }
    let dl_desc = if client_mode {
        "从调用方指定服务器下载一个远端文件到 X-Term 所在主机的本地路径（基于 SFTP），\
成功后返回本地路径。目标服务器由 host/port/username 指定，password 为登录密码\
（敏感字段，仅本次调用有效）。单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。"
    } else if multi_mode {
        "从 target 指定机器下载一个远端文件到 X-Term 所在主机的本地路径（基于 SFTP），\
成功后返回本地路径。单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。"
    } else {
        "从当前 SSH MCP 绑定服务器下载一个远端文件到 X-Term 所在主机的本地路径\
（基于 SFTP），成功后返回本地路径。单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。"
    };
    tools.push(json!({
        "name": "download_file",
        "description": dl_desc,
        "inputSchema": {
            "type": "object",
            "properties": dl_props,
            "required": dl_required
        }
    }));

    // list_machines（多机模式专用，只读免确认）：授权机器清单，供模型路由决策。
    if multi_mode {
        tools.push(json!({
            "name": "list_machines",
            "description": format!(
                "列出用户授权的全部机器（目标名 / 地址 / 标签）。当前共 {} 台：{}。\
其余工具的 target 参数必须使用这里列出的「目标名」（完全一致）。只读操作，无需人工确认。",
                machines.len(),
                machines
                    .iter()
                    .map(|m| format!(
                        "{}（{}@{}:{}{}）",
                        m.display_name,
                        m.username,
                        m.host,
                        m.port,
                        m.tags
                            .as_deref()
                            .filter(|t| !t.trim().is_empty())
                            .map(|t| format!("，标签: {}", t))
                            .unwrap_or_default()
                    ))
                    .collect::<Vec<_>>()
                    .join("、")
            ),
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }));
    }

    tools
}

/// DB kind 的工具定义（bound / client 两分支）。
fn tool_defs_db(resource_mode: &str, bound_database: Option<&str>) -> Vec<Value> {
    if resource_mode == "client" {
        vec![json!({
            "name": "exec_sql",
            "description": "在调用方指定的数据库服务器（MySQL / PostgreSQL）上执行 SQL，返回结果表格文本。\
        目标数据库由参数 host/port/username 指定，password 为连接密码（敏感字段，仅本次调用有效，\
        X-Term 不存储、不落日志）。dbKind 指定数据库类型（mysql/postgres，默认 mysql）；\
        database 可选，传则作为默认库。\
        执行前需要 X-Term 用户人工确认。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": {
                        "type": "string",
                        "description": "目标数据库服务器 IP 或域名"
                    },
                    "port": {
                        "type": "integer",
                        "description": "数据库端口（MySQL 默认 3306，PostgreSQL 默认 5432）",
                        "minimum": 1,
                        "maximum": 65535
                    },
                    "username": {
                        "type": "string",
                        "description": "连接用户名"
                    },
                    "password": {
                        "type": "string",
                        "description": "连接密码（敏感字段，仅本次调用有效）"
                    },
                    "dbKind": {
                        "type": "string",
                        "enum": ["mysql", "postgres"],
                        "description": "数据库类型，默认 mysql"
                    },
                    "database": {
                        "type": "string",
                        "description": "默认数据库（可选，省略则 SQL 需带限定名）"
                    },
                    "sql": { "type": "string", "description": "要执行的 SQL 语句" },
                    "limit": {
                        "type": "integer",
                        "description": "返回行数上限，默认 100",
                        "minimum": 1
                    }
                },
                "required": ["host", "username", "password", "sql"]
            }
        })]
    } else {
        let db_hint = match bound_database {
            Some(db) => format!("当前绑定的数据库为「{}」，SQL 将在此库上执行。", db),
            None => "目标数据库由 X-Term 用户在页面绑定（无需传连接名）。".to_string(),
        };
        vec![json!({
            "name": "exec_sql",
            "description": format!(
                "在当前 DB MCP 绑定的数据库（MySQL / PostgreSQL，按绑定连接的类型）上执行 SQL，返回结果表格文本。{}\
        执行前需要 X-Term 用户人工确认。",
                db_hint
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "要执行的 SQL 语句" },
                    "limit": {
                        "type": "integer",
                        "description": "返回行数上限，默认 100",
                        "minimum": 1
                    }
                },
                "required": ["sql"]
            }
        })]
    }
}

/// File kind 的工具定义（绑定 S3 账号，仅 bound 模式）。
fn tool_defs_file() -> Vec<Value> {
    vec![
        json!({
            "name": "list_files",
            "description": "列出当前 File MCP 绑定的 S3 存储桶中某前缀下的内容。\
        返回 JSON 数组，元素含 name / isDir / size / modified。只读操作，无需人工确认。\
        目标账号由 X-Term 用户在页面绑定（无需传连接信息）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "对象 key 前缀（目录），以 `/` 结尾，如 logs/、backup/2024/。传空串表示 bucket 根。"
                    }
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "upload_file",
            "description": "把 X-Term 所在主机的一个本地文件上传到当前 File MCP 绑定的 S3 存储桶。\
        单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。\
        提示：可先用 exec_ssh 配合 shell 命令把内容写入本地文件（如 heredoc），再调用本工具上传。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "localPath": {
                        "type": "string",
                        "description": "X-Term 所在主机的本地文件路径（必须已存在）"
                    },
                    "remotePath": {
                        "type": "string",
                        "description": "S3 对象 key（目标路径），如 logs/app.log"
                    }
                },
                "required": ["localPath", "remotePath"]
            }
        }),
        json!({
            "name": "download_file",
            "description": "从当前 File MCP 绑定的 S3 存储桶下载一个对象到 X-Term 所在主机的本地路径，\
        成功后返回本地路径。单文件超时 5 分钟。执行前需要 X-Term 用户人工确认。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "remotePath": {
                        "type": "string",
                        "description": "要下载的 S3 对象 key"
                    },
                    "localPath": {
                        "type": "string",
                        "description": "X-Term 所在主机的本地保存路径（父目录必须存在）"
                    }
                },
                "required": ["remotePath", "localPath"]
            }
        }),
    ]
}
