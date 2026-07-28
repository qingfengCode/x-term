<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { useSessionsStore, type TreeNode } from "@/stores/sessions";
import { useTerminalsStore } from "@/stores/terminals";
import type { Group, Session } from "@/api/types";
import SessionDialog from "./SessionDialog.vue";

const sessionsStore = useSessionsStore();
const terminalsStore = useTerminalsStore();

// --- 过滤 ---------------------------------------------------------------
const filter = ref("");
const filterText = computed(() => filter.value.trim().toLowerCase());

const treeRef = ref();
const filterInputRef = ref<{ focus: () => void } | null>(null);
const treeProps = { label: "label", children: "children" };

// el-tree 通过 ref.filter 调用触发 filter-node-method。
watch(filterText, (v) => treeRef.value?.filter(v));

/** 聚焦搜索框（供全局快捷键 focusSessions 调用）。 */
function focusFilter() {
  filterInputRef.value?.focus();
}

defineExpose({ focusFilter });

function filterNode(value: string, data: TreeNode) {
  if (!value) return true;
  if (data.type === "session") return data.label.toLowerCase().includes(value);
  // 分组：本身匹配或包含匹配的子节点（el-tree 默认会保留父节点）。
  return data.label.toLowerCase().includes(value);
}

const isEmpty = computed(
  () => sessionsStore.terminalSessions.length === 0 && sessionsStore.groups.length === 0
);

// --- 对话框 -------------------------------------------------------------
const dialogVisible = ref(false);
const editingSession = ref<Session | null>(null);
const defaultGroupId = ref<string | null>(null);

function openNewSession(groupId: string | null = null) {
  editingSession.value = null;
  defaultGroupId.value = groupId;
  dialogVisible.value = true;
}

function openEditSession(s: Session) {
  editingSession.value = s;
  defaultGroupId.value = s.groupId;
  dialogVisible.value = true;
}

async function duplicateSession(s: Session) {
  const now = new Date().toISOString();
  const copy: Session = {
    ...s,
    id: crypto.randomUUID(),
    name: `${s.name} 副本`,
    credentialId: s.credentialId, // 共享同一凭据 id
    createdAt: now,
    updatedAt: now,
  };
  try {
    await sessionsStore.saveSession(copy);
    ElMessage.success("已复制会话");
  } catch (e) {
    ElMessage.error("复制失败: " + String(e));
  }
}

// --- 连接 ---------------------------------------------------------------
async function connectSession(s: Session) {
  const msg = ElMessage.info({ message: `正在连接 ${s.name}...`, duration: 0 });
  try {
    await terminalsStore.open(s);
    msg.close();
    ElMessage.success(`已连接 ${s.name}`);
  } catch (e) {
    msg.close();
    ElMessage.error("连接失败: " + String(e));
  }
}

// --- 点击 / 交互 --------------------------------------------------------
// 单击会话节点即连接（更顺手，符合常见终端管理器习惯）。
// 分组节点单击仅展开/折叠（由 el-tree 的 expand-on-click-node=false 关闭，
// 默认点击箭头才展开，避免误连）。
function onNodeClick(data: TreeNode) {
  if (data.type === "session") connectSession(data.raw as Session);
}

// --- 右键菜单（el-dropdown 方式，更可控）--------------------------------
// 每个节点 template 内嵌 el-dropdown，避免全局 contextmenu 定位问题。

