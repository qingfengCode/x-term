<script setup lang="ts">
/**
 * 终端页签内容区：分屏骨架 + 稳定挂载的窗格层。
 *
 * 架构（参考 uniTerm 的稳定终端实例池）：
 * - SplitLayout 只渲染布局几何（叶子=空盒子，data-split-leaf 标记）；
 * - TerminalPaneItem 按 tab.panes **平铺渲染**（key=pane.id），绝对定位
 *   叠放在骨架之上，几何由骨架盒子 rect 同步——布局树拆分/关闭/塌缩/
 *   拖比例/最大化都不会卸载 TerminalPane（xterm 实例与 scrollback 完整
 *   保留，仅尺寸变化触发 refit）。
 * - host 层 pointer-events:none 放行骨架分隔条拖拽；窗格自身恢复事件。
 *
 * 几何同步时机：挂载后、布局树/最大化/窗格数变化（deep watch）、根容器
 * 尺寸变化（ResizeObserver——覆盖窗口缩放、侧栏/AI 面板拖宽、页签
 * v-show 显隐）。
 */
import { nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from "vue";
import type { SplitBranchNode } from "@/utils/splitLayout";
import SplitLayout from "@/components/SplitLayout.vue";
import TerminalPaneItem from "@/components/TerminalPaneItem.vue";
import type { TerminalTab } from "@/stores/terminals";

const props = defineProps<{ tab: TerminalTab }>();
const emit = defineEmits<{
  (e: "pane-input", paneId: string, data: string): void;
}>();

const rootRef = ref<HTMLElement | null>(null);
/** 窗格几何（相对本组件根容器，px 字符串）。 */
const geom = reactive<Record<string, { left: string; top: string; width: string; height: string }>>({});
let ro: ResizeObserver | null = null;

/** 读取骨架盒子 rect，同步到窗格几何。 */
function recalc() {
  const root = rootRef.value;
  if (!root) return;
  const base = root.getBoundingClientRect();
  for (const p of props.tab.panes) {
    const box = root.querySelector(`.split-leaf[data-split-leaf="${p.id}"]`);
    if (!box) continue;
    const r = box.getBoundingClientRect();
    geom[p.id] = {
      left: `${r.left - base.left}px`,
      top: `${r.top - base.top}px`,
      width: `${r.width}px`,
      height: `${r.height}px`,
    };
  }
}

// 布局树结构/比例、最大化、窗格增删 → DOM 更新后重算几何。
// flush:post 确保骨架先完成渲染（含 v-show 分隔条/隐藏孩子的变化）。
watch(
  () => [props.tab.layout, props.tab.zoomedPaneId, props.tab.panes.length],
  () => recalc(),
  { deep: true, flush: "post" },
);

/** 分隔条拖拽：写回分支 ratio（store 响应式对象，deep watch 触发重算）。 */
function onRatio(node: SplitBranchNode, ratio: number) {
  node.ratio = ratio;
}

onMounted(() => {
  ro = new ResizeObserver(() => recalc());
  if (rootRef.value) ro.observe(rootRef.value);
  void nextTick(recalc);
});
onBeforeUnmount(() => {
  ro?.disconnect();
  ro = null;
});
</script>

<template>
  <div ref="rootRef" class="split-area">
    <SplitLayout :node="tab.layout" :zoom-id="tab.zoomedPaneId" @ratio-change="onRatio" />
    <div class="split-host">
      <TerminalPaneItem
        v-for="p in tab.panes"
        :key="p.id"
        class="split-item"
        :class="{ measuring: !geom[p.id] }"
        :style="geom[p.id]"
        :tab="tab"
        :pane="p"
        :show-header="tab.panes.length > 1 || !!tab.zoomedPaneId"
        @input="(d: string) => emit('pane-input', p.id, d)"
      />
    </div>
  </div>
</template>

<style scoped>
.split-area {
  position: relative;
  width: 100%;
  height: 100%;
  min-width: 0;
  min-height: 0;
}
/* 窗格承载层：覆盖骨架但放行鼠标事件（分隔条拖拽在骨架上） */
.split-host {
  position: absolute;
  inset: 0;
  pointer-events: none;
}
.split-item {
  position: absolute;
  pointer-events: auto;
}
/* 首帧几何未测量：先全屏铺满，避免闪现在左上角 */
.split-item.measuring {
  inset: 0;
}
</style>
