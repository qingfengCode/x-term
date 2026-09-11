<!--
  VncPane.vue — 内嵌 VNC 查看器（终端页标签页内容）。

  用 noVNC 的 RFB 把远端桌面渲染到画布，经后端 WS↔TCP 桥接（vnc_bridge_start）
  连接目标 VNC 服务端。与 TerminalPane 对齐：props 传实例 id + 连接信息，
  断线时 emit closed，由 Workspace 统一标记断开并停止桥接。
-->
<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { ElMessage } from "element-plus";
import RFB from "@novnc/novnc";

const props = defineProps<{
  /** 桥接实例 id（与 tab.instanceId 一致）。 */
  instanceId: string;
  /** noVNC 直连的 ws 地址（后端 vnc_bridge_start 返回）。 */
  wsUrl: string;
  /**
   * VNC 用户名（可选）。macOS 屏幕共享「账户登录」只提供 Apple ARD 认证
   * （security type 30），必须同时带用户名+口令；普通 VNC Auth 只校验口令。
   */
  username?: string;
  /** VNC 口令（可选；服务端要求认证时由 RFB 握手使用，不经过后端）。 */
  password?: string;
}>();
const emit = defineEmits<{ (e: "closed"): void; (e: "connected"): void }>();

const containerRef = ref<HTMLDivElement | null>(null);
const status = ref<"connecting" | "connected" | "disconnected" | "error">("connecting");
/** 连接中覆盖层显示的分阶段提示（诊断用：挂载/构造/拨号哪一步卡住一目了然）。 */
const stage = ref("连接中…");
/** 最近一次握手失败原因（补丁 noVNC 内部 _fail 捕获，用于界面提示与日志）。 */
const lastError = ref<string | null>(null);
/** 是否已因认证失败提示过（避免断开时再弹一次重复文案）。 */
let authFailed = false;

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

let rfb: RFB | null = null;

// --- 认证输入（macOS 屏幕共享等要求 username+password 的服务端） -----------
// noVNC 在服务端要求认证且缺少凭据时会停下握手并派发 credentialsrequired，
// 这里改成页面内输入继续（sendCredentials 恢复握手），而不是直接断开。
const credPrompt = ref(false);
const credUsername = ref("");
const credPassword = ref("");

/** 提交页面内输入的凭据，恢复被暂停的认证握手。 */
function submitCreds() {
  if (!rfb) return;
  rfb.sendCredentials({
    username: credUsername.value.trim() || undefined,
    password: credPassword.value,
  });
  credPrompt.value = false;
}

/** 取消认证：主动断开（走 disconnect 事件统一通知外层）。 */
function cancelCreds() {
  credPrompt.value = false;
  rfb?.disconnect();
}

// --- 缩放模式：fit 适配容器（等比，noVNC 内部缩放）/ original 原始大小（可滚动）/
// stretch 拉伸填满（非等比，CSS 强制 canvas 尺寸）。 ---
const scaleMode = ref<"fit" | "original" | "stretch">("fit");
const SCALE_MODES: { value: "fit" | "original" | "stretch"; label: string }[] = [
  { value: "fit", label: "适配" },
  { value: "original", label: "原始" },
  { value: "stretch", label: "拉伸" },
];

function cycleScaleMode() {
  const cur = SCALE_MODES.findIndex((m) => m.value === scaleMode.value);
  const next = SCALE_MODES[(cur + 1) % SCALE_MODES.length].value;
  scaleMode.value = next;
  if (!rfb) return;
  // fit 由 noVNC 内部等比缩放；其余模式关闭内部缩放，尺寸交给容器 CSS 控制。
  rfb.scaleViewport = next === "fit";
  // 远端分辨率跟随窗口（fit/stretch）：noVNC 请求服务端把桌面调到容器尺寸，
  // 解码/绘制面积降到容器大小，是高分辨率远端卡顿的主要解药（需服务端支持
  // ExtendedDesktopSize，不支持时自动忽略）。original 模式保留原生分辨率看全图。
  rfb.resizeSession = next !== "original";
}

/**
 * 流畅模式：降低 JPEG 质量与压缩级别，显著减少带宽和编解码 CPU（远程办公/低带宽
 * 场景）。noVNC 默认 quality=6 / compression=2，流畅模式取 quality=2 / 不压缩。
 * 分辨率不变，只牺牲画面细节（文字仍可读）。
 */
const smooth = ref(false);
function toggleSmooth() {
  smooth.value = !smooth.value;
  if (!rfb) return;
  if (smooth.value) {
    rfb.qualityLevel = 2;
    rfb.compressionLevel = 0;
  } else {
    rfb.qualityLevel = 6;
    rfb.compressionLevel = 2;
  }
}

