<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { ElMessage } from "element-plus";
import { useVaultStore } from "@/stores/vault";
import { useSessionsStore } from "@/stores/sessions";
import { useAiSshStore, useAiDbStore, useAiDesktopStore } from "@/stores/ai";
import { useTransferStore } from "@/stores/transfer";
import { useTerminalsStore } from "@/stores/terminals";
import { useUiStore } from "@/stores/ui";
import { useMcpStore } from "@/stores/mcp";
import { useAppShortcuts, isEditableTarget } from "@/composables/useAppShortcuts";
import { useSettingsStore } from "@/stores/settings";
import SessionSidebar from "@/components/SessionSidebar.vue";
import McpApprovalToast from "@/components/McpApprovalToast.vue";
import type { McpApprovalRequest } from "@/api/mcp";

const route = useRoute();
const router = useRouter();
const vault = useVaultStore();
const sessions = useSessionsStore();
// 三个独立 AI 助手 store：事件多分发，各自只响应自己的 requestId（convForRequest 找不到即静默 return）。
const aiSsh = useAiSshStore();
const aiDb = useAiDbStore();
const aiDesktop = useAiDesktopStore();
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
  { key: "sql", label: "DB", icon: "Coin" },
  { key: "forward", label: "转发", icon: "Connection" },
  { key: "remote", label: "桌面", icon: "Platform" },
  { key: "keys", label: "密钥", icon: "Key" },
  { key: "mfa", label: "MFA", icon: "Iphone" },
  { key: "mcp", label: "MCP", icon: "Link" },
];
const navFooterItems = [{ key: "settings", label: "设置", icon: "Setting" }];

/** 是否显示锁定按钮：保险库已创建且已解锁，且设置中开启了锁定功能。 */
const showLock = computed(() => vault.exists && vault.unlocked && settings.vaultLockEnabled);

/**
 * 锁定保险库：清除内存主密钥后跳到解锁页。
 * 温和锁定——不断开已建立的连接（终端/SFTP/隧道/MySQL 继续工作），
 * 解锁后重新进入主界面（MainLayout 重挂载，见 onMounted 的 gate 逻辑）。
 */
async function onLock() {
  try {
    await vault.lock();
    router.replace("/unlock");
  } catch (e) {
    ElMessage.error("锁定失败：" + String(e));
  }
}

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

// --- 全局下载列表（ZMODEM sz / SFTP 下载记录） --------------------------------
// 呼出按钮与抽屉在 TitleBar（标题栏「关于」左侧），本组件只负责进度事件的
// store 更新（见下方 transfer:* 事件订阅）。

// --- AI / 传输 / MCP 事件订阅的注销句柄 ----------------------------------
// 必须在 setup 顶层（onMounted 之外）声明并注册 onBeforeUnmount：
// onMounted 传入的 async 回调在首个 await 之后失去组件实例上下文，此时才
// 注册的 onBeforeUnmount 会被 Vue 静默丢弃（不报错、不生效），导致每次
// 重挂载都重复注册监听（AI 工具调用会被确认两次、执行两次）。
const unlistens: UnlistenFn[] = [];
let unmounted = false;
onBeforeUnmount(() => {
  unmounted = true;
  for (const u of unlistens) u();
});

