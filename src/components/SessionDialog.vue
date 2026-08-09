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

/** 由会话数据反推 UI 认证方式（与 resetFromProps 中的推断保持一致）。 */
function inferFormAuth(s: Session): FormAuth {
  if (s.authType === AuthType.Password) return "password";
  return s.keyPath ? "keyFile" : "keyText";
}

/** 编辑时用户是否切换了认证方式（切换后必须提供新凭据，不能再"留空不变"）。 */
const authChanged = computed(() => {
  if (!props.session) return false;
  return inferFormAuth(props.session) !== form.auth;
});

/** 原认证方式对应的凭据 id（按类型归属，避免跨类型复用 credentialId）。
 *  编辑时用于原地更新（传 id upsert）或清理。 */
const origPasswordId = computed(() => {
  const s = props.session;
  return s && s.authType === AuthType.Password ? s.credentialId : null;
});
const origKeyId = computed(() => {
  const s = props.session;
  return s && s.authType === AuthType.PrivateKey ? s.credentialId : null;
});

const rules: FormRules<FormState> = {
  name: [{ required: true, message: "请输入名称", trigger: "blur" }],
  host: [{ required: true, message: "请输入主机", trigger: "blur" }],
  port: [{ required: true, message: "请输入端口", trigger: "blur" }],
  username: [{ required: true, message: "请输入用户名", trigger: "blur" }],
  password: [
    {
      validator: (_r, _v, cb) => {
        if (
          form.auth === "password" &&
          !form.password &&
          (!isEdit.value || authChanged.value)
        ) {
          cb(new Error(authChanged.value ? "切换认证方式后请填写新密码" : "请输入密码"));
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
        if (
          form.auth === "keyText" &&
          !form.keyText.trim() &&
          (!isEdit.value || authChanged.value)
        ) {
          cb(new Error(authChanged.value ? "切换认证方式后请粘贴新的私钥内容" : "请粘贴私钥内容"));
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
        if (
          form.auth === "keyFile" &&
          !form.keyPath &&
          (!isEdit.value || authChanged.value)
        ) {
          cb(new Error(authChanged.value ? "切换认证方式后请选择私钥文件" : "请选择私钥文件"));
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

/** 删除旧凭据（切换认证方式后不再使用；删除失败不阻塞保存）。 */
async function deleteCredentialQuiet(id: string | null) {
  if (!id) return;
  try {
    await credentialDelete(id);
  } catch {
    /* 忽略删除失败，避免遗留凭据时阻塞保存 */
  }
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
      if (form.password) {
        // 填了新密码 → 原地更新原密码凭据（传 id 不产生孤儿）；若原先是密钥
        // 认证（切换到密码），新建后清掉遗留的密钥凭据。
        credentialId = await credentialSave({
          id: origPasswordId.value ?? undefined,
          name: `${form.name} · password`,
          kind: KIND_PASSWORD,
          value: form.password,
        });
        await deleteCredentialQuiet(origKeyId.value);
      } else {
        // 编辑留空表示不修改（切换认证方式后留空已被表单校验拦截）。
        credentialId = origPasswordId.value;
      }
    } else if (form.auth === "keyText") {
      if (form.keyText.trim()) {
        // 同上：原地更新原密钥凭据；从密码切换到文本密钥时清掉密码凭据。
        credentialId = await credentialSave({
          id: origKeyId.value ?? undefined,
          name: `${form.name} · private_key`,
          kind: KIND_PRIVATE_KEY_TEXT,
          value: form.keyText,
          passphrase: form.passphrase || undefined,
        });
        await deleteCredentialQuiet(origPasswordId.value);
      } else {
        credentialId = origKeyId.value;
      }
    } else if (form.auth === "keyFile") {
      keyPath = form.keyPath || null;
      // 文件密钥不关联 vault 凭据：清掉原密码/文本密钥凭据，避免遗留。
      await deleteCredentialQuiet(origPasswordId.value ?? origKeyId.value);
      credentialId = null;
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
      </template>

      <template v-else>
        <el-form-item label="私钥内容" prop="keyText">
          <el-input
            v-model="form.keyText"
            type="textarea"
            :rows="5"
            :placeholder="isEdit && !authChanged ? '留空表示不修改' : '-----BEGIN OPENSSH PRIVATE KEY-----\n...'"
          />
        </el-form-item>
        <el-form-item label="口令">
          <el-input
            v-model="form.passphrase"
            type="password"
            show-password
            placeholder="私钥加密时所需口令（仅在粘贴新私钥时生效）"
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