onMounted(() => {
  const target = containerRef.value;
  if (!target) return;

  stage.value = "正在连接 " + (props.wsUrl || "(空地址!)");
  try {
    rfb = new RFB(target, props.wsUrl, {
      credentials: { username: props.username, password: props.password },
      shared: true,
      // 缩放到容器尺寸：noVNC 自带 ResizeObserver，容器变化时自动重算。
      scaleViewport: true,
      // 请求服务端把桌面分辨率调整为窗口尺寸（支持 ExtendedDesktopSize 的服务端
      // 生效）：画布像素 = 容器像素，避免高分辨率远端缩放绘制拖垮主线程。
      resizeSession: true,
    });
  } catch (e) {
    console.error("[vnc-pane] RFB 构造失败:", e);
    stage.value = "客户端构造失败：" + String(e);
    status.value = "error";
    emit("closed");
    return;
  }
  rfb.focusOnClick = true; // 点击画布抓取键盘焦点

  // noVNC 的 _fail 只把原因写进内部日志、不随事件暴露；补丁捕获它以便
  // 在界面/console 直接看到失败点（如 "Unsupported security types (types: 30,33,35,36)"）。
  const rawRfb = rfb as unknown as { _fail?: (details: string) => boolean };
  const origFail = rawRfb._fail?.bind(rfb);
  if (typeof origFail === "function") {
    rawRfb._fail = (details: string) => {
      lastError.value = details;
      console.error("[vnc-pane] RFB 失败:", details);
      return origFail(details);
    };
  }

  rfb.addEventListener("connect", () => {
    status.value = "connected";
    // 已连上即通知外层抹除内存中的明文口令（RFB 握手已完成）。
    emit("connected");
  });
  rfb.addEventListener("credentialsrequired", (e) => {
    // 服务端要求认证且缺凭据：macOS 屏幕共享（ARD）需要 username+password，
    // 部分服务端只要求口令。统一弹出输入层，预填连接里保存过的值，
    // 用户补齐后 sendCredentials 继续握手，不再直接断开。
    const need = (e as unknown as CustomEvent<{ types?: string[] }>).detail?.types ?? [];
    stage.value = need.includes("username")
      ? "服务端要求用户名和密码认证（如 macOS 屏幕共享）"
      : "服务端要求口令";
    credUsername.value = props.username ?? "";
    credPassword.value = props.password ?? "";
    credPrompt.value = true;
  });
  rfb.addEventListener("securityfailure", (e) => {
    authFailed = true;
    const detail = (e as unknown as CustomEvent<{ status?: number; reason?: string }>).detail;
    console.warn("[vnc-pane] 认证失败:", detail);
    ElMessage.error(
      "VNC 认证失败：" +
        (detail?.reason ? `（服务端：${detail.reason}）` : "") +
        "用户名/口令不正确，或服务端拒绝了该连接方式",
    );
  });
  rfb.addEventListener("disconnect", (e) => {
    if (!rfb) return; // 组件卸载主动断开时不重复通知
    credPrompt.value = false; // 认证输入层随断开关闭，避免叠在重连覆盖层下
    const clean = (e as unknown as CustomEvent<{ clean?: boolean }>).detail?.clean ?? true;
    status.value = "disconnected";
    // 非正常断开：把捕获到的失败原因弹给用户（认证失败已在 securityfailure 提示过）。
    if (!clean && !authFailed && lastError.value) {
      ElMessage.error("VNC 连接失败：" + lastError.value);
    }
    emit("closed");
  });

  // Ctrl+V 剪贴板粘贴：捕获阶段截获，避免被 noVNC 当作按键发送给远端。
  target.addEventListener("keydown", onKeydown, true);
});

onBeforeUnmount(() => {
  containerRef.value?.removeEventListener("keydown", onKeydown, true);
  const instance = rfb;
  rfb = null; // 置空：后续 disconnect 回调不再 emit
  instance?.disconnect();
});

/** Ctrl+V：把本地剪贴板文本粘贴给远端（ClientCutText）。 */
function onKeydown(e: KeyboardEvent) {
  if ((e.ctrlKey || e.metaKey) && !e.shiftKey && e.key.toLowerCase() === "v") {
    e.preventDefault();
    e.stopPropagation();
    void pasteClipboard();
  }
}

async function pasteClipboard() {
  if (!rfb) return;
  try {
    const text = await navigator.clipboard.readText();
    if (text) rfb.clipboardPasteFrom(text);
  } catch {
    ElMessage.warning("无法读取剪贴板（WebView 权限受限），请在远端窗口内手动粘贴");
  }
}

/** 发送 Ctrl+Alt+Del（Windows VNC 登录/解锁屏常用）。 */
function sendCtrlAltDel() {
  rfb?.sendCtrlAltDel();
}
</script>

