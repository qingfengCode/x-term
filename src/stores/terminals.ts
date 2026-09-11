import { defineStore } from "pinia";
import { computed, reactive, ref } from "vue";
import * as localApi from "@/api/local";
import * as sessionApi from "@/api/session";
import * as terminalApi from "@/api/terminal";
import { AuthType, type Session } from "@/api/types";
import { useSettingsStore } from "@/stores/settings";
import { isAuthCancelled, isAuthError } from "@/utils/error";
import { bytesToBase64 } from "@/utils/binary";
import {
  type SplitDirection,
  type SplitNode,
  collectPaneIds,
  removeLeafNode,
  splitLeafNode,
} from "@/utils/splitLayout";

/**
 * 单个分屏窗格的连接状态（一个终端页签内可拆分出多个窗格，各自持有
 * 独立的后端终端实例 instanceId）。
 */
export interface TerminalPaneState {
  /** 稳定窗格 id（创建时生成，永不变化；重连只更换 instanceId）。 */
  id: string;
  /** 后端终端实例 id（重连会更换）。 */
  instanceId: string;
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
 * 打开的终端 Tab。
 *
 * 每个 tab 绑定一个会话配置，内容区为**分屏窗格树**（`layout` 引用
 * `panes` 中的窗格；未拆分时恒为单叶子）。Tab 关闭时断开全部窗格的
 * 后端实例。桌面会话（VNC/RDP）在桌面页由 desktopTabs store 管理，
 * 不经过本 store。
 *
 * 同一列表也承载**服务器监控页签**（`kind = "monitor"`，见 [`openMonitor`]）：
 * 监控不依赖终端实例，`panes` 恒为空数组。
 *
 * `instanceId / connecting / error / disconnected / reconnecting` 为**活动
 * 窗格的派生值**（getter）：供 TabBar / 工具栏 / AI / MCP 等历史调用方
 * 无改动兼容；窗格级状态请直接读写 `panes` 中对应对象。
 */
export class TerminalTab {
  /** 稳定 tab 标识（创建时生成，永不变化）。
   *
   * Workspace 的 pane 以它为 key：重连/手动认证成功换新 instanceId 时
   * pane 复用同一个 xterm 实例（scrollback 保留），只重绑数据流。 */
  id: string;
  /** 页签类型："terminal"（终端）| "monitor"（服务器监控，无后端终端实例）。 */
  kind: "terminal" | "monitor";
  /** 对应的会话配置。 */
  session: Session;
  /** 分屏窗格状态列表（监控页签为空数组）。 */
  panes: TerminalPaneState[] = [];
  /** 分屏布局树（叶子按 paneId 引用 panes；监控页签不使用）。 */
  layout: SplitNode = { type: "leaf", paneId: "" };
  /** 当前活动窗格 id（点击窗格 / 拆分 / 关闭窗格时切换）。 */
  activePaneId = "";
  /** 最大化（聚焦展示）中的窗格 id；null = 正常分屏布局。 */
  zoomedPaneId: string | null = null;

  constructor(init: {
    id: string;
    kind: "terminal" | "monitor";
    session: Session;
    panes?: TerminalPaneState[];
    layout?: SplitNode;
    activePaneId?: string;
  }) {
    this.id = init.id;
    this.kind = init.kind;
    this.session = init.session;
    if (init.panes) this.panes = init.panes;
    if (init.layout) this.layout = init.layout;
    if (init.activePaneId) this.activePaneId = init.activePaneId;
  }

