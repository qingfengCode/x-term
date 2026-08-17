<!--
  FileExplorerView.vue — 远程文件存储（S3 / 兼容存储）文件浏览器

  独立于 SFTP：左侧本地栏（@tauri-apps/plugin-fs）+ 右侧远端栏（file_* API，
  后端走 S3 SigV4）。支持账号管理、连接、列目录、上传/下载、新建目录、删除、重命名。
  传输进度复用 transfer:* 事件 + TransferQueue 组件（与 SFTP 视图共用）。
-->
<script setup lang="ts">
import { computed, onMounted, onBeforeUnmount, reactive, ref } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import {
  ArrowUp,
  Refresh,
  FolderAdd,
  FolderOpened,
  Document,
  Folder,
  Delete,
  Edit,
  Plus,
  Link as LinkIcon,
  Files,
} from "@element-plus/icons-vue";
import { readDir, mkdir, stat as fsStat } from "@tauri-apps/plugin-fs";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { homeDir, join, dirname, basename } from "@tauri-apps/api/path";

import { useTransferStore } from "@/stores/transfer";
import { formatSize } from "@/utils/format";
import type { FileEntry } from "@/api/types";
import {
  fileAccountList,
  fileAccountDelete,
  fileConnect,
  fileDisconnect,
  fileList,
  fileStat,
  fileMkdir,
  fileRename,
  fileRemove,
  fileDownload,
  fileUpload,
  type FileAccount,
} from "@/api/fileBackend";
import FileAccountDialog from "@/components/FileAccountDialog.vue";
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

const transfer = useTransferStore();

// ---------------------------------------------------------------------------
// 账号管理
// ---------------------------------------------------------------------------
const accounts = ref<FileAccount[]>([]);
const selectedAccountId = ref<string>("");
const accountDialogVisible = ref(false);
const editingAccount = ref<FileAccount | null>(null);

async function loadAccounts() {
  try {
    accounts.value = await fileAccountList();
  } catch (e) {
    ElMessage.error("加载文件账号失败: " + String(e));
  }
}

function openNewAccount() {
  editingAccount.value = null;
  accountDialogVisible.value = true;
}

function openEditAccount(a: FileAccount) {
  editingAccount.value = a;
  accountDialogVisible.value = true;
}

async function removeAccount(a: FileAccount) {
  try {
    await ElMessageBox.confirm(`确认删除账号「${a.name}」？`, "删除账号", {
      type: "warning",
    });
  } catch {
    return;
  }
  try {
    await fileAccountDelete(a.id);
    ElMessage.success("已删除账号");
    if (selectedAccountId.value === a.id) {
      await disconnect();
      selectedAccountId.value = "";
    }
    await loadAccounts();
  } catch (e) {
    ElMessage.error("删除失败: " + String(e));
  }
}

async function onAccountSaved(_a: FileAccount) {
  await loadAccounts();
}

// ---------------------------------------------------------------------------
// 连接生命周期
// ---------------------------------------------------------------------------
const backendId = ref<string>("");
const connecting = ref(false);
const isConnected = computed(() => !!backendId.value);

async function connect() {
  if (!selectedAccountId.value) {
    ElMessage.warning("请先选择账号");
    return;
  }
  connecting.value = true;
  try {
    await disconnect();
    backendId.value = await fileConnect(selectedAccountId.value);
    ElMessage.success("已连接");
    // 连接后加载根目录。
    remotePath.value = "";
    await loadRemote("");
  } catch (e) {
    ElMessage.error("连接失败: " + String(e));
  } finally {
    connecting.value = false;
  }
}

async function disconnect() {
  if (!backendId.value) return;
  try {
    await fileDisconnect(backendId.value);
  } catch {
    // 忽略：可能已断开。
  }
  backendId.value = "";
  remoteEntries.value = [];
}

// ---------------------------------------------------------------------------
// 列排序（本地 / 远端共享排序状态机）
// ---------------------------------------------------------------------------
type SortKey = "name" | "modified" | "size";
const localSortKey = ref<SortKey>("name");
const localSortDesc = ref(false);
const remoteSortKey = ref<SortKey>("name");
const remoteSortDesc = ref(false);

