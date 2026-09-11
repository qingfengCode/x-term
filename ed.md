# 代码规范问题核查清单（ed.md）

> 审查范围：`src/` 前端全部（排除 `tabby-master`、`uniterm-main`、`src-tauri/vendor` 等第三方目录）。
> 核查方式：逐项读取源码验证，所有行号、代码摘录均来自当前磁盘文件。
> 状态标识：✅ 已确认存在 ｜ ⚠️ 存在但有注释说明属有意取舍 ｜ （修正）= 与初版报告数据有出入，已按实际修正。
> 本清单只列出问题，未修改任何代码。

---

## P0 真实缺陷 / 风险（已逐项核实）

> 处理记录（2026-09-11）：1-3、10-19 已修复并通过 `vue-tsc --noEmit` 类型检查；4-9 评估后保留（详见文末「P0 处理记录」）。

| # | 问题 | 位置 | 状态 | 处理 |
|---|------|------|------|------|
| 1 | 死状态：`loading` 恒为 `false`，刷新按钮 loading 永不显示 | `src/components/McpLogPanel.vue:22`（定义）+ `:153`（绑定 `:loading="loading"`，全文仅这两处出现） | ✅ | ✅ 已修复 |
| 2 | 拖拽监听器泄漏：`mousemove/mouseup` 挂 `window`，仅在 `up()` 中移除，无卸载兜底 | `src/components/SplitLayout.vue:47-71`（`startDrag`；文件无任何 `onBeforeUnmount`） | ✅ | ✅ 已修复 |
| 3 | 定时器/RAF 未清理：`cursorPosRAF`、`passwordConfirmTimer` 无卸载清理（该 composable 无 `onBeforeUnmount`/`dispose`，全文检索确认） | `src/composables/useTerminalInput.ts:60-61, 63` | ✅ | ✅ 已修复 |
| 4 | 强转访问 xterm 私有 `_core._selectionService._model` | `src/components/TerminalPane.vue:831-853` | ✅ | ⏸️ 保留（无公开 API 替代，已有注释与防御性编码） |
| 5 | 强转访问 xterm 私有 `_core._renderService`（双重 `as unknown as`） | `src/composables/useTerminalInput.ts:141-148` | ✅ | ⏸️ 保留（有 9/17 兜底值，升级时需回归） |
| 6 | 猴补 noVNC 私有 `_fail`（有注释说明为排障有意为之，仍属升级即碎） | `src/components/VncPane.vue:140-148` | ⚠️ | ⏸️ 保留（有意取舍） |
| 7 | 访问 el-tree 内部 `store.nodesMap` | `src/views/SqlConsoleView.vue:682` | ✅ | ⏸️ 保留（el-tree 无公开的编程展开 API） |
| 8 | 访问组件私有 `$el` + 内部类名 `.el-tabs__content` | `src/views/Settings.vue:94` | ✅ | ⏸️ 保留（可选链兜底；替代方案需重构滚动容器） |
| 9 | 错误判定耦合后端中文文案：`includes("认证错误")`、`includes("已取消")`，后者匹配过宽 | `src/utils/error.ts:8-10, 20-22`（有详细注释说明契约，属有意取舍但脆弱） | ⚠️ | ✅ 已缓解（"已取消"已收紧为"二次认证已取消"，已核对后端 client.rs 文案；结构化错误码仍待后端） |
| 10 | 未捕获 rejection：`clearDownloadDir` 中 `await settings.save()` 无 try/catch（同文件 `pickDownloadDir` 有） | `src/components/DownloadDrawer.vue:47-51` | ✅ | ✅ 已修复 |
| 11 | 错误处理不一致：`saveManifestUrl` 无 try/catch，同 store 其它方法均走 `fail()` 收口 | `src/stores/update.ts:64-67` | ✅ | ✅ 已修复 |
| 12 | 校验与文案不符：占位符"至少 6 位"，`submit()` 仅判 `!passphrase.value`，无长度校验 | `src/views/UnlockView.vue:68`（文案）+ `:24`（校验） | ✅ | ✅ 已修复 |
| 13 | 非空断言：`accounts.find(...)!` 选中项不存在时把 `undefined` 传入函数 | `src/views/FileExplorerView.vue:712, 721` | ✅ | ✅ 已修复 |
| 14 | 非空断言：`toggleSkill(domainSkills.find((s) => s.id === cmd)!)` | `src/components/AiPanel.vue:2636` | ✅ | ✅ 已修复 |
| 15 | 非空断言：`activeConversation.value!` 依赖 `ensureConversation()` 隐式保证 | `src/stores/ai.ts:358` | ✅ | ✅ 已修复 |
| 16 | 语法笔误：`);;` 多一个分号 | `src/views/SqlConsoleView.vue:125` | ✅ | ✅ 已修复 |
| 17 | 无效三元：`let acc = isAbs ? "" : "";` 两分支相同 | `src/views/SftpView.vue:259` | ✅ | ✅ 已修复 |
| 18 | 函数名拼写：`listIndexof`（`Indexof` 大小写错误） | `src/components/TerminalSuggestion.vue:188` | ✅ | ✅ 已修复 |
| 19 | 类型标注错误：`previewTimer` 标注 `ReturnType<typeof setInterval>`，实际 `setTimeout`/`clearTimeout` | `src/views/MfaView.vue:186`（定义）、`:244`（setTimeout 赋值） | ✅ | ✅ 已修复 |

