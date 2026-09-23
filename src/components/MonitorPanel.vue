<template>
  <div class="monitor-panel">
    <!-- 连接中 -->
    <div v-if="!data && !closedReason" class="monitor-state">
      <el-icon class="is-loading"><Loading /></el-icon>
      <span>正在建立监控连接并采集数据…</span>
    </div>
    <!-- 已结束 -->
    <div v-else-if="closedReason" class="monitor-closed">
      <el-icon><WarningFilled /></el-icon>
      <div class="closed-text">
        <div class="closed-title">监控已结束</div>
        <div class="closed-reason">{{ closedReason }}</div>
      </div>
      <el-button size="small" type="primary" plain @click="restart">重新连接</el-button>
    </div>

    <template v-if="data">
      <!-- 主机概览头 -->
      <header v-if="data.host" class="host-header">
        <div class="host-id">
          <span class="host-avatar"><el-icon><Monitor /></el-icon></span>
          <div class="host-info">
            <div class="host-name">{{ data.host.hostname }}</div>
            <div class="host-meta">
              <span class="chip">{{ data.host.os || "未知系统" }}</span>
              <el-tooltip :content="data.host.kernel || '未知内核'" placement="bottom">
                <span class="chip mono">{{ data.host.kernel || "未知内核" }}</span>
              </el-tooltip>
              <span class="chip">{{ data.cpu.cores || "?" }} 核</span>
              <span class="chip">运行 {{ formatUptime(data.uptimeSecs) }}</span>
            </div>
          </div>
        </div>
        <span v-if="!closedReason" class="live-badge">
          <span class="live-dot" />
          实时 · 3s
        </span>
      </header>

      <!-- KPI 指标卡 -->
      <div class="metric-grid">
        <div class="kpi-card">
          <div class="kpi-head">
            <span class="kpi-icon" style="--accent: #409eff"><el-icon><Cpu /></el-icon></span>
            <span class="kpi-title">CPU 使用率</span>
          </div>
          <div class="kpi-value" :class="pctClass(data.cpu.usagePct)">{{ formatPct(data.cpu.usagePct) }}</div>
          <Sparkline :points="cpuHistory" color="#409eff" />
          <div class="kpi-sub">
            负载
            {{ data.load ? `${data.load.m1.toFixed(2)} / ${data.load.m5.toFixed(2)} / ${data.load.m15.toFixed(2)}` : "--" }}
          </div>
        </div>

        <div class="kpi-card">
          <div class="kpi-head">
            <span class="kpi-icon" style="--accent: #8b5cf6"><el-icon><Coin /></el-icon></span>
            <span class="kpi-title">内存</span>
          </div>
          <div class="kpi-value" :class="pctClass(data.mem.usedPct)">
            {{ data.mem.usedPct.toFixed(0) }}<span class="kpi-unit">%</span>
          </div>
          <div class="bar">
            <div
              class="bar-fill"
              :style="{ width: clampPct(data.mem.usedPct) + '%', background: levelColor(data.mem.usedPct, '#8b5cf6') }"
            />
          </div>
          <div class="kpi-sub">
            {{ formatSize(data.mem.usedKb * 1024) }} / {{ formatSize(data.mem.totalKb * 1024) }}
          </div>
          <div class="kpi-sub dim">
            Swap
            {{ data.mem.swapTotalKb > 0
              ? `${formatSize((data.mem.swapTotalKb - data.mem.swapFreeKb) * 1024)} / ${formatSize(data.mem.swapTotalKb * 1024)}${data.mem.swapUsedPct != null ? `（${data.mem.swapUsedPct.toFixed(0)}%）` : ""}`
              : "未启用" }}
          </div>
        </div>

        <div class="kpi-card">
          <div class="kpi-head">
            <span class="kpi-icon" style="--accent: #67c23a"><el-icon><Connection /></el-icon></span>
            <span class="kpi-title">网络</span>
          </div>
          <div class="kpi-value net">
            <span class="net-item"><i class="arr rx">↓</i>{{ formatSpeed(data.net.rxKbps) }}</span>
            <span class="net-item"><i class="arr tx">↑</i>{{ formatSpeed(data.net.txKbps) }}</span>
          </div>
          <Sparkline :points="netHistory" color="#67c23a" />
          <div class="kpi-sub">累计 ↓{{ formatSize(data.net.rxTotal) }} · ↑{{ formatSize(data.net.txTotal) }}</div>
        </div>

        <div v-if="data.load" class="kpi-card">
          <div class="kpi-head">
            <span class="kpi-icon" style="--accent: #e6a23c"><el-icon><DataLine /></el-icon></span>
            <span class="kpi-title">系统负载</span>
          </div>
          <div class="kpi-value">{{ data.load.m1.toFixed(2) }}</div>
          <div class="bar">
            <div class="bar-fill" :style="{ width: loadPct + '%', background: '#e6a23c' }" />
          </div>
          <div class="kpi-sub">
            5m {{ data.load.m5.toFixed(2) }} · 15m {{ data.load.m15.toFixed(2) }} · 满载 {{ data.cpu.cores || "?" }}
          </div>
        </div>
      </div>

      <!-- 磁盘 + 进程：宽屏双栏，窄屏纵向堆叠 -->
      <div class="bottom-grid">
        <section v-if="data.disks.length" class="panel-card">
          <div class="panel-head">
            <span class="panel-mark" />
            <span class="panel-title">磁盘</span>
            <span class="panel-count">{{ data.disks.length }} 个挂载点</span>
          </div>
          <div class="disk-grid">
            <div v-for="d in data.disks" :key="d.mount" class="disk-row">
              <el-tooltip :content="`${d.fs} → ${d.mount} · 可用 ${formatSize(d.availKb * 1024)}`" placement="top">
                <span class="disk-mount">{{ d.mount }}</span>
              </el-tooltip>
              <div class="disk-bar">
                <div class="bar">
                  <div class="bar-fill" :style="{ width: clampPct(d.usedPct) + '%', background: levelColor(d.usedPct) }" />
                </div>
                <span class="disk-pct" :class="pctClass(d.usedPct)">{{ d.usedPct.toFixed(0) }}%</span>
              </div>
              <span class="disk-detail">
                {{ formatSize(d.usedKb * 1024) }} / {{ formatSize(d.totalKb * 1024) }}
              </span>
            </div>
          </div>
        </section>

        <section v-if="data.processes.length" class="panel-card">
          <div class="panel-head">
            <span class="panel-mark" />
            <span class="panel-title">进程 TOP</span>
            <span class="panel-count">按 CPU 排序</span>
          </div>
          <el-table class="proc-table" :data="data.processes" size="small" stripe :max-height="320">
            <el-table-column prop="pid" label="PID" width="72" />
            <el-table-column prop="user" label="用户" width="110" show-overflow-tooltip />
            <el-table-column prop="cpuPct" label="CPU" width="80" align="right">
              <template #default="{ row }">
                <span class="num" :class="pctClass(row.cpuPct)">{{ row.cpuPct.toFixed(1) }}%</span>
              </template>
            </el-table-column>
            <el-table-column prop="memPct" label="内存" width="80" align="right">
              <template #default="{ row }">
                <span class="num">{{ row.memPct.toFixed(1) }}%</span>
              </template>
            </el-table-column>
            <el-table-column prop="comm" label="命令" min-width="160" show-overflow-tooltip>
              <template #default="{ row }">
                <span class="mono">{{ row.comm }}</span>
              </template>
            </el-table-column>
          </el-table>
        </section>
      </div>
    </template>
  </div>
