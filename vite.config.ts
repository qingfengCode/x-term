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
    target: "es2021",
    minify: "esbuild",
    sourcemap: false,
  },
});
