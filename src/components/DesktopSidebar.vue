<!--
  DesktopSidebar.vue — 桌面页左侧列表树（分组 + 桌面连接）。

  交互镜像 SessionSidebar：分组树 + 搜索过滤 + 悬停下拉菜单 + 拖拽归属分组。
  数据来自 desktops store（连接 + 分组），已打开的桌面用绿点标识；
  连接/编辑/删除动作 emit 给 RemoteDesktopView（凭据读取与确认逻辑在视图内）。
-->
<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { Connection, Folder, Monitor, MoreFilled } from "@element-plus/icons-vue";
import { useDesktopsStore, type DesktopTreeNode } from "@/stores/desktops";
import { useDesktopTabsStore } from "@/stores/desktopTabs";
import type { Desktop, DesktopGroup } from "@/api/remote_desktop";

const desktopsStore = useDesktopsStore();
const desktopTabs = useDesktopTabsStore();

const emit = defineEmits<{
  (e: "connect", d: Desktop): void;
  (e: "edit", d: Desktop): void;
  (e: "remove", d: Desktop): void;
  /** 新建桌面连接（groupId 为来源分组的预选项，null = 无分组）。 */
  (e: "new", groupId: string | null): void;
}>();

// --- 过滤 ---------------------------------------------------------------
const filter = ref("");
const filterText = computed(() => filter.value.trim().toLowerCase());

const treeRef = ref();
const treeProps = { label: "label", children: "children" };

// el-tree 通过 ref.filter 调用触发 filter-node-method。
watch(filterText, (v) => treeRef.value?.filter(v));

function filterNode(value: string, data: DesktopTreeNode) {
  if (!value) return true;
  if (data.type === "desktop") {
    // 桌面节点：名称或主机匹配。
    const d = data.raw as Desktop;
    return (
      data.label.toLowerCase().includes(value) || d.host.toLowerCase().includes(value)
    );
  }
  // 分组：本身匹配（el-tree 默认会保留包含匹配子节点的父节点）。
  return data.label.toLowerCase().includes(value);
}

/** 把标签按过滤关键词切成片段，用于匹配子串高亮（大小写不敏感）。 */
function highlightParts(label: string): { text: string; hit: boolean }[] {
  const kw = filterText.value;
  if (!kw) return [{ text: label, hit: false }];
  const parts: { text: string; hit: boolean }[] = [];
  const lower = label.toLowerCase();
  let i = 0;
  while (i < label.length) {
    const idx = lower.indexOf(kw, i);
    if (idx < 0) {
      parts.push({ text: label.slice(i), hit: false });
      break;
    }
    if (idx > i) parts.push({ text: label.slice(i, idx), hit: false });
    parts.push({ text: label.slice(idx, idx + kw.length), hit: true });
    i = idx + kw.length;
  }
  return parts;
}

/** 已打开标签的桌面 id 集合（树节点显示绿点）。 */
const openIds = computed(() => new Set(desktopTabs.tabs.map((t) => t.desktopId)));

/** 节点悬浮提示：桌面显示主机信息，分组显示名称。 */
function nodeTitle(data: DesktopTreeNode): string {
  if (data.type === "desktop") {
    const d = data.raw as Desktop;
    return `${d.host}:${d.port} · ${d.username || "—"}`;
  }
  return data.label;
}

const isEmpty = computed(
  () => desktopsStore.desktops.length === 0 && desktopsStore.groups.length === 0
);

// --- 点击 / 交互 --------------------------------------------------------
// 单击桌面节点即连接（与终端页会话树一致）；分组节点单击切换展开/折叠。
function onNodeClick(data: DesktopTreeNode) {
  if (data.type === "desktop") emit("connect", data.raw as Desktop);
  else treeRef.value?.toggleExpand(data.id);
}

