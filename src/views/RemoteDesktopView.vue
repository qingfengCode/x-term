<!--
  桌面页：左侧分组树管理 RDP/VNC 连接（独立 CRUD + 自定义分组，不复用终端 sessions），
  右侧多标签就地打开内嵌桌面会话（不再跳转终端页）。

  点击连接按「设置 → 连接」里的客户端选择执行：程序内嵌（本页右侧标签页）或
  系统客户端（RDP: mstsc / VNC: vncviewer）。
-->
<script setup lang="ts">
import {
  computed,
  onActivated,
  onBeforeUnmount,
  onDeactivated,
  onMounted,
  reactive,
  ref,
} from "vue";
import { ElMessage, ElMessageBox, type FormInstance, type FormRules } from "element-plus";
import { Refresh } from "@element-plus/icons-vue";
import { useDesktopsStore } from "@/stores/desktops";
import { useDesktopTabsStore, type DesktopTab } from "@/stores/desktopTabs";
import { useSettingsStore } from "@/stores/settings";
import { remoteDesktopLaunch, desktopSaveSize, type Desktop } from "@/api/remote_desktop";
import { credentialDelete, credentialSave, credentialGet } from "@/api/vault";
import DesktopSidebar from "@/components/DesktopSidebar.vue";
import TabBar, { type TabBarItem } from "@/components/TabBar.vue";
import VncPane from "@/components/VncPane.vue";
import RdpPane from "@/components/RdpPane.vue";
import AiPanel from "@/components/AiPanel.vue";
import { matchesCombo } from "@/utils/shortcut";

// KeepAlive 按 name 匹配缓存本组件（MainLayout），切走页面时已连接的桌面会话不中断。
defineOptions({ name: "RemoteDesktopView" });

const store = useDesktopsStore();
const desktopTabs = useDesktopTabsStore();
const settings = useSettingsStore();

const PROTOCOL_OPTIONS: { value: "rdp" | "vnc"; label: string; port: number }[] = [
  { value: "rdp", label: "RDP (Windows 桌面)", port: 3389 },
  { value: "vnc", label: "VNC", port: 5900 },
];

// --- 对话框 ---
const dialogVisible = ref(false);
const editingId = ref<string | null>(null);
const formRef = ref<FormInstance>();
const form = reactive({
  name: "",
  protocol: "rdp" as "rdp" | "vnc",
  host: "",
  port: 3389,
  username: "",
  password: "",
  groupId: null as string | null,
});

// 表单校验规则（与 ForwardView 一致：el-form rules 统一触发与提示）。
const formRules: FormRules = {
  name: [{ required: true, message: "请输入名称", trigger: "blur" }],
  host: [{ required: true, message: "请输入主机", trigger: "blur" }],
  port: [{ required: true, message: "请输入端口", trigger: "blur" }],
};

/** 打开新建对话框（groupId 为来源分组的预选项，来自侧栏「新建子桌面连接」）。 */
function openAdd(groupId: string | null = null) {
  editingId.value = null;
  form.name = "";
  form.protocol = "rdp";
  form.host = "";
  form.port = 3389;
  form.username = "";
  form.password = "";
  form.groupId = groupId;
  dialogVisible.value = true;
}

function openEdit(d: Desktop) {
  editingId.value = d.id;
  form.name = d.name;
  form.protocol = d.protocol;
  form.host = d.host;
  form.port = d.port;
  form.username = d.username ?? "";
  form.password = ""; // 编辑时密码留空表示不修改
  form.groupId = d.groupId;
  dialogVisible.value = true;
}

// 切协议调默认端口。
function onProtocolChange(proto: "rdp" | "vnc") {
  const opt = PROTOCOL_OPTIONS.find((o) => o.value === proto);
  if (opt) form.port = opt.port;
}

