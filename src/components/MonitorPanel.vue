<template>
  <div class="monitor-panel">
    <!-- 状态条 -->
    <div v-if="!data && !closedReason" class="monitor-empty">正在连接并采集数据…</div>
    <div v-else-if="closedReason" class="monitor-closed">
      <el-icon><WarningFilled /></el-icon>
      监控已结束：{{ closedReason }}
      <el-button size="small" type="primary" plain @click="restart">重新连接</el-button>
    </div>

    <template v-if="data">
      <!-- 主机信息 -->
      <div class="host-card" v-if="data.host">
        <div class="host-name">
          <el-icon><Monitor /></el-icon>
          {{ data.host.hostname }}
        </div>
        <div class="host-meta">
          <span>{{ data.host.os || "未知系统" }}</span>
          <el-divider direction="vertical" />
          <span>{{ data.host.kernel || "未知内核" }}</span>
          <el-divider direction="vertical" />
          <span>{{ data.cpu.cores || "?" }} 核</span>
          <el-divider direction="vertical" />
          <span>运行 {{ formatUptime(data.uptimeSecs) }}</span>
        </div>
      </div>

      <!-- CPU + 内存 + 网络 -->
      <div class="metric-grid">
        <div class="metric-card">
          <div class="metric-head">
            <span class="metric-title">CPU</span>
            <span class="metric-value" :class="cpuClass">{{ formatPct(data.cpu.usagePct) }}</span>
          </div>
          <Sparkline :points="cpuHistory" color="#409eff" />
          <div class="metric-sub">
            负载 {{ data.load ? `${data.load.m1.toFixed(2)} / ${data.load.m5.toFixed(2)} / ${data.load.m15.toFixed(2)}` : "--" }}
          </div>
        </div>

        <div class="metric-card">
          <div class="metric-head">
            <span class="metric-title">内存</span>
            <span class="metric-value">{{ formatSize(data.mem.usedKb * 1024) }} / {{ formatSize(data.mem.totalKb * 1024) }}</span>
          </div>
          <el-progress :percentage="roundPct(data.mem.usedPct)" :stroke-width="10" :color="pctColor(data.mem.usedPct)" />
          <div class="metric-sub" v-if="data.mem.swapTotalKb > 0">
            Swap {{ formatSize((data.mem.swapTotalKb - data.mem.swapFreeKb) * 1024) }} /
            {{ formatSize(data.mem.swapTotalKb * 1024) }}
           （{{ data.mem.swapUsedPct != null ? data.mem.swapUsedPct.toFixed(0) + "%" : "--" }}）
          </div>
          <div class="metric-sub" v-else>Swap 未启用</div>
        </div>

        <div class="metric-card">
          <div class="metric-head">
            <span class="metric-title">网络</span>
            <span class="metric-value">↓{{ formatSpeed(data.net.rxKbps) }} ↑{{ formatSpeed(data.net.txKbps) }}</span>
          </div>
          <Sparkline :points="netHistory" color="#67c23a" />
          <div class="metric-sub">
            累计 ↓{{ formatSize(data.net.rxTotal) }} ↑{{ formatSize(data.net.txTotal) }}
          </div>
        </div>
      </div>

      <!-- 磁盘 -->
      <div class="section" v-if="data.disks.length">
        <div class="section-title">磁盘</div>
        <div v-for="d in data.disks" :key="d.mount" class="disk-row">
          <el-tooltip :content="`${d.fs} → ${d.mount}`" placement="top">
            <span class="disk-mount">{{ d.mount }}</span>
          </el-tooltip>
          <el-progress
            class="disk-bar"
            :percentage="d.usedPct"
            :stroke-width="8"
            :color="pctColor(d.usedPct)"
          />
          <span class="disk-detail">
            {{ formatSize(d.usedKb * 1024) }} / {{ formatSize(d.totalKb * 1024) }}
          </span>
        </div>
      </div>

      <!-- 进程 -->
      <div class="section" v-if="data.processes.length">
        <div class="section-title">进程 Top CPU</div>
        <el-table :data="data.processes" size="small" height="240">
          <el-table-column prop="pid" label="PID" width="70" />
          <el-table-column prop="user" label="用户" width="100" show-overflow-tooltip />
          <el-table-column prop="cpuPct" label="CPU%" width="70">
            <template #default="{ row }">{{ row.cpuPct.toFixed(1) }}</template>
          </el-table-column>
          <el-table-column prop="memPct" label="MEM%" width="70">
            <template #default="{ row }">{{ row.memPct.toFixed(1) }}</template>
          </el-table-column>
          <el-table-column prop="comm" label="进程" min-width="140" show-overflow-tooltip />
        </el-table>
      </div>
    </template>
  </div>
