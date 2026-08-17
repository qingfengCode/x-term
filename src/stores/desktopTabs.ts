import { defineStore } from "pinia";
import { ref } from "vue";
import * as rdpApi from "@/api/rdp";
import * as vncApi from "@/api/vnc";
import type { Desktop } from "@/api/remote_desktop";
import { useSettingsStore } from "@/stores/settings";

/**
 * 桌面页打开的桌面会话标签页（内嵌 VNC/RDP）。
 *
 * 与终端页 terminals store 完全独立：桌面页的连接就在桌面页右侧标签栏管理，
 * 不进入终端页。生命周期与 terminals 的 vnc/rdp 分支一致：
 * 占位标签 → 后端启动桥接（仅监听 127.0.0.1 临时端口）→ 填充 wsUrl/instanceId；
 * 连接期间标签被关闭则停止桥接回收幽灵实例；断开后标签保留、可重连。
 */
export interface DesktopTab {
  /** 桥接实例 id（vncBridgeStart / rdpBridgeStart 返回）。 */
  instanceId: string;
  /** 对应桌面连接 id（用于去重：同一连接再次打开时激活已有标签）。 */
  desktopId: string;
  name: string;
  protocol: "rdp" | "vnc";
  host: string;
  port: number;
  username: string;
  /** 口令只交给前端内嵌客户端（noVNC 握手 / IronRDP CredSSP），不经过后端桥接。 */
  password?: string;
  /** 内嵌客户端直连地址（桥接启动返回，重连时更新）。 */
  wsUrl: string;
  /** 是否正在连接中。 */
  connecting: boolean;
  /** 最近一次错误（连接/启动失败）。 */
  error: string | null;
  /** 连接是否已断开（用于显示重连按钮）。 */
  disconnected: boolean;
  /** 是否正在重连中。 */
  reconnecting: boolean;
  /** 上次使用的 RDP 分辨率（"宽x高"）：打开时来自桌面记录，断开时随事件更新。 */
  desktopSize?: string;
}

