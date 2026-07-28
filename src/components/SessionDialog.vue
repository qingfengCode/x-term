<script setup lang="ts">
import { computed, reactive, ref, watch } from "vue";
import type { FormInstance, FormRules } from "element-plus";
import { ElMessage } from "element-plus";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { useSessionsStore } from "@/stores/sessions";
import { credentialDelete, credentialSave } from "@/api/vault";
import { AuthType, type Session, type Protocol } from "@/api/types";

const props = withDefaults(
  defineProps<{
    visible: boolean;
    session?: Session | null;
    defaultGroupId?: string | null;
  }>(),
  { session: null, defaultGroupId: null }
);

const emit = defineEmits<{
  (e: "update:visible", v: boolean): void;
  (e: "saved", s: Session): void;
}>();

const sessionsStore = useSessionsStore();

/** 私钥文本认证对应的 kind（与后端 vault 约定）。 */
const KIND_PASSWORD = "password";
const KIND_PRIVATE_KEY_TEXT = "private_key_text";

/**
 * 表单使用的认证方式。UI 提供 3 个选项（密码 / 私钥文件 / 私钥文本），
 * 后端 AuthType 只有 Password/PrivateKey/Agent，文件与文本都映射为 PrivateKey。
 */
type FormAuth = "password" | "keyFile" | "keyText";

interface FormState {
  name: string;
  groupId: string | null;
  protocol: Protocol;
  host: string;
  port: number;
  username: string;
  auth: FormAuth;
  password: string;
  keyPath: string;
  keyText: string;
  passphrase: string;
  startupScript: string;
  color: string | null;
}

const formRef = ref<FormInstance>();
const saving = ref(false);

const form = reactive<FormState>({
  name: "",
  groupId: null,
  protocol: "ssh",
  host: "127.0.0.1",
  port: 22,
  username: "root",
  auth: "password",
  password: "",
  keyPath: "",
  keyText: "",
  passphrase: "",
  startupScript: "",
  color: null,
});

// 协议选项 + 默认端口。
const PROTOCOL_OPTIONS: { value: Protocol; label: string; port: number }[] = [
  { value: "ssh", label: "SSH", port: 22 },
  { value: "telnet", label: "Telnet", port: 23 },
  { value: "rdp", label: "RDP (Windows 桌面)", port: 3389 },
  { value: "vnc", label: "VNC", port: 5900 },
];
// 切换协议时自动调默认端口 + 限制认证方式。
watch(
  () => form.protocol,
  (proto, old) => {
    if (proto === old) return;
    const opt = PROTOCOL_OPTIONS.find((o) => o.value === old);
    // 仅当当前端口是旧协议默认端口时才改（避免覆盖用户自定义端口）。
    if (opt && form.port === opt.port) {
      const next = PROTOCOL_OPTIONS.find((o) => o.value === proto);
      if (next) form.port = next.port;
    }
    // telnet/rdp/vnc 只支持密码认证。
    if (proto !== "ssh") form.auth = "password";
  },
);
/** 当前协议是否支持密钥认证（仅 SSH）。 */
const supportsKeyAuth = computed(() => form.protocol === "ssh");

const isEdit = computed(() => !!props.session);
const title = computed(() => (isEdit.value ? "编辑会话" : "新建会话"));

const rules: FormRules<FormState> = {
  name: [{ required: true, message: "请输入名称", trigger: "blur" }],
  host: [{ required: true, message: "请输入主机", trigger: "blur" }],
  port: [{ required: true, message: "请输入端口", trigger: "blur" }],
  username: [{ required: true, message: "请输入用户名", trigger: "blur" }],
  password: [
    {
      validator: (_r, _v, cb) => {
        if (form.auth === "password" && !form.password && !isEdit.value) {
          cb(new Error("请输入密码"));
        } else {
          cb();
        }
      },
      trigger: "blur",
    },
  ],
  keyText: [
    {
      validator: (_r, _v, cb) => {
        if (form.auth === "keyText" && !form.keyText.trim() && !isEdit.value) {
          cb(new Error("请粘贴私钥内容"));
        } else {
          cb();
        }
      },
      trigger: "blur",
    },
  ],
  keyPath: [
    {
      validator: (_r, _v, cb) => {
        if (form.auth === "keyFile" && !form.keyPath && !isEdit.value) {
          cb(new Error("请选择私钥文件"));
        } else {
          cb();
        }
      },
      trigger: "change",
    },
  ],
};