---

## P1 规范违反（对照项目前端规范）

### 1. 样式未用 `<style scoped lang="scss">`（规则 2）— ✅ 已确认 → ✅ 已处理（2026-09-11）

**已全部补齐**：39 个文件的 `<style scoped>` 批量改为 `<style scoped lang="scss">`（UTF-8 无 BOM 写回），加上原有 3 个（`DownloadDrawer.vue`、`TerminalSuggestion.vue`、`MonitorPanel.vue`），现 42/42 全部合规。

前置依赖：已安装 `sass-embedded@1.104.0`（devDependency）。

验证：
- grep：纯 `<style scoped>` 清零；`<style scoped lang="scss">` 共 42 处（42 个文件各 1 处）；
- 全量构建 `pnpm build`（`vue-tsc --noEmit && vite build`）通过（exit 0，built in 10.76s）——所有样式块经 sass 编译无报错；
- 事前核对：全仓 `.vue` 无 `#{` 插值、无 `%` 占位符选择器（SCSS 与纯 CSS 的语法冲突点为零），运行时产物不变。

登记例外：`src/components/HelpTip.vue:62` 的非 scoped 全局 `<style>`（`.help-tip-popper`，popper teleport 到 body 所需，已有注释说明）保持纯 CSS 不加 lang。构建输出的 `legacy-js-api` 弃用警告来自 Vite 5.4 调用 Sass 的旧 API 方式，无害（Vite 6 改用 modern API 后自然消失）。

### 2. 显式 import `@element-plus/icons-vue`（规则 6）— ✅ 已确认 → ✅ 已处理（2026-09-11）

图标已在 `src/main.ts:61-64` 全局注册（`app.component(key, component)`），原 23 个文件的显式导入**已全部删除**（现仅剩 main.ts 注册器本身的一条导入）。

