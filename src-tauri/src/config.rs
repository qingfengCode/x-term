//! 全局设置（settings.json）。
//!
//! 设置以 JSON 文件形式存储（用户可手动编辑、版本管理友好）。包含：
//! - 终端外观与行为（主题、字体、滚动等）；
//! - AI provider 列表与当前激活项；
//! - 其他全局偏好。

use serde::{Deserialize, Serialize};

use crate::ai::provider::ProviderConfig;
use crate::error::AppResult;
use crate::state::AppState;
use crate::storage::json_store;

/// 终端外观与行为设置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettings {
    /// 主题名："dark" | "light" | "solarized-dark" | ...
    #[serde(default = "default_theme")]
    pub theme: String,
    /// 字体族。
    #[serde(default = "default_font_family")]
    pub font_family: String,
    /// 字号。
    #[serde(default = "default_font_size")]
    pub font_size: u16,
    /// 行高倍数。
    #[serde(default = "default_line_height")]
    pub line_height: f32,
    /// 滚屏最大行数（0 = 无限）。
    #[serde(default = "default_scrollback")]
    pub scrollback: u32,
    /// 选中即复制。
    #[serde(default = "default_copy_on_select")]
    pub copy_on_select: bool,
    /// 是否启用 Webgl 渲染。
    #[serde(default = "default_enable_webgl")]
    pub enable_webgl: bool,
}

fn default_theme() -> String {
    "dark".into()
}
fn default_font_family() -> String {
    "Consolas, 'Cascadia Code', 'Courier New', monospace".into()
}
fn default_font_size() -> u16 {
    14
}
fn default_line_height() -> f32 {
    1.2
}
fn default_scrollback() -> u32 {
    10000
}
fn default_copy_on_select() -> bool {
    true
}
fn default_enable_webgl() -> bool {
    true
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            font_family: default_font_family(),
            font_size: default_font_size(),
            line_height: default_line_height(),
            scrollback: default_scrollback(),
            copy_on_select: default_copy_on_select(),
            enable_webgl: default_enable_webgl(),
        }
    }
}

// ===========================================================================
// 快捷命令 / 快捷键
// ===========================================================================

/// 一条快捷命令。
///
/// 既作为"终端底部快捷命令栏"的一个按钮，也（可选）绑定一个全局快捷键。
/// 按下按钮或触发快捷键时，把 `command` 文本发送到当前活动终端。
///
/// `command` 支持占位符（前端解析，MVP 不强制）：
/// - `{host}` / `{user}` / `{port}`：当前会话对应字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutCommand {
    /// 唯一 id（前端生成 uuid）。
    pub id: String,
    /// 显示名称（按钮文字）。
    pub label: String,
    /// 要发送的命令文本（不含换行；执行时自动追加换行）。
    pub command: String,
    /// 可选的快捷键组合，如 "Ctrl+1" / "F1" / "Ctrl+Shift+R"。
    /// 为空表示仅作为按钮、不绑定按键。
    #[serde(default)]
    pub shortcut: Option<String>,
    /// 所属分组名称。为空表示未分组。
    #[serde(default)]
    pub group: Option<String>,
}

/// 快捷键设置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutSettings {
    /// 快捷命令列表。
    #[serde(default)]
    pub commands: Vec<ShortcutCommand>,
    /// 有序分组名列表（用于前端标签页排序）。
    #[serde(default)]
    pub groups: Vec<String>,
    /// 应用级快捷键绑定（action -> 组合键）。键为动作名（如 "newTab"），
    /// 值为组合键字符串（如 "Ctrl+T"）。前端读取并在全局 keydown 中分发。
    /// 老配置文件没有此字段时使用 `default_app_shortcuts`。
    #[serde(default = "default_app_shortcuts")]
    pub app: std::collections::BTreeMap<String, String>,
}

/// 默认应用级快捷键（与前端 `APP_SHORTCUT_METAS` 保持一致）。
fn default_app_shortcuts() -> std::collections::BTreeMap<String, String> {
    let mut m = std::collections::BTreeMap::new();
    m.insert("newTab".into(), "Ctrl+T".into());
    m.insert("closeTab".into(), "Ctrl+W".into());
    m.insert("nextTab".into(), "Ctrl+Tab".into());
    m.insert("prevTab".into(), "Ctrl+Shift+Tab".into());
    m.insert("copy".into(), "Ctrl+Shift+C".into());
    m.insert("paste".into(), "Ctrl+Shift+V".into());
    m.insert("toggleAi".into(), "Ctrl+I".into());
    m.insert("search".into(), "Ctrl+F".into());
    m.insert("focusSessions".into(), "Ctrl+P".into());
    m
}

