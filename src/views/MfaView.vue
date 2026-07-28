<!--
  MfaView — TOTP/MFA 验证码管理页。

  功能：
  - 列表：每个条目显示 issuer/account、大字号动态码、倒计时进度环。
  - 每 1 秒刷新所有码（本地按 period 计算，避免频繁 IPC；周期切换时重新调后端生成）。
  - 操作：复制码、填充到当前活动终端、删除。
  - 添加：手动输入 issuer/account/secret，或直接粘贴 otpauth:// URI 自动解析。
-->
<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { Plus, Delete, CopyDocument, Refresh, Position, Iphone } from "@element-plus/icons-vue";
import * as totpApi from "@/api/totp";
import type { TotpEntry, TotpCode } from "@/api/totp";
import { useTerminalsStore } from "@/stores/terminals";

defineOptions({ name: "MfaView" });

const terminals = useTerminalsStore();

// --- 列表 -----------------------------------------------------------------
const entries = ref<TotpEntry[]>([]);
const loading = ref(false);
/** id -> 当前码（含周期起始时间戳，便于本地倒计时）。 */
const codes = ref<Map<string, { code: string; codeTs: number; period: number }>>(new Map());

/** 当前 Unix 秒。 */
const nowSec = ref(Math.floor(Date.now() / 1000));
let tickTimer: ReturnType<typeof setInterval> | null = null;

const hasTerminal = computed(() => !!terminals.activeId);

async function load() {
  loading.value = true;
  try {
    entries.value = await totpApi.totpList();
    // 全量生成码。
    await refreshAllCodes();
  } catch (e) {
    ElMessage.error("加载 MFA 列表失败：" + String(e));
  } finally {
    loading.value = false;
  }
}

/** 为所有条目重新生成码（调后端）。 */
async function refreshAllCodes() {
  const next = new Map<string, { code: string; codeTs: number; period: number }>();
  await Promise.all(
    entries.value.map(async (e) => {
      try {
        const c = await totpApi.totpGenerate(e.id);
        next.set(e.id, {
          code: c.code,
          codeTs: Math.floor(Date.now() / 1000),
          period: e.period,
        });
      } catch {
        /* 单条失败忽略 */
      }
    })
  );
  codes.value = next;
}

/** 某条目的显示码（按 codeTs + period 判断是否仍在有效期；过期返回空，触发刷新）。 */
function displayCode(e: TotpEntry): string {
  const c = codes.value.get(e.id);
  if (!c) return "";
  const elapsed = nowSec.value - c.codeTs;
  if (elapsed >= e.period) return "------"; // 过期，等待刷新
  return c.code;
}

/** 倒计时百分比（用于进度环）：0~100，100 表示新周期刚开始。 */
function progressPercent(e: TotpEntry): number {
  const c = codes.value.get(e.id);
  if (!c) return 0;
  const elapsed = nowSec.value - c.codeTs;
  if (elapsed >= e.period) return 0;
  return Math.max(0, ((e.period - elapsed) / e.period) * 100);
}

/** 倒计时秒数。 */
function remaining(e: TotpEntry): number {
  const c = codes.value.get(e.id);
  if (!c) return 0;
  const r = e.period - (nowSec.value - c.codeTs);
  return r > 0 ? r : 0;
}

/** 是否即将过期（<5s，高亮提示用户等待）。 */
function isUrgent(e: TotpEntry): boolean {
  return remaining(e) <= 5 && remaining(e) > 0;
}

// 每秒 tick：更新 nowSec；检测周期切换 → 重新生成码。
function startTick() {
  tickTimer = setInterval(async () => {
    nowSec.value = Math.floor(Date.now() / 1000);
    // 任一条目过期则刷新全部（简单起见）。
    const anyExpired = entries.value.some((e) => {
      const c = codes.value.get(e.id);
      return !c || nowSec.value - c.codeTs >= e.period;
    });
    if (anyExpired) await refreshAllCodes();
  }, 1000);
}

// --- 操作 -----------------------------------------------------------------
async function copyCode(e: TotpEntry) {
  const code = displayCode(e);
  if (!code || code === "------") {
    ElMessage.warning("验证码尚未生成或已过期，请稍候");
    return;
  }
  try {
    await navigator.clipboard.writeText(code);
    ElMessage.success(`已复制 ${e.issuer} 的验证码`);
  } catch {
    ElMessage.error("复制失败（剪贴板不可用）");
  }
}

