<!--
  StatusDot.vue — 统一状态圆点

  页签连接态、桌面“已打开”、MCP 运行态、监控 LIVE 等场景共用同一套尺寸、
  语义色和脉冲动画，避免每个模块各写一份 6/7/10px 圆点。
-->
<script setup lang="ts">
withDefaults(
  defineProps<{
    tone?: "success" | "warning" | "danger" | "info" | "primary";
    size?: number;
    pulse?: boolean;
  }>(),
  { tone: "success", size: 6, pulse: false },
);
</script>

<template>
  <span
    class="status-dot"
    :class="[tone, { pulse }]"
    :style="{ width: `${size}px`, height: `${size}px` }"
  />
</template>

<style scoped lang="scss">
.status-dot {
  display: inline-block;
  flex-shrink: 0;
  border-radius: 50%;
  background: currentColor;
}

.success {
  color: var(--el-color-success);
}
.warning {
  color: var(--el-color-warning);
}
.danger {
  color: var(--el-color-danger);
}
.info {
  color: var(--el-text-color-placeholder);
}
.primary {
  color: var(--el-color-primary);
}

.pulse {
  animation: status-dot-pulse 1.8s ease-in-out infinite;
}

@keyframes status-dot-pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.35;
  }
}
</style>
