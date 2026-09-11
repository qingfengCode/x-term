<script setup lang="ts">
import { computed, nextTick, onActivated, onBeforeUnmount, onDeactivated, provide, reactive, ref, watch } from "vue";
import { useTerminalsStore, type TerminalTab } from "@/stores/terminals";
import { useSettingsStore } from "@/stores/settings";
import TerminalPane from "@/components/TerminalPane.vue";
import SplitArea from "@/components/SplitArea.vue";
import MonitorPanel from "@/components/MonitorPanel.vue";
import AiPanel from "@/components/AiPanel.vue";
import TabBar, { type TabBarItem } from "@/components/TabBar.vue";
import { ElMessage } from "element-plus";
import { eventToCombo, isModifierOnly, matchesCombo } from "@/utils/shortcut";
import type { ShortcutCommand } from "@/api/types";
import type { SplitDirection } from "@/utils/splitLayout";

// KeepAlive 按 name 匹配缓存本组件（保留终端助手面板状态）。
defineOptions({ name: "Workspace" });

const terminals = useTerminalsStore();
const settings = useSettingsStore();

/** 当前活动页签（终端或服务器监控）。 */
const active = computed(() =>
  terminals.tabs.find((t) => t.id === terminals.activeTabId)
);

/** 当前活动终端页签（终端专属动作/工具栏作用对象；监控页签时为 undefined）。 */
const activeTerminal = computed(() =>
  active.value?.kind === "terminal" ? active.value : undefined
);

// 各窗格的 TerminalPane 引用（按 `tabId::paneId` 复合键索引——instanceId
// 在重连时会更换，用它做 key 会在重连瞬间误删新引用；窗格 id 永不变化）。
// reactive Map 保证增删触发 activePaneRef 重新求值。
const paneRefs = reactive(new Map<string, InstanceType<typeof TerminalPane>>());

/** TerminalPaneItem 经 inject 调用：登记/移除窗格的 TerminalPane 实例。 */
provide("xterm:registerPaneRef", (tabId: string, paneId: string, el: unknown) => {
  const key = `${tabId}::${paneId}`;
  if (el) paneRefs.set(key, el as InstanceType<typeof TerminalPane>);
  else paneRefs.delete(key);
});

/** 当前活动窗格的 pane 引用（工具动作作用于它；监控页签时为 undefined）。 */
const activePaneRef = computed(() => {
  const t = active.value;
  if (!t || t.kind !== "terminal" || !t.activePaneId) return undefined;
  return paneRefs.get(`${t.id}::${t.activePaneId}`);
});

// 程序化激活页签/窗格后把焦点交给新活动 pane：只有用户手动点 tab 才走
// TabBar @select 的 focus 路径，复制标签页 / 克隆通道 / 会话树连接 / 重连 /
// 关闭后回退邻 tab / 拆分窗格都无人还焦。watch 兜底覆盖全部路径（含
// @select，重复 focus 无害）。activeId 覆盖"活动窗格的实例就绪"（拆分出的
// 新窗格连接完成后自动聚焦）。
watch(
  [() => terminals.activeTabId, () => terminals.activeId],
  () => {
    void nextTick(() => {
      // 焦点已在其它文本输入上下文（AI 面板输入框、搜索框、对话框等）时不抢：
      // 后台连接完成的自动激活不该打断用户正在进行的输入。点击其它窗格时
      // xterm 自身已完成聚焦，这里跳过同样安全。
      const el = document.activeElement as HTMLElement | null;
      if (
        el &&
        el !== document.body &&
        (el.tagName === "INPUT" ||
          el.tagName === "TEXTAREA" ||
          el.isContentEditable)
      ) {
        return;
      }
      // 监控页签 / 占位页签没有 pane（activePaneRef 为 undefined）——安全 no-op。
      activePaneRef.value?.focus();
    });
  },
);

// --- Tab 栏（共享 TabBar 组件） ---------------------------------------------

