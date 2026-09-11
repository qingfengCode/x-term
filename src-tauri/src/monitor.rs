//! 服务器监控：一条持久 SSH 连接 + 定时（3s）在远端执行采集脚本并解析。
//!
//! 采集模式借鉴 uniTerm monitor_session（Apache-2.0）：**把整段 POSIX shell
//! 脚本一次 exec 过去，脚本内用 printf/awk 输出分节标记 + 空格分隔的字段**，
//! Rust 端逐行状态机解析——最小化 SSH 往返（每 tick 一次 exec），且不依赖
//! 远端任何额外工具（无 jq/python，busybox awk 即可工作）。
//!
//! CPU 使用率与网络速率需要**两点差值**：脚本输出的是 `/proc` 累计值，
//! Rust 侧保存上一次采样（值 + 时刻），差值/间隔即得速率。首轮无差值时
//! 对应字段为 `None`（前端显示 `--`）。
//!
//! 数据通过 `monitor:data` 事件推送（payload: [`crate::events::MonitorDataEvent`]）；
//! 连接断开/采集失败时推送 `monitor:closed` 并结束循环。停止由前端
//! `monitor_stop` 触发（watch 通道）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use russh::ChannelMsg;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::error::{AppError, AppResult};
use crate::events::{self, MONITOR_CLOSED, MONITOR_DATA};
use crate::state::AppState;

/// 采集间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// 单次采集（exec + 读取）超时。慢机器/高负载下 df/ps 也该在几秒内返回。
const COLLECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 连续采集失败达到该次数即判定连接死亡，退出循环。
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// 采集脚本（POSIX sh；分节标记 `---NAME---` + 空格分隔字段行）。
///
/// 全部走 /proc 与核心工具（head/grep/awk/df/ps），失败的字段输出空——
/// 解析侧对缺失字段容错（busybox 环境降级为部分数据）。
const COLLECT_SCRIPT: &str = r#"printf '%s\n' '---HOST---'
cat /proc/sys/kernel/hostname 2>/dev/null || hostname 2>/dev/null || printf '\n'
uname -sr 2>/dev/null || printf '\n'
(. /etc/os-release 2>/dev/null && printf '%s\n' "$PRETTY_NAME") 2>/dev/null || printf '\n'
printf '%s\n' '---CPU---'
head -n 1 /proc/stat 2>/dev/null
printf '%s\n' "$(grep -c '^cpu[0-9]' /proc/stat 2>/dev/null || printf 0)"
printf '%s\n' '---LOAD---'
cat /proc/loadavg 2>/dev/null
printf '%s\n' '---UPTIME---'
awk '{print int($1)}' /proc/uptime 2>/dev/null
printf '%s\n' '---MEM---'
grep -E '^(MemTotal|MemAvailable|MemFree|Buffers|Cached|SwapTotal|SwapFree):' /proc/meminfo 2>/dev/null
printf '%s\n' '---NET---'
awk 'NR>2 && $1!="lo:" {rx+=$1; tx+=$9} END {printf "%s %s\n", rx+0, tx+0}' /proc/net/dev 2>/dev/null
printf '%s\n' '---DISK---'
df -kP 2>/dev/null | awk 'NR>1 && $1 !~ /tmpfs|devtmpfs|udev|squashfs/ {print $1"|"$2"|"$3"|"$4"|"$5"|"$6}'
printf '%s\n' '---PROC---'
ps -eo user:24,pid,pcpu,pmem,comm --sort=-pcpu 2>/dev/null | head -n 13 || true
"#;

// ===========================================================================
// 数据模型（事件 payload）
// ===========================================================================

/// 主机静态信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorHost {
    pub hostname: String,
    pub kernel: String,
    pub os: String,
}

/// CPU：总使用率（差值）+ 逻辑核数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorCpu {
    /// 总使用率（0-100）。首轮无差值为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_pct: Option<f64>,
    pub cores: u32,
}

/// 内存（来自 /proc/meminfo，kB）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorMem {
    pub total_kb: u64,
    pub available_kb: u64,
    /// used = total - available。
    pub used_kb: u64,
    pub used_pct: f64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_used_pct: Option<f64>,
}

/// 负载（1/5/15 分钟）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorLoad {
    pub m1: f64,
    pub m5: f64,
    pub m15: f64,
}

