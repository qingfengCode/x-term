<!--
  密钥管理页：列出/添加/查看/删除凭据（密码 + 私钥文本）。
  凭据加密存储于保险库，列表只读 DB 的 kind 列（不解密）。
-->
<script setup lang="ts">
import { onMounted, ref } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { Plus, Delete, View, EditPen, Key, Lock } from "@element-plus/icons-vue";
import {
  credentialList,
  credentialSave,
  credentialGet,
  credentialDelete,
  credentialRename,
  type CredentialView,
  type CredentialInput,
} from "@/api/vault";

defineOptions({ name: "KeyManagerView" });

const list = ref<CredentialView[]>([]);
const loading = ref(false);

// 添加/编辑对话框
const dialogVisible = ref(false);
const editingId = ref<string | null>(null);
const form = ref<{
  name: string;
  kind: "password" | "private_key_text";
  value: string;
  passphrase: string;
}>({ name: "", kind: "private_key_text", value: "", passphrase: "" });

async function load() {
  loading.value = true;
  try {
    list.value = await credentialList();
  } catch (e: unknown) {
    ElMessage.error("加载凭据列表失败：" + String(e));
  } finally {
    loading.value = false;
  }
}

function openAdd() {
  editingId.value = null;
  form.value = { name: "", kind: "private_key_text", value: "", passphrase: "" };
  dialogVisible.value = true;
}

async function submit() {
  if (!form.value.name.trim()) {
    ElMessage.warning("请输入名称");
    return;
  }
  if (!form.value.value.trim()) {
    ElMessage.warning("请输入内容");
    return;
  }
  try {
    const input: CredentialInput = {
      id: editingId.value ?? undefined,
      name: form.value.name.trim(),
      kind: form.value.kind,
      value: form.value.value,
      passphrase: form.value.kind === "private_key_text" ? form.value.passphrase || undefined : undefined,
    };
    await credentialSave(input);
    ElMessage.success(editingId.value ? "已更新" : "已添加");
    dialogVisible.value = false;
    await load();
  } catch (e: unknown) {
    ElMessage.error("保存失败：" + String(e));
  }
}

async function reveal(c: CredentialView) {
  try {
    await ElMessageBox.confirm(`确认查看「${c.name}」的明文内容？`, "查看确认", {
      type: "warning",
    });
  } catch {
    return;
  }
  try {
    const plain = await credentialGet(c.id);
    // plain 是 JSON 字符串 {kind, value, passphrase}
    const data = JSON.parse(plain) as { value: string; passphrase?: string };
    await ElMessageBox.alert(
      `<pre style="max-height:400px;overflow:auto;text-align:left;word-break:break-all;font-size:12px">${escapeHtml(data.value)}${data.passphrase ? `\n\nPassphrase: ${escapeHtml(data.passphrase)}` : ""}</pre>`,
      `「${c.name}」明文`,
      { dangerouslyUseHTMLString: true, confirmButtonText: "关闭" },
    );
  } catch (e: unknown) {
    ElMessage.error("查看失败：" + String(e));
  }
}

function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

async function rename(c: CredentialView) {
  try {
    const { value } = await ElMessageBox.prompt("新名称", "重命名", {
      inputValue: c.name,
      confirmButtonText: "确定",
      cancelButtonText: "取消",
    });
    if (value && value !== c.name) {
      await credentialRename(c.id, value);
      ElMessage.success("已重命名");
      await load();
    }
  } catch {
    /* 取消 */
  }
}

async function remove(c: CredentialView) {
  try {
    await ElMessageBox.confirm(`确认删除「${c.name}」？此操作不可恢复。`, "删除确认", {
      type: "warning",
      confirmButtonText: "删除",
      cancelButtonText: "取消",
    });
    await credentialDelete(c.id);
    ElMessage.success("已删除");
    await load();
  } catch {
    /* 取消 */
  }
}

function kindLabel(kind: string): string {
  if (kind === "private_key_text") return "私钥";
  if (kind === "password") return "密码";
  return kind;
}

onMounted(load);
</script>

<template>
  <div class="key-manager">
    <div class="header">
      <h2><el-icon><Key /></el-icon> 密钥管理</h2>
      <el-button type="primary" :icon="Plus" @click="openAdd">添加凭据</el-button>
    </div>
    <div class="hint">
      所有凭据（密码、私钥）经主密码派生密钥 AES-256-GCM 加密后存储，运行时解密。
      列表不显示明文，需手动点"查看"。
    </div>

    <el-table :data="list" v-loading="loading" border stripe>
      <el-table-column label="名称" prop="name" min-width="160">
        <template #default="{ row }">
          <el-icon class="kind-icon">
            <Key v-if="row.kind === 'private_key_text'" />
            <Lock v-else />
          </el-icon>
          {{ row.name }}
        </template>
      </el-table-column>
      <el-table-column label="类型" width="100">
        <template #default="{ row }">
          <el-tag size="small" :type="row.kind === 'private_key_text' ? 'success' : 'info'">
            {{ kindLabel(row.kind) }}
          </el-tag>
        </template>
      </el-table-column>
      <el-table-column label="创建时间" width="180">
        <template #default="{ row }">
          {{ new Date(row.createdAt).toLocaleString() }}
        </template>
      </el-table-column>
      <el-table-column label="操作" width="220" align="center">
        <template #default="{ row }">
          <el-button size="small" :icon="View" link @click="reveal(row)">查看</el-button>
          <el-button size="small" :icon="EditPen" link @click="rename(row)">重命名</el-button>
          <el-button size="small" :icon="Delete" link type="danger" @click="remove(row)">删除</el-button>
        </template>
      </el-table-column>
    </el-table>

    <!-- 添加/编辑对话框 -->
    <el-dialog
      v-model="dialogVisible"
      :title="editingId ? '编辑凭据' : '添加凭据'"
      width="560px"
    >
      <el-form label-width="90px">
        <el-form-item label="名称">
          <el-input v-model="form.name" placeholder="如：我的服务器私钥" />
        </el-form-item>
        <el-form-item label="类型">
          <el-radio-group v-model="form.kind">
            <el-radio value="private_key_text">私钥</el-radio>
            <el-radio value="password">密码</el-radio>
          </el-radio-group>
        </el-form-item>
        <el-form-item :label="form.kind === 'private_key_text' ? '私钥内容' : '密码'">
          <el-input
            v-model="form.value"
            type="textarea"
            :autosize="{ minRows: 4, maxRows: 14 }"
            :placeholder="form.kind === 'private_key_text' ? '粘贴 PEM/OpenSSH 私钥文本（-----BEGIN ...）' : '输入密码'"
          />
        </el-form-item>
        <el-form-item v-if="form.kind === 'private_key_text'" label="Passphrase">
          <el-input v-model="form.passphrase" placeholder="私钥的 passphrase（无则留空）" />
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
.key-manager {
  padding: 20px 24px;
  height: 100%;
  overflow: auto;
  box-sizing: border-box;
}
.header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 12px;
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
  margin-bottom: 16px;
  line-height: 1.6;
}
.kind-icon {
  margin-right: 4px;
  vertical-align: middle;
}
</style>
