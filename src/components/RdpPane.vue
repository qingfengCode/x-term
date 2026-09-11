<!--
  RdpPane.vue — 内嵌 RDP 客户端（终端页标签页内容）。

  用 IronRDP 官方 WASM 客户端（@devolutions/iron-remote-desktop-rdp）把远端
  Windows 桌面渲染到画布，经后端「迷你网关」桥接（rdp_bridge_start）连接目标
  3389。与 TerminalPane/VncPane 对齐：props 传实例 id + 连接信息，断线时
  emit closed，由 Workspace 统一标记断开并停止桥接。

  注意：IronRDP WASM 只讲 Devolutions Gateway 协议（RDCleanPath），由后端桥接
  扮演网关（X.224/TLS 握手 + 证书链回传），用户名/口令在隧道内由 CredSSP 与
  目标协商，不经过后端。
-->
<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { ElMessage } from "element-plus";
import { registerDesktopControl, unregisterDesktopControl } from "@/api/desktopControl";

const props = defineProps<{
  /** 桥接实例 id（与 tab.instanceId 一致）。 */
  instanceId: string;
  /** WASM 客户端直连的 ws 地址（后端 rdp_bridge_start 返回）。 */
  wsUrl: string;
  /** 目标 RDP 主机。 */
  host: string;
  /** 目标 RDP 端口。 */
  port: number;
  /** 登录用户名（NLA 必填）。 */
  username: string;
  /** 登录密码（可选）。 */
  password?: string;
  /** 上次使用的 RDP 分辨率（"宽x高"）：连接后先恢复它，容器变化后再跟随。 */
  initialSize?: string;
}>();
const emit = defineEmits<{
  (e: "closed"): void;
  (e: "connected"): void;
  (e: "sizechange", size: string): void;
}>();

// --- WASM 模块与会话（动态 import：5.8MB WASM 仅首次打开 RDP 标签页时加载） ---

type RdpBackend = typeof import("@devolutions/iron-remote-desktop-rdp");
type RdpSession = Awaited<
  ReturnType<InstanceType<RdpBackend["Backend"]["SessionBuilder"]>["connect"]>
>;
type RdpDeviceEvent = ReturnType<RdpBackend["Backend"]["DeviceEvent"]["keyPressed"]>;
type RdpClipboardData = InstanceType<RdpBackend["Backend"]["ClipboardData"]>;

/** WASM 模块（卸载时置 null，异步回调据此静默退出）。 */
let backend: RdpBackend | null = null;
/** 当前会话（卸载时置 null，run() 结束不再 emit）。 */
let session: RdpSession | null = null;
/** 组件是否已卸载（取代 backend 判空，避免吞掉 import 失败）。 */
let disposed = false;
/**
 * 会话 writer 任务是否仍在运行。connect 成功后即置 true，run() 结束（正常或异常）
 * 置 false。会话结束后任何输入/resize 调用都会失败（applyInputs 抛
 * "Send input events to writer task"、resize 直接 panic），必须先判活。
 */
let sessionAlive = false;

const containerRef = ref<HTMLDivElement | null>(null);
const canvasRef = ref<HTMLCanvasElement | null>(null);
const status = ref<"connecting" | "connected" | "disconnected" | "error">("connecting");
/** 连接中覆盖层显示的分阶段提示（诊断用：加载/初始化/拨号哪一步卡住一目了然）。 */
const stage = ref("连接中…");
/** 画布是否处于元素全屏（fullscreenchange 同步）。 */
const isFullscreen = ref(false);

const statusText = computed(() => {
  switch (status.value) {
    case "connecting":
      return "连接中…";
    case "connected":
      return "已连接";
    case "disconnected":
      return "已断开";
    case "error":
      return "连接失败";
  }
});

// --- 键盘映射（与官方 web 客户端同源的 code → Windows scancode 表） ---

