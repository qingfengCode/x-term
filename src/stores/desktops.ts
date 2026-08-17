import { defineStore } from "pinia";
import { computed, ref } from "vue";
import * as desktopApi from "@/api/remote_desktop";
import type { Desktop, DesktopGroup } from "@/api/remote_desktop";

/** 桌面树节点（分组或桌面连接），与 sessions store 的 TreeNode 同构。 */
export interface DesktopTreeNode {
  type: "group" | "desktop";
  id: string;
  label: string;
  raw: DesktopGroup | Desktop;
  children?: DesktopTreeNode[];
}

/** 建树：分组按 parentId 挂层级，桌面按 groupId 挂载，组在前、再按名称排序。 */
function buildTree(desktops: Desktop[], groups: DesktopGroup[]): DesktopTreeNode[] {
  const groupMap = new Map<string, DesktopTreeNode>();
  for (const g of groups) {
    groupMap.set(g.id, { type: "group", id: g.id, label: g.name, raw: g, children: [] });
  }
  const roots: DesktopTreeNode[] = [];
  for (const g of groups) {
    const node = groupMap.get(g.id)!;
    if (g.parentId && groupMap.has(g.parentId)) {
      groupMap.get(g.parentId)!.children!.push(node);
    } else {
      roots.push(node);
    }
  }
  for (const d of desktops) {
    const node: DesktopTreeNode = { type: "desktop", id: d.id, label: d.name, raw: d };
    if (d.groupId && groupMap.has(d.groupId)) {
      groupMap.get(d.groupId)!.children!.push(node);
    } else {
      roots.push(node);
    }
  }
  const sortRec = (nodes: DesktopTreeNode[]) => {
    nodes.sort((a, b) => {
      if (a.type !== b.type) return a.type === "group" ? -1 : 1;
      return a.label.localeCompare(b.label);
    });
    for (const n of nodes) if (n.children) sortRec(n.children);
  };
  sortRec(roots);
  return roots;
}

/**
 * 桌面连接（RDP/VNC）store。独立于终端 sessions store。
 * 数据（连接 + 分组）在此管理；已打开的桌面会话标签在 desktopTabs store。
 */
export const useDesktopsStore = defineStore("desktops", () => {
  const desktops = ref<Desktop[]>([]);
  const groups = ref<DesktopGroup[]>([]);
  const loaded = ref(false);

  /** 桌面树（左侧树组件数据源）。 */
  const tree = computed(() => buildTree(desktops.value, groups.value));

  async function load() {
    [desktops.value, groups.value] = await Promise.all([
      desktopApi.desktopList(),
      desktopApi.desktopGroupList(),
    ]);
    loaded.value = true;
  }

  async function save(desktop: Desktop) {
    await desktopApi.desktopSave(desktop);
    const idx = desktops.value.findIndex((d) => d.id === desktop.id);
    if (idx >= 0) desktops.value[idx] = desktop;
    else desktops.value.push(desktop);
  }

  /** 更新内存中的 RDP 分辨率记忆（持久化走 desktop_save_size，成功后调用）。 */
  function patchSize(id: string, size: string | null) {
    const d = desktops.value.find((x) => x.id === id);
    if (d) d.desktopSize = size;
  }

  async function remove(id: string) {
    await desktopApi.desktopDelete(id);
    desktops.value = desktops.value.filter((d) => d.id !== id);
  }

  async function saveGroup(g: DesktopGroup) {
    await desktopApi.desktopGroupSave(g);
    const idx = groups.value.findIndex((x) => x.id === g.id);
    if (idx >= 0) groups.value[idx] = g;
    else groups.value.push(g);
  }

  async function removeGroup(id: string) {
    await desktopApi.desktopGroupDelete(id);
    // 后端删除分组不级联，需清理悬空引用（与 sessions 一致）：
    // - 组内桌面的 groupId 置空（变未分组）；
    // - 子分组的 parentId 置空（变根分组）。
    for (const d of desktops.value.filter((d) => d.groupId === id)) {
      await save({ ...d, groupId: null });
    }
    for (const g of groups.value.filter((g) => g.parentId === id)) {
      await saveGroup({ ...g, parentId: null });
    }
    groups.value = groups.value.filter((g) => g.id !== id);
  }

  return { desktops, groups, tree, loaded, load, save, remove, saveGroup, removeGroup, patchSize };
});
