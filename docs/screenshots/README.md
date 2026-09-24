# 界面截图

本目录存放 README / 使用文档引用的产品界面截图。

## 截图来源

所有截图都是**程序真实前端界面的渲染结果**，不是设计稿或手绘：

- 页面来自 `pnpm build` 产出的 `dist/`（与当前源码一致的构建产物）；
- 由本机 Edge 无头模式按 1600×1000 视口渲染，走真实的 Vue 组件、Element Plus 样式与 xterm 终端渲染链路；
- 数据来自一层 **Tauri IPC mock**（`scripts/_shots/mock.js`），用演示用的会话 / 数据库 / MCP 配置喂给界面。也就是说：**界面是真的，数据是演示数据**。

因此截图里出现的服务器名、IP、订单号等均为虚构，不涉及任何真实环境信息。

## 文件清单

| 文件 | 页面 |
|---|---|
| `01-terminal.png` | 终端页（会话树 + 多标签终端 + 快捷命令栏） |
| `02-terminal-tabs.png` | 终端多标签切换 |
| `03-monitor.png` | 服务器监控页签（CPU / 内存 / 网络 / 负载 / 磁盘 / 进程） |
| `04-ai-agent.png` | AI 智能体面板（任务清单 + 工具调用确认卡片） |
| `05-sftp.png` | SFTP 本地 / 远程双栏 |
| `06-sql.png` | MySQL 控制台（数据库树 + 命令行模式 + 结果表格） |
| `07-mcp.png` | MCP 服务端（实例状态、绑定、Token、执行日志） |
| `08-mcp-approval.png` | 外部 AI 调用时的审批浮层 |
| `09-files-s3.png` | 对象存储（S3）文件管理 |
| `10-forward.png` | 端口转发规则 |
| `11-vault.png` | 密钥保险库 |
| `12-mfa.png` | MFA 动态验证码 |
| `13-desktop.png` | 远程桌面（RDP / VNC）连接管理 |
| `14-settings.png` | 设置页（终端 / 外观 / 连接） |
| `15-about.png` | 关于页（检查更新） |

## 重新生成

截图工具链在 `scripts/_shots/`（该目录已 git 忽略，不入库）。重新生成步骤：

```bash
# 1. 构建前端
pnpm build

# 2. 起静态服务（端口与脚本中的 BASE 一致）
python -m http.server 4399 -d dist

# 3. 安装渲染依赖（仅本地）
cd scripts/_shots && npm i playwright-core

# 4. 截图（输出到 docs/screenshots）
node shot.mjs
```

驱动脚本 `shot.mjs` 里每个场景都描述了「路由 + 需要点的按钮/树节点」，改了界面后按需调整即可。