const SCANCODES: Record<string, number> = {
  "AltLeft": 56,
  "AltRight": 57400,
  "ArrowDown": 57424,
  "ArrowLeft": 57419,
  "ArrowRight": 57421,
  "ArrowUp": 57416,
  "Backquote": 41,
  "Backslash": 43,
  "Backspace": 14,
  "BracketLeft": 26,
  "BracketRight": 27,
  "BrowserBack": 57450,
  "BrowserFavorites": 57446,
  "BrowserForward": 57449,
  "BrowserHome": 57394,
  "BrowserRefresh": 57447,
  "BrowserSearch": 57445,
  "BrowserStop": 57448,
  "CapsLock": 58,
  "Comma": 51,
  "ContextMenu": 57437,
  "ControlLeft": 29,
  "ControlRight": 57373,
  "Convert": 121,
  "Delete": 57427,
  "Digit0": 11,
  "Digit1": 2,
  "Digit2": 3,
  "Digit3": 4,
  "Digit4": 5,
  "Digit5": 6,
  "Digit6": 7,
  "Digit7": 8,
  "Digit8": 9,
  "Digit9": 10,
  "End": 57423,
  "Enter": 28,
  "Equal": 13,
  "Escape": 1,
  "F1": 59,
  "F10": 68,
  "F11": 87,
  "F12": 88,
  "F13": 100,
  "F14": 101,
  "F15": 102,
  "F16": 103,
  "F17": 104,
  "F18": 105,
  "F19": 106,
  "F2": 60,
  "F20": 107,
  "F21": 108,
  "F22": 109,
  "F23": 110,
  "F24": 118,
  "F3": 61,
  "F4": 62,
  "F5": 63,
  "F6": 64,
  "F7": 65,
  "F8": 66,
  "F9": 67,
  "Home": 57415,
  "Insert": 57426,
  "IntlBackslash": 86,
  "IntlRo": 115,
  "IntlYen": 125,
  "KanaMode": 112,
  "KeyA": 30,
  "KeyB": 48,
  "KeyC": 46,
  "KeyD": 32,
  "KeyE": 18,
  "KeyF": 33,
  "KeyG": 34,
  "KeyH": 35,
  "KeyI": 23,
  "KeyJ": 36,
  "KeyK": 37,
  "KeyL": 38,
  "KeyM": 50,
  "KeyN": 49,
  "KeyO": 24,
  "KeyP": 25,
  "KeyQ": 16,
  "KeyR": 19,
  "KeyS": 31,
  "KeyT": 20,
  "KeyU": 22,
  "KeyV": 47,
  "KeyW": 17,
  "KeyX": 45,
  "KeyY": 21,
  "KeyZ": 44,
  "Lang1": 114,
  "Lang2": 113,
  "LaunchApp1": 57451,
  "LaunchApp2": 57377,
  "LaunchMail": 57452,
  "MediaPlayPause": 57378,
  "MediaSelect": 57453,
  "MediaStop": 57380,
  "MediaTrackNext": 57369,
  "MediaTrackPrevious": 57360,
  "Minus": 12,
  "NonConvert": 123,
  "NumLock": 57413,
  "Numpad0": 82,
  "Numpad1": 79,
  "Numpad2": 80,
  "Numpad3": 81,
  "Numpad4": 75,
  "Numpad5": 76,
  "Numpad6": 77,
  "Numpad7": 71,
  "Numpad8": 72,
  "Numpad9": 73,
  "NumpadAdd": 78,
  "NumpadComma": 126,
  "NumpadDecimal": 83,
  "NumpadDivide": 57397,
  "NumpadEnter": 57372,
  "NumpadEqual": 89,
  "NumpadMultiply": 55,
  "NumpadSubtract": 74,
  "PageDown": 57425,
  "PageUp": 57417,
  "Pause": 57414,
  "Period": 52,
  "Power": 57438,
  "PrintScreen": 84,
  "Quote": 40,
  "ScrollLock": 70,
  "Semicolon": 39,
  "ShiftLeft": 42,
  "ShiftRight": 54,
  "Slash": 53,
  "Space": 57,
  "Tab": 15,
  "VolumeDown": 57390,
  "VolumeMute": 57376,
  "VolumeUp": 57392,
};

/** 大写锁等锁定键：按下时向远端同步状态。 */
const LOCK_KEYS = ["CapsLock", "NumLock", "ScrollLock", "KanaMode"];

/** 已按下且走 Unicode 通道的键（keyup 时成对发送 unicodeReleased）。 */
const pressedUnicode = new Set<string>();

let resizeObserver: ResizeObserver | null = null;
let resizeTimer: ReturnType<typeof setTimeout> | null = null;

