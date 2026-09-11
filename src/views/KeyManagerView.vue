<!--
  密钥管理页：列出/添加/查看/删除凭据（密码 + 私钥文本）。
  凭据加密存储于保险库，列表只读 DB 的 kind 列（不解密）。
-->
<script setup lang="ts">
import { onMounted, ref } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import {
  credentialList,
  credentialSave,
  credentialGet,
  credentialDelete,
  credentialRename,
  sshKeyGenerate,
  type CredentialView,
  type CredentialInput,
  type SshKeyGenerateInput,
  type GeneratedKeyInfo,
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
  let value: string;
  try {
    ({ value } = await ElMessageBox.prompt("新名称", "重命名", {
      inputValue: c.name,
      confirmButtonText: "确定",
      cancelButtonText: "取消",
    }));
  } catch {
    return; // 用户取消
  }
  if (!value || value === c.name) return;
  try {
    await credentialRename(c.id, value);
    ElMessage.success("已重命名");
    await load();
  } catch (e: unknown) {
    // API 失败与用户取消分开处理：失败必须有提示，不能被"取消"分支吞掉。
    ElMessage.error("重命名失败：" + String(e));
  }
}

async function remove(c: CredentialView) {
  try {
    await ElMessageBox.confirm(`确认删除「${c.name}」？此操作不可恢复。`, "删除确认", {
      type: "warning",
      confirmButtonText: "删除",
      cancelButtonText: "取消",
    });
  } catch {
    return; // 用户取消
  }
  try {
    await credentialDelete(c.id);
    ElMessage.success("已删除");
    await load();
  } catch (e: unknown) {
    ElMessage.error("删除失败：" + String(e));
  }
}

function kindLabel(kind: string): string {
  if (kind === "private_key_text") return "私钥";
  if (kind === "password") return "密码";
  return kind;
}

// 生成密钥对对话框
const genVisible = ref(false);
const generating = ref(false);
const genForm = ref<{
  name: string;
  algorithm: "ed25519" | "rsa";
  bits: number;
  passphrase: string;
}>({ name: "", algorithm: "ed25519", bits: 3072, passphrase: "" });

// 生成结果（只含公钥，私钥已入保险库）
const genResult = ref<GeneratedKeyInfo | null>(null);
const resultVisible = ref(false);

function openGen() {
  genForm.value = { name: "", algorithm: "ed25519", bits: 3072, passphrase: "" };
  genVisible.value = true;
}

async function submitGen() {
  if (!genForm.value.name.trim()) {
    ElMessage.warning("请输入名称");
    return;
  }
  generating.value = true;
  try {
    const input: SshKeyGenerateInput = {
      name: genForm.value.name.trim(),
      algorithm: genForm.value.algorithm,
      bits: genForm.value.algorithm === "rsa" ? genForm.value.bits : undefined,
      passphrase: genForm.value.passphrase || undefined,
    };
    genResult.value = await sshKeyGenerate(input);
    genVisible.value = false;
    resultVisible.value = true;
    await load();
  } catch (e: unknown) {
    ElMessage.error("生成失败：" + String(e));
  } finally {
    generating.value = false;
  }
}

async function copyPublicKey() {
  if (!genResult.value) return;
  try {
    await navigator.clipboard.writeText(genResult.value.publicKey);
    ElMessage.success("公钥已复制");
  } catch {
    ElMessage.error("复制失败，请手动复制");
  }
}

onMounted(load);
</script>

