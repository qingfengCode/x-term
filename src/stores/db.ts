import { defineStore } from "pinia";
import { computed, ref } from "vue";
import * as dbApi from "@/api/db";

/**
 * MySQL 连接的全局状态。
 *
 * 与 [`useTerminalsStore`] 对称：把当前活动连接 id 暴露给 AI 智能体面板，
 * 让 agent 模式知道"有可用数据库上下文"，从而启用 SQL 工具集。
 *
 * 目前只跟踪**单个活动连接**（与 SqlConsoleView 单视图单连接的现状一致）。
 * 后续若支持多连接 tab，可扩展为 `conns: DbConn[]` + `activeConnId`。
 */
export interface DbConn {
  /** 后端返回的连接 id。 */
  id: string;
  /** 对应的 profile id。 */
  profileId: string;
  /** 展示名（取自 profile，便于 UI/AI 提示）。 */
  name: string;
  /**
   * 当前关联的库名（schema）。由用户在 SQL 控制台表树点击/展开库或表、
   * 或把表拖入 AI 面板时设置；供 AI 助手显示上下文并注入 system prompt，
   * 让 agent 默认在该库下操作。`null` 表示未选定具体库。
   */
  activeDatabase: string | null;
}

export const useDbStore = defineStore("db", () => {
  /** 已建立的连接列表（当前实际只有 0 或 1 个）。 */
  const conns = ref<DbConn[]>([]);
  /** 活动连接 id（供 AiPanel 读取以决定 SQL 工具是否启用）。 */
  const activeConnId = computed<string | null>(
    () => conns.value[0]?.id ?? null,
  );
  /** 当前关联的库名（供 AiPanel 显示上下文 / 注入 system prompt）。 */
  const activeDatabase = computed<string | null>(
    () => conns.value[0]?.activeDatabase ?? null,
  );

  /** 建立连接并登记。返回 connId。 */
  async function connect(profileId: string, name: string): Promise<string> {
    const id = await dbApi.dbConnect(profileId);
    conns.value = [{ id, profileId, name, activeDatabase: null }];
    return id;
  }

  /**
   * 设置当前活动连接的关联库。用户在表树点击/展开库或表、或把表拖入 AI 面板时调用。
   * 传 `null` 表示清除关联（断开连接时）。
   */
  function setActiveDatabase(name: string | null) {
    const c = conns.value[0];
    if (c) c.activeDatabase = name;
  }

  /** 断开活动连接并清空。 */
  async function disconnect(): Promise<void> {
    const c = conns.value[0];
    if (!c) return;
    conns.value = [];
    try {
      await dbApi.dbDisconnect(c.id);
    } catch {
      /* 忽略关闭错误 */
    }
  }

  /** 仅清空本地状态（不调用后端），用于后端已断开时的同步。 */
  function clear() {
    conns.value = [];
  }

  return { conns, activeConnId, activeDatabase, connect, disconnect, clear, setActiveDatabase };
});
