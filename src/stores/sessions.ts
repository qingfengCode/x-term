import { defineStore } from "pinia";
import { computed, ref } from "vue";
import * as sessionApi from "@/api/session";
import type { Group, Session } from "@/api/types";
import { useSettingsStore } from "@/stores/settings";

/**
 * 会话与分组数据（来自 SQLite）。
 *
 * UI 通过本 store 读取/修改，所有变更同步写回后端。
 */
export const useSessionsStore = defineStore("sessions", () => {
  const settings = useSettingsStore();
  const sessions = ref<Session[]>([]);
  const groups = ref<Group[]>([]);
  const loaded = ref(false);

  const tree = computed(() => buildTree(sessions.value, groups.value));
  /** 终端页会话树（仅 SSH/Telnet）。 */
  const terminalTree = computed(() => buildTree(terminalSessions.value, groups.value));

  /** 终端会话（SSH/Telnet）——终端页会话树只显示这些。 */
  const terminalSessions = computed(() =>
    sessions.value.filter((s) => s.protocol === "ssh" || s.protocol === "telnet" || !s.protocol),
  );

  /** 最近成功连接的会话（按记录顺序；只保留仍存在且为 SSH/Telnet 的）。 */
  const recentSessions = computed(() =>
    settings.recentSessionIds
      .map((id) => sessions.value.find((s) => s.id === id))
      .filter(
        (s): s is Session => !!s && (s.protocol === "ssh" || s.protocol === "telnet" || !s.protocol),
      ),
  );

  async function load() {
    [sessions.value, groups.value] = await Promise.all([
      sessionApi.listSessions(),
      sessionApi.listGroups(),
    ]);
    loaded.value = true;
  }

  async function saveSession(s: Session) {
    await sessionApi.saveSession(s);
    const idx = sessions.value.findIndex((x) => x.id === s.id);
    if (idx >= 0) sessions.value[idx] = s;
    else sessions.value.push(s);
  }

  async function removeSession(id: string) {
    await sessionApi.deleteSession(id);
    sessions.value = sessions.value.filter((s) => s.id !== id);
  }

  async function saveGroup(g: Group) {
    await sessionApi.saveGroup(g);
    const idx = groups.value.findIndex((x) => x.id === g.id);
    if (idx >= 0) groups.value[idx] = g;
    else groups.value.push(g);
  }

  async function removeGroup(id: string) {
    await sessionApi.deleteGroup(id);
    groups.value = groups.value.filter((g) => g.id !== id);
  }

  return {
    sessions,
    groups,
    loaded,
    tree,
    terminalSessions,
    terminalTree,
    recentSessions,
    load,
    saveSession,
    removeSession,
    saveGroup,
    removeGroup,
  };
});

// ---------------------------------------------------------------------------
// 树构建
// ---------------------------------------------------------------------------

export interface TreeNode {
  type: "group" | "session";
  id: string;
  label: string;
  raw: Session | Group;
  children?: TreeNode[];
}

function buildTree(sessions: Session[], groups: Group[]): TreeNode[] {
  const groupMap = new Map<string, TreeNode>();
  for (const g of groups) {
    groupMap.set(g.id, {
      type: "group",
      id: g.id,
      label: g.name,
      raw: g,
      children: [],
    });
  }

  const roots: TreeNode[] = [];

  // 先把组挂到父组或根。
  for (const g of groups) {
    const node = groupMap.get(g.id)!;
    if (g.parentId && groupMap.has(g.parentId)) {
      groupMap.get(g.parentId)!.children!.push(node);
    } else {
      roots.push(node);
    }
  }

  // 再把会话挂到对应组或根。
  for (const s of sessions) {
    const node: TreeNode = {
      type: "session",
      id: s.id,
      label: s.name,
      raw: s,
    };
    if (s.groupId && groupMap.has(s.groupId)) {
      groupMap.get(s.groupId)!.children!.push(node);
    } else {
      roots.push(node);
    }
  }

  // 简单排序：组在前、会话在后。
  const sortRec = (nodes: TreeNode[]) => {
    nodes.sort((a, b) => {
      if (a.type !== b.type) return a.type === "group" ? -1 : 1;
      return a.label.localeCompare(b.label);
    });
    for (const n of nodes) if (n.children) sortRec(n.children);
  };
  sortRec(roots);

  return roots;
}