/**
 * TabBar 的激活键：当前活动 tab 的稳定 id。
 *
 * TabBar 的键统一用 tab.id（创建时生成、永不变化、同一会话多开也唯一）——
 * 原复合键 `instanceId || session.id` 在复制通道/同会话多开时有歧义：占位
 * tab 的键等于源 tab 的 session.id，点击/关闭占位会命中源 tab（关错对象）。
 */
const activeTabKey = computed(() => terminals.activeTabId ?? "");

/** 映射为 TabBar 的数据抽象（键 = 稳定 tab.id，见 activeTabKey 说明）。 */
const tabItems = computed<TabBarItem[]>(() =>
  terminals.tabs.map((t) => ({
    key: t.id,
    title: t.kind === "monitor" ? `监控 · ${t.session.name}` : t.session.name,
    // 监控页签无连接状态语义：用图标区分、不显示状态圆点（面板内容才是状态）。
    icon: t.kind === "monitor" ? "DataLine" : undefined,
    hideDot: t.kind === "monitor",
    connecting: t.connecting,
    disconnected: t.disconnected,
    // 「复制」/「复制 SSH 通道」：仅终端页签（监控页签无连接可复制）。
    duplicable: t.kind === "terminal" && t.session.protocol === "ssh",
    cloneable:
      t.kind === "terminal" &&
      t.session.protocol === "ssh" &&
      !!t.instanceId &&
      !t.disconnected,
  })),
);

/**
 * TabBar 右键菜单命令（作用于对应 tab）。
 *
 * key 为 TabBar 的键（稳定 tab.id）；保留复合键兜底以防过期引用。
 */
function onTabMenuCommand(cmd: string, key: string) {
  const t = terminals.tabs.find(
    (x) => x.id === key || (x.kind === "terminal" && (x.instanceId || x.session.id) === key)
  );
  if (!t) return;
  // 终端专属命令（复制/克隆通道/重连）：监控页签无连接，直接忽略
  // （菜单项也已按 duplicable/cloneable 隐藏，这里兜底防误触发）。
  const isTerminal = t.kind === "terminal";
  switch (cmd) {
    case "cloneChannel":
      if (!isTerminal) break;
      // 复制 SSH 通道：在同一条已认证连接上开新 channel（不重新认证，二次
      // 认证服务器无需再输口令码）；源未连接/克隆失败时由 cloneChannel
      // 回退全量重连。
      terminals
        .cloneChannel(t)
        .catch((e) => ElMessage.error("复制 SSH 通道失败: " + String(e)));
      break;
    case "duplicate":
      if (!isTerminal) break;
      // 复制：用同一会话配置新开一条**独立连接**的终端标签页（重新认证）。
      // Sidebar 的"复制"复制的是配置记录，这里是运行时新开连接。
      terminals
        .open(t.session)
        .catch((e) => ElMessage.error("打开终端失败: " + String(e)));
      break;
    case "close":
      void terminals.close(t.id);
      break;
    case "closeOthers":
      for (const x of [...terminals.tabs]) {
        if (x !== t) {
          void terminals.close(x.id);
        }
      }
      break;
    case "closeAll":
      for (const x of [...terminals.tabs]) {
        void terminals.close(x.id);
      }
      break;
    case "reconnect":
      if (isTerminal && t.instanceId) void terminals.reconnect(t.instanceId);
      break;
  }
}

// 工具栏动作。
function clearActive() {
  activePaneRef.value?.clear();
  activePaneRef.value?.focus();
}
async function reconnectActive() {
  const t = activeTerminal.value;
  if (!t?.instanceId) return;
  try {
    await terminals.reconnect(t.instanceId);
  } catch (e) {
    /* 错误已存进窗格 error */
  }
}
function zoom(delta: number) {
  // 作用于活动面板的字号覆盖（每 tab 独立、不写全局设置、不持久化）。
  activePaneRef.value?.zoomFont(delta);
}

// --- 分屏 + 广播输入 ------------------------------------------------------

/** 工具栏/快捷键触发：拆分活动窗格（row=右侧 / col=下侧）。 */
function splitActive(dir: SplitDirection) {
  const t = activeTerminal.value;
  if (t) void terminals.splitPane(t, dir);
}

