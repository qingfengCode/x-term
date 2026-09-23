//! 端口转发命令。
//!
//! 转发规则的持久化在 `forward_rules` 表；运行时的隧道实例（[`Tunnel`]）保存在
//! [`AppState::tunnels`]。MVP 阶段每条转发规则启动时新建一条独立的 SSH 连接
//! （不复用终端/SFTP 连接）。

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::error::{AppError, AppResult};
use crate::events::{self, ForwardStateEvent};
use crate::ssh::session::resolve_credential;
use crate::ssh::tunnel::{TunnelKind, TunnelSpec};
use crate::state::AppState;

/// 连接存活监控的轮询间隔。
const TUNNEL_MONITOR_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// 向前端广播一条转发规则的状态变化。
fn emit_state(app: &AppHandle, rule_id: &str, running: bool, reason: &str) {
    events::emit(
        app,
        events::FORWARD_STATE,
        ForwardStateEvent {
            rule_id: rule_id.to_string(),
            running,
            reason: reason.to_string(),
        },
    );
}

/// 后台监控一条运行中的隧道：轮询 SSH 连接存活状态，连接断开（服务器重启 /
/// 网络中断 / keepalive 超时）时把它从 tunnels 表移除并通知前端——否则前端会
/// 一直显示"运行中"，而实际转发早已不可用。
fn spawn_tunnel_monitor(app: AppHandle, state: AppState, rule_id: String, tunnel_handle: std::sync::Arc<russh::client::Handle<crate::ssh::client::ClientHandler>>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(TUNNEL_MONITOR_INTERVAL).await;
            // stop() 移除时监控任务应随之退出：连接句柄仍可能被 accept 循环
            // 持有（已 abort 则已释放），以 tunnels 表是否仍登记该规则为准。
            if !state.tunnels.lock().contains_key(&rule_id) {
                return;
            }
            if tunnel_handle.is_closed() {
                log::warn!("转发 {} 的 SSH 连接已断开，标记为停止", rule_id);
                let tunnel = state.tunnels.lock().remove(&rule_id);
                if let Some(t) = tunnel {
                    tokio::spawn(crate::ssh::tunnel::stop(t));
                }
                emit_state(&app, &rule_id, false, "SSH 连接断开");
                return;
            }
        }
    });
}

/// 从查询结果读取端口列；越界（> 65535 或负数）报错而非 `as u16` 静默截断
/// （截断会把 70000 静默变成 4464，绑定到错误的端口）。
fn port_col(r: &rusqlite::Row, idx: usize) -> rusqlite::Result<u16> {
    let v = r.get::<_, i64>(idx)?;
    u16::try_from(v).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("端口号超出 0-65535: {}", v),
            )),
        )
    })
}

/// 转发规则（与数据库表对应）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardRule {
    pub id: String,
    pub name: String,
    pub session_id: String,
    pub kind: String, // "Local" | "Remote" | "Dynamic"
    pub local_host: String,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    #[serde(default)]
    pub auto_start: bool,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// 持久化 CRUD
// ---------------------------------------------------------------------------

// 三条 CRUD 均 async + `spawn_blocking`：取连接（池耗尽等待上限 5s）+ SQLite
// 写锁等待（`busy_timeout` 5000ms）跑在主线程会冻结整个窗口。

