import { defineStore } from "pinia";
import { ref } from "vue";

/**
 * 文件传输队列。
 *
 * 后端通过 transfer:progress / transfer:done / transfer:error 事件推送进度，
 * 这里维护任务列表的状态。
 */
export interface TransferTask {
  id: string;
  /** 显示名称，通常为文件名。 */
  name: string;
  /** 方向。 */
  direction: "download" | "upload";
  transferred: number;
  total: number;
  status: "pending" | "running" | "done" | "error";
  message?: string;
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
    tasks.value = tasks.value.filter((x) => x.status !== "done" && x.status !== "error");
  }

  return { tasks, add, update, remove, clearDone };
});
