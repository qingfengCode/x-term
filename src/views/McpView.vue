<!--
  McpView — MCP 服务端管理页（三个独立 MCP：SSH / DB / File）。

  布局：顶部页头（品牌徽标 + 标题 + 运行总览）；
  下方为卡片式服务选择器（含各服务运行状态与端口）；
  再下方左右分栏——左侧为配置卡片（绑定资源 / 地址端口 / token / 启停 / 开关），
  右侧为执行日志实时面板（终端风格，轮询刷新）。

  外部客户端调用时的人工确认由 MainLayout 订阅 mcp:approval_request 经
  McpApprovalToast 浮层呈现（全局事件，与本页无关）。
-->
<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useMcpStore } from "@/stores/mcp";
import McpInstancePanel from "@/components/McpInstancePanel.vue";
import McpLogPanel from "@/components/McpLogPanel.vue";

defineOptions({ name: "McpView" });

const mcp = useMcpStore();
const activeTab = ref<"ssh" | "db" | "file">("ssh");

/** 服务选择器数据（运行状态 / 端口实时取自 store）。 */
const services = computed(() => [
  {
    kind: "ssh" as const,
    icon: "Monitor",
    title: "SSH MCP",
    desc: "远程命令执行",
    running: mcp.sshStatus.running,
    port: mcp.sshConfig.port,
  },
  {
    kind: "db" as const,
    icon: "Coin",
    title: "DB MCP",
    desc: "SQL 语句执行",
    running: mcp.dbStatus.running,
    port: mcp.dbConfig.port,
  },
  {
    kind: "file" as const,
    icon: "FolderOpened",
    title: "File MCP",
    desc: "对象存储文件",
    running: mcp.fileStatus.running,
    port: mcp.fileConfig.port,
  },
]);

/** 运行中的服务数量（页头总览）。 */
const runningCount = computed(() => services.value.filter((s) => s.running).length);

onMounted(async () => {
  await mcp.loadAll();
});
</script>

<template>
  <div class="mcp-view">
    <!-- 页头 -->
    <header class="mcp-header">
      <div class="header-badge">
        <el-icon><Link /></el-icon>
      </div>
      <div class="header-text">
        <div class="header-title-row">
          <h2>MCP 服务端</h2>
          <span class="running-summary" :class="{ on: runningCount > 0 }">
            <i class="sum-dot" />
            {{ runningCount > 0 ? `${runningCount}/3 运行中` : "全部未启动" }}
          </span>
        </div>
        <span class="subtitle">
          把 X-Term 的 SSH / MySQL / 对象存储能力，通过标准 MCP 暴露给 Claude Desktop、Cursor 等外部客户端
        </span>
      </div>
    </header>

    <!-- 服务选择器：卡片式 Tab，含运行状态与端口 -->
    <div class="service-select">
      <button
        v-for="s in services"
        :key="s.kind"
        type="button"
        class="service-card"
        :class="{ active: activeTab === s.kind, running: s.running }"
        @click="activeTab = s.kind"
      >
        <span class="svc-ico">
          <el-icon><component :is="s.icon" /></el-icon>
        </span>
        <span class="svc-info">
          <span class="svc-title">{{ s.title }}</span>
          <span class="svc-desc">{{ s.desc }}</span>
        </span>
        <span class="svc-side">
          <span class="svc-status" :class="{ on: s.running }">
            <i class="st-dot" />
            {{ s.running ? "运行中" : "已停止" }}
          </span>
          <span class="svc-port">:{{ s.port }}</span>
        </span>
      </button>
    </div>

    <!-- 左右分栏：配置卡片 | 日志输出。用 :key 触发组件按 kind 重建 -->
    <div class="mcp-columns">
      <div class="mcp-config">
        <el-scrollbar class="config-scroller">
          <McpInstancePanel :key="activeTab" :kind="activeTab" />
        </el-scrollbar>
      </div>
      <div class="mcp-log">
        <McpLogPanel :key="activeTab" :kind="activeTab" />
      </div>
    </div>
  </div>
</template>

<style scoped lang="scss">
.mcp-view {
  display: flex;
  flex-direction: column;
  height: 100%;
  overflow: hidden;
}

