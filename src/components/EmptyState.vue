<!--
  EmptyState.vue — 统一空状态

  图标 / 标题 / 说明 / 操作的层级与间距集中在这里，
  避免各页面分别写 28 / 32 / 40 / 48px 图标和三套字号。
-->
<script setup lang="ts">
withDefaults(
  defineProps<{
    title: string;
    description?: string;
    /** Element Plus 图标组件名（全局注册）。 */
    icon?: string;
    size?: "sm" | "md" | "lg";
  }>(),
  { icon: "InfoFilled", size: "md" },
);
</script>

<template>
  <div class="empty-state" :class="`empty-state--${size}`">
    <span class="empty-state-icon">
      <el-icon><component :is="icon" /></el-icon>
    </span>
    <p class="empty-state-title">{{ title }}</p>
    <p v-if="description" class="empty-state-desc">{{ description }}</p>
    <div v-if="$slots.default" class="empty-state-action">
      <slot />
    </div>
  </div>
</template>

<style scoped lang="scss">
.empty-state {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  text-align: center;
  color: var(--el-text-color-secondary);
}

.empty-state-icon {
  display: inline-flex;
  color: var(--el-text-color-placeholder);
}

.empty-state-icon .el-icon {
  font-size: var(--empty-icon-size, 36px);
}

.empty-state-title {
  margin: 0;
  font-size: var(--empty-title-size, 13px);
  font-weight: 500;
  line-height: 1.5;
  color: var(--el-text-color-regular);
}

.empty-state-desc {
  margin: 2px 0 0;
  max-width: 360px;
  font-size: var(--empty-desc-size, 12px);
  line-height: 1.6;
  color: var(--el-text-color-secondary);
}

.empty-state-action {
  margin-top: 4px;
}

.empty-state--sm {
  --empty-icon-size: 28px;
  --empty-title-size: 12px;
  --empty-desc-size: 11px;
  gap: 4px;
  padding: 24px 16px;
}

.empty-state--md {
  --empty-icon-size: 36px;
  --empty-title-size: 13px;
  --empty-desc-size: 12px;
  gap: 6px;
  padding: 32px 20px;
}

.empty-state--lg {
  --empty-icon-size: 44px;
  --empty-title-size: 14px;
  --empty-desc-size: 12px;
  gap: 8px;
  padding: 48px 24px;
}

.empty-state--lg .empty-state-action {
  margin-top: 8px;
}
</style>