onMounted(() => {
  // AI 桌面工具（desktop_*）控制句柄：注册进全局注册表，桌面助手据此截图/注入输入。
  registerDesktopControl(props.instanceId, desktopControl);
  void start();
  // 元素全屏状态同步（Esc 由浏览器原生退出全屏，无需自行处理）。
  document.addEventListener("fullscreenchange", onFullscreenChange);
  // F11 全屏：挂 window 级，画布外（如工具栏按钮聚焦时）同样生效。
  window.addEventListener("keydown", onWindowKeydown);
  // 容器尺寸变化 → 远端桌面自适应（displayControl 扩展，150ms 防抖）。
  const container = containerRef.value;
  if (container) {
    resizeObserver = new ResizeObserver(() => {
      if (!session) return;
      if (resizeTimer) clearTimeout(resizeTimer);
      resizeTimer = setTimeout(applyResize, 150);
    });
    resizeObserver.observe(container);
  }
});

onBeforeUnmount(() => {
  // 注销 AI 桌面控制句柄（标签关闭后 AI 再操作会得到明确的"会话不可用"错误）。
  unregisterDesktopControl(props.instanceId);
  disposed = true;
  sessionAlive = false; // 停止一切输入/resize 发送，避免 shutdown 竞态中的发送失败
  document.removeEventListener("fullscreenchange", onFullscreenChange);
  window.removeEventListener("keydown", onWindowKeydown);
  if (resizeObserver) {
    resizeObserver.disconnect();
    resizeObserver = null;
  }
  // 清理挂起的输入帧回调（卸载后不发送）。
  if (inputRaf) cancelAnimationFrame(inputRaf);
  inputRaf = 0;
  pendingMove = null;
  pendingWheel = null;
  // 关闭标签时同步记忆当前分辨率（canvas backing 即远端分辨率）。
  notifySize();
  const s = session;
  session = null;
  backend = null; // 后续异步回调静默退出，不再 emit
  s?.shutdown();
});

/** F11：切换画布全屏（拦截默认行为，仅作用于本画布）。 */
function onWindowKeydown(e: KeyboardEvent) {
  if (e.key === "F11") {
    e.preventDefault();
    toggleFullscreen();
  }
}

/** fullscreenchange：同步全屏状态（全屏时远端桌面已随容器自适应）。 */
function onFullscreenChange() {
  isFullscreen.value = document.fullscreenElement === containerRef.value;
}

/** 切换画布元素全屏（进入时远端桌面自动适配新容器尺寸）。 */
function toggleFullscreen() {
  const container = containerRef.value;
  if (!container) return;
  if (document.fullscreenElement) {
    void document.exitFullscreen().catch(() => {});
  } else {
    void container.requestFullscreen().catch(() => {
      ElMessage.warning("全屏被拒绝（WebView 限制），可尝试系统全屏（F11）");
    });
  }
}

/** 目标地址（IPv6 加方括号）。 */
function destination(): string {
  return props.host.includes(":") ? `[${props.host}]:${props.port}` : `${props.host}:${props.port}`;
}

/** 提取错误可读文本。WASM 抛出的 IronError 是 wasm-bindgen 包装对象（String(e) 只得到
 *  "[object Object]"），需调用其 backtrace()/kind() 取真实错误消息。 */
function errorText(e: unknown): string {
  if (e instanceof Error) return e.message || e.name;
  if (typeof e === "string") return e;
  const anyE = e as {
    backtrace?: () => string;
    kind?: () => string;
    rdcleanpathDetails?: () => unknown;
  };
  try {
    if (typeof anyE.backtrace === "function") {
      const bt = anyE.backtrace();
      if (bt) return bt;
    }
    if (typeof anyE.kind === "function") {
      const k = anyE.kind();
      if (k) return "kind=" + String(k);
    }
  } catch {
    /* 忽略取值失败 */
  }
  try {
    const msg = (e as { message?: unknown }).message;
    if (typeof msg === "string" && msg) return msg;
    const s = JSON.stringify(e);
    if (s && s !== "{}") return s;
  } catch {
    /* 忽略序列化失败 */
  }
  return String(e);
}

