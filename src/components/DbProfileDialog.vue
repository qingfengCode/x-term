<!--
  DbProfileDialog.vue — MySQL 数据库 profile 新建 / 编辑表单

  字段：名称 / 主机 / 端口 / 用户名 / 密码 / 默认数据库 / SSH 隧道（含会话选择）。
  密码以 credential（kind="mysql_password"）形式保存到保险库，profile 只持有 credentialId。
  编辑时密码留空表示不修改原密码。
-->
<script setup lang="ts">
import { computed, reactive, ref, watch } from "vue";
import { ElMessage } from "element-plus";
import type { FormInstance, FormRules } from "element-plus";
import { useSessionsStore } from "@/stores/sessions";
import { credentialSave } from "@/api/vault";
import { dbSaveProfile, dbListGroups } from "@/api/db";
import type { DbGroup, DbProfile, Session } from "@/api/types";

const props = withDefaults(
  defineProps<{
    visible: boolean;
    /** 传入则编辑，否则新建。 */
    profile?: DbProfile | null;
    /** 新建时默认所属分组。 */
    defaultGroupId?: string | null;
  }>(),
  { profile: null, defaultGroupId: null }
);

const emit = defineEmits<{
  (e: "update:visible", v: boolean): void;
  (e: "saved", p: DbProfile): void;
}>();

const sessionsStore = useSessionsStore();

/** 与后端保险库约定的 MySQL 密码 credential kind。 */
const KIND_MYSQL_PASSWORD = "mysql_password";

interface FormState {
  name: string;
  host: string;
  port: number;
  username: string;
  password: string;
  defaultDatabase: string;
  useSshTunnel: boolean;
  sshSessionId: string | null;
  groupId: string | null;
}

const formRef = ref<FormInstance>();
const saving = ref(false);
const groups = ref<DbGroup[]>([]);

function emptyForm(): FormState {
  return {
    name: "",
    host: "127.0.0.1",
    port: 3306,
    username: "root",
    password: "",
    defaultDatabase: "",
    useSshTunnel: false,
    sshSessionId: null,
    groupId: null,
  };
}

const form = reactive<FormState>(emptyForm());

const isEdit = computed(() => !!props.profile);

const dialogTitle = computed(() => (isEdit.value ? "编辑数据库连接" : "新建数据库连接"));

const formRules: FormRules = {
  name: [{ required: true, message: "请输入名称", trigger: "blur" }],
  host: [{ required: true, message: "请输入主机地址", trigger: "blur" }],
  port: [
    { required: true, message: "请输入端口", trigger: "blur" },
    { type: "number", min: 1, max: 65535, message: "端口范围 1-65535", trigger: "blur" },
  ],
  username: [{ required: true, message: "请输入用户名", trigger: "blur" }],
  sshSessionId: [
    {
      validator: (_rule, value, callback) => {
        if (form.useSshTunnel && !value) {
          callback(new Error("开启 SSH 隧道后请选择会话"));
        } else {
          callback();
        }
      },
      trigger: "change",
    },
  ],
};

/** SSH 会话选项的展示文本：名称 (host:port)。 */
function sessionLabel(s: Session): string {
  return `${s.name} (${s.host}:${s.port})`;
}

/** 弹窗显示时根据 profile 初始化表单。 */
watch(
  () => props.visible,
  async (v) => {
    if (!v) return;
    // 加载分组列表。
    try {
      groups.value = await dbListGroups();
    } catch {
      groups.value = [];
    }
    if (props.profile) {
      // 编辑模式：密码留空（保持不变）。
      Object.assign(form, {
        name: props.profile.name,
        host: props.profile.host,
        port: props.profile.port,
        username: props.profile.username,
        password: "",
        defaultDatabase: props.profile.defaultDatabase ?? "",
        useSshTunnel: !!props.profile.sshSessionConfigId,
        sshSessionId: props.profile.sshSessionConfigId ?? null,
        groupId: props.profile.groupId ?? null,
      });
    } else {
      Object.assign(form, emptyForm());
      form.groupId = props.defaultGroupId ?? null;
    }
    // 清理上一次校验状态。
    formRef.value?.clearValidate();
  }
);

