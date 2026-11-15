<!--
  McpLogPanel.vue — MCP 执行日志实时面板（终端风格）。

  展示指定 kind（ssh/db）最新日志文件的内容，轮询自动刷新，新内容自动滚动到底部。
  日志行按状态着色（OK 绿 / ERR 红 / REJECTED 橙），文件头信息单独呈现。

  与左侧配置卡片（McpInstancePanel）并排，构成 MCP 管理页的右半区。
-->
<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { ElMessage } from "element-plus";
import { useMcpStore } from "@/stores/mcp";
import { mcpLog } from "@/api/mcp";
import type { McpKind, McpLogContent } from "@/api/mcp";

const props = defineProps<{ kind: McpKind }>();

const mcp = useMcpStore();

const log = ref<McpLogContent>({ filename: "", content: "", exists: false });
const loading = ref(false);
/** 自动滚动到底部。 */
const autoScroll = ref(true);
const scrollerRef = ref<HTMLDivElement | null>(null);

const config = computed(() => {
  if (props.kind === "ssh") return mcp.sshConfig;
  if (props.kind === "db") return mcp.dbConfig;
  return mcp.fileConfig;
});
const status = computed(() => {
  if (props.kind === "ssh") return mcp.sshStatus;
  if (props.kind === "db") return mcp.dbStatus;
  return mcp.fileStatus;
});
/** 运行中且开启日志 → 显示 LIVE 指示。 */
const live = computed(() => status.value.running && config.value.enableLog);

/** 把日志内容拆成「文件头」与「执行行」两段。 */
const headerLines = computed(() => {
  const lines = log.value.content.split("\n");
  const dashIdx = lines.indexOf("---");
  return dashIdx >= 0 ? lines.slice(0, dashIdx) : [];
});
const bodyLines = computed(() => {
  const lines = log.value.content.split("\n");
  const dashIdx = lines.indexOf("---");
  const body = dashIdx >= 0 ? lines.slice(dashIdx + 1) : lines;
  return body.filter((l) => l.trim().length > 0);
});

/** 为一行判定着色类型。 */
function lineKind(line: string): "ok" | "err" | "reject" | "info" {
  if (line.includes("| OK")) return "ok";
  if (line.includes("| ERR")) return "err";
  if (line.includes("REJECTED")) return "reject";
  return "info";
}

/** 从行首提取时间戳 [yyyy-mm-dd HH:MM:SS]。 */
function lineTime(line: string): string {
  const m = line.match(/^\[([^\]]+)\]/);
  return m ? m[1] : "";
}
/** 去掉行首时间戳后的正文。 */
function lineBody(line: string): string {
  return line.replace(/^\[[^\]]+\]\s*/, "");
}

let timer: ReturnType<typeof setInterval> | null = null;

async function fetchLog() {
  try {
    const next = await mcpLog(props.kind, 500);
    const changed = next.content !== log.value.content;
    log.value = next;
    if (changed && autoScroll.value) {
      // 等 DOM 更新后滚动到底部。
      requestAnimationFrame(() => {
        const el = scrollerRef.value;
        if (el) el.scrollTop = el.scrollHeight;
      });
    }
  } catch {
    /* 静默：日志目录可能尚不存在 */
  }
}

/** 手动刷新（带按钮 loading 态；轮询路径不置位，避免指示灯每 1.5s 闪烁）。 */
async function refresh() {
  loading.value = true;
  try {
    await fetchLog();
  } finally {
    loading.value = false;
  }
}

async function copy() {
  if (!log.value.content) return;
  try {
    await navigator.clipboard.writeText(log.value.content);
    ElMessage.success("已复制日志");
  } catch {
    ElMessage.error("复制失败");
  }
}

function startPolling() {
  stopPolling();
  timer = setInterval(fetchLog, 1500);
}
function stopPolling() {
  if (timer) {
    clearInterval(timer);
    timer = null;
  }
}

