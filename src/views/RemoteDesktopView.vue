<!--
  桌面页：独立管理 RDP/VNC 连接（独立 CRUD，不复用终端 sessions）。
  点击连接启动外部客户端（mstsc/vncviewer）。
-->
<script setup lang="ts">
import { computed, onMounted, reactive, ref } from "vue";
import { ElMessage, ElMessageBox, type FormInstance } from "element-plus";
import { Plus, Delete, EditPen, Monitor, Connection, RefreshRight } from "@element-plus/icons-vue";
import { useDesktopsStore } from "@/stores/desktops";
import { remoteDesktopLaunch, type Desktop } from "@/api/remote_desktop";
import { credentialSave, credentialGet } from "@/api/vault";

defineOptions({ name: "RemoteDesktopView" });

const store = useDesktopsStore();

const PROTOCOL_OPTIONS: { value: "rdp" | "vnc"; label: string; port: number }[] = [
  { value: "rdp", label: "RDP (Windows 桌面)", port: 3389 },
  { value: "vnc", label: "VNC", port: 5900 },
];

// --- 对话框 ---
const dialogVisible = ref(false);
const editingId = ref<string | null>(null);
const formRef = ref<FormInstance>();
const form = reactive({
  name: "",
  protocol: "rdp" as "rdp" | "vnc",
  host: "",
  port: 3389,
  username: "",
  password: "",
});

function openAdd() {
  editingId.value = null;
  form.name = "";
  form.protocol = "rdp";
  form.host = "";
  form.port = 3389;
  form.username = "";
  form.password = "";
  dialogVisible.value = true;
}

function openEdit(d: Desktop) {
  editingId.value = d.id;
  form.name = d.name;
  form.protocol = d.protocol;
  form.host = d.host;
  form.port = d.port;
  form.username = d.username ?? "";
  form.password = ""; // 编辑时密码留空表示不修改
  dialogVisible.value = true;
}

// 切协议调默认端口。
function onProtocolChange(proto: "rdp" | "vnc") {
  const opt = PROTOCOL_OPTIONS.find((o) => o.value === proto);
  if (opt) form.port = opt.port;
}