// --- 节点菜单 -----------------------------------------------------------
function onCommand(cmd: string, data: DesktopTreeNode) {
  if (data.type === "desktop") {
    const d = data.raw as Desktop;
    switch (cmd) {
      case "connect":
        emit("connect", d);
        break;
      case "edit":
        emit("edit", d);
        break;
      case "delete":
        emit("remove", d);
        break;
    }
  } else {
    const g = data.raw as DesktopGroup;
    switch (cmd) {
      case "newChild":
        emit("new", g.id);
        break;
      case "rename":
        renameGroup(g);
        break;
      case "delete":
        confirmDeleteGroup(g);
        break;
    }
  }
}

async function confirmDeleteGroup(g: DesktopGroup) {
  const hasChild = desktopsStore.desktops.some((d) => d.groupId === g.id);
  try {
    await ElMessageBox.confirm(
      hasChild
        ? `分组 "${g.name}" 下仍有桌面连接，删除分组后连接将变为无分组。继续？`
        : `确定删除分组 "${g.name}" 吗？`,
      "删除分组",
      { type: "warning", confirmButtonText: "删除", cancelButtonText: "取消" }
    );
  } catch {
    return;
  }
  try {
    await desktopsStore.removeGroup(g.id);
    ElMessage.success("已删除");
  } catch (e) {
    ElMessage.error("删除失败: " + String(e));
  }
}

async function createRootGroup() {
  let name: string;
  try {
    const res = await ElMessageBox.prompt("请输入分组名称", "新建分组", {
      confirmButtonText: "创建",
      cancelButtonText: "取消",
      inputValidator: (v) => !!v?.trim() || "名称不能为空",
    });
    name = res.value;
  } catch {
    return;
  }
  const now = new Date().toISOString();
  try {
    await desktopsStore.saveGroup({
      id: crypto.randomUUID(),
      name: name.trim(),
      parentId: null,
      sortOrder: 0,
      createdAt: now,
    });
    ElMessage.success("已创建分组");
  } catch (e) {
    ElMessage.error("创建分组失败: " + String(e));
  }
}

async function renameGroup(g: DesktopGroup) {
  let name: string;
  try {
    const res = await ElMessageBox.prompt("请输入新的分组名称", "重命名分组", {
      inputValue: g.name,
      confirmButtonText: "保存",
      cancelButtonText: "取消",
      inputValidator: (v) => !!v?.trim() || "名称不能为空",
    });
    name = res.value;
  } catch {
    return;
  }
  try {
    await desktopsStore.saveGroup({ ...g, name: name.trim() });
    ElMessage.success("已重命名");
  } catch (e) {
    ElMessage.error("重命名失败: " + String(e));
  }
}

// 默认展开所有分组。
const defaultExpandedKeys = computed(() => desktopsStore.groups.map((g) => g.id));

// --- 拖拽归属分组 ---------------------------------------------------------
// 只允许桌面节点被拖拽，分组不可拖。
function allowDrag(node: { data: DesktopTreeNode }) {
  return node.data.type === "desktop";
}

// 放置规则：inner 只能放到分组上；prev/next 允许（用于调整归属）。
function allowDrop(
  _draggingNode: { data: DesktopTreeNode },
  dropNode: { data: DesktopTreeNode },
  type: "prev" | "inner" | "next"
) {
  if (type === "inner") {
    return dropNode.data.type === "group";
  }
  // prev / next：不允许放在分组正旁边（避免歧义），只允许放在桌面节点旁。
  return dropNode.data.type === "desktop";
}

// 拖拽完成后持久化 groupId 变更。
async function onNodeDrop(
  draggingNode: { data: DesktopTreeNode },
  dropNode: { data: DesktopTreeNode },
  dropType: "prev" | "inner" | "next"
) {
  const desktop = draggingNode.data.raw as Desktop;
  let newGroupId: string | null = null;

  if (dropType === "inner") {
    // 放入分组内部。
    newGroupId = dropNode.data.id;
  } else {
    // prev / next：取目标桌面的 groupId（可能为 null = 根级）。
    const target = dropNode.data.raw as Desktop;
    newGroupId = target.groupId ?? null;
  }

  if (newGroupId === desktop.groupId) return; // 无变化

  try {
    await desktopsStore.save({ ...desktop, groupId: newGroupId });
    const groupName = newGroupId
      ? desktopsStore.groups.find((g) => g.id === newGroupId)?.name
      : null;
    ElMessage.success(groupName ? `已移动到分组 "${groupName}"` : "已移至未分组");
  } catch (e) {
    ElMessage.error("移动失败: " + String(e));
    // 恢复树状态。
    await desktopsStore.load();
  }
}
</script>

