<!--
  TitleBar.vue — 自绘窗口标题栏
  系统原生标题栏已关闭（tauri.conf.json decorations: false），
  品牌区 + 拖拽区 + 窗口控制（最小化/最大化还原/关闭）由本组件承担，
  挂载于 App.vue 顶部，所有路由（含解锁页）共用。
-->
<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useUpdateStore } from "@/stores/update";
import { useTransferStore } from "@/stores/transfer";
import DownloadDrawer from "@/components/DownloadDrawer.vue";

const emit = defineEmits<{ about: [] }>();

const appWindow = getCurrentWindow();
const updater = useUpdateStore();
const transfer = useTransferStore();

/** 是否存在可用新版本（关于按钮上的红点提示；含下载中/已下载态）。 */
const hasUpdate = computed(() =>
  ["update-available", "downloading", "downloaded"].includes(updater.status),
);

// --- 下载列表（ZMODEM sz / SFTP 下载记录） ----------------------------------
// 呼出按钮放在「关于」左侧：属于全局工具入口，与窗口级动作同排更符合预期
// （原先在左侧导航栏顶部，与页面导航语义混在一起）。
const downloadVisible = ref(false);
/** 进行中的下载数（badge 提示；含 ZMODEM 与 SFTP）。 */
const runningDownloads = computed(
  () =>
    transfer.tasks.filter(
      (t) => t.direction === "download" && (t.status === "running" || t.status === "pending"),
    ).length,
);

/** 当前是否最大化（控制按钮切换 最大化/还原 图标与提示）。 */
const maximized = ref(false);

async function minimize() {
  await appWindow.minimize();
}
async function toggleMaximize() {
  await appWindow.toggleMaximize();
}
async function close() {
  await appWindow.close();
}

// 窗口尺寸变化（最大化/还原/手动调整）时同步图标状态。
let unlisten: (() => void) | null = null;
onMounted(async () => {
  try {
    maximized.value = await appWindow.isMaximized();
    unlisten = await appWindow.onResized(async () => {
      try {
        maximized.value = await appWindow.isMaximized();
      } catch {
        /* 窗口销毁竞态时忽略 */
      }
    });
  } catch (e) {
    console.error("标题栏窗口状态监听失败:", e);
  }
});
onBeforeUnmount(() => unlisten?.());
</script>

<template>
  <div class="title-bar" data-tauri-drag-region>
    <!-- 品牌区 -->
    <div class="brand" data-tauri-drag-region>
      <span class="brand-badge" data-tauri-drag-region>X</span>
      <span class="brand-name" data-tauri-drag-region>X-Term</span>
    </div>

    <!-- 中部拖拽区（双击最大化/还原由 Tauri 拖拽区内置行为处理） -->
    <div class="drag-region" data-tauri-drag-region />

    <!-- 下载列表入口（在「关于」左侧；进行中下载数用 badge 提示） -->
    <button
      class="tb-btn download-btn"
      title="下载列表（ZMODEM / SFTP 下载记录）"
      @click="downloadVisible = true"
    >
      <el-badge :value="runningDownloads" :hidden="runningDownloads === 0" :max="99">
        <el-icon><Download /></el-icon>
      </el-badge>
    </button>

    <!-- 关于入口（有新版本时红点提示） -->
    <button
      class="tb-btn about-btn"
      :class="{ 'has-update': hasUpdate }"
      title="关于 / 检查更新"
      @click="emit('about')"
    >
      <el-icon><InfoFilled /></el-icon>
      <span v-if="hasUpdate" class="update-dot" />
    </button>

    <!-- 窗口控制按钮（Windows 惯例：最小化 / 最大化还原 / 关闭） -->
    <div class="win-controls">
      <button class="win-btn" title="最小化" @click="minimize">
        <el-icon><Minus /></el-icon>
      </button>
      <button
        class="win-btn"
        :title="maximized ? '向下还原' : '最大化'"
        @click="toggleMaximize"
      >
        <el-icon v-if="maximized"><CopyDocument /></el-icon>
        <el-icon v-else><FullScreen /></el-icon>
      </button>
      <button class="win-btn win-close" title="关闭" @click="close">
        <el-icon><Close /></el-icon>
      </button>
    </div>

    <!-- 全局下载列表抽屉（append-to-body，挂在标题栏内以复用其呼出按钮状态） -->
    <DownloadDrawer v-model:visible="downloadVisible" />
  </div>
</template>

<style scoped lang="scss">
.title-bar {
  display: flex;
  align-items: center;
  height: 36px;
  flex-shrink: 0;
  padding-left: 10px;
  background: var(--el-bg-color-overlay);
  border-bottom: 1px solid var(--el-border-color-lighter);
  user-select: none;
}

/* --- 品牌区 --- */
.brand {
  display: flex;
  align-items: center;
  gap: 8px;
  pointer-events: none;
}
.brand-badge {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 20px;
  height: 20px;
  border-radius: 6px;
  font-family: var(--app-font-mono);
  font-size: 12px;
  font-weight: 700;
  color: #fff;
  background: linear-gradient(135deg, #2dd4bf 0%, #0d9488 55%, #0f766e 100%);
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.28);
}
.brand-name {
  font-size: 12px;
  font-weight: 600;
  letter-spacing: 0.5px;
  color: var(--el-text-color-secondary);
}

/* --- 中部拖拽区 --- */
.drag-region {
  flex: 1;
  height: 100%;
}

/* --- 标题栏小按钮（下载 / 关于；位于窗口控制左侧） --- */
.tb-btn {
  position: relative;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 32px;
  height: 26px;
  border: none;
  border-radius: 6px;
  background: transparent;
  color: var(--el-text-color-secondary);
  cursor: pointer;
  outline: none;
  flex-shrink: 0;
  transition: background-color 0.12s ease, color 0.12s ease;
}
.tb-btn .el-icon {
  font-size: 16px;
}
.tb-btn:hover {
  background: var(--el-fill-color);
  color: var(--el-color-primary);
}
.tb-btn:active {
  background: var(--el-fill-color-dark);
}
/* 关于按钮：与窗口控制之间留出间距；下载按钮紧邻其左侧 */
.about-btn {
  margin-right: 10px;
}
.download-btn {
  margin-right: 2px;
}
/* 下载 badge：与图标居中对齐（badge 自带右上偏移，容器改为 inline-flex 防撑高） */
.download-btn :deep(.el-badge) {
  display: inline-flex;
  align-items: center;
  justify-content: center;
}

/* 有新版本时的红点（带底色描边，避免与背景融为一体） */
.update-dot {
  position: absolute;
  top: 1px;
  right: 1px;
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--el-color-danger);
  box-shadow: 0 0 0 2px var(--el-bg-color-overlay);
}
.about-btn.has-update .el-icon {
  color: var(--el-color-primary);
}

/* --- 窗口控制按钮 --- */
.win-controls {
  display: flex;
  align-items: stretch;
  height: 100%;
  flex-shrink: 0;
}
.win-btn {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 46px;
  border: none;
  background: transparent;
  color: var(--el-text-color-secondary);
  cursor: pointer;
  outline: none;
  transition: background-color 0.12s ease, color 0.12s ease;
}
.win-btn .el-icon {
  font-size: 14px;
}
.win-btn:hover {
  background: var(--el-fill-color);
  color: var(--el-text-color-primary);
}
.win-btn:active {
  background: var(--el-fill-color-dark);
}
/* 关闭按钮：Windows 惯例红色警示 */
.win-close:hover {
  background: #e81123;
  color: #fff;
}
.win-close:active {
  background: #c50f1f;
  color: #fff;
}
</style>