/** 排序工厂：目录永远靠前，文件/目录各自按 sortKey 比较，desc 控制升降序。 */
function makeSorted(
  entries: UnifiedEntry[],
  key: SortKey,
  desc: boolean,
): UnifiedEntry[] {
  const sign = desc ? -1 : 1;
  return [...entries].sort((a, b) => {
    if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
    let cmp = 0;
    if (key === "name") {
      cmp = a.name.localeCompare(b.name, undefined, { numeric: true });
    } else if (key === "size") {
      cmp = a.size - b.size;
    } else {
      // modified：null 视为最早。
      const ta = a.modified ? new Date(a.modified).getTime() : 0;
      const tb = b.modified ? new Date(b.modified).getTime() : 0;
      cmp = ta - tb;
    }
    return cmp * sign;
  });
}

/** 原始（未排序）的本地/远端条目，computed 再做排序。 */
function sortEntries(arr: UnifiedEntry[]) {
  // 保留供 loadLocal/loadRemote 内部用（仅做目录在前 + 名称的基础排序，
  // 实际展示以 sorted* computed 为准）。这里仍保持目录在前。
  arr.sort((a, b) => {
    if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
    return a.name.localeCompare(b.name, undefined, { numeric: true });
  });
}

function toggleLocalSort(key: SortKey) {
  if (localSortKey.value === key) localSortDesc.value = !localSortDesc.value;
  else {
    localSortKey.value = key;
    localSortDesc.value = false;
  }
}
function toggleRemoteSort(key: SortKey) {
  if (remoteSortKey.value === key) remoteSortDesc.value = !remoteSortDesc.value;
  else {
    remoteSortKey.value = key;
    remoteSortDesc.value = false;
  }
}

// ---------------------------------------------------------------------------
// 本地文件浏览
// ---------------------------------------------------------------------------
const localPath = ref<string>("");
const localEntries = ref<UnifiedEntry[]>([]);
const localLoading = ref(false);
const selectedLocal = ref<UnifiedEntry | null>(null);
const sortedLocal = computed(() =>
  makeSorted(localEntries.value, localSortKey.value, localSortDesc.value),
);

// 加载竞态防护：每次加载取递增序号，响应返回时若序号已过期（期间又发起了
// 新加载）则丢弃结果，避免先发的慢请求覆盖新导航的列表/loading 状态。
let localLoadSeq = 0;
let remoteLoadSeq = 0;

async function loadLocal(path: string) {
  const seq = ++localLoadSeq;
  localLoading.value = true;
  try {
    const raw = await readDir(path);
    if (seq !== localLoadSeq) return; // 过期响应丢弃。
    const items: UnifiedEntry[] = [];
    for (const e of raw) {
      if (e.name === "." || e.name === "..") continue;
      let size = 0;
      let modified: string | null = null;
      try {
        const full = await join(path, e.name);
        const info = await fsStat(full);
        size = info.size;
        modified = info.mtime ? new Date(info.mtime).toISOString() : null;
      } catch {
        // 无法 stat 的条目用默认值。
      }
      items.push({ name: e.name, isDir: e.isDirectory, size, modified });
    }
    if (seq !== localLoadSeq) return;
    sortEntries(items);
    localEntries.value = items;
    localPath.value = path;
    // 目录切换/刷新后清空选中：残留高亮可能指向已不存在的同名文件。
    selectedLocal.value = null;
  } catch (e) {
    if (seq !== localLoadSeq) return;
    ElMessage.error("读取本地目录失败: " + String(e));
  } finally {
    if (seq === localLoadSeq) localLoading.value = false;
  }
}

async function refreshLocal() {
  if (localPath.value) await loadLocal(localPath.value);
}

async function localEnter(entry: UnifiedEntry) {
  if (!entry.isDir) return;
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
    await refreshLocal();
  } catch (e) {
    ElMessage.error("创建失败: " + String(e));
  }
}

// ---------------------------------------------------------------------------
// 远端（S3）文件浏览
// ---------------------------------------------------------------------------
const remotePath = ref<string>("");
const remoteEntries = ref<UnifiedEntry[]>([]);
const remoteLoading = ref(false);
const selectedRemote = ref<UnifiedEntry | null>(null);
const sortedRemote = computed(() =>
  makeSorted(remoteEntries.value, remoteSortKey.value, remoteSortDesc.value),
);