onMounted(async () => {
  // vault 解锁门卫由 router 全局守卫负责（先于本组件挂载执行），
  // 能挂载到这里说明已解锁，无需再判断/重定向。

  // 初始数据加载失败不应阻断事件订阅：否则 DB 出错时整个 AI/传输/MCP 事件
  // 链路全部失效（且错误无人捕获）。逐个容错，失败只记录。
  try {
    await sessions.load();
  } catch (e) {
    console.error("加载会话列表失败:", e);
  }
  try {
    await Promise.all([aiSsh.loadPersisted(), aiDb.loadPersisted(), aiDesktop.loadPersisted()]);
  } catch (e) {
    console.error("加载 AI 会话历史失败:", e);
  }

  // 订阅 AI 流式事件（多分发到 SSH / DB / desktop 三个 store）。
  // 每个 store 的 convForRequest(requestId) 只在自己的 requestToCid 里命中，
  // 找不到即静默 return，实现天然隔离路由。
  // unlistens / onBeforeUnmount 已在 setup 顶层声明（见上方 onMounted 之前），
  // 这里直接使用；track 配合 unmounted 标志：若组件在某个 await listen 尚未
  // resolve 时就被卸载（挂载后立刻锁定跳 /unlock），后续才 resolve 的监听必须
  // 在注册后立即注销；任一 listen reject 也只影响该事件、不阻断其余订阅。
  /** 注册一个事件监听：resolve 时组件已卸载则立即注销，注册失败单独容错。 */
  const track = async (un: Promise<UnlistenFn>) => {
    try {
      const fn = await un;
      if (unmounted) fn();
      else unlistens.push(fn);
    } catch (e) {
      console.error("事件订阅失败:", e);
    }
  };

  await track(
    listen<{ requestId: string; delta: string }>("ai:chunk", (e) => {
      aiSsh.onChunk(e.payload.requestId, e.payload.delta);
      aiDb.onChunk(e.payload.requestId, e.payload.delta);
      aiDesktop.onChunk(e.payload.requestId, e.payload.delta);
    }),
  );
  await track(
    listen<{ requestId: string; fullText: string }>("ai:done", (e) => {
      aiSsh.onDone(e.payload.requestId);
      aiDb.onDone(e.payload.requestId);
      aiDesktop.onDone(e.payload.requestId);
    }),
  );
  await track(
    listen<{ requestId: string; message: string }>("ai:error", (e) => {
      aiSsh.onError(e.payload.requestId, e.payload.message);
      aiDb.onError(e.payload.requestId, e.payload.message);
      aiDesktop.onError(e.payload.requestId, e.payload.message);
    }),
  );
  await track(
    listen<{ requestId: string }>("ai:stopped", (e) => {
      aiSsh.onStopped(e.payload.requestId);
      aiDb.onStopped(e.payload.requestId);
      aiDesktop.onStopped(e.payload.requestId);
    }),
  );

  // 订阅 AI 工具调用事件（人确认执行机制；多分发）。
  await track(
    listen<{
      requestId: string;
      toolCallId: string;
      name: string;
      arguments: string;
      description: string;
      dangerous: boolean;
      whitelisted: boolean;
      autoApproved: boolean;
      desktopId?: string | null;
    }>("ai:tool_call", (e) => {
      aiSsh.onToolCall(e.payload.requestId, e.payload);
      aiDb.onToolCall(e.payload.requestId, e.payload);
      aiDesktop.onToolCall(e.payload.requestId, e.payload);
    }),
  );
  await track(
    listen<{ requestId: string; toolCallId: string; ok: boolean; output: string }>(
      "ai:tool_result",
      (e) => {
        aiSsh.onToolResult(e.payload.requestId, e.payload);
        aiDb.onToolResult(e.payload.requestId, e.payload);
        aiDesktop.onToolResult(e.payload.requestId, e.payload);
      }
    ),
  );
  // 订阅 AI 任务清单更新事件（todo_write 工具；多分发）。
  await track(
    listen<{
      requestId: string;
      todos: { content: string; status: string }[];
    }>("ai:todo", (e) => {
      const todos = e.payload.todos as {
        content: string;
        status: "pending" | "in_progress" | "completed";
      }[];
      aiSsh.onTodo(e.payload.requestId, todos);
      aiDb.onTodo(e.payload.requestId, todos);
      aiDesktop.onTodo(e.payload.requestId, todos);
    }),
  );
  // 订阅 AI token 用量事件（单次请求用量；前端按会话累计展示，多分发）。
  await track(
    listen<{ requestId: string; promptTokens: number; completionTokens: number }>(
      "ai:usage",
      (e) => {
        aiSsh.onUsage(e.payload.requestId, e.payload.promptTokens, e.payload.completionTokens);
        aiDb.onUsage(e.payload.requestId, e.payload.promptTokens, e.payload.completionTokens);
        aiDesktop.onUsage(e.payload.requestId, e.payload.promptTokens, e.payload.completionTokens);
      }
    ),
  );
  // 订阅自动重试事件（可重试错误后端退避重试时恢复会话状态，多分发）。
  await track(
    listen<{ requestId: string; attempt: number; maxAttempts: number; reason: string }>(
      "ai:retrying",
      (e) => {
        aiSsh.onRetrying(e.payload.requestId);
        aiDb.onRetrying(e.payload.requestId);
        aiDesktop.onRetrying(e.payload.requestId);
      }
    ),
  );
  // 订阅编排层系统提示（重复调用守卫提醒等；前端灰色提示渲染，多分发）。
  await track(
    listen<{ requestId: string; text: string }>("ai:system_note", (e) => {
      aiSsh.onSystemNote(e.payload.requestId, e.payload.text);
      aiDb.onSystemNote(e.payload.requestId, e.payload.text);
      aiDesktop.onSystemNote(e.payload.requestId, e.payload.text);
    }),
  );

  // 订阅传输进度事件。
  await track(
    listen<{ taskId: string; transferred: number; total: number; speed: number }>(
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
  await track(
    listen<{ taskId: string }>("transfer:done", (e) => {
      transfer.update(e.payload.taskId, { status: "done" });
    }),
  );
  await track(
    listen<{ taskId: string; message: string }>("transfer:error", (e) => {
      transfer.update(e.payload.taskId, { status: "error", message: e.payload.message });
    }),
  );

  // 订阅 MCP 工具调用确认请求（外部 MCP 客户端发起 exec_ssh/exec_sql 时，
  // 后端 emit mcp:approval_request；推入 store，由全局浮层 McpApprovalToast 展示）。
  await track(
    listen<McpApprovalRequest>("mcp:approval_request", (e) => {
      mcp.onApprovalRequest(e.payload);
    }),
  );

  // 订阅 MCP 确认请求过期事件（后端超时自动拒绝后 emit），移除对应浮层卡片。
  await track(
    listen<{ requestId: string }>("mcp:approval_expired", (e) => {
      mcp.onApprovalExpired(e.payload.requestId);
    }),
  );

  // 全局快捷键（应用级）由 useAppShortcuts 在下面注册。
});

// --- 应用级快捷键分发 ----------------------------------------------------
// 仅在终端页生效的动作（closeTab/nextTab/prevTab/newTab）；其他动作（toggleAi/
// focusSessions）在任何页面都生效。copy/paste/search 由终端组件自行处理
// （这里不注册，避免与终端内焦点冲突）。
function switchTab(delta: 1 | -1) {
  // 与 newTab/closeTab 一致：仅终端页生效，避免在 SQL/设置等页面按快捷键
  // 静默切换后台终端 tab。
  if (activeNav.value !== "terminals") return;
  const tabs = terminalsStore.tabs;
  if (tabs.length === 0) return;
  const curIdx = tabs.findIndex((t) => t.id === terminalsStore.activeTabId);
  const nextIdx = (curIdx + delta + tabs.length) % tabs.length;
  const target = tabs[nextIdx];
  if (target) terminalsStore.setActive(target.id);
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
    const id = terminalsStore.activeTabId;
    if (id) void terminalsStore.close(id);
  },
  nextTab: () => switchTab(1),
  prevTab: () => switchTab(-1),
  toggleAi: () => ui.toggleAi(),
  focusSessions: () => sidebarRef.value?.focusFilter(),
});