/// 网络速率（差值）+ 累计流量。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorNet {
    /// 下行速率 KB/s（全网卡汇总，不含 lo）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_kbps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_kbps: Option<f64>,
    /// 累计接收/发送字节数（自开机）。
    pub rx_total: u64,
    pub tx_total: u64,
}

/// 一个文件系统挂载点。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorDisk {
    pub fs: String,
    pub mount: String,
    pub total_kb: u64,
    pub used_kb: u64,
    pub avail_kb: u64,
    /// df 的 Capacity（已含 %，如 47 表示 47%）。
    pub used_pct: u64,
}

/// 一个进程（Top CPU）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorProcess {
    pub user: String,
    pub pid: u32,
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub comm: String,
}

// ===========================================================================
// 原始采样与解析
// ===========================================================================

/// /proc/stat 首行的 CPU 累计 jiffies（按固定顺序）。
#[derive(Debug, Clone, Copy, Default)]
struct CpuFields {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
    steal: u64,
}

impl CpuFields {
    fn total(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
    }
    fn idle_all(&self) -> u64 {
        self.idle + self.iowait
    }
}

/// 一次采集的原始（未差值）数据。
#[derive(Debug, Default)]
struct RawSample {
    host: Option<MonitorHost>,
    cpu: Option<CpuFields>,
    cores: u32,
    load: Option<(f64, f64, f64)>,
    uptime_secs: u64,
    mem_total_kb: u64,
    mem_available_kb: u64,
    swap_total_kb: u64,
    swap_free_kb: u64,
    net_rx: u64,
    net_tx: u64,
    disks: Vec<MonitorDisk>,
    processes: Vec<MonitorProcess>,
}

/// 分节标记。
const SEC_HOST: &str = "---HOST---";
const SEC_CPU: &str = "---CPU---";
const SEC_LOAD: &str = "---LOAD---";
const SEC_UPTIME: &str = "---UPTIME---";
const SEC_MEM: &str = "---MEM---";
const SEC_NET: &str = "---NET---";
const SEC_DISK: &str = "---DISK---";
const SEC_PROC: &str = "---PROC---";

