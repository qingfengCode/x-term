<script setup lang="ts">
import { computed, onMounted, onBeforeUnmount, ref } from "vue";
import { useTerminalsStore } from "@/stores/terminals";
import { useSettingsStore } from "@/stores/settings";
import TerminalPane from "@/components/TerminalPane.vue";
import AiPanel from "@/components/AiPanel.vue";
import { Close, Delete, Top, Bottom, ZoomIn, ZoomOut, Refresh } from "@element-plus/icons-vue";
import { eventToCombo, isModifierOnly } from "@/utils/shortcut";
import type { ShortcutCommand } from "@/api/types";

// KeepAlive 按 name 匹配缓存本组件（保留终端助手面板状态）。
defineOptions({ name: "Workspace" });

const terminals = useTerminalsStore();
const settings = useSettingsStore();

const active = computed(() =>
  terminals.tabs.find((t) => t.instanceId === terminals.activeId)
);

// 当前活动 pane 的组件引用（用于调 clear/focus）。
const activePaneRef = ref<InstanceType<typeof TerminalPane> | null>(null);

async function closeTab(instanceId: string, e?: Event) {
  e?.stopPropagation();
  await terminals.close(instanceId);
}

// 终端被通知连接断开（由 TerminalPane emit "closed"）。
function onTerminalClosed(instanceId: string) {
  terminals.markDisconnected(instanceId);
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
  const next = Math.max(8, Math.min(36, settings.terminal.fontSize + delta));
  settings.setTerminal({ fontSize: next });
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
  if (!s) return cmd;
  return cmd
    .replaceAll("{host}", s.host)
    .replaceAll("{user}", s.username)
    .replaceAll("{port}", String(s.port));
}

/** 向活动终端发送一条快捷命令。 */
function runShortcut(sc: ShortcutCommand) {
  if (!active.value?.instanceId) return;
  activePaneRef.value?.sendCommand(resolveCommand(sc.command));
}

/** 用于全局快捷键监听（自定义快捷命令）。 */
function onGlobalKeydown(e: KeyboardEvent) {
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
  }
}

onMounted(() => {
  window.addEventListener("keydown", onGlobalKeydown);
});
onBeforeUnmount(() => {
  window.removeEventListener("keydown", onGlobalKeydown);
});
</script>

<template>
  <div class="workspace">
    <div class="tab-bar">
      <div class="tabs-scroll">
        <div
          v-for="(tab, i) in terminals.tabs"
          :key="tab.instanceId || tab.session.id"
          class="tab"
          :class="{ active: tab.instanceId === terminals.activeId }"
          @click="tab.instanceId && terminals.setActive(tab.instanceId)"
        >
          <span class="dot" :class="{ connecting: tab.connecting, dead: tab.disconnected }" />
          <span class="tab-idx" v-if="i < 9">{{ i + 1 }}</span>
          <span class="title">{{ tab.session.name }}</span>
          <el-icon class="close" @click="(e: Event) => closeTab(tab.instanceId, e)"><Close /></el-icon>
        </div>
        <div v-if="terminals.tabs.length === 0" class="tab-hint">从左侧会话树双击连接</div>
      </div>
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
          :key="tab.instanceId || tab.session.id"
          v-show="tab.instanceId === terminals.activeId"
          class="pane"
        >
          <template v-if="tab.instanceId">
            <TerminalPane
              :ref="(el: any) => { if (tab.instanceId === terminals.activeId) activePaneRef = el }"
              :instance-id="tab.instanceId"
              @closed="onTerminalClosed(tab.instanceId)"
            />
            <!-- 断开重连覆盖层 -->
            <div v-if="tab.disconnected" class="reconnect-overlay">
              <div class="reconnect-card">
                <div class="reconnect-title">连接已断开</div>
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
            连接失败：{{ tab.error }}
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
    <div v-if="active && settings.shortcuts.length > 0" class="shortcut-bar">
      <!-- 分组标签页（仅存在分组时显示） -->
      <div v-if="hasGroups" class="sc-tabs">
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
      </div>
      <!-- 命令按钮区 -->
      <div class="sc-buttons">
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
        <span v-if="visibleShortcuts.length === 0" class="sc-empty">该分组暂无命令</span>
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
  height: 34px;
  background: var(--el-bg-color-overlay);
  border-bottom: 1px solid var(--el-border-color-lighter);
  padding: 0 4px;
  flex-shrink: 0;
}
.tabs-scroll {
  display: flex;
  align-items: center;
  flex: 1;
  min-width: 0;
  overflow-x: auto;
}
.tabs-scroll::-webkit-scrollbar {
  height: 0;
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
  padding: 4px;
  color: var(--el-text-color-secondary);
}
.tool-btn:hover {
  color: var(--el-color-primary);
}
.tab-idx {
  font-size: 10px;
  color: var(--el-text-color-placeholder);
  margin-right: 2px;
}
.tab {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 4px 10px;
  margin-right: 2px;
  border-radius: 4px 4px 0 0;
  cursor: pointer;
  font-size: 13px;
  color: var(--el-text-color-regular);
  max-width: 200px;
}
.tab:hover {
  background: var(--el-fill-color-light);
}
.tab.active {
  background: var(--el-bg-color-page);
  color: var(--el-color-primary);
}
.tab .title {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.tab .dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: #22c55e;
}
.tab .dot.connecting {
  background: #f59e0b;
}
.tab .dot.dead {
  background: #ef4444;
}
.tab .close {
  font-size: 12px;
  padding: 2px;
  border-radius: 2px;
}
.tab .close:hover {
  background: var(--el-fill-color-dark);
}
.tab-hint {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  margin-left: 8px;
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
  backdrop-filter: blur(2px);
  z-index: 20;
}
.reconnect-card {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 12px;
  padding: 20px 28px;
  background: var(--el-bg-color-overlay);
  border-radius: 8px;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.2);
}
.reconnect-title {
  font-size: 14px;
  color: var(--el-text-color-secondary);
}
.pane-status {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100%;
  color: var(--el-text-color-secondary);
}
.pane-status.error {
  color: var(--el-color-danger);
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
.sc-buttons {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 10px;
  overflow-x: auto;
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
  border: 1px solid var(--el-border-color);
  border-radius: 4px;
  background: var(--el-fill-color-blank);
  color: var(--el-text-color-regular);
  font-size: 12px;
  cursor: pointer;
  white-space: nowrap;
  transition: all 0.15s;
}
.sc-btn:hover {
  border-color: var(--el-color-primary);
  color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
}
.sc-btn:active {
  transform: translateY(1px);
}
.sc-label {
  font-family: var(--el-font-family-mono, "Cascadia Code", Consolas, monospace);
}
.sc-key {
  font-size: 10px;
  padding: 1px 4px;
  border-radius: 3px;
  background: var(--el-fill-color-dark);
  color: var(--el-text-color-secondary);
}
</style>
