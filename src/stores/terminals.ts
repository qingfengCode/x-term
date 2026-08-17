import { defineStore } from "pinia";
import { ref } from "vue";
import * as localApi from "@/api/local";
import * as sessionApi from "@/api/session";
import { AuthType, type Session } from "@/api/types";
import { useSettingsStore } from "@/stores/settings";
import { isAuthError } from "@/utils/error";

/**
 * 打开的终端 Tab。
 *
 * 每个 tab 对应一个终端实例（后端返回的 instanceId）。同一会话配置可多开。
 * Tab 关闭时调用 disconnect。桌面会话（VNC/RDP）在桌面页由 desktopTabs store
 * 管理，不经过本 store。
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

/**
 * 认证失败后的手动认证状态（SshManualAuthDialog 弹窗驱动）。
 *
 * 连接失败为认证类错误（密码错误 / 服务器要求口令码等二次认证）时，保留对应
 * tab 并置此状态：弹窗收集用户手动输入的密码/验证码，通过
 * `connectSessionWithManualAuth` 重试。重试仍为认证类错误时弹窗保持打开并展示
 * 错误，供用户继续尝试；成功或非认证类错误时关闭。
 */
export interface ManualAuthRequest {
  /** 对应的会话配置。 */
  session: Session;
  /** 保留的 tab（重试成功后原地更新 instanceId，不重挂载终端）。 */
  tab: TerminalTab;
  /** 最近一次重试的错误信息（认证类错误时展示在弹窗内）。 */
  error: string | null;
  /** 是否正在重试中（重试进行时仍可取消，见 [`cancelled`]）。 */
  busy: boolean;
  /** 用户已在重试进行中取消：invoke 返回后丢弃结果（连接成功则断开新实例）。 */
  cancelled: boolean;
}

