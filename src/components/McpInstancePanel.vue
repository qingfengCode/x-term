<!--
  McpInstancePanel.vue — 单个 MCP 实例（SSH / DB / File）的配置与管理面板。

  作为 McpView 的三个 Tab 共用组件，按 kind 区分：
  - kind="ssh"：对外暴露 exec_ssh + 文件工具，绑定一个 SSH 会话。
  - kind="db"：对外暴露 exec_sql，绑定一个 DB profile。
  - kind="file"：对外暴露 list_files/upload_file/download_file，绑定一个 S3 文件账号（仅 bound 模式）。

  资源模式（resourceMode）：
  - "bound"（默认）：绑定资源为 SSH 会话 / DB profile / S3 文件账号，工具只传 command/sql/path 等。
  - "client"（客户端直连）：免绑定实例，调用方在工具参数中传
    host/port/username/password，凭据即用即弃、不存储不落日志。
    （File MCP 不支持 client 模式）

  功能：
  - 资源模式开关（直连模式隐藏绑定 UI 并展示安全提示；File kind 隐藏此开关）。
  - 绑定资源下拉（启动前/后均可改；运行中改后提示"需重启生效"）。
  - 监听地址（默认 0.0.0.0）与端口可编辑。
  - token 生成 / 复制。
  - 启停按钮 + 运行状态徽标 + SSE 端点。
  - 客户端配置 JSON（一键复制）。
  - 0.0.0.0 安全提示。
-->
<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { ElMessage } from "element-plus";
import { VideoPlay, VideoPause, Refresh, CopyDocument, Key } from "@element-plus/icons-vue";
import { useMcpStore } from "@/stores/mcp";
import { useSessionsStore } from "@/stores/sessions";
import { dbListProfiles, dbConnect, dbListDatabases, dbDisconnect } from "@/api/db";
import { fileAccountList } from "@/api/fileBackend";
import type { FileAccount } from "@/api/fileBackend";
import type { McpKind } from "@/api/mcp";
import type { DbProfile, Session } from "@/api/types";

const props = defineProps<{ kind: McpKind }>();

const mcp = useMcpStore();
const sessions = useSessionsStore();

/** 该 kind 的可用资源列表（ssh→SSH 会话；db→DB profile；file→S3 文件账号）。 */
const sshSessions = ref<Session[]>([]);
const dbProfiles = ref<DbProfile[]>([]);
const fileAccounts = ref<FileAccount[]>([]);

/** DB MCP 专用：绑定 profile 后可选的数据库列表。 */
const databases = ref<string[]>([]);
const loadingDbs = ref(false);

const isSsh = computed(() => props.kind === "ssh");
const isFile = computed(() => props.kind === "file");
/** kind 中文标题。 */
const title = computed(() => (isSsh.value ? "SSH MCP" : isFile.value ? "File MCP" : "DB MCP"));
/** 该 kind 对外暴露的工具说明（按资源模式分支）。 */
const toolHint = computed(() => {
  if (isFile.value) {
    return "对外暴露 list_files(path) / upload_file(localPath, remotePath) / download_file(remotePath, localPath)，\
目标即下方绑定的 S3 文件账号。File MCP 仅支持绑定模式。";
  }
  if (clientMode.value) {
    return isSsh.value
      ? "对外暴露 exec_ssh(host/port/username/password/command)：目标服务器由调用方在参数中指定，免绑定本地实例。"
      : "对外暴露 exec_sql(host/port/username/password/database/sql)：目标数据库由调用方在参数中指定，免绑定本地实例。";
  }
  return isSsh.value
    ? "对外暴露 exec_ssh(command)，目标服务器即下方绑定的 SSH 会话。"
    : "对外暴露 exec_sql(sql)，目标数据库即下方绑定的连接。";
});

const config = computed(() => {
  if (isSsh.value) return mcp.sshConfig;
  if (isFile.value) return mcp.fileConfig;
  return mcp.dbConfig;
});
const status = computed(() => {
  if (isSsh.value) return mcp.sshStatus;
  if (isFile.value) return mcp.fileStatus;
  return mcp.dbStatus;
});
const loading = computed(() => mcp.loading[props.kind]);

/** 客户端直连模式（免绑定实例）：目标与账密由调用方在工具参数中传入。File 不支持。 */
const clientMode = computed(() => !isFile.value && config.value.resourceMode === "client");

/** 该 kind 是否有可用资源可选。 */
const hasResources = computed(() => {
  if (isSsh.value) return sshSessions.value.length > 0;
  if (isFile.value) return fileAccounts.value.length > 0;
  return dbProfiles.value.length > 0;
});

