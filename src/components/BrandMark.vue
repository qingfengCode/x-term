<!--
  BrandMark.vue — 统一品牌标识（渐变底 + 白色 X / 图标）

  标题栏 20px、关于对话框 52px、设置「关于」64px、解锁页 56px 共用同一套
  渐变、圆角比例与投影，避免各页面各写一份。
-->
<script setup lang="ts">
import { computed } from "vue";

const props = withDefaults(defineProps<{ size?: number }>(), { size: 20 });

/** 小尺寸用小圆角，大尺寸用大圆角（与历史视觉尺寸对齐）。 */
const radius = computed(() =>
  props.size >= 56 ? 16 : props.size >= 32 ? 14 : 6,
);
/** 字号随尺寸缩放；最小 12px 保证标题栏徽标仍清晰。 */
const fontSize = computed(() => Math.max(12, Math.round(props.size * 0.5)));
</script>

<template>
  <span
    class="brand-mark"
    :class="{ elevated: size >= 32 }"
    :style="{
      width: `${size}px`,
      height: `${size}px`,
      borderRadius: `${radius}px`,
      fontSize: `${fontSize}px`,
    }"
  >
    <slot>X</slot>
  </span>
</template>

<style scoped lang="scss">
.brand-mark {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
  font-family: var(--app-font-mono);
  font-weight: 700;
  line-height: 1;
  color: #fff;
  background: var(--app-brand-gradient);
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.28);
}

/* 关于 / 解锁等大尺寸场景补一层柔和投影 */
.brand-mark.elevated {
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.28),
    0 4px 14px rgba(13, 148, 136, 0.35);
}
</style>
