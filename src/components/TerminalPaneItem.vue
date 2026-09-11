<script setup lang="ts">
/**
 * 单个分屏窗格项：窗格头部（多窗格时）+ TerminalPane + 连接状态/断开
 * 重连覆盖层。
 *
 * 由 SplitArea 按 tab.panes 平铺渲染（key = pane.id，永不重排卸载）。
 * 窗格动作（拆分/关闭/最大化/激活/重连/手动认证）直接走 terminals store；
 * 按键输入以 input 事件上报父级做广播路由；TerminalPane 实例引用经
 * inject 注册回 Workspace（工具栏/广播按 tabId::paneId 索引）。
 */
import { computed, inject } from "vue";
import { Close, FullScreen, Key, Menu, Refresh } from "@element-plus/icons-vue";
import TerminalPane from "@/components/TerminalPane.vue";
import { useTerminalsStore, type TerminalPaneState, type TerminalTab } from "@/stores/terminals";
import type { SplitDirection } from "@/utils/splitLayout";
import { isAuthError } from "@/utils/error";

const props = defineProps<{
  tab: TerminalTab;
  pane: TerminalPaneState;
  /** 是否显示窗格头部（多窗格 / 最大化时）。 */
  showHeader: boolean;
}>();
const emit = defineEmits<{
  (e: "input", data: string): void;
}>();
const terminals = useTerminalsStore();

/** Workspace 注入的 TerminalPane 实例注册器（键 tabId::paneId）。 */
const registerPaneRef = inject<
  ((tabId: string, paneId: string, el: unknown) => void) | null
>("xterm:registerPaneRef", null);

function onRef(el: unknown) {
  registerPaneRef?.(props.tab.id, props.pane.id, el);
}

/** 窗格标题：会话名；多窗格时追加序号区分。 */
const title = computed(() => {
  const idx = props.tab.panes.indexOf(props.pane);
  return props.tab.panes.length > 1 && idx >= 0
    ? `${props.tab.session.name} · ${idx + 1}`
    : props.tab.session.name;
});
const isActive = computed(() => props.tab.activePaneId === props.pane.id);
const zoomed = computed(() => props.tab.zoomedPaneId === props.pane.id);

function activate() {
  terminals.setActivePane(props.tab, props.pane.id);
}
function split(dir: SplitDirection) {
  terminals.setActivePane(props.tab, props.pane.id);
  void terminals.splitPane(props.tab, dir);
}
function close() {
  void terminals.closePane(props.tab, props.pane.id);
}
function toggleZoom() {
  terminals.toggleZoomPane(props.tab, props.pane.id);
}
function reconnect() {
  if (props.pane.instanceId) void terminals.reconnect(props.pane.instanceId);
}
function manualAuth() {
  terminals.openManualAuth(props.tab, props.pane.id);
}
</script>