/// 解析脚本输出（逐行状态机，按分节标记切换）。
fn parse_output(out: &str) -> RawSample {
    let mut raw = RawSample::default();
    let mut section = "";
    // HOST 节的行游标（hostname/kernel/os 按行序）。
    let mut host_lines: Vec<String> = Vec::new();
    let mut disk_lines: Vec<String> = Vec::new();
    let mut proc_lines: Vec<String> = Vec::new();

    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        let trimmed = line.trim();
        match trimmed {
            SEC_HOST => section = SEC_HOST,
            SEC_CPU => section = SEC_CPU,
            SEC_LOAD => section = SEC_LOAD,
            SEC_UPTIME => section = SEC_UPTIME,
            SEC_MEM => section = SEC_MEM,
            SEC_NET => section = SEC_NET,
            SEC_DISK => section = SEC_DISK,
            SEC_PROC => section = SEC_PROC,
            _ => {
                // HOST 节按**固定行序**取 hostname/kernel/os，命令失败时脚本
                // 输出空行占位——空行必须原样入列（跳过会让后续行错位，
                // kernel 顶到 hostname 的位置）。
                if section == SEC_HOST {
                    host_lines.push(trimmed.to_string());
                    continue;
                }
                if trimmed.is_empty() {
                    continue;
                }
                match section {
                    SEC_CPU => {
                        // 行 1: "cpu  u n s i iow irq sirq [steal ...]"；行 2: 核数。
                        if raw.cpu.is_none() {
                            let parts: Vec<&str> = trimmed.split_whitespace().collect();
                            if parts.len() >= 8 && parts[0] == "cpu" {
                                let p = |i: usize| parts.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
                                raw.cpu = Some(CpuFields {
                                    user: p(1),
                                    nice: p(2),
                                    system: p(3),
                                    idle: p(4),
                                    iowait: p(5),
                                    irq: p(6),
                                    softirq: p(7),
                                    steal: p(8),
                                });
                            }
                        } else if raw.cores == 0 {
                            raw.cores = trimmed.parse().unwrap_or(0);
                        }
                    }
                    SEC_LOAD => {
                        let parts: Vec<&str> = trimmed.split_whitespace().collect();
                        if parts.len() >= 3 {
                            raw.load = Some((
                                parts[0].parse().unwrap_or(0.0),
                                parts[1].parse().unwrap_or(0.0),
                                parts[2].parse().unwrap_or(0.0),
                            ));
                        }
                    }
                    SEC_UPTIME => raw.uptime_secs = trimmed.parse().unwrap_or(0),
                    SEC_MEM => {
                        // "MemTotal:  16384256 kB"
                        let Some((key, val_kb)) = parse_meminfo_line(line) else {
                            continue;
                        };
                        match key {
                            "MemTotal" => raw.mem_total_kb = val_kb,
                            "MemAvailable" => raw.mem_available_kb = val_kb,
                            "SwapTotal" => raw.swap_total_kb = val_kb,
                            "SwapFree" => raw.swap_free_kb = val_kb,
                            _ => {}
                        }
                    }
                    SEC_NET => {
                        let parts: Vec<&str> = trimmed.split_whitespace().collect();
                        if parts.len() >= 2 {
                            raw.net_rx = parts[0].parse().unwrap_or(0);
                            raw.net_tx = parts[1].parse().unwrap_or(0);
                        }
                    }
                    SEC_DISK => disk_lines.push(trimmed.to_string()),
                    SEC_PROC => proc_lines.push(line.trim_start().to_string()),
                    _ => {}
                }
            }
        }
    }

    // HOST 三行。
    if host_lines.len() >= 3 {
        raw.host = Some(MonitorHost {
            hostname: host_lines[0].clone(),
            kernel: host_lines[1].clone(),
            os: host_lines[2].clone(),
        });
    }
    // 磁盘：fs|total|used|avail|pct%|mount
    for l in &disk_lines {
        let f: Vec<&str> = l.split('|').collect();
        if f.len() >= 6 {
            raw.disks.push(MonitorDisk {
                fs: f[0].to_string(),
                mount: f[5].to_string(),
                total_kb: f[1].parse().unwrap_or(0),
                used_kb: f[2].parse().unwrap_or(0),
                avail_kb: f[3].parse().unwrap_or(0),
                used_pct: f[4].trim_end_matches('%').parse().unwrap_or(0),
            });
        }
    }
    // 进程：user pid pcpu pmem comm
    for l in &proc_lines {
        let parts: Vec<&str> = l.split_whitespace().collect();
        if parts.len() >= 5 {
            raw.processes.push(MonitorProcess {
                user: parts[0].to_string(),
                pid: parts[1].parse().unwrap_or(0),
                cpu_pct: parts[2].parse().unwrap_or(0.0),
                mem_pct: parts[3].parse().unwrap_or(0.0),
                comm: parts[4..].join(" "),
            });
        }
    }
    raw
}

/// 解析一行 meminfo：`Key:  value kB` → (key, value_kb)。
fn parse_meminfo_line(line: &str) -> Option<(&str, u64)> {
    let (key, rest) = line.split_once(':')?;
    let val: u64 = rest
        .trim()
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some((key.trim(), val))
}

// ===========================================================================
// 采样状态（差值）
// ===========================================================================

/// 差值所需的上一轮状态。
#[derive(Debug, Clone, Copy)]
struct PrevSample {
    cpu: CpuFields,
    net_rx: u64,
    net_tx: u64,
}