#[tauri::command]
pub async fn forward_list_rules(state: State<'_, AppState>) -> AppResult<Vec<ForwardRule>> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, session_id, kind, local_host, local_port, remote_host, \
             remote_port, auto_start, created_at FROM forward_rules ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            let auto: i64 = r.get(8)?;
            Ok(ForwardRule {
                id: r.get(0)?,
                name: r.get(1)?,
                session_id: r.get(2)?,
                kind: r.get(3)?,
                local_host: r.get(4)?,
                local_port: port_col(r, 5)?,
                remote_host: r.get(6)?,
                remote_port: port_col(r, 7)?,
                auto_start: auto != 0,
                created_at: r.get(9)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
    .map_err(|e| AppError::Storage(format!("读取转发规则任务失败: {}", e)))?
}

/// 保存（插入或 upsert）一条转发规则，返回落库后的规则。
///
/// 返回完整规则（含最终 id）供前端使用：前端本地生成的 id 可能被后端
/// 规范化，autoStart 等后续操作必须用返回值里的 id，避免自启动打到
/// 不存在的规则上。
#[tauri::command]
pub async fn forward_save_rule(
    rule: ForwardRule,
    state: State<'_, AppState>,
) -> AppResult<ForwardRule> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        conn.execute(
            "INSERT INTO forward_rules (id, name, session_id, kind, local_host, local_port, \
             remote_host, remote_port, auto_start, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(id) DO UPDATE SET \
                name = excluded.name, session_id = excluded.session_id, \
                kind = excluded.kind, local_host = excluded.local_host, \
                local_port = excluded.local_port, remote_host = excluded.remote_host, \
                remote_port = excluded.remote_port, auto_start = excluded.auto_start",
            rusqlite::params![
                rule.id,
                rule.name,
                rule.session_id,
                rule.kind,
                rule.local_host,
                rule.local_port as i64,
                rule.remote_host,
                rule.remote_port as i64,
                rule.auto_start as i64,
                rule.created_at,
            ],
        )?;
        Ok(rule)
    })
    .await
    .map_err(|e| AppError::Storage(format!("保存转发规则任务失败: {}", e)))?
}

#[tauri::command]
pub async fn forward_delete_rule(
    id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let state_app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = state_app.state::<AppState>();
        let conn = state.conn()?;
        conn.execute("DELETE FROM forward_rules WHERE id = ?1", [&id])?;
        // 同步停止运行中的隧道。
        if let Some(tunnel) = state.tunnels.lock().remove(&id) {
            // 隧道停止是异步的；这里不 await，spawn 出去避免命令阻塞。
            // async_runtime::spawn 任意线程可用（阻塞线程池中无 tokio 上下文，
            // 裸 tokio::spawn 会 panic）。
            tauri::async_runtime::spawn(crate::ssh::tunnel::stop(tunnel));
            emit_state(&app, &id, false, "规则已删除");
        }
        Ok(())
    })
    .await
    .map_err(|e| AppError::Storage(format!("删除转发规则任务失败: {}", e)))?
}

// ---------------------------------------------------------------------------
// 运行时启停
// ---------------------------------------------------------------------------