<template>
  <div class="vnc-pane">
    <div
      ref="containerRef"
      class="vnc-screen"
      :class="[`scale-${scaleMode}`]"
    ></div>
    <div v-if="status === 'connecting'" class="vnc-status">{{ stage }}</div>

    <!-- 认证输入覆盖层：macOS 屏幕共享(ARD)等要求 username+password -->
    <div v-if="credPrompt" class="cred-overlay">
      <div class="cred-card">
        <div class="cred-title">需要认证</div>
        <div class="cred-hint">
          macOS 屏幕共享需填 macOS 账户的用户名与密码；普通 VNC 只需填密码。
        </div>
        <input
          v-model="credUsername"
          class="cred-input"
          placeholder="用户名（可选）"
          autocomplete="off"
          spellcheck="false"
        />
        <input
          v-model="credPassword"
          class="cred-input"
          type="password"
          placeholder="密码"
          @keydown.enter="submitCreds"
        />
        <div class="cred-actions">
          <button class="cred-btn" @click="cancelCreds">取消</button>
          <button class="cred-btn primary" @click="submitCreds">连接</button>
        </div>
      </div>
    </div>
    <div class="vnc-toolbar">
      <span class="vnc-state" :class="status">{{ statusText }}</span>
      <button
        class="vnc-cad"
        title="切换缩放模式：适配 / 原始大小 / 拉伸"
        @click="cycleScaleMode"
      >
        {{ SCALE_MODES.find((m) => m.value === scaleMode)?.label }}
      </button>
      <button
        class="vnc-cad"
        :class="{ on: smooth }"
        title="流畅模式：降低画质换取更低带宽与 CPU（远程/低带宽场景）"
        @click="toggleSmooth"
      >
        流畅{{ smooth ? "·开" : "" }}
      </button>
      <button
        class="vnc-cad"
        :disabled="status !== 'connected'"
        title="发送 Ctrl+Alt+Del"
        @click="sendCtrlAltDel"
      >
        Ctrl+Alt+Del
      </button>
    </div>
  </div>
</template>

<style scoped lang="scss">
.vnc-pane {
  position: absolute;
  inset: 0;
  background: #000;
}
/* noVNC 在容器内自行创建屏幕层与居中的画布（含自动缩放），容器只需铺满。 */
.vnc-screen {
  position: absolute;
  inset: 0;
}
/* 原始大小：关闭 noVNC 缩放后画布保持像素尺寸，容器滚动查看。 */
.vnc-screen.scale-original {
  overflow: auto;
}
/* 拉伸：非等比强制铺满容器（覆盖 noVNC 设置的居中尺寸）。 */
.vnc-screen.scale-stretch canvas {
  width: 100% !important;
  height: 100% !important;
  max-width: none !important;
  max-height: none !important;
  margin: 0 !important;
}
.vnc-status {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--el-text-color-secondary);
  font-size: 13px;
  pointer-events: none;
}
/* 认证输入覆盖层（macOS 屏幕共享 ARD 等） */
.cred-overlay {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgba(0, 0, 0, 0.55);
  z-index: 10;
}
.cred-card {
  width: 300px;
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 16px;
  border-radius: 8px;
  background: var(--el-bg-color-overlay);
  box-shadow: 0 6px 20px rgba(0, 0, 0, 0.35);
  color: var(--el-text-color-primary);
}
.cred-title {
  font-size: 14px;
  font-weight: 600;
}
.cred-hint {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  line-height: 1.6;
}
.cred-input {
  width: 100%;
  box-sizing: border-box;
  height: 30px;
  padding: 0 10px;
  font-size: 13px;
  color: var(--el-text-color-primary);
  background: var(--el-bg-color);
  border: 1px solid var(--el-border-color);
  border-radius: 4px;
  outline: none;
}
.cred-input:focus {
  border-color: var(--el-color-primary);
}
.cred-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 2px;
}
.cred-btn {
  border: 1px solid var(--el-border-color);
  background: transparent;
  color: var(--el-text-color-primary);
  font-size: 12px;
  padding: 4px 14px;
  border-radius: 4px;
  cursor: pointer;
}
.cred-btn.primary {
  border-color: var(--el-color-primary);
  background: var(--el-color-primary);
  color: #fff;
}
.cred-btn:hover:not(.primary) {
  border-color: var(--el-color-primary);
  color: var(--el-color-primary);
}
.vnc-toolbar {
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
.vnc-state {
  font-size: 12px;
  color: #999;
}
.vnc-state.connected {
  color: #67c23a;
}
.vnc-cad {
  border: 1px solid rgba(255, 255, 255, 0.35);
  background: transparent;
  color: #ddd;
  font-size: 12px;
  padding: 2px 8px;
  border-radius: 4px;
  cursor: pointer;
}
.vnc-cad:hover:not(:disabled) {
  border-color: var(--el-color-primary);
  color: var(--el-color-primary);
}
.vnc-cad.on {
  border-color: var(--el-color-warning);
  color: var(--el-color-warning);
}
.vnc-cad:disabled {
  opacity: 0.4;
  cursor: not-allowed;
}
</style>
