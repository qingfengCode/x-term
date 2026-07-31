<!--
  McpApprovalToast.vue — MCP 工具调用确认浮层

  外部 MCP 客户端（Claude Desktop / Cursor 等）调用 exec_ssh / exec_sql 时，
  后端不会自动执行，而是 emit `mcp:approval_request` 事件（见 mcp/approval.rs）。
  本组件订阅 store 中的 pendingApprovals 队列，以右下角浮层卡片形式逐条展示，
  用户点"允许/拒绝"后调用 store.respond → mcp_respond_approval，后端据此放行/拒绝。

  与 AI 助手的工具确认（AiPanel 内嵌卡片）不同：MCP 请求与当前页面无关，
  属全局事件，故用独立浮层而非对话内卡片。
-->
<script setup lang="ts">
import { useMcpStore } from "@/stores/mcp";
import { ElMessage } from "element-plus";
import { Check, Close } from "@element-plus/icons-vue";

const mcp = useMcpStore();

/** 该工具是否为危险操作（前端粗判，仅用于卡片高亮；后端不做拦截）。
 *  与 ai/tools.rs 的 is_dangerous 规则保持一致：危险命令/危险 SQL 红色高亮。 */
function isDangerous(toolName: string, args: Record<string, unknown>): boolean {
  if (toolName === "exec_ssh") {
    const cmd = String(args.command ?? "");
    return /(rm\s+-[a-z]*r[a-z]*f[a-z]*\s+\/|mkfs|dd\s+if=.*of=\/dev\/|shutdown|reboot|halt|poweroff|:\(\)\s*\{|chmod\s+-R\s+777\s+\/)/i.test(
      cmd
    );
  }
  if (toolName === "exec_sql") {
    const sql = String(args.sql ?? "");
    return /^\s*(DROP|TRUNCATE)\b/i.test(sql);
  }
  return false;
}

/** 解析参数为可读的键值对展示。 */
function argEntries(args: Record<string, unknown>): Array<[string, string]> {
  return Object.entries(args).map(([k, v]) => [
    k,
    typeof v === "string" ? v : JSON.stringify(v),
  ]);
}

/** 简单的危险标识文案。 */
function dangerHint(toolName: string): string {
  if (toolName === "exec_ssh") return "该命令可能造成不可逆破坏，请谨慎确认。";
  if (toolName === "exec_sql") return "该 SQL 会修改数据库结构或数据，请谨慎确认。";
  return "";
}

async function approve(requestId: string) {
  try {
    await mcp.respond(requestId, true);
  } catch (e) {
    ElMessage.error("确认失败：" + String(e));
  }
}

async function reject(requestId: string) {
  try {
    await mcp.respond(requestId, false);
  } catch (e) {
    ElMessage.error("拒绝失败：" + String(e));
  }
}

function toolLabel(toolName: string): string {
  if (toolName === "exec_ssh") return "执行 SSH 命令";
  if (toolName === "exec_sql") return "执行 SQL";
  return toolName;
}

/** kind 中文标签。 */
function kindLabel(kind: string): string {
  return kind === "db" ? "DB MCP" : "SSH MCP";
}
</script>

<template>
  <TransitionGroup v-if="mcp.pendingApprovals.length" name="mcp-toast" tag="div" class="mcp-toast-wrap">
    <div
      v-for="req in mcp.pendingApprovals"
      :key="req.requestId"
      class="mcp-card"
      :class="{ dangerous: isDangerous(req.toolName, req.arguments) }"
    >
      <div class="card-head">
        <el-tag :type="isDangerous(req.toolName, req.arguments) ? 'danger' : 'warning'" size="small" effect="dark">
          {{ kindLabel(req.kind) }} 请求
        </el-tag>
        <span class="tool-name">{{ toolLabel(req.toolName) }}</span>
        <span v-if="req.resourceName" class="resource-name">· {{ req.resourceName }}</span>
        <span v-if="req.clientName" class="client-name">来自：{{ req.clientName }}</span>
      </div>

      <div class="card-desc">{{ req.description }}</div>

      <div class="card-args">
        <div v-for="[k, v] in argEntries(req.arguments)" :key="k" class="arg-row">
          <span class="arg-key">{{ k }}</span>
          <pre class="arg-val">{{ v }}</pre>
        </div>
      </div>

      <div v-if="isDangerous(req.toolName, req.arguments)" class="danger-hint">
        ⚠ {{ dangerHint(req.toolName) }}
      </div>

      <div class="card-actions">
        <el-button
          size="small"
          :type="isDangerous(req.toolName, req.arguments) ? 'danger' : 'primary'"
          :icon="Check"
          @click="approve(req.requestId)"
        >
          {{ isDangerous(req.toolName, req.arguments) ? "确认执行危险操作" : "允许" }}
        </el-button>
        <el-button size="small" :icon="Close" @click="reject(req.requestId)">拒绝</el-button>
      </div>
    </div>
  </TransitionGroup>
</template>

<style scoped>
.mcp-toast-wrap {
  position: fixed;
  right: 20px;
  bottom: 20px;
  z-index: 3000;
  display: flex;
  flex-direction: column-reverse; /* 最新的在最下方堆叠，靠近右下角 */
  gap: 12px;
  max-width: 380px;
  pointer-events: none;
}

.mcp-card {
  pointer-events: auto;
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color-light);
  border-left: 4px solid var(--el-color-warning);
  border-radius: 8px;
  padding: 12px 14px;
  box-shadow: var(--el-box-shadow-light);
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.mcp-card.dangerous {
  border-left-color: var(--el-color-danger);
}

.card-head {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.tool-name {
  font-weight: 600;
  font-size: 13px;
  color: var(--el-text-color-primary);
}
.client-name {
  margin-left: auto;
  font-size: 11px;
  color: var(--el-text-color-secondary);
}
.resource-name {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  font-weight: 500;
}

.card-desc {
  font-size: 13px;
  color: var(--el-text-color-regular);
  word-break: break-all;
}

.card-args {
  background: var(--el-fill-color-light);
  border-radius: 4px;
  padding: 6px 8px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  max-height: 160px;
  overflow: auto;
}
.arg-row {
  display: flex;
  gap: 8px;
  font-size: 12px;
  align-items: flex-start;
}
.arg-key {
  color: var(--el-color-primary);
  flex-shrink: 0;
  font-weight: 500;
  min-width: 70px;
}
.arg-val {
  margin: 0;
  font-family: "Consolas", "Cascadia Code", monospace;
  color: var(--el-text-color-regular);
  white-space: pre-wrap;
  word-break: break-all;
  flex: 1;
}

.danger-hint {
  font-size: 12px;
  color: var(--el-color-danger);
  background: var(--el-color-danger-light-9);
  border-radius: 4px;
  padding: 4px 6px;
}

.card-actions {
  display: flex;
  gap: 8px;
  justify-content: flex-end;
}

/* 进出过渡 */
.mcp-toast-enter-active,
.mcp-toast-leave-active {
  transition: all 0.25s ease;
}
.mcp-toast-enter-from {
  opacity: 0;
  transform: translateX(40px);
}
.mcp-toast-leave-to {
  opacity: 0;
  transform: translateX(40px);
}
.mcp-toast-move {
  transition: transform 0.25s ease;
}
</style>
