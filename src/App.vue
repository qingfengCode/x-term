<script setup lang="ts">
import { onMounted } from "vue";
import { useSettingsStore } from "@/stores/settings";
import { useVaultStore } from "@/stores/vault";

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
</template>

<style>
/* App 级样式由 styles/main.css 提供。 */
</style>