处理方式：
- 模板标签用法（`<Refresh />`）依赖全局注册，无需改动；
- 表达式绑定 `:icon="Refresh"` 统一改为字符串形式 `:icon="'Refresh'"`（el-button/el-dropdown-item 的 icon prop 均接受 string，运行时经全局组件解析）；
- 别名导入还原为原始图标名（`Link as LinkIcon` → `'Link'`、`Upload as IconUpload` → `'Upload'`、`Download as IconDownload`/`IconDownload` → `'Download'`、`Delete as ClearIcon` → `'Delete'`）；
- `<component :is>` 三元表达式中的标识符改为字符串字面量（TerminalPane 的 `'Upload'/'Download'`、TransferQueue 的 `'CaretTop'/'CaretBottom'`、Workspace 的 `'ArrowUp'/'ArrowDown'`、SqlConsoleView 的 `'Expand'/'Fold'`、AiPanel 的 `'ArrowDown'/'ArrowUp'`）；
- AiPanel 的 `RUN_MODE_ICONS` 常量值改为字符串（`{ manual: "Lock", whitelist: "Key", auto: "Lightning" } as const`）；
- 顺带清理了导入即死代码的名字（Workspace 的 `Top`/`Bottom`、SqlConsoleView 的 `Edit`/`ArrowDown` 导入未使用等）。

验证：全仓 grep `from "@element-plus/icons-vue"` 仅剩 main.ts:6；`vue-tsc --noEmit` 通过（exit 0）。

原清单（23 个文件，供追溯）：`Workspace.vue`、`FileExplorerView.vue`、`MfaView.vue`、`SqlConsoleView.vue`、`KeyManagerView.vue`、`SftpView.vue`、`RemoteDesktopView.vue`、`Settings.vue`、`McpInstancePanel.vue`、`AboutDialog.vue`、`McpApprovalToast.vue`、`AiPanel.vue`、`DesktopSidebar.vue`、`McpLogPanel.vue`、`TabBar.vue`、`TerminalPane.vue`、`TerminalPaneItem.vue`、`TerminalSuggestion.vue`、`HelpTip.vue`、`SkillManagerDialog.vue`、`MonitorPanel.vue`、`DownloadDrawer.vue`、`TransferQueue.vue`。

**遗留说明**：规范中"已配置自动导入"的前提实际不成立（vite.config.ts 无 unplugin-*，靠 main.ts 全量注册兜底）。本次处理遵循现状（全局注册），`main.ts:63` 的 `component as never` 断言与全量注册的体积开销仍待决策：补 `unplugin-vue-components` + `ElementPlusResolver` 自动导入，或维持现状。

### 3. 类型定义未放 `.d.ts`（规则 3）— ✅ 已确认

- `src/api/types.ts`：interface / enum / const / function 混在一个文件（`AuthType` 枚举、`PROVIDER_DEFAULTS`、`defaultAppShortcuts()` 等运行时值与类型混放），无法整体迁 `.d.ts`，需拆分。
- 业务类型内联在 `.vue`：`FileExplorerView.vue:53-58`（`UnifiedEntry`）、`SftpView.vue:55-66`（同名 `UnifiedEntry` + `DragPayload`，与 FileExplorerView 重复）、`ForwardView.vue:55-66`（`FormState`）、`SqlConsoleView.vue:280-300, 700-717`（`TreeNode`/`TabState`）、`SessionDialog.vue:34-50`、`AiPanel.vue` 多处等。
- `src/views` 下 0 个 `.d.ts`；全仓仅 `types/novnc.d.ts`、`types/zmodem.d.ts`、`env.d.ts`。

> 全仓无该既有约定，建议先统一约定再整体整改，避免只改个别文件。

### 4. 复杂页面未按功能模块拆分（规则 7）— ✅ 已确认（行数为实测修正值）

| 文件 | 实际行数 |
|---|---|
| `src/components/AiPanel.vue` | 4010（修正：初报 4178） |
| `src/views/Settings.vue` | 2456（修正：初报 2609） |
| `src/views/SqlConsoleView.vue` | 2169（修正：初报 2277） |
| `src/views/SftpView.vue` | 1330 |
| `src/components/TerminalPane.vue` | 1286 |
| `src/components/McpInstancePanel.vue` | 1200 |
| `src/views/FileExplorerView.vue` | 1069 |
| `src/stores/ai.ts` | 952 |
| `src/stores/terminals.ts` | 900 |
| `src/components/RdpPane.vue` | 879 |
| `src/components/SessionSidebar.vue` | 852 |
| `src/views/Workspace.vue` | 844 |