/** 远端面包屑：把 remotePath 按 `/` 拆成可点击段。根段 label 为 bucket 名。 */
const remoteCrumbs = computed(() => {
  const account = accounts.value.find((a) => a.id === selectedAccountId.value);
  const rootLabel = account?.bucket || "根";
  const parts = remotePath.value.replace(/^\/+|\/+$/g, "").split("/").filter(Boolean);
  const crumbs = [{ label: rootLabel, path: "" }];
  let acc = "";
  for (const p of parts) {
    acc = acc ? `${acc}/${p}` : p;
    crumbs.push({ label: p, path: acc });
  }
  return crumbs;
});

/** 点击面包屑跳转到指定路径。 */
function remoteGoToCrumb(path: string) {
  loadRemote(path);
}

/** S3 路径拼接：以 `/` 分隔，根为空串。 */
function joinRemote(name: string): string {
  const base = remotePath.value.replace(/^\/+|\/+$/g, "");
  if (!base) return name;
  return `${base}/${name}`;
}

async function loadRemote(path: string) {
  if (!backendId.value) return;
  const seq = ++remoteLoadSeq;
  remoteLoading.value = true;
  try {
    const entries = await fileList(backendId.value, path);
    if (seq !== remoteLoadSeq) return; // 过期响应丢弃。
    const items: UnifiedEntry[] = entries.map((e) => ({
      name: e.name,
      isDir: e.isDir,
      size: e.size,
      modified: e.modified,
    }));
    sortEntries(items);
    remoteEntries.value = items;
    remotePath.value = path;
    // 目录切换/刷新后清空选中：残留高亮可能指向已不存在的同名文件。
    selectedRemote.value = null;
  } catch (e) {
    if (seq !== remoteLoadSeq) return;
    ElMessage.error("读取远端目录失败: " + String(e));
  } finally {
    if (seq === remoteLoadSeq) remoteLoading.value = false;
  }
}

async function refreshRemote() {
  await loadRemote(remotePath.value);
}

async function remoteEnter(entry: UnifiedEntry) {
  if (!entry.isDir) return;
  await loadRemote(joinRemote(entry.name));
}

async function remoteGoUp() {
  if (!remotePath.value) return;
  const parts = remotePath.value.replace(/\/+$/g, "").split("/").filter(Boolean);
  parts.pop();
  await loadRemote(parts.join("/"));
}

async function remoteGoToInput() {
  const p = remotePath.value.trim().replace(/^\/+|\/+$/g, "");
  await loadRemote(p);
}

async function remoteMkdir() {
  if (!backendId.value) return;
  let name: string;
  try {
    const res = await ElMessageBox.prompt("请输入新目录名称", "新建远端目录", {
      confirmButtonText: "创建",
      cancelButtonText: "取消",
      inputValidator: (v) => !!v?.trim() || "名称不能为空",
    });
    name = res.value.trim();
  } catch {
    return;
  }
  try {
    await fileMkdir(backendId.value, joinRemote(name));
    await refreshRemote();
  } catch (e) {
    ElMessage.error("创建失败: " + String(e));
  }
}

async function remoteRename(entry: UnifiedEntry) {
  if (!backendId.value) return;
  let name: string;
  try {
    const res = await ElMessageBox.prompt("请输入新名称", "重命名", {
      confirmButtonText: "确定",
      cancelButtonText: "取消",
      inputValue: entry.name,
      inputValidator: (v) => !!v?.trim() || "名称不能为空",
    });
    name = res.value.trim();
  } catch {
    return;
  }
  if (name === entry.name) return;
  try {
    await fileRename(backendId.value, joinRemote(entry.name), joinRemote(name));
    await refreshRemote();
  } catch (e) {
    ElMessage.error("重命名失败: " + String(e));
  }
}

