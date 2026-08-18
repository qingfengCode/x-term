import { defineStore } from "pinia";
import { computed, reactive, ref } from "vue";
import * as dbApi from "@/api/db";
import type { DbProfile } from "@/api/types";

/**
 * MySQL 连接的多标签全局状态。
 *
 * 与 [`useTerminalsStore`] 对称：每个标签 = 一个独立的后端连接（connId），
 * 标签绑定一个库（schema），SQL 控制台每个标签一个执行界面。
 * 活动标签的连接 id / 库暴露给 AI 智能体面板（启用 SQL 工具集 + 注入上下文）。
 */
export interface DbTab {
  /** 前端标签 id。 */
  id: string;
  /** 后端连接 id（连接中为 null）。 */
  connId: string | null;
  /** 对应的 profile id。 */
  profileId: string;
  /** profile 展示名（标签标题兜底用）。 */
  profileName: string;
  /**
   * 标签绑定的库（schema）。由用户在左侧表树点击库节点时自动 USE 并绑定；
   * `null` 表示未绑定（走连接 URL 的默认库）。标签内也可通过 `USE xxx`
   * 手动切换（后端拦截更新，前端同步本字段）。
   */
  database: string | null;
  /** 是否正在连接中。 */
  connecting: boolean;
  /** 最近一次连接错误。 */
  error: string | null;
}

export const useDbStore = defineStore("db", () => {
  const tabs = ref<DbTab[]>([]);
  const activeTabId = ref<string | null>(null);

  const activeTab = computed<DbTab | null>(
    () => tabs.value.find((t) => t.id === activeTabId.value) ?? null,
  );
  /** 活动连接的 id（供 AiPanel 读取以决定 SQL 工具是否启用）。 */
  const activeConnId = computed<string | null>(() => activeTab.value?.connId ?? null);
  /** 活动标签绑定的库（供 AiPanel 显示上下文 / 注入 system prompt）。 */
  const activeDatabase = computed<string | null>(() => activeTab.value?.database ?? null);

  /** 进行中的连接 promise：key=`${profileId}|${database ?? ""}`，并发打开同一标签时复用。 */
  const opening = new Map<string, Promise<string>>();

  function genId() {
    return `dbtab-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
  }

  /** 找已打开的标签：同 profile + 同库；database 为空时先精确匹配再回落到同 profile 任一标签。 */
  function findTab(profileId: string, database: string | null): DbTab | null {
    const exact = tabs.value.find(
      (t) => t.profileId === profileId && (t.database ?? null) === database,
    );
    if (exact) return exact;
    if (database === null) return tabs.value.find((t) => t.profileId === profileId) ?? null;
    return null;
  }

  /**
   * 打开（或激活）一个 DB 标签。
   *
   * @param profile 连接的 profile 配置
   * @param database 可选：绑定库（连接成功后自动 USE，如点左侧库节点）
   * @returns 标签 id；连接失败时移除占位标签并抛出错误（调用方弹提示）
   */
  async function openTab(profile: DbProfile, database: string | null = null): Promise<string> {
    const existing = findTab(profile.id, database);
    if (existing && existing.connId) {
      activeTabId.value = existing.id;
      return existing.id;
    }
    // 同 key 的打开动作在途（并发点击/树加载）→ 复用它，避免重复建标签；
    // 已有占位标签先激活，否则连接完成时活动标签可能仍是别的标签。
    const key = `${profile.id}|${database ?? ""}`;
    if (opening.has(key)) {
      if (existing) activeTabId.value = existing.id;
      return opening.get(key)!;
    }

    const p = doOpen(profile, database);
    opening.set(key, p);
    try {
      return await p;
    } finally {
      opening.delete(key);
    }
  }

  async function doOpen(profile: DbProfile, database: string | null): Promise<string> {
    // reactive 化后再 push（与 terminals store 同理：raw 对象的属性赋值
    // 不触发响应，tab 的 connId/connecting/error 更新会滞留）。
    const tab = reactive<DbTab>({
      id: genId(),
      connId: null,
      profileId: profile.id,
      profileName: profile.name,
      database,
      connecting: true,
      error: null,
    });
    tabs.value.push(tab);
    activeTabId.value = tab.id;

    try {
      const connId = await dbApi.dbConnect(profile.id);
      // 连接期间标签可能已被用户关闭（此时 connId 仍为 null，close 会移除占位）。
      if (!tabs.value.includes(tab)) {
        try {
          await dbApi.dbDisconnect(connId);
        } catch {
          /* 忽略清理错误 */
        }
        throw new Error("连接已取消（标签已关闭）");
      }
      tab.connId = connId;
      // 绑定库：自动 USE，之后该标签的 SQL 无需带库前缀。
      if (database) {
        await dbApi.dbUseDatabase(connId, database);
      }
    } catch (e) {
      tab.error = String(e);
      const idx = tabs.value.indexOf(tab);
      if (idx >= 0) tabs.value.splice(idx, 1);
      // 连接已建立但绑定库（自动 USE）失败：必须断开后端连接，否则连接
      // 留在 mysql_conns 里泄漏（反复点一个无权限的库会把连接池撑满）。
      if (tab.connId) {
        try {
          await dbApi.dbDisconnect(tab.connId);
        } catch {
          /* 忽略清理错误 */
        }
      }
      if (activeTabId.value === tab.id) {
        activeTabId.value = tabs.value[0]?.id ?? null;
      }
      throw e;
    } finally {
      tab.connecting = false;
    }
    return tab.id;
  }

  /** 关闭标签（断开后端连接）。 */
  async function closeTab(tabId: string) {
    const idx = tabs.value.findIndex((t) => t.id === tabId);
    if (idx < 0) return;
    const [removed] = tabs.value.splice(idx, 1);
    if (removed.connId) {
      try {
        await dbApi.dbDisconnect(removed.connId);
      } catch {
        /* 忽略关闭错误 */
      }
    }
    if (activeTabId.value === tabId) {
      activeTabId.value = tabs.value[idx]?.id ?? tabs.value[idx - 1]?.id ?? null;
    }
  }

  function setActive(tabId: string) {
    if (tabs.value.some((t) => t.id === tabId)) activeTabId.value = tabId;
  }

  /** 把某标签的当前库切到指定库（后端同步 USE，供"当前标签切换库"用）。 */
  async function useDatabase(tabId: string, database: string | null) {
    const tab = tabs.value.find((t) => t.id === tabId);
    if (!tab || !tab.connId) return;
    await dbApi.dbUseDatabase(tab.connId, database);
    tab.database = database;
  }

  /** 仅清空本地状态（后端已断开时同步）。 */
  function clear() {
    tabs.value = [];
    activeTabId.value = null;
  }

  /** 移动标签（拖拽排序）：把 fromId 移到 toId 之前/之后。 */
  function moveTab(fromId: string, toId: string, before: boolean) {
    const from = tabs.value.findIndex((t) => t.id === fromId);
    const to = tabs.value.findIndex((t) => t.id === toId);
    if (from < 0 || to < 0 || from === to) return;
    const [tab] = tabs.value.splice(from, 1);
    let idx = tabs.value.findIndex((t) => t.id === toId);
    if (!before) idx += 1;
    tabs.value.splice(idx, 0, tab);
  }

  return {
    tabs,
    activeTabId,
    activeTab,
    activeConnId,
    activeDatabase,
    openTab,
    closeTab,
    setActive,
    useDatabase,
    clear,
    moveTab,
  };
});
