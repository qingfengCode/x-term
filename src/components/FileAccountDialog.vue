<!--
  FileAccountDialog.vue — S3 / 兼容存储账号 新建 / 编辑表单

  字段：名称 / Endpoint / Region / Bucket / AccessKey / SecretKey。
  凭据（access_key + secret_key）以 credential（kind="s3_credential"）形式保存到
  保险库，value 字段存 JSON 字符串 {"access_key":"...","secret_key":"..."}；账号只持有 credentialId。
  编辑时凭据留空表示不修改原凭据。
-->
<script setup lang="ts">
import { computed, reactive, ref, watch } from "vue";
import { ElMessage } from "element-plus";
import type { FormInstance, FormRules } from "element-plus";
import { credentialSave } from "@/api/vault";
import { fileAccountSave } from "@/api/fileBackend";
import type { FileAccount } from "@/api/fileBackend";

const props = withDefaults(
  defineProps<{
    visible: boolean;
    /** 传入则编辑，否则新建。 */
    account?: FileAccount | null;
  }>(),
  { account: null },
);

const emit = defineEmits<{
  (e: "update:visible", v: boolean): void;
  (e: "saved", a: FileAccount): void;
}>();

/** 与后端保险库约定的 S3 凭据 credential kind（见 file_accounts_repo::fetch_s3_credential）。 */
const KIND_S3_CREDENTIAL = "s3_credential";

interface FormState {
  name: string;
  endpoint: string;
  region: string;
  bucket: string;
  pathStyle: boolean;
  accessKey: string;
  secretKey: string;
}

const formRef = ref<FormInstance>();
const saving = ref(false);

function emptyForm(): FormState {
  return {
    name: "",
    endpoint: "https://s3.amazonaws.com",
    region: "us-east-1",
    bucket: "",
    pathStyle: true,
    accessKey: "",
    secretKey: "",
  };
}

const form = reactive<FormState>(emptyForm());

const isEdit = computed(() => !!props.account);
const dialogTitle = computed(() => (isEdit.value ? "编辑文件存储账号" : "新建文件存储账号"));

/**
 * TOS（火山引擎）原生域名提醒。
 * 原生接口（tos-<region>.volces.com）使用 TOS4-HMAC-SHA256 签名，与 S3 客户端
 * 的 AWS SigV4 不兼容，必须改用 S3 兼容域名 tos-s3-<region>.volces.com。
 */
const tosEndpointHint = computed(() => {
  const ep = form.endpoint.trim().toLowerCase();
  if (!ep.includes(".volces.com") || ep.includes("tos-s3")) return "";
  return "检测到 TOS 原生域名（tos-xxx.volces.com）：原生接口使用 TOS4 签名，S3 客户端连不上（会报权限不足）。请改用 S3 兼容域名 tos-s3-<region>.volces.com（如 tos-s3-cn-beijing.volces.com），Region 填对应代码（如 cn-beijing），并保持 Path-style。";
});

const formRules: FormRules = {
  name: [{ required: true, message: "请输入名称", trigger: "blur" }],
  endpoint: [{ required: true, message: "请输入 Endpoint", trigger: "blur" }],
  bucket: [{ required: true, message: "请输入 Bucket", trigger: "blur" }],
};

/** 弹窗显示时根据 account 初始化表单。 */
watch(
  () => props.visible,
  (v) => {
    if (!v) return;
    if (props.account) {
      // 编辑模式：凭据留空（保持不变）。
      Object.assign(form, {
        name: props.account.name,
        endpoint: props.account.endpoint,
        region: props.account.region,
        bucket: props.account.bucket,
        pathStyle: props.account.pathStyle,
        accessKey: "",
        secretKey: "",
      });
    } else {
      Object.assign(form, emptyForm());
    }
    formRef.value?.clearValidate();
  },
);