<template>
  <aside class="desktop-sidebar">
    <!-- 顶部标题 + 操作 -->
    <header class="sidebar-header">
      <span class="title">桌面</span>
      <div class="actions">
        <el-tooltip content="新建桌面连接" placement="bottom">
          <el-button circle size="small" :icon="'Plus'" @click="emit('new', null)" />
        </el-tooltip>
        <el-tooltip content="新建分组" placement="bottom">
          <el-button circle size="small" :icon="'FolderAdd'" @click="createRootGroup" />
        </el-tooltip>
      </div>
    </header>

    <!-- 搜索 -->
    <div class="search-wrap">
      <el-input
        v-model="filter"
        placeholder="搜索桌面连接"
        clearable
        :prefix-icon="'Search'"
        size="small"
      />
    </div>

    <!-- 树 -->
    <div class="tree-wrap">
      <el-tree
        v-if="!isEmpty"
        ref="treeRef"
        :data="desktopsStore.tree"
        :props="treeProps"
        node-key="id"
        :default-expanded-keys="defaultExpandedKeys"
        :filter-node-method="filterNode"
        :expand-on-click-node="false"
        :highlight-current="true"
        draggable
        :allow-drag="allowDrag"
        :allow-drop="allowDrop"
        @node-click="onNodeClick"
        @node-drop="onNodeDrop"
      >
        <template #default="{ data }">
          <div class="tree-node" :class="{ 'is-group': data.type === 'group' }">
            <span class="node-label" :title="nodeTitle(data)">
              <span
                v-if="data.type === 'desktop' && openIds.has(data.id)"
                class="status-dot"
                title="已打开"
              />
              <el-icon v-if="data.type === 'group'" class="node-icon"><Folder /></el-icon>
              <el-icon v-else-if="(data.raw as Desktop).protocol === 'rdp'" class="node-icon">
                <Monitor />
              </el-icon>
              <el-icon v-else class="node-icon"><Connection /></el-icon>
              <span class="node-text">
                <!-- 搜索时高亮匹配子串 -->
                <template v-for="(p, i) in highlightParts(data.label)" :key="i">
                  <mark v-if="p.hit" class="hl">{{ p.text }}</mark>
                  <template v-else>{{ p.text }}</template>
                </template>
              </span>
              <span v-if="data.type === 'desktop'" class="node-proto">
                {{ (data.raw as Desktop).protocol.toUpperCase() }}
              </span>
            </span>

            <!-- 悬浮操作按钮（右键菜单等价物） -->
            <el-dropdown
              class="node-menu"
              size="small"
              trigger="click"
              placement="bottom-end"
              @command="(cmd: string) => onCommand(cmd, data)"
            >
              <!-- stop 必须放在触发元素上：放在 el-dropdown 上不会阻止事件冒泡到树节点 -->
              <el-icon class="node-menu-icon" @click.stop><MoreFilled /></el-icon>
              <template #dropdown>
                <el-dropdown-menu v-if="data.type === 'desktop'">
                  <el-dropdown-item command="connect" :icon="'Link'">连接</el-dropdown-item>
                  <el-dropdown-item command="edit" :icon="'Edit'">编辑</el-dropdown-item>
                  <el-dropdown-item command="delete" :icon="'Delete'" divided>
                    删除
                  </el-dropdown-item>
                </el-dropdown-menu>
                <el-dropdown-menu v-else>
                  <el-dropdown-item command="newChild" :icon="'Plus'">
                    新建子桌面连接
                  </el-dropdown-item>
                  <el-dropdown-item command="rename" :icon="'EditPen'">重命名</el-dropdown-item>
                  <el-dropdown-item command="delete" :icon="'Delete'" divided>
                    删除
                  </el-dropdown-item>
                </el-dropdown-menu>
              </template>
            </el-dropdown>
          </div>
        </template>
      </el-tree>

      <!-- 空状态 -->
      <div v-else class="empty-state">
        <el-icon class="empty-icon"><Connection /></el-icon>
        <p class="empty-text">还没有桌面连接，点击右上角新建</p>
        <el-button type="primary" size="small" @click="emit('new', null)">
          新建连接
        </el-button>
      </div>
    </div>
  </aside>