<template>
  <div
    class="term-item"
    :class="{ active: showHeader && isActive, 'with-head': showHeader }"
    :data-pane-id="pane.id"
    @mousedown="activate"
  >
    <div v-if="showHeader" class="term-item-head">
      <span class="term-item-title" :title="title">{{ title }}</span>
      <span
        v-if="terminals.broadcastInput"
        class="term-item-bc"
        title="广播输入已开启：按键将同步到本页签全部窗格"
      >广播</span>
      <span class="term-item-spacer" />
      <el-tooltip content="向右分屏 (Alt+Shift+=)" placement="bottom" :show-after="400">
        <button class="mini-btn" @click.stop="split('row')">
          <el-icon class="rot-90"><Menu /></el-icon>
        </button>
      </el-tooltip>
      <el-tooltip content="向下分屏 (Alt+Shift+-)" placement="bottom" :show-after="400">
        <button class="mini-btn" @click.stop="split('col')">
          <el-icon><Menu /></el-icon>
        </button>
      </el-tooltip>
      <el-tooltip :content="zoomed ? '还原布局 (Alt+Shift+Enter)' : '最大化窗格 (Alt+Shift+Enter)'" placement="bottom" :show-after="400">
        <button class="mini-btn" :class="{ on: zoomed }" @click.stop="toggleZoom">
          <el-icon><FullScreen /></el-icon>
        </button>
      </el-tooltip>
      <el-tooltip content="关闭窗格 (Alt+Shift+W)" placement="bottom" :show-after="400">
        <button class="mini-btn danger" @click.stop="close">
          <el-icon><Close /></el-icon>
        </button>
      </el-tooltip>
    </div>
    <div class="term-item-body">
      <template v-if="pane.instanceId">
        <TerminalPane
          :ref="onRef"
          :instance-id="pane.instanceId"
          :session-config-id="tab.session.id"
          :remote="tab.session.protocol !== 'local'"
          @closed="terminals.handleTerminalClosed(pane.instanceId)"
          @input="(d: string) => emit('input', d)"
        />
        <!-- 断开重连覆盖层 -->
        <div v-if="pane.disconnected" class="reconnect-overlay">
          <div class="reconnect-card">
            <div class="reconnect-title">连接已断开</div>
            <!-- 最近一次重连失败的原因（reconnect 失败写入 pane.error） -->
            <div v-if="pane.error" class="reconnect-error" :title="pane.error">
              {{ pane.error }}
            </div>
            <el-button
              type="primary"
              :icon="Refresh"
              :loading="pane.reconnecting"
              @click="reconnect"
            >
              重新连接
            </el-button>
          </div>
        </div>
      </template>
      <div v-else-if="pane.connecting" class="pane-status">连接中…</div>
      <div v-else-if="pane.error" class="pane-status error">
        <span class="pane-error-text">连接失败：{{ pane.error }}</span>
        <!-- 认证失败：提供手动输入密码/口令码重试的入口（弹窗） -->
        <el-button v-if="isAuthError(pane.error)" size="small" :icon="Key" @click="manualAuth">
          手动认证
        </el-button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.term-item {
  display: flex;
  flex-direction: column;
  width: 100%;
  height: 100%;
  min-width: 0;
  min-height: 0;
  overflow: hidden;
  position: relative;
  background: var(--el-bg-color);
}
/* 多窗格时活动窗格高亮（outline 不占布局空间） */
.term-item.with-head.active {
  outline: 1px solid var(--el-color-primary);
  outline-offset: -1px;
  z-index: 1;
}
.term-item-head {
  display: flex;
  align-items: center;
  gap: 4px;
  height: 26px;
  padding: 0 6px;
  flex-shrink: 0;
  background: var(--el-fill-color-light);
  border-bottom: 1px solid var(--el-border-color-lighter);
  user-select: none;
}
.term-item-title {
  font-size: 12px;
  color: var(--el-text-color-regular);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  min-width: 0;
}
.term-item.active .term-item-title {
  color: var(--el-color-primary);
  font-weight: 500;
}
.term-item-bc {
  flex-shrink: 0;
  font-size: 10px;
  line-height: 1;
  padding: 2px 5px;
  border-radius: 3px;
  background: var(--el-color-primary);
  color: #fff;
}
.term-item-spacer {
  flex: 1;
}
.mini-btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 20px;
  height: 20px;
  border: none;
  border-radius: 4px;
  background: transparent;
  color: var(--el-text-color-secondary);
  font-size: 12px;
  cursor: pointer;
  flex-shrink: 0;
  transition: color 0.15s ease, background-color 0.15s ease;
}
.mini-btn:hover {
  color: var(--el-color-primary);
  background: var(--el-fill-color);
}
.mini-btn.on {
  color: var(--el-color-primary);
}
.mini-btn.danger:hover {
  color: var(--el-color-danger);
}
.rot-90 {
  transform: rotate(90deg);
}
.term-item-body {
  flex: 1;
  min-height: 0;
  min-width: 0;
  position: relative;
  display: flex;
}
.term-item-body > * {
  flex: 1;
  min-width: 0;
  min-height: 0;
}
/* --- 连接状态 / 断开覆盖层（自 Workspace 迁移） --- */
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
.reconnect-overlay {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgba(0, 0, 0, 0.35);
  backdrop-filter: blur(3px);
  z-index: 20;
  /* 遮罩放行鼠标事件：断开后终端输出仍可选中/复制/滚动（排障刚需）；
     只有中央卡片拦截点击。 */
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
.reconnect-title {
  font-size: 14px;
  font-weight: 600;
  letter-spacing: 0.5px;
  color: var(--el-text-color-primary);
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
</style>