/// 把原始采样换算为事件 payload（差值计算 + 单位换算）。
fn build_event(
    monitor_id: &str,
    raw: &RawSample,
    prev: Option<PrevSample>,
    interval_secs: f64,
) -> crate::events::MonitorDataEvent {
    // CPU 差值：usage = (1 - Δidle/Δtotal) * 100。
    let cpu_usage = match (prev, raw.cpu) {
        (Some(p), Some(c)) => {
            let dt = c.total().saturating_sub(p.cpu.total());
            let di = c.idle_all().saturating_sub(p.cpu.idle_all());
            if dt > 0 {
                Some(((1.0 - di as f64 / dt as f64) * 100.0).clamp(0.0, 100.0))
            } else {
                None
            }
        }
        _ => None,
    };
    // 网络速率（KB/s）。
    let (rx_kbps, tx_kbps) = match prev {
        Some(p) => {
            let rx = ((raw.net_rx.saturating_sub(p.net_rx)) as f64 / 1024.0 / interval_secs).max(0.0);
            let tx = ((raw.net_tx.saturating_sub(p.net_tx)) as f64 / 1024.0 / interval_secs).max(0.0);
            (Some(rx), Some(tx))
        }
        None => (None, None),
    };
    let mem_used_kb = raw.mem_total_kb.saturating_sub(raw.mem_available_kb);
    let mem_used_pct = if raw.mem_total_kb > 0 {
        mem_used_kb as f64 / raw.mem_total_kb as f64 * 100.0
    } else {
        0.0
    };
    let swap_used_kb = raw.swap_total_kb.saturating_sub(raw.swap_free_kb);

    crate::events::MonitorDataEvent {
        monitor_id: monitor_id.to_string(),
        ts: chrono::Local::now().timestamp_millis() as u64,
        host: raw.host.clone(),
        cpu: MonitorCpu {
            usage_pct: cpu_usage,
            cores: raw.cores,
        },
        mem: MonitorMem {
            total_kb: raw.mem_total_kb,
            available_kb: raw.mem_available_kb,
            used_kb: mem_used_kb,
            used_pct: mem_used_pct,
            swap_total_kb: raw.swap_total_kb,
            swap_free_kb: raw.swap_free_kb,
            swap_used_pct: if raw.swap_total_kb > 0 {
                Some(swap_used_kb as f64 / raw.swap_total_kb as f64 * 100.0)
            } else {
                None
            },
        },
        load: raw
            .load
            .map(|(m1, m5, m15)| MonitorLoad { m1, m5, m15 }),
        uptime_secs: raw.uptime_secs,
        net: MonitorNet {
            rx_kbps,
            tx_kbps,
            rx_total: raw.net_rx,
            tx_total: raw.net_tx,
        },
        disks: raw.disks.clone(),
        processes: raw.processes.clone(),
    }
}

// ===========================================================================
// 监控循环
// ===========================================================================

/// 运行中的监控句柄（state.monitors 的值）：stop 信号发送端。
pub type MonitorStop = watch::Sender<bool>;

