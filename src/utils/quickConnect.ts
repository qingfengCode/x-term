/**
 * 快速连接串解析：把 `ssh://user@host:port`、`ssh user@host`、`user@host:port`
 * 等连接串解析为会话骨架——粘贴即连，免去新建会话表单。
 *
 * 协议表驱动：x-term 支持的终端类协议（ssh/telnet/rdp/vnc）+ 别名
 * （rdp:// → rdp）。裸 `host` 不识别（会与普通搜索词混淆），至少需要
 * `://`、`@`、`:端口` 或 `协议名 ` 之一的明确信号。
 *
 * 借鉴 uniTerm quickConnect.ts（Apache-2.0），协议集合适配 x-term。
 */
import type { Protocol } from "@/api/types";

/** 协议注册表：scheme → 协议 + 默认端口。 */
const QUICK_PROTOCOLS: Record<string, { protocol: Protocol; defaultPort: number }> = {
  ssh: { protocol: "ssh", defaultPort: 22 },
  telnet: { protocol: "telnet", defaultPort: 23 },
  rdp: { protocol: "rdp", defaultPort: 3389 },
  vnc: { protocol: "vnc", defaultPort: 5900 },
};

/** 解析结果：会话骨架字段（不含 id/时间戳等持久化字段）。 */
export interface QuickConnectTarget {
  protocol: Protocol;
  host: string;
  port: number;
  username: string;
  /** URI 中携带的密码（user:pass@host）。无则 undefined。 */
  password?: string;
  /** 展示名：user@host[:port]（非默认端口才带端口）。 */
  label: string;
}

/** 解析 `[user[:password]@]host[:port]` 片段。 */
function parseHostPart(s: string): {
  username: string;
  password?: string;
  host: string;
  port?: number;
} {
  const m = s.match(/^(?:([^@]+)@)?([A-Za-z0-9._-]+|\[[0-9a-fA-F:]+\])(?::(\d+))?$/);
  if (!m) return { username: "", host: "", port: undefined };
  let username = "";
  let password: string | undefined;
  if (m[1]) {
    const colonIdx = m[1].indexOf(":");
    if (colonIdx >= 0) {
      username = m[1].slice(0, colonIdx);
      password = m[1].slice(colonIdx + 1) || undefined;
    } else {
      username = m[1];
    }
  }
  // IPv6 字面量去方括号。
  const host = m[2].startsWith("[") ? m[2].slice(1, -1) : m[2];
  return { username, password, host, port: m[3] ? parseInt(m[3], 10) : undefined };
}

/** 输入是否"长得像"连接串（用于侧栏提示条显隐判定）。
 *
 * 裸 host 不算（与搜索词无法区分）；要求以下信号之一：
 * - `scheme://` 前缀（且 scheme 是已知协议）；
 * - 含 `@`（user@host）；
 * - `host:数字端口`；
 * - `协议名 host`（首词为已知协议 + 空格）。 */
export function looksLikeQuickConnect(raw: string): boolean {
  return parseQuickConnect(raw) !== null;
}

/** 解析连接串。无法识别时返回 null。 */
export function parseQuickConnect(raw: string): QuickConnectTarget | null {
  const input = raw.trim();
  if (!input) return null;

  // 形式 1：scheme://[user[:pass]@]host[:port]
  const uri = input.match(/^([a-zA-Z][a-zA-Z0-9+.-]*):\/\/(?:([^@/]*)@)?(.+)$/);
  if (uri) {
    const cfg = QUICK_PROTOCOLS[uri[1].toLowerCase()];
    if (!cfg) return null;
    const hostPart = parseHostPart(uri[3].replace(/\/+$/, ""));
    if (!hostPart.host) return null;
    return build(cfg.protocol, hostPart, cfg.defaultPort);
  }

  const parts = input.split(/\s+/);
  // 形式 2：`协议名 [user@]host[:port]`（如 `ssh root@1.2.3.4`）；
  // 两个词但首词不是已知协议 → 不是连接串；超过两个词同理。
  if (parts.length >= 2) {
    const cfg = QUICK_PROTOCOLS[parts[0].toLowerCase()];
    if (!cfg || parts.length > 2) return null;
    const hostPart = parseHostPart(parts[1]);
    if (!hostPart.host) return null;
    return build(cfg.protocol, hostPart, cfg.defaultPort);
  }

  // 形式 3：`[user[:pass]@]host[:port]` 裸串——要求明确信号：含 @ 或 :端口。
  // 裸串场景默认 ssh（终端使用绝对主流），端口照用解析值。
  if (input.includes("@") || /:\d+$/.test(input)) {
    const hostPart = parseHostPart(input);
    if (!hostPart.host) return null;
    return build("ssh", hostPart, 22);
  }
  return null;
}

function build(
  protocol: Protocol,
  part: { username: string; password?: string; host: string; port?: number },
  defaultPort: number,
): QuickConnectTarget {
  const port = part.port ?? defaultPort;
  const showPort = port !== defaultPort;
  const userPrefix = part.username ? `${part.username}@` : "";
  const label = `${userPrefix}${part.host}${showPort ? `:${port}` : ""}`;
  return {
    protocol,
    host: part.host,
    port,
    username: part.username,
    password: part.password,
    label,
  };
}
