<!--
  HelpTip.vue — 问号帮助提示（共享组件）。

  在标签/标题旁渲染一个小问号图标，鼠标悬浮时显示完整说明。
  默认用 content 纯文本，也可用 #content 插槽传富文本（含 <code> 等）。
  说明文字渲染在 body 下的 popper 中（teleport），样式见文末全局块。
-->
<script setup lang="ts">
import { ElIcon, ElTooltip } from "element-plus";
import { QuestionFilled } from "@element-plus/icons-vue";

defineOptions({ name: "HelpTip" });
withDefaults(defineProps<{ content?: string; size?: number }>(), { content: "", size: 14 });
</script>

<template>
  <el-tooltip placement="top" :show-after="120" popper-class="help-tip-popper">
    <template #default>
      <el-icon class="help-icon" :size="size"><QuestionFilled /></el-icon>
    </template>
    <template #content>
      <div class="help-content">
        <slot name="content">{{ content }}</slot>
      </div>
    </template>
  </el-tooltip>
</template>

<style scoped>
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
