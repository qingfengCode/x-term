import { defineStore } from "pinia";
import { ref } from "vue";
import * as sessionApi from "@/api/session";
import type { Session } from "@/api/types";

/**
 * 打开的终端 Tab。
 *
 * 每个 tab 对应一个终端实例（后端返回的 instanceId）。同一会话配置可多开。
 * Tab 关闭时调用 disconnect。
 */
export interface TerminalTab {
  /** 后端终端实例 id（来自 connectSession 返回值）。 */
  instanceId: string;
  /** 对应的会话配置。 */
  session: Session;
  /** 是否正在连接中。 */
  connecting: boolean;
  /** 最近一次错误（连接/写入失败）。 */
  error: string | null;
  /** 连接是否已断开（用于显示重连按钮）。 */
  disconnected: boolean;
  /** 是否正在重连中。 */
  reconnecting: boolean;
}

export const useTerminalsStore = defineStore("terminals", () => {
  const tabs = ref<TerminalTab[]>([]);
  const activeId = ref<string | null>(null);

  async function open(session: Session) {
    const tab: TerminalTab = {
      instanceId: "",
      session,
      connecting: true,
      error: null,
      disconnected: false,
      reconnecting: false,
    };
    tabs.value.push(tab);
    activeId.value = tab.instanceId; // 临时空，连接成功后更新

    try {
      const instanceId = await sessionApi.connectSession(session.id);
      tab.instanceId = instanceId;
      activeId.value = instanceId;
    } catch (e) {
      // 连接失败：移除这个占位 tab，并向上抛出错误，避免调用方（如 SessionSidebar）
      // 误以为连接成功而弹出"已连接"提示。之前这里只把错误存进 tab.error 却没有
      // re-throw，导致连不上时仍提示"已连接"。
      tab.error = String(e);
      const idx = tabs.value.indexOf(tab);
      if (idx >= 0) tabs.value.splice(idx, 1);
      if (activeId.value === "") activeId.value = tabs.value[0]?.instanceId ?? null;
      throw e;
    } finally {
      tab.connecting = false;
    }
  }

  /** 标记某 tab 已断开（由 TerminalPane 的 closed 事件触发）。 */
  function markDisconnected(instanceId: string) {
    const tab = tabs.value.find((t) => t.instanceId === instanceId);
    if (tab) tab.disconnected = true;
  }

  /**
   * 重连已断开的终端 tab。
   *
   * best-effort 断开旧实例 → 用同一会话配置重新连接 → 原地更新 instanceId。
   * 注意：Workspace 的 TerminalPane 以 instanceId 为 key，更新 instanceId 会
   * **重挂载组件、丢失 scrollback**（无后端缓冲回放）。这是当前接受的代价。
   */
  async function reconnect(instanceId: string) {
    const tab = tabs.value.find((t) => t.instanceId === instanceId);
    if (!tab || tab.reconnecting) return;
    tab.reconnecting = true;
    try {
      // 清理旧的后端实例（可能已死，best-effort）。
      try {
        await sessionApi.disconnectSession(instanceId);
      } catch {
        /* 旧实例可能已断开，忽略 */
      }
      const newId = await sessionApi.connectSession(tab.session.id);
      tab.instanceId = newId;
      tab.disconnected = false;
      tab.error = null;
      activeId.value = newId;
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
    if (removed.instanceId) {
      try {
        await sessionApi.disconnectSession(removed.instanceId);
      } catch {
        /* 忽略关闭错误 */
      }
    }
    if (activeId.value === instanceId) {
      activeId.value = tabs.value[idx]?.instanceId ?? tabs.value[idx - 1]?.instanceId ?? null;
    }
  }

  function setActive(instanceId: string) {
    activeId.value = instanceId;
  }

  return { tabs, activeId, open, close, setActive, markDisconnected, reconnect };
});