async function submit() {
  if (formRef.value) {
    try {
      await formRef.value.validate();
    } catch {
      return;
    }
  }
  const now = new Date().toISOString();
  const existed = store.desktops.find((d) => d.id === editingId.value);

  // 密码处理：编辑时留空表示不修改（保持原凭据关联）；填了密码则原地更新
  // 原凭据（传 id 不产生孤儿凭据）或新建。
  // createdCredId：本次新建的凭据 id，桌面保存失败时补偿删除，避免孤儿。
  let credentialId = existed?.credentialId ?? null;
  let createdCredId: string | null = null;
  if (form.password.trim()) {
    try {
      credentialId = await credentialSave({
        id: existed?.credentialId ?? undefined,
        name: `${form.name} 密码`,
        kind: "password",
        value: form.password,
      });
      if (!existed?.credentialId) createdCredId = credentialId;
    } catch (e: unknown) {
      ElMessage.error("保存密码失败：" + String(e));
      return;
    }
  }

  const desktop: Desktop = {
    id: editingId.value ?? `d-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
    name: form.name.trim(),
    protocol: form.protocol,
    host: form.host.trim(),
    port: form.port,
    username: form.username.trim() || null,
    credentialId,
    groupId: form.groupId,
    // 编辑保存时保留已记忆的分辨率（不因改其他字段被清掉）。
    desktopSize: existed?.desktopSize ?? null,
    sortOrder: existed?.sortOrder ?? 0,
    createdAt: existed?.createdAt ?? now,
    updatedAt: now,
  };
  try {
    await store.save(desktop);
    ElMessage.success(editingId.value ? "已更新" : "已添加");
    dialogVisible.value = false;
  } catch (e: unknown) {
    // 桌面保存失败：补偿删除本次新建的凭据，避免孤儿。
    if (createdCredId) {
      await credentialDelete(createdCredId).catch(() => {});
    }
    ElMessage.error("保存失败：" + String(e));
  }
}

/** 读取桌面关联凭据的密码（失败不阻塞连接）。 */
async function loadPassword(d: Desktop): Promise<string | undefined> {
  if (!d.credentialId) return undefined;
  try {
    const plain = await credentialGet(d.credentialId);
    const data = JSON.parse(plain) as { value?: string };
    return data.value;
  } catch {
    return undefined;
  }
}

/** 该连接当前使用的客户端模式（"app" 内嵌 / "system" 系统客户端）。 */
function clientMode(d: Desktop): string {
  return d.protocol === "vnc"
    ? settings.terminal.desktopClients.vnc
    : settings.terminal.desktopClients.rdp;
}

/**
 * 连接桌面：按设置选择内嵌（本页右侧打开标签页）或系统客户端。
 * - 内嵌 VNC：desktopTabs.open（口令只交给前端 noVNC，不经过后端桥接）；
 * - 内嵌 RDP：desktopTabs.open（需用户名+密码，凭据只交给前端 WASM，
 *   由 CredSSP 在隧道内与目标协商）；
 * - 系统客户端：mstsc / vncviewer（remoteDesktopLaunch）。
 */
async function connect(d: Desktop) {
  // 设置未就绪时先补齐（启动早期竞态兜底）：客户端模式必须按保存值执行，
  // 否则会误用默认的"系统客户端"（mstsc）而非设置里的"程序内嵌"。
  if (!settings.loaded) {
    try {
      await settings.load();
    } catch {
      /* 仍失败则按当前值执行，设置页可重试 */
    }
  }
  const isApp = clientMode(d) === "app";
  try {
    const password = await loadPassword(d);
    if (isApp) {
      // 内嵌 RDP 走 NLA 认证，必须带用户名和密码。
      if (d.protocol === "rdp" && (!d.username || !password)) {
        ElMessage.warning(
          "内嵌 RDP 需要用户名和密码：请在「编辑」中填写，或到设置切换为系统客户端",
        );
        return;
      }
      await desktopTabs.open(d, password);
      return;
    }
    const msg = await remoteDesktopLaunch({
      protocol: d.protocol,
      host: d.host,
      port: d.port,
      username: d.username || undefined,
      password,
    });
    ElMessage.success(msg);
  } catch (e: unknown) {
    ElMessage.error(
      isApp
        ? `打开${d.protocol.toUpperCase()} 标签页失败：` + String(e)
        : "启动桌面客户端失败：" + String(e),
    );
  }
}

async function remove(d: Desktop) {
  try {
    await ElMessageBox.confirm(`确认删除「${d.name}」？`, "删除确认", {
      type: "warning",
      confirmButtonText: "删除",
      cancelButtonText: "取消",
    });
  } catch {
    return; // 用户取消
  }
  try {
    await store.remove(d.id);
    // 已打开的对应标签页一并关闭（桥接由 close 内部停止）。
    await desktopTabs.closeByDesktopId(d.id);
    // 同步清理关联的 vault 凭据，避免孤儿凭据残留。
    if (d.credentialId) {
      try {
        await credentialDelete(d.credentialId);
      } catch {
        /* 忽略删除失败 */
      }
    }
    ElMessage.success("已删除");
  } catch (e: unknown) {
    // API 失败与用户取消分开处理：失败必须有提示，不能被"取消"分支吞掉。
    ElMessage.error("删除失败：" + String(e));
  }
}

// --- 标签页交互（共享 TabBar 组件） -----------------------------------------

const active = computed(() =>
  desktopTabs.tabs.find((t) => t.instanceId === desktopTabs.activeId)
);

/** 连接成功：抹除标签内存中的明文口令（握手已完成，不再需要）。 */
function onPaneConnected(instanceId: string) {
  desktopTabs.clearPassword(instanceId);
}

/** 重连前从 vault 重新注入口令（连接成功后内存副本已被抹除）。 */
async function refillPassword(tab: DesktopTab) {
  const d = store.desktops.find((x) => x.id === tab.desktopId);
  if (!d) return;
  desktopTabs.setPassword(tab.instanceId, await loadPassword(d));
}

/** 桌面会话断开（由 VncPane/RdpPane emit "closed"）：标记断开并停止桥接。 */
function onPaneClosed(instanceId: string) {
  void desktopTabs.handleClosed(instanceId);
}

/**
 * RDP 分辨率记忆（RdpPane sizechange）：更新标签内存值 + 持久化到 DB，
 * 下次打开/重连时恢复。VNC 分辨率由服务端控制，不参与。
 */
function onPaneSizeChange(tab: DesktopTab, size: string) {
  if (!tab.desktopId || !size) return;
  desktopTabs.rememberSize(tab.instanceId, size);
  void desktopSaveSize(tab.desktopId, size)
    .then(() => {
      // 同步内存中的桌面记录，同会话内重新打开也能拿到最新尺寸。
      store.patchSize(tab.desktopId, size);
    })
    .catch(() => {
      /* 持久化失败不影响当前会话 */
    });
}

async function reconnectActive() {
  if (!active.value?.instanceId) return;
  try {
    await refillPassword(active.value);
    await desktopTabs.reconnect(active.value.instanceId);
  } catch (e) {
    /* 错误已存进 tab.error */
  }
}

/** 覆盖层「重新连接」按钮：先回填口令再重连。 */
async function reconnectTab(tab: DesktopTab) {
  if (!tab.instanceId || tab.reconnecting) return;
  try {
    await refillPassword(tab);
    await desktopTabs.reconnect(tab.instanceId);
  } catch {
    /* 错误已存进 tab.error */
  }
}

/** 映射为 TabBar 的数据抽象。 */
const tabItems = computed<TabBarItem[]>(() =>
  desktopTabs.tabs.map((t) => ({
    key: t.instanceId || t.desktopId,
    title: t.name,
    connecting: t.connecting,
    disconnected: t.disconnected,
  })),
);

/**
 * TabBar 右键菜单命令（作用于对应 tab）。
 *
 * key 为 TabBar 的复合键（instanceId || desktopId）：连接中的占位标签只有
 * 复合键可用，close 系列命令照常生效；reconnect 仍要求已建立实例。
 */

function onTabMenuCommand(cmd: string, key: string) {
  // 兜底匹配（与 store 的 close 一致）：桥接完成的瞬间旧 key（desktopId）
  // 可能落空，按多字段解析避免"右键菜单点关闭没反应"。
  const t = desktopTabs.tabs.find(
    (x) => x.instanceId === key || x.desktopId === key || (x.instanceId || x.desktopId) === key
  );
  if (!t) return;
  switch (cmd) {
    case "close":
      void desktopTabs.close(key);
      break;
    case "closeOthers":
      void desktopTabs.closeOthers(key);
      break;
    case "closeAll":
      void desktopTabs.closeAll();
      break;
    case "reconnect": {
      if (t.instanceId) {
        const target = desktopTabs.tabs.find((x) => x.instanceId === t.instanceId);
        if (target) void reconnectTab(target);
      }
      break;
    }
  }
}

// --- 标签快捷键（与终端页一致）：Ctrl+1~9 切换、Ctrl+W 关闭 ---

/** 焦点在可编辑元素时不响应快捷键，避免与输入框/编辑对话框冲突。 */
function isEditableTarget(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null;
  if (!el) return false;
  const tag = el.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || el.isContentEditable;
}

function onTabsKeydown(e: KeyboardEvent) {
  if (e.repeat) return;
  if (isEditableTarget(e.target)) return;
  // 焦点在桌面画布内（RDP/VNC 会话）时按键归远端（mstsc 惯例），
  // 不触发标签快捷键，避免与远端应用的 Ctrl+组合冲突。
  const target = e.target as HTMLElement | null;
  if (target?.closest(".rdp-pane, .vnc-pane")) return;
  // 关闭当前标签（默认 Ctrl+W）：跟随「设置 → 快捷键」的 closeTab 绑定，
  // 用户在设置中清除/改绑后这里不再抢按键（否则"删了快捷键仍生效"）。
  const closeTabCombo = settings.getAppShortcut("closeTab");
  if (closeTabCombo && matchesCombo(e, closeTabCombo)) {
    if (!active.value?.instanceId) return;
    e.preventDefault();
    void desktopTabs.close(active.value.instanceId);
    return;
  }
  // Ctrl+1~9：切换第 N 个标签。
  if ((e.ctrlKey || e.metaKey) && !e.altKey && /^[1-9]$/.test(e.key)) {
    const idx = Number(e.key) - 1;
    const tab = desktopTabs.tabs[idx];
    if (tab?.instanceId) {
      e.preventDefault();
      desktopTabs.setActive(tab.instanceId);
    }
  }
}

// 本组件被 KeepAlive 缓存：标签快捷键的全局监听仅在激活期间注册（tab 右键
// 菜单的关闭监听由 TabBar 组件自理）。
onActivated(() => {
  window.addEventListener("keydown", onTabsKeydown);
});
onDeactivated(() => {
  window.removeEventListener("keydown", onTabsKeydown);
});
onBeforeUnmount(() => {
  window.removeEventListener("keydown", onTabsKeydown);
});

onMounted(async () => {
  if (!store.loaded) {
    try {
      await store.load();
    } catch {
      /* ignore */
    }
  }
  preloadRdpWasmWhenIdle();
});

// --- 空闲预加载 IronRDP WASM（约 5.8MB 下载 + 编译） -------------------------
// 首次打开内嵌 RDP 标签时最耗时的就是 WASM 加载；进入桌面页后利用空闲时间
// 后台预热，用户点连接时几乎零等待。只预加载不连接，失败静默、下次重试。
let rdpWasmPreloaded = false;
function preloadRdpWasmWhenIdle() {
  if (rdpWasmPreloaded) return;
  rdpWasmPreloaded = true;
  const schedule =
    window.requestIdleCallback ?? ((cb: () => void) => setTimeout(cb, 3000));
  schedule(() => {
    void import("@devolutions/iron-remote-desktop-rdp")
      .then((mod) => mod.init("off"))
      .catch(() => {
        // 预加载失败（极少见）静默，真正连接时 RdpPane 内再加载并提示。
        rdpWasmPreloaded = false;
      });
  });
}
</script>

<template>
  <div class="desktop-view">
    <!-- 左侧：分组树 -->
    <DesktopSidebar
      @connect="connect"
      @edit="openEdit"
      @remove="remove"
      @new="openAdd"
    />

    <!-- 右侧：标签栏 + 桌面会话内容 -->
    <div class="desktop-main">
      <div class="tab-bar">
        <TabBar
          :tabs="tabItems"
          :active-key="desktopTabs.activeId"
          empty-hint="单击左侧桌面连接打开"
          @select="(k) => desktopTabs.setActive(k)"
          @close="(k) => void desktopTabs.close(k)"
          @move="(from, to, before) => desktopTabs.moveTab(from, to, before)"
          @command="onTabMenuCommand"
        />

        <!-- 断开时工具栏重连按钮 -->
        <div v-if="active?.disconnected" class="desktop-toolbar">
          <el-tooltip content="重新连接" placement="bottom">
            <el-button
              class="tool-btn"
              link
              :loading="active.reconnecting"
              @click="reconnectActive"
            >
              <el-icon><Refresh /></el-icon>
            </el-button>
          </el-tooltip>
        </div>
      </div>

      <div class="panes">
        <div
          v-for="tab in desktopTabs.tabs"
          :key="tab.instanceId || tab.desktopId"
          v-show="tab.instanceId === desktopTabs.activeId"
          class="pane"
        >
          <template v-if="tab.instanceId">
            <VncPane
              v-if="tab.protocol === 'vnc'"
              :instance-id="tab.instanceId"
              :ws-url="tab.wsUrl"
              :username="tab.username"
              :password="tab.password"
              @connected="onPaneConnected(tab.instanceId)"
              @closed="onPaneClosed(tab.instanceId)"
            />
            <RdpPane
              v-else
              :instance-id="tab.instanceId"
              :ws-url="tab.wsUrl"
              :host="tab.host"
              :port="tab.port"
              :username="tab.username"
              :password="tab.password"
              :initial-size="tab.desktopSize"
              @connected="onPaneConnected(tab.instanceId)"
              @closed="onPaneClosed(tab.instanceId)"
              @sizechange="(size: string) => onPaneSizeChange(tab, size)"
            />
            <!-- 断开重连覆盖层 -->
            <div v-if="tab.disconnected" class="reconnect-overlay">
              <div class="reconnect-card">
                <div class="reconnect-title">连接已断开</div>
                <el-button
                  type="primary"
                  :icon="Refresh"
                  :loading="tab.reconnecting"
                  @click="reconnectTab(tab)"
                >
                  重新连接
                </el-button>
              </div>
            </div>
          </template>
          <div v-else-if="tab.connecting" class="pane-status">连接中…</div>
          <div v-else-if="tab.error" class="pane-status error">连接失败：{{ tab.error }}</div>
        </div>

        <!-- 空状态 -->
        <div v-if="!active" class="desktop-empty">
          <div class="empty-main">从左侧列表选择桌面连接打开</div>
          <div class="empty-hint">
            客户端模式可在「设置 → 连接」中按协议选择：程序内嵌（本页打开）或系统客户端
            （RDP: mstsc / VNC: vncviewer）。内嵌 RDP 需填写用户名和密码。
          </div>
        </div>
      </div>
    </div>

    <!-- 桌面助手面板：仅桌面页显示，可查看/操作内嵌 RDP 桌面（多模态模型） -->
    <AiPanel domain="desktop" />

    <!-- 新建/编辑对话框（独立于 SessionDialog） -->
    <el-dialog
      v-model="dialogVisible"
      :title="editingId ? '编辑桌面连接' : '新建桌面连接'"
      width="480px"
    >
      <el-form ref="formRef" :model="form" :rules="formRules" label-width="80px">
        <el-form-item label="名称" prop="name">
          <el-input v-model="form.name" placeholder="如：办公电脑" />
        </el-form-item>
        <el-form-item label="协议">
          <el-select v-model="form.protocol" @change="onProtocolChange" style="width: 100%">
            <el-option
              v-for="opt in PROTOCOL_OPTIONS"
              :key="opt.value"
              :label="opt.label"
              :value="opt.value"
            />
          </el-select>
        </el-form-item>
        <el-form-item label="主机" prop="host">
          <el-input v-model="form.host" placeholder="IP 或域名" />
        </el-form-item>
        <el-form-item label="端口" prop="port">
          <el-input-number v-model="form.port" :min="1" :max="65535" controls-position="right" />
        </el-form-item>
        <el-form-item label="分组">
          <el-select v-model="form.groupId" clearable placeholder="无分组" style="width: 100%">
            <el-option label="无分组" :value="null" />
            <el-option v-for="g in store.groups" :key="g.id" :label="g.name" :value="g.id" />
          </el-select>
        </el-form-item>
        <el-form-item label="用户名">
          <el-input v-model="form.username" placeholder="可选" />
        </el-form-item>
        <el-form-item label="密码">
          <el-input
            v-model="form.password"
            type="password"
            show-password
            :placeholder="editingId ? '留空表示不修改' : '可选'"
          />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" @click="submit">保存</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.desktop-view {
  display: flex;
  flex-direction: row;
  width: 100%;
  height: 100%;
  overflow: hidden;
  box-sizing: border-box;
}

.desktop-main {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

/* --- 标签栏 --- */
.tab-bar {
  display: flex;
  align-items: center;
  height: 34px;
  background: var(--el-bg-color-overlay);
  border-bottom: 1px solid var(--el-border-color-lighter);
  padding: 0 4px;
  flex-shrink: 0;
}

/* 工具栏重连按钮 */
.desktop-toolbar {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 0 6px 0 8px;
  border-left: 1px solid var(--el-border-color-lighter);
  margin-left: 4px;
  flex-shrink: 0;
}
.tool-btn {
  padding: 4px;
  color: var(--el-text-color-secondary);
}
.tool-btn:hover {
  color: var(--el-color-primary);
}

/* --- 画布区 --- */
.panes {
  flex: 1;
  min-height: 0;
  position: relative;
  background: var(--el-bg-color-page);
}
.pane {
  position: absolute;
  inset: 0;
}
.reconnect-overlay {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgba(0, 0, 0, 0.35);
  backdrop-filter: blur(2px);
  z-index: 20;
}
.reconnect-card {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 12px;
  padding: 20px 28px;
  background: var(--el-bg-color-overlay);
  border-radius: 8px;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.2);
}
.reconnect-title {
  font-size: 14px;
  color: var(--el-text-color-secondary);
}
.pane-status {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100%;
  color: var(--el-text-color-secondary);
}
.pane-status.error {
  color: var(--el-color-danger);
}

/* --- 空状态 --- */
.desktop-empty {
  height: 100%;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  padding: 0 24px;
}
.empty-main {
  font-size: 14px;
  color: var(--el-text-color-secondary);
}
.empty-hint {
  font-size: 12px;
  color: var(--el-text-color-placeholder);
  text-align: center;
  line-height: 1.8;
}
</style>