/// AI 设置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSettings {
    /// 已配置的 provider 列表（含 api key 等）。
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// 当前激活的 provider 在列表中的 id（用 base_url + model 作为简易标识；
    /// 这里直接存索引或唯一字段）。简化：存 active provider 的 (kind, model)。
    #[serde(default)]
    pub active: Option<String>,
    /// SSH 智能体配置（exec_ssh 工具）。
    #[serde(default)]
    pub ssh_agent: SshAgentSettings,
    /// SQL 智能体配置（exec_sql 工具）。
    #[serde(default)]
    pub sql_agent: SqlAgentSettings,
    /// 本地文件读写配置（read_file / write_file / list_files 工具）。
    #[serde(default)]
    pub file_access: FileAccessSettings,

    // === 向后兼容：旧字段（已迁移到 ssh_agent）。保留字段以便读取旧 settings.json，
    //     迁移逻辑见 [`migrate_legacy_ai`]。序列化时跳过写出，避免数据重复丢失。 ===
    #[serde(default, skip_serializing)]
    pub command_whitelist: Vec<String>,
    #[serde(default, skip_serializing)]
    pub auto_approve_whitelist: bool,
    #[serde(default, skip_serializing)]
    pub terminal_visualization: bool,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            active: None,
            ssh_agent: SshAgentSettings::default(),
            sql_agent: SqlAgentSettings::default(),
            file_access: FileAccessSettings::default(),
            command_whitelist: Vec::new(),
            auto_approve_whitelist: false,
            terminal_visualization: false,
        }
    }
}

/// SSH 智能体配置（exec_ssh 工具专用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshAgentSettings {
    /// 智能体 `exec_ssh` 命令白名单：命令前缀（如 "df"、"ps"、"systemctl status"）。
    ///
    /// 命中即视为"白名单内"，前端绿色卡片。配合 [`Self::auto_approve_safe`]
    /// 开关可自动放行（详见该字段）。详见 [`crate::ai::tools::check_command_whitelist`]。
    #[serde(default = "default_command_whitelist")]
    pub command_whitelist: Vec<String>,
    /// 白名单内且非危险时自动放行（跳过人工确认按钮）；`false` 时所有命令都需点"执行"确认。
    ///
    /// 安全护栏：`is_dangerous` 判定为危险的命令**永远**走人工确认，不受此开关影响。
    #[serde(default)]
    pub auto_approve_safe: bool,
    /// 终端可视化：`true` 时 `exec_ssh` 命令写入用户活动终端的 PTY（命令和输出
    /// 实时显示在 xterm）；`false` 时走独立 `channel.exec` 连接（输出只在 AI 面板）。
    #[serde(default)]
    pub terminal_visualization: bool,
}

impl Default for SshAgentSettings {
    fn default() -> Self {
        Self {
            command_whitelist: default_command_whitelist(),
            auto_approve_safe: false,
            terminal_visualization: false,
        }
    }
}

/// SQL 智能体配置（exec_sql 工具专用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlAgentSettings {
    /// SQL 执行模式：`"readonly"` | `"restricted"` | `"full"`。
    ///
    /// - `readonly`：只允许 SELECT/SHOW/EXPLAIN/DESCRIBE/DESC/WITH。
    /// - `restricted`：上述 + 允许 INSERT/UPDATE/DELETE/MERGE（DDL 仍需确认）。
    /// - `full`：允许一切（危险操作仍走 `is_dangerous` + 人工确认）。
    #[serde(default = "default_sql_mode")]
    pub sql_mode: String,
    /// 只读查询（SELECT/SHOW/EXPLAIN/DESCRIBE）自动放行（免确认）。
    ///
    /// 安全护栏：`is_dangerous` 判定为危险的 SQL（DROP/TRUNCATE/无 WHERE 的 DELETE）
    /// **永远**走人工确认，不受此开关影响。
    #[serde(default = "default_sql_auto_approve_safe")]
    pub auto_approve_safe: bool,
    /// 终端可视化：`true` 时 AI 执行的 SQL 及结构化结果回显到 SQL 控制台输出流
    /// （命令行模式），就像用户自己敲的一样；`false` 时结果只在 AI 面板。
    /// 与 SSH 智能体的 [`SshAgentSettings::terminal_visualization`] 独立设置。
    #[serde(default)]
    pub terminal_visualization: bool,
}

