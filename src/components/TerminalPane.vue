<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from "vue";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { SearchAddon } from "@xterm/addon-search";
import { WebglAddon } from "@xterm/addon-webgl";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import * as terminalApi from "@/api/terminal";
import { useSettingsStore } from "@/stores/settings";
import { base64ToBytes, bytesToBase64 } from "@/utils/binary";
import "@xterm/xterm/css/xterm.css";

const props = defineProps<{ instanceId: string }>();
const emit = defineEmits<{ (e: "closed"): void }>();
const settings = useSettingsStore();

const containerRef = ref<HTMLElement | null>(null);
let term: Terminal | null = null;
let fitAddon: FitAddon | null = null;
let searchAddon: SearchAddon | null = null;
let unlistens: UnlistenFn[] = [];
let resizeObs: ResizeObserver | null = null;

// --- 右键菜单 ---
const menuVisible = ref(false);
const menuX = ref(0);
const menuY = ref(0);

// --- 搜索浮层 ---
const searchOpen = ref(false);
const searchKeyword = ref("");
const searchMatchInfo = ref(""); // 如 "3/12"

function buildOptions() {
  const t = settings.terminal;
  return {
    fontFamily: t.fontFamily,
    fontSize: t.fontSize,
    lineHeight: t.lineHeight,
    scrollback: t.scrollback,
    cursorBlink: true,
    theme: t.theme === "dark" ? DARK_THEME : LIGHT_THEME,
    allowProposedApi: true,
  };
}

const DARK_THEME = {
  background: "#1e1e2e",
  foreground: "#cdd6f4",
  cursor: "#f5e0dc",
  selectionBackground: "#585b7088",
};
const LIGHT_THEME = {
  background: "#ffffff",
  foreground: "#1e1e2e",
  cursor: "#1e1e2e",
  selectionBackground: "#c0caf588",
};

// --- 剪贴板（webview2 支持 navigator.clipboard；失败静默降级） ---
async function copyText(text: string) {
  try {
    await navigator.clipboard?.writeText(text);
  } catch {
    /* webview 未授权剪贴板则忽略 */
  }
}
async function readClipboard(): Promise<string> {
  try {
    return (await navigator.clipboard?.readText()) ?? "";
  } catch {
    return "";
  }
}

onMounted(async () => {
  if (!containerRef.value) return;
  term = new Terminal(buildOptions());
  fitAddon = new FitAddon();
  searchAddon = new SearchAddon();
  term.loadAddon(fitAddon);
  term.loadAddon(new WebLinksAddon());
  term.loadAddon(searchAddon);
  if (settings.terminal.enableWebgl) {
    try {
      term.loadAddon(new WebglAddon());
    } catch {
      /* WebGL 不可用时回退到 canvas */
    }
  }
  term.open(containerRef.value);
  fitAddon.fit();

  // 监听后端推送的终端数据：按 instanceId 过滤。
  const un1 = await listen<{ sessionId: string; data: string }>("terminal:data", (e) => {
    if (e.payload.sessionId !== props.instanceId) return;
    const bytes = base64ToBytes(e.payload.data);
    term?.write(bytes);
  });
  unlistens.push(un1);

  // 连接断开：终端提示 + 通知父组件（显示重连按钮）。
  const un2 = await listen<{ sessionId: string }>("terminal:closed", (e) => {
    if (e.payload.sessionId !== props.instanceId) return;
    term?.write("\r\n\x1b[31m[连接已断开]\x1b[0m\r\n");
    emit("closed");
  });
  unlistens.push(un2);

  // 用户键盘输入 → 后端。
  term.onData((data) => {
    const b64 = bytesToBase64(new TextEncoder().encode(data));
    terminalApi.terminalWrite(props.instanceId, b64).catch(() => {
      /* 写入失败通常是连接已断 */
    });
  });

  // copyOnSelect：选中即复制（修死设置 bug）。读 settings.terminal.copyOnSelect。
  if (settings.terminal.copyOnSelect) {
    term.onSelectionChange(() => {
      const sel = term?.getSelection();
      if (sel) void copyText(sel);
    });
  }

  // 尺寸变化：浏览器 resize + 容器变化。
  term.onResize(({ cols, rows }) => {
    terminalApi.terminalResize(props.instanceId, cols, rows).catch(() => {});
  });

  resizeObs = new ResizeObserver(() => {
    try {
      fitAddon?.fit();
    } catch {
      /* 容器隐藏时 fit 会抛错，忽略 */
    }
  });
  resizeObs.observe(containerRef.value);

  // 初始尺寸同步给后端。
  terminalApi
    .terminalResize(props.instanceId, term.cols, term.rows)
    .catch(() => {});

  // 搜索结果计数。
  searchAddon?.onDidChangeResults((ev) => {
    const { resultIndex, resultCount } = ev;
    searchMatchInfo.value =
      resultCount > 0 ? `${resultIndex + 1}/${resultCount}` : "无匹配";
  });

  // 全局快捷键：Ctrl+F 搜索（仅本实例激活时，避免多 tab 冲突——下文用 windowKeydownTarget 判断焦点）。
  window.addEventListener("keydown", onGlobalKeydown);
});