function onCommand(cmd: string, data: TreeNode) {
  if (data.type === "session") {
    const s = data.raw as Session;
    switch (cmd) {
      case "connect":
        connectSession(s);
        break;
      case "edit":
        openEditSession(s);
        break;
      case "copy":
        duplicateSession(s);
        break;
      case "delete":
        confirmDeleteSession(s);
        break;
    }
  } else {
    const g = data.raw as Group;
    switch (cmd) {
      case "newChild":
        openNewSession(g.id);
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

async function confirmDeleteSession(s: Session) {
  try {
    await ElMessageBox.confirm(
      `确定删除会话 "${s.name}" 吗？`,
      "删除会话",
      { type: "warning", confirmButtonText: "删除", cancelButtonText: "取消" }
    );
  } catch {
    return;
  }
  try {
    await sessionsStore.removeSession(s.id);
    ElMessage.success("已删除");
  } catch (e) {
    ElMessage.error("删除失败: " + String(e));
  }
}

async function confirmDeleteGroup(g: Group) {
  const hasChild = sessionsStore.terminalSessions.some((s) => s.groupId === g.id);
  try {
    await ElMessageBox.confirm(
      hasChild
        ? `分组 "${g.name}" 下仍有会话，删除分组后会话将变为无分组。继续？`
        : `确定删除分组 "${g.name}" 吗？`,
      "删除分组",
      { type: "warning", confirmButtonText: "删除", cancelButtonText: "取消" }
    );
  } catch {
    return;
  }
  try {
    await sessionsStore.removeGroup(g.id);
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
    await sessionsStore.saveGroup({
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

async function renameGroup(g: Group) {
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
    await sessionsStore.saveGroup({ ...g, name: name.trim() });
    ElMessage.success("已重命名");
  } catch (e) {
    ElMessage.error("重命名失败: " + String(e));
  }
}

// 默认展开所有分组。
const defaultExpandedKeys = computed(() => sessionsStore.groups.map((g) => g.id));
</script>

<template>
  <aside class="session-sidebar">
    <!-- 顶部标题 + 操作 -->
    <header class="sidebar-header">
      <span class="title">会话</span>
      <div class="actions">
        <el-tooltip content="新建会话" placement="bottom">
          <el-button
            circle
            size="small"
            :icon="'Plus'"
            @click="openNewSession(null)"
          />
        </el-tooltip>
        <el-tooltip content="新建分组" placement="bottom">
          <el-button
            circle
            size="small"
            :icon="'FolderAdd'"
            @click="createRootGroup"
          />
        </el-tooltip>
      </div>
    </header>

    <!-- 搜索 -->
    <div class="search-wrap">
      <el-input
        ref="filterInputRef"
        v-model="filter"
        placeholder="搜索会话"
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
        :data="sessionsStore.terminalTree"
        :props="treeProps"
        node-key="id"
        :default-expanded-keys="defaultExpandedKeys"
        :filter-node-method="filterNode"
        :expand-on-click-node="false"
        :highlight-current="true"
        @node-click="onNodeClick"
      >
        <template #default="{ data }">
          <div class="tree-node" :class="{ 'is-group': data.type === 'group' }">
            <span class="node-label">
              <el-icon v-if="data.type === 'group'" class="node-icon">
                <Folder />
              </el-icon>
              <el-icon v-else class="node-icon">
                <Monitor />
              </el-icon>
              <span
                class="node-text"
                :style="
                  data.type === 'session' && (data.raw as Session).color
                    ? { borderLeftColor: String((data.raw as Session).color) }
                    : undefined
                "
                >{{ data.label }}</span
              >
            </span>

            <!-- 悬浮操作按钮（右键菜单等价物） -->
            <el-dropdown
              class="node-menu"
              trigger="click"
              placement="bottom-end"
              @command="(cmd: string) => onCommand(cmd, data)"
              @click.stop
            >
              <el-icon class="node-menu-icon"><MoreFilled /></el-icon>
              <template #dropdown>
                <el-dropdown-menu v-if="data.type === 'session'">
                  <el-dropdown-item command="connect" :icon="'Link'">连接</el-dropdown-item>
                  <el-dropdown-item command="edit" :icon="'Edit'">编辑</el-dropdown-item>
                  <el-dropdown-item command="copy" :icon="'CopyDocument'">复制</el-dropdown-item>
                  <el-dropdown-item command="delete" :icon="'Delete'" divided>
                    删除
                  </el-dropdown-item>
                </el-dropdown-menu>
                <el-dropdown-menu v-else>
                  <el-dropdown-item command="newChild" :icon="'Plus'">
                    新建子会话
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
        <p class="empty-text">还没有会话，点击右上角新建</p>
        <el-button type="primary" size="small" @click="openNewSession(null)">
          新建会话
        </el-button>
      </div>
    </div>

    <!-- 新建/编辑对话框 -->
    <SessionDialog
      v-model:visible="dialogVisible"
      :session="editingSession"
      :default-group-id="defaultGroupId"
    />
  </aside>
</template>

<style scoped>
.session-sidebar {
  display: flex;
  flex-direction: column;
  width: 240px;
  min-width: 240px;
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

.node-text {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  padding-left: 6px;
  border-left: 3px solid transparent;
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