impl Default for SqlAgentSettings {
    fn default() -> Self {
        Self {
            sql_mode: default_sql_mode(),
            auto_approve_safe: default_sql_auto_approve_safe(),
            terminal_visualization: false,
        }
    }
}

/// AI 本地文件读写配置（read_file / write_file / list_files 工具）。
///
/// - 开关关闭时（默认）：文件工具不下发，AI 行为与之前完全一致。
/// - 开启后：AI 只能在两个助手各自的工作目录（沙箱）内读写文件，路径逃逸
///   （`..`、绝对路径、symlink）一律拒绝；读写自动执行，但**覆盖已有文件**
///   仍走危险确认（防数据被意外覆盖）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileAccessSettings {
    /// 是否启用本地文件读写工具。
    #[serde(default)]
    pub enabled: bool,
    /// 各助手域的工作目录：key = "ssh" | "db"，值为绝对路径。
    /// AI 只能访问该目录及其子目录内的文件。
    #[serde(default)]
    pub workspace_dirs: std::collections::HashMap<String, String>,
}

impl Default for FileAccessSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            workspace_dirs: std::collections::HashMap::new(),
        }
    }
}

fn default_sql_mode() -> String {
    "readonly".into()
}
fn default_sql_auto_approve_safe() -> bool {
    false
}

/// 把旧版（顶层 `command_whitelist` / `auto_approve_whitelist` / `terminal_visualization`）
/// 配置迁移到 [`AiSettings::ssh_agent`]。
///
/// 在 [`settings_load_inner`] 反序列化完成后调用一次。判定逻辑：仅当 `ssh_agent`
/// 对应字段仍是默认值、而旧字段有非默认值时执行迁移；迁移后清空旧字段，避免下次
/// 重复迁移（旧字段 `skip_serializing`，不会写出，但内存里清空以保持一致语义）。
pub fn migrate_legacy_ai(ai: &mut AiSettings) {
    // command_whitelist：旧字段非空、且 ssh_agent 仍是默认白名单时迁移。
    if !ai.command_whitelist.is_empty() && ai.ssh_agent.command_whitelist == default_command_whitelist()
    {
        ai.ssh_agent.command_whitelist = ai.command_whitelist.clone();
    }
    if ai.auto_approve_whitelist && !ai.ssh_agent.auto_approve_safe {
        ai.ssh_agent.auto_approve_safe = true;
    }
    if ai.terminal_visualization && !ai.ssh_agent.terminal_visualization {
        ai.ssh_agent.terminal_visualization = true;
    }
    // 清空旧字段，避免下次重复迁移（旧字段已 skip_serializing，不会持久化）。
    ai.command_whitelist = Vec::new();
    ai.auto_approve_whitelist = false;
    ai.terminal_visualization = false;
}

/// `exec_ssh` 的内置默认命令白名单（常用只读命令）。
///
/// 用户可在设置页增删。设计原则：默认只放**无副作用、只读**的命令，避免任何
/// 可能修改系统状态的操作（rm/mv/dd/systemctl start 等）。
fn default_command_whitelist() -> Vec<String> {
    vec![
        // 系统状态
        "df".into(),
        "du".into(),
        "free".into(),
        "uptime".into(),
        "uname".into(),
        "hostname".into(),
        "date".into(),
        "id".into(),
        "who".into(),
        "w".into(),
        "pwd".into(),
        // 进程 / 负载
        "ps".into(),
        "top".into(),
        "htop".into(),
        // 文件查看（只读）
        "ls".into(),
        "cat".into(),
        "head".into(),
        "tail".into(),
        "less".into(),
        "stat".into(),
        "file".into(),
        "wc".into(),
        "sort".into(),
        "uniq".into(),
        "find".into(),
        // 环境变量
        "env".into(),
        "printenv".into(),
        // 网络
        "netstat".into(),
        "ss".into(),
        "ip".into(),
        "ifconfig".into(),
        "ping".into(),
        "traceroute".into(),
        "nslookup".into(),
        "dig".into(),
        "host".into(),
        // 文本处理（管道一侧常用）
        "grep".into(),
        "egrep".into(),
        "fgrep".into(),
        "awk".into(),
        "sed".into(), // 注意：sed -i 会改文件，但默认按前缀放行；改文件类由 is_dangerous/人工把关
        // 服务状态（只读查询）
        "systemctl status".into(),
        "systemctl list-units".into(),
        "systemctl list-unit-files".into(),
        "journalctl".into(),
        // 磁盘 / 文件系统
        "mount".into(),
        "lsof".into(),
        "lsblk".into(),
        "fdisk -l".into(),
        // 容器查看（只读）
        "docker ps".into(),
        "docker images".into(),
        "docker logs".into(),
        "docker inspect".into(),
        "docker stats".into(),
    ]
    .into_iter()
    .collect()
}

