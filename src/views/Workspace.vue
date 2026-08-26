<script setup lang="ts">
import { computed, nextTick, onActivated, onBeforeUnmount, onDeactivated, reactive, ref } from "vue";
import { useTerminalsStore, type TerminalTab } from "@/stores/terminals";
import { useSettingsStore } from "@/stores/settings";
import TerminalPane from "@/components/TerminalPane.vue";
import AiPanel from "@/components/AiPanel.vue";
import TabBar, { type TabBarItem } from "@/components/TabBar.vue";
import { Delete, Top, Bottom, ZoomIn, ZoomOut, Refresh, Plus, ArrowDown, ArrowUp, Monitor, Key } from "@element-plus/icons-vue";
import { ElMessage } from "element-plus";
import { eventToCombo, isModifierOnly } from "@/utils/shortcut";
import { isAuthError } from "@/utils/error";
import type { ShortcutCommand } from "@/api/types";

// KeepAlive 按 name 匹配缓存本组件（保留终端助手面板状态）。
defineOptions({ name: "Workspace" });

const terminals = useTerminalsStore();
const settings = useSettingsStore();

const active = computed(() =>
  terminals.tabs.find((t) => t.instanceId === terminals.activeId)
);

// 各 tab 的 TerminalPane 引用（按稳定 tab.id 索引——instanceId 在重连时会
// 更换，用它做 key 会在重连瞬间误删新引用）。
// reactive Map 保证增删触发 activePaneRef 重新求值。
const paneRefs = reactive(new Map<string, InstanceType<typeof TerminalPane>>());

/** 当前活动 tab 的 pane 引用（工具动作作用于它；vnc tab 时为 undefined）。 */
const activePaneRef = computed(() => {
  const id = active.value?.id;
  return id ? paneRefs.get(id) : undefined;
});

/** TerminalPane 挂载/卸载时的 ref 回调：登记或移除 pane 引用。 */
function onPaneRef(tab: TerminalTab, el: unknown) {
  if (el) {
    paneRefs.set(tab.id, el as InstanceType<typeof TerminalPane>);
  } else {
    paneRefs.delete(tab.id);
  }
}

// --- Tab 栏（共享 TabBar 组件） ---------------------------------------------

/** 映射为 TabBar 的数据抽象。 */
const tabItems = computed<TabBarItem[]>(() =>
  terminals.tabs.map((t) => ({
    key: t.instanceId || t.session.id,
    title: t.session.name,
    connecting: t.connecting,
    disconnected: t.disconnected,
  })),
);

/**
 * TabBar 右键菜单命令（作用于对应 tab）。
 *
 * key 为 TabBar 的复合键（instanceId || session.id）：连接中的占位 tab 只有
 * 复合键可用，close 系列命令照常生效；reconnect 仍要求已建立实例。
 */
function onTabMenuCommand(cmd: string, key: string) {
  const t = terminals.tabs.find((x) => (x.instanceId || x.session.id) === key);
  if (!t) return;
  switch (cmd) {
    case "close":
      void terminals.close(key);
      break;
    case "closeOthers":
      for (const x of [...terminals.tabs]) {
        if (x !== t) {
          void terminals.close(x.instanceId || x.session.id);
        }
      }
      break;
    case "closeAll":
      for (const x of [...terminals.tabs]) {
        void terminals.close(x.instanceId || x.session.id);
      }
      break;
    case "reconnect":
      if (t.instanceId) void terminals.reconnect(t.instanceId);
      break;
  }
}

// 终端被通知连接断开（由 TerminalPane emit "closed"）：标记断开并顺手清理后端
// 已死的 session 实例（避免 registry 泄漏），重连时会自行重建。
function onTerminalClosed(instanceId: string) {
  void terminals.handleTerminalClosed(instanceId);
}

// 工具栏动作。
function clearActive() {
  activePaneRef.value?.clear();
  activePaneRef.value?.focus();
}
async function reconnectActive() {
  if (!active.value?.instanceId) return;
  try {
    await terminals.reconnect(active.value.instanceId);
  } catch (e) {
    /* 错误已存进 tab.error */
  }
}
function zoom(delta: number) {
  // 作用于活动面板的字号覆盖（每 tab 独立、不写全局设置、不持久化）。
  activePaneRef.value?.zoomFont(delta);
}

// --- 快捷命令栏 ---------------------------------------------------------

