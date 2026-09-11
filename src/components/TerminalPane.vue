<script setup lang="ts">
import { computed, onActivated, onBeforeUnmount, onDeactivated, onMounted, ref, watch } from "vue";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { SearchAddon } from "@xterm/addon-search";
import { WebglAddon } from "@xterm/addon-webgl";
import { ElMessageBox } from "element-plus";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import * as terminalApi from "@/api/terminal";
import type { TerminalDataPayload } from "@/api/terminal";
import { useSettingsStore } from "@/stores/settings";
import { matchesCombo } from "@/utils/shortcut";
import { base64ToBytes, bytesToBase64 } from "@/utils/binary";
import { formatSize } from "@/utils/format";
import { resolveScheme } from "@/utils/terminalThemes";
import {
  normalizePastedText,
  needsPasteConfirm,
  pasteConfirmMessage,
  wrapBracketedPaste,
} from "@/utils/terminalPaste";
import { useZmodemTransfer } from "@/composables/useZmodemTransfer";
import { useTerminalInput } from "@/composables/useTerminalInput";
import { useSuggestions, type SuggestionItem } from "@/composables/useSuggestions";
import TerminalSuggestion from "@/components/TerminalSuggestion.vue";
import "@xterm/xterm/css/xterm.css";

const props = defineProps<{
  instanceId: string;
  sessionConfigId?: string;
  /** 是否远程会话（SSH/Telnet）。本地终端 ConPTY 恒 UTF-8，输出不按终端编码解码。 */
  remote?: boolean;
}>();
const emit = defineEmits<{
  (e: "closed"): void;
  /** 用户按键输入（已通过本终端写入路径处理的原始 data）——父级用于广播路由。 */
  (e: "input", data: string): void;
}>();
const settings = useSettingsStore();

const containerRef = ref<HTMLElement | null>(null);
let term: Terminal | null = null;
let fitAddon: FitAddon | null = null;
let searchAddon: SearchAddon | null = null;
let webglAddon: WebglAddon | null = null;
let unlistens: UnlistenFn[] = [];
let resizeObs: ResizeObserver | null = null;
// --- PTY 尺寸同步去重 --------------------------------------------------------
// RO 回调高频触发（拖拽窗口每帧一次），无差别重发 resize 会造成 SIGWINCH
// 风暴（远端 readline 逐帧重绘闪烁）+ 与 term.onResize 双份 IPC。
// 只在尺寸真正变化时发送；从隐藏恢复（v-show 切回）的首次回调例外——
// 即使尺寸未变也强制重发一次，触发远端 SIGWINCH 重绘提示符（残影修复）。
let lastSentCols = 0;
let lastSentRows = 0;
/** 容器处于隐藏（clientWidth/Height 为 0）：恢复可见的首次 RO 回调需强制重发。 */
let pendingVisibleResync = false;

/** 把当前 xterm 尺寸同步到 PTY（尺寸未变时跳过，重绑后由基线复位触发重发）。 */
function syncPtySize() {
  if (!term) return;
  if (term.cols === lastSentCols && term.rows === lastSentRows) return;
  lastSentCols = term.cols;
  lastSentRows = term.rows;
  terminalApi.terminalResize(props.instanceId, term.cols, term.rows).catch(() => {});
}
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
  // 智能补全：输出流喂给输入状态机做备用屏检测（vim/less 等 TUI 期间暂停
  // 补全）。内部对无 ESC 字节的批次快速跳过；ZMODEM 传输期间不喂。
  if (!zmodemActive.value) terminalInput.handleSessionData(bytes);
  // 剥离应用的鼠标上报激活序列（vim set mouse=a / htop 等）：xterm 永不
  // 进入鼠标模式，左键拖拽始终是终端本地选择（可拖拽复制），滚轮行为也由
  // 本组件统一接管（见 onMouseWheelCapture）。检测用原始字节；ZMODEM 协议
  // 字节与 DECSET 序列互不冲突，剥离后再喂 Sentry 安全。
  zmodemFeed(stripMouseReport(bytes));
}