function genId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return "s3_" + Date.now().toString(36) + Math.random().toString(36).slice(2, 8);
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
    const base = props.account;
    let credentialId = base?.credentialId ?? null;

    // 凭据非空 → 保存（更新）为 credential。
    // value 存 JSON 字符串 {access_key, secret_key}，后端 fetch_s3_credential 据此解析。
    const ak = form.accessKey.trim();
    const sk = form.secretKey.trim();
    if (!!ak !== !!sk) {
      // 只填了其中一个：无论新建还是编辑，单边更新都会被静默忽略
      // （后端按 credentialId 整块替换凭据），必须成对填写或都留空。
      ElMessage.error("AccessKey 与 SecretKey 必须同时填写，或同时留空");
      return;
    }
    if (ak && sk) {
      credentialId = await credentialSave({
        name: `s3:${form.name.trim()}`,
        kind: KIND_S3_CREDENTIAL,
        value: JSON.stringify({ access_key: ak, secret_key: sk }),
      });
    } else if (!credentialId) {
      // 新建且未填凭据：保存出来的账号第一次访问必失败（后端拿不到 AK/SK）。
      ElMessage.error("请填写 AccessKey 与 SecretKey");
      return;
    }

    const now = new Date().toISOString();
    const account: FileAccount = {
      id: base?.id ?? genId(),
      name: form.name.trim(),
      kind: base?.kind ?? "s3",
      endpoint: form.endpoint.trim(),
      region: form.region.trim() || "us-east-1",
      bucket: form.bucket.trim(),
      credentialId,
      pathStyle: form.pathStyle,
      sortOrder: base?.sortOrder ?? 0,
      createdAt: base?.createdAt ?? now,
      updatedAt: now,
    };

    await fileAccountSave(account);
    ElMessage.success(isEdit.value ? "已保存修改" : "已新建文件存储账号");
    emit("saved", account);
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
    append-to-body
    @update:model-value="close"
  >
    <el-form ref="formRef" :model="form" :rules="formRules" label-width="92px">
      <el-form-item label="名称" prop="name">
        <el-input v-model="form.name" placeholder="如 生产环境 / 备份桶" />
      </el-form-item>
      <el-form-item label="Endpoint" prop="endpoint">
        <el-input
          v-model="form.endpoint"
          placeholder="https://s3.amazonaws.com 或 https://minio.local:9000"
        />
        <div v-if="tosEndpointHint" class="warn-text">{{ tosEndpointHint }}</div>
      </el-form-item>
      <el-form-item label="Region">
        <el-input v-model="form.region" placeholder="us-east-1（MinIO 可填任意）" />
      </el-form-item>
      <el-form-item label="Bucket" prop="bucket">
        <el-input v-model="form.bucket" placeholder="bucket 名称" />
      </el-form-item>
      <el-form-item label="寻址风格">
        <el-switch
          v-model="form.pathStyle"
          active-text="Path-style"
          inactive-text="Virtual-hosted"
          inline-prompt
        />
        <div class="hint-text">
          Path-style：URL 为 <code>endpoint/bucket/key</code>（MinIO / 自建存储推荐）。
          Virtual-hosted：URL 为 <code>bucket.host/key</code>（AWS S3 默认）。带端口或路径前缀的 endpoint 请保持 Path-style。
        </div>
      </el-form-item>
      <el-form-item label="AccessKey">
        <el-input
          v-model="form.accessKey"
          :placeholder="isEdit ? '留空表示不修改原凭据' : '请输入 AccessKey'"
        />
      </el-form-item>
      <el-form-item label="SecretKey">
        <el-input
          v-model="form.secretKey"
          type="password"
          show-password
          :placeholder="isEdit ? '留空表示不修改原凭据' : '请输入 SecretKey'"
        />
      </el-form-item>
    </el-form>
    <template #footer>
      <el-button @click="close">取消</el-button>
      <el-button type="primary" :loading="saving" @click="submit">
        {{ isEdit ? "保存修改" : "新建账号" }}
      </el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.hint-text {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  line-height: 1.5;
  margin-top: 4px;
}
.hint-text code {
  background: var(--el-fill-color-light);
  padding: 1px 4px;
  border-radius: 3px;
  font-size: 11px;
}
.warn-text {
  font-size: 12px;
  color: #e6a23c;
  line-height: 1.5;
  margin-top: 4px;
}
</style>
