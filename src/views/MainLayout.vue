<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { ElMessage } from "element-plus";
import { useVaultStore } from "@/stores/vault";
import { useSessionsStore } from "@/stores/sessions";
import { useAiSshStore, useAiDbStore } from "@/stores/ai";
import { useTransferStore } from "@/stores/transfer";
import { useTerminalsStore } from "@/stores/terminals";
import { useUiStore } from "@/stores/ui";
import { useMcpStore } from "@/stores/mcp";
import { useAppShortcuts } from "@/composables/useAppShortcuts";
import { useSettingsStore } from "@/stores/settings";
import SessionSidebar from "@/components/SessionSidebar.vue";
import McpApprovalToast from "@/components/McpApprovalToast.vue";
import type { McpApprovalRequest } from "@/api/mcp";

const route = useRoute();
const router = useRouter();
const vault = useVaultStore();
const sessions = useSessionsStore();
// 两个独立 AI 助手 store：事件双分发，各自只响应自己的 requestId（convForRequest 找不到即静默 return）。
const aiSsh = useAiSshStore();
const aiDb = useAiDbStore();
const transfer = useTransferStore();
const terminalsStore = useTerminalsStore();
const ui = useUiStore();
const settings = useSettingsStore();
// MCP 服务端：外部客户端发来的 exec_ssh/exec_sql 确认请求全局可见，故在 layout 层订阅。
const mcp = useMcpStore();

/** 会话侧栏组件引用（用于 focusSessions 快捷键聚焦搜索框）。 */
const sidebarRef = ref<InstanceType<typeof SessionSidebar> | null>(null);

// 导航项：设置固定底部（不随中间项滚动，矮窗口也始终可见）。
const navItems = [
  { key: "terminals", label: "终端", icon: "Monitor" },
  { key: "sftp", label: "SFTP", icon: "FolderOpened" },
  { key: "files", label: "文件", icon: "Files" },
  { key: "sql", label: "SQL", icon: "Coin" },
  { key: "forward", label: "转发", icon: "Connection" },
  { key: "remote", label: "桌面", icon: "Monitor" },
  { key: "keys", label: "密钥", icon: "Key" },
  { key: "mfa", label: "MFA", icon: "Iphone" },
  { key: "mcp", label: "MCP", icon: "Link" },
];
const navFooterItems = [{ key: "settings", label: "设置", icon: "Setting" }];

const activeNav = computed(() => {
  if (
    route.name === "terminals" ||
    route.name === "sftp" ||
    route.name === "files" ||
    route.name === "sql" ||
    route.name === "forward" ||
    route.name === "remote" ||
    route.name === "keys" ||
    route.name === "mfa" ||
    route.name === "mcp" ||
    route.name === "settings"
  ) {
    return route.name as string;
  }
  return "terminals";
});

function go(key: string) {
  router.push({ name: key });
}