/** 绑定资源的展示名（用于运行状态/提示）。 */
const boundResourceName = computed(() => {
  const id = config.value.resourceId;
  if (!id) return "";
  if (isSsh.value) {
    return sshSessions.value.find((s) => s.id === id)?.name ?? "(会话已删除)";
  }
  if (isFile.value) {
    return fileAccounts.value.find((a) => a.id === id)?.name ?? "(账号已删除)";
  }
  return dbProfiles.value.find((p) => p.id === id)?.name ?? "(连接已删除)";
});

/** 完整 MCP 端点 URL（Streamable HTTP 用 Authorization 头鉴权，URL 不带 token）。 */
const fullUrl = computed(() => {
  if (!status.value.running) return "";
  // 绑定地址是 0.0.0.0（对所有网卡监听）时，0.0.0.0 不是合法客户端目的地
  // （多数系统上连接 0.0.0.0 失败或行为未定义），展示为 127.0.0.1 才是实际可用地址。
  const host = status.value.host === "0.0.0.0" ? "127.0.0.1" : status.value.host;
  return `http://${host}:${status.value.port}/mcp`;
});

/** 客户端配置 JSON（一键复制）。 */
const clientConfig = computed(() => {
  if (!fullUrl.value) return "";
  const serverKey = isSsh.value ? "x-term-ssh" : isFile.value ? "x-term-file" : "x-term-db";
  const cfg: Record<string, unknown> = {
    mcpServers: {
      [serverKey]: { url: fullUrl.value },
    },
  };
  if (config.value.token) {
    (cfg.mcpServers as Record<string, Record<string, unknown>>)[serverKey].headers = {
      Authorization: `Bearer ${config.value.token}`,
    };
  }
  return JSON.stringify(cfg, null, 2);
});

/** 绑定资源 / 地址 / 端口 改动后，若服务在运行，需提示重启。 */
const needsRestart = computed(() => status.value.running);

async function loadResources() {
  if (isSsh.value) {
    if (!sessions.loaded) {
      try {
        await sessions.load();
      } catch {
        /* ignore */
      }
    }
    sshSessions.value = sessions.sessions.filter(
      (s) => s.protocol === "ssh" || !s.protocol,
    );
  } else if (isFile.value) {
    try {
      fileAccounts.value = await fileAccountList();
    } catch (e) {
      ElMessage.error("加载 S3 文件账号列表失败：" + String(e));
    }
  } else {
    try {
      dbProfiles.value = await dbListProfiles();
    } catch (e) {
      ElMessage.error("加载数据库连接列表失败：" + String(e));
    }
  }
}

/** DB MCP：选择 profile 后临时连接获取数据库列表。 */
async function loadDatabases(profileId: string) {
  if (!profileId) {
    databases.value = [];
    return;
  }
  loadingDbs.value = true;
  try {
    const connId = await dbConnect(profileId);
    try {
      databases.value = await dbListDatabases(connId);
    } finally {
      await dbDisconnect(connId).catch(() => {});
    }
  } catch {
    // 连接失败（服务不可达等）：清空列表，用户可手动输入。
    databases.value = [];
  } finally {
    loadingDbs.value = false;
  }
}

/** 绑定资源变更处理。 */
async function onResourceChange() {
  // 仅 DB kind：加载该 profile 的数据库列表。
  if (props.kind === "db" && config.value.resourceId) {
    await loadDatabases(config.value.resourceId);
  }
  await saveConfigAndMaybeWarn();
}

/** 绑定/地址/端口改动后保存配置。运行中则提示重启。 */
async function saveConfigAndMaybeWarn() {
  try {
    await mcp.saveConfig(props.kind);
    if (needsRestart.value) {
      ElMessage.warning("配置已保存，需重启该 MCP 服务才能生效。");
    }
  } catch (e) {
    ElMessage.error("保存配置失败：" + String(e));
  }
}

/** 自动放行开关改动：保存配置（后端立即生效，无需重启）。 */
async function saveAutoApprove() {
  try {
    await mcp.saveConfig(props.kind);
    ElMessage.success(config.value.autoApprove ? "已开启自动放行" : "已关闭自动放行");
  } catch (e) {
    ElMessage.error("保存失败：" + String(e));
  }
}

