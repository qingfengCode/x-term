// 启动时立即恢复深色主题：在 CSS 与 JS bundle 加载前同步执行，避免深色主题下
// 先渲染出浅色页面的白屏闪烁。
//
// 单独成文件（而不是 index.html 内联脚本）的原因：生产环境配置了 CSP，
// `script-src 'self'` 会拦截没有 nonce 的内联脚本，而 Tauri 只为外链脚本
// （`script[src^='http']`）注入 nonce。外链文件受 `'self'` 放行，不依赖 nonce。
//
// 缓存由 stores/settings.ts 在 load/setTerminal 时写入（跟随磁盘持久化值），
// 设置页里的"预览"（只切 class 不写 store）不会污染缓存。
try {
  if (localStorage.getItem("xterm-theme") === "dark") {
    document.documentElement.classList.add("dark");
  }
} catch (e) {
  /* localStorage 不可用时忽略，App 加载后仍会按磁盘值校正 */
}
