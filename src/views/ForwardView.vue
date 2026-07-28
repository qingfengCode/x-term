<!--
  ForwardView.vue — 端口转发（SSH 隧道）管理视图

  功能：
  - 列出所有转发规则（forwardListRules）
  - 新建 / 编辑 / 删除规则
  - 启动 / 停止隧道（forwardStart / forwardStop）
  - 仅 Local 类型在 MVP 阶段可用，Remote / Dynamic 由后端返回 InvalidInput，前端 catch 展示

  运行状态采用纯前端内存维护（Set<string>），刷新页面后默认为停止。
-->
<script setup lang="ts">
import { onMounted, reactive, ref } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import type { FormInstance, FormRules } from "element-plus";
import { useSessionsStore } from "@/stores/sessions";
import {
  forwardDeleteRule,
  forwardListRules,
  forwardSaveRule,
  forwardStart,
  forwardStop,
} from "@/api/forward";
import type { ForwardRule } from "@/api/types";

const sessions = useSessionsStore();

const rules = ref<ForwardRule[]>([]);
// 当前在前端认为处于运行中的规则 id 集合（页面刷新后清空）
const running = ref<Set<string>>(new Set());
const loading = ref(false);
// 正在发起启动/停止请求的规则 id（用于按钮 loading）
const toggling = ref<Set<string>>(new Set());

// 类型中文标签映射
const kindLabel: Record<string, string> = {
  Local: "本地转发",
  Remote: "远程转发",
  Dynamic: "动态 SOCKS5",
};
const kindTagType: Record<string, "" | "success" | "warning" | "info" | "danger"> = {
  Local: "success",
  Remote: "warning",
  Dynamic: "info",
};

// ---- 弹窗表单 -------------------------------------------------------------
const dialogVisible = ref(false);
const dialogMode = ref<"create" | "edit">("create");
const formRef = ref<FormInstance>();

interface FormState {
  id: string | null;
  name: string;
  sessionId: string;
  kind: string;
  localHost: string;
  localPort: number;
  remoteHost: string;
  remotePort: number;
  autoStart: boolean;
  createdAt: string;
}

function emptyForm(): FormState {
  return {
    id: null,
    name: "",
    sessionId: "",
    kind: "Local",
    localHost: "127.0.0.1",
    localPort: 8080,
    remoteHost: "127.0.0.1",
    remotePort: 80,
    autoStart: false,
    createdAt: "",
  };
}

const form = reactive<FormState>(emptyForm());

const formRules: FormRules = {
  name: [{ required: true, message: "请输入规则名称", trigger: "blur" }],
  sessionId: [{ required: true, message: "请选择会话", trigger: "change" }],
  kind: [{ required: true, message: "请选择类型", trigger: "change" }],
  localHost: [{ required: true, message: "请输入本地绑定地址", trigger: "blur" }],
  localPort: [
    { required: true, message: "请输入本地端口", trigger: "blur" },
    { type: "number", min: 1, max: 65535, message: "端口范围 1-65535", trigger: "blur" },
  ],
  remoteHost: [{ required: true, message: "请输入远程地址", trigger: "blur" }],
  remotePort: [
    { required: true, message: "请输入远程端口", trigger: "blur" },
    { type: "number", min: 1, max: 65535, message: "端口范围 1-65535", trigger: "blur" },
  ],
};

// ---- 数据加载 -------------------------------------------------------------
async function loadRules() {
  loading.value = true;
  try {
    rules.value = await forwardListRules();
  } catch (e: any) {
    ElMessage.error("加载转发规则失败：" + (e?.message ?? String(e)));
  } finally {
    loading.value = false;
  }
}

function sessionName(id: string): string {
  const s = sessions.sessions.find((x) => x.id === id);
  return s ? s.name : "(未知会话)";
}

function isRunning(id: string): boolean {
  return running.value.has(id);
}

// ---- 新建 / 编辑 ----------------------------------------------------------
function openCreate() {
  dialogMode.value = "create";
  Object.assign(form, emptyForm());
  // 默认选第一个会话
  if (sessions.sessions.length > 0) {
    form.sessionId = sessions.sessions[0].id;
  }
  dialogVisible.value = true;
}

