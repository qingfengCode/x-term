import { defineStore } from "pinia";
import { ref } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  updateCheck,
  updateDownload,
  updateGetInfo,
  updateInstallAndExit,
  updateSetManifestUrl,
} from "@/api/update";
import type { UpdateInfo, UpdateManifest, UpdateProgressEvent } from "@/api/types";

/**
 * 应用自更新状态机。
 *
 * 状态流转：
 *   idle ──check──> checking ──> up-to-date | update-available
 *   update-available ──download──> downloading ──> downloaded ──install──> (退出)
 *   任意环节出错 ──> error
 *
 * 下载进度由后端 update:progress 事件推送，本 store 在下载期间订阅并写入 progress。
 */
export type UpdateStatus =
  | "idle"
  | "checking"
  | "up-to-date"
  | "update-available"
  | "downloading"
  | "downloaded"
  | "error";

const SKIP_KEY = "xterm.update.skippedVersion";

export const useUpdateStore = defineStore("update", () => {
  const status = ref<UpdateStatus>("idle");
  const info = ref<UpdateInfo | null>(null);
  /** 可用的新版本清单（status=update-available / downloading / downloaded 时有效）。 */
  const manifest = ref<UpdateManifest | null>(null);
  const progress = ref<UpdateProgressEvent>({ received: 0, total: 0, percent: 0 });
  const error = ref<string | null>(null);
  const downloadedPath = ref<string | null>(null);
  /** 用户选择跳过的版本（localStorage 持久化）。 */
  const skippedVersion = ref<string>(localStorage.getItem(SKIP_KEY) ?? "");

  let unlisten: UnlistenFn | null = null;

  function fail(msg: string) {
    status.value = "error";
    error.value = msg;
  }

  /** 拉取应用信息（当前版本 / 更新源等），供关于页展示。 */
  async function loadInfo() {
    try {
      info.value = await updateGetInfo();
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e);
    }
  }

  /** 保存更新源地址并刷新 info。 */
  async function saveManifestUrl(url: string) {
    await updateSetManifestUrl(url);
    if (info.value) info.value.manifestUrl = url;
  }

  /**
   * 检查更新。
   * @param includeSkipped 为 true 时忽略"跳过此版本"（手动点检查按钮时用）。
   */
  async function check(includeSkipped = true) {
    status.value = "checking";
    error.value = null;
    try {
      const m = await updateCheck();
      if (m && (includeSkipped || m.version !== skippedVersion.value)) {
        manifest.value = m;
        status.value = "update-available";
      } else {
        manifest.value = null;
        status.value = "up-to-date";
      }
    } catch (e) {
      fail(e instanceof Error ? e.message : String(e));
    }
  }

  /** 下载当前 manifest 对应的安装包，并订阅进度事件。 */
  async function download() {
    const m = manifest.value;
    if (!m) return;
    status.value = "downloading";
    error.value = null;
    progress.value = { received: 0, total: 0, percent: 0 };
    // 订阅进度（下载结束后解绑）。
    unlisten = await listen<UpdateProgressEvent>("update:progress", (e) => {
      progress.value = e.payload;
    });
    try {
      downloadedPath.value = await updateDownload(m);
      status.value = "downloaded";
    } catch (e) {
      fail(e instanceof Error ? e.message : String(e));
    } finally {
      if (unlisten) {
        unlisten();
        unlisten = null;
      }
    }
  }

  /** 安装已下载的安装包并退出应用。 */
  async function install() {
    if (!downloadedPath.value) return;
    try {
      await updateInstallAndExit(downloadedPath.value);
    } catch (e) {
      fail(e instanceof Error ? e.message : String(e));
    }
  }

  /** 跳过当前版本（下次检查不再提示，除非手动点检查）。 */
  function skip() {
    if (manifest.value) {
      skippedVersion.value = manifest.value.version;
      localStorage.setItem(SKIP_KEY, manifest.value.version);
    }
    status.value = "idle";
  }

  /** 回到空闲态（关闭关于页 / 重试前）。 */
  function reset() {
    status.value = "idle";
    error.value = null;
  }

  return {
    status,
    info,
    manifest,
    progress,
    error,
    downloadedPath,
    skippedVersion,
    loadInfo,
    saveManifestUrl,
    check,
    download,
    install,
    skip,
    reset,
  };
});