// --- 鼠标上报序列剥离 ---------------------------------------------------------
// 批次边界任意，激活序列可能被拆在两批之间——尾部可能未完的转义序列前缀
// 暂扣（carry），与下一批拼接后再剥离。
const MOUSE_DECSET_RE = /\x1b\[\?(?:1000|1002|1003|1005|1006|1015)h/g;
/** 尾部 ESC 起头的串是否已是完整转义序列（CSI 含最终字节 / 两字符 Fe 序列）。 */
const ESC_SEQ_COMPLETE_RE = /^\x1b(?:\[[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]|[\x40-\x5f])/;
let mouseStripCarry = "";

function stripMouseReport(bytes: Uint8Array): Uint8Array {
  let text = mouseStripCarry;
  mouseStripCarry = "";
  for (let i = 0; i < bytes.length; i++) text += String.fromCharCode(bytes[i]);
  let body = text;
  const esc = text.lastIndexOf("\x1b");
  if (esc >= 0 && text.length - esc <= 16 && !ESC_SEQ_COMPLETE_RE.test(text.slice(esc))) {
    // 尾部是可能未完的序列前缀：暂扣待下一批（下一批会立即完成判定，不会长期滞留）。
    mouseStripCarry = text.slice(esc);
    body = text.slice(0, esc);
  }
  if (!body.includes("\x1b[?")) return latin1ToBytes(body);
  return latin1ToBytes(body.replace(MOUSE_DECSET_RE, ""));
}

/** Latin-1 文本 → 字节（charCode ≤ 0xFF 双射无损，不破坏 UTF-8 多字节）。 */
function latin1ToBytes(s: string): Uint8Array {
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i) & 0xff;
  return out;
}

/**
 * 按快照基线处理一段输出区间 [start, end)：整段已在快照里则跳过；
 * 跨界则只取基线之后的尾部字节；否则整段渲染。
 */
function dedupOutput(start: number, end: number, bytes: Uint8Array) {
  // 无 total 字段的事件（后端异常路径）无法做区间去重：宁可重复不可丢失，
  // 全量渲染（原实现整段丢弃会静默吞输出，难排查）。
  if (!(end > 0)) {
    feedOutput(bytes);
    return;
  }
  if (end <= attachBaseline) return; // 整段已含在快照里
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
  // 基线复位使 syncPtySize 必然重发（新实例即便尺寸相同也需要同步）。
  lastSentCols = 0;
  lastSentRows = 0;
  syncPtySize();
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
    // attach 失败 = 会话已不存在，含"closed 事件早于本组件监听注册到达"
    // 的场景：主动 emit 让 store 置 disconnected——否则断开覆盖层/重连
    // 按钮不会出现，用户失去重连入口（只剩隐蔽的 tab 右键菜单）。
    emit("closed");
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

// --- 输出多字符编码（GBK / GB18030 / Big5 / Shift-JIS / EUC-KR） -----------
// 非 UTF-8 编码用 TextDecoder 流式解码（stream: true 维护跨块多字节状态，
// 批次/块边界切断双字节字符不会出乱码）。解码位于 ZMODEM 哨兵**之后**：
// 协议二进制已被哨兵消费，到这里的必是普通输出。设置热切换：watch 重建
// 解码器，已打开终端实时生效（与主题热更新行为一致）。
// 仅远程会话（SSH/Telnet）启用：本地终端 ConPTY 输出恒 UTF-8，按 GBK 解码
// 会让本地中文全部乱码。
let outDecoder: TextDecoder | null = null;
watch(
  () => (props.remote === false ? "utf-8" : settings.terminal.encoding),
  (enc) => {
    if (!enc || enc === "utf-8") {
      outDecoder = null;
      return;
    }
    try {
      outDecoder = new TextDecoder(enc);
    } catch {
      // 不支持的标签：退回原样写入（UTF-8）。
      outDecoder = null;
    }
  },
  { immediate: true },
);

/** 输出渲染统一入口：按设置的终端编码解码后写入 xterm（UTF-8 直写字节）。 */
function writeOutput(bytes: Uint8Array) {
  if (outDecoder) term?.write(outDecoder.decode(bytes, { stream: true }));
  else term?.write(bytes);
}

const {
  active: zmodemActive,
  progress: zmodemProgress,
  init: zmodemInit,
  feed: zmodemFeed,
  cancel: zmodemCancel,
  reset: zmodemReset,
} = useZmodemTransfer(
  () => term,
  // raw 旁路：ZMODEM 协议帧/文件内容是任意二进制，跳过后端编码转换。
  (bytes) => terminalApi.terminalWrite(props.instanceId, bytesToBase64(bytes), true),
  writeOutput,
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

// --- 智能补全（历史命令 + 快捷命令 + AI 懒触发） ---------------------------
const suggestions = useSuggestions();
const terminalInput = useTerminalInput(() => term, {
  onHistoryExtract: (cmd) =>
    suggestions.addHistoryCommand(cmd, () => props.sessionConfigId || props.instanceId),
  onResetSuppress: () => suggestions.resetSuppress(),
});

/** AI 建议项可用条件：设置开关开启 && AI 已配置（有 provider 且有激活项）。 */
const aiSuggestEnabled = computed(
  () =>
    settings.terminal.suggestAi && settings.aiProviders.length > 0 && !!settings.aiActive,
);

/** 应用一条建议：ai-preview 触发生成；其余用 Ctrl+U 清行后写入完整命令。
 *
 * 退格方案只适用于"替换光标前 token"；多 token 输入（"git che" → "git
 * checkout"）退格会留下前文，Ctrl+U 清整行最可靠。 */
async function applySuggestion(item: SuggestionItem | null) {
  if (!item) return;
  if (item.type === "ai-preview") {
    await suggestions.generateAISuggestion(terminalInput.lineBuffer.value);
    return;
  }
  if (!item.value) return;
  if (zmodemActive.value) return;
  const b64 = bytesToBase64(new TextEncoder().encode("\x15" + item.value));
  terminalApi.terminalWrite(props.instanceId, b64).catch(() => {
    /* 连接已断 */
  });
  terminalInput.lineBuffer.value = item.value;
  terminalInput.cursorIndex.value = item.value.length;
  terminalInput.currentToken.value = "";
  suggestions.close();
}

function onSuggestionSelect(index: number) {
  void applySuggestion(suggestions.state.value.items[index] ?? null);
  // 鼠标点击建议项后焦点被按钮/行元素带走——显式还给终端。
  term?.focus();
}

function onSuggestionRemove(id: number) {
  suggestions.removeHistoryCommandById(id);
  term?.focus();
}

/** xterm 自定义按键处理：建议可见时拦截 ↑/↓ 导航、Tab/Enter 采纳、Esc 关闭。
 *
 * 返回 false 阻止 xterm 处理（不产生 onData、不发给远端）——↑/↓ 此时不该
 * 翻 shell 历史，而是翻建议列表。xterm v5 的 attach 是替换式（单 handler），
 * 组件独享 term 实例，dispose 时随之清理。 */
function onTerminalKey(e: KeyboardEvent): boolean {
  if (e.type !== "keydown") return true;
  if (suggestions.isVisible()) {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      suggestions.selectNext();
      return false;
    }
    if (e.key === "ArrowUp") {
      e.preventDefault();
      suggestions.selectPrev();
      return false;
    }
    if (e.key === "Tab" || e.key === "Enter") {
      // 仅当用户已用方向键/鼠标显式选中一项时才拦截采纳；未选中时放行
      // （Tab 照常发给远端做 shell 补全，Enter 照常执行当前输入）。
      const selected = suggestions.getSelectedItem();
      if (selected) {
        e.preventDefault();
        void applySuggestion(selected);
        return false;
      }
    }
    if (e.key === "Escape") {
      suggestions.suppress();
      return false;
    }
  }
  return true;
}

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

/** 容器背景 = 主题背景。xterm v6 只把主题背景内联到内容滚动层，
 *  容器余量（不足一行的底部/右侧）由这里兜底，避免露出黑色。 */
function applyBackdrop() {
  const bg =
    resolveScheme(settings.terminal.colorScheme, settings.terminal.theme).theme
      .background ?? "";
  if (containerRef.value) containerRef.value.style.backgroundColor = bg;
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
  applyBackdrop();
  fitAddon.fit();

  // ZMODEM Sentry 必须在注册 terminal:data 监听之前初始化。
  zmodemInit();

  // 智能补全：自定义按键处理（建议可见时拦截 ↑/↓/Tab/Enter/Esc）+ 加载历史。
  term.attachCustomKeyEventHandler(onTerminalKey);
  void suggestions.loadHistory();

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

  // 连接断开：终端提示（含后端透出的断开原因，如服务器 DISCONNECT 文字）+
  // 通知父组件（显示重连按钮）。
  await track(
    listen<{ sessionId: string; reason?: string }>("terminal:closed", (e) => {
      if (unmounted) return;
      if (e.payload.sessionId !== props.instanceId) return;
      zmodemReset();
      const reason = e.payload.reason;
      term?.write("\r\n\x1b[31m[连接已断开" + (reason ? `：${reason}` : "") + "]\x1b[0m\r\n");
      emit("closed");
    }),
  );

  // 用户键盘输入 → 后端。ZMODEM 传输期间屏蔽，避免干扰协议。
  // 写失败（连接已断/卡死超时）在终端内提示一次，成功后自动复位标志——
  // 否则用户对着冻结的终端敲字毫无反馈。
  // 智能补全：输入喂给影子行缓冲（Enter 提取命令记录历史）；建议可见时
  // 同步拦截 Tab/Enter（采纳选中项）与 Esc（关闭并抑制）。
  term.onData((data) => {
    if (zmodemActive.value) return;
    // Ctrl+C 有终端选中内容时优先复制（清选后再按才作为 SIGINT 发送）——
    // 通用终端惯例，与"选中即复制"互补（后者关闭时仍可拖选 + Ctrl+C 复制）。
    if (data === "\x03" && term?.hasSelection()) {
      const sel = term.getSelection();
      if (sel) void copyText(sel);
      term.clearSelection();
      return;
    }
    if (!terminalInput.isInAlternateScreen()) {
      // 采纳拦截在 handleInput 之前：选中建议的 Tab/Enter 不该把半截
      // 输入记入历史（handleInput 的 Enter 分支会提取当前命令入库）。
      if (suggestions.isVisible()) {
        if (data === "\t" || data === "\r" || data === "\n") {
          const selected = suggestions.getSelectedItem();
          if (selected) {
            void applySuggestion(selected);
            return;
          }
        } else if (data === "\x1b") {
          // Esc：关闭建议且不把 Esc 发给远端（否则一次按键两重语义）。
          suggestions.suppress();
          return;
        }
      }
      terminalInput.handleInput(data);
      // 建议刷新放异步微任务：不阻塞输入写入路径。
      const wasVisible = suggestions.isVisible();
      const printable = data.length === 1 && data >= " ";
      queueMicrotask(() => {
        if (unmounted) return;
        if (!settings.terminal.suggestHistory) {
          suggestions.close();
          return;
        }
        if (!terminalInput.lineBuffer.value) {
          suggestions.close();
          return;
        }
        // 弹窗未显示时只有可打印字符才触发（方向键等导航键不弹）。
        if (!wasVisible && !printable) return;
        if (
          terminalInput.isAtLineEnd() &&
          terminalInput.currentToken.value &&
          !terminalInput.isPasswordMode()
        ) {
          suggestions.updateSuggestions(terminalInput.currentToken.value, aiSuggestEnabled.value);
        } else {
          suggestions.close();
        }
      });
    } else {
      suggestions.close();
    }
    // 广播路由：把已放行的原始按键上报父级（父级决定是否同步到其它窗格；
    // 上面的本地拦截分支——复制/采纳建议/Esc 抑制——已提前 return，不会广播）。
    emit("input", data);
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

  // 尺寸变化：浏览器 resize + 容器变化（onResize 仅在尺寸真正变化时触发，
  // 经 syncPtySize 去重后与 RO 回调路径不会双发）。
  term.onResize(() => {
    syncPtySize();
  });

  resizeObs = new ResizeObserver(() => {
    // 隐藏/半隐藏（v-show 切走、布局未完成）时不 fit：此时字符测量失效，
    // fit 会算出**偏小的 cols** 并停在这个错值——PTY/bash 仍按真实列宽输出，
    // readline 局部重绘按两套列宽定位，跨列边界的行错位出"提示符后缀残影"
    //（短主机名不跨界故无症状）。clientWidth/Height 双重校验挡住 0 与半布局。
    const el = containerRef.value;
    if (!el || el.clientWidth === 0 || el.clientHeight === 0) {
      // 标记隐藏：恢复可见的首次回调需强制重发 resize（SIGWINCH 重绘提示符）。
      pendingVisibleResync = true;
      return;
    }
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
        if (pendingVisibleResync) {
          // 恢复可见：强制重发（即使尺寸未变），触发远端提示符重绘。
          pendingVisibleResync = false;
          terminalApi
            .terminalResize(props.instanceId, term.cols, term.rows)
            .catch(() => {
              /* 会话已断开等场景忽略 */
            });
        } else {
          // 常规尺寸变化：尺寸真正改变时才发（fit 内部触发 onResize，
          // 这里只需兜底同发一次相同值，syncPtySize 会去重）。
          syncPtySize();
        }
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

  // 拖拽选择 + 滚轮配合：容器记录左键拖拽状态与指针位置（mouseup 挂
  // window——松键可能发生在容器外）；滚轮在捕获阶段接管（见 onMouseWheelCapture）。
  containerRef.value.addEventListener("mousedown", onPaneMouseDown);
  containerRef.value.addEventListener("mousemove", onPaneMouseMove);
  window.addEventListener("mouseup", onWindowMouseUp);
  containerRef.value.addEventListener("wheel", onMouseWheelCapture, {
    capture: true,
    passive: false,
  });
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
    // 焦点在其它文本输入上下文（AI 面板输入框、对话框输入等）时不劫持，
    // 否则输入被打断、被抢焦点（与下方 copy/paste 的焦点守卫语义对齐）。
    const active = document.activeElement as HTMLElement | null;
    if (
      active &&
      active !== document.body &&
      !el.contains(active) &&
      (active.tagName === "INPUT" ||
        active.tagName === "TEXTAREA" ||
        active.isContentEditable)
    ) {
      return;
    }
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

// --- 拖拽选择 + 滚轮配合 ---------------------------------------------------
// 输出流剥离应用的鼠标上报激活序列（DECSET 1000/1002/1003/1005/1006/1015
// 的 h，见 feedOutput）：xterm 永不进入鼠标模式，vim（set mouse=a）/htop
// 里裸左键拖拽天然是终端本地选择，可拖拽复制。
// 按住左键期间滚轮：
// - 备用屏（vim/less 等）：发送 Ctrl+Y / Ctrl+E——vim 视口滚动、光标不动，
//   用滚轮把更早的内容滚进屏幕继续选；不走 xterm 默认的方向键转换（那会
//   移动光标并重绘清掉选择）。
// - 普通屏：滚动视口并让选择端点跟随指针延伸（scrollLines 移动视口后指针
//   屏幕坐标不变但对应缓冲行已变化——向指针下的元素派发合成 mousemove，
//   xterm 选择服务据此重算选择终点）。
// 未按左键时：普通屏滚轮保持 xterm 原生回滚缓冲；备用屏（vim/less 等）
// 滚轮一律无操作（禁用方向键转换，杜绝误触移动光标）。
let selecting = false;
let pointerX = 0;
let pointerY = 0;

/** 发送按键序列到当前终端（滚轮转按键等场景）。 */
function sendKeys(seq: string) {
  if (!seq || !props.instanceId) return;
  terminalApi
    .terminalWrite(props.instanceId, bytesToBase64(new TextEncoder().encode(seq)))
    .catch(() => {
      /* 连接已断 */
    });
}

function onPaneMouseDown(e: MouseEvent) {
  if (e.button === 0) {
    selecting = true;
    pointerX = e.clientX;
    pointerY = e.clientY;
  }
}

function onPaneMouseMove(e: MouseEvent) {
  pointerX = e.clientX;
  pointerY = e.clientY;
}

function onWindowMouseUp() {
  selecting = false;
}

/** 滚轮行数：像素模式 deltaY≈100-120/格（约 3 行），行模式 deltaY 即行数。 */
function wheelLines(e: WheelEvent): number {
  const mag =
    e.deltaMode === WheelEvent.DOM_DELTA_LINE
      ? Math.abs(e.deltaY)
      : Math.round(Math.abs(e.deltaY) / 40);
  const n = Math.max(1, Math.min(6, mag));
  return e.deltaY < 0 ? -n : n;
}

function onMouseWheelCapture(e: WheelEvent) {
  // Ctrl+滚轮：字号缩放（模板 @wheel，先行放行）。
  if (e.ctrlKey) return;
  if (!term) return;
  if (term.buffer.active.type !== "normal") {
    // 备用屏（vim/less 等）下滚轮一律接管，不再走 xterm 默认的方向键转换
    //（那会移动光标）——与 Xshell/SecureCRT 关闭"滚轮发送方向键"后的行为
    // 一致：未按左键时滚轮无操作（应用内用键盘翻页），杜绝误触移动光标。
    e.preventDefault();
    e.stopPropagation();
    if (!selecting) return;
    // 按住左键选择中：Ctrl+Y（上）/ Ctrl+E（下）视口滚动、光标不动，
    // 把更早的内容滚进屏幕继续选。
    const lines = wheelLines(e);
    if (lines === 0) return;
    sendKeys((lines < 0 ? "\x19" : "\x05").repeat(Math.abs(lines)));
    shiftSelectionAnchor(lines);
    return;
  }
  // 普通屏：仅当按住左键且已拖出终端选中内容时接管延伸；否则保持 xterm
  // 原生滚轮回滚缓冲。
  if (!selecting || !term.hasSelection()) return;
  e.preventDefault();
  e.stopPropagation();
  const lines = wheelLines(e);
  if (lines === 0) return;
  term.scrollLines(lines);
  // 视口已移动：合成 mousemove 让 xterm 把选择终点重算到指针下的新缓冲行。
  const el = document.elementFromPoint(pointerX, pointerY);
  el?.dispatchEvent(
    new MouseEvent("mousemove", {
      clientX: pointerX,
      clientY: pointerY,
      bubbles: true,
      cancelable: true,
    }),
  );
}

/** 备用屏（vim）选择中滚轮：内容随 ^Y/^E 上/下平移 N 行，而 xterm 的
 *  选择锚点固定在终端缓冲坐标上——不平移的话高亮起点会滑到别的文本上
 *  （"起始位置变化"）。这里把锚点随内容平移同样的行数；选择终点跟随
 *  指针坐标不动（滚动后正好指向新滚入的内容），符合"边滚边选"直觉。
 *  取舍：vim 已到缓冲区边界再滚时内容不动而锚点仍平移，起点会贴到屏幕
 *  边缘（滚出屏幕的内容在备用屏本就不存在，无从选起）。 */
function shiftSelectionAnchor(lines: number) {
  if (!term) return;
  // v6 内部路径：_core._selectionService._model（均为私有属性，无公开
  // getter；selectionService/model 不带下划线的写法取到 undefined 会静默
  // 失效）。selectionStart 是 [x, y] 元组，y 为缓冲行号。
  const svc = (
    term as unknown as {
      _core?: {
        _selectionService?: {
          _model?: {
            selectionStart?: [number, number];
            isSelectAllActive?: boolean;
          };
          refresh?: () => void;
        };
      };
    }
  )._core?._selectionService;
  const model = svc?._model;
  if (!model?.selectionStart || model.isSelectAllActive) return;
  // ^Y（滚轮向上）：内容下移 → 锚点行号 += |lines|；^E 相反。行号钳制在
  // 可见行范围内（备用屏无回滚缓冲，出界即内容已滚出屏幕）。
  const y = Math.max(
    0,
    Math.min(term.rows - 1, model.selectionStart[1] - lines),
  );
  model.selectionStart = [model.selectionStart[0], y];
  svc?.refresh?.();
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
    // 智能补全：旧会话的残留输入行与建议清空（新实例从提示符开始）。
    terminalInput.clearBuffer();
    suggestions.close();
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
      applyBackdrop();
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
  containerRef.value?.removeEventListener("mousedown", onPaneMouseDown);
  containerRef.value?.removeEventListener("mousemove", onPaneMouseMove);
  window.removeEventListener("mouseup", onWindowMouseUp);
  containerRef.value?.removeEventListener("wheel", onMouseWheelCapture, { capture: true });
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
  closeMenu();
  const raw = await readClipboard();
  await pasteText(raw);
}

/**
 * 统一的粘贴实体（受"粘贴保护"设置约束）。
 *
 * 所有粘贴入口（右键菜单、粘贴快捷键、系统右键粘贴、xterm 原生 Ctrl+V）
 * 都必须经此写入——否则保护会被绕过。原实现只有 menuPaste 做确认，
 * xterm 内置的 textarea/element paste 监听会把 Ctrl+V 内容直接注入终端。
 */
async function pasteText(raw: string) {
  if (zmodemActive.value) return;
  if (!raw) {
    term?.focus();
    return;
  }
  // 安全粘贴：换行规范化（Windows \r\n → \n）。
  const text = normalizePastedText(raw);
  // 多行 / 危险内容确认：剪贴板里整段脚本被一次回车全部执行是最常见的
  // 事故来源；设置页"粘贴保护"可关。
  if (settings.terminal.pasteConfirm && needsPasteConfirm(text)) {
    try {
      await ElMessageBox.confirm(pasteConfirmMessage(text), "确认粘贴", {
        type: "warning",
        confirmButtonText: "粘贴",
        cancelButtonText: "取消",
      });
    } catch {
      term?.focus();
      return;
    }
  }
  // 括号粘贴：依据远端 shell 真实探测状态（ESC[?2004h）决定是否包裹。
  // 开启时 vim 不逐行缩进；未开启的老 shell 收到包裹序列会输出乱码，故
  // 读 term.modes 而非无条件包。包裹标记属于协议序列，不喂影子缓冲。
  const bracketed = term?.modes.bracketedPasteMode ?? false;
  const inAlt = terminalInput.isInAlternateScreen();
  if (!inAlt && !(bracketed && text.includes("\n"))) {
    // 智能补全：粘贴同步影子缓冲。括号模式下的多行粘贴 shell 按**字面量
    // 插入**当前行（不执行、不清行）——影子缓冲同样只追加文本而非走
    // Enter 分支，故该场景跳过；单行粘贴与无括号模式（真实回车执行）
    // 照常喂入。
    terminalInput.handleInput(text);
  }
  const payload = wrapBracketedPaste(text, bracketed);
  const b64 = bytesToBase64(new TextEncoder().encode(payload));
  terminalApi.terminalWrite(props.instanceId, b64).catch(() => {});
  // 粘贴不走 keydown 路径，xterm 的 scrollOnUserInput 不生效——手动滚底，
  // 否则长输出后粘贴的内容落在视口外。
  term?.scrollToBottom();
  // 粘贴走 terminalWrite API 而非键盘路径，焦点不会自动回到 xterm 的
  // textarea（readClipboard 的异步/权限交互还会把焦点带走）——失焦状态下
  // xterm 光标隐藏，表现为"粘贴后光标消失，要再点一下终端才有"。显式还焦。
  term?.focus();
}

/**
 * 拦截 xterm 原生粘贴（捕获阶段）。
 *
 * xterm 在 textarea / element 上自带 `paste` 监听：Ctrl+V、Shift+Insert、
 * 系统右键粘贴会把内容直接注入终端，绕过 `pasteText` 的粘贴保护。这里在
 * 容器捕获阶段抢先 preventDefault + stopPropagation，统一改走受保护的
 * 粘贴路径（无内容时回退读剪贴板，覆盖 clipboardData 不可用的场景）。
 */
function onPasteCapture(e: ClipboardEvent) {
  e.preventDefault();
  e.stopPropagation();
  closeMenu();
  const raw = e.clipboardData?.getData("text") ?? "";
  if (raw) {
    void pasteText(raw);
  } else {
    void readClipboard().then((t) => pasteText(t));
  }
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
    // 智能补全：快捷命令发出的内容同步影子缓冲（Enter 分支记入历史，
    // 让常用快捷命令也参与后续补全匹配）。
    if (!terminalInput.isInAlternateScreen()) terminalInput.handleInput(command + "\r");
    const b64 = bytesToBase64(new TextEncoder().encode(command + "\r"));
    terminalApi.terminalWrite(props.instanceId, b64).catch(() => {
      /* 连接已断 */
    });
    term?.focus();
  },
  /**
   * 接收广播输入：把其它窗格的原始按键同步到本终端（影子缓冲同步 +
   * 写远端）。不触发建议刷新/焦点转移——广播只关心字节到达远端。
   */
  receiveBroadcast: (data: string) => {
    if (!data || zmodemActive.value || !props.instanceId) return;
    if (!terminalInput.isInAlternateScreen()) terminalInput.handleInput(data);
    const b64 = bytesToBase64(new TextEncoder().encode(data));
    terminalApi.terminalWrite(props.instanceId, b64).catch(() => {
      /* 连接已断 */
    });
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
      @paste.capture="onPasteCapture"
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

    <!-- 智能补全建议弹窗（历史 + 快捷命令 + AI） -->
    <TerminalSuggestion
      :visible="suggestions.state.value.visible"
      :items="suggestions.state.value.items"
      :selected-index="suggestions.state.value.selectedIndex"
      :cursor-x="terminalInput.cursorPixelPos.value.x"
      :cursor-y="terminalInput.cursorPixelPos.value.y"
      @select="onSuggestionSelect"
      @remove="onSuggestionRemove"
    />

    <!-- ZMODEM 传输进度浮层（rz 上传 / sz 下载） -->
    <div v-if="zmodemActive && zmodemProgress" class="zmodem-overlay">
      <div class="zmodem-card">
        <div class="zmodem-head">
          <el-icon :size="14">
            <component :is="zmodemProgress.kind === 'upload' ? 'Upload' : 'Download'" />
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

<style scoped lang="scss">
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