</template>

<script setup lang="ts">
/**
 * 服务器监控面板（Drawer 内容）。
 *
 * 挂载时 monitor_start（一条专用 SSH 连接 + 3s 采集），按 monitorId 过滤
 * monitor:data / monitor:closed 事件；卸载时 monitor_stop。CPU 与网络保留
 * 最近 60 个采样点画 sparkline。
 */
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { Monitor, WarningFilled } from "@element-plus/icons-vue";
import * as monitorApi from "@/api/monitor";
import type { MonitorDataEvent } from "@/api/monitor";
import { formatSize } from "@/utils/format";

const props = defineProps<{ sessionConfigId: string }>();

const data = ref<MonitorDataEvent | null>(null);
const closedReason = ref<string | null>(null);
const cpuHistory = ref<number[]>([]);
const netHistory = ref<number[]>([]);

let monitorId: string | null = null;
let unlistens: UnlistenFn[] = [];
let unmounted = false;
// 启动代际：卸载/restart 会使旧代 start 作废——旧 await 返回后立即回收
// 刚建立的后端监控连接，不再注册监听（否则连接与监听器永久泄漏）。
let startGen = 0;

const HISTORY_CAP = 60;

async function start() {
  closedReason.value = null;
  data.value = null;
  cpuHistory.value = [];
  netHistory.value = [];
  const gen = ++startGen;
  let id: string;
  try {
    id = await monitorApi.monitorStart(props.sessionConfigId);
  } catch (e) {
    closedReason.value = String(e);
    return;
  }
  // 等待建连期间组件已卸载或已被新一轮 start 取代。
  if (unmounted || gen !== startGen) {
    void monitorApi.monitorStop(id).catch(() => {});
    return;
  }
  monitorId = id;
  unlistens.push(
    await listen<MonitorDataEvent>("monitor:data", (e) => {
      if (unmounted || e.payload.monitorId !== monitorId) return;
      data.value = e.payload;
      closedReason.value = null;
      const cpu = e.payload.cpu.usagePct;
      if (cpu != null) {
        cpuHistory.value.push(cpu);
        if (cpuHistory.value.length > HISTORY_CAP) cpuHistory.value.shift();
      }
      const rx = e.payload.net.rxKbps ?? 0;
      const tx = e.payload.net.txKbps ?? 0;
      netHistory.value.push(rx + tx);
      if (netHistory.value.length > HISTORY_CAP) netHistory.value.shift();
    }),
  );
  unlistens.push(
    await listen<monitorApi.MonitorClosedEvent>("monitor:closed", (e) => {
      if (unmounted || e.payload.monitorId !== monitorId) return;
      closedReason.value = e.payload.reason;
    }),
  );
}

function stop() {
  if (monitorId) {
    void monitorApi.monitorStop(monitorId).catch(() => {});
    monitorId = null;
  }
}

function restart() {
  stop();
  for (const u of unlistens.splice(0)) u();
  void start();
}

onMounted(() => void start());
onBeforeUnmount(() => {
  unmounted = true;
  stop();
  for (const u of unlistens.splice(0)) u();
});