async function start() {
  if (!clientMode.value && !config.value.resourceId) {
    const resName = isSsh.value ? "SSH 会话" : isFile.value ? "S3 文件账号" : "数据库连接";
    ElMessage.warning(`请先选择一个${resName}，或开启「客户端直连模式」`);
    return;
  }
  if (!config.value.token) {
    ElMessage.warning("请先生成 token 再启动");
    return;
  }
  // 先把当前 host/port/resourceId/resourceMode 落盘，再用配置启动。
  await saveConfigAndMaybeWarn();
  try {
    const s = await mcp.start(props.kind);
    ElMessage.success(`${title.value} 已启动：${s.host}:${s.port}`);
  } catch (e) {
    ElMessage.error("启动失败：" + String(e));
  }
}

async function stop() {
  try {
    await mcp.stop(props.kind);
    ElMessage.success(`${title.value} 已停止`);
  } catch (e) {
    ElMessage.error("停止失败：" + String(e));
  }
}

async function generateToken() {
  try {
    await mcp.regenerateToken(props.kind);
    ElMessage.success("已生成新 token");
  } catch (e) {
    ElMessage.error("生成 token 失败：" + String(e));
  }
}

async function copy(text: string, label = "已复制") {
  if (!text) return;
  try {
    await navigator.clipboard.writeText(text);
    ElMessage.success(label);
  } catch {
    ElMessage.error("复制失败");
  }
}

onMounted(async () => {
  await loadResources();
  // 仅 DB kind：若已有绑定 profile，加载其数据库列表。
  if (props.kind === "db" && config.value.resourceId) {
    await loadDatabases(config.value.resourceId);
  }
});
</script>