impl AiSettings {
    /// 返回当前激活的 provider 配置；若未设置则取列表第一个。
    pub fn active_provider(&self) -> Option<ProviderConfig> {
        if let Some(active) = &self.active {
            if let Some(p) = self.providers.iter().find(|p| match_active(p, active)) {
                return Some(p.clone());
            }
        }
        self.providers.first().cloned()
    }
}

fn match_active(p: &ProviderConfig, active: &str) -> bool {
    // active 形如 "kind:model"，如 "openai:gpt-4o"。
    let key = format!("{}:{}", p.kind.as_str(), p.model);
    key == active
}

impl ProviderConfig {
    /// 返回该 provider 在设置中的唯一标识 "kind:model"。
    pub fn settings_key(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.model)
    }
}

/// 全局设置根。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default)]
    pub terminal: TerminalSettings,
    #[serde(default)]
    pub ai: AiSettings,
    /// 快捷命令 / 快捷键设置。
    #[serde(default = "default_shortcuts")]
    pub shortcuts: ShortcutSettings,
    /// 是否首次启动（已保存后置 false）。
    #[serde(default)]
    pub first_run: bool,
}

/// 内置默认快捷命令（首次启动时填充，给用户一个起步样例）。
fn default_shortcuts() -> ShortcutSettings {
    ShortcutSettings {
        commands: vec![
            ShortcutCommand {
                id: "sc-default-ls".into(),
                label: "ls -la".into(),
                command: "ls -la".into(),
                shortcut: Some("Ctrl+1".into()),
                group: None,
            },
            ShortcutCommand {
                id: "sc-default-tail".into(),
                label: "tail 日志".into(),
                command: "tail -f /var/log/syslog".into(),
                shortcut: Some("Ctrl+2".into()),
                group: None,
            },
            ShortcutCommand {
                id: "sc-default-disk".into(),
                label: "磁盘".into(),
                command: "df -h".into(),
                shortcut: Some("Ctrl+3".into()),
                group: None,
            },
            ShortcutCommand {
                id: "sc-default-process".into(),
                label: "进程".into(),
                command: "ps aux --sort=-%cpu | head -20".into(),
                shortcut: None,
                group: None,
            },
        ],
        groups: vec![],
        app: default_app_shortcuts(),
    }
}

pub const SETTINGS_FILENAME: &str = "settings.json";

/// 内部读取设置（命令实现共享）。
///
/// 读取后调用 [`migrate_legacy_ai`] 把旧版顶层 AI 配置迁移到 `ssh_agent`。
pub fn settings_load_inner(state: &AppState) -> AppResult<Settings> {
    let path = state.settings_path.as_path().join(SETTINGS_FILENAME);
    let mut settings = json_store::read_json_or_default::<Settings>(&path)?;
    migrate_legacy_ai(&mut settings.ai);
    Ok(settings)
}

// ===========================================================================
// 应用级配置（app.json）：更新源等非用户日常编辑的运行时配置
// ===========================================================================

/// 应用级配置文件名（位于应用数据目录下）。
pub const APP_CONFIG_FILENAME: &str = "app.json";

/// 自更新相关配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConfig {
    /// 更新清单（update.json）地址，指向自建服务器。为空则检查更新时报错提示配置。
    #[serde(default)]
    pub manifest_url: String,
}

/// 应用级配置（区别于用户设置 settings.json，存放更新源等运行期配置）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    #[serde(default)]
    pub update: UpdateConfig,
}

/// 读取应用级配置；文件缺失时返回全默认值。
pub fn app_config_load_inner(path: &std::path::Path) -> AppResult<AppConfig> {
    json_store::read_json_or_default::<AppConfig>(path)
}

/// 写入应用级配置。
pub fn app_config_save_inner(path: &std::path::Path, config: &AppConfig) -> AppResult<()> {
    json_store::write_json(path, config)
}
