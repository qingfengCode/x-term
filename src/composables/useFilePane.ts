import { computed, reactive, ref } from "vue";
import { ElMessage } from "element-plus";

/**
 * useFilePane — 文件传输页单栏状态（本地 / 远程共用）
 *
 * 原SftpView / FileExplorerView 各自维护一份「条目列表 + 选中 + 排序 + 加载
 * 竞态防护」的状态机，合并文件页时抽到这里。路径语义（拼接 / 上级 / 面包屑）
 * 因栏而异——本地走 tauri path API（async），SFTP 远程用 Unix 绝对路径，S3
 * 远端用 `/` 分隔的 bucket 相对路径——由调用方通过 UseFilePaneOptions 注入。
 */

/** 统一的目录条目（本地 / 远程共用）。 */
export interface UnifiedEntry {
  name: string;
  isDir: boolean;
  size: number;
  modified: string | null;
}

export type FileSortKey = "name" | "size" | "modified";

export interface FileCrumb {
  name: string;
  path: string;
}

export interface UseFilePaneOptions {
  /** 错误提示中的栏位名（「读取本地目录失败」）。 */
  label: string;
  /** path + name → 子路径。本地为 tauri join（async），远端按分隔符拼接。 */
  joinPath: (path: string, name: string) => Promise<string>;
  /**
   * 计算父目录；返回 null 表示无法上溯（SFTP 相对路径回退场景），
   * goUp 保持不动。
   */
  parentOf: (path: string) => Promise<string | null>;
  /** 面包屑段；无面包屑的栏（本地）返回 []。 */
  crumbsOf: (path: string) => FileCrumb[];
  /** 路径输入框回车前的规范化（S3 需去首尾 /），缺省原样。 */
  normalizeInput?: (path: string) => string;
}

export function useFilePane(
  loader: (path: string) => Promise<UnifiedEntry[]>,
  opts: UseFilePaneOptions,
) {
  const path = ref("");
  const entries = ref<UnifiedEntry[]>([]);
  const loading = ref(false);
  const selected = ref<UnifiedEntry | null>(null);
  const sortKey = ref<FileSortKey>("name");
  const sortDesc = ref(false);
  // 是否已成功加载过（S3 根目录 path 为 ""，不能用 path 判空来决定能否刷新）。
  let navigated = false;

  // 加载竞态防护：每次加载取递增序号，响应返回时若序号已过期（期间又发起了
  // 新加载）则丢弃结果，避免先发的慢请求覆盖新导航的列表/loading 状态。
  let loadSeq = 0;

  /** 列出目录并更新状态；成功返回 true（连接流程据此判断是否回退）。 */
  async function navigate(p: string): Promise<boolean> {
    const seq = ++loadSeq;
    loading.value = true;
    try {
      const items = await loader(p);
      if (seq !== loadSeq) return false; // 过期响应丢弃（快速双击/刷新并发时）。
      entries.value = items;
      path.value = p;
      navigated = true;
      // 目录切换/刷新后清空选中：残留高亮可能指向已不存在的同名文件。
      selected.value = null;
      return true;
    } catch (e) {
      if (seq !== loadSeq) return false;
      ElMessage.error(`读取${opts.label}目录失败: ` + String(e));
      return false;
    } finally {
      if (seq === loadSeq) loading.value = false;
    }
  }

  async function refresh() {
    if (navigated) await navigate(path.value);
  }

  /** 双击条目：目录进入，文件仅选中。 */
  async function enter(entry: UnifiedEntry) {
    if (!entry.isDir) {
      selected.value = entry;
      return;
    }
    await navigate(await opts.joinPath(path.value, entry.name));
  }

  async function goUp() {
    if (!path.value) return;
    const parent = await opts.parentOf(path.value);
    if (parent && parent !== path.value) await navigate(parent);
  }

  async function goToInput() {
    const p = opts.normalizeInput
      ? opts.normalizeInput(path.value.trim())
      : path.value.trim();
    if (p) await navigate(p);
  }

  function toggleSort(k: FileSortKey) {
    if (sortKey.value === k) sortDesc.value = !sortDesc.value;
    else {
      sortKey.value = k;
      sortDesc.value = false;
    }
  }

  /** 断开连接时清空列表与选中（path 由下次连接的 navigate 重置）。 */
  function reset() {
    entries.value = [];
    selected.value = null;
    navigated = false;
  }

  const crumbs = computed(() => opts.crumbsOf(path.value));

  /** 排序视图：目录永远靠前，文件/目录各自按 sortKey 比较，desc 控制升降序。 */
  const sorted = computed(() => {
    const k = sortKey.value;
    const sign = sortDesc.value ? -1 : 1;
    return [...entries.value].sort((a, b) => {
      if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
      let cmp = 0;
      if (k === "name") {
        cmp = a.name.localeCompare(b.name, undefined, { numeric: true });
      } else if (k === "size") {
        cmp = (a.size || 0) - (b.size || 0);
      } else {
        // modified：null 视为最早。
        const ta = a.modified ? new Date(a.modified).getTime() : 0;
        const tb = b.modified ? new Date(b.modified).getTime() : 0;
        cmp = ta - tb;
      }
      return cmp * sign;
    });
  });

  // reactive 包装：模板与父组件直接 pane.path / pane.sorted 访问，无需 .value。
  return reactive({
    path,
    entries,
    loading,
    selected,
    sortKey,
    sortDesc,
    sorted,
    crumbs,
    navigate,
    refresh,
    enter,
    goUp,
    goToInput,
    toggleSort,
    reset,
  });
}

/** useFilePane 返回值类型（FilePane 组件的 pane prop）。 */
export type FilePaneState = ReturnType<typeof useFilePane>;