// --- 格式化 ---
function formatPct(v?: number | null): string {
  return v == null ? "--" : `${v.toFixed(1)}%`;
}
function formatSpeed(kbps?: number | null): string {
  if (kbps == null) return "--";
  if (kbps < 1024) return `${kbps.toFixed(0)} KB/s`;
  return `${(kbps / 1024).toFixed(2)} MB/s`;
}
function formatUptime(secs: number): string {
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `${d} 天 ${h} 小时`;
  if (h > 0) return `${h} 小时 ${m} 分`;
  return `${m} 分钟`;
}
function roundPct(v: number): number {
  return Math.round(v);
}
function pctColor(pct: number): string {
  if (pct >= 90) return "#f56c6c";
  if (pct >= 75) return "#e6a23c";
  return "#409eff";
}
const cpuClass = computed(() => {
  const v = data.value?.cpu.usagePct;
  if (v == null) return "";
  return v >= 90 ? "danger" : v >= 75 ? "warn" : "";
});
</script>

<script lang="ts">
/** 迷你折线图（SVG polyline，无图表库依赖）。 */
import { defineComponent, h, type PropType } from "vue";

const Sparkline = defineComponent({
  name: "Sparkline",
  props: {
    points: { type: Array as PropType<number[]>, required: true },
    color: { type: String, default: "#409eff" },
  },
  setup(props) {
    return () => {
      const w = 200;
      const height = 36;
      const pts = props.points;
      if (pts.length < 2) {
        return h("svg", { class: "sparkline", viewBox: `0 0 ${w} ${height}`, preserveAspectRatio: "none" });
      }
      const max = Math.max(...pts, 1);
      const step = w / (pts.length - 1);
      const coords = pts
        .map((p, i) => `${(i * step).toFixed(1)},${(height - (p / max) * (height - 2) - 1).toFixed(1)}`)
        .join(" ");
      return h("svg", { class: "sparkline", viewBox: `0 0 ${w} ${height}`, preserveAspectRatio: "none" }, [
        h("polyline", {
          points: coords,
          fill: "none",
          stroke: props.color,
          "stroke-width": "1.5",
          "vector-effect": "non-scaling-stroke",
        }),
      ]);
    };
  },
});
export default { components: { Sparkline } };
</script>

<style scoped lang="scss">
.monitor-panel {
  display: flex;
  flex-direction: column;
  gap: 14px;
  padding: 4px 2px;
}

.monitor-empty {
  padding: 48px 0;
  text-align: center;
  color: var(--el-text-color-secondary);
  font-size: 13px;
}

.monitor-closed {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 14px;
  border-radius: 8px;
  background: var(--el-color-danger-light-9);
  color: var(--el-color-danger);
  font-size: 13px;
}

.host-card {
  padding: 12px 14px;
  border-radius: 10px;
  background: var(--el-fill-color-light);
}

.host-name {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 15px;
  font-weight: 600;
  color: var(--el-color-primary);
}

.host-meta {
  margin-top: 6px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}

.metric-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
  gap: 10px;
}

.metric-card {
  padding: 10px 12px;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 10px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.metric-head {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 8px;
}

.metric-title {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}

.metric-value {
  font-size: 15px;
  font-weight: 600;
  color: var(--el-text-color-primary);
  font-variant-numeric: tabular-nums;

  &.warn {
    color: var(--el-color-warning);
  }
  &.danger {
    color: var(--el-color-danger);
  }
}

.metric-sub {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}

:deep(.sparkline) {
  width: 100%;
  height: 36px;
  display: block;
}

.section-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--el-text-color-regular);
  margin-bottom: 8px;
}

.disk-row {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 4px 0;
}

.disk-mount {
  flex-shrink: 0;
  width: 110px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 12px;
  font-family: Consolas, monospace;
  color: var(--el-text-color-primary);
}

.disk-bar {
  flex: 1;
  min-width: 80px;
}

.disk-detail {
  flex-shrink: 0;
  width: 150px;
  text-align: right;
  font-size: 12px;
  color: var(--el-text-color-secondary);
  font-variant-numeric: tabular-nums;
}
</style>