/// 启动一条转发规则（按规则建立新连接并开始转发）。返回规则 id（便于前端引用）。
#[tauri::command]
pub async fn forward_start(
    rule_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<String> {
    // 取规则。
    let rule = {
        let conn = state.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, session_id, kind, local_host, local_port, remote_host, \
             remote_port, auto_start, created_at FROM forward_rules WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([&rule_id], |r| {
            let auto: i64 = r.get(8)?;
            Ok(ForwardRule {
                id: r.get(0)?,
                name: r.get(1)?,
                session_id: r.get(2)?,
                kind: r.get(3)?,
                local_host: r.get(4)?,
                local_port: port_col(r, 5)?,
                remote_host: r.get(6)?,
                remote_port: port_col(r, 7)?,
                auto_start: auto != 0,
                created_at: r.get(9)?,
            })
        })?;
        rows.next()
            .ok_or_else(|| AppError::NotFound(format!("转发规则 {} 不存在", rule_id)))??
    };

    // 解析 kind。
    let kind = match rule.kind.as_str() {
        "Local" => TunnelKind::Local,
        "Remote" => TunnelKind::Remote,
        "Dynamic" => TunnelKind::Dynamic,
        other => return Err(AppError::InvalidInput(format!("未知的转发类型: {}", other))),
    };

    // 若已经在运行，先报错（避免重复）。
    if state.tunnels.lock().contains_key(&rule.id) {
        return Err(AppError::InvalidInput(format!("转发 {} 已在运行", rule.id)));
    }

    // 解析会话配置和凭据，建立独立连接。
    let session_config = {
        let conn = state.conn()?;
        crate::storage::sessions_repo::get_session(&conn, &rule.session_id)?
            .ok_or_else(|| AppError::NotFound(format!("会话 {} 不存在", rule.session_id)))?
    };
    let resolved = {
        let vault_guard = state.vault_read()?;
        let vault = vault_guard
            .as_ref()
            .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
            .clone();
        drop(vault_guard);
        let conn = state.conn()?;
        resolve_credential(&session_config, &vault, &conn)?
    };

    let spec = TunnelSpec {
        id: rule.id.clone(),
        session_id: rule.session_id.clone(),
        kind,
        local_host: rule.local_host.clone(),
        local_port: rule.local_port,
        remote_host: rule.remote_host.clone(),
        remote_port: rule.remote_port,
    };

    // 远程转发（-R）需要把 forwards 注册表注入 handler（russh 0.45 的 Handle 不
    // 暴露 handler 访问器，故由 connect_direct_tunnel 在构造时注入并回传）。
    // 本地/动态转发无需 forwards，走普通 connect_direct。
    let tunnel = match kind {
        TunnelKind::Remote => {
            let (handle, forwards) = crate::ssh::client::connect_direct_tunnel(
                &session_config.host,
                session_config.port,
                &session_config.username,
                &session_config.id,
                resolved.auth_method,
                state.inner().clone(),
            )
            .await?;
            crate::ssh::tunnel::start_remote(handle, spec, forwards).await?
        }
        TunnelKind::Local => {
            let handle = crate::ssh::client::connect_direct(
                &session_config.host,
                session_config.port,
                &session_config.username,
                &session_config.id,
                resolved.auth_method,
                state.inner().clone(),
            )
            .await?;
            crate::ssh::tunnel::start_local(handle, spec).await?
        }
        TunnelKind::Dynamic => {
            let handle = crate::ssh::client::connect_direct(
                &session_config.host,
                session_config.port,
                &session_config.username,
                &session_config.id,
                resolved.auth_method,
                state.inner().clone(),
            )
            .await?;
            crate::ssh::tunnel::start_dynamic(handle, spec).await?
        }
    };

    // 竞态防护：上面的 contains_key 检查与这里的 insert 之间隔了完整的 SSH
    // 建连过程（可能数秒），并发点击启动时两个请求都会通过检查。必须在 insert
    // 时再次确认；若已被他人注册，立即停掉自己刚建立的连接，避免泄漏。
    // 注意：parking_lot guard 非 Send，不能持锁 await stop，用 spawn 异步停。
    // 存活监控用的连接句柄副本（tunnel 本体将移入注册表）。
    let tunnel_handle = tunnel.handle.clone();
    {
        let mut guard = state.tunnels.lock();
        if guard.contains_key(&rule.id) {
            drop(guard);
            tokio::spawn(crate::ssh::tunnel::stop(tunnel));
            return Err(AppError::InvalidInput(format!("转发 {} 已在运行", rule.id)));
        }
        guard.insert(rule.id.clone(), tunnel);
    }
    // 通知前端 + 启动连接存活监控（SSH 断开时自动收尾并推送停止状态）。
    emit_state(&app, &rule.id, true, "started");
    if let Some(handle) = tunnel_handle {
        spawn_tunnel_monitor(app.clone(), state.inner().clone(), rule.id.clone(), handle);
    }
    Ok(rule.id)
}

/// 停止一条正在运行的转发。
#[tauri::command]
pub async fn forward_stop(
    rule_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let tunnel = state
        .tunnels
        .lock()
        .remove(&rule_id)
        .ok_or_else(|| AppError::NotFound(format!("转发 {} 未在运行", rule_id)))?;
    crate::ssh::tunnel::stop(tunnel).await?;
    emit_state(&app, &rule_id, false, "stopped");
    Ok(())
}

/// 返回当前正在运行的转发规则 id 列表。
///
/// 运行状态由后端持有（[`AppState::tunnels`]），前端刷新页面/重新进入时调用
/// 本命令同步真实状态，避免内存 Set 与实际不一致（如隧道异常退出后仍显示运行）。
#[tauri::command]
pub fn forward_list_running(state: State<'_, AppState>) -> AppResult<Vec<String>> {
    Ok(state.tunnels.lock().keys().cloned().collect())
}