  /** 活动窗格（activePaneId 失效时回退第一个）。 */
  get activePane(): TerminalPaneState | undefined {
    return this.panes.find((p) => p.id === this.activePaneId) ?? this.panes[0];
  }
  /** 活动窗格的后端终端实例 id（派生值，勿直接赋值——写 panes）。 */
  get instanceId(): string {
    return this.kind === "terminal" ? (this.activePane?.instanceId ?? "") : "";
  }
  /** 任一窗格连接中（页签状态点）。 */
  get connecting(): boolean {
    return this.kind === "terminal" && this.panes.some((p) => p.connecting);
  }
  /** 活动窗格的最近错误。 */
  get error(): string | null {
    return this.activePane?.error ?? null;
  }
  /** 活动窗格是否已断开。 */
  get disconnected(): boolean {
    return !!this.activePane?.disconnected;
  }
  /** 活动窗格是否重连中。 */
  get reconnecting(): boolean {
    return !!this.activePane?.reconnecting;
  }
}

/**
 * 认证失败后的手动认证状态（SshManualAuthDialog 弹窗驱动）。
 *
 * 连接失败为认证类错误（密码错误 / 服务器要求口令码等二次认证）时，保留对应
 * 窗格并置此状态：弹窗收集用户手动输入的密码/验证码，通过
 * `connectSessionWithManualAuth` 重试。重试仍为认证类错误时弹窗保持打开并展示
 * 错误，供用户继续尝试；成功或非认证类错误时关闭。
 */
export interface ManualAuthRequest {
  /** 对应的会话配置。 */
  session: Session;
  /** 保留的 tab（重试成功后原地更新窗格 instanceId，不重挂载终端）。 */
  tab: TerminalTab;
  /** 认证失败的窗格 id（重试成功后原地更新该窗格）。 */
  paneId: string;
  /** 最近一次重试的错误信息（认证类错误时展示在弹窗内）。 */
  error: string | null;
  /** 是否正在重试中（重试进行时仍可取消，见 [`cancelled`]）。 */
  busy: boolean;
  /** 用户已在重试进行中取消：invoke 返回后丢弃结果（连接成功则断开新实例）。 */
  cancelled: boolean;
}

export const useTerminalsStore = defineStore("terminals", () => {
  const tabs = ref<TerminalTab[]>([]);
  /** 当前活动页签 id（稳定 tab.id；终端与监控页签共用同一选中标识）。 */
  const activeTabId = ref<string | null>(null);
  /**
   * 当前活动**终端实例** id（派生值 = 活动页签的活动窗格实例）：
   * - 活动页签是终端 → 活动窗格的 instanceId（占位/连接中为空串）；
   * - 活动页签是监控页签 → null（无终端实例，AI / MCP 应视为"无可用终端"）。
   *
   * 供 AI 助手 / MFA / MCP 等按 instanceId 消费的调用方使用；Workspace 的
   * 内容区显隐请用 [`activeTabId`]（否则监控页签无法命中）。
   */
  const activeId = computed(() => {
    const t = tabs.value.find((x) => x.id === activeTabId.value);
    return t && t.kind === "terminal" ? t.instanceId : null;
  });
  const manualAuth = ref<ManualAuthRequest | null>(null);
  /** 广播输入开关：开启后任意窗格的按键同步到同页签其它已连接窗格。 */
  const broadcastInput = ref(false);

  /** 生成稳定 tab id（与消息 id 同款格式，防碰撞）。 */
  function genTabId() {
    return `t-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  }

  /** 生成稳定窗格 id。 */
  function genPaneId() {
    return `p-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  }

  /** 新建连接中的窗格状态。 */
  function makePane(): TerminalPaneState {
    return {
      id: genPaneId(),
      instanceId: "",
      connecting: true,
      error: null,
      disconnected: false,
      reconnecting: false,
    };
  }

  /**
   * 新建终端页签（单窗格）。
   *
   * reactive 化后再 push：后续 pane.instanceId/connecting 等赋值走代理，
   * 才能触发视图更新（直接持有 raw 对象赋值不经过 Proxy、不触发响应）。
   * 返回的 pane 是 reactive 视图（tab.panes[0]），后续闭包内安全赋值。
   */
  function newTerminalTab(session: Session): { tab: TerminalTab; pane: TerminalPaneState } {
    const pane = makePane();
    const tab = reactive(
      new TerminalTab({
        id: genTabId(),
        kind: "terminal",
        session,
        panes: [pane],
        layout: { type: "leaf", paneId: pane.id },
        activePaneId: pane.id,
      }),
    );
    return { tab, pane: tab.panes[0] };
  }

  /** 新建监控页签（无窗格）。 */
  function newMonitorTab(session: Session): TerminalTab {
    return reactive(new TerminalTab({ id: genTabId(), kind: "monitor", session }));
  }

  /** 按后端实例 id 查找窗格（跨页签；terminal:closed / 重连等入口）。 */
  function findPaneByInstance(
    instanceId: string,
  ): { tab: TerminalTab; pane: TerminalPaneState } | undefined {
    if (!instanceId) return undefined;
    for (const t of tabs.value) {
      const pane = t.panes.find((p) => p.instanceId === instanceId);
      if (pane) return { tab: t, pane };
    }
    return undefined;
  }

  /**
   * 打开一个新终端 tab 并连接。
   *
   * @returns 是否连接成功；认证失败时返回 `false`（tab 保留显示错误，并弹出
   * 手动认证框），调用方不要提示"已连接"。
   */
  async function open(session: Session): Promise<boolean> {
    // 同一会话已有"单窗格占位/认证失败"tab 时先移除，避免 v-for key 冲突
    // （tab.instanceId || tab.session.id）与重复连接；若其手动认证弹窗
    // 还开着则一并关闭。
    // **只匹配单窗格且未连接成功的终端 tab**：`t.instanceId` 现在是活动窗格
    // 的派生值——多窗格页签（活动窗格连接中/认证失败）命中会误删整个页签且
    // 不断开兄弟窗格（幽灵连接）；监控页签 instanceId 恒空，同样不能命中
    // （否则打开终端会静默关掉同会话的监控页签）。
    const stale = tabs.value.find(
      (t) =>
        t.kind === "terminal" &&
        t.panes.length === 1 &&
        !t.panes[0].instanceId &&
        t.session.id === session.id
    );
    if (stale) {
      const si = tabs.value.indexOf(stale);
      if (si >= 0) {
        tabs.value.splice(si, 1);
        if (manualAuth.value?.tab === stale) manualAuth.value = null;
      }
    }
    const { tab, pane } = newTerminalTab(session);
    tabs.value.push(tab);
    activeTabId.value = tab.id;

    try {
      const instanceId = await sessionApi.connectSession(session.id);
      // 连接期间 tab 可能已被用户关闭（TabBar 用稳定 tab.id 关闭占位
      // tab，close 已移除它）。此时新实例在后端已经注册成功，必须立即断开，
      // 否则泄漏一个无人管理的幽灵连接。
      if (!tabs.value.includes(tab)) {
        try {
          await sessionApi.disconnectSession(instanceId);
        } catch {
          /* 忽略清理错误 */
        }
        throw new Error("连接已取消（终端已关闭）");
      }
      pane.instanceId = instanceId;
      // 连接成功回到该页签（连接期间用户可能已切走；与旧行为一致，
      // 避免"点了连接却停在别的页签"）。
      activeTabId.value = tab.id;
      // 启动脚本：连接成功后自动执行（此前该字段只有存储/编辑 UI，
      // 从未被执行——用户填了脚本却静默无效）。
      runStartupScript(instanceId, session.startupScript);
      // 记录最近成功连接的会话（供 Ctrl+T 快速重连 / 侧栏"最近"列表）。
      const settings = useSettingsStore();
      settings.recordRecentSession(session.id);
      void settings.save().catch(() => {});
      return true;
    } catch (e) {
      pane.error = String(e);
      if (isAuthError(e)) {
        // 用户在二次认证弹窗点「取消」= 明确放弃本次连接：移除占位 tab 恢复
        // 原状，**不**再自动弹手动认证框——挑战弹窗关闭动画尚未结束时立即
        // 再弹一个 Dialog，Element Plus 遮罩/焦点陷阱清理交错会残留，导致
        // 后续页面点击失效。用户想重试可从会话树再次连接。
        if (isAuthCancelled(e)) {
          const idx = tabs.value.indexOf(tab);
          if (idx >= 0) tabs.value.splice(idx, 1);
          if (manualAuth.value?.tab === tab) manualAuth.value = null;
          if (activeTabId.value === tab.id) {
            activeTabId.value = tabs.value[0]?.id ?? null;
          }
          return false;
        }
        // 认证失败（密码错误 / 服务器要求口令码等二次认证）：保留 tab 显示
        // 错误，并弹出手动认证框让用户输入凭据重试。不向上抛错——弹窗本身
        // 就是失败反馈，避免调用方（SessionSidebar）再弹"连接失败"提示；
        // 返回 false 防止误弹"已连接"。
        pane.connecting = false;
        manualAuth.value = {
          session,
          tab,
          paneId: pane.id,
          error: null,
          busy: false,
          cancelled: false,
        };
        return false;
      }
      const idx = tabs.value.indexOf(tab);
      if (idx >= 0) tabs.value.splice(idx, 1);
      if (activeTabId.value === tab.id) activeTabId.value = tabs.value[0]?.id ?? null;
      throw e;
    } finally {
      pane.connecting = false;
    }
  }

  /**
   * 「复制 SSH 通道」：在一个已连接的 SSH 终端 tab 的**同一条已认证**
   * 连接上开新 channel，不重新认证（二次认证服务器无需再输口令码）。
   *
   * 源不是已连接的 SSH（Telnet/本地/占位/已断开）或克隆失败时，回退
   * `open()` 全量重连（重新认证，可能再次要求口令码）。
   * 「复制」（新开一条独立连接的标签页）直接用 `open()`，不走本函数。
   *
   * @returns 是否连接成功（语义与 open 一致）。
   */
  async function cloneChannel(source: TerminalTab): Promise<boolean> {
    const srcPane = source.panes.find((p) => !!p.instanceId && !p.disconnected);
    if (source.session.protocol !== "ssh" || !srcPane) {
      return open(source.session);
    }
    const { tab, pane } = newTerminalTab(source.session);
    tabs.value.push(tab);
    activeTabId.value = tab.id;

    // 用户在克隆完成前关闭占位 tab 的标记：该场景是"放弃"，不能落入
    // catch 的回退分支（否则会无视用户取消、强行重新 open 一条新连接）。
    let userCancelled = false;

    try {
      const instanceId = await sessionApi.cloneTerminalSession(srcPane.instanceId);
      // 克隆期间 tab 可能已被用户关闭：新实例在后端已注册成功，立即断开，
      // 避免幽灵连接。
      if (!tabs.value.includes(tab)) {
        try {
          await sessionApi.disconnectSession(instanceId);
        } catch {
          /* 忽略清理错误 */
        }
        userCancelled = true;
        throw new Error("连接已取消（终端已关闭）");
      }
      pane.instanceId = instanceId;
      activeTabId.value = tab.id;
      // 与 open() 行为一致：执行启动脚本 + 记录最近会话。
      runStartupScript(instanceId, source.session.startupScript);
      const settings = useSettingsStore();
      settings.recordRecentSession(source.session.id);
      void settings.save().catch(() => {});
      return true;
    } catch {
      if (userCancelled) {
        // 用户已放弃：移除占位 tab 后直接结束，不再回退重连。
        const idx = tabs.value.indexOf(tab);
        if (idx >= 0) tabs.value.splice(idx, 1);
        if (activeTabId.value === tab.id) activeTabId.value = tabs.value[0]?.id ?? null;
        return false;
      }
      // 克隆失败（源刚好断开 / 服务器限制多开 channel 等）：移除占位 tab，
      // 回退全量重连（open 会重建占位 tab；认证失败走手动认证弹窗）。
      const idx = tabs.value.indexOf(tab);
      if (idx >= 0) tabs.value.splice(idx, 1);
      if (manualAuth.value?.tab === tab) manualAuth.value = null;
      if (activeTabId.value === tab.id) activeTabId.value = tabs.value[0]?.id ?? null;
      return open(source.session);
    } finally {
      pane.connecting = false;
    }
  }

  /** 标记某窗格已断开（由 TerminalPane 的 closed 事件触发）。 */
  function markDisconnected(instanceId: string) {
    const hit = findPaneByInstance(instanceId);
    if (hit) hit.pane.disconnected = true;
  }

  /** 断开 tab 全部窗格的后端终端实例（best-effort，已断开/不存在则忽略）。 */
  async function stopBackend(tab: TerminalTab) {
    for (const p of tab.panes) {
      if (!p.instanceId) continue;
      try {
        await sessionApi.disconnectSession(p.instanceId);
      } catch {
        /* 已断开或已被 close() 清理，忽略 */
      }
    }
  }

  /**
   * 执行会话配置的启动脚本（连接成功后调用）。
   *
   * 直接写入 PTY：tty 的输入队列会保留字节，shell 完成初始化（读取
   * profile/banner）后按序消费，无需轮询提示符。末尾追加 CR 触发执行。
   */
  function runStartupScript(instanceId: string, script: string | null | undefined) {
    const cmd = script?.trim();
    if (!cmd || !instanceId) return;
    const b64 = bytesToBase64(new TextEncoder().encode(cmd + "\r"));
    terminalApi.terminalWrite(instanceId, b64).catch(() => {
      /* 连接瞬间断开等：脚本执行失败不阻断会话 */
    });
  }

  /** 合成占位会话（不来自 DB，仅承载 tab 展示信息）。
   *
   * 本地终端的 id 也必须唯一：它是 TabBar 复合键（instanceId || session.id）
   * 在连接期间的取值——两个本地终端同时连接中若 id 都是空串，会出现 v-for
   * key 冲突，且 close("")/setActive("") 会命中第一个占位 tab（关错/切错）。 */
  function placeholderSession(
    protocol: Session["protocol"],
    name: string,
    host: string,
    port: number,
  ): Session {
    return {
      id: `local-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
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
    const { tab, pane } = newTerminalTab(localPlaceholderSession());
    tabs.value.push(tab);
    activeTabId.value = tab.id;

    try {
      const instanceId = await localApi.connectLocalTerminal(shell);
      // 连接期间 tab 可能已被用户关闭（TabBar 用稳定 tab.id 关闭占位
      // tab，close 已移除它）。此时新实例在后端已经注册成功，必须立即断开，
      // 否则泄漏一个无人管理的幽灵连接。
      if (!tabs.value.includes(tab)) {
        try {
          await sessionApi.disconnectSession(instanceId);
        } catch {
          /* 忽略清理错误 */
        }
        throw new Error("连接已取消（终端已关闭）");
      }
      pane.instanceId = instanceId;
      activeTabId.value = tab.id;
    } catch (e) {
      pane.error = String(e);
      const idx = tabs.value.indexOf(tab);
      if (idx >= 0) tabs.value.splice(idx, 1);
      if (activeTabId.value === tab.id) activeTabId.value = tabs.value[0]?.id ?? null;
      throw e;
    } finally {
      pane.connecting = false;
    }
  }

  /**
   * 处理后端 terminal:closed 事件：标记对应窗格断开，并 best-effort 断开
   * 后端实例。
   *
   * 远端断开连接后，后端只是 reader 任务退出，`state.terminals` 里的 session
   * 仍驻留（供"重连"用）。若用户从不重连也不关 tab，后端实例会一直挂着。
   * 这里顺手断开：重连流程本身会先断开旧实例（404 会被忽略），无副作用。
   */
  async function handleTerminalClosed(instanceId: string) {
    markDisconnected(instanceId);
    const hit = findPaneByInstance(instanceId);
    if (hit) {
      try {
        await sessionApi.disconnectSession(instanceId);
      } catch {
        /* 已断开或已被清理，忽略 */
      }
    }
  }

  /**
   * 重连已断开的终端窗格。
   *
   * best-effort 断开旧实例 → 用同一会话配置重新连接 → 原地更新窗格的
   * instanceId。窗格以稳定 pane id 为 key，instanceId 更新只会触发 pane 内部
   * 重绑数据流（见 TerminalPane 的 instanceId watch）——**xterm 实例与
   * scrollback 完整保留**，不再有"重连丢回滚"的代价。
   * 连接等待期间窗格/tab 被关闭则断开新实例并放弃重连（见函数体内检查）。
   */
  async function reconnect(instanceId: string) {
    const hit = findPaneByInstance(instanceId);
    if (!hit || hit.pane.reconnecting) return;
    const { tab, pane } = hit;
    pane.reconnecting = true;
    try {
      // 清理旧的后端实例（可能已死，best-effort）。
      try {
        if (pane.instanceId) await sessionApi.disconnectSession(pane.instanceId);
      } catch {
        /* 旧实例可能已断开，忽略 */
      }
      const newId =
        tab.session.protocol === "local"
          ? await localApi.connectLocalTerminal()
          : await sessionApi.connectSession(tab.session.id);
      // 重连等待期间 tab/窗格可能已被关闭。此时 newId 在后端已注册成功但
      // 无人管理，必须立即断开并放弃本次重连，否则泄漏幽灵连接。
      if (!tabs.value.includes(tab) || !tab.panes.includes(pane)) {
        try {
          await sessionApi.disconnectSession(newId);
        } catch {
          /* 忽略清理错误 */
        }
        return;
      }
      pane.instanceId = newId;
      pane.disconnected = false;
      pane.error = null;
      tab.activePaneId = pane.id;
      activeTabId.value = tab.id;
      // 重连后同样执行启动脚本（与首次连接行为一致）。
      runStartupScript(newId, tab.session.startupScript);
    } catch (e) {
      pane.error = String(e);
      if (isAuthError(e)) {
        // 用户在二次认证弹窗点「取消」：保持断开状态即可，不弹手动认证框
        // （理由同 open 的取消分支——避免快速关/开 Dialog 的遮罩残留）。
        if (isAuthCancelled(e)) return;
        // 认证失败：保持断开状态，弹手动认证框让用户输入凭据重试。
        manualAuth.value = {
          session: tab.session,
          tab,
          paneId: pane.id,
          error: null,
          busy: false,
          cancelled: false,
        };
        return;
      }
      throw e;
    } finally {
      pane.reconnecting = false;
    }
  }

  // --- 分屏窗格操作 -------------------------------------------------------

  /**
   * 拆分活动窗格：在指定方向（row=右侧 / col=下侧）新增一个窗格并连接。
   *
   * 新窗格连接策略：SSH 且存在已连接窗格 → 克隆通道（同一条已认证连接开
   * 新 channel，免重认证，二次认证服务器无需再输口令码），克隆失败回退
   * 全量重连；本地终端 → 新开本地 Shell；其余（Telnet 等）→ 全量连接。
   * 认证失败保留窗格弹手动认证框；用户取消则移除窗格恢复布局；其它错误
   * 保留窗格展示错误（可关闭窗格重试），不整页弹错。
   */
  async function splitPane(tab: TerminalTab, dir: SplitDirection) {
    if (tab.kind !== "terminal") return;
    const source = tab.activePane;
    if (!source) return;
    // 最大化状态下先还原，避免新窗格落在被隐藏的区域。
    if (tab.zoomedPaneId) tab.zoomedPaneId = null;
    tab.panes.push(makePane());
    const pane = tab.panes[tab.panes.length - 1];
    tab.layout = splitLeafNode(tab.layout, source.id, dir, pane.id);
    tab.activePaneId = pane.id;

    try {
      // 供克隆的已连接窗格（活动窗格未连接时退而求其次）。
      const donor = tab.panes.find(
        (p) => p !== pane && p.instanceId && !p.disconnected
      );
      let instanceId: string;
      if (tab.session.protocol === "local") {
        instanceId = await localApi.connectLocalTerminal();
      } else if (tab.session.protocol === "ssh" && donor) {
        try {
          instanceId = await sessionApi.cloneTerminalSession(donor.instanceId);
        } catch {
          instanceId = await sessionApi.connectSession(tab.session.id);
        }
      } else {
        instanceId = await sessionApi.connectSession(tab.session.id);
      }
      // 拆分期间窗格/tab 被关闭：新实例在后端已注册成功，立即断开回收。
      if (!tabs.value.includes(tab) || !tab.panes.includes(pane)) {
        try {
          await sessionApi.disconnectSession(instanceId);
        } catch {
          /* 忽略清理错误 */
        }
        return;
      }
      pane.instanceId = instanceId;
      runStartupScript(instanceId, tab.session.startupScript);
    } catch (e) {
      if (!tabs.value.includes(tab) || !tab.panes.includes(pane)) return;
      if (isAuthError(e)) {
        // 用户取消二次认证挑战：移除新窗格恢复原布局。
        if (isAuthCancelled(e)) {
          removePaneLocal(tab, pane.id);
          return;
        }
        pane.error = String(e);
        manualAuth.value = {
          session: tab.session,
          tab,
          paneId: pane.id,
          error: null,
          busy: false,
          cancelled: false,
        };
        return;
      }
      // 非认证错误：保留窗格展示错误（窗格头部可关闭重试）。
      pane.error = String(e);
    } finally {
      pane.connecting = false;
    }
  }

  /** 从窗格列表与布局树中移除窗格（不断开后端实例，供内部复用）。 */
  function removePaneLocal(tab: TerminalTab, paneId: string) {
    const idx = tab.panes.findIndex((p) => p.id === paneId);
    if (idx < 0) return;
    tab.panes.splice(idx, 1);
    const next = removeLeafNode(tab.layout, paneId);
    if (next) tab.layout = next;
    if (tab.zoomedPaneId === paneId) tab.zoomedPaneId = null;
    // 窗格减少到单个时最大化态失去意义：清掉残留——否则单窗格页签头部常驻
    // （showHeader 条件含 zoomedPaneId），且 toggleZoomPane 的 length<2 守卫
    // 会把最大化卡死在无法还原的状态。
    if (tab.panes.length < 2) tab.zoomedPaneId = null;
    if (tab.activePaneId === paneId) tab.activePaneId = tab.panes[0]?.id ?? "";
    if (manualAuth.value?.tab === tab && manualAuth.value.paneId === paneId) {
      manualAuth.value = null;
    }
  }

  /** 关闭指定窗格（最后一个窗格时关闭整个页签）。 */
  async function closePane(tab: TerminalTab, paneId: string) {
    if (tab.kind !== "terminal") return;
    const pane = tab.panes.find((p) => p.id === paneId);
    if (!pane) return;
    if (tab.panes.length <= 1) {
      await close(tab.id);
      return;
    }
    removePaneLocal(tab, paneId);
    if (pane.instanceId) {
      try {
        await sessionApi.disconnectSession(pane.instanceId);
      } catch {
        /* 已断开或已被清理，忽略 */
      }
    }
  }

  /** 激活窗格（点击窗格 / 快捷键导航时切换活动窗格）。 */
  function setActivePane(tab: TerminalTab, paneId: string) {
    if (tab.kind !== "terminal") return;
    if (tab.panes.some((p) => p.id === paneId)) tab.activePaneId = paneId;
  }

  /** 最大化/还原窗格（多窗格时可用；还原由同一开关翻转）。 */
  function toggleZoomPane(tab: TerminalTab, paneId: string) {
    if (tab.kind !== "terminal" || tab.panes.length < 2) return;
    tab.zoomedPaneId = tab.zoomedPaneId === paneId ? null : paneId;
  }

  /** 按布局顺序在窗格间导航（+1 = 下一个，-1 = 上一个，循环）。 */
  function focusPaneRelative(tab: TerminalTab, delta: 1 | -1) {
    if (tab.kind !== "terminal" || tab.panes.length < 2) return;
    const order = collectPaneIds(tab.layout);
    if (order.length < 2) return;
    const cur = Math.max(0, order.indexOf(tab.activePaneId));
    const next = order[(cur + delta + order.length) % order.length];
    if (!next) return;
    tab.activePaneId = next;
    // 最大化态下导航：视图跟随切到目标窗格。否则活动窗格会落到 display:none
    // 的隐藏窗格——键盘输入/工具栏/AI 全部作用于不可见终端。
    if (tab.zoomedPaneId) tab.zoomedPaneId = next;
  }

  /** 切换广播输入开关。 */
  function toggleBroadcast() {
    broadcastInput.value = !broadcastInput.value;
  }

  /**
   * 关闭 tab。返回是否找到并移除了目标（找不到由调用方提示）。
   *
   * key 优先为稳定 tab id（TabBar 现用它作键，同一会话多开时唯一）；
   * 也兼容 instanceId、session.id 或 TabBar 的复合键（instanceId || session.id）。
   *
   * **必须先匹配 tab.id**：复合键时代占位 tab 的键是 session.id，而同一
   * 会话多开（复制通道）时源 tab 的 session.id 相同——若 session.id 先于
   * 精确 id 匹配，关闭占位 tab 会命中源 tab（关错对象，两个 tab 一起失效）。
   * session.id 兜底仍保留：覆盖"连接完成的瞬间旧复合键落空"的竞态（见下）。
   */
  async function close(key: string): Promise<boolean> {
    const idx = tabs.value.findIndex(
      (t) =>
        t.id === key ||
        // 监控页签 instanceId 为空串：跳过模糊匹配，避免 key="" 时误命中。
        (t.kind === "terminal" && t.instanceId === key) ||
        (t.kind === "terminal" && (t.instanceId || t.session.id) === key) ||
        t.session.id === key
    );
    if (idx < 0) return false;
    const [removed] = tabs.value.splice(idx, 1);
    // 弹窗引用的 tab 被关闭：一并关闭弹窗，避免指向已移除的 tab。
    if (manualAuth.value?.tab === removed) manualAuth.value = null;
    await stopBackend(removed);
    if (activeTabId.value === removed.id) {
      activeTabId.value = tabs.value[idx]?.id ?? tabs.value[idx - 1]?.id ?? null;
    }
    return true;
  }

  /**
   * 从失败面板重新打开手动认证弹窗（tab 已存在于列表，连接未成功）。
   * 已有弹窗打开时忽略。paneId 指定认证失败的窗格；缺省取第一个未连接
   * 成功的窗格。
   */
  function openManualAuth(tab: TerminalTab, paneId?: string) {
    if (manualAuth.value) return;
    const pane =
      (paneId ? tab.panes.find((p) => p.id === paneId) : undefined) ??
      tab.panes.find((p) => !p.instanceId) ??
      tab.activePane;
    if (!pane || pane.instanceId) return;
    manualAuth.value = {
      session: tab.session,
      tab,
      paneId: pane.id,
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
   * 使用手动输入的密码/验证码重试认证失败的连接（作用于认证失败的窗格）。
   *
   * 成功：原地更新窗格的 instanceId 并关闭弹窗（窗格复用同一 xterm 实例，
   * 重挂载属于已知代价）；失败且仍为认证类错误：弹窗保持打开并展示
   * 错误供用户继续尝试；其余错误（网络等）：关闭弹窗，错误展示在窗格。
   * 重试期间被取消（[`ManualAuthRequest::cancelled`]）或窗格被关闭时，丢弃
   * 本次结果（连接成功则断开新实例）。
   *
   * @returns 是否连接成功。
   */
  async function retryWithManualAuth(password: string, otp: string): Promise<boolean> {
    const req = manualAuth.value;
    if (!req || req.busy) return false;
    const { session, tab } = req;
    // 认证失败的窗格（tab/窗格可能在弹窗期间被用户关闭）。
    const pane = tab.panes.find((p) => p.id === req.paneId);
    if (!tabs.value.includes(tab) || !pane) {
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
      // 重试期间被取消，或 tab/窗格被关闭：新实例已注册成功但无人管理，
      // 立即断开，避免幽灵连接。
      if (req.cancelled || !tabs.value.includes(tab) || !tab.panes.includes(pane)) {
        try {
          await sessionApi.disconnectSession(instanceId);
        } catch {
          /* 忽略清理错误 */
        }
        return false;
      }
      pane.instanceId = instanceId;
      pane.disconnected = false;
      pane.error = null;
      tab.activePaneId = pane.id;
      activeTabId.value = tab.id;
      runStartupScript(instanceId, tab.session.startupScript);
      manualAuth.value = null;
      return true;
    } catch (e) {
      // 已取消：弹窗已关闭，错误不再展示。
      if (req.cancelled) return false;
      pane.error = String(e);
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

  /** 关闭某个会话配置对应的全部已开 tab（删除会话配置时调用）。
   *  含该会话的监控页签（会话配置已删除，监控也失去意义）。 */
  async function closeBySessionId(sessionId: string) {
    const matches = tabs.value.filter((t) => t.session.id === sessionId);
    for (const t of matches) {
      await close(t.id);
    }
  }

  /**
   * 激活 tab。
   *
   * key 优先为稳定 tab id（同 close 的说明——同一会话多开时 session.id /
   * 复合键有歧义，精确 id 必须最先匹配）；也兼容 instanceId、session.id
   * 与复合键（连接完成的瞬间旧 key 落空的竞态由 session.id 兜底消除）。
   */
  function setActive(key: string) {
    const tab = tabs.value.find(
      (t) =>
        t.id === key ||
        (t.kind === "terminal" && (t.instanceId || t.session.id) === key) ||
        (t.kind === "terminal" && t.instanceId === key) ||
        t.session.id === key
    );
    if (tab) activeTabId.value = tab.id;
  }

  /**
   * 打开服务器监控页签（仅 SSH 会话；监控面板自建 SSH 连接采集，与终端实例
   * 无关）。同一会话配置已开则直接激活，不重复开（多开同一台机器的监控没有
   * 意义，且会加倍占用连接）。
   *
   * 页签切到后台时监控继续采集（Workspace 用 v-show 常驻），关闭页签时
   * MonitorPanel 卸载即停止采集。
   */
  function openMonitor(session: Session) {
    const exist = tabs.value.find(
      (t) => t.kind === "monitor" && t.session.id === session.id
    );
    if (exist) {
      activeTabId.value = exist.id;
      return;
    }
    const tab = newMonitorTab(session);
    tabs.value.push(tab);
    activeTabId.value = tab.id;
  }

  /**
   * 移动 tab（拖拽排序）：把 fromKey 移到 toKey 之前/之后。
   * 拖拽中会连续触发，若目标已被移动过则按当前索引重算，保持跟手。
   * key 匹配优先稳定 tab.id（同 close 的说明），兼容复合键/instanceId/
   * session.id 兜底。
   */
  function moveTab(fromKey: string, toKey: string, before: boolean) {
    const match = (k: string) =>
      tabs.value.findIndex(
        (t) =>
          t.id === k ||
          (t.kind === "terminal" && t.instanceId === k) ||
          (t.kind === "terminal" && (t.instanceId || t.session.id) === k) ||
          t.session.id === k
      );
    const from = match(fromKey);
    const to = match(toKey);
    if (from < 0 || to < 0 || from === to) return;
    const [tab] = tabs.value.splice(from, 1);
    let idx = match(toKey);
    if (!before) idx += 1;
    tabs.value.splice(idx, 0, tab);
  }

  return {
    tabs,
    activeTabId,
    activeId,
    manualAuth,
    broadcastInput,
    open,
    cloneChannel,
    openLocal,
    openMonitor,
    close,
    closeBySessionId,
    setActive,
    markDisconnected,
    handleTerminalClosed,
    reconnect,
    moveTab,
    openManualAuth,
    cancelManualAuth,
    retryWithManualAuth,
    splitPane,
    closePane,
    setActivePane,
    toggleZoomPane,
    focusPaneRelative,
    toggleBroadcast,
  };
});
