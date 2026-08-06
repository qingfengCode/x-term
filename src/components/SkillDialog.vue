<!--
  SkillDialog.vue — 单条 skill 的编辑/查看弹窗

  字段：标题 / 内容（大文本框）/ 启用开关。domain 只读展示（由调用方决定，不可改）。
  新建时 skill=null；编辑时 skill=已有对象。保存后 emit saved。
-->
<script setup lang="ts">
import { computed, reactive, ref, watch } from "vue";
import { ElMessage } from "element-plus";
import type { FormInstance, FormRules } from "element-plus";
import type { SkillConfig } from "@/api/types";

const props = withDefaults(
  defineProps<{
    visible: boolean;
    /** 传入则编辑，否则新建。新建时 domain 必填。 */
    skill?: SkillConfig | null;
    /** 所属域："ssh" | "db"。新建时用；编辑时从 skill 取。 */
    domain?: "ssh" | "db";
  }>(),
  { skill: null, domain: "ssh" },
);

const emit = defineEmits<{
  (e: "update:visible", v: boolean): void;
  (e: "saved", s: SkillConfig): void;
}>();

interface FormState {
  title: string;
  content: string;
  enabled: boolean;
  domain: "ssh" | "db";
}

const formRef = ref<FormInstance>();
const form = reactive<FormState>({
  title: "",
  content: "",
  enabled: true,
  domain: "ssh",
});

const isEdit = computed(() => !!props.skill);
const dialogTitle = computed(() => (isEdit.value ? "编辑技能" : "保存为技能"));

const rules: FormRules = {
  title: [{ required: true, message: "请输入标题", trigger: "blur" }],
  content: [{ required: true, message: "请输入技能内容", trigger: "blur" }],
};

watch(
  () => props.visible,
  (v) => {
    if (!v) return;
    if (props.skill) {
      Object.assign(form, {
        title: props.skill.title,
        content: props.skill.content,
        enabled: props.skill.enabled,
        domain: props.skill.domain,
      });
    } else {
      Object.assign(form, {
        title: "",
        content: "",
        enabled: true,
        domain: props.domain,
      });
    }
    formRef.value?.clearValidate();
  },
);

function close() {
  emit("update:visible", false);
}

function submit() {
  formRef.value?.validate((ok) => {
    if (!ok) return;
    const result: SkillConfig = {
      id: props.skill?.id ?? "",
      title: form.title.trim(),
      content: form.content.trim(),
      enabled: form.enabled,
      domain: form.domain,
    };
    ElMessage.success(isEdit.value ? "技能已更新" : "技能已保存");
    emit("saved", result);
    close();
  });
}
</script>

<template>
  <el-dialog
    :model-value="visible"
    :title="dialogTitle"
    width="560px"
    :close-on-click-modal="false"
    append-to-body
    @update:model-value="close"
  >
    <el-form ref="formRef" :model="form" :rules="rules" label-width="72px">
      <el-form-item label="标题" prop="title">
        <el-input v-model="form.title" placeholder="如：MySQL 慢查询排查 / 磁盘清理流程" />
      </el-form-item>
      <el-form-item label="所属域">
        <el-tag size="small" :type="form.domain === 'ssh' ? 'primary' : 'success'">
          {{ form.domain === "ssh" ? "终端助手" : "SQL 助手" }}
        </el-tag>
      </el-form-item>
      <el-form-item label="内容" prop="content">
        <el-input
          v-model="form.content"
          type="textarea"
          :rows="10"
          placeholder="可直接作为系统提示词使用的技能内容（建议 < 500 字）。描述这类任务的标准处理步骤、注意事项、常用命令等。"
        />
      </el-form-item>
      <el-form-item label="启用">
        <el-switch v-model="form.enabled" />
        <span class="hint">启用后会自动注入到该助手对话的系统提示词中</span>
      </el-form-item>
    </el-form>
    <template #footer>
      <el-button @click="close">取消</el-button>
      <el-button type="primary" @click="submit">保存</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.hint {
  margin-left: 8px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
</style>
