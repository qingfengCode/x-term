<script setup lang="ts">
import { computed, onActivated, onBeforeUnmount, onDeactivated, onMounted, ref, watch } from "vue";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { SearchAddon } from "@xterm/addon-search";
import { WebglAddon } from "@xterm/addon-webgl";
import { Upload, Download } from "@element-plus/icons-vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import * as terminalApi from "@/api/terminal";
import type { TerminalDataPayload } from "@/api/terminal";
import { useSettingsStore } from "@/stores/settings";
import { matchesCombo } from "@/utils/shortcut";
import { base64ToBytes, bytesToBase64 } from "@/utils/binary";
import { formatSize } from "@/utils/format";
import { resolveScheme } from "@/utils/terminalThemes";
import { useZmodemTransfer } from "@/composables/useZmodemTransfer";
import "@xterm/xterm/css/xterm.css";

const props = defineProps<{ instanceId: string }>();
const emit = defineEmits<{ (e: "closed"): void }>();
const settings = useSettingsStore();

const containerRef = ref<HTMLElement | null>(null);
let term: Terminal | null = null;
let fitAddon: FitAddon | null = null;
let searchAddon: SearchAddon | null = null;
let webglAddon: WebglAddon | null = null;
let unlistens: UnlistenFn[] = [];
let resizeObs: ResizeObserver | null = null;
// 组件销毁标志：listen 未 resolve 前组件可能已卸载，resolve 后据此立即反订阅。
let unmounted = false;

// --- attach 回放状态（首屏输出不丢；重连换 instanceId 时重置复用同一 xterm）---
// 后端 reader 在 connect 返回前就开始推 terminal:data，监听注册之前的输出会丢。
// 流程：监听先注册（期间事件缓存 pendingData，不渲染）→ terminal_attach 取
// 「缓冲快照 + 累计基线」→ 快照先入终端 → 缓存事件按 [startTotal,total) 区间
// 对基线精确去重后放行（整段 ≤ 基线已含在快照里；跨界的只取基线之后的部分）。
// attach 完成后 live 事件同样按区间处理（批量 emit 的事件可能与快照跨界）。
let attachDone = false;
let attachBaseline = 0;
let pendingData: { bytes: Uint8Array; startTotal: number; total: number }[] = [];
// attach 代际：连续快速重连（重连→立刻再重连）时旧的 attach 尚在 await 中，
// 新的已重置状态——旧代返回后必须丢弃，否则会把旧实例的快照写进新会话。
let attachGen = 0;

/** 把字节喂给渲染链（ZMODEM Sentry → 终端），快照与事件统一走这一入口。 */
function feedOutput(bytes: Uint8Array) {
  zmodemFeed(bytes);
}

/**
 * 按快照基线处理一段输出区间 [start, end)：整段已在快照里则跳过；
 * 跨界则只取基线之后的尾部字节；否则整段渲染。
 */
function dedupOutput(start: number, end: number, bytes: Uint8Array) {
  if (!(end > 0) || end <= attachBaseline) return; // 整段已含在快照里
  if (start < attachBaseline) {
    // 跨界：快照含 [start, baseline)，只渲染 [baseline, end)。
    const drop = attachBaseline - start;
    if (drop >= bytes.length) return;
    feedOutput(bytes.subarray(drop));
  } else {
    feedOutput(bytes);
  }
}

/**
 * 绑定一个终端实例：重置回放状态 → 同步当前尺寸 → attach 快照 → 放行缓存。
 * 挂载时与 props.instanceId 变化（重连/手动认证成功换新实例）时调用。
 */
async function attachSession(instanceId: string) {
  const gen = ++attachGen;
  attachDone = false;
  attachBaseline = 0;
  pendingData = [];
  if (!instanceId) return;
  // 新实例 PTY 默认 80x24：先把当前实际尺寸同步过去，尽早对齐减少首屏折行。
  terminalApi
    .terminalResize(instanceId, term?.cols ?? 80, term?.rows ?? 24)
    .catch(() => {});
  try {
    const snap = await terminalApi.terminalAttach(instanceId);
    // 已卸载，或期间又发生了重绑（新一次 attachSession）：丢弃本次结果。
    if (unmounted || gen !== attachGen) return;
    attachBaseline = snap.total;
    const bytes = base64ToBytes(snap.data);
    if (bytes.length) feedOutput(bytes);
  } catch {
    /* 会话已不存在（连接瞬间失败/挂载前恰好断开）：无基线，缓存事件全量
       放行；同时给出一行提示——否则 pane 白屏且无人补发"已断开"事件
       （terminal:closed 在挂载前到达时无人接收）。 */
    if (unmounted || gen !== attachGen) return;
    term?.write("\r\n\x1b[31m[终端会话已结束或已断开，可点击工具栏重连]\x1b[0m\r\n");
  }
  attachDone = true;
  for (const p of pendingData.splice(0)) {
    dedupOutput(p.startTotal, p.total, p.bytes);
  }
}