另：`src/components/MonitorPanel.vue:232-268` 用第二个普通 `<script lang="ts">` 内联定义 `Sparkline` 子组件（`export default { components: { Sparkline } }`），应独立为 `components/Sparkline.vue`。已确认该写法可运行，但违反规则 1 的 script setup 单一风格与规则 7 的拆分要求。

### 5. 移动端适配残留（规则 8）— ✅ 已确认 → ✅ 已处理（2026-09-11）

`src/views/McpView.vue:337-348` 的 `@media (max-width: 960px)` 窄屏堆叠块**已删除**，全仓现已无 `@media`。

### 6. 未使用 element-plus 组件（规则 5）— ✅ 已确认（是否豁免需决策）

- `src/components/RdpPane.vue:849-861`：工具栏原生 `<button class="rdp-cad">`；
- `src/components/VncPane.vue:242-259`：认证表单原生 `<input>` / `<button>`。

属画布覆盖层场景，若为刻意规避 el 组件请登记为例外。

---

## P2 通用质量问题（关键项已核实，grep 统计项为全量扫描结果）

### 类型安全 — catch/隐式 any 已处理（2026-09-11）
- ~~`catch (e: any)`：Settings.vue 8 处、ForwardView.vue 6 处~~ → 全部改为 `catch (e)` + 新增的 `utils/error.ts#errorMessage(e)` 统一提取。✅
- ~~隐式 any：`SessionSidebar.vue`/`DesktopSidebar.vue` 的 `const treeRef = ref();`~~ → 改为 `ref<TreeInstance | null>(null)`；`toggleExpand`（运行时存在但类型未导出）改用文档化的 `getNode` + `expand/collapse` 等价实现。✅
- `any`：`SqlConsoleView.vue:304`（`ref<any>`）、`:432`（`node: any`）、`ResultTableV2.vue:27-28`（`Column<any>[]`）。（未处理）
- 双重 `as unknown as`：`stores/ai.ts:212-213`（已读源码确认）、`SftpView.vue:835`、`VncPane.vue:160/170/181`。（未处理）
- ~~冗余断言：`DbProfileDialog.vue:100`（`old as DbKind`）~~ → 已随 normalizeKind 收敛删除。✅ `useZmodemTransfer.ts:148`（`(v) => v as T`）未处理。
- `types/zmodem.d.ts:57, 73` 用 `(...args: any[])`，与 `novnc.d.ts` 强类型风格不一致。（未处理）

### 未使用 / 死代码（均已 grep 全仓确认仅定义处一处）— ✅ 全部已处理（2026-09-11）
- `src/api/index.ts`：无人引用的 barrel（绝对/相对/副作用导入全仓零匹配）→ **已删除文件**，逻辑零影响。✅
- `Workspace.vue` / `SqlConsoleView.vue` 的未使用图标导入（`Top`/`Bottom`/`Edit`/`ArrowDown`）→ 已随 P1-2 图标任务删除（本条原记录已过时）。✅
- `DownloadDrawer.vue`：`const props = defineProps(...)` 未使用 → 已改为不接收变量。✅
- `useCodeMirror.ts`：`StateEffect` 未使用 + 重复 import → 已合并为单条。✅
- 未引用导出：`S3CredentialInput`、`AiToolCallEvent`/`AiToolResultEvent`、`looksLikeQuickConnect` → 均已删除。✅