<template>
  <div class="key-manager">
    <div class="header">
      <h2><el-icon><Key /></el-icon> 密钥管理</h2>
      <div class="header-actions">
        <el-button type="primary" :icon="'MagicStick'" @click="openGen">生成密钥对</el-button>
        <el-button type="primary" :icon="'Plus'" @click="openAdd">添加凭据</el-button>
      </div>
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
          <el-button size="small" :icon="'View'" link @click="reveal(row)">查看</el-button>
          <el-button size="small" :icon="'EditPen'" link @click="rename(row)">重命名</el-button>
          <el-button size="small" :icon="'Delete'" link type="danger" @click="remove(row)">删除</el-button>
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

    <!-- 生成密钥对对话框 -->
    <el-dialog v-model="genVisible" title="生成密钥对" width="560px">
      <el-form label-width="90px">
        <el-form-item label="名称">
          <el-input v-model="genForm.name" placeholder="如：gitlab 部署密钥（同时用作公钥注释）" />
        </el-form-item>
        <el-form-item label="算法">
          <el-radio-group v-model="genForm.algorithm">
            <el-radio value="ed25519">ED25519（推荐）</el-radio>
            <el-radio value="rsa">RSA</el-radio>
          </el-radio-group>
        </el-form-item>
        <el-form-item v-if="genForm.algorithm === 'rsa'" label="位数">
          <el-radio-group v-model="genForm.bits">
            <el-radio-button :value="2048">2048</el-radio-button>
            <el-radio-button :value="3072">3072</el-radio-button>
            <el-radio-button :value="4096">4096</el-radio-button>
          </el-radio-group>
          <div class="gen-hint">RSA 生成需要数秒，请耐心等待</div>
        </el-form-item>
        <el-form-item label="私钥口令">
          <el-input
            v-model="genForm.passphrase"
            type="password"
            show-password
            placeholder="留空则私钥不加密（可选）"
          />
          <div class="gen-hint">设置口令后，使用该私钥连接时需输入相同口令</div>
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button :disabled="generating" @click="genVisible = false">取消</el-button>
        <el-button type="primary" :loading="generating" @click="submitGen">生成并保存</el-button>
      </template>
    </el-dialog>

    <!-- 生成结果对话框（只展示公钥，私钥已入库） -->
    <el-dialog v-model="resultVisible" title="密钥对生成成功" width="640px">
      <el-alert
        type="success"
        :closable="false"
        show-icon
        title="私钥已加密存入凭据保险库"
        description="私钥未在界面展示，可在列表中点击「查看」解密查看。"
      />
      <div v-if="genResult" class="gen-result">
        <div class="gen-result-label">公钥</div>
        <div class="gen-result-pub">
          <el-input
            v-model="genResult.publicKey"
            type="textarea"
            readonly
            :autosize="{ minRows: 2, maxRows: 6 }"
          />
          <el-button :icon="'CopyDocument'" @click="copyPublicKey">复制公钥</el-button>
        </div>
        <div class="gen-result-label">指纹</div>
        <div class="gen-result-fp">{{ genResult.fingerprint }}</div>
        <div class="gen-result-tip">
          将公钥追加到目标服务器 <code>~/.ssh/authorized_keys</code>
          后，即可在会话配置中使用该私钥免密登录。
        </div>
      </div>
      <template #footer>
        <el-button type="primary" @click="resultVisible = false">完成</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped lang="scss">
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
.header-actions {
  display: flex;
  gap: 8px;
}
.gen-hint {
  width: 100%;
  font-size: 12px;
  color: var(--el-text-color-secondary);
  line-height: 1.5;
  margin-top: 4px;
}
.gen-result {
  margin-top: 16px;
}
.gen-result-label {
  font-size: 13px;
  font-weight: 600;
  color: var(--el-text-color-primary);
  margin-bottom: 6px;
}
.gen-result-pub {
  display: flex;
  align-items: flex-start;
  gap: 8px;
}
.gen-result-pub .el-textarea {
  flex: 1;
}
.gen-result-fp {
  font-family: Consolas, Monaco, monospace;
  font-size: 13px;
  color: var(--el-text-color-primary);
  margin-bottom: 12px;
}
.gen-result-tip {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  line-height: 1.6;
}
.gen-result-tip code {
  background: var(--el-fill-color-light);
  padding: 1px 5px;
  border-radius: 3px;
  font-family: Consolas, Monaco, monospace;
}
</style>
