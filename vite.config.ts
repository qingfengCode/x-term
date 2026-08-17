import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";
import path from "path";

// Tauri 期望前端在固定端口，且开发时通过 iframe 访问
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [vue()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  // Tauri 要求前端以相对路径打包
  base: "./",
  // 开发服务器依赖预打包：esbuild 默认 target 是 chrome87 等旧基线，不支持
  // noVNC 1.7 的 top-level await（browser.js），需与 build.target 对齐为 es2022。
  // （build.target 只作用于生产构建，预打包不吃它）
  optimizeDeps: {
    esbuildOptions: {
      target: "es2022",
    },
  },
  // 开发服务器配置
  clearScreen: false,
  server: {
    host: host || false,
    port: 1622,
    strictPort: true,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1623,
        }
      : undefined,
    watch: {
      // 忽略 rust 目录变化
      ignored: ["**/src-tauri/**"],
    },
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    // es2022：noVNC 1.7 的 feature 检测模块使用了 top-level await（es2021 不支持）。
    // Tauri v2 要求 WebView2 ≥ 109（支持 TLA），桌面端无兼容性问题。
    target: "es2022",
    minify: "esbuild",
    sourcemap: false,
  },
});