// --- 会话侧栏宽度拖拽（写法沿用 SQL 控制台左侧树 sidebar-resizer 惯例） ---
/** 切换会话侧栏展开/收起（默认展开；状态持久化，重启后保持）。 */
function toggleSidebar() {
  settings.toggleSidebar();
  void settings.save().catch(() => {});
}

function startSidebarResize(e: MouseEvent) {
  e.preventDefault();
  const startX = e.clientX;
  const startW = settings.sidebarWidth;
  const onMove = (ev: MouseEvent) => {
    const w = startW + (ev.clientX - startX);
    settings.setSidebarWidth(Math.max(180, Math.min(420, w)));
  };
  const cleanup = () => {
    document.removeEventListener("mousemove", onMove);
    document.removeEventListener("mouseup", onUp);
    window.removeEventListener("blur", onBlur);
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
    void settings.save().catch(() => {});
  };
  const onUp = () => cleanup();
  // 鼠标在窗口外释放（拖出窗口边缘 / Alt-Tab 切走）时 mouseup 不触发，
  // 监听器与 col-resize 光标、userSelect:none 会永久残留——用 window blur 兜底清理。
  const onBlur = () => cleanup();
  document.body.style.cursor = "col-resize";
  document.body.style.userSelect = "none";
  document.addEventListener("mousemove", onMove);
  document.addEventListener("mouseup", onUp);
  window.addEventListener("blur", onBlur);
}

