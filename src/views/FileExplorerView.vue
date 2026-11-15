<!--
  FileExplorerView.vue — 文件传输页（原 SFTP 视图与对象存储视图合并）

  左右双栏：本地（@tauri-apps/plugin-fs）+ 远端。远端后端由顶部统一连接选择器决定：
  - SSH 会话 → SFTP（openSftpForSession / sftp_* API，Unix 绝对路径）
  - 对象存储账号 → file_* API（后端走 S3 SigV4，bucket 内相对路径）
  两类目标分组列在同一个下拉里，选中谁就以对应后端连接，点击路径与旧的两个
  一级入口一致（点导航 → 选连接）。

  传输入口：中间方向按钮 / 行内上传下载 / 面板间拖拽（HTML5 DnD）/ OS 文件
  拖入（Tauri 原生 onDragDropEvent，落点在页面内即上传到远端当前目录）。
  覆盖保护、新建/删除/重命名、账号管理与两个旧视图一致。传输进度复用
  transfer:* 事件 + TransferQueue（事件由 MainLayout 统一写入 transferStore）。

  上次使用的连接目标记在 localStorage（xterm.files.lastTarget），重进页面默认
  选中；未记录时默认第一个 SSH 会话，否则第一个存储账号。
-->
<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { readDir, mkdir, stat as fsStat } from "@tauri-apps/plugin-fs";
import { open as openFileDialog, save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { homeDir, join, dirname, basename } from "@tauri-apps/api/path";

import { useSessionsStore } from "@/stores/sessions";
import { useTransferStore } from "@/stores/transfer";
import { uniquePathIn, usableDownloadDir } from "@/utils/downloadPath";
import type { FileEntry } from "@/api/types";
import {
  sftpList,
  sftpStat,
  sftpPwd,
  sftpMkdir,
  sftpRename,
  sftpRemove,
  sftpDownload,
  sftpUpload,
  sftpClose,
} from "@/api/sftp";
import { openSftpForSession } from "@/api/session";
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
import EmptyState from "@/components/EmptyState.vue";
import FilePane from "@/components/FilePane.vue";
import { useFilePane, type UnifiedEntry } from "@/composables/useFilePane";

const sessionsStore = useSessionsStore();
const transfer = useTransferStore();

// ---------------------------------------------------------------------------
// 连接目标（统一选择器："sftp:<sessionId>" / "s3:<accountId>"）
// ---------------------------------------------------------------------------
const LAST_TARGET_KEY = "xterm.files.lastTarget";

const accounts = ref<FileAccount[]>([]);
const selectedTarget = ref("");

/** 选择器当前指向的后端类型（与「已连接」的类型独立：选择器只是意向）。 */
const targetKind = computed<"sftp" | "s3" | null>(() => {
  if (selectedTarget.value.startsWith("sftp:")) return "sftp";
  if (selectedTarget.value.startsWith("s3:")) return "s3";
  return null;
});
const selectedSessionId = computed(() =>
  targetKind.value === "sftp" ? selectedTarget.value.slice(5) : "",
);
const selectedAccountId = computed(() =>
  targetKind.value === "s3" ? selectedTarget.value.slice(3) : "",
);

// ---------------------------------------------------------------------------
// 连接生命周期
// ---------------------------------------------------------------------------
/** 当前已连接的后端类型；null = 未连接。 */
const activeKind = ref<"sftp" | "s3" | null>(null);
const sftpId = ref("");
const backendId = ref("");
const connecting = ref(false);
/** 连接成功时记录的显示名（会话名 / 账号名），用于标题与状态标签。 */
const connectedName = ref("");
/** S3 连接的账号 ID（删除账号时判断是否断开当前连接；名字可能重复，不能用名字比）。 */
const connectedAccountId = ref("");
/** S3 连接的 bucket 名（面包屑根段标签）。 */
const connectedBucket = ref("");

const isConnected = computed(() =>
  activeKind.value === "sftp"
    ? !!sftpId.value
    : activeKind.value === "s3"
      ? !!backendId.value
      : false,
);

/** 远端操作的连接句柄（按当前已连接的类型取）。 */
function activeBackendId(): string {
  return activeKind.value === "sftp" ? sftpId.value : backendId.value;
}

async function connect() {
  const kind = targetKind.value;
  if (!kind) {
    ElMessage.warning("请先选择 SSH 会话或对象存储账号");
    return;
  }
  // 已连接时先关闭旧连接（任意类型），避免后端连接与 sftp_sessions 注册条目
  // 累积泄漏（反复"重连"会不断堆积旧连接）。
  await closeActiveConnection();
  connecting.value = true;
  const msg = ElMessage.info({ message: "正在连接...", duration: 0 });
  try {
    if (kind === "sftp") {
      const id = await openSftpForSession(selectedSessionId.value);
      sftpId.value = id;
      activeKind.value = "sftp";
      connectedName.value =
        sessionsStore.sessions.find((s) => s.id === selectedSessionId.value)?.name ?? "";
      connectedBucket.value = "";
      msg.close();
      ElMessage.success("SFTP 连接已建立");
      // 用 REALPATH 解析会话家目录的绝对路径，之后统一用绝对路径导航；
      // 直接 navigate(".") 会留下相对路径，goUp 会错误地跳到根目录、面包屑
      // 也会把 "." 拼成 "/./..."，与双击进入的目录不一致。
      let root = "/";
      try {
        root = (await sftpPwd(id)) || "/";
      } catch {
        root = "."; // pwd 失败（老服务器不支持 REALPATH 等）回退相对路径，不阻断。
      }
      await remotePane.navigate(root);
    } else {
      const id = await fileConnect(selectedAccountId.value);
      backendId.value = id;
      activeKind.value = "s3";
      const account = accounts.value.find((a) => a.id === selectedAccountId.value);
      connectedName.value = account?.name ?? "";
      connectedAccountId.value = selectedAccountId.value;
      connectedBucket.value = account?.bucket ?? "";
      msg.close();
      ElMessage.success("已连接");
      await remotePane.navigate("");
    }
    localStorage.setItem(LAST_TARGET_KEY, selectedTarget.value);
  } catch (e) {
    msg.close();
    ElMessage.error("连接失败: " + String(e));
    // 半连接状态（句柄已赋值但 navigate 失败等）不视为已连接。
    if (!isConnected.value) await closeActiveConnection();
  } finally {
    connecting.value = false;
  }
}

async function disconnect() {
  if (!isConnected.value) return;
  try {
    await ElMessageBox.confirm("确定断开当前连接？进行中的传输将中止。", "断开连接", {
      type: "warning",
      confirmButtonText: "断开",
      cancelButtonText: "取消",
    });
  } catch {
    return;
  }
  await closeActiveConnection();
  ElMessage.success("已断开连接");
}

/**
 * 关闭当前连接（SFTP 或对象存储）并清空远端栏状态。
 *
 * 无确认弹窗/无成功提示：供 disconnect 确认后调用，也供切换连接与视图卸载时
 * 静默调用（组件可能已销毁，此时不应弹 ElMessage）。
 */
async function closeActiveConnection() {
  if (activeKind.value === "sftp" && sftpId.value) {
    const id = sftpId.value;
    sftpId.value = "";
    try {
      await sftpClose(id);
    } catch (e) {
      // 连接已死时 sftpClose 失败属正常。
      console.warn("关闭 SFTP 连接失败:", e);
    }
  } else if (activeKind.value === "s3" && backendId.value) {
    const id = backendId.value;
    backendId.value = "";
    try {
      await fileDisconnect(id);
    } catch {
      // 忽略：可能已断开。
    }
  }
  activeKind.value = null;
  connectedName.value = "";
  connectedAccountId.value = "";
  connectedBucket.value = "";
  remotePane.reset();
}

// ---------------------------------------------------------------------------
// 账号管理（对象存储）
// ---------------------------------------------------------------------------
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

/** 编辑当前选中的存储账号（选中项已不存在时静默忽略）。 */
function editSelectedAccount() {
  const a = accounts.value.find((x) => "s3:" + x.id === selectedTarget.value);
  if (a) openEditAccount(a);
}

/** 删除当前选中的存储账号。 */
function removeSelectedAccount() {
  const a = accounts.value.find((x) => "s3:" + x.id === selectedTarget.value);
  if (a) void removeAccount(a);
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
    if (activeKind.value === "s3" && connectedAccountId.value === a.id) {
      await closeActiveConnection();
    }
    if ("s3:" + a.id === selectedTarget.value) selectedTarget.value = "";
    await loadAccounts();
  } catch (e) {
    ElMessage.error("删除失败: " + String(e));
  }
}