### 复制粘贴 / 重复逻辑（函数名 grep 全量定位）— 数值/传输类已收敛（2026-09-11）
- ~~`percent` / `statusText` / `progressStatus` / `humanSize`：`DownloadDrawer.vue` ≈ `TransferQueue.vue` 逐行重复~~ → 传输助手（`transferPercent`/`transferStatusText`/`transferProgressStatus`）统一到 `stores/transfer.ts` 导出，两组件经 import 别名复用，模板零改动。✅
- ~~`humanSize` 共 4 份~~ → 统一为 `utils/format.ts` 导出（FileExplorerView/SftpView/DownloadDrawer/TransferQueue 四处本地拷贝已删）。✅
- ~~`formatBytes` 2 份（Settings + AboutDialog）~~ → 与 `formatSize` 语义等价（含 TB 单位、0 → "0 B"），两处本地拷贝已删、模板改用 `formatSize`。✅
- ~~`normalizeKind`：`stores/db.ts` 与 `DbProfileDialog.vue` 完全相同~~ → store 侧导出、组件导入复用；顺带删除 `kindMeta(old as DbKind)` 冗余断言。✅
- `SftpView.vue` ≈ `FileExplorerView.vue`：`UnifiedEntry` 接口、`sortEntries`、`makeSorted`、`confirmOverwrite`、面包屑等大段重复。（未处理，属大重构）
- `RdpPane.vue` ≈ `VncPane.vue`：`statusText`、工具栏、Ctrl+Alt+Del、`.*-toolbar`/`.*-cad` CSS 几乎逐行相同。（未处理）
- `SessionSidebar.vue` ≈ `DesktopSidebar.vue`：`filterNode`/`highlightParts`/`allowDrag`/`allowDrop`/`onNodeDrop`/分组增删等大面积重复。（未处理）
- 树构建：`stores/sessions.ts:105-155` 与 `stores/desktops.ts:16-47` 几乎逐行相同。（未处理）
- 右键菜单：`TabBar.vue:125-126`（150/240）与 `TerminalPane.vue:921-922`（160/200）尺寸常量与 `.tab-menu`/`.term-menu` 样式重复且取值不同。（未处理）

### 错误处理不一致
- 全仓 89 处空 `catch`（grep 确认）。多数带注释属合理静默，需补注释的代表：`TerminalPane.vue:66`、`AboutDialog.vue:37`、`main.ts:74`、`router/index.ts:51-55`、`downloadPath.ts:24-28`（把"目录不可用"与"程序异常"等同吞掉）。
- `update.ts:55-61`：`loadInfo` 的 catch 只设 `error` 不改 `status`，与其它方法统一置 `error` 不一致。✅ 已读源码确认

### 魔法数字 / 硬编码 — 前两项已处理（2026-09-11）
- ~~`quickConnect.ts:100`：硬编码端口 22~~ → 改用 `QUICK_PROTOCOLS.ssh.protocol/.defaultPort`。✅
- ~~`stores/ai.ts:196`：防抖 `800` 字面量~~ → 抽为 `PERSIST_DEBOUNCE_MS` 常量。✅
- `downloadPath.ts:36`：`i < 1000` 魔数。（未处理）
- `SplitLayout.vue:29`：`DIVIDER = 6` 与 `:125` CSS `flex: 0 0 6px` 两处硬编码同值。（未处理）
- 硬编码颜色未走 `--el-*`：`McpView.vue:146`（`#fff`）、`AiPanel.vue:3607/4115`（`#fff`）、`MonitorPanel.vue:220-223`（`#f56c6c/#e6a23c/#409eff`）、`TerminalPaneItem.vue:194`（`#fff`）、`DesktopSidebar.vue:461`（`#67c23a`）、`TitleBar.vue:258-263`（窗口关闭红，Windows 惯例可保留）。（未处理）

### v-for 用 index 作 key — SftpView 数据行已处理（2026-09-11）
~~`SftpView.vue` 表格行 `'l-' + idx` / `'r-' + idx`~~ → 改为 `'l-' + e.name` / `'r-' + e.name`（与 FileExplorerView 一致），`idx` 变量一并移除。✅