async function submit() {
  if (!form.name.trim() || !form.host.trim()) {
    ElMessage.warning("请填写名称和主机");
    return;
  }
  const now = new Date().toISOString();
  const existed = store.desktops.find((d) => d.id === editingId.value);

  // 密码处理：填了密码就加密存 vault，拿 credentialId 关联。
  let credentialId = existed?.credentialId ?? null;
  if (form.password.trim()) {
    try {
      credentialId = await credentialSave({
        name: `${form.name} 密码`,
        kind: "password",
        value: form.password,
      });
    } catch (e: unknown) {
      ElMessage.error("保存密码失败：" + String(e));
      return;
    }
  }

  const desktop: Desktop = {
    id: editingId.value ?? `d-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
    name: form.name.trim(),
    protocol: form.protocol,
    host: form.host.trim(),
    port: form.port,
    username: form.username.trim() || null,
    credentialId,
    sortOrder: existed?.sortOrder ?? 0,
    createdAt: existed?.createdAt ?? now,
    updatedAt: now,
  };
  try {
    await store.save(desktop);
    ElMessage.success(editingId.value ? "已更新" : "已添加");
    dialogVisible.value = false;
  } catch (e: unknown) {
    ElMessage.error("保存失败：" + String(e));
  }
}

async function connect(d: Desktop) {
  try {
    // 如果有关联凭据，取出密码传给启动命令。
    let password: string | undefined;
    if (d.credentialId) {
      try {
        const plain = await credentialGet(d.credentialId);
        const data = JSON.parse(plain) as { value?: string };
        password = data.value;
      } catch {
        /* 凭据读取失败不阻塞连接 */
      }
    }
    const msg = await remoteDesktopLaunch({
      protocol: d.protocol,
      host: d.host,
      port: d.port,
      username: d.username || undefined,
      password,
    });
    ElMessage.success(msg);
  } catch (e: unknown) {
    ElMessage.error("启动桌面客户端失败：" + String(e));
  }
}

async function remove(d: Desktop) {
  try {
    await ElMessageBox.confirm(`确认删除「${d.name}」？`, "删除确认", {
      type: "warning",
      confirmButtonText: "删除",
      cancelButtonText: "取消",
    });
    await store.remove(d.id);
    ElMessage.success("已删除");
  } catch {
    /* 取消 */
  }
}

function protoIcon(p: string) {
  return p === "rdp" ? Monitor : Connection;
}

onMounted(async () => {
  if (!store.loaded) {
    try {
      await store.load();
    } catch {
      /* ignore */
    }
  }
});
</script>

<template>
  <div class="desktop-view">
    <div class="header">
      <h2><el-icon><Monitor /></el-icon> 桌面</h2>
      <el-button type="primary" :icon="Plus" @click="openAdd">新建连接</el-button>
    </div>
    <div class="hint">
      管理 RDP（Windows 桌面）和 VNC 连接。点击连接会启动系统自带的桌面客户端
      （Windows: mstsc / VNC: vncviewer）。
    </div>

    <div v-if="store.desktops.length === 0" class="empty">
      <el-empty description="还没有桌面连接。点击「新建连接」添加 RDP/VNC。" />
    </div>

    <div v-else class="card-list">
      <div v-for="d in store.desktops" :key="d.id" class="d-card">
        <div class="card-icon">
          <el-icon :size="28"><component :is="protoIcon(d.protocol)" /></el-icon>
        </div>
        <div class="card-info">
          <div class="card-title">
            {{ d.name }}
            <el-tag size="small" :type="d.protocol === 'rdp' ? 'primary' : 'success'">
              {{ d.protocol.toUpperCase() }}
            </el-tag>
          </div>
          <div class="card-meta">{{ d.host }}:{{ d.port }} · {{ d.username || "—" }}</div>
        </div>
        <div class="card-actions">
          <el-button type="primary" size="small" :icon="RefreshRight" @click="connect(d)">
            连接
          </el-button>
          <el-button size="small" :icon="EditPen" link @click="openEdit(d)">编辑</el-button>
          <el-button size="small" :icon="Delete" link type="danger" @click="remove(d)">删除</el-button>
        </div>
      </div>
    </div>

    <!-- 新建/编辑对话框（独立于 SessionDialog） -->
    <el-dialog v-model="dialogVisible" :title="editingId ? '编辑桌面连接' : '新建桌面连接'" width="480px">
      <el-form ref="formRef" label-width="80px">
        <el-form-item label="名称">
          <el-input v-model="form.name" placeholder="如：办公电脑" />
        </el-form-item>
        <el-form-item label="协议">
          <el-select v-model="form.protocol" @change="onProtocolChange" style="width: 100%">
            <el-option v-for="opt in PROTOCOL_OPTIONS" :key="opt.value" :label="opt.label" :value="opt.value" />
          </el-select>
        </el-form-item>
        <el-form-item label="主机">
          <el-input v-model="form.host" placeholder="IP 或域名" />
        </el-form-item>
        <el-form-item label="端口">
          <el-input-number v-model="form.port" :min="1" :max="65535" controls-position="right" />
        </el-form-item>
        <el-form-item label="用户名">
          <el-input v-model="form.username" placeholder="可选" />
        </el-form-item>
        <el-form-item label="密码">
          <el-input
            v-model="form.password"
            type="password"
            show-password
            :placeholder="editingId ? '留空表示不修改' : '可选'"
          />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" @click="submit">保存</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.desktop-view {
  padding: 20px 24px;
  height: 100%;
  overflow: auto;
  box-sizing: border-box;
}
.header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 8px;
}
.header h2 {
  margin: 0;
  font-size: 20px;
  font-weight: 600;
  display: flex;
  align-items: center;
  gap: 8px;
}
.hint {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  margin-bottom: 20px;
  line-height: 1.6;
}
.empty {
  display: flex;
  justify-content: center;
  padding: 40px 0;
}
.card-list {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(360px, 1fr));
  gap: 12px;
}
.d-card {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 14px 16px;
  background: var(--el-bg-color);
  border: 1px solid var(--el-border-color);
  border-radius: 8px;
  transition: box-shadow 0.15s, border-color 0.15s;
}
.d-card:hover {
  border-color: var(--el-color-primary);
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.08);
}
.card-icon {
  color: var(--el-color-primary);
  flex-shrink: 0;
}
.card-info {
  flex: 1;
  min-width: 0;
}
.card-title {
  font-size: 14px;
  font-weight: 600;
  display: flex;
  align-items: center;
  gap: 8px;
}
.card-meta {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  margin-top: 4px;
  font-family: "Consolas", monospace;
}
.card-actions {
  display: flex;
  align-items: center;
  gap: 4px;
  flex-shrink: 0;
}
</style>