/// 启动监控循环（spawn 后台任务，立即返回）。
///
/// 流程：按会话配置建一条独立 SSH 连接 → 每 [`POLL_INTERVAL`] 开一个
/// channel exec [`COLLECT_SCRIPT`] → 解析 → 差值 → emit `monitor:data`。
/// 连续 [`MAX_CONSECUTIVE_FAILURES`] 次失败或 stop 信号到达时 emit
/// `monitor:closed` 并退出。
pub async fn start_monitor(
    state: AppState,
    session_config_id: String,
    monitor_id: String,
    mut stop_rx: watch::Receiver<bool>,
) {
    let app = state.app.clone();

    // 1. 解析会话配置与凭据（与 exec_ssh 独立连接模式同一套路）。
    let setup: AppResult<(
        crate::storage::sessions_repo::Session,
        crate::ssh::session::ResolvedCredential,
    )> = (|| {
        let session_config = {
            let conn = state.conn()?;
            crate::storage::sessions_repo::get_session(&conn, &session_config_id)?
                .ok_or_else(|| AppError::NotFound(format!("会话配置 {session_config_id} 不存在")))?
        };
        let vault = {
            let guard = state.vault_read()?;
            guard.as_ref().ok_or_else(|| AppError::Auth("保险库未解锁".into()))?.clone()
        };
        let conn = state.conn()?;
        let resolved = crate::ssh::session::resolve_credential(&session_config, &vault, &conn)?;
        Ok((session_config, resolved))
    })();
    let (session_config, resolved) = match setup {
        Ok(v) => v,
        Err(e) => {
            events::emit(
                &app,
                MONITOR_CLOSED,
                crate::events::MonitorClosedEvent {
                    monitor_id: monitor_id.clone(),
                    reason: e.to_string(),
                },
            );
            return;
        }
    };

    // 2. 建连（一次，监控期间保持）。
    let mut handle = match crate::ssh::client::connect_direct(
        &session_config.host,
        session_config.port,
        &session_config.username,
        &session_config.id,
        resolved.auth_method,
        state.clone(),
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            emit_closed(&app, &monitor_id, format!("连接失败: {e}"));
            return;
        }
    };

    // 3. 采集循环。
    let mut prev: Option<PrevSample> = None;
    let mut failures = 0u32;
    // 上次成功采样时刻：网络速率差值必须除以真实窗口（采集超时/失败会拉长
    // 间隔，仍按固定 3s 计算会把速率高估数倍）。
    let mut last_sample_at: Option<tokio::time::Instant> = None;
    loop {
        // 间隔（首轮立即采集）。
        if prev.is_some() || failures > 0 {
            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
                _ = stop_rx.changed() => {
                    emit_closed(&app, &monitor_id, "已停止".into());
                    // disconnect 在连接卡死（半开）时会永久挂起，加超时兜底
                    //（与 SshSession::close 的 DISCONNECT_TIMEOUT 同策略）。
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        handle.disconnect(russh::Disconnect::ByApplication, "monitor stop", "en"),
                    )
                    .await;
                    return;
                }
            }
        }

        // 执行采集脚本（带超时）。
        let collect = exec_collect(&mut handle);
        let collected = match tokio::time::timeout(COLLECT_TIMEOUT, collect).await {
            Ok(Ok(text)) => {
                failures = 0;
                Some(text)
            }
            Ok(Err(e)) => {
                failures += 1;
                log::warn!("[monitor:{monitor_id}] 采集失败（{failures}/{MAX_CONSECUTIVE_FAILURES}）: {e}");
                if failures >= MAX_CONSECUTIVE_FAILURES {
                    emit_closed(&app, &monitor_id, format!("连续 {failures} 次采集失败: {e}"));
                    return;
                }
                None
            }
            Err(_) => {
                failures += 1;
                log::warn!("[monitor:{monitor_id}] 采集超时（{failures}/{MAX_CONSECUTIVE_FAILURES}）");
                if failures >= MAX_CONSECUTIVE_FAILURES {
                    emit_closed(&app, &monitor_id, "采集连续超时，连接可能已卡死".into());
                    return;
                }
                None
            }
        };

        if let Some(text) = collected {
            let raw = parse_output(&text);
            // 非 Linux 目标（网络设备/BSD 等）没有 /proc：脚本各节全空。
            // 直接推全 0 数据只会让面板显示一堆 0%——明确报不支持。
            if raw.cpu.is_none() && raw.mem_total_kb == 0 && raw.disks.is_empty() {
                emit_closed(
                    &app,
                    &monitor_id,
                    "目标系统不支持监控（采集依赖 /proc，仅支持 Linux）".into(),
                );
                return;
            }
            let interval = last_sample_at
                .map_or(POLL_INTERVAL.as_secs_f64(), |t| {
                    // 下限防除零/异常尖峰（真实窗口不会小于轮询间隔的量级）。
                    t.elapsed().as_secs_f64().max(0.1)
                });
            let event = build_event(&monitor_id, &raw, prev, interval);
            prev = Some(PrevSample {
                cpu: raw.cpu.unwrap_or_default(),
                net_rx: raw.net_rx,
                net_tx: raw.net_tx,
            });
            last_sample_at = Some(tokio::time::Instant::now());
            events::emit(&app, MONITOR_DATA, event);
        }
    }
}

/// 在已有连接上执行采集脚本并收集完整 stdout。
async fn exec_collect(
    handle: &mut russh::client::Handle<crate::ssh::client::ClientHandler>,
) -> AppResult<String> {
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| AppError::Ssh(format!("打开 channel 失败: {e}")))?;
    // want_reply=false：与 exec_ssh 独立连接模式一致——个别服务器（老交换机
    // 等）不回 channel 确认，want_reply=true 会挂到采集超时。
    channel
        .exec(false, COLLECT_SCRIPT)
        .await
        .map_err(|e| AppError::Ssh(format!("exec 失败: {e}")))?;
    let mut out = Vec::new();
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Data { ref data }) => out.extend_from_slice(data.as_ref()),
            Some(ChannelMsg::ExtendedData { ref data, .. }) => out.extend_from_slice(data.as_ref()),
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
            Some(_) => {}
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn emit_closed(app: &tauri::AppHandle, monitor_id: &str, reason: String) {
    events::emit(
        app,
        MONITOR_CLOSED,
        crate::events::MonitorClosedEvent {
            monitor_id: monitor_id.to_string(),
            reason,
        },
    );
}

/// state.monitors 的类型别名（commands 层使用）。
pub type MonitorMap = Arc<parking_lot::Mutex<HashMap<String, MonitorStop>>>;