/** 注册事件监听：resolve 前已卸载则立刻反订阅，避免监听器泄漏到卸载之后。 */
const track = async (un: Promise<UnlistenFn>) => {
  try {
    const fn = await un;
    if (unmounted) fn();
    else unlistens.push(fn);
  } catch (e) {
    console.error("事件订阅失败:", e);
  }
};

// --- 右键菜单 ---
const menuVisible = ref(false);
const menuX = ref(0);
const menuY = ref(0);

// --- 搜索浮层 ---
const searchOpen = ref(false);
const searchKeyword = ref("");
const searchMatchInfo = ref(""); // 如 "3/12"

// --- lrzsz（ZMODEM rz 上传 / sz 下载）---
// 远端输出经 feed 先喂给 ZMODEM Sentry 检测；传输期间 zmodemActive 置位，
// 键盘输入 / 粘贴被门控，避免干扰协议字节流。
const {
  active: zmodemActive,
  progress: zmodemProgress,
  init: zmodemInit,
  feed: zmodemFeed,
  cancel: zmodemCancel,
  reset: zmodemReset,
} = useZmodemTransfer(() => term, (bytes) =>
  terminalApi.terminalWrite(props.instanceId, bytesToBase64(bytes))
);

const zmodemPercent = computed(() => {
  const p = zmodemProgress.value;
  if (!p || p.total <= 0) return 0;
  return Math.max(0, Math.min(100, Math.floor((p.transferred / p.total) * 100)));
});
const zmodemSizeText = computed(() => {
  const p = zmodemProgress.value;
  if (!p) return "";
  return p.total > 0 ? `${formatSize(p.transferred)} / ${formatSize(p.total)}` : formatSize(p.transferred);
});
function buildOptions() {
  const t = settings.terminal;
  return {
    fontFamily: t.fontFamily,
    fontSize: t.fontSize,
    lineHeight: t.lineHeight,
    scrollback: t.scrollback,
    cursorBlink: true,
    // 完整 ANSI 16 色配色方案（旧配置空值按明暗回退 Catppuccin）。
    theme: resolveScheme(t.colorScheme, t.theme).theme,
    allowProposedApi: true,
  };
}

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

// --- 写入失败提示（防抖：只提示一次，成功写入后复位） ---
let writeFailed = false;