function genId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return "db_" + Date.now().toString(36) + Math.random().toString(36).slice(2, 8);
}

function close() {
  emit("update:visible", false);
}

async function submit() {
  if (!formRef.value) return;
  try {
    await formRef.value.validate();
  } catch {
    return;
  }
  saving.value = true;
  try {
    const base = props.profile;
    let credentialId = base?.credentialId ?? null;

    // 密码非空 → 保存（更新）为 credential。
    const pwd = form.password;
    if (pwd) {
      credentialId = await credentialSave({
        name: `mysql:${form.name}`,
        kind: KIND_MYSQL_PASSWORD,
        value: pwd,
      });
    }

    const profile: DbProfile = {
      id: base?.id ?? genId(),
      name: form.name.trim(),
      kind: base?.kind ?? "mysql",
      host: form.host.trim(),
      port: Number(form.port),
      username: form.username.trim(),
      defaultDatabase: form.defaultDatabase.trim() || null,
      credentialId,
      sshSessionConfigId: form.useSshTunnel ? form.sshSessionId : null,
      groupId: form.groupId || null,
      createdAt: base?.createdAt ?? new Date().toISOString(),
    };

    await dbSaveProfile(profile);
    ElMessage.success(isEdit.value ? "已保存修改" : "已新建数据库连接");
    emit("saved", profile);
    close();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    ElMessage.error("保存失败：" + msg);
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <el-dialog
    :model-value="visible"
    :title="dialogTitle"
    width="520px"
    :close-on-click-modal="false"
    @update:model-value="emit('update:visible', $event)"
  >
    <el-form
      ref="formRef"
      :model="form"
      :rules="formRules"
      label-width="100px"
      label-position="right"
    >
      <el-form-item label="名称" prop="name">
        <el-input v-model="form.name" placeholder="例如：生产库-主" />
      </el-form-item>

      <el-form-item label="分组">
        <el-select v-model="form.groupId" placeholder="无分组" clearable style="width: 100%">
          <el-option
            v-for="g in groups"
            :key="g.id"
            :label="g.name"
            :value="g.id"
          />
        </el-select>
      </el-form-item>

      <el-form-item label="主机" prop="host">
        <el-input v-model="form.host" placeholder="127.0.0.1" />
      </el-form-item>

      <el-form-item label="端口" prop="port">
        <el-input-number
          v-model="form.port"
          :min="1"
          :max="65535"
          controls-position="right"
          style="width: 100%"
        />
      </el-form-item>

      <el-form-item label="用户名" prop="username">
        <el-input v-model="form.username" placeholder="root" />
      </el-form-item>

      <el-form-item label="密码" prop="password">
        <el-input
          v-model="form.password"
          type="password"
          show-password
          :placeholder="isEdit ? '留空表示不修改' : '输入密码'"
        />
      </el-form-item>

      <el-form-item label="默认数据库">
        <el-input v-model="form.defaultDatabase" placeholder="可选，如 app_db" />
      </el-form-item>

      <el-form-item label="SSH 隧道">
        <el-switch v-model="form.useSshTunnel" />
        <span class="form-hint">通过 SSH 会话连接数据库</span>
      </el-form-item>

      <el-form-item v-if="form.useSshTunnel" label="SSH 会话" prop="sshSessionId">
        <el-select
          v-model="form.sshSessionId"
          placeholder="选择 SSH 会话"
          filterable
          style="width: 100%"
        >
          <el-option
            v-for="s in sessionsStore.sessions"
            :key="s.id"
            :label="sessionLabel(s)"
            :value="s.id"
          />
        </el-select>
      </el-form-item>
    </el-form>

    <template #footer>
      <el-button @click="close">取消</el-button>
      <el-button type="primary" :loading="saving" @click="submit">确定</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.form-hint {
  margin-left: 12px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
</style>
