<script setup lang="ts">
// DownloadDrawer.vue — 全局下载列表抽屉
// ----------------------------------------------------------------------------
// 从标题栏「下载」按钮呼出（TitleBar，位于「关于」左侧），列出 transfer store
// 中的下载任务：SFTP 下载（会话内存态）+ ZMODEM sz 下载（持久化到 localStorage，
// 重启后仍可见）。上传任务不在此展示（SFTP 视图底部的传输队列含双向任务）。
//
// 顶部可设置「默认下载目录」（写入 settings.terminal.downloadDir）：设置后终端
// sz / SFTP / 对象存储下载都直接落盘到该目录、不再逐个弹保存对话框；每条记录的
// 路径右侧有「在文件管理器中显示」按钮（揭示 localPath）。
// ----------------------------------------------------------------------------
import { computed } from "vue";
import { Delete, Download, Folder, FolderOpened, Position } from "@element-plus/icons-vue";
import { ElMessage } from "element-plus";
import { useTransferStore, type TransferTask } from "@/stores/transfer";
import { useSettingsStore } from "@/stores/settings";
import { zmodemPickFolder } from "@/api/terminal";
import { revealInFolder } from "@/api/system";
import { configuredDownloadDir } from "@/utils/downloadPath";
import { formatSize } from "@/utils/format";

const props = defineProps<{ visible: boolean }>();
const emit = defineEmits<{ (e: "update:visible", v: boolean): void }>();

const transfer = useTransferStore();
const settings = useSettingsStore();

// --- 默认下载目录 -----------------------------------------------------------
// 在此处即可配置（不必再进设置页）：设置后所有下载（终端 sz / SFTP / 对象存储）
// 直接落盘到该目录，不再逐个弹保存对话框。
const downloadDir = computed(() => configuredDownloadDir());

/** 选择默认下载目录（无父窗口原生目录框，规避光标消失问题）。 */
async function pickDownloadDir() {
  try {
    const dir = await zmodemPickFolder("选择默认下载目录");
    if (!dir) return;
    settings.terminal.downloadDir = dir;
    await settings.save();
    ElMessage.success("已设置默认下载目录");
  } catch (e) {
    ElMessage.error("设置失败：" + String(e));
  }
}

/** 清除默认下载目录（恢复"每次下载时选择路径"）。 */
async function clearDownloadDir() {
  settings.terminal.downloadDir = "";
  await settings.save();
  ElMessage.info("已清除，下载时将每次询问保存位置");
}

/** 在系统文件管理器中定位该下载文件。 */
async function openLocation(t: TransferTask) {
  if (!t.localPath) return;
  try {
    await revealInFolder(t.localPath);
  } catch (e) {
    ElMessage.warning(String(e));
  }
}

/** 下载任务（含 SFTP 与 ZMODEM），最新在前。 */
const downloads = computed(() =>
  transfer.tasks.filter((t) => t.direction === "download").reverse(),
);

const endedCount = computed(
  () =>
    downloads.value.filter(
      (t) => t.status === "done" || t.status === "error" || t.status === "cancelled",
    ).length,
);

function percent(t: TransferTask): number {
  if (!t.total || t.total <= 0) return 0;
  return Math.max(0, Math.min(100, Math.floor((t.transferred / t.total) * 100)));
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
    case "cancelled":
      return "已取消";
  }
}

function progressStatus(t: TransferTask): "" | "success" | "exception" | "warning" {
  if (t.status === "done") return "success";
  if (t.status === "error") return "exception";
  if (t.status === "cancelled") return "warning";
  return "";
}

function humanSize(n: number): string {
  return !n || n <= 0 ? "-" : formatSize(n);
}

/** 结束时间（HH:mm）；无结束时间（进行中）显示空。 */
function endTime(t: TransferTask): string {
  if (!t.endedAt) return "";
  const d = new Date(t.endedAt);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  return `${hh}:${mm}`;
}

/** 清除已结束的下载记录（不影响上传/进行中任务）。 */
function clearEnded() {
  for (const t of downloads.value) {
    if (t.status !== "running" && t.status !== "pending") transfer.remove(t.id);
  }
}

function removeItem(t: TransferTask) {
  transfer.remove(t.id);
}
</script>

