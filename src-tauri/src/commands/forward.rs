//! 端口转发命令。
//!
//! 转发规则的持久化在 `forward_rules` 表；运行时的隧道实例（[`Tunnel`]）保存在
//! [`AppState::tunnels`]。MVP 阶段每条转发规则启动时新建一条独立的 SSH 连接
//! （不复用终端/SFTP 连接）。

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::error::{AppError, AppResult};
use crate::ssh::session::resolve_credential;
use crate::ssh::tunnel::{TunnelKind, TunnelSpec};
use crate::state::AppState;

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

#[tauri::command]
pub fn forward_list_rules(state: State<'_, AppState>) -> AppResult<Vec<ForwardRule>> {
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
            local_port: r.get::<_, i64>(5)? as u16,
            remote_host: r.get(6)?,
            remote_port: r.get::<_, i64>(7)? as u16,
            auto_start: auto != 0,
            created_at: r.get(9)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[tauri::command]
pub fn forward_save_rule(rule: ForwardRule, state: State<'_, AppState>) -> AppResult<()> {
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
    Ok(())
}

#[tauri::command]
pub fn forward_delete_rule(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    conn.execute("DELETE FROM forward_rules WHERE id = ?1", [&id])?;
    // 同步停止运行中的隧道。
    if let Some(tunnel) = state.tunnels.lock().remove(&id) {
        // 隧道停止是异步的；这里不 await，spawn 出去避免命令阻塞。
        tokio::spawn(crate::ssh::tunnel::stop(tunnel));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 运行时启停
// ---------------------------------------------------------------------------

/// 启动一条转发规则（按规则建立新连接并开始转发）。返回规则 id（便于前端引用）。
#[tauri::command]
pub async fn forward_start(
    rule_id: String,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
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
                local_port: r.get::<_, i64>(5)? as u16,
                remote_host: r.get(6)?,
                remote_port: r.get::<_, i64>(7)? as u16,
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
        other => {
            return Err(AppError::InvalidInput(format!(
                "未知的转发类型: {}",
                other
            )))
        }
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

    let handle = crate::ssh::client::connect_direct(
        &session_config.host,
        session_config.port,
        &session_config.username,
        resolved.auth_method,
        app,
    )
    .await?;

    let spec = TunnelSpec {
        id: rule.id.clone(),
        session_id: rule.session_id.clone(),
        kind,
        local_host: rule.local_host.clone(),
        local_port: rule.local_port,
        remote_host: rule.remote_host.clone(),
        remote_port: rule.remote_port,
    };

    let tunnel = match kind {
        TunnelKind::Local => crate::ssh::tunnel::start_local(handle, spec).await?,
        // Remote / Dynamic MVP 阶段占位：返回错误（tunnel.rs 内已实现为 InvalidInput）。
        TunnelKind::Remote => crate::ssh::tunnel::start_remote(&handle, spec).await?,
        TunnelKind::Dynamic => crate::ssh::tunnel::start_dynamic(&handle, spec).await?,
    };

    state.tunnels.lock().insert(rule.id.clone(), tunnel);
    Ok(rule.id)
}

/// 停止一条正在运行的转发。
#[tauri::command]
pub async fn forward_stop(rule_id: String, state: State<'_, AppState>) -> AppResult<()> {
    let tunnel = state
        .tunnels
        .lock()
        .remove(&rule_id)
        .ok_or_else(|| AppError::NotFound(format!("转发 {} 未在运行", rule_id)))?;
    crate::ssh::tunnel::stop(tunnel).await
}
