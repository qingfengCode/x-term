<template>
  <div
    ref="popupRef"
    v-show="visible"
    class="terminal-suggestion-popup"
    :style="popupStyle"
  >
    <!-- 可滚动列表（历史 + 快捷命令） -->
    <div ref="listRef" class="suggestion-list">
      <div
        v-for="{ item, index } in listItems"
        :key="item.type === 'history' ? `h-${item.id ?? item.value}` : `q-${item.value}`"
        class="suggestion-item"
        :class="{ selected: index === selectedIndex }"
        @click="emit('select', index)"
        @mouseenter="onHover(index)"
      >
        <span class="suggestion-icon">
          <el-icon v-if="item.type === 'quick-command'" :size="12"><Lightning /></el-icon>
          <el-icon v-else :size="12"><Clock /></el-icon>
        </span>
        <!-- 快捷命令：名称 + 命令双行（label ≠ command 时） -->
        <span v-if="item.type === 'quick-command' && item.label !== item.value" class="suggestion-label">
          <span class="qc-name">
            <template v-for="(ch, i) in item.label" :key="i">
              <span :class="{ 'match-char': item.matchIndices?.includes(i) }">{{ ch }}</span>
            </template>
          </span>
          <span class="qc-cmd">
            <template v-for="(ch, i) in item.value" :key="i">
              <span :class="{ 'match-char': item.commandMatchIndices?.includes(i) }">{{ ch }}</span>
            </template>
          </span>
        </span>
        <span v-else class="suggestion-label">
          <template v-for="(ch, i) in item.label" :key="i">
            <span :class="{ 'match-char': item.matchIndices?.includes(i) }">{{ ch }}</span>
          </template>
        </span>
        <button
          v-if="item.type === 'history'"
          class="suggestion-delete"
          title="从历史中删除"
          @click.stop="item.id != null && emit('remove', item.id)"
        >
          <el-icon :size="12"><Delete /></el-icon>
        </button>
      </div>
    </div>
    <!-- AI 项固定在底部 -->
    <div
      v-if="aiItem"
      class="suggestion-item ai-fixed"
      :class="{
        selected: aiItem.index === selectedIndex,
        'ai-result': aiItem.item.type === 'ai-result',
      }"
      @click="emit('select', aiItem.index)"
      @mouseenter="onHover(aiItem.index)"
    >
      <span class="suggestion-icon ai"><el-icon :size="12"><MagicStick /></el-icon></span>
      <span class="suggestion-label">{{ aiItem.item.label }}</span>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * 终端补全建议弹窗。
 *
 * fixed 定位在光标像素位置附近（cursorX/cursorY 为相对宿主终端元素原点的
 * 坐标，由 useTerminalInput 的 rAF 合帧更新）；下方放不下时翻转到光标行上方。
 * 历史项悬停显示删除按钮；AI 项固定列表底部。
 */
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { Clock, Delete, Lightning, MagicStick } from "@element-plus/icons-vue";
import type { SuggestionItem } from "@/composables/useSuggestions";

const props = defineProps<{
  visible: boolean;
  items: SuggestionItem[];
  selectedIndex: number;
  /** 光标像素位置（相对宿主终端元素原点）。 */
  cursorX: number;
  cursorY: number;
}>();

const emit = defineEmits<{
  select: [index: number];
  remove: [id: number];
}>();

const popupRef = ref<HTMLDivElement | null>(null);
const listRef = ref<HTMLDivElement | null>(null);
const screenPos = ref({ x: 0, y: 0 });
const mouseMoved = ref(false);

function onGlobalMouseMove() {
  mouseMoved.value = true;
}

/** 键盘导航时鼠标悬停不抢选中（触屏/误碰保护）：鼠标真正移动过才响应 hover。 */
function onHover(index: number) {
  if (!mouseMoved.value) return;
  emit("select", index);
}

onMounted(() => {
  window.addEventListener("mousemove", onGlobalMouseMove, { passive: true });
});

onBeforeUnmount(() => {
  window.removeEventListener("mousemove", onGlobalMouseMove);
});

const popupStyle = computed(() => ({
  position: "fixed" as const,
  left: `${screenPos.value.x}px`,
  top: `${screenPos.value.y}px`,
}));

/** 非 AI 项（带 index，指向原始 items 下标）。 */
const listItems = computed(() => {
  const out: { item: SuggestionItem; index: number }[] = [];
  props.items.forEach((item, index) => {
    if (item.type !== "ai-preview" && item.type !== "ai-result") out.push({ item, index });
  });
  return out;
});

const aiItem = computed(() => {
  const index = props.items.findIndex((i) => i.type === "ai-preview" || i.type === "ai-result");
  return index >= 0 ? { item: props.items[index], index } : null;
});