/** 建立 WASM 会话：初始化 → SessionBuilder → connect → run 主循环。 */
async function start() {
  const container = containerRef.value;
  const canvas = canvasRef.value;
  if (!container || !canvas) {
    console.error("[rdp-pane] 容器/画布缺失，无法启动");
    return;
  }
  try {
    // 懒加载 WASM（首个 RDP 标签页触发下载/编译，约 5.8MB）。
    stage.value = "加载 WASM 模块…";
    const mod = await import("@devolutions/iron-remote-desktop-rdp");
    backend = mod;
    stage.value = "初始化 WASM…";
    await mod.init("off"); // 关闭 WASM 日志

    const builder = new mod.Backend.SessionBuilder();
    builder
      .username(props.username)
      .password(props.password ?? "")
      .destination(destination())
      .proxyAddress(props.wsUrl)
      // 无真实网关 token：空字符串即可，本桥接不校验。
      .authToken("")
      .renderCanvas(canvas)
      .setCursorStyleCallback(onCursorStyle)
      .setCursorStyleCallbackContext(undefined)
      // 允许远端桌面随本窗口尺寸变化（displayControl 动态分辨率）。
      .extension(mod.displayControl(true))
      // 远端剪贴板变化 → 同步到本地。
      .remoteClipboardChangedCallback(onRemoteClipboard);

    stage.value = "正在连接代理 " + (props.wsUrl || "(空地址!)");
    const s = await builder.connect();
    if (disposed) {
      // 连接期间组件已卸载：立即回收会话。
      s.shutdown();
      return;
    }
    session = s;
    sessionAlive = true;
    status.value = "connected";
    // 已连上即通知外层抹除内存中的明文密码（NLA 凭据已交由会话持有）。
    emit("connected");
    // 有记忆分辨率则先恢复（远端桌面回到上次使用的尺寸，之后容器尺寸变化再由
    // ResizeObserver 触发跟随）；没有记忆则直接跟随当前容器。物理像素与 CSS
    // 一致（scale_factor 100），理由见 applyResize 注释：高 DPI 放大是卡顿主因。
    const remembered = parseSize(props.initialSize);
    if (remembered) {
      const size = clampDesktopSize(remembered.w, remembered.h);
      s.resize(size.w, size.h, 100);
    } else {
      applyResize();
    }

    // 会话主循环：远端断开/出错时结束 → 记忆分辨率 → 通知 Workspace 标记断开。
    // rejection 也必须处理：会话异常终止（如 resize 参数非法导致 writer 任务死亡）
    // 时 run() 以 Err 结束，不处理会停留在「已连接」且后续输入全部抛错。
    void s
      .run()
      .then(() => {
        sessionAlive = false;
        if (!session) return; // 组件卸载或主动断开时不重复通知
        notifySize();
        status.value = "disconnected";
        emit("closed");
      })
      .catch((err: unknown) => {
        sessionAlive = false;
        if (!session) return; // 组件已卸载：不再通知
        console.error("[rdp-pane] RDP 会话异常结束:", err);
        notifySize();
        status.value = "disconnected";
        emit("closed");
      });
  } catch (e) {
    console.error("[rdp-pane] 连接失败:", e);
    if (disposed) return; // 组件已卸载：不再通知
    notifySize();
    status.value = "error";
    stage.value = "连接失败：" + errorText(e);
    ElMessage.error("RDP 连接失败：" + errorText(e));
    emit("closed");
  }
}

/** 光标样式回调（WASM 要求注册）：远端光标变化时更新画布 cursor。 */
function onCursorStyle(
  kind: string,
  data: string | null,
  hotX: number | null,
  hotY: number | null,
) {
  const canvas = canvasRef.value;
  if (!canvas) return;
  switch (kind) {
    case "hidden":
      canvas.style.cursor = "none";
      break;
    case "default":
      canvas.style.cursor = "default";
      break;
    case "url":
      if (data != null && hotX != null && hotY != null) {
        canvas.style.cursor = `url(${data}) ${Math.round(hotX)} ${Math.round(hotY)}, default`;
      }
      break;
  }
}

/** 远端剪贴板变化：把 text/plain 同步到本地剪贴板（失败静默）。 */
function onRemoteClipboard(data: RdpClipboardData) {
  try {
    const text = data
      .items()
      .find((item) => item.mimeType() === "text/plain")
      ?.value();
    if (typeof text === "string" && text) {
      void navigator.clipboard.writeText(text).catch(() => {});
    }
  } catch {
    /* 忽略剪贴板同步失败 */
  }
}