async function onAccountSaved(_a: FileAccount) {
  await loadAccounts();
}

// ---------------------------------------------------------------------------
// 双栏状态（useFilePane：列表/选中/排序/加载竞态防护）
// ---------------------------------------------------------------------------

// --- 本地栏（plugin-fs） ---
function loadLocal(path: string): Promise<UnifiedEntry[]> {
  return (async () => {
    const raw = await readDir(path);
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
        // 无法 stat 的条目（权限等）用默认值。
      }
      items.push({ name: e.name, isDir: e.isDirectory, size, modified });
    }
    return items;
  })();
}

const localPane = useFilePane(loadLocal, {
  label: "本地",
  joinPath: (p, name) => join(p, name),
  parentOf: async (p) => {
    const parent = await dirname(p);
    return parent && parent !== p ? parent : null;
  },
  crumbsOf: () => [],
});

// --- 远端栏（SFTP / S3 路径语义不同，按「已连接」的类型分发） ---
function fromFileEntry(e: FileEntry): UnifiedEntry {
  return { name: e.name, isDir: e.isDir, size: e.size, modified: e.modified };
}

function remoteList(path: string): Promise<UnifiedEntry[]> {
  const id = activeBackendId();
  if (!id) return Promise.resolve([]);
  const p =
    activeKind.value === "sftp" ? sftpList(id, path) : fileList(id, path);
  return p.then((list) => list.map(fromFileEntry));
}