export const useDesktopTabsStore = defineStore("desktopTabs", () => {
  const tabs = ref<DesktopTab[]>([]);
  const activeId = ref<string | null>(null);
  // RDP 桥接的证书校验开关来自设置（严格模式：系统信任根验证证书链）。
  const settings = useSettingsStore();

  /** 按协议停止桥接实例（best-effort，已停止/不存在则忽略）。 */
  async function stopBridge(protocol: "rdp" | "vnc", instanceId: string) {
    if (!instanceId) return;
    try {
      if (protocol === "vnc") await vncApi.vncBridgeStop(instanceId);
      else await rdpApi.rdpBridgeStop(instanceId);
    } catch {
      /* 已停止或不存在，忽略 */
    }
  }

  /** 停止标签对应的桥接。 */
  async function stopBackend(tab: DesktopTab) {
    await stopBridge(tab.protocol, tab.instanceId);
  }

  /**
   * 在桌面页打开一个内嵌桌面标签页。
   *
   * 同一连接已开标签时直接激活（不重复建桥接）；否则占位标签 → 启动桥接 →
   * 填充 tab。连接期间标签被关闭则停止桥接回收幽灵实例；失败移除标签并 re-throw。
   */
  async function open(desktop: Desktop, password?: string) {
    const existed = tabs.value.find((t) => t.desktopId === desktop.id);
    if (existed) {
      activeId.value = existed.instanceId;
      return;
    }
    const tab: DesktopTab = {
      instanceId: "",
      desktopId: desktop.id,
      name: desktop.name,
      protocol: desktop.protocol,
      host: desktop.host,
      port: desktop.port,
      username: desktop.username ?? "",
      password,
      wsUrl: "",
      connecting: true,
      error: null,
      disconnected: false,
      reconnecting: false,
      desktopSize: desktop.desktopSize ?? undefined,
    };
    tabs.value.push(tab);
    activeId.value = tab.instanceId; // 临时空，连接成功后更新

    try {
      const info =
        tab.protocol === "vnc"
          ? await vncApi.vncBridgeStart(tab.host, tab.port)
          : await rdpApi.rdpBridgeStart(tab.host, tab.port, settings.terminal.rdpVerifyCert);
      // 连接期间标签可能已被用户关闭（占位标签的 instanceId 为空，close("") 会
      // 命中并移除它）。此时桥接已在后端注册成功，必须立即停止，否则泄漏。
      if (!tabs.value.includes(tab)) {
        await stopBridge(tab.protocol, info.instanceId);
        throw new Error("连接已取消（标签页已关闭）");
      }
      tab.wsUrl = info.wsUrl;
      tab.instanceId = info.instanceId;
      activeId.value = info.instanceId;
    } catch (e) {
      tab.error = String(e);
      const idx = tabs.value.indexOf(tab);
      if (idx >= 0) tabs.value.splice(idx, 1);
      if (activeId.value === "") activeId.value = tabs.value[0]?.instanceId ?? null;
      throw e;
    } finally {
      tab.connecting = false;
    }
  }

  /** 标记某标签已断开（由 VncPane/RdpPane 的 closed 事件触发）。 */
  function markDisconnected(instanceId: string) {
    const tab = tabs.value.find((t) => t.instanceId === instanceId);
    if (tab) tab.disconnected = true;
  }

  /**
   * 记住某标签的 RDP 分辨率（RdpPane sizechange 事件触发）。
   * 只更新内存值；持久化由视图层调 desktopSaveSize 完成。
   */
  function rememberSize(instanceId: string, size: string) {
    const tab = tabs.value.find((t) => t.instanceId === instanceId);
    if (tab && size) tab.desktopSize = size;
  }

  /** 处理后端/客户端断开：标记断开 + best-effort 停止桥接（重连时会再启新桥接）。 */
  async function handleClosed(instanceId: string) {
    markDisconnected(instanceId);
    const tab = tabs.value.find((t) => t.instanceId === instanceId);
    if (tab) await stopBackend(tab);
  }

  /**
   * 重连已断开的桌面标签页。
   *
   * best-effort 停止旧桥接 → 重新启动桥接 → 先更新 wsUrl 再更新 instanceId
   * （pane 以新 instanceId 重挂载时读取）。重连等待期间标签被关闭则停止新桥接
   * 并放弃，避免泄漏幽灵实例。
   */
  async function reconnect(instanceId: string) {
    const tab = tabs.value.find((t) => t.instanceId === instanceId);
    if (!tab || tab.reconnecting) return;
    tab.reconnecting = true;
    try {
      await stopBackend(tab); // 旧桥接可能已死，best-effort
      const info =
        tab.protocol === "vnc"
          ? await vncApi.vncBridgeStart(tab.host, tab.port)
          : await rdpApi.rdpBridgeStart(tab.host, tab.port, settings.terminal.rdpVerifyCert);
      if (!tabs.value.includes(tab)) {
        await stopBridge(tab.protocol, info.instanceId);
        return;
      }
      tab.wsUrl = info.wsUrl;
      tab.instanceId = info.instanceId;
      tab.disconnected = false;
      tab.error = null;
      activeId.value = info.instanceId;
    } catch (e) {
      tab.error = String(e);
      throw e;
    } finally {
      tab.reconnecting = false;
    }
  }

  async function close(instanceId: string) {
    const idx = tabs.value.findIndex((t) => t.instanceId === instanceId);
    if (idx < 0) return;
    const [removed] = tabs.value.splice(idx, 1);
    await stopBackend(removed);
    if (activeId.value === instanceId) {
      activeId.value = tabs.value[idx]?.instanceId ?? tabs.value[idx - 1]?.instanceId ?? null;
    }
  }

  /** 关闭除指定标签外的所有标签。 */
  async function closeOthers(instanceId: string) {
    const keep = tabs.value.find((t) => t.instanceId === instanceId);
    const rest = tabs.value.filter((t) => t.instanceId !== instanceId);
    tabs.value = keep ? [keep] : [];
    await Promise.all(rest.map((t) => stopBackend(t)));
    activeId.value = instanceId;
  }

  /** 关闭所有标签。 */
  async function closeAll() {
    const all = tabs.value.slice();
    tabs.value = [];
    activeId.value = null;
    await Promise.all(all.map((t) => stopBackend(t)));
  }

  /** 关闭某个桌面连接对应的已开标签（删除桌面连接时调用）。 */
  async function closeByDesktopId(desktopId: string) {
    const matches = tabs.value.filter((t) => t.desktopId === desktopId);
    if (!matches.length) return;
    for (const t of matches) await close(t.instanceId);
  }

  function setActive(instanceId: string) {
    activeId.value = instanceId;
  }

  /**
   * 抹除标签内存中的明文口令（pane 连接成功后调用）：口令只在握手阶段需要，
   * 长期驻留 pinia 会被 devtools/内存快照读到。重连时由视图层从 vault 重新
   * 注入（setPassword）。
   */
  function clearPassword(instanceId: string) {
    const tab = tabs.value.find((t) => t.instanceId === instanceId);
    if (tab && tab.password) tab.password = undefined;
  }

  /** 重连前重新注入口令（来自 vault 读取）。 */
  function setPassword(instanceId: string, password?: string) {
    const tab = tabs.value.find((t) => t.instanceId === instanceId);
    if (tab) tab.password = password;
  }

  /**
   * 移动 tab（拖拽排序）：把 fromId 移到 toId 之前/之后。
   * 拖拽中会连续触发，若目标已被移动过则按当前索引重算，保持跟手。
   */
  function moveTab(fromId: string, toId: string, before: boolean) {
    const from = tabs.value.findIndex((t) => t.instanceId === fromId);
    const to = tabs.value.findIndex((t) => t.instanceId === toId);
    if (from < 0 || to < 0 || from === to) return;
    const [tab] = tabs.value.splice(from, 1);
    let idx = tabs.value.findIndex((t) => t.instanceId === toId);
    if (!before) idx += 1;
    tabs.value.splice(idx, 0, tab);
  }

  return {
    tabs,
    activeId,
    open,
    close,
    closeOthers,
    closeAll,
    closeByDesktopId,
    setActive,
    moveTab,
    handleClosed,
    rememberSize,
    reconnect,
    clearPassword,
    setPassword,
  };
});