/** 按当前容器尺寸请求远端桌面调整（displayControl）。 */
function applyResize() {
  const container = containerRef.value;
  if (!container || !session || !sessionAlive) return;
  const size = clampDesktopSize(container.clientWidth, container.clientHeight);
  // 物理像素 = CSS 像素（scale_factor 100）：IronRDP 渲染在 WASM 主线程同步
  // 解码，若按 devicePixelRatio 放大物理像素，高 DPI 下解码面积膨胀 2.25~4 倍，
  // 是局域网内卡顿的主要来源。1x 下桌面文字/UI 依然清晰，画布无 CSS 缩放。
  //
  // resize 参数约束（ironrdp-displaycontrol 校验，非法值会让 writer 任务直接
  // 报错死亡、后续输入全部失效）：第 3 参是缩放百分比（100..=500，传 1 非法），
  // 第 4/5 参是显示器物理尺寸毫米（10..=10000，传像素值语义错误）；两者都省略。
  // 尺寸本身须收敛到 200..=8192 且宽为偶数（clampDesktopSize）。
  try {
    session.resize(size.w, size.h, 100);
  } catch (e) {
    // 会话恰好在此刻结束（writer 任务已死）时 resize 会失败：静默即可，
    // run() 的 then/catch 负责标记断开。
    console.error("[rdp-pane] resize 发送失败:", e);
  }
}

/**
 * 把尺寸收敛到 displayControl 合法范围：宽高 200..=8192，宽度取偶数
 * （MS-RDPEDISP 要求宽度不得为奇数；小于 200 或大于 8192 会编码报错）。
 */
function clampDesktopSize(w: number, h: number): { w: number; h: number } {
  const clamp = (v: number) => Math.min(8192, Math.max(200, Math.floor(v)));
  const width = clamp(w);
  return { w: width - (width % 2), h: clamp(h) };
}

/** 解析记忆分辨率 "宽x高"（非法/缺省返回 null）。 */
function parseSize(s?: string): { w: number; h: number } | null {
  if (!s) return null;
  const m = /^(\d+)x(\d+)$/.exec(s.trim());
  if (!m) return null;
  const w = Number(m[1]);
  const h = Number(m[2]);
  if (!Number.isInteger(w) || !Number.isInteger(h) || w < 1 || h < 1) return null;
  return { w, h };
}

/**
 * 上报当前远端分辨率（"宽x高"，canvas backing 即远端分辨率）供上层持久化。
 * 未真正连接过（画布 0 尺寸）时不上报，避免覆盖既有记忆。
 */
function notifySize() {
  const canvas = canvasRef.value;
  if (!canvas || !canvas.width || !canvas.height) return;
  emit("sizechange", `${canvas.width}x${canvas.height}`);
}

/** 把一串输入事件打包成一个事务发送。会话结束后 writer 任务已死，任何发送都会
 *  抛错（resize 甚至会 panic），先判活再发。 */
function sendTx(events: RdpDeviceEvent[]) {
  if (!session || !backend || !sessionAlive) return;
  try {
    const tx = new backend.Backend.InputTransaction();
    for (const event of events) tx.addEvent(event);
    session.applyInputs(tx);
  } catch (e) {
    // 诊断：输入发送失败立即暴露（console 已被 main.ts 转发到后端日志）。
    console.error("[rdp-pane] 发送输入失败:", e);
  }
}

// --- 键盘 ---

function onKeydown(e: KeyboardEvent) {
  if (!backend || !sessionAlive) return;
  // Ctrl+V 剪贴板粘贴：捕获阶段截获，避免被当作按键发送给远端。
  if ((e.ctrlKey || e.metaKey) && !e.shiftKey && e.key.toLowerCase() === "v") {
    e.preventDefault();
    e.stopPropagation();
    void pasteClipboard();
    return;
  }
  if (!session) return;
  e.preventDefault();
  syncLockKeys(e);
  const scancode = SCANCODES[e.code];
  if (scancode !== undefined) {
    if (!e.repeat) sendTx([backend.Backend.DeviceEvent.keyPressed(scancode)]);
    return;
  }
  // 不在映射表内的可打印单字符走 Unicode 通道（如 IME 输入）。
  if (!e.repeat && isUnicodeKey(e)) {
    pressedUnicode.add(e.key);
    sendTx([backend.Backend.DeviceEvent.unicodePressed(e.key)]);
  }
}

function onKeyup(e: KeyboardEvent) {
  if (!session || !backend || !sessionAlive) return;
  e.preventDefault();
  syncLockKeys(e);
  const scancode = SCANCODES[e.code];
  if (scancode !== undefined) {
    sendTx([backend.Backend.DeviceEvent.keyReleased(scancode)]);
  } else if (pressedUnicode.has(e.key)) {
    pressedUnicode.delete(e.key);
    sendTx([backend.Backend.DeviceEvent.unicodeReleased(e.key)]);
  }
}

function isUnicodeKey(e: KeyboardEvent): boolean {
  return e.key.length === 1 && !["Dead", "Unidentified"].includes(e.key);
}

