<!--
  SkillManagerDialog.vue — 某个 domain 的 skill 管理列表

  列出该 domain 下所有 skill，支持：启停切换、编辑、删除。
  新建走 SkillDialog（由 AiPanel 的「总结」按钮触发，这里不负责新建）。
-->
<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { Edit, Delete, Plus } from "@element-plus/icons-vue";
import { useSettingsStore } from "@/stores/settings";
import type { SkillConfig } from "@/api/types";
import SkillDialog from "@/components/SkillDialog.vue";

const props = defineProps<{
  visible: boolean;
  /** 管理哪个 domain 的 skill。 */
  domain: "ssh" | "db";
}>();

const emit = defineEmits<{
  (e: "update:visible", v: boolean): void;
}>();

const settings = useSettingsStore();

/** 该 domain 的 skill 列表（过滤）。 */
const list = computed(() => settings.skills.filter((s) => s.domain === props.domain));

/** 当前编辑的 skill（SkillDialog 用）。 */
const editingSkill = ref<SkillConfig | null>(null);
const skillDialogVisible = ref(false);

function close() {
  emit("update:visible", false);
}

function toggleEnabled(s: SkillConfig) {
  settings.toggleSkill(s.id);
  settings.save().catch(() => {});
}

function editSkill(s: SkillConfig) {
  editingSkill.value = s;
  skillDialogVisible.value = true;
}

function newSkill() {
  editingSkill.value = null;
  skillDialogVisible.value = true;
}

async function onSkillSaved(s: SkillConfig) {
  if (s.id) {
    // 编辑：更新已有。
    settings.updateSkill(s.id, {
      title: s.title,
      content: s.content,
      enabled: s.enabled,
    });
  } else {
    // 新建。
    settings.addSkill({
      title: s.title,
      content: s.content,
      enabled: s.enabled,
      domain: props.domain,
    });
  }
  await settings.save().catch(() => {});
}

async function removeSkill(s: SkillConfig) {
  try {
    await ElMessageBox.confirm(`确认删除技能「${s.title}」？`, "删除", { type: "warning" });
  } catch {
    return;
  }
  settings.removeSkill(s.id);
  await settings.save().catch(() => {});
  ElMessage.success("已删除");
}
</script>

<template>
  <el-dialog
    :model-value="visible"
    :title="`技能管理 · ${domain === 'ssh' ? '终端助手' : 'SQL 助手'}`"
    width="600px"
    append-to-body
    @update:model-value="close"
  >
    <div class="skill-list">
      <div v-if="list.length === 0" class="empty">
        暂无技能。点击对话中的「总结成技能」按钮，或在下方新建。
      </div>
      <div v-for="s in list" :key="s.id" class="skill-row">
        <div class="skill-info">
          <div class="skill-title">
            <span :class="{ disabled: !s.enabled }">{{ s.title }}</span>
          </div>
          <div class="skill-content">{{ s.content.slice(0, 80) }}{{ s.content.length > 80 ? "…" : "" }}</div>
        </div>
        <div class="skill-actions">
          <el-switch
            :model-value="s.enabled"
            size="small"
            @change="toggleEnabled(s)"
          />
          <el-button link size="small" @click="editSkill(s)">
            <el-icon><Edit /></el-icon>
          </el-button>
          <el-button link size="small" type="danger" @click="removeSkill(s)">
            <el-icon><Delete /></el-icon>
          </el-button>
        </div>
      </div>
    </div>
    <template #footer>
      <el-button :icon="Plus" @click="newSkill">新建技能</el-button>
      <el-button @click="close">关闭</el-button>
    </template>

    <SkillDialog
      v-model:visible="skillDialogVisible"
      :skill="editingSkill"
      :domain="domain"
      @saved="onSkillSaved"
    />
  </el-dialog>
</template>

<style scoped>
.skill-list {
  max-height: 400px;
  overflow-y: auto;
}
.empty {
  padding: 24px;
  text-align: center;
  color: var(--el-text-color-secondary);
  font-size: 13px;
}
.skill-row {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 8px;
  border-bottom: 1px solid var(--el-border-color-lighter);
}
.skill-row:hover {
  background: var(--el-fill-color-light);
}
.skill-info {
  flex: 1;
  min-width: 0;
}
.skill-title {
  font-size: 13px;
  font-weight: 500;
}
.skill-title .disabled {
  color: var(--el-text-color-placeholder);
  text-decoration: line-through;
}
.skill-content {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  margin-top: 2px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.skill-actions {
  display: flex;
  align-items: center;
  gap: 4px;
  flex-shrink: 0;
}
</style>
