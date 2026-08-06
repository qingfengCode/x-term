<script setup lang="ts">
/**
 * SSH 二次认证（keyboard-interactive）挑战弹窗。
 *
 * 全局监听 `ssh:auth_challenge` 事件（后端在认证过程中收到 OTP/验证码等
 * 挑战时发出），弹出输入框收集用户输入，通过 `ssh_auth_respond` 回传。
 *
 * - 多个挑战（如快速连多个会话）按到达顺序排队串行处理，避免弹多个框；
 * - 若用户已保存 TOTP 条目，提供"使用 TOTP 验证码"下拉，选中后实时取码
 *   填入第一个空输入框（保险库未解锁时自动隐藏）；
 * - 后端等待超时 120s，前端设置 125s 兜底计时器自动取消，防止弹窗悬挂。
 */
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { ElMessage } from "element-plus";
import { sshAuthRespond, type SshAuthChallengeEvent } from "@/api/session";
import { totpGenerate, totpList, type TotpEntry } from "@/api/totp";

/** 后端等待超时 120s，前端兜底略长，到点自动取消弹窗。 */
const CHALLENGE_TIMEOUT_MS = 125_000;

interface QueuedChallenge {
  challenge: SshAuthChallengeEvent;
  /** 各输入项当前值，与 challenge.prompts 一一对应。 */
  values: string[];
  /** 兜底取消计时器。 */
  timer: number;
  /** 可用的 TOTP 条目（保险库未解锁时为空数组）。 */
  totpEntries: TotpEntry[];
}

const queue = ref<QueuedChallenge[]>([]);
const submitting = ref(false);
let unlisten: UnlistenFn | null = null;

const current = computed(() => queue.value[0] ?? null);
const visible = computed(() => queue.value.length > 0);

// --- 事件监听 ---------------------------------------------------------------

onMounted(async () => {
  unlisten = await listen<SshAuthChallengeEvent>("ssh:auth_challenge", (e) => {
    void enqueue(e.payload);
  });
});

onBeforeUnmount(() => {
  unlisten?.();
  for (const item of queue.value) window.clearTimeout(item.timer);
});

async function enqueue(challenge: SshAuthChallengeEvent) {
  // TOTP 条目加载失败（保险库未解锁等）则隐藏该功能，不影响主流程。
  let totpEntries: TotpEntry[] = [];
  try {
    totpEntries = await totpList();
  } catch {
    totpEntries = [];
  }
  const timer = window.setTimeout(() => void cancelCurrent(), CHALLENGE_TIMEOUT_MS);
  queue.value.push({
    challenge,
    values: challenge.prompts.map(() => ""),
    timer,
    totpEntries,
  });
}

// --- 提交 / 取消 ------------------------------------------------------------

async function submit() {
  const item = current.value;
  if (!item || submitting.value) return;
  submitting.value = true;
  try {
    window.clearTimeout(item.timer);
    await sshAuthRespond(item.challenge.challengeId, item.values);
    queue.value.shift();
  } catch (e) {
    // 挑战可能已被后端判定超时/取消（返回 NotFound），此时直接丢弃即可。
    ElMessage.error("提交验证码失败: " + String(e));
    queue.value.shift();
  } finally {
    submitting.value = false;
  }
}

async function cancelCurrent() {
  const item = current.value;
  if (!item) return;
  window.clearTimeout(item.timer);
  queue.value.shift();
  try {
    await sshAuthRespond(item.challenge.challengeId, null);
  } catch {
    /* 挑战已关闭，忽略 */
  }
}

// --- TOTP 填充 --------------------------------------------------------------

/** 判断提示文本是否像验证码/OTP 输入框（用于挑选填充目标）。 */
function isOtpPrompt(text: string): boolean {
  return /verification|code|otp|passcode|mfa|token|验证码|动态码/i.test(text);
}

/** 选中某条 TOTP 后，把当前验证码填入第一个空的验证码输入框。 */
async function fillTotp(entryId: string) {
  const item = current.value;
  if (!item) return;
  try {
    const code = await totpGenerate(entryId);
    const otpIdx = item.values.findIndex(
      (v, i) => !v && isOtpPrompt(item.challenge.prompts[i]?.prompt ?? "")
    );
    const target = otpIdx >= 0 ? otpIdx : item.values.findIndex((v) => !v);
    if (target >= 0) {
      item.values[target] = code.code;
    }
  } catch (e) {
    ElMessage.error("获取验证码失败: " + String(e));
  }
}

function totpLabel(entry: TotpEntry): string {
  return [entry.issuer, entry.account].filter(Boolean).join(" / ");
}
</script>

<template>
  <el-dialog
    :model-value="visible"
    :show-close="false"
    :close-on-click-modal="false"
    :close-on-press-escape="false"
    width="440px"
    append-to-body
  >
    <template #header>
      <div class="auth-dialog-title">
        <el-icon><Key /></el-icon>
        <span>SSH 二次认证</span>
      </div>
    </template>

    <template v-if="current">
      <p class="auth-dialog-desc">
        <span class="auth-target">{{ current.challenge.username }}@{{ current.challenge.host }}:{{ current.challenge.port }}</span>
        需要输入额外验证信息以完成登录。
      </p>
      <p v-if="current.challenge.instructions" class="auth-instructions">
        {{ current.challenge.instructions }}
      </p>

      <div class="auth-form">
        <div v-for="(prompt, i) in current.challenge.prompts" :key="i" class="auth-field">
          <label class="auth-label">{{ prompt.prompt }}</label>
          <el-input
            v-model="current.values[i]"
            :type="prompt.echo ? 'text' : 'password'"
            :autofocus="i === 0"
            :show-password="!prompt.echo"
            clearable
            @keyup.enter="submit"
          />
        </div>

        <div v-if="current.totpEntries.length > 0" class="auth-field">
          <label class="auth-label">使用 TOTP 验证码</label>
          <el-select
            placeholder="选择已保存的 TOTP 条目"
            style="width: 100%"
            @change="(id: string) => fillTotp(id)"
          >
            <el-option
              v-for="entry in current.totpEntries"
              :key="entry.id"
              :value="entry.id"
              :label="totpLabel(entry)"
            />
          </el-select>
        </div>
      </div>
    </template>

    <template #footer>
      <el-button :disabled="submitting" @click="cancelCurrent">取消</el-button>
      <el-button type="primary" :loading="submitting" @click="submit">
        提交
      </el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.auth-dialog-title {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 15px;
  font-weight: 600;
  color: var(--el-text-color-primary);
}

.auth-dialog-desc {
  margin: 0 0 8px;
  font-size: 13px;
  color: var(--el-text-color-regular);
  line-height: 1.6;
}

.auth-target {
  color: var(--el-color-primary);
  font-weight: 600;
}

.auth-instructions {
  margin: 0 0 12px;
  padding: 8px 10px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
  background: var(--el-fill-color-light);
  border-radius: 4px;
  white-space: pre-wrap;
  word-break: break-all;
}

.auth-form {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.auth-field {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.auth-label {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
</style>
