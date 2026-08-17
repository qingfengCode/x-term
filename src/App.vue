<script setup lang="ts">
import { onMounted, watch } from "vue";
import { useSettingsStore } from "@/stores/settings";
import SshAuthPrompt from "@/components/SshAuthPrompt.vue";
import HostKeyPrompt from "@/components/HostKeyPrompt.vue";
import SshManualAuthDialog from "@/components/SshManualAuthDialog.vue";

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
  // 设置通常在 main.ts 挂载前已预热加载；这里兜底启动早期的失败场景并重试
  // （IPC/编译竞态偶发失败），避免整个会话一直用默认设置（如桌面连接方式
  // 误退回系统客户端）直到有人重新触发加载。
  for (let attempt = 0; attempt < 3 && !settings.loaded; attempt++) {
    try {
      await settings.load();
    } catch (e) {
      if (attempt < 2) {
        console.warn(`加载设置失败（第 ${attempt + 1} 次），稍后重试:`, e);
        await new Promise((r) => setTimeout(r, 500));
      } else {
        // 重试仍失败按默认值继续，避免页面停留在浅色/不可用。
        // （index.html 内联脚本已按上次主题缓存提前恢复，这里再做磁盘值校正。）
        console.error("加载设置失败:", e);
      }
    }
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
  <!-- SSH 认证失败手动重试弹窗（终端连接失败时收集密码/口令码重试） -->
  <SshManualAuthDialog />
</template>

<style>
/* App 级样式由 styles/main.css 提供。 */
</style>