/** 锁定键按下时同步 CapsLock/NumLock/ScrollLock 状态。 */
function syncLockKeys(e: KeyboardEvent) {
  if (!session || !sessionAlive || !LOCK_KEYS.includes(e.code)) return;
  session.synchronizeLockKeys(
    e.getModifierState("ScrollLock"),
    e.getModifierState("NumLock"),
    e.getModifierState("CapsLock"),
    e.getModifierState("KanaMode"),
  );
}

// --- 鼠标 ---

/**
 * pointermove / wheel 是高频事件（游戏鼠标回报率可达 1000Hz，远超屏幕刷新率），
 * 逐事件发送会让 JS→WASM 调用与 WS 帧数暴涨。统一做 rAF 合并：同帧内只保留
 * 最新坐标（滚轮则累加增量），每帧最多发送一次，与屏幕刷新同步。
 */
let pendingMove: { x: number; y: number } | null = null;
let pendingWheel: { vertical: boolean; amount: number; unit: 0 | 1 | 2 } | null = null;
let inputRaf = 0;

/** 预约一帧后的统一发送（move/wheel 共用一个 rAF，同帧合并一次发送）。 */
function scheduleFlush() {
  if (inputRaf) return;
  inputRaf = requestAnimationFrame(() => {
    inputRaf = 0;
    flushPendingInputs();
  });
}

/** 帧回调统一出口：发送挂起的输入（卸载后 session/backend 置空，静默丢弃）。 */
function flushPendingInputs() {
  if (pendingMove) {
    const m = pendingMove;
    pendingMove = null;
    if (session && backend) sendTx([backend.Backend.DeviceEvent.mouseMove(m.x, m.y)]);
  }
  if (pendingWheel) {
    const w = pendingWheel;
    pendingWheel = null;
    if (session && backend) {
      sendTx([backend.Backend.DeviceEvent.wheelRotations(w.vertical, -w.amount, w.unit)]);
    }
  }
}

/** 立即发送挂起的鼠标移动。down/up 前必须调用：保证"先落位、再按下"的顺序，
 *  否则快速点击时挂起的旧位置移动会在按钮事件之后到达远端，点击落点错乱。 */
function flushPendingMove() {
  if (!pendingMove) return;
  const m = pendingMove;
  pendingMove = null;
  if (session && backend) sendTx([backend.Backend.DeviceEvent.mouseMove(m.x, m.y)]);
}

function onPointerDown(e: PointerEvent) {
  if (!session || !backend || !sessionAlive) return;
  e.preventDefault();
  const container = containerRef.value;
  container?.focus(); // 键盘事件目标
  flushPendingMove(); // 先落位再按下，避免点击落在旧位置
  sendTx([backend.Backend.DeviceEvent.mouseButtonPressed(e.button)]);
}

function onPointerUp(e: PointerEvent) {
  if (!session || !backend || !sessionAlive) return;
  e.preventDefault();
  flushPendingMove(); // 释放前同样先落位
  sendTx([backend.Backend.DeviceEvent.mouseButtonReleased(e.button)]);
}

function onPointerMove(e: PointerEvent) {
  const canvas = canvasRef.value;
  if (!session || !backend || !sessionAlive || !canvas) return;
  // CSS 坐标 → 画布 backing 像素（canvas.width 即远端桌面分辨率），
  // 画布外的空白区域（信箱边）坐标钳制进桌面范围。
  const rect = canvas.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return;
  const x = Math.min(canvas.width, Math.max(0, Math.round(((e.clientX - rect.left) / rect.width) * canvas.width)));
  const y = Math.min(canvas.height, Math.max(0, Math.round(((e.clientY - rect.top) / rect.height) * canvas.height)));
  pendingMove = { x, y };
  scheduleFlush();
}

function onWheel(e: WheelEvent) {
  if (!session || !backend || !sessionAlive) return;
  e.preventDefault();
  const vertical = e.deltaY !== 0;
  const amount = vertical ? e.deltaY : e.deltaX;
  // RotationUnit: 0=Pixel 1=Line 2=Page（官方 d.ts 未导出该枚举，用成员字面量）。
  const unit: 0 | 1 | 2 = e.deltaMode === 1 ? 1 : e.deltaMode === 2 ? 2 : 0;
  // 同帧内多个滚轮事件（触摸板惯性滚动）增量累加后一次发送。
  if (pendingWheel && pendingWheel.vertical === vertical && pendingWheel.unit === unit) {
    pendingWheel.amount += amount;
  } else {
    pendingWheel = { vertical, amount, unit };
  }
  scheduleFlush();
}

