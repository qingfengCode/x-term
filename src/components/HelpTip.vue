<!--
  HelpTip.vue — 帮助提示（共享组件）。

  两种用法：
  1. 传默认插槽：悬浮在插槽内容（标签/标题文字）上时显示提示——
     <HelpTip content="说明">SSH 空闲断开</HelpTip>
  2. 不传默认插槽：在旁边渲染一个小问号图标，悬浮图标显示提示（旧行为，兼容存量用法）。

  说明文字用 content 纯文本，或用 #content 插槽传富文本（含 <code> 等）。
  说明渲染在 body 下的 popper 中（teleport），样式见文末全局块。
-->
<script setup lang="ts">
import { ElIcon, ElTooltip } from "element-plus";
import { QuestionFilled } from "@element-plus/icons-vue";

defineOptions({ name: "HelpTip" });
withDefaults(defineProps<{ content?: string; size?: number }>(), { content: "", size: 14 });
</script>

<template>
  <el-tooltip
    placement="top"
    :show-after="1000"
    :hide-after="0"
    popper-class="help-tip-popper"
  >
    <template #default>
      <span class="help-trigger">
        <slot>
          <el-icon class="help-icon" :size="size"><QuestionFilled /></el-icon>
        </slot>
      </span>
    </template>
    <template #content>
      <div class="help-content">
        <slot name="content">{{ content }}</slot>
      </div>
    </template>
  </el-tooltip>
</template>

<style scoped>
/* 悬浮触发容器：内容即触发区，不再需要问号图标 */
.help-trigger {
  cursor: help;
}

/* 问号图标（未提供默认插槽时的回退形态） */
.help-icon {
  cursor: help;
  color: var(--el-text-color-secondary);
  vertical-align: -2px;
  margin-left: 2px;
  transition: color 0.15s;
}
.help-icon:hover {
  color: var(--el-color-primary);
}
</style>

<!-- popper 渲染在 body 下，须为非 scoped 全局样式 -->
<style>
.help-tip-popper {
  max-width: 340px;
  font-size: 12px;
  line-height: 1.6;
}
.help-tip-popper code {
  background: var(--el-fill-color-light);
  /* el-tooltip 默认 effect=dark：popper 文字恒为白色。若不给 code 显式设色，
     浅色主题下会变成"浅灰底 + 白字"（继承白色），几乎不可读且不随主题变化。
     这里显式用主题变量，深/浅主题下都保持深字浅底 / 浅字深底。 */
  color: var(--el-text-color-regular);
  padding: 1px 4px;
  border-radius: 3px;
  font-size: 11px;
}
</style>