</template>

<style scoped>
.desktop-sidebar {
  display: flex;
  flex-direction: column;
  width: 240px;
  flex-shrink: 0;
  height: 100%;
  padding: 8px;
  background: var(--el-bg-color-overlay);
  border-right: 1px solid var(--el-border-color-light);
  box-sizing: border-box;
  user-select: none;
}

.sidebar-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 4px 4px 8px;
}

.title {
  font-size: 13px;
  font-weight: 600;
  color: var(--el-text-color-primary);
  letter-spacing: 0.5px;
}

.actions {
  display: flex;
  gap: 4px;
}

.search-wrap {
  padding: 0 4px 8px;
}

/* 搜索高亮 */
.node-text mark.hl {
  background: var(--el-color-primary-light-5);
  color: var(--el-color-primary);
  border-radius: 2px;
  padding: 0;
}

.tree-wrap {
  flex: 1;
  overflow: auto;
  padding: 0 4px;
}

/* el-tree 节点高度统一 28px */
.tree-wrap :deep(.el-tree-node__content) {
  height: 28px;
}

.tree-wrap :deep(.el-tree-node__content:hover) {
  background: var(--el-fill-color-light);
}

.tree-wrap :deep(.el-tree-node.is-current > .el-tree-node__content) {
  background: var(--el-fill-color);
}

.tree-node {
  display: flex;
  align-items: center;
  justify-content: space-between;
  flex: 1;
  min-width: 0;
  height: 28px;
  padding-right: 4px;
}

.node-label {
  display: flex;
  align-items: center;
  gap: 6px;
  min-width: 0;
  font-size: 13px;
  color: var(--el-text-color-primary);
}

.node-icon {
  font-size: 14px;
  color: var(--el-text-color-secondary);
  flex-shrink: 0;
}

.is-group .node-icon {
  color: var(--el-color-primary);
}

/* 已打开标签的绿点 */
.status-dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: #67c23a;
  flex-shrink: 0;
}

.node-text {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  padding-left: 6px;
  border-left: 3px solid transparent;
}

/* 协议小字（RDP/VNC） */
.node-proto {
  font-size: 10px;
  color: var(--el-text-color-placeholder);
  flex-shrink: 0;
  transform: translateY(0.5px);
}

.node-menu {
  display: none;
  align-items: center;
  cursor: pointer;
  padding: 2px;
}

.tree-node:hover .node-menu,
.node-menu:focus-within {
  display: flex;
}

.node-menu-icon {
  font-size: 14px;
  color: var(--el-text-color-secondary);
  border-radius: 4px;
}

.node-menu-icon:hover {
  color: var(--el-color-primary);
  background: var(--el-fill-color);
}

/* 空状态 */
.empty-state {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  padding: 40px 16px;
  color: var(--el-text-color-secondary);
}

.empty-icon {
  font-size: 36px;
  color: var(--el-text-color-placeholder);
}

.empty-text {
  margin: 0;
  font-size: 12px;
  text-align: center;
  line-height: 1.6;
}

/* 滚动条美化 */
.tree-wrap::-webkit-scrollbar {
  width: 6px;
}
.tree-wrap::-webkit-scrollbar-thumb {
  background: var(--el-border-color);
  border-radius: 3px;
}
</style>