/* --- 页头 --- */
.mcp-header {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 14px 18px 12px;
  flex-shrink: 0;
  border-bottom: 1px solid var(--el-border-color-lighter);
}
.header-badge {
  width: 38px;
  height: 38px;
  border-radius: 10px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 20px;
  color: #fff;
  flex-shrink: 0;
  background: linear-gradient(135deg, var(--el-color-primary) 0%, var(--el-color-primary-dark-2) 100%);
  box-shadow: 0 2px 8px color-mix(in srgb, var(--el-color-primary) 35%, transparent);
}
.header-text {
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.header-title-row {
  display: flex;
  align-items: center;
  gap: 10px;
}
.mcp-header h2 {
  margin: 0;
  font-size: 16px;
  font-weight: 600;
  letter-spacing: 0.2px;
}
.running-summary {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 11px;
  font-weight: 500;
  padding: 2px 8px;
  border-radius: 10px;
  color: var(--el-text-color-secondary);
  background: var(--el-fill-color);
  border: 1px solid var(--el-border-color-lighter);
  font-family: var(--app-font-mono);
  user-select: none;
}
.running-summary.on {
  color: var(--el-color-success);
  background: var(--el-color-success-light-9);
  border-color: var(--el-color-success-light-7);
}
.sum-dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: currentColor;
}
.running-summary.on .sum-dot {
  animation: sum-pulse 1.8s ease-in-out infinite;
}
@keyframes sum-pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.35; }
}
.subtitle {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* --- 服务选择器卡片 --- */
.service-select {
  display: flex;
  gap: 10px;
  padding: 12px 18px 0;
  flex-shrink: 0;
}
.service-card {
  flex: 1;
  min-width: 0;
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 12px;
  border-radius: 10px;
  border: 1px solid var(--el-border-color-lighter);
  background: var(--el-bg-color-overlay);
  cursor: pointer;
  font-family: inherit;
  text-align: left;
  transition: border-color 0.2s, box-shadow 0.2s, transform 0.15s;
}
.service-card:hover {
  border-color: var(--el-color-primary-light-5);
  box-shadow: 0 2px 10px rgba(0, 0, 0, 0.06);
}
.service-card:active {
  transform: scale(0.99);
}
.service-card.active {
  border-color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
  box-shadow: 0 0 0 1px var(--el-color-primary) inset;
}
.svc-ico {
  width: 32px;
  height: 32px;
  border-radius: 8px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 17px;
  flex-shrink: 0;
  color: var(--el-text-color-secondary);
  background: var(--el-fill-color);
  transition: color 0.2s, background 0.2s;
}
.service-card.active .svc-ico,
.service-card.running .svc-ico {
  color: var(--el-color-primary);
  background: var(--el-color-primary-light-8);
}
.svc-info {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
}
.svc-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--el-text-color-primary);
  line-height: 1.3;
}
.svc-desc {
  font-size: 11px;
  color: var(--el-text-color-secondary);
  line-height: 1.3;
}
.svc-side {
  margin-left: auto;
  display: flex;
  flex-direction: column;
  align-items: flex-end;
  gap: 3px;
  flex-shrink: 0;
}
.svc-status {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-size: 10.5px;
  color: var(--el-text-color-secondary);
  font-family: var(--app-font-mono);
  user-select: none;
}
.svc-status.on {
  color: var(--el-color-success);
}
.st-dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: var(--el-text-color-disabled);
}
.svc-status.on .st-dot {
  background: var(--el-color-success);
  box-shadow: 0 0 5px color-mix(in srgb, var(--el-color-success) 70%, transparent);
  animation: sum-pulse 1.8s ease-in-out infinite;
}
.svc-port {
  font-size: 11px;
  color: var(--el-text-color-secondary);
  font-family: var(--app-font-mono);
  user-select: none;
}

/* --- 左右分栏 --- */
.mcp-columns {
  flex: 1;
  min-height: 0;
  display: grid;
  grid-template-columns: minmax(320px, 380px) 1fr;
  gap: 14px;
  padding: 12px 18px 16px;
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

</style>