// Ctrl+F 打开搜索；Esc 关闭。
function onGlobalKeydown(e: KeyboardEvent) {
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "f") {
    // 仅当本终端面板可见（父容器有尺寸）时响应。
    const el = containerRef.value;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return;
    e.preventDefault();
    openSearch();
  }
  if (e.key === "Escape" && searchOpen.value) {
    closeSearch();
  }
}

// 设置变化时重建主题。
watch(
  () => settings.terminal,
  (t) => {
    if (term) {
      term.options.fontFamily = t.fontFamily;
      term.options.fontSize = t.fontSize;
      term.options.lineHeight = t.lineHeight;
      term.options.theme = t.theme === "dark" ? DARK_THEME : LIGHT_THEME;
      try {
        fitAddon?.fit();
      } catch {
        /* ignore */
      }
    }
  },
  { deep: true }
);

onBeforeUnmount(() => {
  for (const u of unlistens) u();
  unlistens = [];
  resizeObs?.disconnect();
  window.removeEventListener("keydown", onGlobalKeydown);
  term?.dispose();
  term = null;
});

// --- 右键菜单处理 ---
function onContextMenu(e: MouseEvent) {
  e.preventDefault();
  menuX.value = e.clientX;
  menuY.value = e.clientY;
  menuVisible.value = true;
}
function closeMenu() {
  menuVisible.value = false;
}
async function menuCopy() {
  const sel = term?.getSelection() ?? "";
  if (sel) await copyText(sel);
  closeMenu();
}
async function menuPaste() {
  const text = await readClipboard();
  if (text) {
    const b64 = bytesToBase64(new TextEncoder().encode(text));
    terminalApi.terminalWrite(props.instanceId, b64).catch(() => {});
  }
  closeMenu();
}
function menuSelectAll() {
  term?.selectAll();
  closeMenu();
}
function menuClear() {
  term?.clear();
  closeMenu();
}
function menuSearch() {
  closeMenu();
  openSearch();
}

// --- 搜索浮层 ---
function openSearch() {
  searchOpen.value = true;
  // 用当前选中文本预填。
  const sel = term?.getSelection();
  if (sel) searchKeyword.value = sel;
  setTimeout(() => {
    searchInputRef.value?.focus();
    searchInputRef.value?.select();
  }, 0);
}
function closeSearch() {
  searchOpen.value = false;
  searchAddon?.clearDecorations();
  searchMatchInfo.value = "";
  term?.focus();
}
function runSearch(dir: "next" | "prev") {
  const kw = searchKeyword.value;
  if (!kw || !searchAddon) return;
  if (dir === "next") searchAddon.findNext(kw);
  else searchAddon.findPrevious(kw);
}
const searchInputRef = ref<HTMLInputElement | null>(null);

