import { defineStore } from "pinia";
import { ref } from "vue";
import * as desktopApi from "@/api/remote_desktop";
import type { Desktop } from "@/api/remote_desktop";

/**
 * 桌面连接（RDP/VNC）store。独立于终端 sessions store。
 */
export const useDesktopsStore = defineStore("desktops", () => {
  const desktops = ref<Desktop[]>([]);
  const loaded = ref(false);

  async function load() {
    desktops.value = await desktopApi.desktopList();
    loaded.value = true;
  }

  async function save(desktop: Desktop) {
    await desktopApi.desktopSave(desktop);
    const idx = desktops.value.findIndex((d) => d.id === desktop.id);
    if (idx >= 0) desktops.value[idx] = desktop;
    else desktops.value.push(desktop);
  }

  async function remove(id: string) {
    await desktopApi.desktopDelete(id);
    desktops.value = desktops.value.filter((d) => d.id !== id);
  }

  return { desktops, loaded, load, save, remove };
});