/** 当前激活的分组标签（"__all__" 表示全部）。 */
const activeGroup = ref("__all__");

/** 是否存在多个分组（决定是否显示标签行）。 */
const hasGroups = computed(() => settings.shortcutGroups.length > 0);

/** 当前标签下可见的快捷命令。 */
const visibleShortcuts = computed(() => {
  if (activeGroup.value === "__all__") return settings.shortcuts;
  return settings.shortcuts.filter((sc) => sc.group === activeGroup.value);
});

/** 把命令文本中的占位符 {host}/{user}/{port} 按当前活动会话替换。 */
function resolveCommand(cmd: string): string {
  const s = active.value?.session;
  // 本地终端无会话配置（合成占位 Session），跳过占位符替换。
  if (!s || s.protocol === "local") return cmd;
  return cmd
    .replaceAll("{host}", s.host)
    .replaceAll("{user}", s.username)
    .replaceAll("{port}", String(s.port));
}

/** 打开本地终端标签页（默认 shell 在设置中配置）。 */
async function openLocalTerminal() {
  try {
    await terminals.openLocal();
  } catch (e) {
    // "连接已取消"（连接期间关闭 tab）静默——不是失败。
    if (String(e).includes("连接已取消")) return;
    ElMessage.error(String(e));
  }
}

/** 向活动终端发送一条快捷命令。 */
function runShortcut(sc: ShortcutCommand) {
  if (!active.value?.instanceId) return;
  activePaneRef.value?.sendCommand(resolveCommand(sc.command));
}

// --- 快捷命令栏：展开/折叠（状态持久化） --------------------------------

/** 展开状态来自设置（持久化，重启后保持）。 */
const scExpanded = computed(() => settings.shortcutBarExpanded);

function toggleScExpand() {
  settings.shortcutBarExpanded = !settings.shortcutBarExpanded;
  void settings.save();
}

// --- 快捷命令栏：在终端直接添加快捷命令 ----------------------------------

/** 添加命令弹窗（位于分组标签行右侧的 + 按钮）。 */
const addVisible = ref(false);
const newScLabel = ref("");
const newScCommand = ref("");
const newScGroup = ref("");

