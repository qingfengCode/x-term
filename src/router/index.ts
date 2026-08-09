import { createRouter, createWebHashHistory, type RouteRecordRaw } from "vue-router";
import { useVaultStore } from "@/stores/vault";

const routes: RouteRecordRaw[] = [
  {
    path: "/",
    name: "main",
    component: () => import("@/views/MainLayout.vue"),
    children: [
      { path: "", redirect: "/terminals" },
      { path: "terminals", name: "terminals", component: () => import("@/views/Workspace.vue") },
      { path: "sftp", name: "sftp", component: () => import("@/views/SftpView.vue") },
      { path: "files", name: "files", component: () => import("@/views/FileExplorerView.vue") },
      { path: "sql", name: "sql", component: () => import("@/views/SqlConsoleView.vue") },
      { path: "forward", name: "forward", component: () => import("@/views/ForwardView.vue") },
      { path: "remote", name: "remote", component: () => import("@/views/RemoteDesktopView.vue") },
      { path: "keys", name: "keys", component: () => import("@/views/KeyManagerView.vue") },
      { path: "mfa", name: "mfa", component: () => import("@/views/MfaView.vue") },
      { path: "mcp", name: "mcp", component: () => import("@/views/McpView.vue") },
      { path: "settings", name: "settings", component: () => import("@/views/Settings.vue") },
    ],
  },
  {
    path: "/unlock",
    name: "unlock",
    component: () => import("@/views/UnlockView.vue"),
  },
];

const router = createRouter({
  history: createWebHashHistory(),
  routes,
});

/**
 * vault 解锁门卫（全局守卫，先于组件挂载执行）：
 * - 首次导航时刷新一次 vault 状态（冷启动 store 里 unlocked 是初始 false，
 *   后端进程可能仍处于解锁状态，需要拉一次真实状态；之后锁定/解锁都由
 *   store 的 lock()/unlock() 同步维护，无需再刷）；
 * - 锁定状态下访问任何主路由 → 重定向 /unlock；
 * - 已解锁时访问 /unlock → 重定向回终端页。
 *
 * 把门卫放在路由层而非 MainLayout onMounted：后者在子组件挂载之后才执行，
 * 锁定态下子页面会先渲染出完整 UI 并触发副作用（读凭据报错、闪屏）再被跳走。
 */
let vaultChecked = false;
router.beforeEach(async (to) => {
  const vault = useVaultStore();
  if (!vaultChecked) {
    vaultChecked = true;
    try {
      await vault.refresh();
    } catch {
      // refresh 失败按未解锁处理（unlocked 保持 false），落到 /unlock，安全侧。
    }
  }
  if (to.name === "unlock") {
    return vault.unlocked ? { name: "terminals" } : true;
  }
  if (!vault.unlocked) {
    return { name: "unlock" };
  }
  return true;
});

export default router;
