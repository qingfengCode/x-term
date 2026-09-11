<!--
  DbProfileDialog.vue — 数据库 profile（MySQL / PostgreSQL）新建 / 编辑表单

  字段：类型 / 名称 / 主机 / 端口 / 用户名 / 密码 / 默认数据库 / SSH 隧道（含会话选择）。
  密码以 credential（kind="db_password"）形式保存到保险库，profile 只持有 credentialId。
  编辑时密码留空表示不修改原密码。
-->
<script setup lang="ts">
import { computed, reactive, ref, watch } from "vue";
import { ElMessage } from "element-plus";
import type { FormInstance, FormRules } from "element-plus";
import { useSessionsStore } from "@/stores/sessions";
import { normalizeKind } from "@/stores/db";
import { credentialDelete, credentialSave } from "@/api/vault";
import { dbSaveProfile, dbListGroups } from "@/api/db";
import type { DbGroup, DbKind, DbProfile, Session } from "@/api/types";

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

/** 与后端保险库约定的数据库密码 credential kind（历史 mysql_password 仍可解密）。 */
const KIND_DB_PASSWORD = "db_password";

/** 数据库类型选项（value 与后端 normalize_kind 归一化值一致）。 */
const KIND_OPTIONS: { label: string; value: DbKind; port: number; user: string }[] = [
  { label: "MySQL", value: "mysql", port: 3306, user: "root" },
  { label: "PostgreSQL", value: "postgres", port: 5432, user: "postgres" },
  { label: "SQLite（本地文件）", value: "sqlite", port: 0, user: "" },
];

function kindMeta(kind: DbKind) {
  return KIND_OPTIONS.find((o) => o.value === kind) ?? KIND_OPTIONS[0];
}

/** SQLite 是本地文件连接：无端口/用户/密码/SSH 隧道概念。 */
const isSqlite = computed(() => form.kind === "sqlite");

interface FormState {
  kind: DbKind;
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
    kind: "mysql",
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

/** 切换数据库类型：端口 / 用户名若仍是另一类型的默认值则联动更新。 */
watch(
  () => form.kind,
  (kind, old) => {
    if (kind === old) return;
    const next = kindMeta(kind);
    const prev = kindMeta(old);
    if (form.port === prev.port) form.port = next.port;
    if (form.username === prev.user) form.username = next.user;
  }
);

const isEdit = computed(() => !!props.profile);

const dialogTitle = computed(() => (isEdit.value ? "编辑数据库连接" : "新建数据库连接"));

const formRules: FormRules = {
  name: [{ required: true, message: "请输入名称", trigger: "blur" }],
  host: [
    {
      required: true,
      // SQLite 填文件路径，其余填主机地址。
      message: "请输入主机地址（SQLite 填数据库文件路径）",
      trigger: "blur",
    },
  ],
  port: [
    // SQLite 无端口概念，跳过校验。
    {
      validator: (_rule, value, callback) => {
        if (isSqlite.value || (value >= 1 && value <= 65535)) callback();
        else callback(new Error("端口范围 1-65535"));
      },
      trigger: "blur",
    },
  ],
  username: [
    {
      validator: (_rule, value, callback) => {
        if (isSqlite.value || value.trim()) callback();
        else callback(new Error("请输入用户名"));
      },
      trigger: "blur",
    },
  ],
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
        kind: normalizeKind(props.profile.kind),
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
  // 本次新建的凭据 id（编辑已有凭据时为 null）：账号保存失败时补偿删除，避免孤儿。
  let createdCredId: string | null = null;
  try {
    const base = props.profile;
    let credentialId = base?.credentialId ?? null;

    // 密码非空 → 保存（更新）为 credential。编辑时传原 id 原地更新，
    // 不再每次新建 credential（旧实现每次修改密码都新增一条、旧凭据永不删除）。
    const pwd = form.password;
    if (pwd) {
      credentialId = await credentialSave({
        id: base?.credentialId ?? undefined,
        name: `db:${form.name}`,
        kind: KIND_DB_PASSWORD,
        value: pwd,
      });
      if (!base?.credentialId) createdCredId = credentialId;
    }

    const profile: DbProfile = {
      id: base?.id ?? genId(),
      name: form.name.trim(),
      kind: form.kind,
      host: form.host.trim(),
      port: Number(form.port),
      username: form.username.trim(),
      defaultDatabase: form.defaultDatabase.trim() || null,
      credentialId,
      // SQLite 是本地文件：SSH 开关已随 v-if 隐藏，但表单值可能残留
      //（先选 MySQL 开了隧道再切 SQLite），提交时强制清空。
      sshSessionConfigId:
        !isSqlite.value && form.useSshTunnel ? form.sshSessionId : null,
      groupId: form.groupId || null,
      createdAt: base?.createdAt ?? new Date().toISOString(),
    };

    await dbSaveProfile(profile);
    ElMessage.success(isEdit.value ? "已保存修改" : "已新建数据库连接");
    emit("saved", profile);
    close();
  } catch (e: unknown) {
    // 新建场景下账号保存失败：补偿删除刚创建的凭据，避免孤儿。
    if (createdCredId) {
      await credentialDelete(createdCredId).catch(() => {});
    }
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
    append-to-body
    @update:model-value="emit('update:visible', $event)"
  >
    <el-form
      ref="formRef"
      :model="form"
      :rules="formRules"
      label-width="92px"
      label-position="right"
    >
      <el-form-item label="类型">
        <el-select v-model="form.kind" style="width: 100%">
          <el-option
            v-for="o in KIND_OPTIONS"
            :key="o.value"
            :label="o.label"
            :value="o.value"
          />
        </el-select>
      </el-form-item>

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

      <el-form-item :label="isSqlite ? '文件路径' : '主机'" prop="host">
        <el-input
          v-model="form.host"
          :placeholder="isSqlite ? '例如：D:\\data\\app.db（不存在会自动创建）' : '127.0.0.1'"
        />
      </el-form-item>

      <el-form-item v-if="!isSqlite" label="端口" prop="port">
        <el-input-number
          v-model="form.port"
          :min="1"
          :max="65535"
          controls-position="right"
          style="width: 100%"
        />
      </el-form-item>

      <el-form-item v-if="!isSqlite" label="用户名" prop="username">
        <el-input v-model="form.username" placeholder="root" />
      </el-form-item>

      <el-form-item v-if="!isSqlite" label="密码" prop="password">
        <el-input
          v-model="form.password"
          type="password"
          show-password
          :placeholder="isEdit ? '留空表示不修改' : '输入密码'"
        />
      </el-form-item>

      <el-form-item v-if="!isSqlite" label="默认数据库">
        <el-input v-model="form.defaultDatabase" placeholder="可选，如 app_db" />
      </el-form-item>

      <el-form-item v-if="!isSqlite" label="SSH 隧道">
        <el-switch v-model="form.useSshTunnel" />
        <span class="form-hint">通过 SSH 会话连接数据库</span>
      </el-form-item>

      <el-form-item v-if="!isSqlite && form.useSshTunnel" label="SSH 会话" prop="sshSessionId">
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

<style scoped lang="scss">
.form-hint {
  margin-left: 12px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
</style>
