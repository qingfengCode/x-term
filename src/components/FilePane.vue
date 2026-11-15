<!--
  FilePane.vue — 文件传输页的单栏（本地 / 远程共用）

  只负责展示与交互事件，状态由父组件通过 useFilePane 创建后以 pane prop 传入
  （路径/列表/选中/排序/加载都归 composable，切换后端时父组件直接调 pane.navigate）。

  行内操作按 props 开关：本地栏开 rowUpload，远端栏开 rowDownload/rowRename/rowRemove。
  面板间拖拽（HTML5 DnD）：行 draggable 触发 drag-start，面板的 dragover/leave/drop
  原样透传给父组件（drop 的 preventDefault 由父组件在 dnd-over 里执行）。
-->
<script setup lang="ts">
import { humanSize } from "@/utils/format";
import type { UnifiedEntry, FilePaneState } from "@/composables/useFilePane";

withDefaults(
  defineProps<{
    /** useFilePane 创建的栏状态。 */
    pane: FilePaneState;
    title: string;
    /** Element Plus 图标组件名（全局注册）。 */
    icon?: string;
    pathPlaceholder?: string;
    /** 拖拽高亮（面板间拖拽目标 / OS 文件拖入）。 */
    dragOver?: boolean;
    /** 行内「上传」按钮（本地栏）。 */
    rowUpload?: boolean;
    /** 行内「下载」按钮（远端栏）。 */
    rowDownload?: boolean;
    rowRename?: boolean;
    rowRemove?: boolean;
    /** 行可拖拽（面板间传输）。 */
    draggableRows?: boolean;
  }>(),
  {
    icon: "Document",
    pathPlaceholder: "路径",
    dragOver: false,
    rowUpload: false,
    rowDownload: false,
    rowRename: false,
    rowRemove: false,
    draggableRows: false,
  },
);

defineEmits<{
  (e: "upload", entry: UnifiedEntry): void;
  (e: "download", entry: UnifiedEntry): void;
  (e: "rename", entry: UnifiedEntry): void;
  (e: "remove", entry: UnifiedEntry): void;
  (e: "mkdir"): void;
  (e: "drag-start", entry: UnifiedEntry): void;
  (e: "dnd-over", ev: DragEvent): void;
  (e: "dnd-leave"): void;
  (e: "dnd-drop", ev: DragEvent): void;
}>();

