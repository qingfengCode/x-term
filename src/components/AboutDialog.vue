<!--
  AboutDialog.vue — 「关于」对话框
  由顶部标题栏的关于按钮 / 每日 12 点更新检查通知打开，
  内容与设置页「关于」Tab 一致：品牌区 + 检查更新卡片 + 技术信息，
  共享 update store（任何入口触发的检查状态全局同步）。
-->
<script setup lang="ts">
import { watch } from "vue";
import { ElMessageBox } from "element-plus";
import { Refresh } from "@element-plus/icons-vue";
import { useUpdateStore } from "@/stores/update";

const visible = defineModel<boolean>("visible", { default: false });
const updater = useUpdateStore();

/** 字节数格式化为人类可读单位（升级包可能超过 1TB，缺 TB 会显示成 1536.0 GB）。 */
function formatBytes(n: number): string {
  if (!n) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}

/** 安装前二次确认（会退出应用）。 */
async function confirmInstall() {
  try {
    await ElMessageBox.confirm(
      "安装将退出当前应用并启动安装程序，未保存的会话将断开。继续？",
      "安装确认",
      { type: "warning", confirmButtonText: "安装并重启", cancelButtonText: "取消" },
    );
  } catch {
    return;
  }
  void updater.install();
}

// 打开时按需加载应用信息（当前版本 / 数据目录等）；检查状态由共享 store 维护，
// 定时检查发现新版本后点"查看详情"打开本对话框即直接显示更新卡片。
watch(visible, (v) => {
  if (v && !updater.info) void updater.loadInfo();
});
</script>

<template>
  <el-dialog
    v-model="visible"
    title="关于"
    width="480px"
    align-center
    :close-on-click-modal="true"
    append-to-body
  >
    <div class="about-body">
      <!-- 品牌区 -->
      <div class="about-brand">
        <div class="about-logo">X</div>
        <div class="about-name">X-Term</div>
        <div class="about-slogan">一站式运维工作站</div>
        <div class="about-version">当前版本 v{{ updater.info?.currentVersion ?? "—" }}</div>
      </div>

      <!-- 检查更新 -->
      <div class="about-section">
        <div class="about-section-title">检查更新</div>

        <!-- 空闲 -->
        <div v-if="updater.status === 'idle'" class="update-body">
          <el-button type="primary" :icon="Refresh" @click="updater.check()">检查更新</el-button>
          <span v-if="updater.skippedVersion" class="update-hint">
            已跳过 v{{ updater.skippedVersion }}，点击检查将重新提示
          </span>
        </div>

        <!-- 检查中 -->
        <div v-else-if="updater.status === 'checking'" class="update-body">
          <el-icon class="is-loading"><Refresh /></el-icon>
          <span>正在检查更新…</span>
        </div>

        <!-- 已是最新 -->
        <div v-else-if="updater.status === 'up-to-date'" class="update-body">
          <el-tag type="success" effect="light">✓ 当前已是最新版本</el-tag>
          <el-button link @click="updater.reset()">返回</el-button>
        </div>

        <!-- 发现新版本 -->
        <div v-else-if="updater.status === 'update-available'" class="update-body column">
          <div class="new-ver">
            <el-tag type="warning" effect="dark">发现新版本 v{{ updater.manifest?.version }}</el-tag>
          </div>
          <pre v-if="updater.manifest?.notes" class="update-notes">{{ updater.manifest.notes }}</pre>
          <div class="update-actions">
            <el-button type="primary" @click="updater.download()">立即更新</el-button>
            <el-button @click="updater.skip()">跳过此版本</el-button>
          </div>
        </div>

        <!-- 下载中 -->
        <div v-else-if="updater.status === 'downloading'" class="update-body column">
          <el-progress
            :percentage="updater.progress.percent"
            :stroke-width="14"
            :format="(p: number) => `${p}%`"
          />
          <div class="dl-meta">
            {{ formatBytes(updater.progress.received) }}
            <template v-if="updater.progress.total"> / {{ formatBytes(updater.progress.total) }}</template>
          </div>
        </div>

        <!-- 下载完成 -->
        <div v-else-if="updater.status === 'downloaded'" class="update-body column">
          <el-tag type="success" effect="light">✓ 下载完成</el-tag>
          <div class="update-actions">
            <el-button type="primary" @click="confirmInstall">立即安装并重启</el-button>
          </div>
        </div>

        <!-- 出错 -->
        <div v-else-if="updater.status === 'error'" class="update-body column">
          <el-alert :title="updater.error ?? '更新失败'" type="error" :closable="false" show-icon />
          <div class="update-actions">
            <el-button @click="updater.reset()">返回</el-button>
            <el-button type="primary" @click="updater.check()">重试</el-button>
          </div>
        </div>
      </div>

      <!-- 技术信息 -->
      <div class="about-section">
        <div class="about-section-title">技术信息</div>
        <div class="about-meta">
          <span class="k">平台</span><span class="v">Windows x64</span>
          <span class="k">数据目录</span><span class="v">{{ updater.info?.dataDir ?? "—" }}</span>
          <span class="k">项目地址</span>
          <span class="v">
            <a
              class="about-link"
              href="https://github.com/qingfengCode/x-term"
              target="_blank"
              rel="noopener"
            >
              github.com/qingfengCode/x-term
            </a>
          </span>
        </div>
      </div>
    </div>
  </el-dialog>