export const useTerminalsStore = defineStore("terminals", () => {
  const tabs = ref<TerminalTab[]>([]);
  const activeId = ref<string | null>(null);
  const manualAuth = ref<ManualAuthRequest | null>(null);

  /**
   * 打开一个新终端 tab 并连接。
   *
   * @returns 是否连接成功；认证失败时返回 `false`（tab 保留显示错误，并弹出
   * 手动认证框），调用方不要提示"已连接"。
   */
  async function open(session: Session): Promise<boolean> {
    // 同一会话已有占位/失败 tab（instanceId 为空）时先移除，避免 v-for key
    // 冲突（tab.instanceId || tab.session.id）与重复连接；若其手动认证弹窗
    // 还开着则一并关闭。
    const stale = tabs.value.find(
      (t) => t.instanceId === "" && t.session.id === session.id
    );
    if (stale) {
      const si = tabs.value.indexOf(stale);
      if (si >= 0) {
        tabs.value.splice(si, 1);
        if (manualAuth.value?.tab === stale) manualAuth.value = null;
      }
    }
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
      // 连接期间 tab 可能已被用户关闭（占位 tab 的 instanceId 为空，close("") 会
      // 命中并移除它）。此时新实例在后端已经注册成功，必须立即断开，否则泄漏
      // 一个无人管理的幽灵连接。
      if (!tabs.value.includes(tab)) {
        try {
          await sessionApi.disconnectSession(instanceId);
        } catch {
          /* 忽略清理错误 */
        }
        throw new Error("连接已取消（终端已关闭）");
      }
      tab.instanceId = instanceId;
      activeId.value = instanceId;
      // 记录最近成功连接的会话（供 Ctrl+T 快速重连 / 侧栏"最近"列表）。
      const settings = useSettingsStore();
      settings.recordRecentSession(session.id);
      void settings.save().catch(() => {});
      return true;
    } catch (e) {
      tab.error = String(e);
      if (isAuthError(e)) {
        // 认证失败（密码错误 / 服务器要求口令码等二次认证）：保留 tab 显示
        // 错误，并弹出手动认证框让用户输入凭据重试。不向上抛错——弹窗本身
        // 就是失败反馈，避免调用方（SessionSidebar）再弹"连接失败"提示；
        // 返回 false 防止误弹"已连接"。
        tab.connecting = false;
        manualAuth.value = { session, tab, error: null, busy: false, cancelled: false };
        return false;
      }
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

  /** 断开 tab 对应的后端终端实例（best-effort，已断开/不存在则忽略）。 */
  async function stopBackend(tab: TerminalTab) {
    if (!tab.instanceId) return;
    try {
      await sessionApi.disconnectSession(tab.instanceId);
    } catch {
      /* 已断开或已被 close() 清理，忽略 */
    }
  }

  /** 合成占位会话（不来自 DB，仅承载 tab 展示信息）。 */
  function placeholderSession(
    protocol: Session["protocol"],
    name: string,
    host: string,
    port: number,
  ): Session {
    return {
      id: "",
      name,
      groupId: null,
      host,
      port,
      username: "",
      authType: AuthType.Password,
      credentialId: null,
      keyPath: null,
      jumpSessionId: null,
      startupScript: null,
      tags: null,
      color: null,
      sortOrder: 0,
      createdAt: "",
      updatedAt: "",
      protocol,
    };
  }

  /** 本地终端的合成占位会话。 */
  function localPlaceholderSession(): Session {
    return placeholderSession("local", "本地终端", "localhost", 0);
  }

  /**
   * 打开一个本地终端标签页（cmd / PowerShell / Git Bash）。
   *
   * 本地终端不关联会话配置：用合成的占位 Session（protocol="local"）承载 tab，
   * 且不记录最近会话（Ctrl+T 快速重连仅针对远程会话）。其余流程与 open() 一致
   * （占位 tab、连接期间关闭则回收后端幽灵实例、失败移除 tab 并 re-throw）。
   */
  async function openLocal(shell?: string) {
    const tab: TerminalTab = {
      instanceId: "",
      session: localPlaceholderSession(),
      connecting: true,
      error: null,
      disconnected: false,
      reconnecting: false,
    };
    tabs.value.push(tab);
    activeId.value = tab.instanceId; // 临时空，连接成功后更新

    try {
      const instanceId = await localApi.connectLocalTerminal(shell);
      // 连接期间 tab 可能已被用户关闭（占位 tab 的 instanceId 为空，close("") 会
      // 命中并移除它）。此时新实例在后端已经注册成功，必须立即断开，否则泄漏
      // 一个无人管理的幽灵连接。
      if (!tabs.value.includes(tab)) {
        try {
          await sessionApi.disconnectSession(instanceId);
        } catch {
          /* 忽略清理错误 */
        }
        throw new Error("连接已取消（终端已关闭）");
      }
      tab.instanceId = instanceId;
      activeId.value = instanceId;
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

  /**
   * 处理后端 terminal:closed 事件：标记断开，并 best-effort 断开后端实例。
   *
   * 远端断开连接后，后端只是 reader 任务退出，`state.terminals` 里的 session
   * 仍驻留（供"重连"用）。若用户从不重连也不关 tab，后端实例会一直挂着。
   * 这里顺手断开：重连流程本身会先断开旧实例（404 会被忽略），无副作用。
   */
  async function handleTerminalClosed(instanceId: string) {
    markDisconnected(instanceId);
    const tab = tabs.value.find((t) => t.instanceId === instanceId);
    if (tab) await stopBackend(tab);
  }

  /**
   * 重连已断开的终端 tab。
   *
   * best-effort 断开旧实例 → 用同一会话配置重新连接 → 原地更新 instanceId。
   * 注意：Workspace 的 TerminalPane 以 instanceId 为 key，更新 instanceId 会
   * **重挂载组件、丢失 scrollback**（无后端缓冲回放）。这是当前接受的代价。
   * 连接等待期间 tab 被关闭则断开新实例并放弃重连（见函数体内检查）。
   */
  async function reconnect(instanceId: string) {
    const tab = tabs.value.find((t) => t.instanceId === instanceId);
    if (!tab || tab.reconnecting) return;
    tab.reconnecting = true;
    try {
      // 清理旧的后端实例（可能已死，best-effort）。
      try {
        await stopBackend(tab);
      } catch {
        /* 旧实例可能已断开，忽略 */
      }
      const newId =
        tab.session.protocol === "local"
          ? await localApi.connectLocalTerminal()
          : await sessionApi.connectSession(tab.session.id);
      // 重连等待期间 tab 可能已被用户关闭（close() 已把它移出 tabs 并断开旧
      // 实例）。此时 newId 在后端已注册成功但无人管理，必须立即断开并放弃
      // 本次重连，否则泄漏幽灵连接，且 activeId 会指向不存在的 tab。
      if (!tabs.value.includes(tab)) {
        try {
          await sessionApi.disconnectSession(newId);
        } catch {
          /* 忽略清理错误 */
        }
        return;
      }
      tab.instanceId = newId;
      tab.disconnected = false;
      tab.error = null;
      activeId.value = newId;
    } catch (e) {
      tab.error = String(e);
      if (isAuthError(e)) {
        // 认证失败：保持断开状态，弹手动认证框让用户输入凭据重试。
        manualAuth.value = {
          session: tab.session,
          tab,
          error: null,
          busy: false,
          cancelled: false,
        };
        return;
      }
      throw e;
    } finally {
      tab.reconnecting = false;
    }
  }

  async function close(instanceId: string) {
    const idx = tabs.value.findIndex((t) => t.instanceId === instanceId);
    if (idx < 0) return;
    const [removed] = tabs.value.splice(idx, 1);
    // 弹窗引用的 tab 被关闭：一并关闭弹窗，避免指向已移除的 tab。
    if (manualAuth.value?.tab === removed) manualAuth.value = null;
    await stopBackend(removed);
    if (activeId.value === instanceId) {
      activeId.value = tabs.value[idx]?.instanceId ?? tabs.value[idx - 1]?.instanceId ?? null;
    }
  }

  /**
   * 从失败面板重新打开手动认证弹窗（tab 已存在于列表，连接未成功）。
   * 已有弹窗打开时忽略。
   */
  function openManualAuth(tab: TerminalTab) {
    if (manualAuth.value || tab.instanceId) return;
    manualAuth.value = {
      session: tab.session,
      tab,
      error: null,
      busy: false,
      cancelled: false,
    };
  }

  /**
   * 取消手动认证：关闭弹窗。tab 保留（面板上仍有"手动认证"按钮可重试）。
   *
   * 重试进行中（busy）也允许取消：立即关闭弹窗并标记 `cancelled`，重试的
   * invoke 返回后由 [`retryWithManualAuth`] 丢弃结果（连接成功则断开新实例，
   * 避免幽灵连接）。
   */
  function cancelManualAuth() {
    const req = manualAuth.value;
    if (!req) return;
    if (req.busy) req.cancelled = true;
    manualAuth.value = null;
  }

  /**
   * 使用手动输入的密码/验证码重试认证失败的连接。
   *
   * 成功：原地更新 tab 的 instanceId 并关闭弹窗（terminal pane 以 instanceId
   * 为 key，重挂载属于已知代价）；失败且仍为认证类错误：弹窗保持打开并展示
   * 错误供用户继续尝试；其余错误（网络等）：关闭弹窗，错误展示在终端面板。
   * 重试期间被取消（[`ManualAuthRequest::cancelled`]）或 tab 被关闭时，丢弃
   * 本次结果（连接成功则断开新实例）。
   *
   * @returns 是否连接成功。
   */
  async function retryWithManualAuth(password: string, otp: string): Promise<boolean> {
    const req = manualAuth.value;
    if (!req || req.busy) return false;
    const { session, tab } = req;
    // tab 可能在弹窗期间被用户关闭。
    if (!tabs.value.includes(tab)) {
      manualAuth.value = null;
      return false;
    }
    req.busy = true;
    req.error = null;
    try {
      const instanceId = await sessionApi.connectSessionWithManualAuth(
        session.id,
        password || null,
        otp || null,
      );
      // 重试期间被取消，或 tab 被关闭：新实例已注册成功但无人管理，立即断开，
      // 避免幽灵连接。
      if (req.cancelled || !tabs.value.includes(tab)) {
        try {
          await sessionApi.disconnectSession(instanceId);
        } catch {
          /* 忽略清理错误 */
        }
        return false;
      }
      tab.instanceId = instanceId;
      tab.disconnected = false;
      tab.error = null;
      activeId.value = instanceId;
      manualAuth.value = null;
      return true;
    } catch (e) {
      // 已取消：弹窗已关闭，错误不再展示。
      if (req.cancelled) return false;
      tab.error = String(e);
      if (isAuthError(e)) {
        // 认证类错误：弹窗保持打开，展示错误让用户继续尝试。
        req.error = String(e);
        return false;
      }
      manualAuth.value = null;
      return false;
    } finally {
      req.busy = false;
    }
  }

  function setActive(instanceId: string) {
    activeId.value = instanceId;
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
    manualAuth,
    open,
    openLocal,
    close,
    setActive,
    markDisconnected,
    handleTerminalClosed,
    reconnect,
    moveTab,
    openManualAuth,
    cancelManualAuth,
    retryWithManualAuth,
  };
});