/**
 * 窗格原始按键 → 广播路由：开启广播后同步到同页签其它已连接窗格。
 * 源窗格已自行写入（TerminalPane.onData），目标窗格走 receiveBroadcast
 * （影子缓冲同步 + 写远端，不触发建议/焦点副作用）。
 */
function onPaneInput(tab: TerminalTab, fromPaneId: string, data: string) {
  if (!terminals.broadcastInput) return;
  for (const p of tab.panes) {
    if (p.id === fromPaneId || !p.instanceId) continue;
    paneRefs.get(`${tab.id}::${p.id}`)?.receiveBroadcast(data);
  }
}

// --- 快捷命令栏 ---------------------------------------------------------

/** 当前激活的分组标签（"__all__" 表示全部）。 */
const activeGroup = ref("__all__");

/** 是否存在多个分组（决定是否显示标签行）。 */
const hasGroups = computed(() => settings.shortcutGroups.length > 0);

/** 当前标签下可见的快捷命令。 */
const visibleShortcuts = computed(() => {
  if (activeGroup.value === "__all__") return settings.shortcuts;
  return settings.shortcuts.filter((sc) => sc.group === activeGroup.value);
});

/** 把命令文本中的占位符 {host}/{user}/{port} 按当前活动会话替换。 */
function resolveCommand(cmd: string): string {
  const s = active.value?.session;
  // 本地终端无会话配置（合成占位 Session），跳过占位符替换。
  if (!s || s.protocol === "local") return cmd;
  return cmd
    .replaceAll("{host}", s.host)
    .replaceAll("{user}", s.username)
    .replaceAll("{port}", String(s.port));
}

/** 打开本地终端标签页（默认 shell 在设置中配置）。 */
async function openLocalTerminal() {
  try {
    await terminals.openLocal();
  } catch (e) {
    // "连接已取消"（连接期间关闭 tab）静默——不是失败。
    if (String(e).includes("连接已取消")) return;
    ElMessage.error(String(e));
  }
}

/** 向活动终端发送一条快捷命令（开启广播时同步到本页签全部已连接窗格）。 */
function runShortcut(sc: ShortcutCommand) {
  const t = activeTerminal.value;
  if (!t?.instanceId) return;
  const cmd = resolveCommand(sc.command);
  if (terminals.broadcastInput && t.panes.length > 1) {
    for (const p of t.panes) {
      if (p.instanceId) paneRefs.get(`${t.id}::${p.id}`)?.sendCommand(cmd);
    }
    return;
  }
  activePaneRef.value?.sendCommand(cmd);
}

// --- 快捷命令栏：展开/折叠（状态持久化） --------------------------------

/** 展开状态来自设置（持久化，重启后保持）。 */
const scExpanded = computed(() => settings.shortcutBarExpanded);

function toggleScExpand() {
  settings.shortcutBarExpanded = !settings.shortcutBarExpanded;
  void settings.save();
}

// --- 快捷命令栏：在终端直接添加快捷命令 ----------------------------------

/** 添加命令弹窗（位于分组标签行右侧的 + 按钮）。 */
const addVisible = ref(false);
const newScLabel = ref("");
const newScCommand = ref("");
const newScGroup = ref("");

