<script setup lang="ts">
// SFTP 主视图
// ----------------------------------------------------------------------------
// 左右双栏：本地（plugin-fs）与远程（sftpList）文件浏览，支持上传/下载、
// 远程目录的增/改/删，底部嵌入 TransferQueue 展示传输进度。
//
// 传输进度由后端通过 transfer:progress / transfer:done / transfer:error
// 事件推送，MainLayout 已统一订阅写入 transferStore；本视图只负责：
//   1. transferStore.add 创建任务（status=pending）
//   2. 调 sftpDownload / sftpUpload 触发传输
// ----------------------------------------------------------------------------
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, type Ref } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import {
  ArrowUp,
  Refresh,
  FolderAdd,
  Download as IconDownload,
  Upload as IconUpload,
  Delete,
  EditPen,
  Link,
  FolderOpened,
  Document,
  Folder,
  Right,
} from "@element-plus/icons-vue";
import { readDir, mkdir, stat as fsStat } from "@tauri-apps/plugin-fs";
import { open as openFileDialog, save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { homeDir, join, sep as pathSep, dirname, basename } from "@tauri-apps/api/path";
import { useSessionsStore } from "@/stores/sessions";
import { useTransferStore } from "@/stores/transfer";
import type { Session, FileEntry } from "@/api/types";
import {
  sftpList,
  sftpMkdir,
  sftpRename,
  sftpRemove,
  sftpDownload,
  sftpUpload,
  sftpClose,
} from "@/api/sftp";
import { openSftpForSession } from "@/api/session";
import TransferQueue from "@/components/TransferQueue.vue";

// ---------------------------------------------------------------------------
// 统一的目录条目（本地 / 远程共用）。
// ---------------------------------------------------------------------------
interface UnifiedEntry {
  name: string;
  isDir: boolean;
  size: number;
  modified: string | null;
}

/** 面板间拖拽的载荷。 */
interface DragPayload {
  source: "local" | "remote";
  entry: UnifiedEntry;
}

// ---------------------------------------------------------------------------
// 会话 / SFTP 连接
// ---------------------------------------------------------------------------
const sessionsStore = useSessionsStore();
const transfer = useTransferStore();

const selectedSessionId = ref<string>("");
const sftpId = ref<string>("");
const connecting = ref(false);

const isConnected = computed(() => !!sftpId.value);

// ---------------------------------------------------------------------------
// 本地文件浏览
// ---------------------------------------------------------------------------
const localPath = ref<string>("");
const localEntries = ref<UnifiedEntry[]>([]);
const localLoading = ref(false);
const selectedLocal = ref<UnifiedEntry | null>(null);

async function loadLocal(path: string) {
  localLoading.value = true;
  try {
    const raw = await readDir(path);
    const items: UnifiedEntry[] = [];
    for (const e of raw) {
      // 跳过 . / .. 等系统条目，readDir 已不含，但兼容过滤。
      if (e.name === "." || e.name === "..") continue;
      let size = 0;
      let modified: string | null = null;
      try {
        const full = await join(path, e.name);
        const info = await fsStat(full);
        size = info.size;
        modified = info.mtime ? new Date(info.mtime).toISOString() : null;
      } catch {
        // 无法 stat 的条目（权限等）用默认值。
      }
      items.push({
        name: e.name,
        isDir: e.isDirectory,
        size,
        modified,
      });
    }
    sortEntries(items);
    localEntries.value = items;
    localPath.value = path;
    selectedLocal.value = null;
  } catch (e) {
    ElMessage.error("读取本地目录失败: " + String(e));
  } finally {
    localLoading.value = false;
  }
}

async function refreshLocal() {
  if (localPath.value) await loadLocal(localPath.value);
}

async function localEnter(entry: UnifiedEntry) {
  if (!entry.isDir) {
    selectedLocal.value = entry;
    return;
  }
  const next = await join(localPath.value, entry.name);
  await loadLocal(next);
}

async function localGoUp() {
  if (!localPath.value) return;
  const parent = await dirname(localPath.value);
  if (parent && parent !== localPath.value) await loadLocal(parent);
}

async function localGoToInput() {
  const p = localPath.value.trim();
  if (p) await loadLocal(p);
}

async function localMkdir() {
  let name: string;
  try {
    const res = await ElMessageBox.prompt("请输入新目录名称", "新建本地目录", {
      confirmButtonText: "创建",
      cancelButtonText: "取消",
      inputValidator: (v) => !!v?.trim() || "名称不能为空",
    });
    name = res.value.trim();
  } catch {
    return;
  }
  try {
    const full = await join(localPath.value, name);
    await mkdir(full);
    ElMessage.success("已创建目录");
    await refreshLocal();
  } catch (e) {
    ElMessage.error("创建目录失败: " + String(e));
  }
}

// ---------------------------------------------------------------------------
// 远程文件浏览
// ---------------------------------------------------------------------------
const remotePath = ref<string>("");
const remoteEntries = ref<UnifiedEntry[]>([]);
const remoteLoading = ref(false);
const selectedRemote = ref<UnifiedEntry | null>(null);

function fromFileEntry(e: FileEntry): UnifiedEntry {
  return { name: e.name, isDir: e.isDir, size: e.size, modified: e.modified };
}

async function loadRemote(path: string) {
  if (!sftpId.value) return;
  remoteLoading.value = true;
  try {
    const list = await sftpList(sftpId.value, path);
    const items = list.map(fromFileEntry);
    sortEntries(items);
    remoteEntries.value = items;
    remotePath.value = path;
    selectedRemote.value = null;
  } catch (e) {
    ElMessage.error("读取远程目录失败: " + String(e));
  } finally {
    remoteLoading.value = false;
  }
}

async function refreshRemote() {
  if (remotePath.value && sftpId.value) await loadRemote(remotePath.value);
}

function remoteEnter(entry: UnifiedEntry) {
  if (!entry.isDir) {
    selectedRemote.value = entry;
    return;
  }
  const sep = "/";
  const next =
    remotePath.value.endsWith(sep)
      ? remotePath.value + entry.name
      : remotePath.value + sep + entry.name;
  loadRemote(next);
}

function remoteGoUp() {
  if (!remotePath.value) return;
  const sep = "/";
  const parts = remotePath.value.split(sep).filter(Boolean);
  if (parts.length <= 1) {
    loadRemote(sep);
    return;
  }
  parts.pop();
  loadRemote(sep + parts.join(sep));
}

function remoteGoToInput() {
  const p = remotePath.value.trim();
  if (p) loadRemote(p);
}

// 面包屑：把 remotePath 按分隔符拆成可点击的段。
const remoteCrumbs = computed<{ name: string; path: string }[]>(() => {
  const p = remotePath.value;
  if (!p) return [];
  // 兼容 Unix（/）和 Windows（\）分隔符。远程通常用 /。
  const parts = p.split(/[\\/]/).filter(Boolean);
  const isAbs = p.startsWith("/") || p.startsWith("\\");
  const crumbs: { name: string; path: string }[] = [];
  if (isAbs) crumbs.push({ name: "/", path: "/" });
  let acc = isAbs ? "" : "";
  for (const part of parts) {
    acc = acc ? acc + "/" + part : "/" + part;
    crumbs.push({ name: part, path: acc });
  }
  return crumbs;
});
function remoteGoToCrumb(path: string) {
  loadRemote(path);
}

async function remoteMkdir() {
  if (!sftpId.value) return;
  let name: string;
  try {
    const res = await ElMessageBox.prompt("请输入新目录名称", "新建远程目录", {
      confirmButtonText: "创建",
      cancelButtonText: "取消",
      inputValidator: (v) => !!v?.trim() || "名称不能为空",
    });
    name = res.value.trim();
  } catch {
    return;
  }
  const sep = "/";
  const full = remotePath.value.endsWith(sep)
    ? remotePath.value + name
    : remotePath.value + sep + name;
  try {
    await sftpMkdir(sftpId.value, full);
    ElMessage.success("已创建目录");
    await refreshRemote();
  } catch (e) {
    ElMessage.error("创建目录失败: " + String(e));
  }
}

async function remoteRename(entry: UnifiedEntry) {
  if (!sftpId.value) return;
  let name: string;
  try {
    const res = await ElMessageBox.prompt("请输入新的名称", "重命名", {
      inputValue: entry.name,
      confirmButtonText: "保存",
      cancelButtonText: "取消",
      inputValidator: (v) => !!v?.trim() || "名称不能为空",
    });
    name = res.value.trim();
  } catch {
    return;
  }
  if (name === entry.name) return;
  const sep = "/";
  const oldFull = remotePath.value.endsWith(sep)
    ? remotePath.value + entry.name
    : remotePath.value + sep + entry.name;
  const newFull = remotePath.value.endsWith(sep)
    ? remotePath.value + name
    : remotePath.value + sep + name;
  try {
    await sftpRename(sftpId.value, oldFull, newFull);
    ElMessage.success("已重命名");
    await refreshRemote();
  } catch (e) {
    ElMessage.error("重命名失败: " + String(e));
  }
}

async function remoteRemove(entry: UnifiedEntry) {
  if (!sftpId.value) return;
  try {
    await ElMessageBox.confirm(
      entry.isDir ? `确定删除远程目录 "${entry.name}" 及其内容？` : `确定删除远程文件 "${entry.name}"？`,
      "删除",
      { type: "warning", confirmButtonText: "删除", cancelButtonText: "取消" }
    );
  } catch {
    return;
  }
  const sep = "/";
  const full = remotePath.value.endsWith(sep)
    ? remotePath.value + entry.name
    : remotePath.value + sep + entry.name;
  try {
    await sftpRemove(sftpId.value, full, entry.isDir);
    ElMessage.success("已删除");
    await refreshRemote();
  } catch (e) {
    ElMessage.error("删除失败: " + String(e));
  }
}

// ---------------------------------------------------------------------------
// 排序辅助：目录在前，按名称字母序。
// ---------------------------------------------------------------------------
function sortEntries(items: UnifiedEntry[]) {
  items.sort((a, b) => {
    if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
    return a.name.localeCompare(b.name, undefined, { numeric: true });
  });
}

// --- 列排序（表头点击切换 name/size/mtime，目录始终优先） ---
type SortKey = "name" | "size" | "modified";
const remoteSortKey = ref<SortKey>("name");
const remoteSortDesc = ref(false);
const localSortKey = ref<SortKey>("name");
const localSortDesc = ref(false);

function makeSorted(entries: Ref<UnifiedEntry[]>, key: Ref<SortKey>, desc: Ref<boolean>) {
  return computed(() => {
    const arr = [...entries.value];
    const k = key.value;
    const d = desc.value ? -1 : 1;
    arr.sort((a, b) => {
      if (a.isDir !== b.isDir) return a.isDir ? -1 : 1; // 目录永远靠前
      let cmp = 0;
      if (k === "name") cmp = a.name.localeCompare(b.name, undefined, { numeric: true });
      else if (k === "size") cmp = (a.size || 0) - (b.size || 0);
      else cmp = (a.modified || "").localeCompare(b.modified || "");
      return cmp * d;
    });
    return arr;
  });
}
const sortedRemote = makeSorted(remoteEntries, remoteSortKey, remoteSortDesc);
const sortedLocal = makeSorted(localEntries, localSortKey, localSortDesc);
function toggleRemoteSort(k: SortKey) {
  if (remoteSortKey.value === k) remoteSortDesc.value = !remoteSortDesc.value;
  else {
    remoteSortKey.value = k;
    remoteSortDesc.value = false;
  }
}
function toggleLocalSort(k: SortKey) {
  if (localSortKey.value === k) localSortDesc.value = !localSortDesc.value;
  else {
    localSortKey.value = k;
    localSortDesc.value = false;
  }
}

// ---------------------------------------------------------------------------
// 连接 / 断开 SFTP
// ---------------------------------------------------------------------------
async function connectSftp() {
  if (!selectedSessionId.value) {
    ElMessage.warning("请先选择目标会话");
    return;
  }
  connecting.value = true;
  const msg = ElMessage.info({
    message: "正在打开 SFTP 连接...",
    duration: 0,
  });
  try {
    const id = await openSftpForSession(selectedSessionId.value);
    sftpId.value = id;
    msg.close();
    ElMessage.success("SFTP 连接已建立");
    // 默认进入远程家目录（后端通常以 . 表示家目录，列表里能拿到绝对路径）。
    await loadRemote(".");
  } catch (e) {
    msg.close();
    ElMessage.error("SFTP 连接失败: " + String(e));
  } finally {
    connecting.value = false;
  }
}

async function disconnectSftp() {
  if (!sftpId.value) return;
  try {
    await ElMessageBox.confirm("确定关闭当前 SFTP 连接？", "关闭连接", {
      type: "warning",
      confirmButtonText: "关闭",
      cancelButtonText: "取消",
    });
  } catch {
    return;
  }
  if (await closeSftpCore()) {
    ElMessage.success("已关闭 SFTP 连接");
  } else {
    ElMessage.error("关闭 SFTP 连接失败");
  }
}

/**
 * 关闭 SFTP 连接并清空本地状态，返回是否成功。
 *
 * 无确认弹窗/无提示：供 disconnectSftp 确认后调用，也供视图卸载时静默调用
 * （组件可能已销毁，此时不应弹 ElMessage）。
 */
async function closeSftpCore(): Promise<boolean> {
  const id = sftpId.value;
  if (!id) return true;
  sftpId.value = "";
  let ok = false;
  try {
    await sftpClose(id);
    ok = true;
  } catch (e) {
    // 连接已死时 sftpClose 失败属正常。
    console.warn("关闭 SFTP 连接失败:", e);
  } finally {
    remotePath.value = "";
    remoteEntries.value = [];
    selectedRemote.value = null;
  }
  return ok;
}

/** 视图卸载时的静默清理。 */
function closeSftpSilently() {
  void closeSftpCore();
}

// ---------------------------------------------------------------------------
// 上传 / 下载
// ---------------------------------------------------------------------------
function joinRemote(name: string): string {
  const sep = "/";
  return remotePath.value.endsWith(sep)
    ? remotePath.value + name
    : remotePath.value + sep + name;
}

async function joinLocal(name: string): Promise<string> {
  return await join(localPath.value, name);
}

// 上传：从本地选中条目（或弹框选择文件）上传到远程当前目录。
async function uploadSelected() {
  if (!sftpId.value) {
    ElMessage.warning("请先打开 SFTP 连接");
    return;
  }
  let localFile = selectedLocal.value;
  if (!localFile || localFile.isDir) {
    // 让用户从对话框选择源文件。
    try {
      const picked = await openFileDialog({
        multiple: false,
        directory: false,
        defaultPath: localPath.value || undefined,
      });
      if (!picked) return;
      // picked 是绝对路径，提取文件名。
      const name = await basename(picked);
      localFile = { name, isDir: false, size: 0, modified: null };
      // 记录实际路径供后续使用。
      uploadOne(picked, localFile.name);
      return;
    } catch (e) {
      ElMessage.error("选择文件失败: " + String(e));
      return;
    }
  }
  const full = await joinLocal(localFile.name);
  await uploadOne(full, localFile.name);
}

async function uploadOne(localAbs: string, name: string) {
  const remoteAbs = joinRemote(name);
  const taskId = crypto.randomUUID();
  transfer.add({
    id: taskId,
    name,
    direction: "upload",
    transferred: 0,
    total: 0,
    status: "pending",
  });
  // 后端命令整体 await（完成才返回），"运行中"须在发起前标记，
  // 否则完成后才更新会覆盖 transfer:done 事件的完成状态。
  transfer.update(taskId, { status: "running" });
  try {
    await sftpUpload({
      sftpId: sftpId.value,
      localPath: localAbs,
      remotePath: remoteAbs,
      taskId,
    });
    ElMessage.success(`上传完成 ${name}`);
  } catch (e) {
    transfer.update(taskId, { status: "error", message: String(e) });
    ElMessage.error("上传失败: " + String(e));
  }
}

// --- 拖拽上传（Tauri 原生 onDragDropEvent，拿 OS 文件真实路径） ---
const remotePaneRef = ref<HTMLElement | null>(null);
const dragOver = ref(false);
let dragDropUnlisten: UnlistenFn | null = null;
let transferDoneUnlisten: UnlistenFn | null = null;

// --- 面板间拖拽传输（HTML5 DnD） ---
const paneDragOver = ref<"local" | "remote" | null>(null);
let dragPayload: DragPayload | null = null;

async function handleDragDrop(event: { payload: { type: string; paths?: string[]; position?: { x: number; y: number } } }) {
  const p = event.payload;
  if (p.type === "over" || p.type === "enter") {
    // 仅当鼠标在远程面板区域内时显示高亮。
    if (isPointInRemotePane(p.position)) dragOver.value = true;
    return;
  }
  if (p.type === "leave" || p.type === "cancel") {
    dragOver.value = false;
    return;
  }
  // drop
  dragOver.value = false;
  if (!sftpId.value) {
    ElMessage.warning("请先连接 SFTP 会话");
    return;
  }
  if (!isPointInRemotePane(p.position)) return; // 不在远程面板上，忽略
  const paths = p.paths ?? [];
  if (paths.length === 0) return;
  for (const localPath of paths) {
    // basename 取文件名（跨平台用 @tauri-apps/api/path 的 basename）。
    const name = await basename(localPath).catch(() => localPath);
    void uploadOne(localPath, name);
  }
}

function isPointInRemotePane(pos?: { x: number; y: number }): boolean {
  if (!pos || !remotePaneRef.value) return false;
  const rect = remotePaneRef.value.getBoundingClientRect();
  return (
    pos.x >= rect.left && pos.x <= rect.right && pos.y >= rect.top && pos.y <= rect.bottom
  );
}

// 下载：从远程选中条目下载，用 save 对话框选择保存位置。
async function downloadSelected() {
  if (!sftpId.value) {
    ElMessage.warning("请先打开 SFTP 连接");
    return;
  }
  const remoteFile = selectedRemote.value;
  if (!remoteFile || remoteFile.isDir) {
    ElMessage.warning("请选择一个远程文件（暂不支持目录下载）");
    return;
  }
  const remoteAbs = joinRemote(remoteFile.name);
  // 默认保存到本地当前目录。
  const defaultSave = await joinLocal(remoteFile.name);
  let savePath: string;
  try {
    const picked = await saveFileDialog({
      defaultPath: defaultSave,
    });
    if (!picked) return;
    savePath = picked;
  } catch (e) {
    ElMessage.error("选择保存位置失败: " + String(e));
    return;
  }
  const taskId = crypto.randomUUID();
  transfer.add({
    id: taskId,
    name: remoteFile.name,
    direction: "download",
    transferred: 0,
    total: remoteFile.size || 0,
    status: "pending",
  });
  transfer.update(taskId, { status: "running" });
  try {
    await sftpDownload({
      sftpId: sftpId.value,
      remotePath: remoteAbs,
      localPath: savePath,
      taskId,
    });
    ElMessage.success(`下载完成 ${remoteFile.name}`);
  } catch (e) {
    transfer.update(taskId, { status: "error", message: String(e) });
    ElMessage.error("下载失败: " + String(e));
  }
}

// 下载单个远程文件到本地当前目录（拖拽用，无需弹框）。
async function downloadOne(remoteAbs: string, name: string) {
  const localAbs = await joinLocal(name);
  const taskId = crypto.randomUUID();
  transfer.add({
    id: taskId,
    name,
    direction: "download",
    transferred: 0,
    total: 0,
    status: "pending",
  });
  transfer.update(taskId, { status: "running" });
  try {
    await sftpDownload({
      sftpId: sftpId.value,
      remotePath: remoteAbs,
      localPath: localAbs,
      taskId,
    });
    ElMessage.success(`下载完成 ${name}`);
  } catch (e) {
    transfer.update(taskId, { status: "error", message: String(e) });
    ElMessage.error("下载失败: " + String(e));
  }
}

// --- 面板间拖拽传输（HTML5 DnD） ---
function onRowDragStart(source: "local" | "remote", entry: UnifiedEntry) {
  if (entry.isDir) return;
  dragPayload = { source, entry };
}

function onPaneDragOver(target: "local" | "remote", e: DragEvent) {
  e.preventDefault();
  if (dragPayload && dragPayload.source !== target) {
    paneDragOver.value = target;
  }
}

function onPaneDragLeave() {
  paneDragOver.value = null;
}

async function onPaneDrop(target: "local" | "remote", e: DragEvent) {
  e.preventDefault();
  paneDragOver.value = null;
  if (!dragPayload || dragPayload.source === target) return;
  if (!sftpId.value) {
    ElMessage.warning("请先打开 SFTP 连接");
    return;
  }
  const { entry } = dragPayload;
  dragPayload = null;
  if (target === "remote") {
    // 本地 → 远程 = 上传
    const full = await joinLocal(entry.name);
    void uploadOne(full, entry.name);
  } else {
    // 远程 → 本地 = 下载
    const remoteAbs = joinRemote(entry.name);
    void downloadOne(remoteAbs, entry.name);
  }
}

// ---------------------------------------------------------------------------
// 工具函数
// ---------------------------------------------------------------------------
function humanSize(n: number): string {
  if (!n || n <= 0) return "-";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}

function formatTime(s: string | null): string {
  if (!s) return "-";
  const d = new Date(s);
  if (isNaN(d.getTime())) return "-";
  const pad = (x: number) => String(x).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(
    d.getHours()
  )}:${pad(d.getMinutes())}`;
}

// 本地路径分隔符用于显示。
const sepDisplay = ref("/");
async function initSep() {
  try {
    sepDisplay.value = await pathSep();
  } catch {
    sepDisplay.value = "/";
  }
}

const selectedSession = computed<Session | undefined>(() =>
  sessionsStore.sessions.find((s) => s.id === selectedSessionId.value)
);

// ---------------------------------------------------------------------------
// 初始化
// ---------------------------------------------------------------------------
onMounted(async () => {
  await initSep();
  if (!sessionsStore.loaded) await sessionsStore.load();
  // 默认选第一个会话 + 默认进入本地家目录。
  if (sessionsStore.sessions.length > 0 && !selectedSessionId.value) {
    selectedSessionId.value = sessionsStore.sessions[0].id;
  }
  try {
    const home = await homeDir();
    await loadLocal(home);
  } catch (e) {
    ElMessage.error("无法读取本地家目录: " + String(e));
  }
  // 注册 Tauri 原生拖拽事件（获取 OS 文件真实路径，HTML5 drop 不暴露路径）。
  dragDropUnlisten = await getCurrentWebview().onDragDropEvent((e) => {
    void handleDragDrop(e as unknown as Parameters<typeof handleDragDrop>[0]);
  });
  // 传输完成后自动刷新对应侧：upload→刷新远程列表，download→刷新本地列表。
  transferDoneUnlisten = await listen<{ taskId: string }>("transfer:done", (e) => {
    const task = transfer.tasks.find((t) => t.id === e.payload.taskId);
    if (!task) return;
    if (task.direction === "upload" && sftpId.value && remotePath.value) {
      void loadRemote(remotePath.value);
    } else if (task.direction === "download" && localPath.value) {
      void loadLocal(localPath.value);
    }
  });
});

onBeforeUnmount(() => {
  if (dragDropUnlisten) {
    dragDropUnlisten();
    dragDropUnlisten = null;
  }
  if (transferDoneUnlisten) {
    transferDoneUnlisten();
    transferDoneUnlisten = null;
  }
  // 视图卸载（切换路由/关闭）时关闭 SFTP 会话，否则后端 SSH 连接与
  // sftp_sessions 注册表条目会一直驻留泄漏。
  closeSftpSilently();
});
</script>

<template>
  <div class="sftp-view">
    <!-- 顶部工具栏 -->
    <header class="toolbar">
      <div class="session-picker">
        <span class="picker-label">目标会话</span>
        <el-select
          v-model="selectedSessionId"
          placeholder="选择会话"
          size="default"
          filterable
          class="session-select"
          :disabled="connecting"
        >
          <el-option
            v-for="s in sessionsStore.sessions"
            :key="s.id"
            :label="`${s.name} (${s.host}:${s.port})`"
            :value="s.id"
          />
        </el-select>
      </div>

      <div class="toolbar-actions">
        <el-button
          type="primary"
          :icon="Link"
          :loading="connecting"
          @click="connectSftp"
        >
          {{ isConnected ? "重连 SFTP" : "打开 SFTP" }}
        </el-button>
        <el-button
          :icon="Right"
          :disabled="!isConnected"
          @click="disconnectSftp"
        >
          关闭连接
        </el-button>
        <el-tag v-if="isConnected" type="success" size="default" effect="light" class="conn-tag">
          已连接 · {{ selectedSession?.name }}
        </el-tag>
      </div>
    </header>

    <!-- 主体：双栏 -->
    <div class="sftp-body">
      <template v-if="isConnected">
        <!-- 本地栏 -->
        <section
          class="pane local-pane"
          :class="{ 'drag-over': paneDragOver === 'local' }"
          @dragover="onPaneDragOver('local', $event)"
          @dragleave="onPaneDragLeave"
          @drop="onPaneDrop('local', $event)"
        >
          <div class="pane-header">
            <span class="pane-title">
              <el-icon><FolderOpened /></el-icon>
              本地
            </span>
            <div class="pane-tools">
              <el-tooltip content="上级目录" placement="bottom">
                <el-button circle size="small" :icon="ArrowUp" @click="localGoUp" />
              </el-tooltip>
              <el-tooltip content="新建目录" placement="bottom">
                <el-button circle size="small" :icon="FolderAdd" @click="localMkdir" />
              </el-tooltip>
              <el-tooltip content="刷新" placement="bottom">
                <el-button circle size="small" :icon="Refresh" @click="refreshLocal" />
              </el-tooltip>
            </div>
          </div>
          <div class="path-bar">
            <el-input
              v-model="localPath"
              size="small"
              placeholder="本地路径"
              @keyup.enter="localGoToInput"
            >
              <template #prefix>
                <el-icon><Document /></el-icon>
              </template>
            </el-input>
          </div>
          <div class="file-table" v-loading="localLoading">
            <div class="file-header">
              <span class="file-name" @click="toggleLocalSort('name')">
                名称<span v-if="localSortKey === 'name'">{{ localSortDesc ? " ↓" : " ↑" }}</span>
              </span>
              <span class="file-mtime" @click="toggleLocalSort('modified')">
                修改时间<span v-if="localSortKey === 'modified'">{{ localSortDesc ? " ↓" : " ↑" }}</span>
              </span>
              <span class="file-size" @click="toggleLocalSort('size')">
                大小<span v-if="localSortKey === 'size'">{{ localSortDesc ? " ↓" : " ↑" }}</span>
              </span>
              <span class="row-actions"></span>
            </div>
            <div
              v-for="(e, idx) in sortedLocal"
              :key="'l-' + idx"
              class="file-row"
              :class="{ selected: selectedLocal?.name === e.name }"
              :draggable="!e.isDir"
              @click="selectedLocal = e"
              @dblclick="localEnter(e)"
              @dragstart="onRowDragStart('local', e)"
            >
              <el-icon class="file-icon" :class="{ 'is-dir': e.isDir }">
                <Folder v-if="e.isDir" />
                <Document v-else />
              </el-icon>
              <span class="file-name" :title="e.name">{{ e.name }}</span>
              <span class="file-size">{{ humanSize(e.size) }}</span>
              <span class="file-mtime">{{ formatTime(e.modified) }}</span>
            </div>
            <div v-if="!localLoading && localEntries.length === 0" class="empty-row">
              目录为空
            </div>
          </div>
        </section>

        <!-- 中间传输操作 -->
        <div class="transfer-arrows">
          <el-tooltip content="上传到远程 (→)" placement="right">
            <el-button
              circle
              type="primary"
              :icon="IconUpload"
              @click="uploadSelected"
            />
          </el-tooltip>
          <el-tooltip content="从远程下载 (←)" placement="right">
            <el-button
              circle
              :icon="IconDownload"
              @click="downloadSelected"
            />
          </el-tooltip>
        </div>

        <!-- 远程栏 -->
        <section
          ref="remotePaneRef"
          class="pane remote-pane"
          :class="{ 'drag-over': dragOver || paneDragOver === 'remote' }"
          @dragover="onPaneDragOver('remote', $event)"
          @dragleave="onPaneDragLeave"
          @drop="onPaneDrop('remote', $event)"
        >
          <div class="pane-header">
            <span class="pane-title">
              <el-icon><Connection /></el-icon>
              远程
            </span>
            <div class="pane-tools">
              <el-tooltip content="上级目录" placement="bottom">
                <el-button circle size="small" :icon="ArrowUp" @click="remoteGoUp" />
              </el-tooltip>
              <el-tooltip content="新建目录" placement="bottom">
                <el-button circle size="small" :icon="FolderAdd" @click="remoteMkdir" />
              </el-tooltip>
              <el-tooltip content="刷新" placement="bottom">
                <el-button circle size="small" :icon="Refresh" @click="refreshRemote" />
              </el-tooltip>
            </div>
          </div>
          <div class="breadcrumb">
            <template v-for="(c, i) in remoteCrumbs" :key="i">
              <span v-if="i > 0" class="breadcrumb-sep">/</span>
              <span class="breadcrumb-item" @click="remoteGoToCrumb(c.path)">{{ c.name }}</span>
            </template>
          </div>
          <div class="path-bar">
            <el-input
              v-model="remotePath"
              size="small"
              placeholder="远程路径"
              @keyup.enter="remoteGoToInput"
            >
              <template #prefix>
                <el-icon><Document /></el-icon>
              </template>
            </el-input>
          </div>
          <div class="file-table" v-loading="remoteLoading">
            <div class="file-header">
              <span class="file-name" @click="toggleRemoteSort('name')">
                名称<span v-if="remoteSortKey === 'name'">{{ remoteSortDesc ? " ↓" : " ↑" }}</span>
              </span>
              <span class="file-mtime" @click="toggleRemoteSort('modified')">
                修改时间<span v-if="remoteSortKey === 'modified'">{{ remoteSortDesc ? " ↓" : " ↑" }}</span>
              </span>
              <span class="file-size" @click="toggleRemoteSort('size')">
                大小<span v-if="remoteSortKey === 'size'">{{ remoteSortDesc ? " ↓" : " ↑" }}</span>
              </span>
              <span class="row-actions"></span>
            </div>
            <div
              v-for="(e, idx) in sortedRemote"
              :key="'r-' + idx"
              class="file-row"
              :class="{ selected: selectedRemote?.name === e.name }"
              :draggable="!e.isDir"
              @click="selectedRemote = e"
              @dblclick="remoteEnter(e)"
              @dragstart="onRowDragStart('remote', e)"
            >
              <el-icon class="file-icon" :class="{ 'is-dir': e.isDir }">
                <Folder v-if="e.isDir" />
                <Document v-else />
              </el-icon>
              <span class="file-name" :title="e.name">{{ e.name }}</span>
              <span class="file-mtime">{{ formatTime(e.modified) }}</span>
              <span class="file-size">{{ humanSize(e.size) }}</span>
              <span class="row-actions">
                <el-icon
                  class="row-action"
                  title="重命名"
                  @click.stop="remoteRename(e)"
                >
                  <EditPen />
                </el-icon>
                <el-icon
                  class="row-action danger"
                  title="删除"
                  @click.stop="remoteRemove(e)"
                >
                  <Delete />
                </el-icon>
              </span>
            </div>
            <div v-if="!remoteLoading && remoteEntries.length === 0" class="empty-row">
              目录为空
            </div>
          </div>
        </section>
      </template>

      <!-- 未连接占位 -->
      <div v-else class="placeholder">
        <el-icon class="placeholder-icon"><FolderOpened /></el-icon>
        <p class="placeholder-text">请选择会话并打开 SFTP 连接</p>
        <el-button type="primary" :icon="Link" :loading="connecting" @click="connectSftp">
          打开 SFTP 连接
        </el-button>
      </div>
    </div>

    <!-- 底部传输队列 -->
    <TransferQueue />
  </div>
</template>

<style scoped>
.sftp-view {
  display: flex;
  flex-direction: column;
  width: 100%;
  height: 100%;
  overflow: hidden;
  background: var(--el-bg-color-page);
}

/* 顶部工具栏 */
.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 8px 12px;
  background: var(--el-bg-color-overlay);
  border-bottom: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
  flex-wrap: wrap;
}
.session-picker {
  display: flex;
  align-items: center;
  gap: 8px;
}
.picker-label {
  font-size: 13px;
  color: var(--el-text-color-secondary);
  white-space: nowrap;
}
.session-select {
  width: 280px;
}
.toolbar-actions {
  display: flex;
  align-items: center;
  gap: 8px;
}
.conn-tag {
  margin-left: 4px;
}

/* 主体 */
.sftp-body {
  flex: 1;
  display: flex;
  flex-direction: row;
  align-items: stretch;
  min-height: 0;
  overflow: hidden;
}

/* 面板（本地 / 远程） */
.pane {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  background: var(--el-bg-color-overlay);
  overflow: hidden;
}
/* 拖拽传输高亮（面板间 + OS 拖入） */
.pane.drag-over {
  outline: 2px dashed var(--el-color-primary);
  outline-offset: -4px;
  background: var(--el-color-primary-light-9);
}
/* 可拖拽行光标 */
.file-row[draggable="true"] {
  cursor: grab;
}
.file-row[draggable="true"]:active {
  cursor: grabbing;
}
.pane-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 6px 12px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
}
.pane-title {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
  font-weight: 600;
  color: var(--el-text-color-primary);
}
.pane-title .el-icon {
  color: var(--el-color-primary);
}
.pane-tools {
  display: flex;
  gap: 4px;
}

.path-bar {
  padding: 8px 12px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
}
/* 面包屑 */
.breadcrumb {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 2px;
  padding: 6px 12px 0;
  font-size: 12px;
  color: var(--el-text-color-secondary);
  flex-shrink: 0;
}
.breadcrumb-item {
  cursor: pointer;
  padding: 1px 4px;
  border-radius: 3px;
}
.breadcrumb-item:hover {
  background: var(--el-fill-color-light);
  color: var(--el-color-primary);
}
.breadcrumb-sep {
  color: var(--el-text-color-placeholder);
}

/* 文件列表 */
.file-table {
  flex: 1;
  overflow: auto;
  padding: 4px 0;
  font-size: 13px;
}
.file-row {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 4px 12px;
  cursor: pointer;
  user-select: none;
  color: var(--el-text-color-regular);
}
/* 排序表头 */
.file-header {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 4px 12px;
  font-size: 11px;
  color: var(--el-text-color-secondary);
  border-bottom: 1px solid var(--el-border-color-lighter);
  background: var(--el-fill-color-light);
  position: sticky;
  top: 0;
  z-index: 1;
}
.file-header > span {
  cursor: pointer;
  white-space: nowrap;
}
.file-header > span:hover {
  color: var(--el-color-primary);
}
.file-row:hover {
  background: var(--el-fill-color-light);
}
.file-row.selected {
  background: var(--el-color-primary-light-9);
  color: var(--el-color-primary);
}
.file-icon {
  font-size: 15px;
  flex-shrink: 0;
  color: var(--el-text-color-secondary);
}
.file-icon.is-dir {
  color: var(--el-color-primary);
}
.file-name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.file-size {
  width: 80px;
  text-align: right;
  color: var(--el-text-color-secondary);
  font-variant-numeric: tabular-nums;
  flex-shrink: 0;
}
.file-mtime {
  width: 140px;
  color: var(--el-text-color-secondary);
  font-variant-numeric: tabular-nums;
  flex-shrink: 0;
}
.row-actions {
  display: none;
  align-items: center;
  gap: 6px;
  width: 50px;
  justify-content: flex-end;
  flex-shrink: 0;
}
.file-row:hover .row-actions {
  display: flex;
}
.row-action {
  font-size: 14px;
  color: var(--el-text-color-secondary);
  padding: 2px;
  border-radius: 4px;
}
.row-action:hover {
  color: var(--el-color-primary);
  background: var(--el-fill-color);
}
.row-action.danger:hover {
  color: var(--el-color-danger);
}
.empty-row {
  padding: 24px;
  text-align: center;
  color: var(--el-text-color-placeholder);
  font-size: 12px;
}

/* 中间传输按钮 */
.transfer-arrows {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  padding: 0 6px;
  flex-shrink: 0;
  background: var(--el-bg-color-page);
}

/* 占位 */
.placeholder {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 14px;
  color: var(--el-text-color-secondary);
}
.placeholder-icon {
  font-size: 48px;
  color: var(--el-text-color-placeholder);
}
.placeholder-text {
  margin: 0;
  font-size: 14px;
}

/* 滚动条 */
.file-table::-webkit-scrollbar {
  width: 6px;
}
.file-table::-webkit-scrollbar-thumb {
  background: var(--el-border-color);
  border-radius: 3px;
}
</style>