// --- 每面板字号覆盖（null = 跟随全局设置） ---
// 工具栏 ± / Ctrl+= 只改本面板的覆盖值：不再污染全局设置（避免"临时放大
// 被之后的 settings.save() 顺带持久化"），也不影响其它 tab；重置回到全局值。
let fontSizeOverride: number | null = null;

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
      webglAddon = new WebglAddon();
      // WebGL 上下文被浏览器/驱动驱逐（多标签超限、系统休眠恢复、驱动重置）
      // 时 xterm 不会自动降级，终端会永久空白。dispose 后 xterm 自动回退
      // DOM 渲染器，内容（含 scrollback）原样重绘。
      webglAddon.onContextLoss(() => {
        webglAddon?.dispose();
        webglAddon = null;
      });
      term.loadAddon(webglAddon);
    } catch {
      /* WebGL 不可用时回退到 canvas */
      webglAddon = null;
    }
  }
  term.open(containerRef.value);
  fitAddon.fit();

  // ZMODEM Sentry 必须在注册 terminal:data 监听之前初始化。
  zmodemInit();

  // 监听后端推送的终端数据：按 instanceId 过滤（动态读 props——重连换新
  // 实例时同一监听继续生效，无需重注册）。
  // 字节先经过 ZMODEM Sentry（检测起始序列 / 路由协议字节），
  // 非传输数据由 Sentry 转回终端写入。attach 完成前先缓存（见 attachSession）。
  await track(
    listen<TerminalDataPayload>("terminal:data", (e) => {
      if (unmounted) return;
      // 动态读当前 instanceId：重连/手动认证成功后监听自动切到新实例。
      if (e.payload.sessionId !== props.instanceId) return;
      const bytes = base64ToBytes(e.payload.data);
      if (!attachDone) {
        pendingData.push({ bytes, startTotal: e.payload.startTotal ?? 0, total: e.payload.total ?? 0 });
        return;
      }
      dedupOutput(e.payload.startTotal ?? 0, e.payload.total ?? 0, bytes);
    }),
  );

  // 连接断开：终端提示 + 通知父组件（显示重连按钮）。
  await track(
    listen<{ sessionId: string }>("terminal:closed", (e) => {
      if (unmounted) return;
      if (e.payload.sessionId !== props.instanceId) return;
      zmodemReset();
      term?.write("\r\n\x1b[31m[连接已断开]\x1b[0m\r\n");
      emit("closed");
    }),
  );

  // 用户键盘输入 → 后端。ZMODEM 传输期间屏蔽，避免干扰协议。
  // 写失败（连接已断/卡死超时）在终端内提示一次，成功后自动复位标志——
  // 否则用户对着冻结的终端敲字毫无反馈。
  term.onData((data) => {
    if (zmodemActive.value) return;
    const b64 = bytesToBase64(new TextEncoder().encode(data));
    terminalApi
      .terminalWrite(props.instanceId, b64)
      .then(() => {
        writeFailed = false;
      })
      .catch(() => {
        if (!writeFailed) {
          writeFailed = true;
          term?.write("\r\n\x1b[31m[写入失败：连接可能已断开或已卡死，可尝试重连]\x1b[0m\r\n");
        }
      });
  });

  // copyOnSelect：选中即复制。回调内实时读设置值（而非挂载时读一次），
  // 设置页切换"选中即复制"对已打开的终端立即生效，无需重挂载。
  term.onSelectionChange(() => {
    if (!settings.terminal.copyOnSelect) return;
    const sel = term?.getSelection();
    if (sel) void copyText(sel);
  });

  // 尺寸变化：浏览器 resize + 容器变化。
  term.onResize(({ cols, rows }) => {
    terminalApi.terminalResize(props.instanceId, cols, rows).catch(() => {});
  });

  resizeObs = new ResizeObserver(() => {
    // 隐藏/半隐藏（v-show 切走、布局未完成）时不 fit：此时字符测量失效，
    // fit 会算出**偏小的 cols** 并停在这个错值——PTY/bash 仍按真实列宽输出，
    // readline 局部重绘按两套列宽定位，跨列边界的行错位出"提示符后缀残影"
    //（短主机名不跨界故无症状）。clientWidth/Height 双重校验挡住 0 与半布局。
    const el = containerRef.value;
    if (!el || el.clientWidth === 0 || el.clientHeight === 0) return;
    try {
      fitAddon?.fit();
      // 容器从隐藏（切走 tab，v-show display:none）恢复可见时：
      // 1. 全量重绘视口——隐藏期间的渲染不落盘，渲染层保留旧画面；
      // 2. 向 PTY 重发一次 window_change（即使尺寸未变）——SSH 照发
      //    SIGWINCH，远端 shell/readline 会**重绘当前提示符行**。
      // 残影的根因在数据层：切换期间 readline 的局部重绘与终端状态错位，
      // 最后一行 buffer 里就带着残缺片段（"root@… ~]# hanzy ~]#"），
      // refresh 治不了 buffer——SIGWINCH 触发的 shell 重绘才是正解
      //（与"按回车残影消失"同机制，但无需用户敲键）。
      term?.refresh(0, (term?.rows ?? 1) - 1);
      if (term) {
        // 视口对齐：v-show 隐藏（高度 0）→ 恢复的过程中，xterm viewport 的
        // 滚动位置会错位——底部露出半行旧内容（形如提示符后缀残影），
        // refresh 治不了（行渲染本身是对的，错的是滚动偏移）。若切换前
        // 就停在底部，这里重新对齐到缓冲区末尾；正在向上翻阅历史的
        // 不打扰。
        const buf = term.buffer.active;
        if (buf.baseY + term.rows >= buf.length) {
          term.scrollToBottom();
        }
        terminalApi
          .terminalResize(props.instanceId, term.cols, term.rows)
          .catch(() => {
            /* 会话已断开等场景忽略 */
          });
      }
    } catch {
      /* 容器隐藏时 fit 会抛错，忽略 */
    }
  });
  resizeObs.observe(containerRef.value);

  // attach 回放（含初始尺寸同步，见 attachSession）：补齐监听注册前丢失的
  // 首屏输出（SSH banner/MOTD、本地 shell 提示符等）。
  void attachSession(props.instanceId);

  // 搜索结果计数。
  searchAddon?.onDidChangeResults((ev) => {
    const { resultIndex, resultCount } = ev;
    searchMatchInfo.value =
      resultCount > 0 ? `${resultIndex + 1}/${resultCount}` : "无匹配";
  });

  // 全局快捷键：Ctrl+F 搜索（仅本实例激活时，避免多 tab 冲突——下文用 windowKeydownTarget 判断焦点）。
  window.addEventListener("keydown", onGlobalKeydown);
});

