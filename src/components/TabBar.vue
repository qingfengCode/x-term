<!--
  TabBar.vue — 共享标签栏组件

  从 Workspace（终端页）与 RemoteDesktopView（桌面页）的逐行雷同实现中抽取，
  统一以下交互与样式：
  - 点击切换 / 中键关闭 / 关闭按钮
  - Ctrl 序号提示（前 9 个 tab 显示序号）
  - 连接状态圆点（connecting 黄 / disconnected 红 / 正常绿）
  - 滚轮纵向转横向滚动
  - HTML5 拖拽排序（过半即换位，通过 move 事件交给 store 执行）
  - 右键菜单（关闭/关闭其他/关闭全部/断开时显示重新连接），带视口钳制
    （窗口右/下边缘右键时菜单不会被裁掉）与统一 z-index。

  用法：
    <TabBar :tabs="items" :active-key="activeId" empty-hint="..."
            @select="..." @close="..." @move="..." @command="..." />
-->
<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from "vue";
import { Close } from "@element-plus/icons-vue";

/** 单个标签的数据抽象（终端 tab 与桌面 tab 的公共子集）。 */
export interface TabBarItem {
  /** 唯一键（instanceId，未建立连接前可为 session/desktop id）。 */
  key: string;
  /** 标签标题。 */
  title: string;
  /** 连接中（黄点）。 */
  connecting?: boolean;
  /** 已断开（红点，右键菜单追加"重新连接"）。 */
  disconnected?: boolean;
}

const props = defineProps<{
  tabs: TabBarItem[];
  /** 当前激活的 tab key（store 的 activeId 可能为 null 表示无激活）。 */
  activeKey?: string | null;
  /** 无 tab 时的提示文案。 */
  emptyHint?: string;
}>();

const emit = defineEmits<{
  /** 点击切换 tab。 */
  select: [key: string];
  /** 关闭单个 tab（关闭按钮 / 中键）。 */
  close: [key: string];
  /** 拖拽排序：把 from 移到 target 的前/后。 */
  move: [from: string, to: string, before: boolean];
  /** 右键菜单命令（作用于右键的 tab）。 */
  command: [cmd: "close" | "closeOthers" | "closeAll" | "reconnect", key: string];
}>();

function onSelect(tab: TabBarItem) {
  emit("select", tab.key);
}

function onClose(tab: TabBarItem, e?: Event) {
  e?.stopPropagation();
  emit("close", tab.key);
}

function onAuxClick(tab: TabBarItem, e: MouseEvent) {
  // 中键关闭（并阻止中键自动滚动）。
  if (e.button === 1) {
    e.preventDefault();
    emit("close", tab.key);
  }
}

/** 标签区滚轮：纵向滚动转为横向滚动。 */
function onWheel(e: WheelEvent) {
  const el = e.currentTarget as HTMLElement;
  el.scrollLeft += e.deltaY;
}

// --- 拖拽排序（HTML5 DnD，dragover 时按过半即换位） -------------------------

const dragKey = ref<string | null>(null);

function onDragStart(e: DragEvent, key: string) {
  dragKey.value = key;
  if (e.dataTransfer) {
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/plain", key);
  }
}

function onDragOver(e: DragEvent, targetKey: string) {
  const from = dragKey.value;
  if (!from || from === targetKey) return;
  e.preventDefault();
  if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
  const el = e.currentTarget as HTMLElement;
  const before = e.offsetX < el.clientWidth / 2;
  emit("move", from, targetKey, before);
}

function onDragEnd() {
  dragKey.value = null;
}

// --- 右键菜单（视口钳制 + 统一 z-index） -------------------------------------

/** 菜单浮层的估算尺寸（钳制坐标用，需覆盖最坏情况：断开时 4 项 + 分隔线）。 */
const MENU_W = 150;
const MENU_H = 170;

const menu = ref<{ x: number; y: number; tab: TabBarItem | null }>({
  x: 0,
  y: 0,
  tab: null,
});

function onContextMenu(tab: TabBarItem, e: MouseEvent) {
  // 窗口右/下边缘右键时按菜单估算尺寸钳制坐标，避免菜单超出视口被裁掉。
  menu.value = {
    x: Math.min(e.clientX, window.innerWidth - MENU_W - 8),
    y: Math.min(e.clientY, window.innerHeight - MENU_H - 8),
    tab,
  };
}

