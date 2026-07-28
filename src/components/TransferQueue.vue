<script setup lang="ts">
// 传输队列组件
// ----------------------------------------------------------------------------
// 固定显示在 SFTP 视图底部（可折叠），列出 transferStore.tasks。
// 后端通过 transfer:progress / transfer:done / transfer:error 事件推送进度，
// MainLayout 已统一订阅并写入 transferStore，本组件仅负责展示。
// ----------------------------------------------------------------------------
import { computed, ref } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { Download, Upload, Delete, FolderOpened, CaretBottom, CaretTop } from "@element-plus/icons-vue";
import { useTransferStore, type TransferTask } from "@/stores/transfer";

const transfer = useTransferStore();

// 折叠状态：默认展开，便于看到正在进行的任务。
const collapsed = ref(false);

// 倒序展示，让最新创建的任务出现在顶部。
const sortedTasks = computed(() => [...transfer.tasks].reverse());

const runningCount = computed(
  () => transfer.tasks.filter((t) => t.status === "running" || t.status === "pending").length
);

const doneCount = computed(
  () => transfer.tasks.filter((t) => t.status === "done" || t.status === "error").length
);

function percent(t: TransferTask): number {
  if (!t.total || t.total <= 0) return 0;
  const p = Math.floor((t.transferred / t.total) * 100);
  return Math.max(0, Math.min(100, p));
}

function statusText(t: TransferTask): string {
  switch (t.status) {
    case "pending":
      return "等待中";
    case "running":
      return `${percent(t)}%`;
    case "done":
      return "已完成";
    case "error":
      return "失败";
  }
}

function progressStatus(t: TransferTask): "" | "success" | "exception" | "warning" {
  if (t.status === "done") return "success";
  if (t.status === "error") return "exception";
  return "";
}

// 人类可读大小。
function humanSize(n: number): string {
  if (!n || n <= 0) return "-";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}

async function removeTask(t: TransferTask) {
  // 仅清理本地展示；后端实际取消需另外提供 cancel 接口，这里按 MVP 处理。
  if (t.status === "running") {
    try {
      await ElMessageBox.confirm(
        `任务 "${t.name}" 正在传输，移除后将不再显示进度。继续？`,
        "移除任务",
        { type: "warning", confirmButtonText: "移除", cancelButtonText: "取消" }
      );
    } catch {
      return;
    }
  }
  transfer.remove(t.id);
}

async function clearDone() {
  if (doneCount.value === 0) {
    ElMessage.info("没有可清除的任务");
    return;
  }
  transfer.clearDone();
}

function toggle() {
  collapsed.value = !collapsed.value;
}
</script>

<template>
  <div class="transfer-queue" :class="{ collapsed }">
    <!-- 头部：标题 + 操作 -->
    <header class="tq-header" @click="toggle">
      <div class="tq-title">
        <el-icon class="tq-icon"><FolderOpened /></el-icon>
        <span class="tq-label">传输队列</span>
        <el-badge v-if="runningCount > 0" :value="runningCount" class="tq-badge" type="primary" />
        <span v-else-if="doneCount > 0" class="tq-done-count">{{ doneCount }} 完成</span>
      </div>
      <div class="tq-actions" @click.stop>
        <el-button
          size="small"
          text
          :icon="Delete"
          :disabled="doneCount === 0"
          @click="clearDone"
        >
          清除已完成
        </el-button>
        <el-icon class="tq-toggle"><component :is="collapsed ? CaretTop : CaretBottom" /></el-icon>
      </div>
    </header>

    <!-- 任务列表 -->
    <div v-show="!collapsed" class="tq-body">
      <div v-if="sortedTasks.length === 0" class="tq-empty">
        <el-icon class="tq-empty-icon"><FolderOpened /></el-icon>
        <span class="tq-empty-text">暂无传输任务</span>
      </div>

      <div v-else class="tq-list">
        <div
          v-for="t in sortedTasks"
          :key="t.id"
          class="tq-item"
          :class="`is-${t.status}`"
        >
          <el-icon class="tq-dir" :class="`dir-${t.direction}`">
            <Download v-if="t.direction === 'download'" />
            <Upload v-else />
          </el-icon>

          <div class="tq-info">
            <div class="tq-row">
              <span class="tq-name" :title="t.name">{{ t.name }}</span>
              <span class="tq-size">
                {{ humanSize(t.transferred) }} / {{ humanSize(t.total) }}
              </span>
            </div>
            <el-progress
              :percentage="percent(t)"
              :status="progressStatus(t)"
              :stroke-width="6"
              :show-text="false"
              class="tq-progress"
            />
          </div>

          <div class="tq-meta">
            <span class="tq-status" :class="`status-${t.status}`">
              {{ statusText(t) }}
            </span>
            <el-tooltip
              v-if="t.status === 'error' && t.message"
              :content="t.message"
              placement="top"
            >
              <span class="tq-errmsg">!</span>
            </el-tooltip>
          </div>

          <el-icon class="tq-remove" @click="removeTask(t)"><Delete /></el-icon>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.transfer-queue {
  display: flex;
  flex-direction: column;
  background: var(--el-bg-color-overlay);
  border-top: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
  transition: height 0.2s ease;
}

