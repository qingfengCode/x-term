<script setup lang="ts">
/**
 * SSH 主机公钥变更确认弹窗。
 *
 * 全局监听 `ssh:host_key_challenge` 事件（后端 check_server_key 检测到主机公钥
 * 与 known_hosts 记录不符时发出），弹出确认框展示新旧指纹对比，让用户选择：
 * - 接受并更新（写入新指纹到 known_hosts）
 * - 仅本次接受（本次连接放行，不更新记录）
 * - 拒绝（终止连接）
 *
 * 多个确认（如快速连多个会话）按到达顺序排队串行处理。后端等待超时 120s，
 * 前端设置 125s 兜底计时器自动拒绝，防止弹窗悬挂。
 */
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { ElMessage } from "element-plus";
import {
  sshHostKeyRespond,
  type HostKeyDecision,
  type SshHostKeyEvent,
} from "@/api/session";

/** 后端等待超时 120s，前端兜底略长，到点自动拒绝。 */
const CHALLENGE_TIMEOUT_MS = 125_000;

interface QueuedChallenge {
  challenge: SshHostKeyEvent;
  /** 兜底计时器。 */
  timer: number;
}

const queue = ref<QueuedChallenge[]>([]);
const submitting = ref(false);
let unlisten: UnlistenFn | null = null;

const current = computed(() => queue.value[0] ?? null);
const visible = computed(() => queue.value.length > 0);

onMounted(async () => {
  unlisten = await listen<SshHostKeyEvent>("ssh:host_key_challenge", (e) => {
    void enqueue(e.payload);
  });
});

onBeforeUnmount(() => {
  unlisten?.();
  for (const item of queue.value) window.clearTimeout(item.timer);
});

function enqueue(challenge: SshHostKeyEvent) {
  const item: QueuedChallenge = { challenge, timer: 0 };
  // 兜底计时器必须拒绝**自己**这条挑战：队尾的挑战若先超时，不能去拒绝
  // 队首的（否则用户对 A 的决策会被 B 的超时抢跑）。
  item.timer = window.setTimeout(() => {
    void rejectItem(item);
  }, CHALLENGE_TIMEOUT_MS);
  queue.value.push(item);
}

/** 兜底超时：按 challengeId 拒绝指定的排队挑战并出队。 */
async function rejectItem(item: QueuedChallenge) {
  const idx = queue.value.indexOf(item);
  if (idx < 0) return;
  queue.value.splice(idx, 1);
  window.clearTimeout(item.timer);
  try {
    await sshHostKeyRespond(item.challenge.challengeId, "Reject");
  } catch {
    /* 后端可能已自行超时（返回 NotFound），忽略 */
  }
}

async function respondCurrent(decision: HostKeyDecision) {
  const item = current.value;
  if (!item || submitting.value) return;
  submitting.value = true;
  try {
    window.clearTimeout(item.timer);
    await sshHostKeyRespond(item.challenge.challengeId, decision);
    const idx = queue.value.indexOf(item);
    if (idx >= 0) queue.value.splice(idx, 1);
  } catch (e) {
    // 挑战可能已被后端判定超时（返回 NotFound），直接丢弃即可。
    ElMessage.error("主机公钥确认失败: " + String(e));
    const idx = queue.value.indexOf(item);
    if (idx >= 0) queue.value.splice(idx, 1);
  } finally {
    submitting.value = false;
  }
}
</script>

<template>
  <el-dialog
    :model-value="visible"
    :show-close="false"
    :close-on-click-modal="false"
    :close-on-press-escape="false"
    width="480px"
    append-to-body
  >
    <template #header>
      <div class="hk-dialog-title">
        <el-icon color="var(--el-color-danger)"><WarningFilled /></el-icon>
        <span>主机公钥变更警告</span>
      </div>
    </template>

    <template v-if="current">
      <el-alert
        type="warning"
        :closable="false"
        show-icon
        title="检测到服务器公钥与历史记录不一致"
      >
        <div class="hk-alert-body">
          目标 <strong>{{ current.challenge.host }}:{{ current.challenge.port }}</strong>
          的公钥指纹与 known_hosts 中记录的不同。这通常意味着服务器已重装系统、
          更换了密钥，但也可能是中间人攻击。请核对指纹后再决定是否信任。
        </div>
      </el-alert>

      <div class="hk-fingerprints">
        <div class="hk-fp-row">
          <div class="hk-fp-label">服务器实际公钥</div>
          <div class="hk-fp-value">
            <el-tag size="small" type="success">{{ current.challenge.keyType }}</el-tag>
            <code class="hk-fp-code">{{ current.challenge.fingerprint }}</code>
          </div>
        </div>
        <div class="hk-fp-row">
          <div class="hk-fp-label">known_hosts 记录</div>
          <div class="hk-fp-value">
            <code class="hk-fp-code hk-fp-code-old">{{ current.challenge.knownFingerprint }}</code>
          </div>
        </div>
      </div>
    </template>

    <template #footer>
      <el-button :disabled="submitting" @click="respondCurrent('Reject')">
        拒绝连接
      </el-button>
      <el-button :disabled="submitting" @click="respondCurrent('AcceptOnce')">
        仅本次接受
      </el-button>
      <el-button
        type="primary"
        :loading="submitting"
        @click="respondCurrent('AcceptAndUpdate')"
      >
        接受并更新
      </el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.hk-dialog-title {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 15px;
  font-weight: 600;
  color: var(--el-text-color-primary);
}

.hk-alert-body {
  margin-top: 4px;
  font-size: 13px;
  line-height: 1.6;
}

.hk-fingerprints {
  margin-top: 14px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.hk-fp-row {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.hk-fp-label {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}

.hk-fp-value {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.hk-fp-code {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 12px;
  padding: 6px 8px;
  background: var(--el-fill-color-light);
  border-radius: 4px;
  word-break: break-all;
  color: var(--el-color-success);
}

.hk-fp-code-old {
  color: var(--el-color-danger);
}
</style>