<template>
  <el-drawer
    :model-value="visible"
    title="下载列表"
    direction="rtl"
    size="420px"
    append-to-body
    @update:model-value="emit('update:visible', $event)"
  >
    <!-- 默认下载目录：设置后下载不再逐个弹保存框 -->
    <div class="dl-dir">
      <el-icon class="dl-dir-icon"><FolderOpened /></el-icon>
      <span class="dl-dir-text" :class="{ empty: !downloadDir }" :title="downloadDir">
        {{ downloadDir || "未设置默认下载位置（每次下载需选择）" }}
      </span>
      <el-button size="small" text :icon="FolderOpened" @click="pickDownloadDir">
        {{ downloadDir ? "更改" : "设置" }}
      </el-button>
      <el-button v-if="downloadDir" size="small" text @click="clearDownloadDir">清除</el-button>
    </div>

    <div class="dl-toolbar">
      <span class="dl-hint">ZMODEM 下载记录重启后保留（最近 100 条）</span>
      <el-button size="small" text :icon="Delete" :disabled="endedCount === 0" @click="clearEnded">
        清除已结束
      </el-button>
    </div>

    <div v-if="downloads.length === 0" class="dl-empty">
      <el-icon class="dl-empty-icon"><Download /></el-icon>
      <span class="dl-empty-text">暂无下载记录</span>
      <span class="dl-empty-sub">终端里执行 sz、或 SFTP 下载文件后会出现在这里</span>
    </div>

    <div v-else class="dl-list">
      <div v-for="t in downloads" :key="t.id" class="dl-item" :class="`is-${t.status}`">
        <el-icon class="dl-icon"><Download /></el-icon>

        <div class="dl-info">
          <div class="dl-row">
            <span class="dl-name" :title="t.name">{{ t.name }}</span>
            <span class="dl-meta">
              <span v-if="endTime(t)" class="dl-time">{{ endTime(t) }}</span>
              <span class="dl-status" :class="`status-${t.status}`">{{ statusText(t) }}</span>
            </span>
          </div>
          <div class="dl-sub">
            <span class="dl-size">{{ humanSize(t.transferred) }} / {{ humanSize(t.total) }}</span>
            <span v-if="t.source === 'zmodem'" class="dl-source">ZMODEM</span>
          </div>
          <el-progress
            :percentage="percent(t)"
            :status="progressStatus(t)"
            :stroke-width="4"
            :show-text="false"
            class="dl-progress"
          />
          <div v-if="t.localPath" class="dl-path">
            <el-icon class="dl-path-icon"><Folder /></el-icon>
            <span class="dl-path-text" :title="t.localPath">{{ t.localPath }}</span>
            <el-button
              class="dl-open"
              size="small"
              text
              :icon="Position"
              title="在文件管理器中显示"
              @click.stop="openLocation(t)"
            />
          </div>
          <div v-if="t.status === 'error' && t.message" class="dl-error" :title="t.message">
            {{ t.message }}
          </div>
        </div>

        <el-icon
          v-if="t.status !== 'running' && t.status !== 'pending'"
          class="dl-remove"
          title="移除记录"
          @click="removeItem(t)"
        >
          <Delete />
        </el-icon>
      </div>
    </div>
  </el-drawer>
</template>

<style scoped lang="scss">
.dl-toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  margin-bottom: 8px;
}
.dl-hint {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* --- 默认下载目录行 --- */
.dl-dir {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 8px;
  margin-bottom: 8px;
  border-radius: 6px;
  background: var(--el-fill-color-light);
  border: 1px solid var(--el-border-color-lighter);
}
.dl-dir-icon {
  font-size: 14px;
  flex-shrink: 0;
  color: var(--el-color-primary);
}
.dl-dir-text {
  flex: 1;
  min-width: 0;
  font-size: 12px;
  color: var(--el-text-color-regular);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  /* 超长路径保留尾部目录名 */
  direction: rtl;
  text-align: left;
}
.dl-dir-text.empty {
  color: var(--el-text-color-placeholder);
  direction: ltr;
}

.dl-empty {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 6px;
  padding: 48px 0;
  color: var(--el-text-color-secondary);
}
.dl-empty-icon {
  font-size: 32px;
  color: var(--el-color-info-light-5);
}
.dl-empty-text {
  font-size: 13px;
}
.dl-empty-sub {
  font-size: 12px;
  color: var(--el-text-color-placeholder);
}

.dl-list {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.dl-item {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 10px 8px;
  border-radius: 6px;
}
.dl-item:hover {
  background: var(--el-fill-color-light);
}
.dl-item.is-error {
  background: var(--el-color-danger-light-9);
}

.dl-icon {
  font-size: 18px;
  flex-shrink: 0;
  margin-top: 2px;
  color: var(--el-color-primary);
}

.dl-info {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.dl-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}
.dl-name {
  font-size: 13px;
  color: var(--el-text-color-primary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
}
.dl-meta {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
}
.dl-time {
  font-size: 11px;
  color: var(--el-text-color-placeholder);
}
.dl-status {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.status-running {
  color: var(--el-color-primary);
  font-variant-numeric: tabular-nums;
}
.status-done {
  color: var(--el-color-success);
}
.status-error {
  color: var(--el-color-danger);
}
.status-cancelled {
  color: var(--el-color-warning);
}

.dl-sub {
  display: flex;
  align-items: center;
  gap: 8px;
}
.dl-size {
  font-size: 11px;
  color: var(--el-text-color-secondary);
  font-variant-numeric: tabular-nums;
}
.dl-source {
  font-size: 10px;
  line-height: 1;
  padding: 2px 4px;
  border-radius: 3px;
  color: var(--el-color-info);
  background: var(--el-fill-color);
}

.dl-progress {
  width: 100%;
}

.dl-path {
  display: flex;
  align-items: center;
  gap: 4px;
  min-width: 0;
}
.dl-path-icon {
  font-size: 12px;
  flex-shrink: 0;
  color: var(--el-text-color-placeholder);
}
.dl-path-text {
  flex: 1;
  min-width: 0;
  font-size: 11px;
  color: var(--el-text-color-secondary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  direction: rtl; /* 超长路径保留尾部文件名 */
  text-align: left;
}

.dl-error {
  font-size: 11px;
  color: var(--el-color-danger);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* 「在文件管理器中显示」按钮：紧邻路径文本，弱化配色，悬停高亮 */
.dl-open {
  flex-shrink: 0;
  height: 18px;
  padding: 0 4px;
  color: var(--el-text-color-placeholder);
}
.dl-open:hover {
  color: var(--el-color-primary);
}

.dl-remove {
  font-size: 14px;
  flex-shrink: 0;
  margin-top: 2px;
  color: var(--el-text-color-secondary);
  cursor: pointer;
  padding: 4px;
  border-radius: 4px;
}
.dl-remove:hover {
  color: var(--el-color-danger);
  background: var(--el-fill-color);
}
</style>