function openEdit(row: ForwardRule) {
  dialogMode.value = "edit";
  Object.assign(form, {
    id: row.id,
    name: row.name,
    sessionId: row.sessionId,
    kind: row.kind,
    localHost: row.localHost,
    localPort: row.localPort,
    remoteHost: row.remoteHost,
    remotePort: row.remotePort,
    autoStart: row.autoStart,
    createdAt: row.createdAt,
  });
  dialogVisible.value = true;
}

async function submitForm() {
  if (!formRef.value) return;
  try {
    await formRef.value.validate();
  } catch {
    return;
  }
  const rule: ForwardRule = {
    id: form.id ?? cryptoId(),
    name: form.name.trim(),
    sessionId: form.sessionId,
    kind: form.kind,
    localHost: form.localHost.trim(),
    localPort: Number(form.localPort),
    remoteHost: form.remoteHost.trim(),
    remotePort: Number(form.remotePort),
    autoStart: !!form.autoStart,
    createdAt: form.createdAt || new Date().toISOString(),
  };
  try {
    await forwardSaveRule(rule);
    ElMessage.success(dialogMode.value === "create" ? "已新建转发规则" : "已保存修改");
    dialogVisible.value = false;
    await loadRules();
  } catch (e: any) {
    ElMessage.error("保存失败：" + (e?.message ?? String(e)));
  }
}

// 简单的本地 id 生成（后端若用 UUID 会被覆盖）
function cryptoId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return "fwd_" + Date.now().toString(36) + Math.random().toString(36).slice(2, 8);
}

// ---- 删除 -----------------------------------------------------------------
async function removeRule(row: ForwardRule) {
  try {
    await ElMessageBox.confirm(`确定删除转发规则「${row.name}」吗？`, "删除确认", {
      type: "warning",
      confirmButtonText: "删除",
      cancelButtonText: "取消",
    });
  } catch {
    return;
  }
  try {
    await forwardDeleteRule(row.id);
    running.value.delete(row.id);
    ElMessage.success("已删除");
    await loadRules();
  } catch (e: any) {
    ElMessage.error("删除失败：" + (e?.message ?? String(e)));
  }
}

// ---- 启动 / 停止 ----------------------------------------------------------
async function startRule(row: ForwardRule) {
  toggling.value.add(row.id);
  try {
    await forwardStart(row.id);
    running.value.add(row.id);
    ElMessage.success(`已启动：${row.name}`);
  } catch (e: any) {
    ElMessage.error("启动失败：" + (e?.message ?? String(e)));
  } finally {
    toggling.value.delete(row.id);
  }
}

async function stopRule(row: ForwardRule) {
  toggling.value.add(row.id);
  try {
    await forwardStop(row.id);
    running.value.delete(row.id);
    ElMessage.success(`已停止：${row.name}`);
  } catch (e: any) {
    ElMessage.error("停止失败：" + (e?.message ?? String(e)));
  } finally {
    toggling.value.delete(row.id);
  }
}

onMounted(async () => {
  // 确保 sessions 已加载（用于显示会话名）
  if (!sessions.loaded) {
    try {
      await sessions.load();
    } catch {
      /* ignore */
    }
  }
  await loadRules();
});
</script>