// --- 剪贴板 / 工具栏 ---

/** Ctrl+V：把本地剪贴板文本粘贴给远端（CLIPRDR）。 */
async function pasteClipboard() {
  if (!session || !backend || !sessionAlive) return;
  try {
    const text = await navigator.clipboard.readText();
    if (!text) return;
    const data = new backend.Backend.ClipboardData();
    data.addText("text/plain", text);
    await session.onClipboardPaste(data);
  } catch {
    ElMessage.warning("无法读取剪贴板（WebView 权限受限），请在远端窗口内手动粘贴");
  }
}

/** 发送 Ctrl+Alt+Del（Windows 登录/解锁屏常用）。 */
function sendCtrlAltDel() {
  if (!session || !backend) return;
  const de = backend.Backend.DeviceEvent;
  // LCtrl(0x1D) + LAlt(0x38) + Delete(0x53) 按→放。
  sendTx([
    de.keyPressed(0x1d),
    de.keyPressed(0x38),
    de.keyPressed(0x53),
    de.keyReleased(0x53),
    de.keyReleased(0x38),
    de.keyReleased(0x1d),
  ]);
}

// --- AI 桌面工具控制（desktop_*：截图 / 点击 / 输入，注册到全局注册表） ---------
// 桌面助手（AiPanel domain="desktop"）通过注册表调用这些方法，实现对远端桌面的
// 程序化控制。与人工输入共用 sendTx / sessionAlive 判活：会话结束后一律抛错，
// AI 工具执行器把错误回填给模型（而非静默吞掉）。

/** 取活动会话（AI 控制用）：未连接/已断开时抛错，由执行器转成 ok=false 结果。 */
function requireSession(): { s: NonNullable<RdpSession>; b: RdpBackend } {
  if (!session || !backend || !sessionAlive) {
    throw new Error("RDP 会话未连接或已断开");
  }
  return { s: session, b: backend };
}

/** 远端桌面分辨率（画布 backing 像素，即截图与点击共用的坐标系）。 */
function desktopSize(): { width: number; height: number } | null {
  const canvas = canvasRef.value;
  if (!canvas || !canvas.width || !canvas.height) return null;
  return { width: canvas.width, height: canvas.height };
}

/** 截取当前画面为 PNG base64（画布由 WASM 本机绘制，无跨源污染，toBlob 可用）。 */
function captureScreenshot(): Promise<{ width: number; height: number; base64: string }> {
  const canvas = canvasRef.value;
  requireSession();
  if (!canvas || !canvas.width || !canvas.height) {
    return Promise.reject(new Error("画布尚无画面（桌面未渲染完成）"));
  }
  return new Promise((resolve, reject) => {
    canvas.toBlob((blob) => {
      if (!blob) {
        reject(new Error("画布导出失败"));
        return;
      }
      const reader = new FileReader();
      reader.onload = () => {
        const dataUrl = reader.result;
        if (typeof dataUrl !== "string") {
          reject(new Error("截屏编码失败"));
          return;
        }
        const comma = dataUrl.indexOf(",");
        resolve({
          width: canvas.width,
          height: canvas.height,
          base64: dataUrl.slice(comma + 1),
        });
      };
      reader.onerror = () => reject(new Error("截屏读取失败"));
      reader.readAsDataURL(blob);
    }, "image/png");
  });
}

/** AI 点击：坐标即画布 backing 像素（与截图同源）。 */
function controlClick(x: number, y: number, button: number, double: boolean) {
  const { s, b } = requireSession();
  const de = b.Backend.DeviceEvent;
  // 丢弃用户挂起的鼠标移动：否则 rAF 合帧稍后会把旧坐标发到远端，
  // 覆盖 AI 注入的位置，点击落点错乱。
  pendingMove = null;
  const tx = new b.Backend.InputTransaction();
  tx.addEvent(de.mouseMove(x, y));
  const clicks = double ? 2 : 1;
  for (let i = 0; i < clicks; i++) {
    tx.addEvent(de.mouseButtonPressed(button));
    tx.addEvent(de.mouseButtonReleased(button));
  }
  s.applyInputs(tx);
}

/** AI 输入文本：逐字符走 Unicode 通道（与人工 IME 输入同路径）。 */
function controlTypeText(text: string) {
  const { s, b } = requireSession();
  const de = b.Backend.DeviceEvent;
  const tx = new b.Backend.InputTransaction();
  for (const ch of text) {
    tx.addEvent(de.unicodePressed(ch));
    tx.addEvent(de.unicodeReleased(ch));
  }
  s.applyInputs(tx);
}