function remoteJoinPath(p: string, name: string): Promise<string> {
  if (activeKind.value === "s3") {
    // S3：以 `/` 分隔的 bucket 相对路径，根为空串。
    const base = p.replace(/^\/+|\/+$/g, "");
    return Promise.resolve(base ? `${base}/${name}` : name);
  }
  // SFTP：Unix 绝对路径拼接。
  const sep = "/";
  return Promise.resolve(p.endsWith(sep) ? p + name : p + sep + name);
}

function remoteParentOf(p: string): Promise<string | null> {
  if (activeKind.value === "s3") {
    const parts = p.replace(/\/+$/g, "").split("/").filter(Boolean);
    parts.pop();
    return Promise.resolve(parts.join("/")); // 根（""）的上级仍是根，goUp 自然no-op。
  }
  // SFTP 相对路径（pwd 解析失败的回退场景）无法可靠计算父目录，返回 null 保持
  // 不动（旧实现会把 "." 的父目录误算成根目录，从家目录"向上"直接跳到 /）。
  if (!p.startsWith("/") && !p.startsWith("\\")) return Promise.resolve(null);
  const parts = p.split("/").filter(Boolean);
  if (parts.length <= 1) return Promise.resolve("/");
  parts.pop();
  return Promise.resolve("/" + parts.join("/"));
}

function remoteCrumbsOf(p: string) {
  if (activeKind.value === "s3") {
    // 根段 label 为 bucket 名。
    const rootLabel = connectedBucket.value || "根";
    const parts = p.replace(/^\/+|\/+$/g, "").split("/").filter(Boolean);
    const crumbs = [{ name: rootLabel, path: "" }];
    let acc = "";
    for (const part of parts) {
      acc = acc ? `${acc}/${part}` : part;
      crumbs.push({ name: part, path: acc });
    }
    return crumbs;
  }
  // SFTP：兼容 Unix（/）和 Windows（\）分隔符，远程通常用 /。
  if (!p) return [];
  const parts = p.split(/[\\/]/).filter(Boolean);
  const isAbs = p.startsWith("/") || p.startsWith("\\");
  const crumbs = isAbs ? [{ name: "/", path: "/" }] : [];
  let acc = "";
  for (const part of parts) {
    acc = acc ? acc + "/" + part : "/" + part;
    crumbs.push({ name: part, path: acc });
  }
  return crumbs;
}