// ===========================================================================
// 单元测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 一段典型的采集脚本输出（含各分节）。
    const SAMPLE: &str = "---HOST---\nweb01\nLinux 5.15.0-91-generic\nUbuntu 22.04.3 LTS\n---CPU---\ncpu  100 20 50 8000 10 0 5 0 0 0\n4\n---LOAD---\n0.52 0.58 0.59 1/567 12345\n---UPTIME---\n123456\n---MEM---\nMemTotal:       16384256 kB\nMemAvailable:    8388608 kB\nSwapTotal:       2097148 kB\nSwapFree:        1048574 kB\n---NET---\n123456789 9876543\n---DISK---\n/dev/sda1|41152812|12345678|26685634|32%|/\n/dev/sdb1|205113212|98765432|95756112|51%|/data\n---PROC---\nroot 1 0.0 0.5 systemd\nwww-data 812 3.2 1.7 nginx\n";

    #[test]
    fn parse_full_sample() {
        let raw = parse_output(SAMPLE);
        let host = raw.host.expect("host");
        assert_eq!(host.hostname, "web01");
        assert_eq!(host.kernel, "Linux 5.15.0-91-generic");
        assert_eq!(host.os, "Ubuntu 22.04.3 LTS");
        assert_eq!(raw.cores, 4);
        assert_eq!(raw.load, Some((0.52, 0.58, 0.59)));
        assert_eq!(raw.uptime_secs, 123456);
        assert_eq!(raw.mem_total_kb, 16384256);
        assert_eq!(raw.mem_available_kb, 8388608);
        assert_eq!(raw.swap_total_kb, 2097148);
        assert_eq!(raw.swap_free_kb, 1048574);
        assert_eq!(raw.net_rx, 123456789);
        assert_eq!(raw.net_tx, 9876543);
        assert_eq!(raw.disks.len(), 2);
        assert_eq!(raw.disks[0].mount, "/");
        assert_eq!(raw.disks[1].used_pct, 51);
        assert_eq!(raw.processes.len(), 2);
        assert_eq!(raw.processes[1].comm, "nginx");
        assert!(raw.cpu.is_some());
    }

    /// 回归：HOST 节的空行占位（hostname 命令失败）不得让 kernel 错位到
    /// hostname 位置——空行必须原样入列。
    #[test]
    fn parse_host_blank_line_keeps_alignment() {
        let out = "---HOST---\n\nLinux 5.15\nDebian\n---CPU---\ncpu  1 2 3 4 5 6 7 8\n2\n";
        let raw = parse_output(out);
        let host = raw.host.expect("host");
        assert_eq!(host.hostname, "");
        assert_eq!(host.kernel, "Linux 5.15");
        assert_eq!(host.os, "Debian");
    }

    /// 非 Linux 目标（无 /proc）：各节全空——用于上层"不支持"判定。
    #[test]
    fn parse_empty_output_is_all_default() {
        let raw = parse_output("---HOST---\n\n\n\n---CPU---\n\n0\n---MEM---\n");
        assert!(raw.cpu.is_none());
        assert_eq!(raw.mem_total_kb, 0);
        assert!(raw.disks.is_empty());
    }

    /// CPU 差值：Δidle/Δtotal → 使用率。
    #[test]
    fn cpu_delta_computes_usage() {
        let raw = RawSample {
            cpu: Some(CpuFields {
                user: 100,
                nice: 0,
                system: 100,
                idle: 700,
                iowait: 100,
                irq: 0,
                softirq: 0,
                steal: 0,
            }),
            cores: 4,
            ..Default::default()
        };
        let prev = PrevSample {
            cpu: CpuFields {
                user: 0,
                nice: 0,
                system: 0,
                idle: 500,
                iowait: 0,
                irq: 0,
                softirq: 0,
                steal: 0,
            },
            net_rx: 0,
            net_tx: 0,
        };
        // Δtotal = 1000-500 = 500，Δidle = (700+100)-(500+0) = 300 → 40%。
        let ev = build_event("m1", &raw, Some(prev), 3.0);
        assert_eq!(ev.cpu.usage_pct.map(|v| (v * 10.0).round() / 10.0), Some(40.0));
    }

    /// 首轮无差值：usagePct 为 None；网络速率同为 None。
    #[test]
    fn first_round_has_no_delta() {
        let raw = RawSample {
            cpu: Some(CpuFields::default()),
            cores: 1,
            ..Default::default()
        };
        let ev = build_event("m1", &raw, None, 3.0);
        assert!(ev.cpu.usage_pct.is_none());
        assert!(ev.net.rx_kbps.is_none());
        assert!(ev.net.tx_kbps.is_none());
    }
}