const groupOptions = computed(() => sessionsStore.groups);

/** visible 或 session 变化时重置表单。 */
watch(
  () => [props.visible, props.session],
  ([vis]) => {
    if (!vis) return;
    resetFromProps();
  },
  { immediate: true }
);

function resetFromProps() {
  const s = props.session;
  if (s) {
    form.name = s.name;
    form.groupId = s.groupId;
    form.protocol = s.protocol ?? "ssh";
    form.host = s.host;
    form.port = s.port;
    form.username = s.username;
    form.startupScript = s.startupScript ?? "";
    form.color = s.color;
    form.password = "";
    form.keyText = "";
    form.passphrase = "";
    form.keyPath = s.keyPath ?? "";
    // 反推 UI 认证方式。
    if (s.authType === AuthType.Password) {
      form.auth = "password";
    } else if (s.keyPath) {
      form.auth = "keyFile";
    } else {
      form.auth = "keyText";
    }
  } else {
    form.name = "";
    form.groupId = props.defaultGroupId ?? null;
    form.protocol = "ssh";
    form.host = "127.0.0.1";
    form.port = 22;
    form.username = "root";
    form.auth = "password";
    form.password = "";
    form.keyPath = "";
    form.keyText = "";
    form.passphrase = "";
    form.startupScript = "";
    form.color = null;
  }
  formRef.value?.clearValidate();
}

async function chooseKeyFile() {
  try {
    const selected = await openDialog({
      title: "选择私钥文件",
      multiple: false,
      directory: false,
    });
    if (selected) {
      // openDialog 单选返回 string | null。
      form.keyPath = typeof selected === "string" ? selected : "";
      formRef.value?.validateField("keyPath");
    }
  } catch (e) {
    ElMessage.error("选择文件失败: " + String(e));
  }
}

function mapAuthType(a: FormAuth): AuthType {
  // 文件与文本私钥均映射为 PrivateKey。
  return a === "password" ? AuthType.Password : AuthType.PrivateKey;
}

function close() {
  emit("update:visible", false);
}