function remoteNormalizeInput(p: string): string {
  return activeKind.value === "s3" ? p.replace(/^\/+|\/+$/g, "") : p;
}

const remotePane = useFilePane(remoteList, {
  label: "远端",
  joinPath: remoteJoinPath,
  parentOf: remoteParentOf,
  crumbsOf: remoteCrumbsOf,
  normalizeInput: remoteNormalizeInput,
});

// --- 本地目录操作 ---
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
    const full = await join(localPane.path, name);
    await mkdir(full);
    ElMessage.success("已创建目录");
    await localPane.refresh();
  } catch (e) {
    ElMessage.error("创建目录失败: " + String(e));
  }
}

async function joinLocal(name: string): Promise<string> {
  return await join(localPane.path, name);
}

// --- 远端目录操作（SFTP / S3 后端分发） ---
async function remoteMkdir() {
  const id = activeBackendId();
  if (!id) return;
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
  const full = await remoteJoinPath(remotePane.path, name);
  try {
    if (activeKind.value === "sftp") await sftpMkdir(id, full);
    else await fileMkdir(id, full);
    ElMessage.success("已创建目录");
    await remotePane.refresh();
  } catch (e) {
    ElMessage.error("创建目录失败: " + String(e));
  }
}

async function remoteRename(entry: UnifiedEntry) {
  const id = activeBackendId();
  if (!id) return;
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
  const oldFull = await remoteJoinPath(remotePane.path, entry.name);
  const newFull = await remoteJoinPath(remotePane.path, name);
  try {
    if (activeKind.value === "sftp") await sftpRename(id, oldFull, newFull);
    else await fileRename(id, oldFull, newFull);
    ElMessage.success("已重命名");
    await remotePane.refresh();
  } catch (e) {
    ElMessage.error("重命名失败: " + String(e));
  }
}

async function remoteRemove(entry: UnifiedEntry) {
  const id = activeBackendId();
  if (!id) return;
  try {
    await ElMessageBox.confirm(
      entry.isDir
        ? `确定删除远端目录 "${entry.name}" 及其所有内容？`
        : `确定删除远端文件 "${entry.name}"？`,
      "删除",
      { type: "warning", confirmButtonText: "删除", cancelButtonText: "取消" },
    );
  } catch {
    return;
  }
  const full = await remoteJoinPath(remotePane.path, entry.name);
  try {
    if (activeKind.value === "sftp") await sftpRemove(id, full, entry.isDir);
    else await fileRemove(id, full, entry.isDir);
    ElMessage.success("已删除");
    await remotePane.refresh();
  } catch (e) {
    ElMessage.error("删除失败: " + String(e));
  }
}

// ---------------------------------------------------------------------------
// 上传 / 下载（覆盖保护 + 传输队列）
// ---------------------------------------------------------------------------
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