async function fillTerminal(e: TotpEntry) {
  const instanceId = terminals.activeId;
  if (!instanceId) {
    ElMessage.warning("没有活动终端，请先连接一个会话");
    return;
  }
  try {
    await totpApi.totpFillTerminal(e.id, instanceId);
    ElMessage.success(`已填充到终端（按回车提交）`);
  } catch (err) {
    ElMessage.error("填充失败：" + String(err));
  }
}

async function removeEntry(e: TotpEntry) {
  try {
    await ElMessageBox.confirm(
      `确定删除 ${e.issuer} (${e.account}) 的 MFA 条目？`,
      "删除确认",
      { type: "warning" }
    );
  } catch {
    return;
  }
  try {
    await totpApi.totpDelete(e.id);
    entries.value = entries.value.filter((x) => x.id !== e.id);
    codes.value.delete(e.id);
    ElMessage.success("已删除");
  } catch (err) {
    ElMessage.error("删除失败：" + String(err));
  }
}

// --- 添加对话框 -----------------------------------------------------------
const addVisible = ref(false);
const addForm = ref({
  issuer: "",
  account: "",
  secret: "",
  algorithm: "SHA1",
  digits: 6,
  period: 30,
});
const addLoading = ref(false);
/** 预览码（用户输入 secret 后实时显示，验证有效性）。 */
const previewCode = ref("");
const previewTimer = ref<ReturnType<typeof setInterval> | null>(null);

const algorithms = [
  { label: "SHA1（默认）", value: "SHA1" },
  { label: "SHA256", value: "SHA256" },
  { label: "SHA512", value: "SHA512" },
];

function openAdd() {
  addForm.value = {
    issuer: "",
    account: "",
    secret: "",
    algorithm: "SHA1",
    digits: 6,
    period: 30,
  };
  previewCode.value = "";
  addVisible.value = true;
}

/** secret 输入变化：若为 otpauth URI 自动解析填充；否则实时预览码。 */
watch(
  () => addForm.value.secret,
  async (v) => {
    previewCode.value = "";
    if (!v) return;
    // otpauth URI 自动解析。
    if (v.startsWith("otpauth://")) {
      tryParseOtpauth(v);
      return;
    }
    // 尝试实时预览。
    tryPreview();
  }
);