<template>
  <div class="instance-panel">
    <div class="form-card">
      <div class="card-title">{{ title }}</div>
      <div class="card-desc">{{ toolHint }}</div>

      <!-- 资源模式开关：绑定模式 / 客户端直连模式（File kind 仅支持 bound，隐藏开关） -->
      <div v-if="!isFile" class="mode-row">
        <div class="switch-label">
          <div>客户端直连模式（免绑定实例）</div>
          <div class="switch-desc">
            开启后无需绑定本地资源，调用方在工具参数中传入目标服务器与账密
            （host/port/username/password）。适用于调用方自带服务器账密表的巡检场景。
          </div>
        </div>
        <el-switch
          v-model="config.resourceMode"
          active-value="client"
          inactive-value="bound"
          @change="saveConfigAndMaybeWarn"
        />
      </div>

      <!-- 直连模式安全提示 -->
      <el-alert
        v-if="clientMode"
        type="warning"
        :closable="false"
        show-icon
        class="mt8"
      >
        <div class="client-mode-alert">
          <div>调用方需在工具参数中传 <code>host</code> / <code>port</code> /
            <code>username</code> / <code>password</code>（DB 另可传 <code>database</code>）。</div>
          <div>密码<strong>仅本次调用有效</strong>：不存储、不落日志、不显示在确认弹窗中。</div>
          <div>切换本模式后需<strong>重启 MCP 服务</strong>才能生效。</div>
        </div>
      </el-alert>

      <!-- 绑定资源（仅绑定模式） -->
      <div v-if="!clientMode" class="field-row">
        <label class="field-label">
          绑定{{ isSsh ? "SSH 会话" : isFile ? "S3 文件账号" : "数据库连接" }}
          <span class="required">*</span>
        </label>
        <el-select
          v-model="config.resourceId"
          :placeholder="`选择一个${isSsh ? 'SSH 会话' : isFile ? 'S3 文件账号' : '数据库连接'}`"
          filterable
          class="field-control"
          :disabled="!hasResources"
          @change="onResourceChange"
        >
          <template v-if="isSsh">
            <el-option
              v-for="s in sshSessions"
              :key="s.id"
              :label="`${s.name} (${s.host}:${s.port})`"
              :value="s.id"
            />
          </template>
          <template v-else-if="isFile">
            <el-option
              v-for="a in fileAccounts"
              :key="a.id"
              :label="`${a.name} (${a.bucket || a.endpoint})`"
              :value="a.id"
            />
          </template>
          <template v-else>
            <el-option
              v-for="p in dbProfiles"
              :key="p.id"
              :label="`${p.name} (${p.host}:${p.port})`"
              :value="p.id"
            />
          </template>
        </el-select>
      </div>

      <!-- DB MCP：绑定具体数据库（仅 db kind + 绑定模式） -->
      <div v-if="kind === 'db' && !clientMode" class="field-row">
        <label class="field-label">绑定数据库（可选）</label>
        <el-select
          v-model="config.boundDatabase"
          placeholder="不选则使用连接默认库；可选择或手动输入"
          filterable
          allow-create
          clearable
          class="field-control"
          :loading="loadingDbs"
          :disabled="!config.resourceId"
          @change="saveConfigAndMaybeWarn"
        >
          <el-option v-for="db in databases" :key="db" :label="db" :value="db" />
        </el-select>
        <div class="hint-text">
          选择后，exec_sql 将只在该库上执行（外部 AI 工具描述中会注明库名）。
          若连接不可达，可手动输入库名。
        </div>
      </div>
      <el-alert
        v-if="!clientMode && !hasResources"
        type="warning"
        :closable="false"
        show-icon
        class="mt8"
        :title="`暂无可用${isSsh ? 'SSH 会话' : isFile ? 'S3 文件账号' : '数据库连接'}，请先在对应页面创建${isFile ? '' : '，或开启「客户端直连模式」'}`"
      />

      <!-- 监听地址 + 端口 -->
      <div class="addr-row">
        <div class="field-row flex1">
          <label class="field-label">监听地址</label>
          <el-input v-model="config.host" placeholder="0.0.0.0" class="field-control" @change="saveConfigAndMaybeWarn" />
        </div>
        <div class="field-row port-field">
          <label class="field-label">端口</label>
          <el-input-number
            v-model="config.port"
            :min="1"
            :max="65535"
            controls-position="right"
            class="field-control"
            @change="saveConfigAndMaybeWarn"
          />
        </div>
      </div>
      <div class="hint-text">
        默认 <code>0.0.0.0</code>（对局域网开放）；仅本机使用可填 <code>127.0.0.1</code>。
        <strong v-if="config.host === '0.0.0.0'" class="warn-inline">
          ⚠ 对局域网开放，务必保管好 token。
        </strong>
      </div>
    </div>

    <!-- 运行状态 + 启停 -->
    <div class="form-card">
      <div class="card-title-row">
        <div class="card-title">运行状态</div>
        <div class="actions">
          <el-button :icon="Refresh" size="small" @click="mcp.refresh(kind)">刷新</el-button>
          <el-button
            v-if="!status.running"
            type="primary"
            :icon="VideoPlay"
            size="small"
            :loading="loading"
            @click="start"
          >
            启动
          </el-button>
          <el-button
            v-else
            type="danger"
            :icon="VideoPause"
            size="small"
            :loading="loading"
            @click="stop"
          >
            停止
          </el-button>
        </div>
      </div>

      <div class="status-line">
        <span class="status-label">状态</span>
        <el-tag :type="status.running ? 'success' : 'info'" effect="dark" size="small">
          {{ status.running ? "运行中" : "已停止" }}
        </el-tag>
        <template v-if="status.running && clientMode">
          <span class="status-label">模式</span>
          <span class="bound-name">客户端直连（未绑定）</span>
        </template>
        <template v-else-if="status.running && boundResourceName">
          <span class="status-label">绑定</span>
          <span class="bound-name">{{ boundResourceName }}</span>
        </template>
      </div>
      <div v-if="status.running" class="status-line">
        <span class="status-label">SSE 端点</span>
        <code class="endpoint">{{ status.endpoint }}</code>
        <el-button :icon="CopyDocument" link size="small" @click="copy(fullUrl, '已复制端点地址')" />
      </div>

      <!-- 自动放行开关 -->
      <div class="switch-row">
        <div class="switch-label">
          <div>自动放行（免确认）</div>
          <div class="switch-desc">
            开启后，外部客户端的 exec_ssh / exec_sql 请求<strong>不再弹出确认框</strong>，直接执行。
            适用于你信任的客户端场景。关闭则每次执行都需要你在 X-Term 中手动确认。
          </div>
        </div>
        <el-switch v-model="config.autoApprove" @change="saveAutoApprove" />
      </div>

      <!-- 执行日志开关 -->
      <div class="switch-row">
        <div class="switch-label">
          <div>记录执行日志</div>
          <div class="switch-desc">
            开启后，每次启动服务会生成一个文本日志文件（位于应用数据目录
            <code>mcp-logs/</code>），记录每次工具调用的时间、命令/SQL、结果与耗时。
            重启服务后生效。
          </div>
        </div>
        <el-switch v-model="config.enableLog" @change="saveConfigAndMaybeWarn" />
      </div>
    </div>

    <!-- 访问令牌 -->
    <div class="form-card">
      <div class="card-title">
        <el-icon><Key /></el-icon>
        访问令牌 (Token)
      </div>
      <div class="card-desc">
        外部客户端请求时需在 Header 携带
        <code>Authorization: Bearer &lt;token&gt;</code>，或在 URL 加 <code>?token=&lt;token&gt;</code>。
        <strong>请妥善保管 token</strong>：任何持有 token 的客户端均可调用本服务执行操作。
      </div>
      <div class="token-row">
        <el-input :model-value="config.token ?? ''" placeholder="点击生成 token" readonly class="token-input">
          <template #prefix><el-icon><Key /></el-icon></template>
        </el-input>
        <el-button type="primary" :icon="Refresh" @click="generateToken">
          {{ config.token ? "重新生成" : "生成 Token" }}
        </el-button>
        <el-button v-if="config.token" :icon="CopyDocument" @click="copy(config.token, '已复制 token')">复制</el-button>
      </div>
    </div>

    <!-- 客户端配置示例 -->
    <div class="form-card">
      <div class="card-title">客户端配置示例</div>
      <div class="card-desc">
        将以下配置加入 Claude Desktop 的 <code>claude_desktop_config.json</code>（或 Cursor 的 MCP 设置）。
        需先启动服务并生成 token。
      </div>
      <div v-if="!status.running || !config.token" class="config-empty">
        <el-alert type="info" :closable="false" show-icon>
          请先完成配置（{{ clientMode ? "直连模式无需绑定实例" : "绑定资源" }}）、生成 token 并启动服务，配置将自动生成。
        </el-alert>
      </div>
      <template v-else>
        <pre class="config-json">{{ clientConfig }}</pre>
        <el-button :icon="CopyDocument" size="small" @click="copy(clientConfig, '已复制配置 JSON')">
          复制配置
        </el-button>
      </template>
    </div>
  </div>
