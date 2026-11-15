import { createApp } from "vue";
import { createPinia } from "pinia";
import ElementPlus from "element-plus";
import "element-plus/dist/index.css";
import "element-plus/theme-chalk/dark/css-vars.css";
import * as ElementPlusIconsVue from "@element-plus/icons-vue";

import App from "./App.vue";
import router from "./router";
import "./styles/main.css";
import { useSettingsStore } from "@/stores/settings";
import { openExternalUrl } from "@/api/system";
import { info, warn, error, debug } from "@tauri-apps/plugin-log";

// 把 webview console 转发到后端日志（Rust log → dev 终端），WebView2 里无法直接
// 观测 console。注意 plugin-log 的 attachConsole 是「Rust 日志 → console」的反向，
// 不能自动转发 webview console，需手动挂钩。正常路径几乎不产生 console 调用，
// 仅错误/警告时走一次 IPC，开销可忽略；排障时后端日志可见前端错误。
{
  const send = (level: "info" | "warn" | "error" | "debug", args: unknown[]) => {
    const msg = args
      .map((a) =>
        a instanceof Error
          ? a.stack ?? String(a)
          : typeof a === "object"
            ? (() => {
                try {
                  return JSON.stringify(a);
                } catch {
                  return String(a);
                }
              })()
            : String(a),
      )
      .join(" ");
    const fn = level === "warn" ? warn : level === "error" ? error : level === "debug" ? debug : info;
    void fn(msg).catch(() => {});
  };
  const hook = (name: "log" | "info" | "warn" | "error" | "debug", level: "info" | "warn" | "error" | "debug") => {
    const orig = console[name].bind(console);
    console[name] = (...args: unknown[]) => {
      orig(...args);
      send(level, args);
    };
  };
  hook("log", "info");
  hook("info", "info");
  hook("warn", "warn");
  hook("error", "error");
  hook("debug", "debug");
}

// 全局兜底：拦截所有 `<a href>` 的点击，改走系统默认浏览器。
//
// Tauri webview 中链接默认在应用内导航——点一下整个窗口就被外部网页替换，
// 未保存状态全丢（AI 回复里的 markdown 链接、帮助文档等都可能触发）。用
// 捕获阶段拦在所有组件之前，避免各组件各写一份。Rust 侧还有导航护栏兜底。
document.addEventListener(
  "click",
  (e) => {
    const anchor = (e.target as HTMLElement | null)?.closest?.("a[href]") as
      | HTMLAnchorElement
      | null;
    if (!anchor) return;
    const href = anchor.getAttribute("href") ?? "";
    // 站内路由链接（#/xxx 或相对路径）保持默认行为。
    if (!/^(https?:|mailto:)/i.test(href)) return;
    e.preventDefault();
    e.stopPropagation();
    void openExternalUrl(href).catch((err) => {
      void warn(`打开外部链接失败: ${String(err)}`).catch(() => {});
    });
  },
  true,
);

const app = createApp(App);

// Vue 未处理错误（如原生事件处理器抛错）转发到后端日志。
// Vue warn 本身不带异常对象，errorHandler 才能拿到真实错误。
app.config.errorHandler = (err, _instance, info) => {
  const msg = err instanceof Error ? (err.stack ?? err.message) : String(err);
  void error(`[vue:errorHandler] ${msg}（${info ?? ""}）`).catch(() => {});
};

// 注册所有 Element Plus 图标。
for (const [key, component] of Object.entries(ElementPlusIconsVue)) {
  app.component(key, component as never);
}

app.use(createPinia());
app.use(router);
app.use(ElementPlus);

// 挂载前预热加载设置：避免页面已可交互但设置仍为默认值的窗口期（启动早期
// Vite 编译 / IPC 初始化竞态），否则桌面连接方式等会误用默认值直到有人重试。
// 失败不阻塞渲染：App.vue 有重试兜底，使用方（如桌面连接）也有未加载补齐。
const settings = useSettingsStore();
await settings.load().catch(() => {});

app.mount("#app");