function tryParseOtpauth(uri: string) {
  try {
    const u = new URL(uri);
    // path: /Issuer:account
    const path = decodeURIComponent(u.pathname).replace(/^\//, "");
    const [issuer, account] = path.includes(":") ? path.split(":", 2) : ["", path];
    const q = u.searchParams;
    if (issuer) addForm.value.issuer = issuer;
    else if (q.get("issuer")) addForm.value.issuer = q.get("issuer")!;
    if (account) addForm.value.account = account;
    if (q.get("algorithm")) addForm.value.algorithm = q.get("algorithm")!.toUpperCase();
    if (q.get("digits")) addForm.value.digits = Number(q.get("digits"));
    if (q.get("period")) addForm.value.period = Number(q.get("period"));
    if (q.get("secret")) addForm.value.secret = q.get("secret")!;
  } catch {
    /* 非法 URI 忽略 */
  }
}

async function tryPreview() {
  if (previewTimer.value) clearTimeout(previewTimer.value);
  previewTimer.value = setTimeout(async () => {
    const f = addForm.value;
    if (!f.secret) {
      previewCode.value = "";
      return;
    }
    try {
      const c = await totpApi.totpGenerateForSecret(
        f.secret,
        f.algorithm,
        f.digits,
        f.period
      );
      previewCode.value = c.code;
    } catch {
      previewCode.value = "";
    }
  }, 300);
}

async function submitAdd() {
  const f = addForm.value;
  if (!f.issuer.trim()) {
    ElMessage.warning("请输入发行方");
    return;
  }
  if (!f.account.trim()) {
    ElMessage.warning("请输入账号");
    return;
  }
  if (!f.secret.trim()) {
    ElMessage.warning("请输入 secret 或 otpauth URI");
    return;
  }
  addLoading.value = true;
  try {
    const entry = await totpApi.totpAdd({
      issuer: f.issuer,
      account: f.account,
      secret: f.secret,
      algorithm: f.algorithm,
      digits: f.digits,
      period: f.period,
    });
    entries.value.push(entry);
    entries.value.sort((a, b) => a.issuer.localeCompare(b.issuer));
    addVisible.value = false;
    ElMessage.success("已添加");
    // 立即生成码。
    try {
      const c = await totpApi.totpGenerate(entry.id);
      codes.value.set(entry.id, {
        code: c.code,
        codeTs: Math.floor(Date.now() / 1000),
        period: entry.period,
      });
    } catch {
      /* ignore */
    }
  } catch (err) {
    ElMessage.error("添加失败：" + String(err));
  } finally {
    addLoading.value = false;
  }
}

// --- 生命周期 -------------------------------------------------------------
onMounted(async () => {
  await load();
  startTick();
});

onBeforeUnmount(() => {
  if (tickTimer) clearInterval(tickTimer);
  if (previewTimer.value) clearTimeout(previewTimer.value);
});
</script>

<template>
  <div class="mfa-view">
    <header class="mfa-header">
      <div class="title-area">
        <el-icon class="title-icon"><Iphone /></el-icon>
        <h2>MFA 验证码</h2>
        <span class="subtitle">TOTP 双因素验证码管理 · 自动填充到终端</span>
      </div>
      <div class="actions">
        <el-button :icon="Refresh" size="small" @click="load">刷新</el-button>
        <el-button type="primary" :icon="Plus" size="small" @click="openAdd">
          添加验证码
        </el-button>
      </div>
    </header>

    <!-- 列表 -->
    <div v-loading="loading" class="mfa-list">
      <div v-if="entries.length === 0 && !loading" class="empty-tip">
        <el-icon><Iphone /></el-icon>
        <p>还没有 MFA 条目</p>
        <span>点击"添加验证码"，输入 otpauth URI 或手动填写 secret</span>
      </div>

      <div v-for="e in entries" :key="e.id" class="mfa-card" :class="{ urgent: isUrgent(e) }">
        <!-- 左：发行方/账号 + 大字号码 -->
        <div class="card-main">
          <div class="card-meta">
            <span class="issuer">{{ e.issuer }}</span>
            <span class="account">{{ e.account }}</span>
            <el-tag size="small" type="info" effect="plain" class="algo-tag">
              {{ e.algorithm }} · {{ e.digits }}位
            </el-tag>
          </div>
          <div class="code-display">
            {{ displayCode(e) || "------" }}
          </div>
        </div>
        <!-- 右：倒计时环 + 操作 -->
        <div class="card-side">
          <div class="countdown" :class="{ urgent: isUrgent(e) }">
            <svg viewBox="0 0 36 36" class="ring">
              <circle
                class="ring-bg"
                cx="18" cy="18" r="15.9"
                fill="none" stroke-width="3"
              />
              <circle
                class="ring-fg"
                cx="18" cy="18" r="15.9"
                fill="none" stroke-width="3"
                :stroke-dasharray="`${progressPercent(e)} 100`"
              />
            </svg>
            <span class="countdown-num">{{ remaining(e) }}</span>
          </div>
          <div class="card-actions">
            <el-tooltip content="复制验证码" placement="top">
              <el-button :icon="CopyDocument" circle size="small" @click="copyCode(e)" />
            </el-tooltip>
            <el-tooltip :content="hasTerminal ? '填充到当前终端' : '（无活动终端）'" placement="top">
              <el-button
                :icon="Position"
                circle
                size="small"
                :disabled="!hasTerminal"
                @click="fillTerminal(e)"
              />
            </el-tooltip>
            <el-tooltip content="删除" placement="top">
              <el-button
                :icon="Delete"
                circle
                size="small"
                type="danger"
                @click="removeEntry(e)"
              />
            </el-tooltip>
          </div>
        </div>
      </div>
    </div>

    <!-- 添加对话框 -->
    <el-dialog v-model="addVisible" title="添加 MFA 验证码" width="480px">
      <el-alert
        type="info"
        :closable="false"
        show-icon
        title="可直接粘贴 otpauth:// URI 到 secret 框，系统会自动解析填充其余字段"
        style="margin-bottom: 16px"
      />
      <el-form :model="addForm" label-position="top">
        <el-form-item label="Secret / otpauth URI">
          <el-input
            v-model="addForm.secret"
            placeholder="如 JBSWY3DPEHPK3PXP 或 otpauth://totp/..."
            class="mono"
          />
          <div v-if="previewCode" class="preview">
            预览：<span class="preview-code">{{ previewCode }}</span>
          </div>
        </el-form-item>
        <el-form-item label="发行方 (Issuer)">
          <el-input v-model="addForm.issuer" placeholder="如 GitHub、AWS" />
        </el-form-item>
        <el-form-item label="账号 (Account)">
          <el-input v-model="addForm.account" placeholder="如 user@example.com" />
        </el-form-item>
        <div style="display: flex; gap: 12px">
          <el-form-item label="算法" style="flex: 1">
            <el-select v-model="addForm.algorithm">
              <el-option
                v-for="a in algorithms"
                :key="a.value"
                :label="a.label"
                :value="a.value"
              />
            </el-select>
          </el-form-item>
          <el-form-item label="位数" style="width: 100px">
            <el-input-number v-model="addForm.digits" :min="6" :max="8" />
          </el-form-item>
          <el-form-item label="周期(秒)" style="width: 100px">
            <el-input-number v-model="addForm.period" :min="15" :max="120" :step="15" />
          </el-form-item>
        </div>
      </el-form>
      <template #footer>
        <el-button @click="addVisible = false">取消</el-button>
        <el-button type="primary" :loading="addLoading" @click="submitAdd">添加</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.mfa-view {
  display: flex;
  flex-direction: column;
  height: 100%;
  overflow: hidden;
}
.mfa-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 16px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  flex-shrink: 0;
}
.title-area {
  display: flex;
  align-items: baseline;
  gap: 8px;
}
.title-icon {
  font-size: 22px;
  color: var(--el-color-primary);
  align-self: center;
}
.title-area h2 {
  margin: 0;
  font-size: 16px;
}
.subtitle {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.actions {
  display: flex;
  gap: 8px;
}

/* 列表 */
.mfa-list {
  flex: 1;
  overflow: auto;
  padding: 16px;
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
  gap: 12px;
  align-content: start;
}
.empty-tip {
  grid-column: 1 / -1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  padding: 60px 20px;
  color: var(--el-text-color-secondary);
}
.empty-tip .el-icon {
  font-size: 48px;
  margin-bottom: 8px;
  opacity: 0.4;
}
.empty-tip p {
  margin: 4px 0;
  font-size: 14px;
}
.empty-tip span {
  font-size: 12px;
}

/* 卡片 */
.mfa-card {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 14px 16px;
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 8px;
  transition: border-color 0.2s, box-shadow 0.2s;
}
.mfa-card:hover {
  border-color: var(--el-color-primary-light-5);
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.06);
}
.mfa-card.urgent {
  border-color: var(--el-color-warning);
}
.card-main {
  min-width: 0;
  flex: 1;
}
.card-meta {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 6px;
  flex-wrap: wrap;
}
.issuer {
  font-weight: 600;
  font-size: 14px;
}
.account {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.algo-tag {
  font-size: 10px;
}
.code-display {
  font-family: "Cascadia Code", Consolas, monospace;
  font-size: 28px;
  font-weight: 700;
  letter-spacing: 3px;
  color: var(--el-color-primary);
  line-height: 1.2;
}

/* 倒计时环 */
.card-side {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
  margin-left: 12px;
}
.countdown {
  position: relative;
  width: 40px;
  height: 40px;
}
.countdown .ring {
  transform: rotate(-90deg);
  width: 100%;
  height: 100%;
}
.ring-bg {
  stroke: var(--el-fill-color-dark);
}
.ring-fg {
  stroke: var(--el-color-primary);
  stroke-linecap: round;
  transition: stroke-dasharray 0.3s linear;
}
.countdown.urgent .ring-fg {
  stroke: var(--el-color-warning);
}
.countdown-num {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 12px;
  font-weight: 600;
  color: var(--el-text-color-regular);
}
.card-actions {
  display: flex;
  gap: 4px;
}

/* 预览 */
.preview {
  margin-top: 4px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.preview-code {
  font-family: monospace;
  font-size: 16px;
  font-weight: 700;
  color: var(--el-color-success);
  letter-spacing: 2px;
}
.mono :deep(input) {
  font-family: "Cascadia Code", Consolas, monospace;
}
</style>