</template>

<script setup lang="ts">
/**
 * 服务器监控面板（监控页签内容，Workspace 常驻 v-show）。
 *
 * 挂载时 monitor_start（一条专用 SSH 连接 + 3s 采集），按 monitorId 过滤
 * monitor:data / monitor:closed 事件；卸载时 monitor_stop。CPU 与网络保留
 * 最近 60 个采样点画 sparkline（面积渐变）。
 */
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
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
function clampPct(v: number | null | undefined): number {
  if (v == null || Number.isNaN(v)) return 0;
  return Math.min(100, Math.max(0, v));
}
/** 阈值着色（文本）：>=90 红，>=75 橙，其余默认。 */
function pctClass(v?: number | null): string {
  if (v == null) return "";
  return v >= 90 ? "danger" : v >= 75 ? "warn" : "";
}
/** 阈值着色（进度条）：达到阈值用警示色，否则用指标主题色。 */
function levelColor(v: number | null | undefined, ok = "#409eff"): string {
  if (v == null) return ok;
  if (v >= 90) return "#f56c6c";
  if (v >= 75) return "#e6a23c";
  return ok;
}
/** 1m 负载相对核数的占用（封顶 100%），驱动负载卡进度条。 */
const loadPct = computed(() => {
  const cores = data.value?.cpu.cores || 1;
  const m1 = data.value?.load?.m1;
  if (m1 == null) return 0;
  return clampPct((m1 / cores) * 100);
});
</script>