</template>

<style scoped>
.instance-panel {
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.form-card {
  background: var(--el-bg-color-overlay);
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 8px;
  padding: 16px;
}
.card-title {
  font-size: 14px;
  font-weight: 600;
  margin-bottom: 6px;
  display: flex;
  align-items: center;
  gap: 6px;
}
.card-title-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 12px;
}
.card-title-row .card-title {
  margin-bottom: 0;
}
.card-desc {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  line-height: 1.6;
  margin-bottom: 14px;
}
.card-desc code {
  background: var(--el-fill-color-light);
  padding: 1px 4px;
  border-radius: 3px;
  font-size: 11px;
}

.field-row {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin-bottom: 14px;
}
.field-label {
  font-size: 13px;
  color: var(--el-text-color-regular);
  font-weight: 500;
}
.required {
  color: var(--el-color-danger);
}
.field-control {
  width: 100%;
}
.addr-row {
  display: flex;
  gap: 12px;
}
.addr-row .flex1 {
  flex: 1;
}
.port-field {
  max-width: 160px;
}
.mode-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  margin-bottom: 14px;
}
.client-mode-alert {
  font-size: 12px;
  line-height: 1.8;
}
.client-mode-alert code {
  background: var(--el-fill-color-light);
  padding: 1px 4px;
  border-radius: 3px;
}
.hint-text {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  margin-top: -6px;
  margin-bottom: 4px;
}
.hint-text code {
  background: var(--el-fill-color-light);
  padding: 1px 4px;
  border-radius: 3px;
}
.warn-inline {
  color: var(--el-color-danger);
}
.mt8 {
  margin-top: -4px;
  margin-bottom: 14px;
}

.actions {
  display: flex;
  gap: 8px;
}
.status-line {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-bottom: 10px;
  flex-wrap: wrap;
}
.status-label {
  font-size: 13px;
  color: var(--el-text-color-secondary);
  min-width: 56px;
}
.bound-name {
  font-weight: 500;
  color: var(--el-text-color-primary);
}
.endpoint {
  font-family: "Cascadia Code", Consolas, monospace;
  font-size: 13px;
  color: var(--el-color-primary);
  word-break: break-all;
}

.token-row {
  display: flex;
  gap: 8px;
  align-items: center;
}
.token-input {
  flex: 1;
}

.config-empty {
  margin-top: 4px;
}
.config-json {
  background: var(--el-fill-color-dark);
  color: var(--el-color-success);
  padding: 12px;
  border-radius: 6px;
  font-size: 12px;
  overflow: auto;
  margin: 8px 0;
  font-family: "Cascadia Code", Consolas, monospace;
}

.switch-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  padding-top: 12px;
  margin-top: 12px;
  border-top: 1px solid var(--el-border-color-lighter);
}
.switch-label > div:first-child {
  font-size: 13px;
  font-weight: 500;
  color: var(--el-text-color-primary);
}
.switch-desc {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  margin-top: 4px;
  line-height: 1.5;
}
</style>