/** 尽力从终端当前行剥离 shell 提示符，得到用户输入的命令。 */
function stripPrompt(line: string): string {
  const s = line.replace(/\s+$/, "");
  // 提示符常见形态：user@host:~$ / root@host:/opt# / [user@host ~]$ / C:\> / ❯ 等。
  // 特征：提示符字符（$ # % > ❯ λ ➜）后跟空白，且其前含 @、:、]、~、/ 或形如盘符。
  const matches = [...s.matchAll(/[#$%>❯λ➜]\s+/g)];
  for (let i = matches.length - 1; i >= 0; i--) {
    const m = matches[i];
    const before = s.slice(0, m.index);
    if (/[@:\]~/]/.test(before) || /^[A-Za-z]:\\?$/.test(before)) {
      return s.slice(m.index! + m[0].length).trimStart();
    }
  }
  return s;
}

/** 点击 +：预填当前终端输入的命令，打开添加弹窗。 */
function openAddShortcut() {
  const line = activePaneRef.value?.getCurrentLine() ?? "";
  newScCommand.value = stripPrompt(line);
  newScLabel.value = newScCommand.value.split(/\s+/)[0] || "新命令";
  newScGroup.value = activeGroup.value === "__all__" ? "" : activeGroup.value;
  addVisible.value = true;
}

/** 保存新快捷命令（写入设置持久化）。 */
async function saveNewShortcut() {
  const command = newScCommand.value.trim();
  const label = newScLabel.value.trim();
  if (!label && !command) {
    ElMessage.warning("名称和命令不能同时为空");
    return;
  }
  const finalLabel = label || command || "新命令";
  const group = newScGroup.value.trim();
  if (group && !settings.shortcutGroups.includes(group)) {
    settings.addShortcutGroup(group);
  }
  const id = settings.addShortcut(group || undefined);
  settings.updateShortcut(id, { label: finalLabel, command, shortcut: null });
  addVisible.value = false;
  await settings.save();
  ElMessage.success(`已添加快捷命令「${finalLabel}」`);
}

/** 用于全局快捷键监听（自定义快捷命令）。 */
function onGlobalKeydown(e: KeyboardEvent) {
  // 长按连发（e.repeat）只响应首次按键，避免自定义命令被连续执行。
  if (e.repeat) return;
  // Ctrl+W 关闭当前标签：放在可编辑元素排除**之前**——焦点在终端画布内时
  // 事件源是 xterm 的隐藏 textarea，若先走排除逻辑就永远拦不到。捕获阶段
  // 拦截 + stopPropagation：xterm 收不到就不会把 Ctrl+W（\x17 删词）发往
  // 远端，关标签不会连带删掉远端一个词（shell 删词可用 Alt+Backspace）。
  if ((e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === "w") {
    if (!active.value?.instanceId) return;
    e.preventDefault();
    e.stopPropagation();
    void terminals.close(active.value.instanceId);
    return;
  }
  // 仅当聚焦在 body 或非可编辑元素时才响应快捷键，避免与输入框冲突。
  const target = e.target as HTMLElement | null;
  if (target) {
    const tag = target.tagName;
    if (tag === "INPUT" || tag === "TEXTAREA" || target.isContentEditable) return;
  }
  const combo = eventToCombo(e);
  if (!combo || isModifierOnly(combo)) return;
  const hit = settings.shortcuts.find((s) => s.shortcut && s.shortcut === combo);
  if (hit) {
    e.preventDefault();
    runShortcut(hit);
    return;
  }
  // 内置标签快捷键（用户自定义命令未占用时生效，与桌面页一致）：
  // Ctrl+1~9 切换标签。焦点在终端画布内时让给远端（终端应用的 Ctrl+组合
  // 冲突更少，且切换有会话侧栏可用）。
  if ((e.ctrlKey || e.metaKey) && !e.altKey) {
    if (!target?.closest(".xterm-wrap")) {
      if (/^[1-9]$/.test(e.key)) {
        const idx = Number(e.key) - 1;
        const tab = terminals.tabs[idx];
        if (tab?.instanceId) {
          e.preventDefault();
          terminals.setActive(tab.instanceId);
        }
      }
    }
  }
}

// 本组件被 KeepAlive 缓存（MainLayout），切到其他页面时不会卸载——若在
// onMounted 里注册全局监听，切走后自定义命令快捷键仍会在后台终端执行命令。
// 改为随页面激活/停用注册/注销（tab 右键菜单的关闭监听由 TabBar 组件自理）。
// 注册用捕获阶段：Ctrl+W 要抢在 xterm 的 textarea 处理之前拦截（否则删词
// 已发往远端，关标签变成"关标签 + 删远端一个词"）。
onActivated(() => {
  window.addEventListener("keydown", onGlobalKeydown, true);
});
onDeactivated(() => {
  window.removeEventListener("keydown", onGlobalKeydown, true);
});
// 兜底：组件真正销毁（如应用退出）时确保清理。
onBeforeUnmount(() => {
  window.removeEventListener("keydown", onGlobalKeydown, true);
});
</script>

<template>
  <div class="workspace">
    <div class="tab-bar">
      <TabBar
        :tabs="tabItems"
        :active-key="terminals.activeId"
        empty-hint="从左侧会话树双击连接"
        @select="
          (k) => {
            terminals.setActive(k);
            // 切 tab 后把焦点交给新活动终端：否则焦点留在隐藏的旧 pane 的
            // textarea 上，新终端光标不显示（要再点一下才有）。
            void nextTick(() => activePaneRef?.focus());
            // 二次校验列宽：恢复可见瞬间的首次 fit 可能取到半布局的失效
            // 测量（cols 偏小且停留），与 PTY 列宽不一致会让 shell 局部重绘
            // 错位出残影。布局稳定后由 pane 重新对齐一次。
            void nextTick(() => activePaneRef?.refit());
          }
        "
        @close="(k) => void terminals.close(k)"
        @move="(from, to, before) => terminals.moveTab(from, to, before)"
        @command="onTabMenuCommand"
      />
      <!-- 本地终端：常驻按钮，无论有无会话都可用 -->
      <button class="local-term-btn" title="打开本地终端（默认 Shell 可在设置中配置）" @click="openLocalTerminal">
        <el-icon><Monitor /></el-icon>
        <span>本地终端</span>
      </button>
      <!-- 终端工具栏 -->
      <div v-if="active" class="term-toolbar">
        <el-tooltip content="清屏" placement="bottom">
          <el-button class="tool-btn" link @click="clearActive"><el-icon><Delete /></el-icon></el-button>
        </el-tooltip>
        <el-tooltip content="字体增大" placement="bottom">
          <el-button class="tool-btn" link @click="zoom(1)"><el-icon><ZoomIn /></el-icon></el-button>
        </el-tooltip>
        <el-tooltip content="字体减小" placement="bottom">
          <el-button class="tool-btn" link @click="zoom(-1)"><el-icon><ZoomOut /></el-icon></el-button>
        </el-tooltip>
        <el-tooltip content="重连" placement="bottom" v-if="active.disconnected">
          <el-button class="tool-btn" link :loading="active.reconnecting" @click="reconnectActive">
            <el-icon><Refresh /></el-icon>
          </el-button>
        </el-tooltip>
      </div>
    </div>
    <div class="workspace-body">
      <div class="panes">
        <div
          v-for="tab in terminals.tabs"
          :key="tab.id"
          v-show="tab.instanceId === terminals.activeId"
          class="pane"
        >
          <template v-if="tab.instanceId">
            <TerminalPane
              :ref="(el: any) => onPaneRef(tab, el)"
              :instance-id="tab.instanceId"
              @closed="onTerminalClosed(tab.instanceId)"
            />
            <!-- 断开重连覆盖层 -->
            <div v-if="tab.disconnected" class="reconnect-overlay">
              <div class="reconnect-card">
                <div class="reconnect-title">连接已断开</div>
                <!-- 最近一次重连失败的原因（reconnect 失败写入 tab.error；pane
                     已挂载时错误分支显示不到，不在 overlay 上展示用户就无从
                     知道为什么连不上）。 -->
                <div v-if="tab.error" class="reconnect-error" :title="tab.error">
                  {{ tab.error }}
                </div>
                <el-button
                  type="primary"
                  :icon="Refresh"
                  :loading="tab.reconnecting"
                  @click="terminals.reconnect(tab.instanceId)"
                >
                  重新连接
                </el-button>
              </div>
            </div>
          </template>
          <div v-else-if="tab.connecting" class="pane-status">连接中…</div>
          <div v-else-if="tab.error" class="pane-status error">
            <span class="pane-error-text">连接失败：{{ tab.error }}</span>
            <!-- 认证失败：提供手动输入密码/口令码重试的入口（弹窗） -->
            <el-button
              v-if="isAuthError(tab.error)"
              size="small"
              :icon="Key"
              @click="terminals.openManualAuth(tab)"
            >
              手动认证
            </el-button>
          </div>
        </div>
        <div v-if="!active" class="workspace-empty">
          还没有打开任何终端。请从左侧会话树连接一台服务器。
        </div>
      </div>
      <!-- 终端助手面板：仅在终端页显示，与 DB 助手完全隔离 -->
      <AiPanel domain="ssh" />
    </div>
    <!-- 终端底部快捷命令栏 -->
    <div v-if="active" class="shortcut-bar">
      <!-- 分组标签行：分组在左，右侧为「添加快捷命令」按钮 -->
      <div class="sc-tabs">
        <template v-if="hasGroups">
          <button
            class="sc-tab"
            :class="{ active: activeGroup === '__all__' }"
            @click="activeGroup = '__all__'"
          >
            全部
          </button>
          <button
            v-for="g in settings.shortcutGroups"
            :key="g"
            class="sc-tab"
            :class="{ active: activeGroup === g }"
            @click="activeGroup = g"
          >
            {{ g }}
          </button>
        </template>
        <span class="sc-tabs-spacer" />
        <!-- 添加命令弹窗：预填当前终端输入的命令 -->
        <el-popover v-model:visible="addVisible" placement="top-start" :width="320" trigger="click">
          <div class="sc-add-form">
            <div class="sc-add-field">
              <span class="sc-add-label">命令</span>
              <el-input
                v-model="newScCommand"
                size="small"
                placeholder="要发送的命令"
                spellcheck="false"
              />
            </div>
            <div class="sc-add-field">
              <span class="sc-add-label">名称</span>
              <el-input
                v-model="newScLabel"
                size="small"
                placeholder="按钮显示名称（默认取命令首词）"
              />
            </div>
            <div class="sc-add-field">
              <span class="sc-add-label">分组</span>
              <el-select
                v-model="newScGroup"
                size="small"
                filterable
                allow-create
                default-first-option
                clearable
                placeholder="未分组"
                style="flex: 1"
              >
                <el-option v-for="g in settings.shortcutGroups" :key="g" :label="g" :value="g" />
              </el-select>
            </div>
            <div class="sc-add-actions">
              <el-button size="small" @click="addVisible = false">取消</el-button>
              <el-button size="small" type="primary" @click="saveNewShortcut">添加</el-button>
            </div>
          </div>
          <template #reference>
            <button class="sc-add" title="添加快捷命令">
              <el-icon><Plus /></el-icon>
            </button>
          </template>
        </el-popover>
      </div>
      <!-- 命令按钮行：折叠时超出宽度隐藏；展开时自动往下换行显示全部 -->
      <div class="sc-row">
        <div class="sc-buttons" :class="{ expanded: scExpanded }">
          <el-tooltip
            v-for="sc in visibleShortcuts"
            :key="sc.id"
            :content="sc.shortcut ? `${sc.command}  (${sc.shortcut})` : sc.command"
            placement="top"
            :show-after="400"
          >
            <button class="sc-btn" @click="runShortcut(sc)">
              <span class="sc-label">{{ sc.label }}</span>
              <span v-if="sc.shortcut" class="sc-key">{{ sc.shortcut }}</span>
            </button>
          </el-tooltip>
          <span v-if="visibleShortcuts.length === 0" class="sc-empty">
            {{ settings.shortcuts.length === 0 ? "暂无快捷命令，点击上方 + 添加" : "该分组暂无命令" }}
          </span>
        </div>
        <button
          v-if="settings.shortcuts.length > 0"
          class="sc-toggle"
          :title="scExpanded ? '收起（单行显示）' : '展开（多行换行）'"
          @click="toggleScExpand"
        >
          <el-icon><component :is="scExpanded ? ArrowUp : ArrowDown" /></el-icon>
          <span>{{ scExpanded ? "收起" : "展开" }}</span>
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.workspace {
  display: flex;
  flex-direction: column;
  width: 100%;
  height: 100%;
  overflow: hidden;
}
.tab-bar {
  display: flex;
  align-items: center;
  height: 38px;
  background: var(--el-bg-color-overlay);
  border-bottom: 1px solid var(--el-border-color-lighter);
  padding: 0 8px;
  flex-shrink: 0;
}
/* 本地终端按钮：tab 栏右侧常驻（描边按钮，悬停主色化）。 */
.local-term-btn {
  display: flex;
  align-items: center;
  gap: 5px;
  height: 26px;
  padding: 0 12px;
  margin-left: 6px;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  background: var(--el-fill-color-blank);
  color: var(--el-text-color-regular);
  font-size: 12px;
  cursor: pointer;
  flex-shrink: 0;
  white-space: nowrap;
  transition: border-color 0.15s ease, color 0.15s ease, background-color 0.15s ease;
}
.local-term-btn:hover {
  border-color: var(--el-color-primary-light-5);
  color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
}
.term-toolbar {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 0 6px 0 8px;
  border-left: 1px solid var(--el-border-color-lighter);
  margin-left: 4px;
  flex-shrink: 0;
}
.tool-btn {
  padding: 5px;
  border-radius: 6px;
  color: var(--el-text-color-secondary);
  transition: background-color 0.15s ease, color 0.15s ease;
}
.tool-btn:hover {
  color: var(--el-color-primary);
  background: var(--el-fill-color-light);
}
.workspace-body {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: row;
  overflow: hidden;
}
.panes {
  flex: 1;
  min-width: 0;
  position: relative;
  background: var(--el-bg-color-page);
}
.pane {
  position: absolute;
  inset: 0;
}
/* 断开重连覆盖层 */
.reconnect-overlay {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgba(0, 0, 0, 0.35);
  backdrop-filter: blur(3px);
  z-index: 20;
  /* 遮罩放行鼠标事件：断开后终端输出仍可选中/复制/滚动（排障刚需——
     拿不到最后几行日志就没法定位问题）；只有中央卡片拦截点击。 */
  pointer-events: none;
}
.reconnect-card {
  pointer-events: auto;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 12px;
  padding: 24px 32px;
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 12px;
  box-shadow: 0 12px 32px rgba(0, 0, 0, 0.22);
}
.reconnect-error {
  max-width: 320px;
  margin-bottom: 2px;
  font-size: 12px;
  color: var(--el-color-danger);
  text-align: center;
  line-height: 1.5;
  word-break: break-all;
  display: -webkit-box;
  -webkit-line-clamp: 3;
  -webkit-box-orient: vertical;
  overflow: hidden;
}
.reconnect-title {
  font-size: 14px;
  font-weight: 600;
  letter-spacing: 0.5px;
  color: var(--el-text-color-primary);
}
.pane-status {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  height: 100%;
  font-size: 13px;
  color: var(--el-text-color-secondary);
}
/* 连接中：主色旋转指示环 */
.pane-status:not(.error)::before {
  content: "";
  width: 14px;
  height: 14px;
  border-radius: 50%;
  border: 2px solid var(--el-border-color-light);
  border-top-color: var(--el-color-primary);
  animation: pane-spin 0.8s linear infinite;
}
@keyframes pane-spin {
  to {
    transform: rotate(360deg);
  }
}
.pane-status.error {
  color: var(--el-color-danger);
  gap: 12px;
  flex-wrap: wrap;
  padding: 0 32px;
  text-align: center;
}

.pane-error-text {
  word-break: break-all;
}

/* --- 终端底部快捷命令栏 --- */
.shortcut-bar {
  display: flex;
  flex-direction: column;
  background: var(--el-bg-color-overlay);
  border-top: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
}
.sc-tabs {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 4px 10px 0;
  border-bottom: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
}
.sc-tabs-spacer {
  flex: 1;
}
/* 「添加快捷命令」按钮（分组右侧） */
.sc-add {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 22px;
  height: 22px;
  margin-left: 6px;
  border: 1px dashed var(--el-border-color);
  border-radius: 6px;
  background: transparent;
  color: var(--el-text-color-secondary);
  font-size: 14px;
  line-height: 1;
  cursor: pointer;
  transition: all 0.15s;
}
.sc-add:hover {
  border-color: var(--el-color-primary);
  color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
}
.sc-tab {
  padding: 3px 10px;
  border: none;
  border-bottom: 2px solid transparent;
  background: transparent;
  color: var(--el-text-color-secondary);
  font-size: 12px;
  cursor: pointer;
  white-space: nowrap;
  transition: all 0.15s;
}
.sc-tab:hover {
  color: var(--el-color-primary);
}
.sc-tab.active {
  color: var(--el-color-primary);
  border-bottom-color: var(--el-color-primary);
  font-weight: 500;
}
/* 命令按钮行：按钮区（可折叠/展开）+ 展开收起按钮 */
.sc-row {
  display: flex;
  align-items: center;
  min-width: 0;
}
.sc-buttons {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 10px;
  flex: 1;
  min-width: 0;
  /* 折叠：单行，超出宽度直接隐藏（不出现滚动条） */
  flex-wrap: nowrap;
  overflow: hidden;
}
.sc-buttons.expanded {
  /* 展开：自动往下换行显示全部命令 */
  flex-wrap: wrap;
  overflow: visible;
}
.sc-toggle {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  margin: 0 8px 0 2px;
  padding: 3px 8px;
  border: none;
  border-radius: 4px;
  background: transparent;
  color: var(--el-text-color-secondary);
  font-size: 12px;
  cursor: pointer;
  white-space: nowrap;
  flex-shrink: 0;
  transition: all 0.15s;
}
.sc-toggle:hover {
  color: var(--el-color-primary);
  background: var(--el-fill-color);
}
.sc-empty {
  font-size: 12px;
  color: var(--el-text-color-placeholder);
}
.sc-btn {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 3px 10px;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  background: var(--el-fill-color-blank);
  color: var(--el-text-color-regular);
  font-size: 12px;
  cursor: pointer;
  white-space: nowrap;
  transition: all 0.15s;
}
.sc-btn:hover {
  border-color: var(--el-color-primary-light-5);
  color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
}
.sc-btn:active {
  transform: translateY(1px);
}
.sc-label {
  font-family: var(--app-font-mono);
}
.sc-key {
  font-size: 10px;
  padding: 1px 4px;
  border-radius: 4px;
  background: var(--el-fill-color-dark);
  color: var(--el-text-color-secondary);
  font-family: var(--app-font-mono);
}

/* --- 添加命令弹窗 --- */
.sc-add-form {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.sc-add-field {
  display: flex;
  align-items: center;
  gap: 8px;
}
.sc-add-label {
  flex-shrink: 0;
  width: 34px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
  text-align: right;
}
.sc-add-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 2px;
}
</style>