async function remoteRemove(entry: UnifiedEntry) {
  if (!backendId.value) return;
  try {
    await ElMessageBox.confirm(
      `确认删除「${entry.name}」${entry.isDir ? "（及其所有内容）" : ""}？`,
      "删除",
      { type: "warning" },
    );
  } catch {
    return;
  }
  try {
    await fileRemove(backendId.value, joinRemote(entry.name), entry.isDir);
    await refreshRemote();
  } catch (e) {
    ElMessage.error("删除失败: " + String(e));
  }
}

// ---------------------------------------------------------------------------
// 上传 / 下载
// ---------------------------------------------------------------------------
/** 行内"上传"按钮：用 join 拼绝对路径（旧实现把 path 与 sep 函数对象直接
 *  字符串拼接，路径必坏），完成后刷新远端列表。 */
async function onRowUpload(e: UnifiedEntry) {
  if (e.isDir) return;
  const full = await join(localPath.value, e.name);
  await uploadOne(full, e.name);
  await refreshRemote();
}

// --- 覆盖保护（上传/下载前确认，避免静默覆盖同名文件造成数据丢失） ---

async function confirmOverwrite(message: string): Promise<boolean> {
  try {
    await ElMessageBox.confirm(message, "覆盖确认", {
      type: "warning",
      confirmButtonText: "覆盖",
      cancelButtonText: "取消",
    });
    return true;
  } catch {
    return false;
  }
}

/** 远端目标已存在同名文件时弹确认；不存在或 stat 失败放行。 */
async function confirmRemoteOverwrite(remoteAbs: string, name: string): Promise<boolean> {
  try {
    await fileStat(backendId.value, remoteAbs);
  } catch {
    return true;
  }
  return confirmOverwrite(`远端已存在同名文件「${name}」，是否覆盖？`);
}

/** 本地目标已存在同名文件时弹确认。 */
async function confirmLocalOverwrite(localAbs: string, name: string): Promise<boolean> {
  try {
    await fsStat(localAbs);
  } catch {
    return true; // 本地不存在，直接写。
  }
  return confirmOverwrite(`本地已存在同名文件「${name}」，是否覆盖？`);
}