// 终端页被 KeepAlive 缓存：切到数据库/桌面页时本组件 deactivated 但监听
// 仍挂着——隐藏终端的字号缩放（Ctrl+=/-/0）会误改。随页面激活/停用注册/注销
// （activated 钩子沿 KeepAlive 组件树传播到后代；重复注册同引用幂等）。
onActivated(() => {
  window.addEventListener("keydown", onGlobalKeydown);
});
onDeactivated(() => {
  window.removeEventListener("keydown", onGlobalKeydown);
});

// 搜索（组合键跟随设置页绑定，默认 Ctrl+F）、复制/粘贴、字号缩放。
function onGlobalKeydown(e: KeyboardEvent) {
  // 长按连发（e.repeat）只响应首次按键，避免搜索/缩放被连续触发。
  if (e.repeat) return;
  // 搜索：仅当本终端面板可见（父容器有尺寸）时响应。
  const searchCombo = settings.getAppShortcut("search");
  if (searchCombo && matchesCombo(e, searchCombo)) {
    const el = containerRef.value;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return;
    e.preventDefault();
    openSearch();
    return;
  }
  if (e.key === "Escape" && searchOpen.value) {
    closeSearch();
  }

  // 以下仅当终端拥有焦点时响应，避免与 AI 输入框等抢快捷键。
  if (!containerRef.value?.contains(document.activeElement)) return;

  // 复制 / 粘贴：组合键跟随设置页"快捷键"tab（默认 Ctrl+Shift+C / Ctrl+Shift+V）。
  const copyCombo = settings.getAppShortcut("copy");
  if (copyCombo && matchesCombo(e, copyCombo)) {
    e.preventDefault();
    menuCopy();
    return;
  }
  const pasteCombo = settings.getAppShortcut("paste");
  if (pasteCombo && matchesCombo(e, pasteCombo)) {
    e.preventDefault();
    void menuPaste();
    return;
  }

  // 字号缩放：Ctrl+= / Ctrl+- / Ctrl+0（重置为全局设置值）。与工具条按钮一致，
  // 改的是本面板的覆盖值（不写全局设置、不持久化）。
  if (e.ctrlKey || e.metaKey) {
    if (e.key === "=" || e.key === "+") {
      e.preventDefault();
      zoomFont(1);
    } else if (e.key === "-") {
      e.preventDefault();
      zoomFont(-1);
    } else if (e.key === "0") {
      e.preventDefault();
      resetZoom();
    }
  }
}

/** 放大/缩小本面板字号（8..36，基于当前生效值叠加）。 */
function zoomFont(delta: number) {
  const base = fontSizeOverride ?? settings.terminal.fontSize;
  fontSizeOverride = Math.max(8, Math.min(36, base + delta));
  if (term) term.options.fontSize = fontSizeOverride;
  try {
    fitAddon?.fit();
  } catch {
    /* ignore */
  }
}

/** Ctrl + 滚轮缩放字号（VS Code 惯例）；不写全局设置，仅作用于本面板。 */
function onCtrlWheel(e: WheelEvent) {
  if (!e.ctrlKey) return;
  e.preventDefault();
  const delta = e.deltaY < 0 ? 1 : -1;
  if (term) zoomFont(delta);
}

/** 重置本面板字号覆盖，回到全局设置值。 */
function resetZoom() {
  fontSizeOverride = null;
  if (term) term.options.fontSize = settings.terminal.fontSize;
  try {
    fitAddon?.fit();
  } catch {
    /* ignore */
  }
}

// 重连/手动认证成功换新实例：复用同一 xterm 实例（scrollback 保留），只重绑
// 数据流——重置 ZMODEM 状态并重新 attach 新实例的输出缓冲。
watch(
  () => props.instanceId,
  (id) => {
    if (!id || unmounted) return;
    zmodemReset();
    writeFailed = false;
    void attachSession(id);
  },
);

