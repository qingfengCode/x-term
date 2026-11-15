<!--
  PageHeader.vue — 内容页统一页头

  标题字号、图标徽标、副标题、右侧动作与底部分隔线集中在这里，
  避免转发 / 密钥 / MFA / MCP / 设置各写一套。

  用法：
    <PageHeader title="端口转发" subtitle="..." icon="Connection">
      <template #actions><el-button ... /></template>
    </PageHeader>
-->
<script setup lang="ts">
withDefaults(
  defineProps<{
    title: string;
    subtitle?: string;
    /** Element Plus 图标组件名（全局注册）。 */
    icon?: string;
    /** 图标使用品牌渐变底（MCP 等服务型页面）。 */
    gradient?: boolean;
    /** 副标题与标题同一行（设置 / MFA 等紧凑页头）。 */
    inlineSubtitle?: boolean;
    /** 页头左右留白，默认 20px。 */
    gutter?: number;
  }>(),
  { gradient: false, inlineSubtitle: false, gutter: 20 },
);
</script>

<template>
  <header class="page-header" :style="{ '--page-header-gutter': `${gutter}px` }">
    <div class="page-header-main">
      <span v-if="icon" class="page-header-icon" :class="{ gradient }">
        <el-icon><component :is="icon" /></el-icon>
      </span>
      <div class="page-header-text">
        <div class="page-header-title-row">
          <h1 class="page-header-title">{{ title }}</h1>
          <slot name="meta" />
          <span v-if="inlineSubtitle && subtitle" class="page-header-inline-sub">
            {{ subtitle }}
          </span>
        </div>
        <p v-if="subtitle && !inlineSubtitle" class="page-header-sub">{{ subtitle }}</p>
      </div>
    </div>
    <div v-if="$slots.actions" class="page-header-actions">
      <slot name="actions" />
    </div>
  </header>
</template>

<style scoped lang="scss">
.page-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  flex-wrap: wrap;
  gap: 8px 16px;
  flex-shrink: 0;
  padding: 10px var(--page-header-gutter, 20px) 11px;
  border-bottom: 1px solid var(--el-border-color-lighter);
}

.page-header-main {
  display: flex;
  align-items: center;
  gap: 10px;
  flex: 1 1 auto;
  min-width: 0;
}

.page-header-icon {
  width: 28px;
  height: 28px;
  flex-shrink: 0;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border-radius: var(--app-radius-md);
  font-size: 15px;
  color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
}

/* 服务型页面（MCP）使用品牌渐变底，与普通页面的浅色图标徽标区分；
   使用固定的品牌渐变，避免暗色主题下 primary → dark-2 变成“浅青到更浅青” */
.page-header-icon.gradient {
  color: #fff;
  background: var(--app-brand-gradient);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.28),
    0 2px 8px rgba(13, 148, 136, 0.3);
}

.page-header-text {
  min-width: 0;
}

.page-header-title-row {
  display: flex;
  align-items: center;
  gap: 10px;
  min-width: 0;
}

.page-header-title {
  margin: 0;
  min-width: 0;
  font-size: var(--app-font-size-title);
  font-weight: 600;
  line-height: 1.3;
  letter-spacing: 0.2px;
  color: var(--el-text-color-primary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.page-header-sub {
  margin: 2px 0 0;
  font-size: var(--app-font-size-sm);
  line-height: 1.4;
  color: var(--el-text-color-secondary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* 行内副标题：标题右侧竖线分隔（设置 / MFA）；可收缩，窄窗口不顶出动作区 */
.page-header-inline-sub {
  flex-shrink: 1;
  min-width: 0;
  padding-left: 12px;
  border-left: 1px solid var(--el-border-color-lighter);
  font-size: var(--app-font-size-sm);
  color: var(--el-text-color-placeholder);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.page-header-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
  margin-left: auto;
}
</style>