</template>

<style scoped>
.about-body {
  display: flex;
  flex-direction: column;
  gap: 18px;
}

/* --- 品牌区 --- */
.about-brand {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  padding: 8px 0 4px;
}
.about-logo {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 52px;
  height: 52px;
  border-radius: 14px;
  font-family: var(--app-font-mono);
  font-size: 26px;
  font-weight: 700;
  color: #fff;
  background: linear-gradient(135deg, #2dd4bf 0%, #0d9488 55%, #0f766e 100%);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.28),
    0 4px 14px rgba(13, 148, 136, 0.35);
  margin-bottom: 6px;
}
.about-name {
  font-size: 17px;
  font-weight: 700;
  color: var(--el-text-color-primary);
}
.about-slogan {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.about-version {
  margin-top: 6px;
  font-size: 12px;
  font-family: var(--app-font-mono);
  padding: 2px 10px;
  border-radius: 10px;
  background: var(--el-fill-color-light);
  color: var(--el-text-color-secondary);
}

/* --- 分区卡片 --- */
.about-section {
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 10px;
  padding: 12px 14px;
}
.about-section-title {
  font-size: 12px;
  font-weight: 600;
  letter-spacing: 1px;
  color: var(--el-text-color-secondary);
  margin-bottom: 10px;
}

/* --- 检查更新各状态 --- */
.update-body {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}
.update-body.column {
  flex-direction: column;
  align-items: stretch;
  gap: 12px;
}
.update-hint {
  font-size: 12px;
  color: var(--el-text-color-placeholder);
}
.update-actions {
  display: flex;
  gap: 8px;
}
.update-actions .el-button + .el-button {
  margin-left: 0;
}
.new-ver {
  display: flex;
}
.update-notes {
  margin: 0;
  padding: 8px 10px;
  max-height: 160px;
  overflow: auto;
  font-family: var(--app-font-mono);
  font-size: 12px;
  line-height: 1.6;
  white-space: pre-wrap;
  word-break: break-word;
  color: var(--el-text-color-regular);
  background: var(--el-fill-color-light);
  border-radius: 6px;
}
.dl-meta {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  text-align: center;
}

/* --- 技术信息 --- */
.about-meta {
  display: grid;
  grid-template-columns: 64px 1fr;
  row-gap: 8px;
  column-gap: 10px;
  font-size: 12px;
}
.about-meta .k {
  color: var(--el-text-color-secondary);
}
.about-meta .v {
  color: var(--el-text-color-primary);
  word-break: break-all;
}
.about-link {
  color: var(--el-color-primary);
  text-decoration: none;
}
.about-link:hover {
  text-decoration: underline;
}
</style>
