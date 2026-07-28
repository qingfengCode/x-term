<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useRouter } from "vue-router";
import { ElMessage } from "element-plus";
import { useVaultStore } from "@/stores/vault";

const router = useRouter();
const vault = useVaultStore();

const passphrase = ref("");
const confirm = ref("");
const submitting = ref(false);

onMounted(async () => {
  await vault.refresh();
});

async function submit() {
  if (!passphrase.value) {
    ElMessage.warning("请输入主密码");
    return;
  }
  submitting.value = true;
  try {
    if (!vault.exists) {
      // 创建：需要二次确认。
      if (passphrase.value !== confirm.value) {
        ElMessage.error("两次输入的主密码不一致");
        return;
      }
      await vault.create(passphrase.value);
    } else {
      await vault.unlock(passphrase.value);
    }
    ElMessage.success(vault.exists ? "解锁成功" : "保险库已创建");
    router.push("/");
  } catch (e) {
    ElMessage.error(String(e));
  } finally {
    submitting.value = false;
  }
}
</script>

<template>
  <div class="unlock-page">
    <div class="unlock-card">
      <div class="logo">🔐</div>
      <h2>{{ vault.exists ? "解锁凭据保险库" : "创建凭据保险库" }}</h2>
      <p class="hint">
        {{
          vault.exists
            ? "请输入主密码以解锁已加密保存的服务器凭据。"
            : "首次使用，请设置一个主密码。所有密码/私钥将以此加密保存于本机，丢失后无法找回。"
        }}
      </p>
      <el-form @submit.prevent="submit" label-position="top">
        <el-form-item :label="vault.exists ? '主密码' : '设置主密码'">
          <el-input
            v-model="passphrase"
            type="password"
            show-password
            placeholder="至少 6 位"
            @keyup.enter="submit"
          />
        </el-form-item>
        <el-form-item v-if="!vault.exists" label="确认主密码">
          <el-input
            v-model="confirm"
            type="password"
            show-password
            @keyup.enter="submit"
          />
        </el-form-item>
        <el-button type="primary" :loading="submitting" @click="submit" style="width: 100%">
          {{ vault.exists ? "解锁" : "创建并进入" }}
        </el-button>
      </el-form>
    </div>
  </div>
</template>

<style scoped>
.unlock-page {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100vh;
  background: var(--el-bg-color-page);
}
.unlock-card {
  width: 380px;
  padding: 32px;
  background: var(--el-bg-color);
  border-radius: 8px;
  box-shadow: 0 4px 24px rgba(0, 0, 0, 0.15);
}
.logo {
  font-size: 48px;
  text-align: center;
}
h2 {
  text-align: center;
  margin: 8px 0 4px;
}
.hint {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  text-align: center;
  margin-bottom: 20px;
  line-height: 1.6;
}
</style>