onMounted(async () => {
  // 路由守卫：未解锁则跳到 /unlock。
  if (!vault.unlocked) {
    router.replace("/unlock");
    return;
  }

  // 初始数据加载失败不应阻断事件订阅：否则 DB 出错时整个 AI/传输/MCP 事件
  // 链路全部失效（且错误无人捕获）。逐个容错，失败只记录。
  try {
    await sessions.load();
  } catch (e) {
    console.error("加载会话列表失败:", e);
  }
  try {
    await Promise.all([aiSsh.loadPersisted(), aiDb.loadPersisted()]);
  } catch (e) {
    console.error("加载 AI 会话历史失败:", e);
  }

  // 订阅 AI 流式事件（双分发到 SSH / DB 两个 store）。
  // 每个 store 的 convForRequest(requestId) 只在自己的 requestToCid 里命中，
  // 找不到即静默 return，实现天然隔离路由。
  // 注意：layout 会被卸载（锁定保险库 → /unlock → 重新解锁 → 重挂载），
  // 若不在 onBeforeUnmount 注销监听，每次重挂载都会重复注册 → 事件双倍触发
  // （AI 工具调用会被确认两次、执行两次）。故全部收集进 unlistens。
  const unlistens: UnlistenFn[] = [];
  unlistens.push(
    await listen<{ requestId: string; delta: string }>("ai:chunk", (e) => {
      aiSsh.onChunk(e.payload.requestId, e.payload.delta);
      aiDb.onChunk(e.payload.requestId, e.payload.delta);
    }),
  );
  unlistens.push(
    await listen<{ requestId: string; fullText: string }>("ai:done", (e) => {
      aiSsh.onDone(e.payload.requestId);
      aiDb.onDone(e.payload.requestId);
    }),
  );
  unlistens.push(
    await listen<{ requestId: string; message: string }>("ai:error", (e) => {
      aiSsh.onError(e.payload.requestId, e.payload.message);
      aiDb.onError(e.payload.requestId, e.payload.message);
    }),
  );
  unlistens.push(
    await listen<{ requestId: string }>("ai:stopped", (e) => {
      aiSsh.onStopped(e.payload.requestId);
      aiDb.onStopped(e.payload.requestId);
    }),
  );

  // 订阅 AI 工具调用事件（人确认执行机制；双分发）。
  unlistens.push(
    await listen<{
      requestId: string;
      toolCallId: string;
      name: string;
      arguments: string;
      description: string;
      dangerous: boolean;
      whitelisted: boolean;
      autoApproved: boolean;
    }>("ai:tool_call", (e) => {
      aiSsh.onToolCall(e.payload.requestId, e.payload);
      aiDb.onToolCall(e.payload.requestId, e.payload);
    }),
  );
  unlistens.push(
    await listen<{ requestId: string; toolCallId: string; ok: boolean; output: string }>(
      "ai:tool_result",
      (e) => {
        aiSsh.onToolResult(e.payload.requestId, e.payload);
        aiDb.onToolResult(e.payload.requestId, e.payload);
      }
    ),
  );

  // 订阅传输进度事件。
  unlistens.push(
    await listen<{ taskId: string; transferred: number; total: number; speed: number }>(
      "transfer:progress",
      (e) => {
        transfer.update(e.payload.taskId, {
          transferred: e.payload.transferred,
          total: e.payload.total,
          status: "running",
        });
      }
    ),
  );
  unlistens.push(
    await listen<{ taskId: string }>("transfer:done", (e) => {
      transfer.update(e.payload.taskId, { status: "done" });
    }),
  );
  unlistens.push(
    await listen<{ taskId: string; message: string }>("transfer:error", (e) => {
      transfer.update(e.payload.taskId, { status: "error", message: e.payload.message });
    }),
  );

  // 订阅 MCP 工具调用确认请求（外部 MCP 客户端发起 exec_ssh/exec_sql 时，
  // 后端 emit mcp:approval_request；推入 store，由全局浮层 McpApprovalToast 展示）。
  unlistens.push(
    await listen<McpApprovalRequest>("mcp:approval_request", (e) => {
      mcp.onApprovalRequest(e.payload);
    }),
  );

  // 订阅 MCP 确认请求过期事件（后端超时自动拒绝后 emit），移除对应浮层卡片。
  unlistens.push(
    await listen<{ requestId: string }>("mcp:approval_expired", (e) => {
      mcp.onApprovalExpired(e.payload.requestId);
    }),
  );

  // 全局快捷键（应用级）由 useAppShortcuts 在下面注册。
  onBeforeUnmount(() => {
    for (const u of unlistens) u();
  });
});

// --- 应用级快捷键分发 ----------------------------------------------------
// 仅在终端页生效的动作（closeTab/nextTab/prevTab/newTab）；其他动作（toggleAi/
// focusSessions）在任何页面都生效。copy/paste/search 由终端组件自行处理
// （这里不注册，避免与终端内焦点冲突）。
function switchTab(delta: 1 | -1) {
  const tabs = terminalsStore.tabs;
  if (tabs.length === 0) return;
  const curIdx = tabs.findIndex((t) => t.instanceId === terminalsStore.activeId);
  const nextIdx = (curIdx + delta + tabs.length) % tabs.length;
  const target = tabs[nextIdx];
  if (target?.instanceId) terminalsStore.setActive(target.instanceId);
}

/** 新建终端（Ctrl+T）：快速连接最近一次成功连接的会话。 */
async function openRecentTab() {
  const s = sessions.recentSessions[0];
  if (!s) {
    ElMessage.info("暂无最近会话，请先从左侧会话树连接一台服务器");
    return;
  }
  const msg = ElMessage.info({ message: `正在连接 ${s.name}...`, duration: 0 });
  try {
    await terminalsStore.open(s);
    msg.close();
  } catch (e) {
    msg.close();
    ElMessage.error("连接失败: " + String(e));
  }
}

useAppShortcuts({
  newTab: () => {
    if (activeNav.value !== "terminals") return;
    void openRecentTab();
  },
  closeTab: () => {
    if (activeNav.value !== "terminals") return;
    const id = terminalsStore.activeId;
    if (id) void terminalsStore.close(id);
  },
  nextTab: () => switchTab(1),
  prevTab: () => switchTab(-1),
  toggleAi: () => ui.toggleAi(),
  focusSessions: () => sidebarRef.value?.focusFilter(),
});

// --- 会话侧栏宽度拖拽（写法沿用 SQL 控制台左侧树 sidebar-resizer 惯例） ---
function startSidebarResize(e: MouseEvent) {
  e.preventDefault();
  const startX = e.clientX;
  const startW = settings.sidebarWidth;
  const onMove = (ev: MouseEvent) => {
    const w = startW + (ev.clientX - startX);
    settings.setSidebarWidth(Math.max(180, Math.min(420, w)));
  };
  const onUp = () => {
    document.removeEventListener("mousemove", onMove);
    document.removeEventListener("mouseup", onUp);
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
    void settings.save().catch(() => {});
  };
  document.body.style.cursor = "col-resize";
  document.body.style.userSelect = "none";
  document.addEventListener("mousemove", onMove);
  document.addEventListener("mouseup", onUp);
}

