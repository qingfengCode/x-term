<script setup lang="ts">
import { onMounted } from "vue";
import { useSettingsStore } from "@/stores/settings";
import { useVaultStore } from "@/stores/vault";
import SshAuthPrompt from "@/components/SshAuthPrompt.vue";
import HostKeyPrompt from "@/components/HostKeyPrompt.vue";

const settings = useSettingsStore();
const vault = useVaultStore();

onMounted(async () => {
  await settings.load();
  // 应用主题。
  document.documentElement.classList.toggle("dark", settings.terminal.theme === "dark");
  // 检查保险库状态（首次启动需要创建，否则需要解锁）。
  await vault.refresh();
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