async function handleSave() {
  if (!formRef.value) return;
  const valid = await formRef.value.validate().catch(() => false);
  if (!valid) return;

  saving.value = true;
  try {
    const now = new Date().toISOString();
    const existed = props.session;
    const id = existed?.id ?? crypto.randomUUID();

    let credentialId = existed?.credentialId ?? null;
    let keyPath: string | null = null;

    if (form.auth === "password") {
      // 仅在输入了新密码时更新凭据；编辑时留空表示不变。
      if (form.password) {
        credentialId = await credentialSave({
          name: `${form.name} · password`,
          kind: KIND_PASSWORD,
          value: form.password,
        });
      }
    } else if (form.auth === "keyText") {
      if (form.keyText.trim()) {
        credentialId = await credentialSave({
          name: `${form.name} · private_key`,
          kind: KIND_PRIVATE_KEY_TEXT,
          value: form.keyText,
          passphrase: form.passphrase || undefined,
        });
      }
    } else if (form.auth === "keyFile") {
      keyPath = form.keyPath || null;
      // 切换到文件方式时，旧文本凭据不再使用——删除以避免遗留。
      if (existed?.credentialId) {
        try {
          await credentialDelete(existed.credentialId);
        } catch {
          /* 忽略删除失败 */
        }
        credentialId = null;
      }
    }

    const session: Session = {
      id,
      name: form.name.trim(),
      groupId: form.groupId || null,
      host: form.host.trim(),
      port: Number(form.port) || 22,
      username: form.username.trim(),
      authType: mapAuthType(form.auth),
      credentialId,
      keyPath,
      jumpSessionId: existed?.jumpSessionId ?? null,
      startupScript: form.startupScript.trim() || null,
      tags: existed?.tags ?? null,
      color: form.color || null,
      sortOrder: existed?.sortOrder ?? 0,
      createdAt: existed?.createdAt ?? now,
      updatedAt: now,
      protocol: form.protocol,
    };

    await sessionsStore.saveSession(session);
    emit("saved", session);
    ElMessage.success(isEdit.value ? "已更新会话" : "已创建会话");
    close();
  } catch (e) {
    ElMessage.error("保存失败: " + String(e));
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <el-dialog
    :model-value="visible"
    :title="title"
    width="560px"
    :close-on-click-modal="false"
    append-to-body
    @update:model-value="(v: boolean) => emit('update:visible', v)"
  >
    <el-form
      ref="formRef"
      :model="form"
      :rules="rules"
      label-width="92px"
      label-position="right"
      @submit.prevent
    >
      <el-form-item label="名称" prop="name">
        <el-input v-model="form.name" placeholder="例如：prod-web-01" clearable />
      </el-form-item>

      <el-form-item label="分组" prop="groupId">
        <el-select v-model="form.groupId" clearable placeholder="无分组" style="width: 100%">
          <el-option label="无分组" :value="null" />
          <el-option
            v-for="g in groupOptions"
            :key="g.id"
            :label="g.name"
            :value="g.id"
          />
        </el-select>
      </el-form-item>

      <el-form-item label="协议" prop="protocol">
        <el-select v-model="form.protocol" style="width: 100%">
          <el-option
            v-for="opt in PROTOCOL_OPTIONS"
            :key="opt.value"
            :label="opt.label"
            :value="opt.value"
          />
        </el-select>
      </el-form-item>

      <el-form-item label="主机" prop="host">
        <el-input v-model="form.host" placeholder="127.0.0.1 或域名" clearable />
      </el-form-item>

      <el-form-item label="端口" prop="port">
        <el-input-number v-model="form.port" :min="1" :max="65535" controls-position="right" />
      </el-form-item>

      <el-form-item label="用户名" prop="username">
        <el-input v-model="form.username" placeholder="root" clearable />
      </el-form-item>

      <el-form-item label="认证方式" prop="auth">
        <el-radio-group v-model="form.auth">
          <el-radio-button value="password">密码</el-radio-button>
          <el-radio-button v-if="supportsKeyAuth" value="keyFile">私钥文件</el-radio-button>
          <el-radio-button v-if="supportsKeyAuth" value="keyText">私钥文本</el-radio-button>
        </el-radio-group>
      </el-form-item>

      <el-form-item v-if="form.auth === 'password'" label="密码" prop="password">
        <el-input
          v-model="form.password"
          type="password"
          show-password
          :placeholder="isEdit ? '留空表示不修改' : '请输入密码'"
        />
      </el-form-item>

      <template v-else-if="form.auth === 'keyFile'">
        <el-form-item label="私钥文件" prop="keyPath">
          <el-input v-model="form.keyPath" placeholder="点击右侧选择文件" readonly>
            <template #append>
              <el-button @click="chooseKeyFile">选择</el-button>
            </template>
          </el-input>
        </el-form-item>
        <el-form-item label="口令">
          <el-input
            v-model="form.passphrase"
            type="password"
            show-password
            placeholder="可选"
          />
        </el-form-item>
      </template>

      <template v-else>
        <el-form-item label="私钥内容" prop="keyText">
          <el-input
            v-model="form.keyText"
            type="textarea"
            :rows="5"
            placeholder="-----BEGIN OPENSSH PRIVATE KEY-----&#10;..."
          />
        </el-form-item>
        <el-form-item label="口令">
          <el-input
            v-model="form.passphrase"
            type="password"
            show-password
            placeholder="可选"
          />
        </el-form-item>
      </template>

      <el-form-item label="启动脚本">
        <el-input
          v-model="form.startupScript"
          type="textarea"
          :rows="3"
          placeholder="连接后自动执行的命令（可选）"
        />
      </el-form-item>

      <el-form-item label="颜色标签">
        <el-color-picker v-model="form.color" show-alpha />
        <el-button
          v-if="form.color"
          link
          type="info"
          style="margin-left: 8px"
          @click="form.color = null"
        >
          清除
        </el-button>
      </el-form-item>
    </el-form>

    <template #footer>
      <el-button @click="close">取消</el-button>
      <el-button type="primary" :loading="saving" @click="handleSave">保存</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
:deep(.el-input-number) {
  width: 100%;
}
:deep(.el-textarea__inner) {
  font-family: "JetBrains Mono", "Cascadia Code", Consolas, monospace;
  font-size: 12px;
}
</style>