/** AI 按键：code 为 DOM KeyboardEvent.code，modifiers 为其 code 数组（先按后放）。 */
function controlKeyPress(code: string, modifiers: string[]) {
  const { s, b } = requireSession();
  const de = b.Backend.DeviceEvent;
  const tx = new b.Backend.InputTransaction();
  for (const m of modifiers) {
    const sc = SCANCODES[m];
    if (sc === undefined) throw new Error(`未知修饰键 code: ${m}`);
    tx.addEvent(de.keyPressed(sc));
  }
  const sc = SCANCODES[code];
  if (sc !== undefined) {
    tx.addEvent(de.keyPressed(sc));
    tx.addEvent(de.keyReleased(sc));
  } else if (code.length === 1 && !["Dead", "Unidentified"].includes(code)) {
    // 不在映射表的单字符走 Unicode 通道（与人工输入一致）。
    tx.addEvent(de.unicodePressed(code));
    tx.addEvent(de.unicodeReleased(code));
  } else {
    throw new Error(`未知键 code: ${code}`);
  }
  for (const m of [...modifiers].reverse()) {
    tx.addEvent(de.keyReleased(SCANCODES[m]));
  }
  s.applyInputs(tx);
}

/** 注册进全局注册表的控制句柄（桌面助手按 instanceId 取用）。 */
const desktopControl = {
  size: desktopSize,
  captureScreenshot,
  click: controlClick,
  typeText: controlTypeText,
  keyPress: controlKeyPress,
};
</script>

<template>
  <div
    ref="containerRef"
    class="rdp-pane"
    tabindex="0"
    @pointerdown="onPointerDown"
    @pointerup="onPointerUp"
    @pointermove="onPointerMove"
    @wheel="onWheel"
    @contextmenu.prevent
    @keydown="onKeydown"
    @keyup="onKeyup"
  >
    <canvas ref="canvasRef" class="rdp-canvas"></canvas>
    <div v-if="status === 'connecting'" class="rdp-status">{{ stage }}</div>
    <div class="rdp-toolbar" @pointerdown.stop>
      <span class="rdp-state" :class="status">{{ statusText }}</span>
      <button
        class="rdp-cad"
        :disabled="status !== 'connected'"
        title="发送 Ctrl+Alt+Del"
        @click="sendCtrlAltDel"
      >
        Ctrl+Alt+Del
      </button>
      <button class="rdp-cad" title="全屏 / 退出全屏（F11）" @click="toggleFullscreen">
        {{ isFullscreen ? "退出全屏" : "全屏" }}
      </button>
    </div>
  </div>
</template>

<style scoped lang="scss">
.rdp-pane {
  position: absolute;
  inset: 0;
  background: #000;
  outline: none;
  overflow: hidden;
}
/* WASM 按远端桌面分辨率设置画布 backing 尺寸；CSS 缩放到容器内并保持比例居中。 */
.rdp-canvas {
  position: absolute;
  inset: 0;
  margin: auto;
  max-width: 100%;
  max-height: 100%;
}
.rdp-status {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--el-text-color-secondary);
  font-size: 13px;
  pointer-events: none;
}
/* 元素全屏时铺满屏幕并隐藏残留边距（canvas 随容器自适应居中）。 */
.rdp-pane:fullscreen {
  width: 100%;
  height: 100%;
  border-radius: 0;
}
.rdp-toolbar {
  position: absolute;
  top: 8px;
  right: 12px;
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 4px 10px;
  border-radius: 6px;
  background: rgba(0, 0, 0, 0.45);
  backdrop-filter: blur(4px);
}
.rdp-state {
  font-size: 12px;
  color: #999;
}
.rdp-state.connected {
  color: #67c23a;
}
.rdp-state.error {
  color: #f56c6c;
}
.rdp-cad {
  border: 1px solid rgba(255, 255, 255, 0.35);
  background: transparent;
  color: #ddd;
  font-size: 12px;
  padding: 2px 8px;
  border-radius: 4px;
  cursor: pointer;
}
.rdp-cad:hover:not(:disabled) {
  border-color: var(--el-color-primary);
  color: var(--el-color-primary);
}
.rdp-cad:disabled {
  opacity: 0.4;
  cursor: not-allowed;
}
</style>
