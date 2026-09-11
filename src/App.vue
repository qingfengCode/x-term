<script setup lang="ts">
import { h, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { ElButton, ElMessageBox, ElNotification } from "element-plus";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useSettingsStore } from "@/stores/settings";
import { useUpdateStore } from "@/stores/update";
import { useTerminalsStore } from "@/stores/terminals";
import { useDbStore } from "@/stores/db";
import TitleBar from "@/components/TitleBar.vue";
import AboutDialog from "@/components/AboutDialog.vue";
import SshAuthPrompt from "@/components/SshAuthPrompt.vue";
import HostKeyPrompt from "@/components/HostKeyPrompt.vue";
import SshManualAuthDialog from "@/components/SshManualAuthDialog.vue";

const settings = useSettingsStore();
const updater = useUpdateStore();

/** 关于对话框（标题栏关于按钮 / 更新通知"查看详情"打开）。 */
const aboutVisible = ref(false);

// --- 关闭确认状态 ---
/** 用户已确认退出（destroy 路径不再弹窗）。 */
let closeConfirmed = false;
/** 关闭事件监听的反订阅函数。 */
let unlistenClose: (() => void) | null = null;

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
  // --- 关闭确认：有活动会话时拦截窗口关闭，弹确认框 ------------------------
  // preventDefault 后窗口保持打开；确认退出走 destroy()（跳过 close-requested
  // 再触发，避免二次弹窗）。无活动会话直接放行，不打扰日常关闭。
  const appWindow = getCurrentWindow();
  unlistenClose = await appWindow.onCloseRequested(async (event) => {
    if (closeConfirmed) return;
    const terminals = useTerminalsStore();
    const db = useDbStore();
    // 终端页签排除已断开的；监控页签自建 SSH 连接，只要开着就占连接
    // （无"已断开"态，全部计入）。
    const activeTerminals = terminals.tabs.filter(
      (t) => t.kind === "terminal" && !t.disconnected,
    ).length;
    const activeMonitors = terminals.tabs.filter((t) => t.kind === "monitor").length;
    const activeDb = db.tabs.length;
    const active = activeTerminals + activeMonitors + activeDb;
    if (active === 0) {
      // 无会话：直接退出（仍走 destroy 以统一退出路径）。
      closeConfirmed = true;
      await appWindow.destroy();
      return;
    }
    event.preventDefault();
    const parts: string[] = [];
    if (activeTerminals > 0) parts.push(`${activeTerminals} 个终端会话`);
    if (activeMonitors > 0) parts.push(`${activeMonitors} 个服务器监控`);
    if (activeDb > 0) parts.push(`${activeDb} 个数据库连接`);
    try {
      await ElMessageBox.confirm(
        `仍有 ${parts.join("与")}在运行，退出将断开全部连接。确定退出吗？`,
        "退出确认",
        { type: "warning", confirmButtonText: "退出", cancelButtonText: "取消" },
      );
      closeConfirmed = true;
      await appWindow.destroy();
    } catch {
      /* 用户取消：窗口保持打开 */
    }
  });

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

  // --- 每天 12:00 定时检查更新 ---
  // 启动补漏：今天已过 12 点且今天还没自动检查过（应用在 12 点后才开始运行），
  // 立即补一次检查；否则等今天的 12 点（或下一个 12 点）。
  const now = new Date();
  const noonToday = new Date(now);
  noonToday.setHours(12, 0, 0, 0);
  if (now.getTime() >= noonToday.getTime() && localStorage.getItem(LAST_CHECK_KEY) !== todayKey()) {
    void autoCheckUpdate();
  }
  scheduleNextNoonCheck();
});

// --- 每天 12:00 定时检查更新 ------------------------------------------------
// 调度方式：setTimeout 到下一个 12:00，触发后检查并重新调度次日；
// localStorage 记录最近一次自动检查日期，避免同一天重复提示（含启动补漏去重）。
const LAST_CHECK_KEY = "xterm.update.lastAutoCheckDate";

/** 今天的日期键（YYYY-M-D，仅用于同日去重，无需补零）。 */
function todayKey(): string {
  const d = new Date();
  return `${d.getFullYear()}-${d.getMonth() + 1}-${d.getDate()}`;
}

/** 自动检查发现新版本时弹出的提示（不自动关闭；可跳转关于对话框或跳过该版本）。 */
function notifyUpdateAvailable(version: string) {
  const n = ElNotification({
    title: "发现新版本",
    message: h("div", { style: "display:flex;flex-direction:column;gap:12px;" }, [
      h("div", `新版本 v${version} 已发布，建议更新到最新版本。`),
      h("div", { style: "display:flex;gap:8px;" }, [
        h(
          ElButton,
          {
            size: "small",
            type: "primary",
            onClick: () => {
              aboutVisible.value = true;
              n.close();
            },
          },
          () => "查看详情",
        ),
        h(
          ElButton,
          {
            size: "small",
            onClick: () => {
              updater.skip();
              n.close();
            },
          },
          () => "跳过此版本",
        ),
      ]),
    ]),
    type: "warning",
    duration: 0,
    position: "bottom-right",
  });
}

/** 执行一次自动检查：尊重"跳过此版本"（被跳过的版本不再提示），失败静默。 */
async function autoCheckUpdate() {
  localStorage.setItem(LAST_CHECK_KEY, todayKey());
  try {
    await updater.check(false);
    if (updater.status === "update-available" && updater.manifest) {
      notifyUpdateAvailable(updater.manifest.version);
    }
  } catch (e) {
    // 自动检查失败不打扰用户（网络不可达等），明天 12 点再试。
    console.warn("定时检查更新失败:", e);
  }
}

let noonCheckTimer: number | null = null;

/** 调度到下一个 12:00（今天未到则为今天，已过则为明天）。 */
function scheduleNextNoonCheck() {
  const now = new Date();
  const next = new Date(now);
  next.setHours(12, 0, 0, 0);
  if (next.getTime() <= now.getTime()) next.setDate(next.getDate() + 1);
  noonCheckTimer = window.setTimeout(async () => {
    await autoCheckUpdate();
    scheduleNextNoonCheck();
  }, next.getTime() - now.getTime());
}

onBeforeUnmount(() => {
  if (noonCheckTimer !== null) window.clearTimeout(noonCheckTimer);
  unlistenClose?.();
});
</script>

<template>
  <!-- 自绘标题栏（系统装饰已关闭），所有路由共用 -->
  <TitleBar @about="aboutVisible = true" />
  <div class="app-body">
    <router-view />
  </div>
  <!-- 关于对话框（标题栏关于按钮 / 12 点更新通知打开） -->
  <AboutDialog v-model:visible="aboutVisible" />
  <!-- SSH 二次认证挑战弹窗（全局监听，任何连接流程触发） -->
  <SshAuthPrompt />
  <!-- SSH 主机公钥变更确认弹窗（全局监听，known_hosts 冲突时触发） -->
  <HostKeyPrompt />
  <!-- SSH 认证失败手动重试弹窗（终端连接失败时收集密码/口令码重试） -->
  <SshManualAuthDialog />
</template>

<style scoped>
/* 标题栏之下的页面容器：占满剩余高度（#app 为纵向 flex，见 main.css） */
.app-body {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
}
</style>