/** 远端目标已存在同名文件时弹确认；不存在或 stat 失败放行（stat 失败由传输命令自身报错）。 */
async function confirmRemoteOverwrite(remoteAbs: string, name: string): Promise<boolean> {
  const id = activeBackendId();
  if (!id) return false;
  try {
    if (activeKind.value === "sftp") await sftpStat(id, remoteAbs);
    else await fileStat(id, remoteAbs);
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
  const id = activeBackendId();
  if (!id) {
    ElMessage.warning("请先连接");
    return;
  }
  const remoteAbs = await remoteJoinPath(remotePane.path, name);
  // 覆盖保护：静默覆盖远端同名文件可能丢失远端数据。
  if (!(await confirmRemoteOverwrite(remoteAbs, name))) return;
  const taskId = crypto.randomUUID();
  transfer.add({
    id: taskId,
    name,
    direction: "upload",
    transferred: 0,
    total: 0,
    status: "pending",
    // 仅 SFTP 后端提供取消命令（sftpTransferCancel）。
    cancellable: activeKind.value === "sftp",
  });
  // 后端命令整体 await（完成才返回），"运行中"须在发起前标记，否则完成后才
  // 更新会覆盖 transfer:done 事件的完成状态。
  transfer.update(taskId, { status: "running" });
  try {
    if (activeKind.value === "sftp") {
      await sftpUpload({ sftpId: id, localPath: localAbs, remotePath: remoteAbs, taskId });
    } else {
      await fileUpload({ backendId: id, localPath: localAbs, remotePath: remoteAbs, taskId });
    }
    ElMessage.success(`上传完成 ${name}`);
  } catch (e) {
    transfer.update(taskId, { status: "error", message: String(e) });
    ElMessage.error("上传失败: " + String(e));
  }
}

/** 下载核心：写入传输队列并按已连接后端发起传输。 */
async function startDownload(remoteAbs: string, savePath: string, name: string, total: number) {
  const id = activeBackendId();
  if (!id) return;
  const taskId = crypto.randomUUID();
  transfer.add({
    id: taskId,
    name,
    direction: "download",
    transferred: 0,
    total,
    status: "pending",
    cancellable: activeKind.value === "sftp",
    source: "sftp",
    localPath: savePath,
  });
  transfer.update(taskId, { status: "running" });
  try {
    if (activeKind.value === "sftp") {
      await sftpDownload({ sftpId: id, remotePath: remoteAbs, localPath: savePath, taskId });
    } else {
      await fileDownload({ backendId: id, remotePath: remoteAbs, localPath: savePath, taskId });
    }
    ElMessage.success(`下载完成 ${name}`);
  } catch (e) {
    transfer.update(taskId, { status: "error", message: String(e) });
    ElMessage.error("下载失败: " + String(e));
  }
}

/**
 * 下载远端文件（按钮/方向键入口）：设置了「默认下载目录」时直接落盘（同名
 * 自动加 (n) 后缀，不弹框），否则用 save 对话框选择保存位置（默认落在本地
 * 当前浏览目录）。
 */
async function downloadEntry(entry: UnifiedEntry) {
  if (!activeBackendId()) return;
  if (entry.isDir) {
    ElMessage.warning("暂不支持目录下载");
    return;
  }
  const remoteAbs = await remoteJoinPath(remotePane.path, entry.name);
  let savePath: string;
  const dir = await usableDownloadDir();
  if (dir) {
    savePath = await uniquePathIn(dir, entry.name);
  } else {
    const defaultSave = await joinLocal(entry.name);
    try {
      const picked = await saveFileDialog({ defaultPath: defaultSave });
      if (!picked) return;
      savePath = picked;
    } catch (e) {
      ElMessage.error("选择保存位置失败: " + String(e));
      return;
    }
  }
  await startDownload(remoteAbs, savePath, entry.name, entry.size || 0);
}

/** 拖拽到本地栏的下载：直接落在本地当前目录，弹覆盖确认。 */
async function downloadToCurrentDir(remoteAbs: string, name: string) {
  const localAbs = await joinLocal(name);
  if (!(await confirmLocalOverwrite(localAbs, name))) return;
  await startDownload(remoteAbs, localAbs, name, 0);
}

/** 方向键「→」：上传本地选中条目；未选中（或是目录）时弹文件选择框。 */
async function uploadSelected() {
  if (!activeBackendId()) {
    ElMessage.warning("请先连接");
    return;
  }
  let localFile = localPane.selected;
  if (!localFile || localFile.isDir) {
    try {
      const picked = await openFileDialog({
        multiple: false,
        directory: false,
        defaultPath: localPane.path || undefined,
      });
      if (!picked) return;
      // picked 是绝对路径，提取文件名。
      const name = await basename(picked);
      await uploadOne(picked, name);
      return;
    } catch (e) {
      ElMessage.error("选择文件失败: " + String(e));
      return;
    }
  }
  const full = await joinLocal(localFile.name);
  await uploadOne(full, localFile.name);
}

/** 方向键「←」：下载远端选中文件。 */
function downloadSelected() {
  const remoteFile = remotePane.selected;
  if (!remoteFile || remoteFile.isDir) {
    ElMessage.warning("请选择一个远端文件（暂不支持目录下载）");
    return;
  }
  void downloadEntry(remoteFile);
}

/** 远端栏工具按钮：弹出本地文件选择（可多选）上传。 */
async function pickAndUpload() {
  if (!activeBackendId()) {
    ElMessage.warning("请先连接");
    return;
  }
  try {
    const picked = await openFileDialog({
      defaultPath: localPane.path,
      multiple: true,
      directory: false,
    });
    const paths = Array.isArray(picked) ? picked : picked ? [picked] : [];
    if (paths.length === 0) return;
    for (const p of paths) {
      const name = await basename(p as string);
      await uploadOne(p as string, name);
    }
    await remotePane.refresh();
  } catch (e) {
    ElMessage.error("上传失败: " + String(e));
  }
}

/** 本地行内「上传」按钮。 */
async function onRowUpload(e: UnifiedEntry) {
  if (e.isDir) return;
  const full = await joinLocal(e.name);
  await uploadOne(full, e.name);
}

// ---------------------------------------------------------------------------
// 面板间拖拽传输（HTML5 DnD）
// ---------------------------------------------------------------------------
interface DragPayload {
  source: "local" | "remote";
  entry: UnifiedEntry;
}
const paneDragOver = ref<"local" | "remote" | null>(null);
let dragPayload: DragPayload | null = null;

function onRowDragStart(source: "local" | "remote", entry: UnifiedEntry) {
  if (entry.isDir) return;
  dragPayload = { source, entry };
}

function onPaneDragOver(target: "local" | "remote", e: DragEvent) {
  // preventDefault 才允许后续 drop 触发。
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
  if (!activeBackendId()) {
    ElMessage.warning("请先连接");
    return;
  }
  const { entry } = dragPayload;
  dragPayload = null;
  if (target === "remote") {
    // 本地 → 远端 = 上传
    const full = await joinLocal(entry.name);
    void uploadOne(full, entry.name);
  } else {
    // 远端 → 本地 = 下载
    const remoteAbs = await remoteJoinPath(remotePane.path, entry.name);
    void downloadToCurrentDir(remoteAbs, entry.name);
  }
}

// ---------------------------------------------------------------------------
// OS 文件拖入（Tauri 原生 onDragDropEvent，拿 OS 文件真实路径）
// ---------------------------------------------------------------------------
const nativeDragOver = ref(false);
let dragDropUnlisten: UnlistenFn | null = null;
let transferDoneUnlisten: UnlistenFn | null = null;
// 组件销毁标志：监听注册未 resolve 前组件可能已卸载，resolve 后据此立即反订阅。
let unmounted = false;

// ---------------------------------------------------------------------------
// 生命周期
// ---------------------------------------------------------------------------
function targetExists(t: string): boolean {
  if (t.startsWith("sftp:")) return sessionsStore.sessions.some((s) => "sftp:" + s.id === t);
  if (t.startsWith("s3:")) return accounts.value.some((a) => "s3:" + a.id === t);
  return false;
}

onMounted(async () => {
  // 两类连接目标并行加载，互不阻断（会话列表失败仍可浏览本地/用存储账号）。
  if (!sessionsStore.loaded) {
    try {
      await sessionsStore.load();
    } catch (e) {
      console.warn("加载会话列表失败:", e);
    }
  }
  await loadAccounts();

  // 恢复上次使用的连接目标；未记录或已失效时默认第一个 SSH 会话，否则第一个账号。
  const last = localStorage.getItem(LAST_TARGET_KEY);
  if (last && targetExists(last)) {
    selectedTarget.value = last;
  } else if (sessionsStore.sessions.length > 0) {
    selectedTarget.value = "sftp:" + sessionsStore.sessions[0].id;
  } else if (accounts.value.length > 0) {
    selectedTarget.value = "s3:" + accounts.value[0].id;
  }

  // 默认进入本地家目录。
  try {
    const home = await homeDir();
    await localPane.navigate(home);
  } catch (e) {
    ElMessage.error("无法读取本地家目录: " + String(e));
  }

  // OS 文件拖入：落点在页面内即上传到远端当前目录（高亮远端栏作提示）。
  try {
    const fn = await getCurrentWebview().onDragDropEvent((e) => {
      if (unmounted) return;
      if (e.payload.type === "enter" || e.payload.type === "over") {
        nativeDragOver.value = true;
      } else {
        nativeDragOver.value = false;
        if (e.payload.type === "drop") {
          if (!activeBackendId()) {
            ElMessage.warning("请先连接");
            return;
          }
          const paths = (e.payload.paths || []) as string[];
          for (const p of paths) {
            const name = p.split(/[\\/]/).pop() || p;
            void uploadOne(p, name);
          }
        }
      }
    });
    if (unmounted) fn();
    else dragDropUnlisten = fn;
  } catch (e) {
    console.error("拖拽事件注册失败:", e);
  }

  // 传输完成后自动刷新对应侧：upload→刷新远端列表，download→刷新本地列表。
  // （完成状态本身由 MainLayout 的全局 transfer:done 订阅写入 store。）
  try {
    const fn = await listen<{ taskId: string }>("transfer:done", (e) => {
      if (unmounted) return;
      const task = transfer.tasks.find((t) => t.id === e.payload.taskId);
      if (!task) return;
      if (task.direction === "upload") void remotePane.refresh();
      else void localPane.refresh();
    });
    if (unmounted) fn();
    else transferDoneUnlisten = fn;
  } catch (e) {
    console.error("传输完成事件订阅失败:", e);
  }
});

onBeforeUnmount(() => {
  unmounted = true;
  if (dragDropUnlisten) dragDropUnlisten();
  if (transferDoneUnlisten) transferDoneUnlisten();
  // 离开页面时静默断开当前连接（SFTP 会话与后端连接不驻留泄漏）。
  void closeActiveConnection();
});

const remoteTitle = computed(() =>
  connectedName.value ? `远端 · ${connectedName.value}` : "远端",
);
const remoteIcon = computed(() => (activeKind.value === "sftp" ? "Connection" : "Files"));
const remotePlaceholder = computed(() =>
  activeKind.value === "sftp" ? "远程绝对路径" : "（bucket 根）",
);
</script>

<template>
  <div class="file-explorer-view">
    <!-- 顶部工具栏：统一连接选择器（SSH 会话 / 对象存储账号分组） -->
    <header class="toolbar">
      <div class="target-picker">
        <span class="picker-label">连接目标</span>
        <el-select
          v-model="selectedTarget"
          placeholder="选择 SSH 会话或存储账号"
          size="default"
          filterable
          class="target-select"
          :disabled="connecting"
        >
          <el-option-group v-if="sessionsStore.sessions.length" label="SSH 会话">
            <el-option
              v-for="s in sessionsStore.sessions"
              :key="'sftp:' + s.id"
              :label="`${s.name} (${s.host}:${s.port})`"
              :value="'sftp:' + s.id"
            />
          </el-option-group>
          <el-option-group v-if="accounts.length" label="对象存储账号">
            <el-option
              v-for="a in accounts"
              :key="'s3:' + a.id"
              :label="`${a.name} (${a.bucket || a.endpoint})`"
              :value="'s3:' + a.id"
            />
          </el-option-group>
        </el-select>
        <!-- 账号管理仅在选择存储账号时有意义（SSH 会话在终端侧管理） -->
        <template v-if="targetKind === 's3'">
          <el-button :icon="'Plus'" @click="openNewAccount">新建账号</el-button>
          <el-button :disabled="!selectedAccountId" @click="editSelectedAccount">
            编辑
          </el-button>
          <el-button
            type="danger"
            plain
            :disabled="!selectedAccountId"
            @click="removeSelectedAccount"
          >
            删除
          </el-button>
        </template>
      </div>

      <div class="toolbar-actions">
        <el-button type="primary" :icon="'Link'" :loading="connecting" @click="connect">
          {{ isConnected ? "重连" : "连接" }}
        </el-button>
        <el-button :disabled="!isConnected" @click="disconnect">断开</el-button>
        <el-tag v-if="isConnected" type="success" size="default" effect="light" class="conn-tag">
          已连接 · {{ connectedName }}
        </el-tag>
      </div>
    </header>

    <!-- 主体：双栏 + 中间传输按钮 -->
    <div class="fe-body">
      <template v-if="isConnected">
        <!-- 本地栏 -->
        <FilePane
          :pane="localPane"
          title="本地"
          icon="FolderOpened"
          path-placeholder="本地路径"
          :drag-over="paneDragOver === 'local'"
          row-upload
          draggable-rows
          @upload="onRowUpload"
          @mkdir="localMkdir"
          @drag-start="onRowDragStart('local', $event)"
          @dnd-over="onPaneDragOver('local', $event)"
          @dnd-leave="onPaneDragLeave"
          @dnd-drop="onPaneDrop('local', $event)"
        />

        <!-- 中间传输操作 -->
        <div class="transfer-arrows">
          <el-tooltip content="上传到远端 (→)" placement="right">
            <el-button circle type="primary" :icon="'Upload'" @click="uploadSelected" />
          </el-tooltip>
          <el-tooltip content="从远端下载 (←)" placement="right">
            <el-button circle :icon="'Download'" @click="downloadSelected" />
          </el-tooltip>
        </div>

        <!-- 远端栏 -->
        <FilePane
          :pane="remotePane"
          :title="remoteTitle"
          :icon="remoteIcon"
          :path-placeholder="remotePlaceholder"
          :drag-over="nativeDragOver || paneDragOver === 'remote'"
          row-download
          row-rename
          row-remove
          draggable-rows
          @download="downloadEntry"
          @rename="remoteRename"
          @remove="remoteRemove"
          @mkdir="remoteMkdir"
          @drag-start="onRowDragStart('remote', $event)"
          @dnd-over="onPaneDragOver('remote', $event)"
          @dnd-leave="onPaneDragLeave"
          @dnd-drop="onPaneDrop('remote', $event)"
        >
          <template #tools>
            <el-tooltip content="选择文件上传" placement="bottom">
              <el-button circle size="small" type="primary" :icon="'Plus'" @click="pickAndUpload" />
            </el-tooltip>
          </template>
        </FilePane>
      </template>

      <!-- 未连接占位 -->
      <EmptyState
        v-else
        class="fe-placeholder"
        icon="Files"
        title="选择 SSH 会话或对象存储账号并连接"
        description="SFTP 服务器文件与 S3 / 兼容存储的上传下载"
        size="lg"
      >
        <el-button type="primary" :icon="'Link'" :loading="connecting" @click="connect">
          连接
        </el-button>
        <el-button :icon="'Plus'" @click="openNewAccount">新建存储账号</el-button>
      </EmptyState>
    </div>

    <!-- 底部传输队列 -->
    <TransferQueue />

    <!-- 存储账号配置弹窗 -->
    <FileAccountDialog
      v-model:visible="accountDialogVisible"
      :account="editingAccount"
      @saved="onAccountSaved"
    />
  </div>
</template>

<style scoped lang="scss">
.file-explorer-view {
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
  gap: 8px 12px;
  padding: 8px 12px;
  background: var(--el-bg-color-overlay);
  border-bottom: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
  flex-wrap: wrap;
}
.target-picker {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.picker-label {
  font-size: 13px;
  color: var(--el-text-color-secondary);
  white-space: nowrap;
}
.target-select {
  width: 300px;
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
.fe-body {
  flex: 1;
  display: flex;
  align-items: stretch;
  gap: 8px;
  padding: 8px;
  min-height: 0;
  overflow: hidden;
}

/* 中间传输按钮 */
.transfer-arrows {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  padding: 0 2px;
  flex-shrink: 0;
}

/* 占位 */
.fe-placeholder {
  flex: 1;
}
</style>
