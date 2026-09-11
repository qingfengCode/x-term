<script setup lang="ts">
/**
 * SSH 认证失败后的手动认证弹窗。
 *
 * 连接失败为认证类错误（密码错误 / 服务器要求口令码等二次认证）时，terminals
 * store 置 `manualAuth` 状态，本组件弹出收集用户手动输入的密码 / 验证码，
 * 通过 `connect_session_with_manual_auth` 重试连接。
 *
 * - 密码留空时后端回退使用会话配置已保存的凭据；
 * - 口令码 / 验证码（OTP）会预填进 keyboard-interactive 首个验证码提示，其余
 *   提示仍会走 `ssh:auth_challenge` 弹窗；
 * - 重试仍为认证类错误时弹窗保持打开并展示错误，供用户继续尝试；
 * - 保险库解锁时提供 TOTP 下拉，选中后实时取码填入验证码框。
 */
import { computed, ref, watch } from "vue";
import { ElMessage } from "element-plus";
import { useTerminalsStore } from "@/stores/terminals";
import { totpGenerate, totpList, type TotpEntry } from "@/api/totp";

const terminals = useTerminalsStore();

const password = ref("");
const otp = ref("");
/** 可用的 TOTP 条目（保险库未解锁时为空数组，隐藏下拉）。 */
const totpEntries = ref<TotpEntry[]>([]);

const current = computed(() => terminals.manualAuth);
const visible = computed(() => current.value !== null);

// 每次弹窗打开时清空输入并加载 TOTP 条目（加载失败静默隐藏该功能）。
watch(visible, async (v) => {
  if (v) {
    password.value = "";
    otp.value = "";
    try {
      totpEntries.value = await totpList();
    } catch {
      totpEntries.value = [];
    }
  }
});

/** 提交：调用 store 重试；成功/失败状态由 store 维护，弹窗随之关闭或保持。 */
async function submit() {
  const req = current.value;
  if (!req || req.busy) return;
  await terminals.retryWithManualAuth(password.value, otp.value);
}

/**
 * 取消：任何状态都可取消。重试进行中取消会立即关闭弹窗，store 标记 cancelled，
 * 重试返回后丢弃结果（连接成功则断开新实例）。
 */
function cancel() {
  terminals.cancelManualAuth();
}

/** 选中某条 TOTP 后，把当前验证码填入验证码输入框。 */
async function fillTotp(entryId: string) {
  try {
    const code = await totpGenerate(entryId);
    otp.value = code.code;
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
        <span>SSH 认证失败</span>
      </div>
    </template>

    <template v-if="current">
      <p class="auth-dialog-desc">
        <span class="auth-target">{{ current.session.username }}@{{ current.session.host }}:{{ current.session.port }}</span>
        认证失败，请手动输入凭据重试。
      </p>
      <p v-if="current.error" class="auth-error">{{ current.error }}</p>

      <div class="auth-form">
        <div class="auth-field">
          <label class="auth-label">密码</label>
          <el-input
            v-model="password"
            type="password"
            show-password
            clearable
            placeholder="留空则使用已保存的凭据"
            :autofocus="true"
            @keyup.enter="submit"
          />
        </div>
        <div class="auth-field">
          <label class="auth-label">口令码 / 验证码（二次认证，可选）</label>
          <el-input
            v-model="otp"
            clearable
            placeholder="服务器要求动态口令时填写"
            @keyup.enter="submit"
          />
        </div>

        <div v-if="totpEntries.length > 0" class="auth-field">
          <label class="auth-label">使用 TOTP 验证码</label>
          <el-select
            placeholder="选择已保存的 TOTP 条目"
            style="width: 100%"
            @change="(id: string) => fillTotp(id)"
          >
            <el-option
              v-for="entry in totpEntries"
              :key="entry.id"
              :value="entry.id"
              :label="totpLabel(entry)"
            />
          </el-select>
        </div>
      </div>
    </template>

    <template #footer>
      <!-- 取消始终可用：重试进行中取消会丢弃本次结果（含断开已建立的新连接） -->
      <el-button @click="cancel">取消</el-button>
      <el-button type="primary" :loading="current?.busy" @click="submit">
        重试连接
      </el-button>
    </template>
  </el-dialog>
</template>

<style scoped lang="scss">
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

.auth-error {
  margin: 0 0 12px;
  padding: 8px 10px;
  font-size: 12px;
  color: var(--el-color-danger);
  background: var(--el-color-danger-light-9);
  border-radius: 4px;
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