defineExpose({
  search: (keyword: string) => searchAddon?.findNext(keyword),
  findNext: (keyword: string) => searchAddon?.findNext(keyword),
  findPrevious: (keyword: string) => searchAddon?.findPrevious(keyword),
  clearSearch: () => searchAddon?.clearDecorations(),
  clear: () => term?.clear(),
  focus: () => term?.focus(),
  /**
   * 向终端发送一条命令（自动追加换行）。
   * 用于快捷命令按钮 / 快捷键触发。
   */
  sendCommand: (command: string) => {
    if (!command) return;
    const b64 = bytesToBase64(new TextEncoder().encode(command + "\r"));
    terminalApi.terminalWrite(props.instanceId, b64).catch(() => {
      /* 连接已断 */
    });
    term?.focus();
  },
});
</script>

<template>
  <div class="xterm-wrap">
    <div
      ref="containerRef"
      class="xterm-pane"
      @contextmenu.prevent="onContextMenu"
      @click="closeMenu"
    />

    <!-- 搜索浮层 -->
    <div v-if="searchOpen" class="term-search">
      <input
        ref="searchInputRef"
        v-model="searchKeyword"
        class="term-search-input"
        placeholder="搜索..."
        spellcheck="false"
        @keydown.enter.prevent="runSearch('next')"
        @keydown.esc.prevent="closeSearch"
      />
      <button class="term-search-btn" title="上一个" @click="runSearch('prev')">↑</button>
      <button class="term-search-btn" title="下一个" @click="runSearch('next')">↓</button>
      <span class="term-search-info">{{ searchMatchInfo }}</span>
      <button class="term-search-close" title="关闭 (Esc)" @click="closeSearch">×</button>
    </div>

    <!-- 右键菜单 -->
    <div
      v-if="menuVisible"
      class="term-menu"
      :style="{ left: menuX + 'px', top: menuY + 'px' }"
      @click.stop
    >
      <div class="term-menu-item" @click="menuCopy">复制</div>
      <div class="term-menu-item" @click="menuPaste">粘贴</div>
      <div class="term-menu-item" @click="menuSelectAll">全选</div>
      <div class="term-menu-sep" />
      <div class="term-menu-item" @click="menuClear">清屏</div>
      <div class="term-menu-item" @click="menuSearch">搜索 (Ctrl+F)</div>
    </div>
  </div>
</template>

<style scoped>
.xterm-wrap {
  position: relative;
  width: 100%;
  height: 100%;
}
.xterm-pane {
  width: 100%;
  height: 100%;
}

/* 搜索浮层 */
.term-search {
  position: absolute;
  top: 8px;
  right: 16px;
  display: flex;
  align-items: center;
  gap: 4px;
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color);
  border-radius: 6px;
  padding: 4px 6px;
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.15);
  z-index: 10;
}
.term-search-input {
  width: 160px;
  border: none;
  outline: none;
  background: transparent;
  color: var(--el-text-color-primary);
  font-size: 13px;
}
.term-search-btn,
.term-search-close {
  border: none;
  background: transparent;
  color: var(--el-text-color-secondary);
  cursor: pointer;
  padding: 2px 6px;
  border-radius: 3px;
  font-size: 13px;
  line-height: 1;
}
.term-search-btn:hover,
.term-search-close:hover {
  background: var(--el-fill-color);
  color: var(--el-color-primary);
}
.term-search-info {
  font-size: 11px;
  color: var(--el-text-color-secondary);
  min-width: 40px;
  text-align: center;
}

/* 右键菜单 */
.term-menu {
  position: fixed;
  min-width: 140px;
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color);
  border-radius: 6px;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.2);
  padding: 4px 0;
  z-index: 100;
}
.term-menu-item {
  padding: 6px 14px;
  font-size: 13px;
  color: var(--el-text-color-primary);
  cursor: pointer;
}
.term-menu-item:hover {
  background: var(--el-color-primary-light-9);
  color: var(--el-color-primary);
}
.term-menu-sep {
  height: 1px;
  background: var(--el-border-color-lighter);
  margin: 4px 0;
}
</style>
