// UI 全局状态：跨组件协调的界面状态（如 AI 面板折叠）。
//
// 用于让全局快捷键分发器能控制 AiPanel 的展开/收起，而不需要组件间直接引用。

import { defineStore } from "pinia";
import { ref } from "vue";

export const useUiStore = defineStore("ui", () => {
  /** AI 面板是否折叠。 */
  const aiCollapsed = ref(true);

  /** AI 面板展开宽度（按 domain 独立记忆，拖拽调整后保持，重启恢复默认）。 */
  const aiWidths = ref<Record<"ssh" | "db", number>>({ ssh: 340, db: 340 });

  /**
   * SSH 认证类全局弹窗互斥：认证挑战弹窗（SshAuthPrompt）与主机公钥确认弹窗
   * （HostKeyPrompt）不能同时弹出——同一时刻只允许一个处理用户交互，另一个的
   * 待处理项留在队列中等待。各自组件把"实际正在显示"写入这里。
   */
  const sshAuthVisible = ref(false);
  const hostKeyVisible = ref(false);

  function toggleAi() {
    aiCollapsed.value = !aiCollapsed.value;
  }

  function setAiCollapsed(v: boolean) {
    aiCollapsed.value = v;
  }

  function setAiWidth(domain: "ssh" | "db", w: number) {
    aiWidths.value = { ...aiWidths.value, [domain]: Math.round(w) };
  }

  return {
    aiCollapsed,
    toggleAi,
    setAiCollapsed,
    aiWidths,
    setAiWidth,
    sshAuthVisible,
    hostKeyVisible,
  };
});