其余保留（字符高亮/静态枚举等 index 属可接受场景或待统一处理）：`FileExplorerView.vue:846`、`SftpView.vue:1045`（面包屑）、`AiPanel.vue`、`TerminalSuggestion.vue`、`DesktopSidebar.vue:308`、`SessionSidebar.vue:588`、`McpLogPanel.vue:179`、`SshAuthPrompt.vue:195`、`SqlConsoleView.vue:1660/1688`。

### 其它一致性 / 可读性
- `TerminalSuggestion.vue`：`<template>`（第 1 行）在 `<script setup>`（第 67 行）之前，与其余 42 个组件顺序相反。✅
- `TerminalPane.vue:1064-1082`：`refit()` 及注释整体多缩进两格。✅ 已读源码确认
- `api/remote_desktop.ts`：snake_case 命名与同目录 camelCase（`desktopControl.ts`、`fileBackend.ts`）不一致。
- `AiPanel.vue:369-378`：`terminalOptions_dup` 下划线命名，且依赖过滤后数组下标生成序号，脆弱。
- 领域错位：`api/db.ts:144-177` 的 `aiExecuteTool`/`aiCancelTool`/`aiAskUserRespond`/`aiAddToWhitelist` 属 AI 域，调用方（`AiPanel.vue`、`stores/ai.ts`）需从 `@/api/db` 导入。✅ grep 确认
- `FileAccountDialog.vue:193`：`@update:model-value="close"`（无论 true/false 都发 close），其它对话框用 `emit('update:visible', $event)` 直传。✅ 已读源码确认
- 模板内重复重计算：`AiPanel.vue:2289/2294/2295` 三次调 `askQuestions(item)`；`McpApprovalToast.vue` 模板 5 次调 `isDangerous()`（含正则）。
- 渲染期写响应式状态：`AiPanel.vue:1576-1578` 模板调用的 `askSel()` 内执行 `??=` 写 ref。
- `ResultTableV2.vue:53-54` 的 `34/36` 与样式 `:81` 三处硬编码同一行高。
- `useSuggestions.ts:147`：`item.id == null` 宽松相等。
- `styles/main.css`：`:root` 出现两次（6/36 行）、`.dark` 两次（28/45 行），令牌块分散易失同步。
- 内联样式：`UnlockView.vue:80`（`style="width: 100%"`）、`MfaView.vue:412/431`。

---

## 核查结论汇总

| 类别 | 数量/范围 | 核查状态 |
|---|---|---|
| P0 真实缺陷/风险 | 19 项 | 17 项完全确认；2 项（VncPane `_fail` 猴补、error.ts 文案匹配）存在但有注释说明属有意取舍 |
| 样式缺 `lang="scss"` | 40 个文件 | ✅ 已全部补齐（39 改 + 3 原有 = 42/42），已装 sass-embedded，build 通过 |
| 显式导入图标 | 23 个文件 | ✅ grep 全量确认；"自动导入"前提实际不成立，需先决策 |
| 超大文件待拆分 | 12 个文件 ≥844 行 | ✅ 实测行数（含 5 处修正） |
| 类型未外置 `.d.ts` | 全仓性 | ✅ 确认；建议先定约定 |
| 复制粘贴重复 | 9 组 | ✅ 函数名 grep 全量定位（新增 formatBytes 在 AboutDialog 的重复、humanSize 共 4 份） |
| `catch (e: any)` / `any` | 14+3 处 | ✅ grep 全量确认 |
| 死代码/未使用 | 8 处 | ✅ 全部 grep 确认仅定义无引用 |
| v-for index 作 key | 13 处 | ✅ grep 全量确认 |

### 与初版报告的差异（修正项）
1. 大文件行数按 `Get-Content | Measure-Object` 实测修正（AiPanel 4010 而非 4178 等，见 P1-4 表）。
2. **新发现**：`formatBytes` 重复不止 Settings.vue 一处，`AboutDialog.vue:17` 还有一份。
3. **新发现**：`humanSize` 共 4 份（初报只提 Sftp/FileExplorer 一组）。
4. `error.ts` 与 `VncPane._fail` 补充说明：源码均有详尽注释说明取舍原因，降级为"有意但脆弱"，修复优先级可下调。

