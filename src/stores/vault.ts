import { defineStore } from "pinia";
import { ref } from "vue";
import * as vaultApi from "@/api/vault";

/**
 * 凭据保险库状态。
 *
 * 三种状态：
 * - exists=false,unlocked=false：首次启动，需创建。
 * - exists=true,unlocked=false：已存在但未解锁，需输入主密码。
 * - unlocked=true：已解锁，可正常使用。
 */
export const useVaultStore = defineStore("vault", () => {
  const exists = ref(false);
  const unlocked = ref(false);
  const loading = ref(false);

  async function refresh() {
    loading.value = true;
    try {
      exists.value = await vaultApi.vaultExists();
      unlocked.value = await vaultApi.vaultUnlocked();
    } finally {
      loading.value = false;
    }
  }

  async function create(passphrase: string) {
    await vaultApi.vaultCreate(passphrase);
    exists.value = true;
    unlocked.value = true;
  }

  async function unlock(passphrase: string) {
    await vaultApi.vaultUnlock(passphrase);
    unlocked.value = true;
  }

  return { exists, unlocked, loading, refresh, create, unlock };
});