function formatTime(s: string | null): string {
  if (!s) return "-";
  const d = new Date(s);
  if (isNaN(d.getTime())) return "-";
  const pad = (x: number) => String(x).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(
    d.getHours(),
  )}:${pad(d.getMinutes())}`;
}
</script>

<template>
  <section
    class="pane"
    :class="{ 'drag-over': dragOver }"
    @dragover="$emit('dnd-over', $event)"
    @dragleave="$emit('dnd-leave')"
    @drop="$emit('dnd-drop', $event)"
  >
    <div class="pane-header">
      <span class="pane-title">
        <el-icon><component :is="icon" /></el-icon>
        <span class="pane-title-text">{{ title }}</span>
      </span>
      <div class="pane-tools">
        <slot name="tools" />
        <el-tooltip content="上级目录" placement="bottom">
          <el-button circle size="small" :icon="'ArrowUp'" @click="pane.goUp()" />
        </el-tooltip>
        <el-tooltip content="新建目录" placement="bottom">
          <el-button circle size="small" :icon="'FolderAdd'" @click="$emit('mkdir')" />
        </el-tooltip>
        <el-tooltip content="刷新" placement="bottom">
          <el-button circle size="small" :icon="'Refresh'" @click="pane.refresh()" />
        </el-tooltip>
      </div>
    </div>

    <!-- 面包屑导航：点击任意段跳转（本地栏无面包屑） -->
    <div v-if="pane.crumbs.length" class="breadcrumb-bar">
      <template v-for="(c, i) in pane.crumbs" :key="i">
        <span class="crumb" @click="pane.navigate(c.path)">{{ c.name }}</span>
        <span v-if="i < pane.crumbs.length - 1" class="crumb-sep">/</span>
      </template>
    </div>

    <div class="path-bar">
      <el-input
        v-model="pane.path"
        size="small"
        :placeholder="pathPlaceholder"
        @keyup.enter="pane.goToInput()"
      >
        <template #prefix>
          <el-icon><Document /></el-icon>
        </template>
      </el-input>
    </div>

    <div v-loading="pane.loading" class="file-table">
      <div class="file-header">
        <span class="file-name" @click="pane.toggleSort('name')">
          名称<span v-if="pane.sortKey === 'name'">{{ pane.sortDesc ? " ↓" : " ↑" }}</span>
        </span>
        <span class="file-mtime" @click="pane.toggleSort('modified')">
          修改时间<span v-if="pane.sortKey === 'modified'">{{ pane.sortDesc ? " ↓" : " ↑" }}</span>
        </span>
        <span class="file-size" @click="pane.toggleSort('size')">
          大小<span v-if="pane.sortKey === 'size'">{{ pane.sortDesc ? " ↓" : " ↑" }}</span>
        </span>
        <span class="row-actions"></span>
      </div>
      <div
        v-for="e in pane.sorted"
        :key="e.name"
        class="file-row"
        :class="{ selected: pane.selected?.name === e.name }"
        :draggable="draggableRows && !e.isDir"
        @click="pane.selected = e"
        @dblclick="pane.enter(e)"
        @dragstart="$emit('drag-start', e)"
      >
        <el-icon class="file-icon" :class="{ 'is-dir': e.isDir }">
          <Folder v-if="e.isDir" />
          <Document v-else />
        </el-icon>
        <span class="file-name" :title="e.name">{{ e.name }}</span>
        <span class="file-mtime">{{ formatTime(e.modified) }}</span>
        <span class="file-size">{{ humanSize(e.size) }}</span>
        <span class="row-actions">
          <el-button
            v-if="rowUpload && !e.isDir"
            link
            size="small"
            type="primary"
            @click.stop="$emit('upload', e)"
          >
            上传
          </el-button>
          <el-button
            v-if="rowDownload && !e.isDir"
            link
            size="small"
            type="success"
            @click.stop="$emit('download', e)"
          >
            下载
          </el-button>
          <el-button v-if="rowRename" link size="small" @click.stop="$emit('rename', e)">
            <el-icon><EditPen /></el-icon>
          </el-button>
          <el-button
            v-if="rowRemove"
            link
            size="small"
            type="danger"
            @click.stop="$emit('remove', e)"
          >
            <el-icon><Delete /></el-icon>
          </el-button>
        </span>
      </div>
      <div v-if="!pane.loading && pane.entries.length === 0" class="empty-row">
        {{ dragOver ? "松开鼠标传输文件" : "目录为空" }}
      </div>
    </div>
  </section>
</template>

<style scoped lang="scss">
.pane {
  flex: 1;
  display: flex;
  flex-direction: column;
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: var(--app-radius-sm);
  overflow: hidden;
  min-width: 0;
}
/* 拖拽传输高亮（面板间 + OS 拖入） */
.pane.drag-over {
  border-color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
  outline: 2px dashed var(--el-color-primary);
  outline-offset: -4px;
}
/* 可拖拽行光标 */
.file-row[draggable="true"] {
  cursor: grab;
}
.file-row[draggable="true"]:active {
  cursor: grabbing;
}
.pane-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 6px 12px;
  background: var(--el-fill-color-light);
  border-bottom: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
}
.pane-title {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
  font-weight: 600;
  color: var(--el-text-color-primary);
  /* 连接名可能较长（会话名/账号名），整体收缩，不把工具按钮挤出面板 */
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
}
.pane-title :deep(.el-icon) {
  color: var(--el-color-primary);
  flex-shrink: 0;
}
.pane-title-text {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
}
.pane-tools {
  display: flex;
  gap: 4px;
  flex-shrink: 0;
}
/* 面包屑 */
.breadcrumb-bar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 2px;
  padding: 4px 8px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  font-size: 12px;
}
.crumb {
  color: var(--el-color-primary);
  cursor: pointer;
  padding: 1px 4px;
  border-radius: var(--app-radius-xs);
}
.crumb:hover {
  background: var(--el-fill-color-light);
}
.crumb-sep {
  color: var(--el-text-color-placeholder);
}
.path-bar {
  padding: 6px 8px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
}
/* 文件列表 */
.file-table {
  flex: 1;
  overflow-y: auto;
  padding: 4px;
  font-size: 13px;
}
/* 排序表头 */
.file-header {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px 8px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  font-size: 12px;
  color: var(--el-text-color-secondary);
  position: sticky;
  top: 0;
  background: var(--el-fill-color-light);
  z-index: 1;
}
.file-header > span {
  cursor: pointer;
  user-select: none;
  white-space: nowrap;
}
.file-header > span:hover {
  color: var(--el-color-primary);
}
.file-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px 8px;
  border-radius: var(--app-radius-xs);
  cursor: pointer;
  user-select: none;
  font-size: 13px;
  color: var(--el-text-color-regular);
}
.file-row:hover {
  background: var(--el-fill-color-light);
}
.file-row.selected {
  background: var(--el-color-primary-light-9);
  color: var(--el-color-primary);
}
.file-icon {
  color: var(--el-text-color-secondary);
  flex-shrink: 0;
}
.file-icon.is-dir {
  color: var(--el-color-primary);
}
.file-name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.file-size {
  width: 80px;
  text-align: right;
  color: var(--el-text-color-secondary);
  font-variant-numeric: tabular-nums;
  flex-shrink: 0;
}
.file-mtime {
  width: 140px;
  color: var(--el-text-color-secondary);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
  flex-shrink: 0;
}
.row-actions {
  /* 常驻占位（visibility 隐藏而非 display:none），悬停时出现操作按钮不再挤压文件名 */
  display: flex;
  visibility: hidden;
  align-items: center;
  justify-content: flex-end;
  gap: 6px;
  width: 96px;
  flex-shrink: 0;
}
.file-row:hover .row-actions,
.file-row:focus-within .row-actions {
  visibility: visible;
}
/* 操作按钮之间的间距交给 gap，避免 Element Plus 默认 margin 撑破占位宽度 */
.row-actions :deep(.el-button + .el-button) {
  margin-left: 0;
}
.empty-row {
  padding: 24px;
  text-align: center;
  color: var(--el-text-color-placeholder);
  font-size: 12px;
}
/* 滚动条 */
.file-table::-webkit-scrollbar {
  width: var(--app-scrollbar-thin);
}
.file-table::-webkit-scrollbar-thumb {
  background: var(--el-border-color);
  border-radius: var(--app-radius-xs);
}
</style>