function closeMenu() {
  menu.value.tab = null;
}

function onMenuCommand(cmd: "close" | "closeOthers" | "closeAll" | "reconnect") {
  const t = menu.value.tab;
  closeMenu();
  if (t) emit("command", cmd, t.key);
}

// 点击任意处 / Esc 关闭菜单（挂 window，随组件生命周期注销）。
function onWindowClick() {
  closeMenu();
}
function onWindowKeydown(e: KeyboardEvent) {
  if (e.key === "Escape") closeMenu();
}
onMounted(() => {
  window.addEventListener("click", onWindowClick);
  window.addEventListener("keydown", onWindowKeydown);
});
onBeforeUnmount(() => {
  window.removeEventListener("click", onWindowClick);
  window.removeEventListener("keydown", onWindowKeydown);
});

defineExpose({ closeMenu });
</script>

<template>
  <div class="tabs-scroll" @wheel="onWheel">
    <div
      v-for="(tab, i) in props.tabs"
      :key="tab.key"
      class="tab"
      :class="{ active: tab.key === props.activeKey, dragging: dragKey === tab.key }"
      draggable="true"
      :title="tab.title"
      @click="onSelect(tab)"
      @auxclick="(e: MouseEvent) => onAuxClick(tab, e)"
      @mousedown.middle.prevent
      @contextmenu.prevent="(e: MouseEvent) => onContextMenu(tab, e)"
      @dragstart="(e: DragEvent) => onDragStart(e, tab.key)"
      @dragover="(e: DragEvent) => onDragOver(e, tab.key)"
      @dragend="onDragEnd"
    >
      <span class="dot" :class="{ connecting: tab.connecting, dead: tab.disconnected }" />
      <span v-if="i < 9" class="tab-idx">{{ i + 1 }}</span>
      <span class="title">{{ tab.title }}</span>
      <el-icon class="close" @click="(e: Event) => onClose(tab, e)"><Close /></el-icon>
    </div>
    <div v-if="props.tabs.length === 0 && props.emptyHint" class="tab-hint">
      {{ props.emptyHint }}
    </div>

    <!-- 右键菜单（fixed 浮层） -->
    <div
      v-if="menu.tab"
      class="tab-menu"
      :style="{ left: menu.x + 'px', top: menu.y + 'px' }"
      @click.stop
    >
      <div class="tab-menu-item" @click="onMenuCommand('close')">关闭</div>
      <div class="tab-menu-item" @click="onMenuCommand('closeOthers')">关闭其他</div>
      <div class="tab-menu-item" @click="onMenuCommand('closeAll')">关闭全部</div>
      <template v-if="menu.tab.disconnected">
        <div class="tab-menu-sep" />
        <div class="tab-menu-item" @click="onMenuCommand('reconnect')">重新连接</div>
      </template>
    </div>
  </div>
</template>

<style scoped>
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
  background: var(--el-color-success);
  flex-shrink: 0;
}
.tab .dot.connecting {
  background: var(--el-color-warning);
}
.tab .dot.dead {
  background: var(--el-color-danger);
}
/* 拖拽中的 tab 半透明提示 */
.tab.dragging {
  opacity: 0.5;
}
.tab .close {
  font-size: 12px;
  padding: 2px;
  border-radius: 2px;
}
.tab .close:hover {
  background: var(--el-fill-color-dark);
}
.tab-idx {
  font-size: 10px;
  color: var(--el-text-color-placeholder);
  margin-right: 2px;
}
.tab-hint {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  margin-left: 8px;
}

/* --- Tab 右键菜单（fixed 浮层） --- */
.tab-menu {
  position: fixed;
  min-width: 140px;
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color);
  border-radius: 6px;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.2);
  padding: 4px 0;
  /* 统一浮层层级（与 SqlConsoleView 菜单一致；高于 el-dialog 遮罩 2000+，
     低于右侧终端右键菜单被浮层遮挡的问题不复存在）。 */
  z-index: 3000;
}
.tab-menu-item {
  padding: 6px 14px;
  font-size: 13px;
  color: var(--el-text-color-primary);
  cursor: pointer;
}
.tab-menu-item:hover {
  background: var(--el-color-primary-light-9);
  color: var(--el-color-primary);
}
.tab-menu-sep {
  height: 1px;
  background: var(--el-border-color-lighter);
  margin: 4px 0;
}
</style>