/** 尽力从终端当前行剥离 shell 提示符，得到用户输入的命令。 */
function stripPrompt(line: string): string {
  const s = line.replace(/\s+$/, "");
  // 提示符常见形态：user@host:~$ / root@host:/opt# / [user@host ~]$ / C:\> / ❯ 等。
  // 特征：提示符字符（$ # % > ❯ λ ➜）后跟空白，且其前含 @、:、]、~、/ 或形如盘符。
  const matches = [...s.matchAll(/[#$%>❯λ➜]\s+/g)];
  for (let i = matches.length - 1; i >= 0; i--) {
    const m = matches[i];
    const before = s.slice(0, m.index);
    if (/[@:\]~/]/.test(before) || /^[A-Za-z]:\\?$/.test(before)) {
      return s.slice(m.index! + m[0].length).trimStart();
    }
  }
  return s;
}

/** 点击 +：预填当前终端输入的命令，打开添加弹窗。 */
function openAddShortcut() {
  const line = activePaneRef.value?.getCurrentLine() ?? "";
  newScCommand.value = stripPrompt(line);
  newScLabel.value = newScCommand.value.split(/\s+/)[0] || "新命令";
  newScGroup.value = activeGroup.value === "__all__" ? "" : activeGroup.value;
  addVisible.value = true;
}

/** 保存新快捷命令（写入设置持久化）。 */
async function saveNewShortcut() {
  const command = newScCommand.value.trim();
  const label = newScLabel.value.trim();
  if (!label && !command) {
    ElMessage.warning("名称和命令不能同时为空");
    return;
  }
  const finalLabel = label || command || "新命令";
  const group = newScGroup.value.trim();
  if (group && !settings.shortcutGroups.includes(group)) {
    settings.addShortcutGroup(group);
  }
  const id = settings.addShortcut(group || undefined);
  settings.updateShortcut(id, { label: finalLabel, command, shortcut: null });
  addVisible.value = false;
  await settings.save();
  ElMessage.success(`已添加快捷命令「${finalLabel}」`);
}

/** 用于全局快捷键监听（自定义快捷命令）。 */
function onGlobalKeydown(e: KeyboardEvent) {
  // 长按连发（e.repeat）只响应首次按键，避免自定义命令被连续执行。
  if (e.repeat) return;
  const target = e.target as HTMLElement | null;
  // 是否聚焦在可编辑元素（AI 输入框/搜索框，以及 xterm 的隐藏 textarea——
  // 后者同样是 TEXTAREA，用 inXterm 区分）。
  const isEditable =
    !!target &&
    (target.tagName === "INPUT" ||
      target.tagName === "TEXTAREA" ||
      target.isContentEditable);
  const inXterm = !!target?.closest(".xterm-wrap");

  // 关闭当前标签（默认 Ctrl+W）：焦点在终端画布内时事件源是 xterm 的隐藏
  // textarea，必须在此捕获阶段拦截 + stopPropagation——xterm 收不到就不会
  // 把 Ctrl+W（\x17 删词）发往远端。**其它可编辑元素**（AI 输入框/搜索框）
  // 放行原按键，不再误关标签。拦截与否跟随「设置 → 快捷键」的 closeTab
  // 绑定：用户清除/改绑后这里不再抢按键。
  const closeTabCombo = settings.getAppShortcut("closeTab");
  if (closeTabCombo && matchesCombo(e, closeTabCombo) && (!isEditable || inXterm)) {
    if (!active.value) return;
    e.preventDefault();
    e.stopPropagation();
    void terminals.close(active.value.id);
    return;
  }
  // --- 分屏窗格快捷键（固定 Alt+Shift 组合，参考 Windows Terminal） ---
  // 焦点在终端画布内（xterm 隐藏 textarea）也响应——Alt+Shift 组合与终端
  // 应用冲突少；捕获阶段拦截防止 Alt 转义序列发给远端。
  if (e.altKey && e.shiftKey && !e.ctrlKey && !e.metaKey && (!isEditable || inXterm)) {
    const t = activeTerminal.value;
    if (t) {
      let handled = true;
      if (e.key === "+" || e.key === "=") {
        void terminals.splitPane(t, "row");
      } else if (e.key === "-" || e.key === "_") {
        void terminals.splitPane(t, "col");
      } else if (e.key === "w" || e.key === "W") {
        void terminals.closePane(t, t.activePaneId);
      } else if (e.key === "Enter") {
        terminals.toggleZoomPane(t, t.activePaneId);
      } else if (e.key === "b" || e.key === "B") {
        terminals.toggleBroadcast();
      } else if (e.key === "ArrowRight" || e.key === "ArrowDown") {
        terminals.focusPaneRelative(t, 1);
      } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
        terminals.focusPaneRelative(t, -1);
      } else {
        handled = false;
      }
      if (handled) {
        e.preventDefault();
        e.stopPropagation();
        return;
      }
    }
  }
  // 其余快捷键仅当聚焦在 body 或非可编辑元素时响应，避免与输入框冲突。
  if (isEditable) return;
  const combo = eventToCombo(e);
  if (!combo || isModifierOnly(combo)) return;
  const hit = settings.shortcuts.find((s) => s.shortcut && s.shortcut === combo);
  if (hit) {
    e.preventDefault();
    runShortcut(hit);
    return;
  }
  // 内置标签快捷键（用户自定义命令未占用时生效，与桌面页一致）：
  // Ctrl+1~9 切换标签。焦点在终端画布内时让给远端（终端应用的 Ctrl+组合
  // 冲突更少，且切换有会话侧栏可用）。
  if ((e.ctrlKey || e.metaKey) && !e.altKey) {
    if (!inXterm) {
      if (/^[1-9]$/.test(e.key)) {
        const idx = Number(e.key) - 1;
        const tab = terminals.tabs[idx];
        if (tab) {
          e.preventDefault();
          terminals.setActive(tab.id);
        }
      }
    }
  }
}

// 本组件被 KeepAlive 缓存（MainLayout），切到其他页面时不会卸载——若在
// onMounted 里注册全局监听，切走后自定义命令快捷键仍会在后台终端执行命令。
// 改为随页面激活/停用注册/注销（tab 右键菜单的关闭监听由 TabBar 组件自理）。
// 注册用捕获阶段：Ctrl+W 要抢在 xterm 的 textarea 处理之前拦截（否则删词
// 已发往远端，关标签变成"关标签 + 删远端一个词"）。
onActivated(() => {
  window.addEventListener("keydown", onGlobalKeydown, true);
});
onDeactivated(() => {
  window.removeEventListener("keydown", onGlobalKeydown, true);
});
// 兜底：组件真正销毁（如应用退出）时确保清理。
onBeforeUnmount(() => {
  window.removeEventListener("keydown", onGlobalKeydown, true);
});
</script>

<template>
  <div class="workspace">
    <div class="tab-bar">
      <TabBar
        :tabs="tabItems"
        :active-key="activeTabKey"
        empty-hint="从左侧会话树双击连接"
        @select="
          (k) => {
            terminals.setActive(k);
            // 切 tab 后把焦点交给新活动终端：否则焦点留在隐藏的旧 pane 的
            // textarea 上，新终端光标不显示（要再点一下才有）。
            void nextTick(() => activePaneRef?.focus());
            // 二次校验列宽：恢复可见瞬间的首次 fit 可能取到半布局的失效
            // 测量（cols 偏小且停留），与 PTY 列宽不一致会让 shell 局部重绘
            // 错位出残影。布局稳定后由 pane 重新对齐一次。
            void nextTick(() => activePaneRef?.refit());
          }
        "
        @close="(k) => void terminals.close(k)"
        @move="(from, to, before) => terminals.moveTab(from, to, before)"
        @command="onTabMenuCommand"
      />
      <!-- 本地终端：常驻按钮，无论有无会话都可用 -->
      <button class="local-term-btn" title="打开本地终端（默认 Shell 可在设置中配置）" @click="openLocalTerminal">
        <el-icon><Monitor /></el-icon>
        <span>本地终端</span>
      </button>
      <!-- 终端工具栏（仅终端页签；监控页签无清屏/字号/重连语义） -->
      <div v-if="activeTerminal" class="term-toolbar">
        <el-tooltip content="向右分屏 (Alt+Shift+=)" placement="bottom">
          <el-button class="tool-btn" link @click="splitActive('row')">
            <el-icon class="rot-90"><Menu /></el-icon>
          </el-button>
        </el-tooltip>
        <el-tooltip content="向下分屏 (Alt+Shift+-)" placement="bottom">
          <el-button class="tool-btn" link @click="splitActive('col')">
            <el-icon><Menu /></el-icon>
          </el-button>
        </el-tooltip>
        <el-tooltip
          :content="terminals.broadcastInput ? '关闭广播输入 (Alt+Shift+B)' : '广播输入：按键同步到本页签全部窗格 (Alt+Shift+B)'"
          placement="bottom"
        >
          <el-button
            class="tool-btn"
            link
            :class="{ 'is-on': terminals.broadcastInput }"
            @click="terminals.toggleBroadcast()"
          >
            <el-icon><Bell /></el-icon>
          </el-button>
        </el-tooltip>
        <span class="tool-sep" />
        <el-tooltip content="清屏" placement="bottom">
          <el-button class="tool-btn" link @click="clearActive"><el-icon><Delete /></el-icon></el-button>
        </el-tooltip>
        <el-tooltip content="字体增大" placement="bottom">
          <el-button class="tool-btn" link @click="zoom(1)"><el-icon><ZoomIn /></el-icon></el-button>
        </el-tooltip>
        <el-tooltip content="字体减小" placement="bottom">
          <el-button class="tool-btn" link @click="zoom(-1)"><el-icon><ZoomOut /></el-icon></el-button>
        </el-tooltip>
        <el-tooltip content="重连" placement="bottom" v-if="activeTerminal.disconnected">
          <el-button class="tool-btn" link :loading="activeTerminal.reconnecting" @click="reconnectActive">
            <el-icon><Refresh /></el-icon>
          </el-button>
        </el-tooltip>
      </div>
    </div>
    <div class="workspace-body">
      <div class="panes">
        <div
          v-for="tab in terminals.tabs"
          :key="tab.id"
          v-show="tab.id === terminals.activeTabId"
          class="pane"
        >
          <!-- 服务器监控页签：自建 SSH 连接采集，与终端实例无关；v-show 常驻
               使其切到后台仍持续采集（曲线不中断），关闭页签即卸载停止 -->
          <div v-if="tab.kind === 'monitor'" class="monitor-pane">
            <MonitorPanel :session-config-id="tab.session.id" />
          </div>
          <!-- 终端页签：分屏窗格树 + 稳定挂载的窗格实例（连接中/失败/断开
               重连等状态由各窗格内部渲染） -->
          <SplitArea
            v-else
            :tab="tab"
            @pane-input="(pid: string, d: string) => onPaneInput(tab, pid, d)"
          />
        </div>
        <div v-if="!active" class="workspace-empty">
          还没有打开任何终端。请从左侧会话树连接一台服务器。
        </div>
      </div>
      <!-- 终端助手面板：仅在终端页显示，与 DB 助手完全隔离 -->
      <AiPanel domain="ssh" />
    </div>
    <!-- 终端底部快捷命令栏（仅终端页签；监控页签无终端可发命令） -->
    <div v-if="activeTerminal" class="shortcut-bar">
      <!-- 分组标签行：分组在左，右侧为「添加快捷命令」按钮 -->
      <div class="sc-tabs">
        <template v-if="hasGroups">
          <button
            class="sc-tab"
            :class="{ active: activeGroup === '__all__' }"
            @click="activeGroup = '__all__'"
          >
            全部
          </button>
          <button
            v-for="g in settings.shortcutGroups"
            :key="g"
            class="sc-tab"
            :class="{ active: activeGroup === g }"
            @click="activeGroup = g"
          >
            {{ g }}
          </button>
        </template>
        <span class="sc-tabs-spacer" />
        <!-- 添加命令弹窗：预填当前终端输入的命令 -->
        <el-popover v-model:visible="addVisible" placement="top-start" :width="320" trigger="click">
          <div class="sc-add-form">
            <div class="sc-add-field">
              <span class="sc-add-label">命令</span>
              <el-input
                v-model="newScCommand"
                size="small"
                placeholder="要发送的命令"
                spellcheck="false"
              />
            </div>
            <div class="sc-add-field">
              <span class="sc-add-label">名称</span>
              <el-input
                v-model="newScLabel"
                size="small"
                placeholder="按钮显示名称（默认取命令首词）"
              />
            </div>
            <div class="sc-add-field">
              <span class="sc-add-label">分组</span>
              <el-select
                v-model="newScGroup"
                size="small"
                filterable
                allow-create
                default-first-option
                clearable
                placeholder="未分组"
                style="flex: 1"
              >
                <el-option v-for="g in settings.shortcutGroups" :key="g" :label="g" :value="g" />
              </el-select>
            </div>
            <div class="sc-add-actions">
              <el-button size="small" @click="addVisible = false">取消</el-button>
              <el-button size="small" type="primary" @click="saveNewShortcut">添加</el-button>
            </div>
          </div>
          <template #reference>
            <button class="sc-add" title="添加快捷命令">
              <el-icon><Plus /></el-icon>
            </button>
          </template>
        </el-popover>
      </div>
      <!-- 命令按钮行：折叠时超出宽度隐藏；展开时自动往下换行显示全部 -->
      <div class="sc-row">
        <div class="sc-buttons" :class="{ expanded: scExpanded }">
          <el-tooltip
            v-for="sc in visibleShortcuts"
            :key="sc.id"
            :content="sc.shortcut ? `${sc.command}  (${sc.shortcut})` : sc.command"
            placement="top"
            :show-after="400"
          >
            <button class="sc-btn" @click="runShortcut(sc)">
              <span class="sc-label">{{ sc.label }}</span>
              <span v-if="sc.shortcut" class="sc-key">{{ sc.shortcut }}</span>
            </button>
          </el-tooltip>
          <span v-if="visibleShortcuts.length === 0" class="sc-empty">
            {{ settings.shortcuts.length === 0 ? "暂无快捷命令，点击上方 + 添加" : "该分组暂无命令" }}
          </span>
        </div>
        <button
          v-if="settings.shortcuts.length > 0"
          class="sc-toggle"
          :title="scExpanded ? '收起（单行显示）' : '展开（多行换行）'"
          @click="toggleScExpand"
        >
          <el-icon><component :is="scExpanded ? 'ArrowUp' : 'ArrowDown'" /></el-icon>
          <span>{{ scExpanded ? "收起" : "展开" }}</span>
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped lang="scss">
.workspace {
  display: flex;
  flex-direction: column;
  width: 100%;
  height: 100%;
  overflow: hidden;
}
.tab-bar {
  display: flex;
  align-items: center;
  height: 38px;
  background: var(--el-bg-color-overlay);
  border-bottom: 1px solid var(--el-border-color-lighter);
  padding: 0 8px;
  flex-shrink: 0;
}
/* 本地终端按钮：tab 栏右侧常驻（描边按钮，悬停主色化）。 */
.local-term-btn {
  display: flex;
  align-items: center;
  gap: 5px;
  height: 26px;
  padding: 0 12px;
  margin-left: 6px;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  background: var(--el-fill-color-blank);
  color: var(--el-text-color-regular);
  font-size: 12px;
  cursor: pointer;
  flex-shrink: 0;
  white-space: nowrap;
  transition: border-color 0.15s ease, color 0.15s ease, background-color 0.15s ease;
}
.local-term-btn:hover {
  border-color: var(--el-color-primary-light-5);
  color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
}
.term-toolbar {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 0 6px 0 8px;
  border-left: 1px solid var(--el-border-color-lighter);
  margin-left: 4px;
  flex-shrink: 0;
}
.tool-btn {
  padding: 5px;
  border-radius: 6px;
  color: var(--el-text-color-secondary);
  transition: background-color 0.15s ease, color 0.15s ease;
}
.tool-btn:hover {
  color: var(--el-color-primary);
  background: var(--el-fill-color-light);
}
/* 广播输入开启态 */
.tool-btn.is-on {
  color: var(--el-color-primary);
}
/* 工具按钮分组分隔线 */
.tool-sep {
  width: 1px;
  height: 16px;
  margin: 0 4px;
  background: var(--el-border-color-lighter);
}
/* 向右分屏图标（横线图标旋转 90° = 垂直分隔） */
.rot-90 {
  transform: rotate(90deg);
}
.workspace-body {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: row;
  overflow: hidden;
}
.panes {
  flex: 1;
  min-width: 0;
  position: relative;
  background: var(--el-bg-color-page);
}
.pane {
  position: absolute;
  inset: 0;
}
/* 监控页签容器：撑满 pane 并可滚动（面板本身是内容高度布局，
   矮窗口/多指标时需要滚动而不是被裁切） */
.monitor-pane {
  height: 100%;
  overflow-y: auto;
  padding: 14px 16px;
  box-sizing: border-box;
}

/* --- 终端底部快捷命令栏 --- */
.shortcut-bar {
  display: flex;
  flex-direction: column;
  background: var(--el-bg-color-overlay);
  border-top: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
}
.sc-tabs {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 4px 10px 0;
  border-bottom: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
}
.sc-tabs-spacer {
  flex: 1;
}
/* 「添加快捷命令」按钮（分组右侧） */
.sc-add {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 22px;
  height: 22px;
  margin-left: 6px;
  border: 1px dashed var(--el-border-color);
  border-radius: 6px;
  background: transparent;
  color: var(--el-text-color-secondary);
  font-size: 14px;
  line-height: 1;
  cursor: pointer;
  transition: all 0.15s;
}
.sc-add:hover {
  border-color: var(--el-color-primary);
  color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
}
.sc-tab {
  padding: 3px 10px;
  border: none;
  border-bottom: 2px solid transparent;
  background: transparent;
  color: var(--el-text-color-secondary);
  font-size: 12px;
  cursor: pointer;
  white-space: nowrap;
  transition: all 0.15s;
}
.sc-tab:hover {
  color: var(--el-color-primary);
}
.sc-tab.active {
  color: var(--el-color-primary);
  border-bottom-color: var(--el-color-primary);
  font-weight: 500;
}
/* 命令按钮行：按钮区（可折叠/展开）+ 展开收起按钮 */
.sc-row {
  display: flex;
  align-items: center;
  min-width: 0;
}
.sc-buttons {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 10px;
  flex: 1;
  min-width: 0;
  /* 折叠：单行，超出宽度直接隐藏（不出现滚动条） */
  flex-wrap: nowrap;
  overflow: hidden;
}
.sc-buttons.expanded {
  /* 展开：自动往下换行显示全部命令 */
  flex-wrap: wrap;
  overflow: visible;
}
.sc-toggle {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  margin: 0 8px 0 2px;
  padding: 3px 8px;
  border: none;
  border-radius: 4px;
  background: transparent;
  color: var(--el-text-color-secondary);
  font-size: 12px;
  cursor: pointer;
  white-space: nowrap;
  flex-shrink: 0;
  transition: all 0.15s;
}
.sc-toggle:hover {
  color: var(--el-color-primary);
  background: var(--el-fill-color);
}
.sc-empty {
  font-size: 12px;
  color: var(--el-text-color-placeholder);
}
.sc-btn {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 3px 10px;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  background: var(--el-fill-color-blank);
  color: var(--el-text-color-regular);
  font-size: 12px;
  cursor: pointer;
  white-space: nowrap;
  transition: all 0.15s;
}
.sc-btn:hover {
  border-color: var(--el-color-primary-light-5);
  color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
}
.sc-btn:active {
  transform: translateY(1px);
}
.sc-label {
  font-family: var(--app-font-mono);
}
.sc-key {
  font-size: 10px;
  padding: 1px 4px;
  border-radius: 4px;
  background: var(--el-fill-color-dark);
  color: var(--el-text-color-secondary);
  font-family: var(--app-font-mono);
}

/* --- 添加命令弹窗 --- */
.sc-add-form {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.sc-add-field {
  display: flex;
  align-items: center;
  gap: 8px;
}
.sc-add-label {
  flex-shrink: 0;
  width: 34px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
  text-align: right;
}
.sc-add-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 2px;
}
</style>