---

## P0 处理记录（2026-09-11）

类型检查：`npx vue-tsc --noEmit` 通过（exit 0，无错误）。

### 已修复（13 项）

| # | 文件 | 修改内容 |
|---|------|---------|
| 1 | `src/components/McpLogPanel.vue` | 新增 `refresh()` 包装（置位 `loading` → `fetchLog` → finally 复位），刷新按钮改绑 `refresh`；轮询路径不置位，避免指示灯每 1.5s 闪烁 |
| 2 | `src/components/SplitLayout.vue` | `startDrag` 记录 `stopDrag` 清理函数，新增 `onBeforeUnmount(() => stopDrag?.())`——拖拽中组件销毁时移除 window 监听并复位 body 光标/禁选样式 |
| 3 | `src/composables/useTerminalInput.ts` | composable 内注册 `onBeforeUnmount`：清理 `passwordConfirmTimer` 与 `cursorPosRAF` |
| 10 | `src/components/DownloadDrawer.vue` | `clearDownloadDir` 补 try/catch + `ElMessage.error`，与 `pickDownloadDir` 一致 |
| 11 | `src/stores/update.ts` | `saveManifestUrl` 补 try/catch + `fail()` 收口，与 check/download/install 一致 |
| 12 | `src/views/UnlockView.vue` | 创建路径补"至少 6 位"校验（解锁不强制，避免锁死旧短密码）；占位符改为动态（解锁态显示"请输入主密码"） |
| 13 | `src/views/FileExplorerView.vue` | 模板 `find(...)!` 断言改为脚本方法 `editSelectedAccount()` / `removeSelectedAccount()`，判空守卫 |
| 14 | `src/components/AiPanel.vue` | 技能下拉 `@command` 内联三元 + `find(...)!` 抽为 `onSkillCommand(cmd)` 方法，判空守卫 |
| 15 | `src/stores/ai.ts` | `activeConversation.value!` 改为判空 return（防御 store 状态被外部清空） |
| 16 | `src/views/SqlConsoleView.vue` | `);;` → `);` |
| 17 | `src/views/SftpView.vue` | `let acc = isAbs ? "" : "";` → `let acc = "";`（两分支相同） |
| 18 | `src/components/TerminalSuggestion.vue` | `listIndexof` → `listIndexOf`（定义 + 调用点共 2 处） |
| 19 | `src/views/MfaView.vue` | `previewTimer` 类型 `ReturnType<typeof setInterval>` → `ReturnType<typeof setTimeout>` |

### 评估后保留（6 项，均有理由）

| # | 位置 | 保留理由 |
|---|------|---------|
| 4 | `TerminalPane.vue:831-853` | xterm v6 无公开 API 平移选择锚点（`shiftSelectionAnchor` 依赖 `_selectionService._model`）；已有详尽注释 + 可选链防御，升级 xterm 时需回归 |
| 5 | `useTerminalInput.ts:141-148` | 单元格尺寸取自 `_renderService.dimensions`，无公开 getter；已有 9/17 兜底值，取不到时功能退化为默认字号定位 |
| 6 | `VncPane.vue:140-148` | 猴补 `_fail` 为排障有意为之（捕获 noVNC 不外露的失败原因），注释已说明；noVNC 升级时需回归 |
| 7 | `SqlConsoleView.vue:682` | Element Plus el-tree 无公开的按 key 编程展开 API，`nodesMap` 是社区通用做法；可选链兜底 |
| 8 | `Settings.vue:94` | 滚动容器是 EP 内部 `.el-tabs__content`；替代方案需自建滚动层重构布局，风险大于收益；可选链兜底 |
| 9 | `utils/error.ts` | 根治需后端把 `AppError` 序列化为结构化错误码（Rust 端改动 + 前端判定同步），属跨端改造；现有注释已说明文案契约，短期可收紧 `"已取消"` 为更长的锚点串（如"二次认证已取消"）作为低成本缓解 |