// Ctrl/Cmd + 1..9：切换到第 N 个终端 tab（动态数字快捷键，不纳入可配置列表，
// 作为额外的便捷绑定保留；仅在终端页生效）。
function onNumberKeydown(e: KeyboardEvent) {
  if (activeNav.value !== "terminals") return;
  if (!(e.ctrlKey || e.metaKey) || !/^[1-9]$/.test(e.key)) return;
  const tabs = terminalsStore.tabs;
  const idx = Number(e.key) - 1;
  if (idx < tabs.length && tabs[idx].instanceId) {
    e.preventDefault();
    terminalsStore.setActive(tabs[idx].instanceId);
  }
}
onMounted(() => window.addEventListener("keydown", onNumberKeydown));
onBeforeUnmount(() => window.removeEventListener("keydown", onNumberKeydown));
</script>

<template>
  <div class="main-layout">
    <!-- 左侧导航栏 -->
    <aside class="nav-rail">
      <div class="nav-logo">X</div>
      <!-- 中间导航项：超高时可滚动 -->
      <div class="nav-items">
        <div
          v-for="item in navItems"
          :key="item.key"
          class="nav-item"
          :class="{ active: activeNav === item.key }"
          @click="go(item.key)"
        >
          <el-icon><component :is="item.icon" /></el-icon>
          <span>{{ item.label }}</span>
        </div>
      </div>
      <!-- 底部固定项（设置）：矮窗口也始终可见 -->
      <div
        v-for="item in navFooterItems"
        :key="item.key"
        class="nav-item nav-item-fixed"
        :class="{ active: activeNav === item.key }"
        @click="go(item.key)"
      >
        <el-icon><component :is="item.icon" /></el-icon>
        <span>{{ item.label }}</span>
      </div>
    </aside>

    <!-- 会话侧栏（仅终端页显示） -->
    <SessionSidebar
      v-if="activeNav === 'terminals'"
      ref="sidebarRef"
      :style="{ width: settings.sidebarWidth + 'px' }"
    />
    <!-- 侧栏拖拽分隔条 -->
    <div
      v-if="activeNav === 'terminals'"
      class="sidebar-resizer"
      @mousedown="startSidebarResize"
    />

    <!-- 主内容 -->
    <main class="main-content">
      <!-- KeepAlive 缓存终端页/SQL 页，保留 AI 助手面板的滚动位置/输入草稿；
           store 是单例，状态本就常驻，流式中途切走再切回仍能继续接收。 -->
      <router-view v-slot="{ Component }">
        <KeepAlive :include="['Workspace', 'SqlConsoleView']">
          <component :is="Component" />
        </KeepAlive>
      </router-view>
    </main>

    <!-- MCP 工具调用确认浮层（外部客户端发起 exec_ssh/exec_sql 时弹出，全局可见） -->
    <McpApprovalToast />
  </div>
</template>

<style scoped>
.main-layout {
  display: flex;
  flex-direction: row;
  width: 100%;
  height: 100%;
  overflow: hidden;
  background: var(--el-bg-color);
}
.nav-rail {
  width: 60px;
  background: var(--el-bg-color-overlay);
  border-right: 1px solid var(--el-border-color-lighter);
  display: flex;
  flex-direction: column;
  align-items: center;
  padding: 8px 0 0;
}
/* 中间导航项区：超高（矮窗口）时可滚动，避免底部项被裁切 */
.nav-items {
  flex: 1;
  min-height: 0;
  width: 100%;
  display: flex;
  flex-direction: column;
  align-items: center;
  overflow-y: auto;
  scrollbar-width: thin;
}
/* 窄栏用细滚动条，避免挤占 60px 宽度 */
.nav-items::-webkit-scrollbar {
  width: 4px;
}
.nav-items::-webkit-scrollbar-thumb {
  background: var(--el-border-color);
  border-radius: 2px;
}
.nav-items::-webkit-scrollbar-track {
  background: transparent;
}
/* 底部固定项（设置）：不与中间项一起滚动，矮窗口始终可见 */
.nav-item-fixed {
  flex-shrink: 0;
  width: 100%;
  border-top: 1px solid var(--el-border-color-lighter);
  border-radius: 0;
  padding-top: 12px;
  margin-bottom: 0;
}
.nav-logo {
  font-size: 22px;
  font-weight: bold;
  color: #2563eb;
  margin: 4px 0 12px;
}
.nav-item {
  width: 48px;
  padding: 10px 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 2px;
  cursor: pointer;
  border-radius: 6px;
  font-size: 11px;
  color: var(--el-text-color-regular);
  margin-bottom: 2px;
}
.nav-item:hover {
  background: var(--el-fill-color-light);
}
.nav-item.active {
  background: var(--el-color-primary-light-9);
  color: var(--el-color-primary);
}
.nav-item .el-icon {
  font-size: 18px;
}
.main-content {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  display: flex;
  flex-direction: column;
}
/* 会话侧栏拖拽分隔条 */
.sidebar-resizer {
  width: 4px;
  flex-shrink: 0;
  cursor: col-resize;
  background: var(--el-border-color-lighter);
  transition: background 0.15s;
}
.sidebar-resizer:hover {
  background: var(--el-color-primary);
}
</style>
