import { createRouter, createWebHashHistory, type RouteRecordRaw } from "vue-router";

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

export default router;