<script lang="ts">
/** 迷你面积折线图（SVG polyline + 渐变填充，无图表库依赖）。 */
import { defineComponent, h, type PropType } from "vue";

let sparkUid = 0;

const Sparkline = defineComponent({
  name: "Sparkline",
  props: {
    points: { type: Array as PropType<number[]>, required: true },
    color: { type: String, default: "#409eff" },
  },
  setup(props) {
    const gid = `spark-grad-${++sparkUid}`;
    return () => {
      const w = 200;
      const height = 38;
      const pts = props.points;
      if (pts.length < 2) {
        return h("svg", { class: "sparkline", viewBox: `0 0 ${w} ${height}`, preserveAspectRatio: "none" });
      }
      const max = Math.max(...pts, 1);
      const step = w / (pts.length - 1);
      const y = (p: number) => height - (p / max) * (height - 4) - 2;
      const coords = pts
        .map((p, i) => `${(i * step).toFixed(1)},${y(p).toFixed(1)}`)
        .join(" ");
      const area = `0,${height} ${coords} ${w},${height}`;
      return h("svg", { class: "sparkline", viewBox: `0 0 ${w} ${height}`, preserveAspectRatio: "none" }, [
        h("defs", [
          h("linearGradient", { id: gid, x1: "0", y1: "0", x2: "0", y2: "1" }, [
            h("stop", { offset: "0%", "stop-color": props.color, "stop-opacity": "0.22" }),
            h("stop", { offset: "100%", "stop-color": props.color, "stop-opacity": "0" }),
          ]),
        ]),
        h("polygon", { points: area, fill: `url(#${gid})` }),
        h("polyline", {
          points: coords,
          fill: "none",
          stroke: props.color,
          "stroke-width": "1.5",
          "stroke-linejoin": "round",
          "stroke-linecap": "round",
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
  gap: 12px;
  width: 100%;
  max-width: 1240px;
  margin: 0 auto;
}

/* --- 状态占位 ---------------------------------------------------------- */
.monitor-state {
  min-height: 280px;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  color: var(--el-text-color-secondary);
  font-size: 13px;

  .el-icon {
    font-size: 22px;
    color: var(--el-color-primary);
  }
}

.monitor-closed {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 14px 16px;
  border-radius: 10px;
  background: var(--el-color-danger-light-9);
  color: var(--el-color-danger);

  > .el-icon {
    font-size: 22px;
    flex-shrink: 0;
  }

  .closed-text {
    flex: 1;
    min-width: 0;
  }

  .closed-title {
    font-size: 13px;
    font-weight: 600;
  }

  .closed-reason {
    margin-top: 2px;
    font-size: 12px;
    opacity: 0.85;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
}

/* --- 主机概览头 -------------------------------------------------------- */
.host-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 14px 16px;
  background: var(--el-bg-color);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 12px;
}

.host-id {
  display: flex;
  align-items: center;
  gap: 12px;
  min-width: 0;
}

.host-avatar {
  width: 38px;
  height: 38px;
  border-radius: 10px;
  flex-shrink: 0;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 19px;
  color: #fff;
  background: linear-gradient(135deg, #409eff, #2f6fed);
}

.host-info {
  min-width: 0;
}

.host-name {
  font-size: 16px;
  font-weight: 700;
  color: var(--el-text-color-primary);
  line-height: 1.3;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.host-meta {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-top: 5px;
  flex-wrap: wrap;
}

.chip {
  display: inline-block;
  max-width: 280px;
  padding: 2px 8px;
  border-radius: 6px;
  background: var(--el-fill-color);
  font-size: 12px;
  line-height: 18px;
  color: var(--el-text-color-secondary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  vertical-align: bottom;

  &.mono {
    font-family: Consolas, ui-monospace, monospace;
  }
}

.live-badge {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  flex-shrink: 0;
  padding: 4px 10px;
  border-radius: 999px;
  font-size: 12px;
  color: var(--el-color-success);
  background: var(--el-color-success-light-9);
  border: 1px solid rgba(103, 194, 58, 0.35);
}

.live-dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: var(--el-color-success);
  animation: live-pulse 1.8s ease-out infinite;
}

@keyframes live-pulse {
  0% {
    box-shadow: 0 0 0 0 rgba(103, 194, 58, 0.5);
  }
  70% {
    box-shadow: 0 0 0 6px rgba(103, 194, 58, 0);
  }
  100% {
    box-shadow: 0 0 0 0 rgba(103, 194, 58, 0);
  }
}

/* --- KPI 指标卡 -------------------------------------------------------- */
.metric-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(230px, 1fr));
  gap: 12px;
}

.kpi-card {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 12px 14px 10px;
  background: var(--el-bg-color);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 12px;
  transition: box-shadow 0.2s ease, border-color 0.2s ease;

  &:hover {
    border-color: var(--el-border-color);
    box-shadow: var(--el-box-shadow-light);
  }

  // 末行说明贴底，参差内容也保持底边对齐
  > .kpi-sub:last-child {
    margin-top: auto;
  }
}

.kpi-head {
  display: flex;
  align-items: center;
  gap: 8px;
}

.kpi-icon {
  width: 26px;
  height: 26px;
  border-radius: 8px;
  flex-shrink: 0;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 14px;
  color: var(--accent, var(--el-color-primary));
  background: color-mix(in srgb, var(--accent, var(--el-color-primary)) 12%, transparent);
}

.kpi-title {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.kpi-value {
  font-size: 24px;
  font-weight: 700;
  line-height: 1.15;
  color: var(--el-text-color-primary);
  font-variant-numeric: tabular-nums;

  .kpi-unit {
    font-size: 13px;
    font-weight: 600;
    margin-left: 2px;
    color: var(--el-text-color-secondary);
  }

  &.warn {
    color: var(--el-color-warning);
  }

  &.danger {
    color: var(--el-color-danger);
  }

  &.net {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 14px;
    font-size: 17px;

    .arr {
      font-style: normal;
      margin-right: 4px;

      &.rx {
        color: #409eff;
      }

      &.tx {
        color: #67c23a;
      }
    }
  }
}

.kpi-sub {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  font-variant-numeric: tabular-nums;

  &.dim {
    opacity: 0.75;
  }
}

:deep(.sparkline) {
  width: 100%;
  height: 38px;
  display: block;
}

/* --- 细进度条 ---------------------------------------------------------- */
.bar {
  height: 8px;
  border-radius: 999px;
  background: var(--el-fill-color-dark);
  overflow: hidden;
}

.bar-fill {
  height: 100%;
  border-radius: 999px;
  transition: width 0.4s ease;
}

/* --- 磁盘 / 进程面板 --------------------------------------------------- */
.bottom-grid {
  display: grid;
  grid-template-columns: minmax(0, 1fr);
  gap: 12px;

  @media (min-width: 1280px) {
    grid-template-columns: minmax(0, 5fr) minmax(0, 7fr);
  }
}

.panel-card {
  padding: 12px 16px 14px;
  background: var(--el-bg-color);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 12px;
  min-width: 0;
}

.panel-head {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 10px;
}

.panel-mark {
  width: 3px;
  height: 14px;
  border-radius: 2px;
  background: var(--el-color-primary);
}

.panel-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--el-text-color-regular);
}

.panel-count {
  margin-left: auto;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}

.disk-grid {
  display: grid;
  gap: 10px;
}

.disk-row {
  display: grid;
  grid-template-columns: minmax(90px, 150px) minmax(0, 1fr) 150px;
  align-items: center;
  gap: 10px;
}

.disk-mount {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 12px;
  font-family: Consolas, ui-monospace, monospace;
  color: var(--el-text-color-primary);
}

.disk-bar {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;

  .bar {
    flex: 1;
  }
}

.disk-pct {
  flex-shrink: 0;
  width: 38px;
  text-align: right;
  font-size: 12px;
  font-variant-numeric: tabular-nums;
  color: var(--el-text-color-regular);

  &.warn {
    color: var(--el-color-warning);
  }

  &.danger {
    color: var(--el-color-danger);
  }
}

.disk-detail {
  text-align: right;
  font-size: 12px;
  color: var(--el-text-color-secondary);
  font-variant-numeric: tabular-nums;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* --- 进程表 ------------------------------------------------------------ */
.proc-table {
  // 表头底色（CSS 变量直接作用于 el-table 根元素）
  --el-table-header-bg-color: var(--el-fill-color-light);

  .num {
    font-variant-numeric: tabular-nums;

    &.warn {
      color: var(--el-color-warning);
    }

    &.danger {
      color: var(--el-color-danger);
    }
  }

  .mono {
    font-family: Consolas, ui-monospace, monospace;
    font-size: 12px;
  }
}
</style>