onMounted(async () => {
  await fetchLog();
  startPolling();
});

// 切换 kind 时立即刷新一次。
watch(
  () => props.kind,
  async () => {
    await fetchLog();
  },
);

onBeforeUnmount(() => {
  stopPolling();
});
</script>

<template>
  <div class="log-panel">
    <!-- 面板头（终端窗口铬：红黄绿圆点 + 标题 + LIVE 徽标 + 工具按钮） -->
    <div class="log-head">
      <div class="head-left">
        <span class="mac-dots"><i class="md-r" /><i class="md-y" /><i class="md-g" /></span>
        <span class="head-title">执行日志</span>
        <span v-if="live" class="live-badge">LIVE</span>
        <span v-if="log.exists" class="head-file">{{ log.filename }}</span>
      </div>
      <div class="head-right">
        <el-tooltip content="新内容自动滚动到底部" placement="top">
          <el-button
            :type="autoScroll ? 'primary' : 'default'"
            :icon="'Bottom'"
            size="small"
            circle
            @click="autoScroll = !autoScroll"
          />
        </el-tooltip>
        <el-tooltip content="复制全部日志" placement="top">
          <el-button :icon="'CopyDocument'" size="small" circle @click="copy" />
        </el-tooltip>
        <el-tooltip content="立即刷新" placement="top">
          <el-button :icon="'Refresh'" size="small" circle :loading="loading" @click="refresh" />
        </el-tooltip>
      </div>
    </div>

    <!-- 日志主体（终端风格） -->
    <div ref="scrollerRef" class="log-scroller">
      <!-- 无日志 -->
      <div v-if="!log.exists" class="log-empty">
        <div class="empty-glyph">▤</div>
        <div class="empty-text">暂无日志</div>
        <div class="empty-sub">
          启动该 MCP 服务（并开启「记录执行日志」）后，执行记录会显示在这里。
        </div>
      </div>

      <template v-else>
        <!-- 文件头信息 -->
        <div v-if="headerLines.length" class="log-meta">
          <div v-for="h in headerLines" :key="h" class="meta-line">{{ h }}</div>
        </div>

        <!-- 执行记录 -->
        <div v-if="bodyLines.length" class="log-lines">
          <div
            v-for="(ln, i) in bodyLines"
            :key="i"
            class="log-line"
            :class="`k-${lineKind(ln)}`"
          >
            <span class="ln-time">{{ lineTime(ln) }}</span>
            <span class="ln-body">{{ lineBody(ln) }}</span>
          </div>
        </div>

        <!-- 有文件但无执行记录 -->
        <div v-else class="log-idle">— 尚无执行记录，等待外部客户端调用 —</div>
      </template>
    </div>
  </div>
</template>

<style scoped lang="scss">
.log-panel {
  display: flex;
  flex-direction: column;
  height: 100%;
  border: 1px solid var(--app-console-border);
  border-radius: var(--app-radius-lg);
  overflow: hidden;
  background: var(--app-console-bg);
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.12);
}