<template>
  <div class="forward-view">
    <div class="header">
      <div class="title">
        <h2>端口转发</h2>
        <p class="subtitle">通过 SSH 隧道转发本地 / 远程端口</p>
      </div>
      <el-button type="primary" :icon="undefined" @click="openCreate">新建转发规则</el-button>
    </div>

    <el-table
      v-loading="loading"
      :data="rules"
      empty-text="暂无转发规则，点击右上角“新建转发规则”创建"
      stripe
      class="rules-table"
    >
      <el-table-column label="名称" prop="name" min-width="140">
        <template #default="{ row }">
          <span class="rule-name">{{ row.name }}</span>
        </template>
      </el-table-column>

      <el-table-column label="会话" min-width="140">
        <template #default="{ row }">{{ sessionName(row.sessionId) }}</template>
      </el-table-column>

      <el-table-column label="类型" width="120">
        <template #default="{ row }">
          <el-tag :type="kindTagType[row.kind] ?? 'info'" size="small" effect="light">
            {{ kindLabel[row.kind] ?? row.kind }}
          </el-tag>
        </template>
      </el-table-column>

      <el-table-column label="转发" min-width="220">
        <template #default="{ row }">
          <span class="addr">{{ row.localHost }}:{{ row.localPort }}</span>
          <span class="arrow">→</span>
          <span class="addr">{{ row.remoteHost }}:{{ row.remotePort }}</span>
        </template>
      </el-table-column>

      <el-table-column label="状态" width="110" align="center">
        <template #default="{ row }">
          <el-tag v-if="isRunning(row.id)" type="success" size="small" effect="dark">运行中</el-tag>
          <el-tag v-else type="info" size="small" effect="plain">已停止</el-tag>
        </template>
      </el-table-column>

      <el-table-column label="操作" width="240" align="center" fixed="right">
        <template #default="{ row }">
          <el-button
            v-if="!isRunning(row.id)"
            type="success"
            size="small"
            :loading="toggling.has(row.id)"
            @click="startRule(row)"
          >
            启动
          </el-button>
          <el-button
            v-else
            type="warning"
            size="small"
            :loading="toggling.has(row.id)"
            @click="stopRule(row)"
          >
            停止
          </el-button>
          <el-button size="small" :disabled="isRunning(row.id)" @click="openEdit(row)">
            编辑
          </el-button>
          <el-button type="danger" size="small" :disabled="isRunning(row.id)" @click="removeRule(row)">
            删除
          </el-button>
        </template>
      </el-table-column>
    </el-table>

    <!-- 新建 / 编辑 弹窗 -->
    <el-dialog
      v-model="dialogVisible"
      :title="dialogMode === 'create' ? '新建转发规则' : '编辑转发规则'"
      width="560px"
      :close-on-click-modal="false"
    >
      <el-form
        ref="formRef"
        :model="form"
        :rules="formRules"
        label-width="100px"
        label-position="right"
      >
        <el-form-item label="名称" prop="name">
          <el-input v-model="form.name" placeholder="例如：内网 web" />
        </el-form-item>

        <el-form-item label="会话" prop="sessionId">
          <el-select v-model="form.sessionId" placeholder="选择 SSH 会话" filterable style="width: 100%">
            <el-option
              v-for="s in sessions.sessions"
              :key="s.id"
              :label="s.name"
              :value="s.id"
            />
          </el-select>
        </el-form-item>

        <el-form-item label="类型" prop="kind">
          <el-select v-model="form.kind" style="width: 100%">
            <el-option label="本地转发 (Local)" value="Local" />
            <el-option label="远程转发 (Remote)" value="Remote" />
            <el-option label="动态 SOCKS5 (Dynamic)" value="Dynamic" />
          </el-select>
        </el-form-item>

        <el-form-item label="本地绑定">
          <div class="port-row">
            <el-input v-model="form.localHost" placeholder="127.0.0.1" />
            <el-input-number v-model="form.localPort" :min="1" :max="65535" controls-position="right" />
          </div>
        </el-form-item>

        <el-form-item label="远程目标">
          <div class="port-row">
            <el-input v-model="form.remoteHost" placeholder="127.0.0.1" />
            <el-input-number v-model="form.remotePort" :min="1" :max="65535" controls-position="right" />
          </div>
        </el-form-item>

        <el-form-item label="自动启动">
          <el-switch v-model="form.autoStart" />
          <span class="form-hint">建立 SSH 会话后自动开启本规则</span>
        </el-form-item>
      </el-form>

      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" @click="submitForm">确定</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.forward-view {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 16px;
  height: 100%;
  box-sizing: border-box;
}

.header {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 16px;
}

.title h2 {
  margin: 0;
  font-size: 20px;
  font-weight: 600;
  color: var(--el-text-color-primary);
}

.subtitle {
  margin: 4px 0 0;
  font-size: 13px;
  color: var(--el-text-color-secondary);
}

.rules-table {
  flex: 1;
  min-height: 0;
}

.rule-name {
  font-weight: 500;
  color: var(--el-text-color-primary);
}

.addr {
  font-family: "Consolas", "Cascadia Code", monospace;
  font-size: 13px;
  color: var(--el-text-color-regular);
}

.arrow {
  margin: 0 8px;
  color: var(--el-text-color-secondary);
}

.port-row {
  display: flex;
  gap: 8px;
  width: 100%;
}

.port-row .el-input {
  flex: 1;
}

.form-hint {
  margin-left: 12px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
</style>
