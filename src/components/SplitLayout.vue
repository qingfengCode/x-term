<script setup lang="ts">
/**
 * 分屏布局骨架（递归渲染分屏树）。
 *
 * 只负责**布局几何**：分支节点按方向 flex 二分 + 可拖拽分隔条；叶子节点
 * 渲染为空盒子（data-split-leaf 标记窗格 id）。真正的终端窗格实例由父级
 * SplitArea 以「稳定挂载 + 几何同步」方式叠放在骨架之上——布局树增删/塌缩
 * 不会卸载任何 TerminalPane（scrollback 不丢）。
 *
 * 最大化（zoomId）时：包含目标窗格的分支只保留对应孩子占满空间，其余
 * 孩子与分隔条隐藏（display:none，窗格仍挂载、持续接收数据）。
 */
import { ref } from "vue";
import type { SplitBranchNode, SplitNode } from "@/utils/splitLayout";
import { subtreeHas } from "@/utils/splitLayout";

defineOptions({ name: "SplitLayout" });

const props = defineProps<{
  node: SplitNode;
  /** 最大化中的窗格 id（null = 正常布局）。 */
  zoomId?: string | null;
}>();
const emit = defineEmits<{
  (e: "ratio-change", node: SplitBranchNode, ratio: number): void;
}>();

const branchEl = ref<HTMLElement | null>(null);
const DIVIDER = 6;

/** 分支孩子尺寸：正常布局按 ratio 分配；最大化时只显示包含目标的孩子。 */
function childStyle(idx: 0 | 1): Record<string, string> {
  const node = props.node;
  if (node.type !== "branch") return {};
  if (props.zoomId) {
    return subtreeHas(node.children[idx], props.zoomId)
      ? { flex: "1 1 0%", minWidth: "0", minHeight: "0" }
      : { display: "none" };
  }
  if (idx === 0) {
    return { flex: `0 0 calc((100% - ${DIVIDER}px) * ${node.ratio})`, minWidth: "0", minHeight: "0" };
  }
  return { flex: "1 1 0%", minWidth: "0", minHeight: "0" };
}

/** 拖拽分隔条：按鼠标位置重算 ratio（钳制 0.1 ~ 0.9）并上报。 */
function startDrag(e: MouseEvent) {
  const node = props.node;
  if (node.type !== "branch" || props.zoomId) return;
  e.preventDefault();
  const dir = node.dir;
  document.body.style.userSelect = "none";
  document.body.style.cursor = dir === "row" ? "col-resize" : "row-resize";
  const move = (ev: MouseEvent) => {
    const el = branchEl.value;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const total = dir === "row" ? rect.width - DIVIDER : rect.height - DIVIDER;
    if (total <= 0) return;
    const pos = dir === "row" ? ev.clientX - rect.left : ev.clientY - rect.top;
    emit("ratio-change", node, Math.min(0.9, Math.max(0.1, pos / total)));
  };
  const up = () => {
    document.body.style.userSelect = "";
    document.body.style.cursor = "";
    window.removeEventListener("mousemove", move);
    window.removeEventListener("mouseup", up);
  };
  window.addEventListener("mousemove", move);
  window.addEventListener("mouseup", up);
}
</script>

<template>
  <div v-if="node.type === 'leaf'" class="split-leaf" :data-split-leaf="node.paneId">
    <slot name="leaf" :pane-id="node.paneId" />
  </div>
  <div v-else ref="branchEl" class="split-branch" :class="node.dir">
    <div class="split-child" :style="childStyle(0)">
      <SplitLayout :node="node.children[0]" :zoom-id="zoomId" @ratio-change="(n, r) => emit('ratio-change', n, r)">
        <template #leaf="scope: { paneId: string }"><slot name="leaf" v-bind="scope" /></template>
      </SplitLayout>
    </div>
    <div v-show="!zoomId" class="split-divider" :class="node.dir" @mousedown="startDrag" />
    <div class="split-child" :style="childStyle(1)">
      <SplitLayout :node="node.children[1]" :zoom-id="zoomId" @ratio-change="(n, r) => emit('ratio-change', n, r)">
        <template #leaf="scope: { paneId: string }"><slot name="leaf" v-bind="scope" /></template>
      </SplitLayout>
    </div>
  </div>
</template>

<style scoped>
.split-leaf {
  width: 100%;
  height: 100%;
  min-width: 0;
  min-height: 0;
}
.split-branch {
  display: flex;
  width: 100%;
  height: 100%;
  min-width: 0;
  min-height: 0;
}
.split-branch.row {
  flex-direction: row;
}
.split-branch.col {
  flex-direction: column;
}
.split-child {
  display: flex;
  min-width: 0;
  min-height: 0;
}
/* 嵌套的 SplitLayout 根节点撑满孩子容器 */
.split-child > * {
  flex: 1;
  min-width: 0;
  min-height: 0;
}
.split-divider {
  flex: 0 0 6px;
  background: var(--el-border-color-lighter);
  transition: background-color 0.15s ease;
}
.split-divider.row {
  width: 6px;
  cursor: col-resize;
}
.split-divider.col {
  height: 6px;
  cursor: row-resize;
}
.split-divider:hover {
  background: var(--el-color-primary-light-5);
}
</style>