.tq-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  height: 32px;
  padding: 0 12px;
  cursor: pointer;
  user-select: none;
  border-bottom: 1px solid var(--el-border-color-lighter);
}
.tq-header:hover {
  background: var(--el-fill-color-light);
}

.tq-title {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 13px;
  color: var(--el-text-color-primary);
  font-weight: 600;
}
.tq-icon {
  font-size: 14px;
  color: var(--el-color-primary);
}
.tq-badge {
  margin-left: 4px;
}
.tq-done-count {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  font-weight: normal;
}

.tq-actions {
  display: flex;
  align-items: center;
  gap: 8px;
}
.tq-toggle {
  font-size: 14px;
  color: var(--el-text-color-secondary);
}

.tq-body {
  max-height: 220px;
  overflow-y: auto;
}

/* 空状态 */
.tq-empty {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 6px;
  padding: 24px;
  color: var(--el-text-color-secondary);
}
.tq-empty-icon {
  font-size: 28px;
  color: var(--el-text-color-placeholder);
}
.tq-empty-text {
  font-size: 12px;
}

/* 任务行 */
.tq-list {
  display: flex;
  flex-direction: column;
  padding: 4px 8px;
}
.tq-item {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 8px;
  border-radius: 4px;
}
.tq-item:hover {
  background: var(--el-fill-color-light);
}
.tq-item.is-error {
  background: var(--el-color-danger-light-9);
}

.tq-dir {
  font-size: 18px;
  flex-shrink: 0;
}
.dir-download {
  color: var(--el-color-primary);
}
.dir-upload {
  color: var(--el-color-success);
}

.tq-info {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.tq-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}
.tq-name {
  font-size: 12px;
  color: var(--el-text-color-primary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
}
.tq-size {
  font-size: 11px;
  color: var(--el-text-color-secondary);
  flex-shrink: 0;
  font-variant-numeric: tabular-nums;
}
.tq-progress {
  width: 100%;
}

.tq-meta {
  display: flex;
  align-items: center;
  gap: 4px;
  flex-shrink: 0;
  min-width: 56px;
  justify-content: flex-end;
}
.tq-status {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.status-running {
  color: var(--el-color-primary);
}
.status-done {
  color: var(--el-color-success);
}
.status-error {
  color: var(--el-color-danger);
}
.tq-errmsg {
  color: var(--el-color-danger);
  font-weight: bold;
  cursor: help;
}

.tq-remove {
  font-size: 14px;
  color: var(--el-text-color-secondary);
  cursor: pointer;
  padding: 4px;
  border-radius: 4px;
  flex-shrink: 0;
}
.tq-remove:hover {
  color: var(--el-color-danger);
  background: var(--el-fill-color);
}

/* 折叠时只显示头部 */
.transfer-queue.collapsed .tq-body {
  display: none;
}

/* 滚动条美化 */
.tq-body::-webkit-scrollbar {
  width: 6px;
}
.tq-body::-webkit-scrollbar-thumb {
  background: var(--el-border-color);
  border-radius: 3px;
}
</style>
