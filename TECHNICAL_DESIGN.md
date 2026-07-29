# X-Term

> 现代化运维终端客户端 —— 集 SSH 终端、SFTP 文件传输、端口转发与 AI 智能助手于一体的桌面应用。

类似 SecureCRT / XShell / Tabby / Termius，但用更现代的技术栈（Tauri 2 + Vue 3）重写：体积小、性能好、UI 现代、且内置 AI 运维助手。

![platform](https://img.shields.io/badge/platform-Windows%2010%2F11-blue)
![tauri](https://img.shields.io/badge/Tauri-2.x-orange)
![vue](https://img.shields.io/badge/Vue-3.5-brightgreen)

## ✨ 功能特性

### 核心模块

| 模块 | 能力 |
|---|---|
| **SSH 终端** | 多标签、xterm.js + WebGL 渲染、分组会话树、密码/密钥认证、跳转器、主题与字体可配、URL 可点、选中复制 |
| **SFTP 文件传输** | 双栏浏览（本地/远程）、上传/下载、断点续传、传输队列、进度实时显示、新建/重命名/删除/改权限 |
| **端口转发** | 本地（-L）/ 远程（-R）/ 动态 SOCKS5（-D）；规则持久化、按需启停；本地转发已实现，远程/动态为占位 |
| **MySQL 数据库** | SQL 控制台、表结构浏览、表格化结果展示；支持**直连**与**经 SSH 隧道**两种连接方式；只读/读写模式切换 |
| **AI 智能助手** | BYOK 多模型；流式响应；**智能体模式**——AI 可实际操作 SSH 执行命令、操作 MySQL 执行 SQL（人确认后执行） |

### AI 智能化（核心差异化）

AI 不只是"回答问题"，而是**会调工具、能执行操作**的智能体：

- **Tool Calling 协议**：通过 OpenAI Function Calling / Claude tools 协议，AI 可调用 5 个工具：
  - `exec_ssh(sessionId, command)` — 在服务器上执行 shell 命令（非交互 `channel.exec`，输出干净）
  - `terminal_snapshot(sessionId)` — 读取终端最近输出（"上下文感知"）
  - `exec_sql(dbConnId, sql, limit)` — 在 MySQL 上执行 SQL
  - `list_db_tables(dbConnId)` / `describe_table(dbConnId, table)` — 数据库元数据
- **人确认执行机制**：AI 发起工具调用后**不自动执行**，前端弹出确认卡片（含参数预览），用户点"执行"才真正调用；拒绝则告知 AI"用户拒绝"。这保证 AI 永远不会未经允许动用户的服务器/数据库。
- **安全护栏**：危险命令（`rm -rf /`、`mkfs`、`dd of=/dev/`、`shutdown`/`reboot`、fork bomb、`chmod -R 777 /`）和无 `WHERE` 的 `DELETE`、`DROP`/`TRUNCATE` 自动识别并红色高亮 + 强制确认。
- **多轮编排**：AI 可以连续调用多个工具完成任务（如"先看磁盘满没满 → 满了就找大文件 → 列出 top10"），最多 10 轮、总超时保护。
- **终端上下文感知**：reader 任务维护最近 64KB 输出的环形缓冲，AI 通过 `terminal_snapshot` 知道用户屏幕上当前是什么。

典型对话：
```
用户（agent 模式）：看看这台机器磁盘满没满
AI：好，我来检查磁盘使用情况。
    [工具调用] exec_ssh: df -h              [执行] [拒绝]
用户点执行
AI：根分区 / 使用 45%（可用 12G），未满。/data 用了 87%，接近告警线。
    建议进一步查看 /data 下的大文件吗？
```

### 安全设计

- **凭据保险库**：所有密码/私钥/MySQL 密码用主密码（Argon2id 派生密钥）+ AES-256-GCM 加密后存盘，运行时内存解密，主密码不存盘。
- 首次启动需创建保险库（设置主密码）；之后每次启动需解锁。
- 丢失主密码无法找回，需重新输入凭据。
- **AI 操作安全**：默认所有工具调用必须人工确认；危险操作二次确认；单命令 30s 超时、输出 16KB 截断；SQL 默认只读、最多 100 行。

## 🏗 技术栈

**后端（Rust）**：Tauri 2、russh 0.45（SSH/SFTP）、russh-sftp、rusqlite（bundled SQLite）+ r2d2 连接池、sqlx（MySQL，runtime-tokio-rustls）、aes-gcm + argon2（加密）、reqwest（AI HTTP）、regex（安全护栏）。

**前端（TypeScript）**：Vue 3.5 + Vite、Element Plus、Pinia、Vue Router、xterm.js（终端）、@tauri-apps/api。

**存储**：混合方案
- SQLite：会话、分组、命令历史、转发规则、DB profile、日志。
- JSON：全局设置（主题/字体/AI 配置）。
- 加密文件：凭据（`credentials.enc` + `master.key`）。

## 📂 项目结构

```
x-term/
├── src-tauri/                  # Rust 后端
│   ├── src/
│   │   ├── main.rs             # 入口
│   │   ├── lib.rs              # 应用装配、命令注册
│   │   ├── state.rs            # AppState（终端/MySQL/隧道/工具确认状态）
│   │   ├── error.rs            # 统一 AppError
│   │   ├── events.rs           # Tauri 事件定义（含工具调用/SQL 结果）
│   │   ├── config.rs           # settings.json 模型
│   │   ├── commands/           # #[tauri::command] 处理器
│   │   │   ├── vault.rs  session.rs  terminal.rs
│   │   │   ├── sftp.rs  forward.rs  ai.rs  db.rs  config.rs
│   │   ├── ssh/                # SSH 核心
│   │   │   ├── client.rs  session.rs  sftp.rs  tunnel.rs
│   │   ├── storage/            # 持久化（SQLite + JSON + 加密）
│   │   │   ├── db.rs  sessions_repo.rs  history_repo.rs
│   │   │   ├── json_store.rs  secure.rs
│   │   ├── database/           # MySQL 业务连接管理
│   │   │   ├── mysql.rs  profiles.rs
│   │   └── ai/                 # LLM provider + 工具调用
│   │       ├── provider.rs  openai.rs  claude.rs  prompts.rs
│   │       └── tools.rs        # 工具定义、执行器、安全护栏
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   └── capabilities/default.json
│
├── src/                        # Vue 前端
│   ├── main.ts  App.vue  env.d.ts
│   ├── router/  styles/  utils/
│   ├── api/                    # invoke 调用封装 + 类型
│   ├── stores/                 # Pinia: settings/vault/sessions/terminals/transfer/ai
│   ├── views/                  # 路由级页面
│   │   ├── MainLayout.vue  Workspace.vue  SftpView.vue
│   │   ├── ForwardView.vue  Settings.vue  UnlockView.vue
│   └── components/             # 可复用组件
│       ├── SessionSidebar.vue  SessionDialog.vue
│       ├── TerminalPane.vue  AiPanel.vue  TransferQueue.vue
│
├── package.json  vite.config.ts  tsconfig.json
└── README.md
```

## 🚀 快速开始

### 环境要求

- **Node.js** ≥ 20（推荐 24）、**pnpm** ≥ 10
- **Rust** ≥ 1.77（stable，MSVC 工具链 `x86_64-pc-windows-msvc`）
- **Microsoft Visual Studio 2022**（含 "Desktop development with C++" 工作负载）或 Build Tools
- **Windows 10/11 SDK**

### ⚠️ 构建注意事项（Windows + GCC 干扰）

如果系统装了 MinGW/TDM-GCC 且环境变量里设了 `CC=gcc`，会导致 `libsqlite3-sys`（bundled SQLite）被编成 GNU ABI 对象，链接时报：

```
error LNK2019: unresolved external symbol __chkstk_ms
```

**解决方法**：构建前确保不使用 GCC：

```bash
# Git Bash / PowerShell 中
unset CC CXX          # bash
$env:CC=""; $env:CXX=""  # PowerShell
```

让 cc crate 自动探测并使用 MSVC 的 `cl.exe`。

### 安装与运行

```bash
# 1. 安装前端依赖
pnpm install

# 2. 开发模式（同时启动 vite 和 tauri，自动热重载）
pnpm tauri:dev

# 3. 生产构建（生成 .msi / .exe 安装包）
pnpm tauri:build
# 产物：src-tauri/target/release/bundle/
```

### 仅前端开发（不经 Tauri）

```bash
pnpm dev          # 仅 vite，浏览器预览（后端 invoke 调用会失败）
pnpm build        # 类型检查 + 打包到 dist/
```

## 🧭 使用指南

1. **首次启动**：创建凭据保险库（设置一个 ≥6 位主密码，务必记牢）。
2. **添加会话**：左侧"会话"区点 + 新建，填主机/端口/用户名/认证方式。
3. **连接**：单击会话节点 → 自动打开终端 tab。
4. **SFTP**：顶部导航切到"SFTP"，选会话 → 打开 SFTP → 双栏浏览传输。
5. **MySQL**：导航到"SQL"，新建 DB profile（主机/端口/用户/密码；可选 SSH 隧道）→ 连接 → 写 SQL 执行，或点左侧表名快速 SELECT。
6. **端口转发**：导航到"转发"，新建规则（本地/远程/动态）→ 启动。
7. **AI 助手**：右侧折叠条点击展开，先到"设置 → AI 助手"配置 provider 与 API Key。
   - **对话/翻译/诊断/解释模式**：纯问答。
   - **智能体模式**：让 AI 实际操作。先连接一个终端（和/或 MySQL），切到"智能体"模式，描述任务（如"看看磁盘满没满""找出最大的 5 张表"）。AI 提出要执行的命令/SQL，你点"执行"确认后它真正调用，再根据结果继续。
8. **设置**：导航到"设置"，调整终端主题/字体、AI provider 等。

## 🔑 数据存放位置

- 应用数据目录：`%APPDATA%\x-term\`
  - `xterm.db`：SQLite 数据库
  - `settings.json`：全局设置
  - `credentials.enc`：加密凭据
  - `master.key`：主密钥包装信息（含 salt + verifier）

## 🛣 后续路线图（非 MVP）

- [ ] 远程端口转发（-R）与 SOCKS5 动态转发的完整实现
- [ ] ssh-agent 认证支持
- [ ] Jump Host 链式连接的实际 UI（后端字段已预留）
- [ ] 终端 Ctrl+I 唤起 AI：选中文本 → 一键解释/诊断
- [ ] 命令历史下拉补全
- [ ] 多 provider 并发对比
- [ ] 凭据导出加密备份（跨机器迁移）
- [ ] known_hosts 校验（当前为接受所有 host key，生产环境前应补上）

## 📝 设计要点

### 终端数据流（高性能）

前端键盘输入 → `invoke('terminal_write', ...)` → 后端写入 mpsc → reader 任务调 `channel.data()`。
远程输出 → reader 任务 `channel.wait()` → base64 → `emit('terminal:data')` → 前端 xterm 写入。

输出走事件而非 invoke 返回值，避免高频数据的同步等待；输入走 mpsc 是因为 russh 的 `Channel::wait()` 需要 channel 独占所有权（与 `data()` 互斥），通过 mpsc 把输入指令转发到 reader 任务统一执行。

### 安全模型

主密钥永远不出现在磁盘明文中：`master.key` 文件只存 Argon2 的 salt 和一个用主密钥加密的固定 verifier。解锁时派生密钥 → 解密 verifier → 比对，失败即视为口令错误。运行时凭据仅在内存中解密使用。

### AI 工具调用与"人确认执行"

```
[用户提问]
   ↓
[run_agent_loop] ←→ [LlmProvider.chat_with_tools]（带 tools 声明）
   ↓ 模型返回 tool_calls
[emit ai:tool_call]  ──────────────► [前端弹确认卡片]
   ↓ oneshot 阻塞等待                    ↓ 用户点执行/拒绝
[execute_tool]  ◄────── ai_execute_tool / ai_cancel_tool
   ↓ 工具结果
[回填 messages，继续下一轮]  → 直至模型返回纯文本
```

关键设计：
- **不自动执行**：后端发起 tool_call 后挂起在 oneshot channel 上，前端必须 `invoke('ai_execute_tool')` 才放行。这是"人确认"的硬保障，AI 无法绕过。
- **exec_ssh 不复用交互式 PTY**：每次工具调用基于会话配置新建一条独立 SSH 连接，用 `channel.exec()` 非交互执行，输出无 ANSI/提示符污染。
- **MySQL 经 SSH 隧道**：直连用 sqlx；走 SSH 时后端起一个本地随机端口 TcpListener，对每个 sqlx 连接开一个 `channel_open_direct_tcpip` 桥接，pool 上限 2 防止 SSH channel 泛滥。
- **失控保护**：单对话最多 10 轮工具调用；用户 5 分钟不确认自动拒绝。

## 📄 许可证

本项目代码可自由用于学习和内部使用。
