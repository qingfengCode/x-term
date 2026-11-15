<!--
  AboutPanel.vue — 关于内容（品牌 / 检查更新 / 技术信息）

  设置页「关于」Tab 与标题栏「关于」对话框共用同一份内容与更新逻辑，
  仅通过 variant 调整品牌尺寸、卡片间距和表面样式。
-->
<script setup lang="ts">
import { ElMessageBox } from "element-plus";
import { useUpdateStore } from "@/stores/update";
import { formatSize } from "@/utils/format";
import BrandMark from "@/components/BrandMark.vue";

withDefaults(defineProps<{ variant?: "dialog" | "page" }>(), { variant: "dialog" });

const updater = useUpdateStore();

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
</script>

<template>
  <div class="about-panel" :class="`is-${variant}`">
    <!-- 品牌区 -->
    <div class="about-brand">
      <BrandMark :size="variant === 'page' ? 64 : 52" class="about-logo" />
      <div class="about-name">X-Term</div>
      <div class="about-slogan">一站式运维工作站</div>
      <div class="about-version">当前版本 v{{ updater.info?.currentVersion ?? "—" }}</div>
    </div>

    <!-- 检查更新 -->
    <section class="about-section">
      <div class="about-section-title">检查更新</div>

      <div v-if="updater.status === 'idle'" class="about-update-body">
        <el-button type="primary" :icon="'Refresh'" @click="updater.check()">检查更新</el-button>
        <span v-if="updater.skippedVersion" class="about-hint">
          已跳过 v{{ updater.skippedVersion }}，点击检查将重新提示
        </span>
      </div>

      <div v-else-if="updater.status === 'checking'" class="about-update-body">
        <el-icon class="is-loading"><Refresh /></el-icon>
        <span>正在检查更新…</span>
      </div>

      <div v-else-if="updater.status === 'up-to-date'" class="about-update-body">
        <el-tag type="success" effect="light">✓ 当前已是最新版本</el-tag>
        <el-button link @click="updater.reset()">返回</el-button>
      </div>

      <div v-else-if="updater.status === 'update-available'" class="about-update-body column">
        <div class="about-new-ver">
          <el-tag type="warning" effect="dark">
            发现新版本 v{{ updater.manifest?.version }}
          </el-tag>
        </div>
        <pre v-if="updater.manifest?.notes" class="about-notes">{{ updater.manifest.notes }}</pre>
        <div class="about-actions">
          <el-button type="primary" @click="updater.download()">立即更新</el-button>
          <el-button @click="updater.skip()">跳过此版本</el-button>
        </div>
      </div>

      <div v-else-if="updater.status === 'downloading'" class="about-update-body column">
        <el-progress
          :percentage="updater.progress.percent"
          :stroke-width="14"
          :format="(p: number) => `${p}%`"
        />
        <div class="about-dl-meta">
          {{ formatSize(updater.progress.received) }}
          <template v-if="updater.progress.total">
            / {{ formatSize(updater.progress.total) }}
          </template>
        </div>
      </div>

      <div v-else-if="updater.status === 'downloaded'" class="about-update-body column">
        <el-tag type="success" effect="light">✓ 下载完成</el-tag>
        <div class="about-actions">
          <el-button type="primary" @click="confirmInstall">立即安装并重启</el-button>
        </div>
      </div>

      <div v-else-if="updater.status === 'error'" class="about-update-body column">
        <el-alert :title="updater.error ?? '更新失败'" type="error" :closable="false" show-icon />
        <div class="about-actions">
          <el-button @click="updater.reset()">返回</el-button>
          <el-button type="primary" @click="updater.check()">重试</el-button>
        </div>
      </div>
    </section>

    <!-- 技术信息 -->
    <section class="about-section">
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
            https://github.com/qingfengCode/x-term
          </a>
        </span>
      </div>
    </section>
  </div>
</template>

<style scoped lang="scss">
.about-panel {
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
  padding: 2px 10px;
  border-radius: var(--app-radius-lg);
  font-family: var(--app-font-mono);
  font-size: 12px;
  color: var(--el-text-color-secondary);
  background: var(--el-fill-color-light);
}

/* --- 分区卡片 --- */
.about-section {
  border: 1px solid var(--el-border-color-lighter);
  border-radius: var(--app-radius-lg);
  padding: 12px 14px;
}
.about-section-title {
  margin-bottom: 10px;
  font-size: 12px;
  font-weight: 600;
  letter-spacing: 1px;
  color: var(--el-text-color-secondary);
}

/* --- 检查更新 --- */
.about-update-body {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}
.about-update-body.column {
  flex-direction: column;
  align-items: stretch;
  gap: 12px;
}
.about-hint {
  font-size: 12px;
  color: var(--el-text-color-placeholder);
}
.about-actions {
  display: flex;
  gap: 8px;
}
.about-actions .el-button + .el-button {
  margin-left: 0;
}
.about-new-ver {
  display: flex;
}
.about-notes {
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
  border-radius: var(--app-radius-sm);
}
.about-dl-meta {
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

/* --- 页面变体（设置 → 关于）：品牌更大、分区是浮层卡片 --- */
.is-page {
  gap: 16px;
}
.is-page .about-brand {
  padding: 24px 0 28px;
}
.is-page .about-logo {
  margin-bottom: 8px;
}
.is-page .about-name {
  font-size: 22px;
}
.is-page .about-slogan {
  font-size: 13px;
}
.is-page .about-version {
  margin-top: 4px;
  color: var(--el-text-color-placeholder);
}
.is-page .about-section {
  background: var(--el-bg-color-overlay);
  padding: 18px 22px 20px;
  box-shadow: var(--app-shadow-1);
}
.is-page .about-section-title {
  margin-bottom: 14px;
  font-size: var(--app-font-size-md);
  font-weight: 600;
  letter-spacing: normal;
  color: var(--el-text-color-primary);
}
.is-page .about-meta {
  grid-template-columns: 90px 1fr;
  column-gap: 12px;
  font-size: 13px;
}
</style>
