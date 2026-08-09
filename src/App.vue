<script setup lang="ts">
import { onMounted, watch } from "vue";
import { useSettingsStore } from "@/stores/settings";
import SshAuthPrompt from "@/components/SshAuthPrompt.vue";
import HostKeyPrompt from "@/components/HostKeyPrompt.vue";

const settings = useSettingsStore();

/** 应用主题到 documentElement（dark class 驱动 Element Plus 深色变量）。 */
function applyTheme(theme: string) {
  document.documentElement.classList.toggle("dark", theme === "dark");
}

// store 主题变化（设置页点"应用"、load 校正等）全局同步，单一事实源。
watch(
  () => settings.terminal.theme,
  (t) => applyTheme(t),
);

onMounted(async () => {
  try {
    await settings.load();
  } catch (e) {
    // 加载失败不阻断主题应用：按默认（dark）继续，避免页面停留在浅色。
    // （index.html 内联脚本已按上次主题缓存提前恢复，这里再做磁盘值校正。）
    console.error("加载设置失败:", e);
  }
  applyTheme(settings.terminal.theme);
  // vault 解锁门卫由 router 全局守卫负责（先于组件挂载执行），这里不再重复刷新。
});
</script>

<template>
  <router-view />
  <!-- SSH 二次认证挑战弹窗（全局监听，任何连接流程触发） -->
  <SshAuthPrompt />
  <!-- SSH 主机公钥变更确认弹窗（全局监听，known_hosts 冲突时触发） -->
  <HostKeyPrompt />
</template>

<style>
/* App 级样式由 styles/main.css 提供。 */
</style>
