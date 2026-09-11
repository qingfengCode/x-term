import { defineStore } from "pinia";
import { ref, watch } from "vue";

/**
 * 文件传输队列。
 *
 * 后端通过 transfer:progress / transfer:done / transfer:error 事件推送进度，
 * 这里维护任务列表的状态。
 *
 * 除 SFTP 任务外，ZMODEM（sz）下载也记录在此（source="zmodem"）：
 * 下载记录持久化到 localStorage（最近 100 条已结束记录，重启后仍可在框架
 * 顶部的下载列表中查看）；SFTP 任务为会话内存态，维持原有行为。
 */
export interface TransferTask {
  id: string;
  /** 显示名称，通常为文件名。 */
  name: string;
  /** 方向。 */
  direction: "download" | "upload";
  transferred: number;
  total: number;
  status: "pending" | "running" | "done" | "error" | "cancelled";
  message?: string;
  /** 后端是否支持取消（SFTP 传输 true；S3 等暂不支持取消的不显示按钮）。 */
  cancellable?: boolean;
  /** 任务来源：SFTP 传输事件 / ZMODEM 终端下载。 */
  source?: "sftp" | "zmodem";
  /** 本地保存路径（ZMODEM 下载记录用；SFTP 下载也可补充）。 */
  localPath?: string;
  /** 结束时刻（ms 时间戳）。 */
  endedAt?: number;
}

/** ZMODEM 下载记录的 localStorage 键。 */
const ZMODEM_HISTORY_KEY = "xterm-zmodem-history";
/** 历史记录上限（超出丢弃最旧）。 */
const ZMODEM_HISTORY_MAX = 100;

/** 已结束状态判定（持久化与"清除已完成"共用）。 */
function isEnded(t: TransferTask): boolean {
  return t.status === "done" || t.status === "error" || t.status === "cancelled";
}

export const useTransferStore = defineStore("transfer", () => {
  const tasks = ref<TransferTask[]>([]);

  function add(task: TransferTask) {
    tasks.value.push(task);
  }

  function update(id: string, patch: Partial<TransferTask>) {
    const t = tasks.value.find((x) => x.id === id);
    if (t) Object.assign(t, patch);
  }

  function remove(id: string) {
    tasks.value = tasks.value.filter((x) => x.id !== id);
  }

  function clearDone() {
    tasks.value = tasks.value.filter((x) => !isEnded(x));
  }

  // --- ZMODEM 下载记录持久化 ---------------------------------------------------
  // 只持久化 source="zmodem" 的已结束任务；进行中的不落盘（重启即失效，
  // 避免恢复出永远 running 的死记录）。加载失败（旧格式/隐私模式）静默忽略。
  function loadZmodemHistory() {
    try {
      const raw = localStorage.getItem(ZMODEM_HISTORY_KEY);
      if (!raw) return;
      const list = JSON.parse(raw) as TransferTask[];
      if (!Array.isArray(list)) return;
      // 新记录在后（push 顺序），展示层倒序——这里按时间正序恢复。
      for (const t of list) {
        if (t && typeof t.id === "string" && isEnded(t)) tasks.value.push(t);
      }
    } catch {
      // 忽略损坏的历史数据
    }
  }
  loadZmodemHistory();

  // 防抖持久化：ZMODEM 下载的 on_input 每个数据块都 update 进度，watch 若
  // 每次 filter+stringify+setItem 同步执行，高速下载时（每秒数百次事件帧）
  // 会持续占用主线程造成 UI 卡顿。1s 尾随防抖把落盘压到空闲期最多一次；
  // 任务的最终状态在最后一次变更后 1s 落盘（重启恢复只关心已结束任务，
  // 进行中的进度本来就不持久化）。
  let persistTimer: ReturnType<typeof setTimeout> | null = null;
  watch(
    tasks,
    () => {
      if (persistTimer) clearTimeout(persistTimer);
      persistTimer = setTimeout(() => {
        persistTimer = null;
        try {
          const history = tasks.value
            .filter((t) => t.source === "zmodem" && isEnded(t))
            .slice(-ZMODEM_HISTORY_MAX);
          localStorage.setItem(ZMODEM_HISTORY_KEY, JSON.stringify(history));
        } catch {
          // localStorage 不可用不影响功能
        }
      }, 1000);
    },
    { deep: true },
  );

  return { tasks, add, update, remove, clearDone };
});
