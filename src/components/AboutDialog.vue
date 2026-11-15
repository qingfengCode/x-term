<!--
  AboutDialog.vue — 「关于」对话框
  由顶部标题栏的关于按钮 / 每日 12 点更新检查通知打开，
  内容与设置页「关于」Tab 共用 AboutPanel，update store 全局同步。
-->
<script setup lang="ts">
import { watch } from "vue";
import { useUpdateStore } from "@/stores/update";
import AboutPanel from "@/components/AboutPanel.vue";

const visible = defineModel<boolean>("visible", { default: false });
const updater = useUpdateStore();

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
    <AboutPanel />
  </el-dialog>
</template>