// 设置变化时热更新（字号遵循面板覆盖优先；scrollback 同步热更，xterm 支持
// 运行时调整，改设置页的回滚行数对已开终端立即生效）。
watch(
  () => settings.terminal,
  (t) => {
    if (term) {
      term.options.fontFamily = t.fontFamily;
      term.options.fontSize = fontSizeOverride ?? t.fontSize;
      term.options.lineHeight = t.lineHeight;
      term.options.scrollback = t.scrollback;
      term.options.theme = resolveScheme(t.colorScheme, t.theme).theme;
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
  unmounted = true;
  for (const u of unlistens) u();
  unlistens = [];
  resizeObs?.disconnect();
  window.removeEventListener("keydown", onGlobalKeydown);
  term?.dispose();
  term = null;
});

// --- 右键菜单处理 ---
// 菜单固定定位在鼠标处；窗口右/下边缘右键时按菜单估算尺寸钳制坐标，
// 避免菜单超出视口被裁掉（终端区几乎占满窗口，边缘右键是高频操作）。
const MENU_W = 160;
const MENU_H = 200;
function onContextMenu(e: MouseEvent) {
  e.preventDefault();
  // 右键粘贴模式（PuTTY 风格，快捷键设置里开关）：右键直接粘贴剪贴板，
  // 不弹菜单——高频粘贴操作少一次菜单往返。
  if (settings.terminal.rightClickPaste) {
    void menuPaste();
    return;
  }
  menuX.value = Math.min(e.clientX, window.innerWidth - MENU_W - 8);
  menuY.value = Math.min(e.clientY, window.innerHeight - MENU_H - 8);
  menuVisible.value = true;
}
function closeMenu() {
  menuVisible.value = false;
}
async function menuCopy() {
  const sel = term?.getSelection() ?? "";
  if (sel) await copyText(sel);
  closeMenu();
  term?.focus();
}
async function menuPaste() {
  // ZMODEM 传输期间不粘贴，避免污染协议字节流。
  if (zmodemActive.value) return;
  const text = await readClipboard();
  if (text) {
    const b64 = bytesToBase64(new TextEncoder().encode(text));
    terminalApi.terminalWrite(props.instanceId, b64).catch(() => {});
  }
  closeMenu();
  // 粘贴走 terminalWrite API 而非键盘路径，焦点不会自动回到 xterm 的
  // textarea（readClipboard 的异步/权限交互还会把焦点带走）——失焦状态下
  // xterm 光标隐藏，表现为"粘贴后光标消失，要再点一下终端才有"。显式还焦。
  term?.focus();
}
function menuSelectAll() {
  term?.selectAll();
  closeMenu();
  term?.focus();
}
function menuClear() {
  term?.clear();
  closeMenu();
  term?.focus();
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

  /**
   * 外部触发的重新适配（切回 tab 布局稳定后调用）：走与 ResizeObserver
   * 相同的守卫逻辑，纠正"恢复瞬间首次 fit 取到失效测量"造成的 cols 偏差。
   */
  function refit() {
    const el = containerRef.value;
    if (!el || el.clientWidth === 0 || el.clientHeight === 0) return;
    try {
      fitAddon?.fit();
      term?.refresh(0, (term?.rows ?? 1) - 1);
      if (term) {
        terminalApi
          .terminalResize(props.instanceId, term.cols, term.rows)
          .catch(() => {});
      }
    } catch {
      /* ignore */
    }
  }

defineExpose({
  refit,
  search: (keyword: string) => searchAddon?.findNext(keyword),
  findNext: (keyword: string) => searchAddon?.findNext(keyword),
  findPrevious: (keyword: string) => searchAddon?.findPrevious(keyword),
  clearSearch: () => searchAddon?.clearDecorations(),
  clear: () => term?.clear(),
  focus: () => term?.focus(),
  /** 本面板字号缩放（覆盖值，不写全局设置）。工具栏 ± 按钮调用。 */
  zoomFont,
  /** 重置本面板字号到全局设置值。 */
  resetZoom,
  /**
   * 向终端发送一条命令（自动追加换行）。
   * 用于快捷命令按钮 / 快捷键触发。
   */
  sendCommand: (command: string) => {
    if (!command || zmodemActive.value) return;
    const b64 = bytesToBase64(new TextEncoder().encode(command + "\r"));
    terminalApi.terminalWrite(props.instanceId, b64).catch(() => {
      /* 连接已断 */
    });
    term?.focus();
  },
  /**
   * 读取当前行光标前的输入内容（原始文本，含 shell 提示符，
   * 由调用方自行剥离）。用于「添加命令到快捷命令」时预填。
   */
  getCurrentLine: () => {
    if (!term) return "";
    const buf = term.buffer.active;
    const line = buf.getLine(buf.cursorY);
    if (!line) return "";
    return line.translateToString(true).slice(0, buf.cursorX);
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
      @wheel="onCtrlWheel"
    />

    <!-- 搜索浮层 -->
    <div v-if="searchOpen" class="term-search">
      <input
        ref="searchInputRef"
        v-model="searchKeyword"
        class="term-search-input"
        placeholder="搜索..."
        aria-label="终端内搜索"
        spellcheck="false"
        @keydown.enter.prevent="runSearch('next')"
        @keydown.esc.prevent="closeSearch"
      />
      <button class="term-search-btn" title="上一个" aria-label="上一个匹配" @click="runSearch('prev')">↑</button>
      <button class="term-search-btn" title="下一个" aria-label="下一个匹配" @click="runSearch('next')">↓</button>
      <span class="term-search-info" aria-live="polite">{{ searchMatchInfo }}</span>
      <button class="term-search-close" title="关闭 (Esc)" aria-label="关闭搜索" @click="closeSearch">×</button>
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

    <!-- ZMODEM 传输进度浮层（rz 上传 / sz 下载） -->
    <div v-if="zmodemActive && zmodemProgress" class="zmodem-overlay">
      <div class="zmodem-card">
        <div class="zmodem-head">
          <el-icon :size="14">
            <component :is="zmodemProgress.kind === 'upload' ? Upload : Download" />
          </el-icon>
          <span class="zmodem-name" :title="zmodemProgress.name">{{ zmodemProgress.name }}</span>
        </div>
        <el-progress
          :percentage="zmodemPercent"
          :stroke-width="8"
          :status="zmodemPercent >= 100 ? 'success' : undefined"
        />
        <div class="zmodem-meta">
          <span>
            {{ zmodemProgress.kind === "upload" ? "上传中" : "下载中" }} {{ zmodemSizeText }}
          </span>
          <button class="zmodem-cancel" @click="zmodemCancel">取消</button>
        </div>
      </div>
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
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 8px;
  padding: 4px 6px;
  box-shadow:
    0 8px 24px rgba(0, 0, 0, 0.16),
    0 2px 8px rgba(0, 0, 0, 0.08);
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
  border-radius: 4px;
  font-size: 13px;
  line-height: 1;
  transition: background-color 0.15s ease, color 0.15s ease;
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
  min-width: 148px;
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 8px;
  box-shadow:
    0 8px 24px rgba(0, 0, 0, 0.16),
    0 2px 8px rgba(0, 0, 0, 0.08);
  padding: 4px;
  /* 与共享 TabBar 菜单统一层级（避免被后开的浮层/菜单遮挡）。 */
  z-index: 3000;
}
.term-menu-item {
  padding: 6px 10px;
  border-radius: 5px;
  font-size: 13px;
  color: var(--el-text-color-primary);
  cursor: pointer;
  transition: background-color 0.12s ease, color 0.12s ease;
}
.term-menu-item:hover {
  background: var(--el-color-primary-light-9);
  color: var(--el-color-primary);
}
.term-menu-sep {
  height: 1px;
  background: var(--el-border-color-lighter);
  margin: 4px 6px;
}

/* ZMODEM 传输进度浮层 */
.zmodem-overlay {
  position: absolute;
  top: 12px;
  left: 50%;
  transform: translateX(-50%);
  width: min(420px, calc(100% - 32px));
  z-index: 20;
  pointer-events: none;
}
.zmodem-card {
  pointer-events: auto;
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 10px;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.18);
  padding: 10px 12px;
}
.zmodem-head {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-bottom: 8px;
  color: var(--el-color-primary);
}
.zmodem-name {
  flex: 1;
  font-size: 13px;
  color: var(--el-text-color-primary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.zmodem-meta {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-top: 6px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.zmodem-cancel {
  border: none;
  background: transparent;
  color: var(--el-color-danger);
  font-size: 12px;
  cursor: pointer;
  padding: 2px 8px;
  border-radius: 4px;
}
.zmodem-cancel:hover {
  background: var(--el-color-danger-light-9);
}
</style>