/* 头部（mac 窗口铬） */
.log-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 6px 8px 6px 10px;
  background: var(--app-console-head);
  border-bottom: 1px solid var(--app-console-border);
  flex-shrink: 0;
  user-select: none;
}
.head-left {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.mac-dots {
  display: inline-flex;
  gap: 6px;
  flex-shrink: 0;
  margin-right: 2px;
}
.mac-dots i {
  width: 10px;
  height: 10px;
  border-radius: 50%;
}
.md-r { background: var(--app-console-dot-r); }
.md-y { background: var(--app-console-dot-y); }
.md-g { background: var(--app-console-dot-g); }
.head-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--app-console-text);
  white-space: nowrap;
}
/* LIVE 徽标：绿色胶囊 + 呼吸圆点 */
.live-badge {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 10px;
  font-weight: 700;
  letter-spacing: 1px;
  color: var(--app-console-live);
  background: color-mix(in srgb, var(--app-console-live) 12%, transparent);
  border: 1px solid color-mix(in srgb, var(--app-console-live) 35%, transparent);
  padding: 1px 7px;
  border-radius: var(--app-radius-lg);
  font-family: var(--app-font-mono);
  flex-shrink: 0;
}
.live-badge::before {
  content: "";
  width: 5px;
  height: 5px;
  border-radius: 50%;
  background: var(--app-console-live);
  animation: badge-pulse 1.6s ease-in-out infinite;
}
@keyframes badge-pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.35; }
}
.head-file {
  font-size: 11px;
  color: var(--app-console-muted);
  font-family: var(--app-font-mono);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.head-right {
  display: flex;
  gap: 4px;
}
/* 深色背景下的按钮调暗 */
.log-head :deep(.el-button) {
  --el-button-bg-color: var(--app-console-border);
  --el-button-border-color: var(--app-console-btn-border);
  --el-button-text-color: var(--app-console-info);
  --el-button-hover-bg-color: var(--app-console-btn-hover-bg);
  --el-button-hover-border-color: var(--app-console-btn-hover-border);
  --el-button-hover-text-color: var(--app-console-text);
}
.log-head :deep(.el-button--primary) {
  --el-button-bg-color: var(--app-console-primary-bg);
  --el-button-border-color: var(--app-console-primary-border);
  --el-button-text-color: var(--app-console-primary-text);
  --el-button-hover-bg-color: var(--app-console-primary-hover-bg);
  --el-button-hover-border-color: var(--app-console-primary-hover-border);
  --el-button-hover-text-color: #ffffff;
}

/* 滚动区 */
.log-scroller {
  flex: 1;
  overflow-y: auto;
  padding: 8px 10px 12px;
  font-family: var(--app-font-mono);
  font-size: 12px;
  line-height: 1.65;
}
.log-scroller::-webkit-scrollbar {
  width: var(--app-scrollbar);
}
.log-scroller::-webkit-scrollbar-thumb {
  background: var(--app-console-scrollbar);
  border-radius: var(--app-radius-xs);
}

/* 空状态 */
.log-empty {
  text-align: center;
  padding: 48px 20px;
  color: var(--app-console-faint);
}
.empty-glyph {
  font-size: 30px;
  opacity: 0.4;
  margin-bottom: 8px;
}
.empty-text {
  font-size: 14px;
  font-weight: 600;
  color: var(--app-console-muted);
  margin-bottom: 6px;
}
.empty-sub {
  font-size: 12px;
  color: var(--app-console-faint);
  max-width: 280px;
  margin: 0 auto;
  line-height: 1.6;
}

/* 文件头 */
.log-meta {
  border: 1px solid var(--app-console-border);
  border-left: 3px solid color-mix(in srgb, var(--app-console-accent) 40%, transparent);
  background: var(--app-console-meta);
  border-radius: var(--app-radius-xs);
  padding: 6px 9px;
  margin-bottom: 12px;
}
.meta-line {
  color: var(--app-console-muted);
  font-size: 12px;
  line-height: 1.6;
  white-space: pre-wrap;
}

/* 执行行 */
.log-line {
  display: flex;
  gap: 10px;
  padding: 1px 4px;
  margin: 0 -4px;
  border-radius: var(--app-radius-xs);
  transition: background 0.15s;
}
.log-line:hover {
  background: var(--app-console-hover);
}
.ln-time {
  color: var(--app-console-faint);
  flex-shrink: 0;
  user-select: none;
}
.ln-body {
  white-space: pre-wrap;
  word-break: break-all;
}
.k-ok .ln-body { color: var(--app-console-success); }
.k-err .ln-body { color: var(--app-console-danger); }
.k-reject .ln-body { color: var(--app-console-warning); }
.k-info .ln-body { color: var(--app-console-info); }

.log-idle {
  color: var(--app-console-faint);
  text-align: center;
  padding: 30px 0;
  font-size: 12px;
}
</style>