/** 计算屏幕坐标：宿主容器（.xterm-wrap）rect + 光标相对坐标；下方空间不足时上翻。 */
function adjustPosition() {
  nextTick(() => {
    const popupEl = popupRef.value;
    const host = popupEl?.closest(".xterm-wrap") as HTMLElement | null;
    if (!popupEl || !host) return;
    if (!props.visible) {
      screenPos.value = { x: 0, y: 0 };
      return;
    }
    const hostRect = host.getBoundingClientRect();
    const popupRect = popupEl.getBoundingClientRect();
    const cursorScreenX = hostRect.left + props.cursorX;
    const cursorScreenY = hostRect.top + props.cursorY;
    // 下方空间（光标行底到容器底）放得下 → 光标下方 4px；否则整体上移到光标行上方。
    const spaceBelow = hostRect.bottom - cursorScreenY - 4;
    let y = cursorScreenY + 4;
    if (popupRect.height + 4 > spaceBelow) {
      y = cursorScreenY - popupRect.height - 4;
      if (y < hostRect.top) y = hostRect.top; // 上方也放不下：贴容器顶
    }
    screenPos.value = {
      x: Math.min(cursorScreenX, Math.max(hostRect.left, hostRect.right - popupRect.width - 4)),
      y,
    };
  });
}

watch(
  () => [props.visible, props.cursorX, props.cursorY, props.items.length],
  ([visible]) => {
    // 每次重新弹出重置：首次 hover 前必须有一次真实鼠标移动。
    if (visible) mouseMoved.value = false;
    adjustPosition();
  },
  { immediate: true },
);

// 选中项滚入视野（键盘导航）；AI 项固定底部不参与滚动。
watch(
  () => props.selectedIndex,
  (idx) => {
    nextTick(() => {
      const item = props.items[idx];
      if (!item || item.type === "ai-preview" || item.type === "ai-result") return;
      const el = popupRef.value?.querySelectorAll(".suggestion-item")[listIndexof(idx)];
      el?.scrollIntoView({ block: "nearest" });
    });
  },
);

/** 原始 items 下标 → 列表内 DOM 下标（AI 项不在列表 DOM 中）。 */
function listIndexof(originalIndex: number): number {
  let n = 0;
  for (let i = 0; i < originalIndex && i < props.items.length; i++) {
    const t = props.items[i].type;
    if (t !== "ai-preview" && t !== "ai-result") n++;
  }
  return n;
}

// 列表内容变化时回到顶部（新一轮输入刷新）。
watch(
  () => props.items,
  () => {
    nextTick(() => {
      if (listRef.value) listRef.value.scrollTop = 0;
    });
  },
  { deep: true },
);
</script>

<style scoped lang="scss">
.terminal-suggestion-popup {
  position: fixed;
  z-index: 3000;
  min-width: 220px;
  max-width: 480px;
  max-height: 220px;
  overflow: hidden;
  display: flex;
  flex-direction: column;
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 8px;
  box-shadow:
    0 8px 24px rgba(0, 0, 0, 0.16),
    0 2px 8px rgba(0, 0, 0, 0.08);
}

.suggestion-list {
  overflow-y: auto;
  flex: 1;
  min-height: 0;
  padding-top: 4px;
}

.suggestion-item {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 5px 10px;
  font-size: 13px;
  font-family: Consolas, "Cascadia Code", "Courier New", monospace;
  color: var(--el-text-color-primary);
  cursor: pointer;
  user-select: none;
  transition: background-color 0.1s ease;
}

.suggestion-item:hover,
.suggestion-item.selected {
  background: var(--el-fill-color);
}

.suggestion-item.selected {
  box-shadow: inset 2px 0 0 var(--el-color-primary);
}

.suggestion-icon {
  display: flex;
  color: var(--el-text-color-secondary);
  flex-shrink: 0;

  &.ai {
    color: var(--el-color-primary);
  }
}

.suggestion-label {
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;

  .qc-name {
    display: block;
    line-height: 1.3;
    color: var(--el-text-color-primary);
  }

  .qc-cmd {
    display: block;
    font-size: 11px;
    line-height: 1.3;
    color: var(--el-text-color-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
}

.match-char {
  color: var(--el-color-primary);
  font-weight: 600;
}

.suggestion-delete {
  display: none;
  align-items: center;
  justify-content: center;
  width: 18px;
  height: 18px;
  border: none;
  background: transparent;
  color: var(--el-text-color-secondary);
  cursor: pointer;
  border-radius: 4px;
  flex-shrink: 0;
  padding: 0;
}

.suggestion-item:hover .suggestion-delete {
  display: flex;
}

.suggestion-delete:hover {
  background: var(--el-color-danger-light-9);
  color: var(--el-color-danger);
}

.ai-fixed {
  flex-shrink: 0;
  border-top: 1px solid var(--el-border-color-lighter);
  color: var(--el-text-color-secondary);
}

.suggestion-item.ai-result {
  color: var(--el-color-success);
}
</style>
