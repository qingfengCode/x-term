<!--
  McpLogPanel.vue — MCP 执行日志实时面板（终端风格）。

  展示指定 kind（ssh/db）最新日志文件的内容，轮询自动刷新，新内容自动滚动到底部。
  日志行按状态着色（OK 绿 / ERR 红 / REJECTED 橙），文件头信息单独呈现。

  与左侧配置卡片（McpInstancePanel）并排，构成 MCP 管理页的右半区。
-->
<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { ElMessage } from "element-plus";
import { Refresh, CopyDocument, Bottom } from "@element-plus/icons-vue";
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

const config = computed(() => (props.kind === "ssh" ? mcp.sshConfig : mcp.dbConfig));
const status = computed(() => (props.kind === "ssh" ? mcp.sshStatus : mcp.dbStatus));
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
    <!-- 面板头 -->
    <div class="log-head">
      <div class="head-left">
        <span class="live-dot" :class="{ on: live }" />
        <span class="head-title">执行日志</span>
        <span v-if="log.exists" class="head-file">{{ log.filename }}</span>
      </div>
      <div class="head-right">
        <el-tooltip content="新内容自动滚动到底部" placement="top">
          <el-button
            :type="autoScroll ? 'primary' : 'default'"
            :icon="Bottom"
            size="small"
            circle
            @click="autoScroll = !autoScroll"
          />
        </el-tooltip>
        <el-tooltip content="复制全部日志" placement="top">
          <el-button :icon="CopyDocument" size="small" circle @click="copy" />
        </el-tooltip>
        <el-tooltip content="立即刷新" placement="top">
          <el-button :icon="Refresh" size="small" circle :loading="loading" @click="fetchLog" />
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

<style scoped>
.log-panel {
  display: flex;
  flex-direction: column;
  height: 100%;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 8px;
  overflow: hidden;
  background: #161b22;
}

/* 头部 */
.log-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 8px 10px;
  background: #1c2330;
  border-bottom: 1px solid #2a3442;
  flex-shrink: 0;
}
.head-left {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.live-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: #57606a;
  flex-shrink: 0;
}
.live-dot.on {
  background: #3fb950;
  box-shadow: 0 0 6px rgba(63, 185, 80, 0.8);
  animation: pulse 1.6s ease-in-out infinite;
}
@keyframes pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.45; }
}
.head-title {
  font-size: 13px;
  font-weight: 600;
  color: #e6edf3;
}
.head-file {
  font-size: 11px;
  color: #7d8590;
  font-family: "Cascadia Code", Consolas, monospace;
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
  --el-button-bg-color: #2a3442;
  --el-button-border-color: #3a4656;
  --el-button-text-color: #adbac7;
  --el-button-hover-bg-color: #344050;
  --el-button-hover-border-color: #46536a;
  --el-button-hover-text-color: #e6edf3;
}

/* 滚动区 */
.log-scroller {
  flex: 1;
  overflow-y: auto;
  padding: 10px 12px 16px;
  font-family: "Cascadia Code", Consolas, "Courier New", monospace;
  font-size: 12px;
  line-height: 1.7;
}
.log-scroller::-webkit-scrollbar {
  width: 8px;
}
.log-scroller::-webkit-scrollbar-thumb {
  background: #2f3a48;
  border-radius: 4px;
}

/* 空状态 */
.log-empty {
  text-align: center;
  padding: 60px 20px;
  color: #57606a;
}
.empty-glyph {
  font-size: 34px;
  opacity: 0.4;
  margin-bottom: 10px;
}
.empty-text {
  font-size: 14px;
  font-weight: 600;
  color: #7d8590;
  margin-bottom: 6px;
}
.empty-sub {
  font-size: 12px;
  color: #57606a;
  max-width: 280px;
  margin: 0 auto;
  line-height: 1.6;
}

/* 文件头 */
.log-meta {
  border: 1px solid #263040;
  border-left: 3px solid #388bfd66;
  background: #1a2230;
  border-radius: 4px;
  padding: 8px 10px;
  margin-bottom: 14px;
}
.meta-line {
  color: #7d8590;
  font-size: 11.5px;
  line-height: 1.6;
  white-space: pre-wrap;
}

/* 执行行 */
.log-line {
  display: flex;
  gap: 10px;
  padding: 1px 0;
  border-radius: 3px;
  transition: background 0.15s;
}
.log-line:hover {
  background: #1d2530;
}
.ln-time {
  color: #57606a;
  flex-shrink: 0;
  user-select: none;
}
.ln-body {
  white-space: pre-wrap;
  word-break: break-all;
}
.k-ok .ln-body { color: #7ee787; }
.k-err .ln-body { color: #ff7b72; }
.k-reject .ln-body { color: #ffa657; }
.k-info .ln-body { color: #adbac7; }

.log-idle {
  color: #57606a;
  text-align: center;
  padding: 30px 0;
  font-size: 12px;
}
</style>