// Ctrl/Cmd + 1..9：切换到第 N 个终端 tab（动态数字快捷键，不纳入可配置列表，
// 作为额外的便捷绑定保留；仅在终端页生效）。
function onNumberKeydown(e: KeyboardEvent) {
  if (activeNav.value !== "terminals") return;
  // 与 useAppShortcuts 一致：焦点在输入框/搜索框等可编辑元素时不抢占（避免
  // 在 AI 输入框按 Ctrl+1 时切走后台终端 tab）。
  if (isEditableTarget(e)) return;
  if (!(e.ctrlKey || e.metaKey) || !/^[1-9]$/.test(e.key)) return;
  const tabs = terminalsStore.tabs;
  const idx = Number(e.key) - 1;
  if (idx < tabs.length) {
    e.preventDefault();
    terminalsStore.setActive(tabs[idx].id);
  }
}
onMounted(() => window.addEventListener("keydown", onNumberKeydown));
onBeforeUnmount(() => window.removeEventListener("keydown", onNumberKeydown));
</script>

<template>
  <div class="main-layout">
    <!-- 左侧导航栏 -->
    <aside class="nav-rail">
      <!-- 中间导航项：超高时可滚动 -->
      <div class="nav-items" role="navigation" aria-label="主导航">
        <div
          v-for="item in navItems"
          :key="item.key"
          class="nav-item"
          :class="{ active: activeNav === item.key }"
          role="button"
          tabindex="0"
          :aria-current="activeNav === item.key ? 'page' : undefined"
          @click="go(item.key)"
          @keydown.enter.prevent="go(item.key)"
        >
          <el-icon><component :is="item.icon" /></el-icon>
          <span>{{ item.label }}</span>
        </div>
      </div>
      <!-- 底部固定项（设置/锁定）：矮窗口也始终可见 -->
      <div
        v-if="showLock"
        class="nav-item nav-item-fixed"
        title="锁定保险库（需重新输入主密码）"
        @click="onLock"
      >
        <el-icon><component :is="'Lock'" /></el-icon>
        <span>锁定</span>
      </div>
      <div
        v-for="item in navFooterItems"
        :key="item.key"
        class="nav-item nav-item-fixed"
        :class="{ active: activeNav === item.key }"
        role="button"
        tabindex="0"
        :aria-current="activeNav === item.key ? 'page' : undefined"
        @click="go(item.key)"
        @keydown.enter.prevent="go(item.key)"
      >
        <el-icon><component :is="item.icon" /></el-icon>
        <span>{{ item.label }}</span>
      </div>
    </aside>

    <!-- 会话侧栏（仅终端页显示）：支持展开/收起，默认展开。收起时不占布局空间 -->
    <SessionSidebar
      v-if="activeNav === 'terminals' && !settings.sidebarCollapsed"
      ref="sidebarRef"
      :style="{ width: settings.sidebarWidth + 'px' }"
      @collapse="toggleSidebar"
    />
    <!-- 侧栏拖拽分隔条 -->
    <div
      v-if="activeNav === 'terminals' && !settings.sidebarCollapsed"
      class="sidebar-resizer"
      @mousedown="startSidebarResize"
    />

    <!-- 主内容 -->
    <main class="main-content">
      <!-- 收起状态：展开按钮悬浮在内容区左上角，不占布局空间 -->
      <button
        v-if="activeNav === 'terminals' && settings.sidebarCollapsed"
        class="sidebar-expand-btn floating"
        title="展开侧栏"
        @click="toggleSidebar"
      >
        <el-icon><DArrowRight /></el-icon>
      </button>
      <!-- KeepAlive 缓存终端页/SQL 页/桌面页：保留 AI 助手面板的滚动位置/输入草稿，
           且桌面页已连接的 VNC/RDP 会话在切走再切回时不中断（store 单例常驻 + 组件不卸载）。 -->
      <router-view v-slot="{ Component }">
        <KeepAlive :include="['Workspace', 'SqlConsoleView', 'RemoteDesktopView']">
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
  width: 64px;
  background: var(--el-bg-color-overlay);
  border-right: 1px solid var(--el-border-color-lighter);
  display: flex;
  flex-direction: column;
  align-items: center;
  padding: 10px 0 0;
  /* 兜底滚动：页面缩放极大（Ctrl+滚轮/触摸板捏合）时视口 CSS 高度被除以缩放
     值数，导航项+底部固定项本身可能超出栏高，被 main-layout 的 overflow:hidden
     裁掉且无处可滚（设置图标"永久消失"）。整栏允许滚动后：空间充足时中间区
     flex:1 内部滚动、固定项仍贴底（与原行为一致）；空间极度不足时整栏可滚到
     底部，固定项始终可达。 */
  overflow-y: auto;
  scrollbar-width: thin;
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
/* 窄栏用细滚动条，避免挤占栏宽（nav-rail 与 nav-items 一致） */
.nav-rail::-webkit-scrollbar,
.nav-items::-webkit-scrollbar {
  width: 4px;
}
.nav-rail::-webkit-scrollbar-thumb,
.nav-items::-webkit-scrollbar-thumb {
  background: var(--el-border-color);
  border-radius: 2px;
}
.nav-rail::-webkit-scrollbar-track,
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
.nav-item {
  position: relative;
  width: 52px;
  padding: 8px 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 3px;
  cursor: pointer;
  border-radius: 8px;
  font-size: 11px;
  color: var(--el-text-color-secondary);
  margin-bottom: 2px;
  transition: background-color 0.15s ease, color 0.15s ease;
}
/* 下载呼出按钮（badge 与图标对齐）已移至 TitleBar，样式随之迁移。 */
.nav-item:hover {
  background: var(--el-fill-color-light);
  color: var(--el-text-color-primary);
}
.nav-item.active {
  background: var(--el-color-primary-light-9);
  color: var(--el-color-primary);
  font-weight: 600;
}
/* 激活项左侧指示条（VS Code 风格） */
.nav-item.active::before {
  content: "";
  position: absolute;
  left: 0;
  top: 50%;
  transform: translateY(-50%);
  width: 3px;
  height: 18px;
  border-radius: 2px;
  background: var(--el-color-primary);
}
/* 键盘焦点环（click 由背景反馈，focus-visible 用内描边） */
.nav-item:focus-visible {
  outline: none;
  box-shadow: inset 0 0 0 2px var(--el-color-primary-light-5);
}
.nav-item .el-icon {
  font-size: 19px;
  transition: color 0.15s ease, transform 0.1s ease;
}
/* 按下微缩反馈 */
.nav-item:active .el-icon {
  transform: scale(0.92);
}
.main-content {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  display: flex;
  flex-direction: column;
  /* 锚定收起状态下悬浮的展开按钮 */
  position: relative;
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
/* 展开按钮（收起状态）：悬浮在内容区左上角，不占布局空间 */
.sidebar-expand-btn.floating {
  position: absolute;
  left: 8px;
  top: 6px;
  z-index: 30;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 24px;
  height: 24px;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  background: var(--el-bg-color-overlay);
  color: var(--el-text-color-secondary);
  cursor: pointer;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.1);
  transition: background-color 0.15s ease, color 0.15s ease, border-color 0.15s ease;
}
.sidebar-expand-btn.floating:hover {
  color: var(--el-color-primary);
  border-color: var(--el-color-primary-light-5);
  background: var(--el-color-primary-light-9);
}
</style>
