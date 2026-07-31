<!--
  McpView — MCP 服务端管理页（两个独立 MCP：SSH / DB）。

  布局：顶部 Tab 切换 SSH MCP / DB MCP；下方左右分栏——
  左侧为配置卡片（绑定资源 / 地址端口 / token / 启停 / 开关），
  右侧为执行日志实时面板（终端风格，轮询刷新）。

  外部客户端调用时的人工确认由 MainLayout 订阅 mcp:approval_request 经
  McpApprovalToast 浮层呈现（全局事件，与本页无关）。
-->
<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useMcpStore } from "@/stores/mcp";
import McpInstancePanel from "@/components/McpInstancePanel.vue";
import McpLogPanel from "@/components/McpLogPanel.vue";

defineOptions({ name: "McpView" });

const mcp = useMcpStore();
const activeTab = ref<"ssh" | "db">("ssh");

onMounted(async () => {
  await mcp.loadAll();
});
</script>

<template>
  <div class="mcp-view">
    <header class="mcp-header">
      <h2>MCP 服务端</h2>
      <span class="subtitle">
        把 X-Term 的 SSH / MySQL 执行能力，通过标准 MCP 暴露给 Claude Desktop、Cursor 等外部客户端
      </span>
    </header>

    <!-- Tab 切换 kind -->
    <div class="mcp-tabs-wrap">
      <el-tabs v-model="activeTab" class="mcp-tabs">
        <el-tab-pane label="SSH MCP" name="ssh" />
        <el-tab-pane label="DB MCP" name="db" />
      </el-tabs>
    </div>

    <!-- 左右分栏：配置卡片 | 日志输出 -->
    <div class="mcp-columns">
      <div class="mcp-config">
        <el-scrollbar class="config-scroller">
          <McpInstancePanel v-if="activeTab === 'ssh'" kind="ssh" />
          <McpInstancePanel v-else kind="db" />
        </el-scrollbar>
      </div>
      <div class="mcp-log">
        <McpLogPanel v-if="activeTab === 'ssh'" kind="ssh" />
        <McpLogPanel v-else kind="db" />
      </div>
    </div>
  </div>
</template>

<style scoped>
.mcp-view {
  display: flex;
  flex-direction: column;
  height: 100%;
  overflow: hidden;
}

.mcp-header {
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 12px 16px 4px;
  flex-shrink: 0;
}
.mcp-header h2 {
  margin: 0;
  font-size: 16px;
}
.subtitle {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}

.mcp-tabs-wrap {
  padding: 0 16px;
  flex-shrink: 0;
}
.mcp-tabs :deep(.el-tabs__header) {
  margin-bottom: 0;
}

/* 左右分栏 */
.mcp-columns {
  flex: 1;
  min-height: 0;
  display: grid;
  grid-template-columns: minmax(380px, 460px) 1fr;
  gap: 14px;
  padding: 12px 16px 16px;
}
.mcp-config {
  min-height: 0;
  display: flex;
}
.config-scroller {
  flex: 1;
  height: 100%;
}
.mcp-log {
  min-height: 0;
  min-width: 0;
}

/* 窄屏时改为上下堆叠 */
@media (max-width: 960px) {
  .mcp-columns {
    grid-template-columns: 1fr;
    grid-template-rows: auto 1fr;
  }
}
</style>