async function uploadOne(localAbs: string, name: string) {
  if (!backendId.value) {
    ElMessage.warning("请先连接账号");
    return;
  }
  const remoteAbs = joinRemote(name);
  // 覆盖保护：旧实现直接覆盖远端同名文件，可能丢失远端数据。
  if (!(await confirmRemoteOverwrite(remoteAbs, name))) return;
  const taskId = crypto.randomUUID();
  transfer.add({
    id: taskId,
    name,
    direction: "upload",
    transferred: 0,
    total: 0,
    status: "pending",
  });
  // 后端命令是整体 await 的（完成才返回），所以"运行中"必须在发起前标记，
  // 否则 await 返回后再更新会覆盖 transfer:done 事件的完成状态。
  transfer.update(taskId, { status: "running" });
  try {
    await fileUpload({
      backendId: backendId.value,
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

async function downloadOne(remoteName: string) {
  if (!backendId.value) return;
  // 选择本地保存路径。
  let savePath: string;
  try {
    const picked = await openFileDialog({
      defaultPath: localPath.value,
      directory: true,
    });
    if (!picked) return;
    savePath = picked as string;
  } catch {
    return;
  }
  const full = await join(savePath, remoteName);
  // 覆盖保护：旧实现直接写盘覆盖本地同名文件，可能丢失本地数据。
  if (!(await confirmLocalOverwrite(full, remoteName))) return;
  const taskId = crypto.randomUUID();
  transfer.add({
    id: taskId,
    name: remoteName,
    direction: "download",
    transferred: 0,
    total: 0,
    status: "pending",
  });
  // 同 uploadOne：后端命令整体 await，"运行中"需在发起前标记。
  transfer.update(taskId, { status: "running" });
  try {
    await fileDownload({
      backendId: backendId.value,
      remotePath: joinRemote(remoteName),
      localPath: full,
      taskId,
    });
    ElMessage.success(`下载完成 ${remoteName}`);
  } catch (e) {
    transfer.update(taskId, { status: "error", message: String(e) });
    ElMessage.error("下载失败: " + String(e));
  }
}

// 上传按钮：弹出本地文件选择，可多选。
async function pickAndUpload() {
  try {
    const picked = await openFileDialog({
      defaultPath: localPath.value,
      multiple: true,
      directory: false,
    });
    const paths = Array.isArray(picked) ? picked : picked ? [picked] : [];
    if (paths.length === 0) return;
    for (const p of paths) {
      const name = await basename(p as string);
      await uploadOne(p as string, name);
    }
    await refreshRemote();
  } catch (e) {
    ElMessage.error("上传失败: " + String(e));
  }
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------
function humanSize(n: number): string {
  if (!n || n <= 0) return "-";
  return formatSize(n);
}

function formatTime(s: string | null): string {
  if (!s) return "-";
  try {
    return new Date(s).toLocaleString();
  } catch {
    return s;
  }
}

// ---------------------------------------------------------------------------
// 生命周期
// ---------------------------------------------------------------------------
let transferDoneUnlisten: UnlistenFn | null = null;
let dragDropUnlisten: UnlistenFn | null = null;
/** 原生文件拖入远端面板时的高亮状态。 */
const dragOver = ref(false);
// 组件销毁标志：监听注册未 resolve 前组件可能已卸载，resolve 后据此立即反订阅。
let unmounted = false;

onMounted(async () => {
  await loadAccounts();
  // 初始本地路径为用户家目录。
  try {
    const home = await homeDir();
    await loadLocal(home);
  } catch {
    // 忽略。
  }
  // 传输完成事件：按方向刷新对应面板（上传→刷远端，下载→刷本地）。
  try {
    const fn = await listen<{ taskId: string }>("transfer:done", (e) => {
      if (unmounted) return;
      const task = transfer.tasks.find((t) => t.id === e.payload.taskId);
      if (task) {
        transfer.update(e.payload.taskId, { status: "done", transferred: task.total || 0 });
        if (task.direction === "upload") refreshRemote();
        else refreshLocal();
      }
    });
    if (unmounted) fn();
    else transferDoneUnlisten = fn;
  } catch (e) {
    console.error("传输完成事件订阅失败:", e);
  }
  // 原生文件拖拽上传（Tauri onDragDropEvent 拿 OS 文件真实路径）。
  try {
    const { getCurrentWebview } = await import("@tauri-apps/api/webview");
    const fn = await getCurrentWebview().onDragDropEvent((event) => {
      if (unmounted) return;
      if (event.payload.type === "enter" || event.payload.type === "over") {
        dragOver.value = true;
      } else {
        dragOver.value = false;
        if (event.payload.type === "drop") {
          const paths = (event.payload.paths || []) as string[];
          for (const p of paths) {
            const name = p.split(/[\\/]/).pop() || p;
            uploadOne(p, name).then(() => refreshRemote());
          }
        }
      }
    });
    if (unmounted) fn();
    else dragDropUnlisten = fn;
  } catch {
    // 非 Tauri 环境忽略。
  }
});

onBeforeUnmount(() => {
  unmounted = true;
  if (transferDoneUnlisten) transferDoneUnlisten();
  if (dragDropUnlisten) dragDropUnlisten();
  // 离开页面时断开连接。
  disconnect();
});
</script>

<template>
  <div class="file-explorer-view">
    <!-- 顶部工具栏 -->
    <header class="toolbar">
      <div class="account-picker">
        <span class="picker-label">文件账号</span>
        <el-select
          v-model="selectedAccountId"
          placeholder="选择 S3 / 兼容存储账号"
          size="default"
          filterable
          class="account-select"
          :disabled="connecting"
        >
          <el-option
            v-for="a in accounts"
            :key="a.id"
            :label="`${a.name} (${a.bucket || a.endpoint})`"
            :value="a.id"
          />
        </el-select>
        <el-button :icon="Plus" size="default" @click="openNewAccount">新建账号</el-button>
        <el-button
          size="default"
          :disabled="!selectedAccountId"
          @click="openEditAccount(accounts.find((a) => a.id === selectedAccountId)!)"
        >
          编辑
        </el-button>
        <el-button
          size="default"
          type="danger"
          plain
          :disabled="!selectedAccountId"
          @click="removeAccount(accounts.find((a) => a.id === selectedAccountId)!)"
        >
          删除
        </el-button>
      </div>

      <div class="toolbar-actions">
        <el-button
          type="primary"
          :icon="LinkIcon"
          :loading="connecting"
          @click="connect"
        >
          {{ isConnected ? "重连" : "连接" }}
        </el-button>
        <el-button :disabled="!isConnected" @click="disconnect">断开</el-button>
        <el-tag v-if="isConnected" type="success" size="default" effect="light" class="conn-tag">
          已连接 · {{ accounts.find((a) => a.id === selectedAccountId)?.name }}
        </el-tag>
      </div>
    </header>

    <!-- 主体：双栏 -->
    <div class="fe-body">
      <template v-if="isConnected">
        <!-- 本地栏 -->
        <section class="pane">
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
            />
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
              v-for="e in sortedLocal"
              :key="'l-' + e.name"
              class="file-row"
              :class="{ selected: selectedLocal?.name === e.name }"
              :draggable="!e.isDir"
              @click="selectedLocal = e"
              @dblclick="localEnter(e)"
            >
              <el-icon class="file-icon" :class="{ 'is-dir': e.isDir }">
                <Folder v-if="e.isDir" />
                <Document v-else />
              </el-icon>
              <span class="file-name" :title="e.name">{{ e.name }}</span>
              <span class="file-mtime">{{ formatTime(e.modified) }}</span>
              <span class="file-size">{{ humanSize(e.size) }}</span>
              <span class="row-actions">
                <el-button
                  v-if="!e.isDir"
                  link
                  size="small"
                  type="primary"
                  @click.stop="onRowUpload(e)"
                >
                  上传
                </el-button>
              </span>
            </div>
            <div v-if="!localLoading && localEntries.length === 0" class="empty-row">
              目录为空
            </div>
          </div>
        </section>

        <!-- 远端栏 -->
        <section class="pane" :class="{ 'drag-over': dragOver }">
          <div class="pane-header">
            <span class="pane-title">
              <el-icon><Files /></el-icon>
              远端（S3）
            </span>
            <div class="pane-tools">
              <el-tooltip content="上传文件" placement="bottom">
                <el-button circle size="small" type="primary" @click="pickAndUpload">
                  <el-icon><Plus /></el-icon>
                </el-button>
              </el-tooltip>
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
          <!-- 面包屑导航：点击任意段跳转 -->
          <div class="breadcrumb-bar">
            <template v-for="(c, i) in remoteCrumbs" :key="i">
              <span class="crumb" @click="remoteGoToCrumb(c.path)">{{ c.label }}</span>
              <span v-if="i < remoteCrumbs.length - 1" class="crumb-sep">/</span>
            </template>
          </div>
          <div class="path-bar">
            <el-input
              v-model="remotePath"
              size="small"
              placeholder="（bucket 根）"
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
              v-for="e in sortedRemote"
              :key="'r-' + e.name"
              class="file-row"
              :class="{ selected: selectedRemote?.name === e.name }"
              @click="selectedRemote = e"
              @dblclick="remoteEnter(e)"
            >
              <el-icon class="file-icon" :class="{ 'is-dir': e.isDir }">
                <Folder v-if="e.isDir" />
                <Document v-else />
              </el-icon>
              <span class="file-name" :title="e.name">{{ e.name }}</span>
              <span class="file-mtime">{{ formatTime(e.modified) }}</span>
              <span class="file-size">{{ humanSize(e.size) }}</span>
              <span class="row-actions">
                <el-button v-if="!e.isDir" link size="small" type="success" @click.stop="downloadOne(e.name)">
                  下载
                </el-button>
                <el-button link size="small" @click.stop="remoteRename(e)">
                  <el-icon><Edit /></el-icon>
                </el-button>
                <el-button link size="small" type="danger" @click.stop="remoteRemove(e)">
                  <el-icon><Delete /></el-icon>
                </el-button>
              </span>
            </div>
            <div v-if="!remoteLoading && remoteEntries.length === 0" class="empty-row">
              {{ dragOver ? "松开鼠标上传文件" : "目录为空" }}
            </div>
          </div>
        </section>
      </template>

      <!-- 未连接占位 -->
      <div v-else class="fe-placeholder">
        <el-icon :size="48"><Files /></el-icon>
        <p>选择一个文件账号并点击「连接」</p>
        <el-button type="primary" :icon="Plus" @click="openNewAccount">新建 S3 账号</el-button>
      </div>
    </div>

    <!-- 传输队列（与 SFTP 共用） -->
    <TransferQueue />

    <!-- 账号配置弹窗 -->
    <FileAccountDialog
      v-model:visible="accountDialogVisible"
      :account="editingAccount"
      @saved="onAccountSaved"
    />
  </div>
</template>

<style scoped>
.file-explorer-view {
  display: flex;
  flex-direction: column;
  height: 100%;
  background: var(--el-bg-color);
}

.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 8px 12px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  flex-wrap: wrap;
  gap: 8px;
}
.account-picker {
  display: flex;
  align-items: center;
  gap: 8px;
}
.picker-label {
  color: var(--el-text-color-secondary);
  font-size: 13px;
}
.account-select {
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

.fe-body {
  flex: 1;
  display: flex;
  gap: 8px;
  padding: 8px;
  overflow: hidden;
}

.pane {
  flex: 1;
  display: flex;
  flex-direction: column;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  overflow: hidden;
  min-width: 0;
}
/* 原生文件拖入高亮 */
.pane.drag-over {
  border-color: var(--el-color-primary);
  background: var(--el-color-primary-light-9);
}
/* 面包屑 */
.breadcrumb-bar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 2px;
  padding: 4px 8px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  font-size: 12px;
}
.crumb {
  color: var(--el-color-primary);
  cursor: pointer;
}
.crumb:hover {
  text-decoration: underline;
}
.crumb-sep {
  color: var(--el-text-color-secondary);
  margin: 0 2px;
}
/* 列排序表头 */
.file-header {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px 8px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  font-size: 12px;
  color: var(--el-text-color-secondary);
  position: sticky;
  top: 0;
  /* 与 SftpView 表头统一使用填充色，滚动时与行内容区分 */
  background: var(--el-fill-color-light);
  z-index: 1;
}
.file-header .file-name,
.file-header .file-mtime,
.file-header .file-size {
  cursor: pointer;
  user-select: none;
}
.file-header .file-name:hover,
.file-header .file-mtime:hover,
.file-header .file-size:hover {
  color: var(--el-color-primary);
}
/* 行选中态 */
.file-row.selected {
  background: var(--el-color-primary-light-9);
}
.pane-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 6px 8px;
  background: var(--el-fill-color-light);
  border-bottom: 1px solid var(--el-border-color-lighter);
}
.pane-title {
  display: flex;
  align-items: center;
  gap: 4px;
  font-weight: 500;
  font-size: 13px;
}
.pane-tools {
  display: flex;
  gap: 4px;
}
.path-bar {
  padding: 6px 8px;
  border-bottom: 1px solid var(--el-border-color-lighter);
}
.file-table {
  flex: 1;
  overflow-y: auto;
  padding: 4px;
}
.file-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px 8px;
  border-radius: 4px;
  cursor: pointer;
  font-size: 13px;
}
.file-row:hover {
  background: var(--el-fill-color-light);
}
.file-icon {
  color: var(--el-text-color-secondary);
  flex-shrink: 0;
}
.file-icon.is-dir {
  color: var(--el-color-primary);
}
.file-name {
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.file-size {
  width: 80px;
  text-align: right;
  color: var(--el-text-color-secondary);
  font-variant-numeric: tabular-nums;
}
.file-mtime {
  width: 150px;
  color: var(--el-text-color-secondary);
  font-size: 12px;
}
.row-actions {
  /* 与 SftpView 统一：悬停行时浮现操作按钮，平时隐藏节省空间 */
  display: none;
  align-items: center;
  justify-content: flex-end;
  gap: 6px;
  flex-shrink: 0;
}
.file-row:hover .row-actions {
  display: flex;
}
.empty-row {
  padding: 24px;
  text-align: center;
  color: var(--el-text-color-secondary);
}

.fe-placeholder {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 12px;
  color: var(--el-text-color-secondary);
}
</style>