---

## 第一档处理记录（2026-09-11）

范围：死代码清理、重复函数收敛、catch(e:any)/隐式 any、两处魔法值、error.ts 收紧、McpView @media、SftpView 表格 key。
验证：`npx vue-tsc --noEmit` 通过（exit 0，无错误）。

### 修改明细

| 类别 | 文件 | 修改 |
|---|---|---|
| 死代码 | `src/api/index.ts` | **整文件删除**（绝对/相对/副作用导入全仓零匹配，纯 re-export 桶，逻辑零影响） |
| 死代码 | `src/composables/useCodeMirror.ts` | 删未使用的 `StateEffect`，合并 `@codemirror/state` 重复 import |
| 死代码 | `src/api/fileBackend.ts` | 删未引用导出 `S3CredentialInput` |
| 死代码 | `src/api/types.ts` | 删未引用导出 `AiToolCallEvent` / `AiToolResultEvent` |
| 死代码 | `src/utils/quickConnect.ts` | 删未引用导出 `looksLikeQuickConnect` |
| 死代码 | `src/components/DownloadDrawer.vue` | `const props = defineProps(...)` → `defineProps<...>()`（props 未使用） |
| 收敛 | `src/utils/format.ts` | 新增导出 `humanSize`（0/负值 → "-"，包装 formatSize） |
| 收敛 | `src/stores/transfer.ts` | 新增导出 `transferPercent` / `transferStatusText` / `transferProgressStatus` |
| 收敛 | `src/components/DownloadDrawer.vue`、`src/components/TransferQueue.vue` | 删除两组件各自的 `percent`/`statusText`/`progressStatus`/`humanSize` 拷贝，改 import（别名保留模板短名，模板零改动） |
| 收敛 | `FileExplorerView.vue`、`SftpView.vue` | 删除各自 `humanSize` 拷贝，改 import |
| 收敛 | `Settings.vue`、`AboutDialog.vue` | 删除 `formatBytes` 拷贝（与 `formatSize` 语义等价），模板改用 `formatSize` |
| 收敛 | `src/stores/db.ts` + `DbProfileDialog.vue` | `normalizeKind` 由 store 导出、组件复用；顺带删 `kindMeta(old as DbKind)` 冗余断言 |
| 类型 | `src/utils/error.ts` | 新增 `errorMessage(e: unknown)` 统一错误消息提取 |
| 类型 | `Settings.vue` 8 处、`ForwardView.vue` 6 处 | `catch (e: any)` + `e?.message ?? String(e)` → `catch (e)` + `errorMessage(e)` |
| 类型 | `SessionSidebar.vue`、`DesktopSidebar.vue` | `treeRef = ref()` → `ref<TreeInstance \| null>(null)`；`toggleExpand`（类型未导出）改用 `getNode` + `expand/collapse` 等价实现 |
| 魔法值 | `src/utils/quickConnect.ts` | 裸串默认端口 `build("ssh", hostPart, 22)` → `QUICK_PROTOCOLS.ssh.protocol/.defaultPort` |
| 魔法值 | `src/stores/ai.ts` | 持久化防抖 `800` → `PERSIST_DEBOUNCE_MS` 常量 |
| 缓解 | `src/utils/error.ts` | `isAuthCancelled` 的 `includes("已取消")` 收紧为 `includes("二次认证已取消")`（已核对后端 `ssh/client.rs:1284` 文案） |
| 规则8 | `src/views/McpView.vue` | 删除 `@media (max-width: 960px)` 移动端适配残留块 |
| key | `src/views/SftpView.vue` | 文件行 v-for key `'l-' + idx` / `'r-' + idx` → `e.name`，未用的 `idx` 一并移除 |
