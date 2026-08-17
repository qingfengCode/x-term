//! 桌面会话（RDP/VNC）的 CRUD。
//!
//! 与 sessions 表完全独立，不复用终端的 CRUD。

use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::storage::db::DbConn;

/// 一个桌面连接（RDP 或 VNC）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Desktop {
    pub id: String,
    pub name: String,
    /// "rdp" 或 "vnc"。
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub credential_id: Option<String>,
    /// 所属桌面分组（NULL = 未分组）。
    pub group_id: Option<String>,
    /// 上次使用的 RDP 分辨率（"宽x高"，NULL = 未记忆）。
    pub desktop_size: Option<String>,
    pub sort_order: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// 一个桌面分组（与 sessions 的 groups 表完全独立）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopGroup {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub sort_order: i64,
    pub created_at: String,
}

pub fn list_desktops(conn: &DbConn) -> AppResult<Vec<Desktop>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, protocol, host, port, username, credential_id, group_id, \
         desktop_size, sort_order, created_at, updated_at FROM desktops \
         ORDER BY sort_order ASC, name ASC",
    )?;
    let rows = stmt.query_map([], row_to_desktop)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn get_desktop(conn: &DbConn, id: &str) -> AppResult<Option<Desktop>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, protocol, host, port, username, credential_id, group_id, \
         desktop_size, sort_order, created_at, updated_at FROM desktops WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map([id], row_to_desktop)?;
    if let Some(row) = rows.next() {
        Ok(Some(row?))
    } else {
        Ok(None)
    }
}

/// 只更新分辨率列（断开时保存，不重写整行，避免并发覆盖其他字段）。
pub fn update_desktop_size(conn: &DbConn, id: &str, size: Option<&str>) -> AppResult<()> {
    conn.execute(
        "UPDATE desktops SET desktop_size = ?1, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![size, chrono::Utc::now().to_rfc3339(), id],
    )?;
    Ok(())
}

pub fn upsert_desktop(conn: &DbConn, d: &Desktop) -> AppResult<()> {
    conn.execute(
        "INSERT INTO desktops (id, name, protocol, host, port, username, credential_id, \
         group_id, desktop_size, sort_order, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
         ON CONFLICT(id) DO UPDATE SET \
            name = excluded.name, \
            protocol = excluded.protocol, \
            host = excluded.host, \
            port = excluded.port, \
            username = excluded.username, \
            credential_id = excluded.credential_id, \
            group_id = excluded.group_id, \
            desktop_size = excluded.desktop_size, \
            sort_order = excluded.sort_order, \
            updated_at = excluded.updated_at",
        rusqlite::params![
            d.id,
            d.name,
            d.protocol,
            d.host,
            d.port as i64,
            d.username,
            d.credential_id,
            d.group_id,
            d.desktop_size,
            d.sort_order,
            d.created_at,
            d.updated_at,
        ],
    )?;
    Ok(())
}

pub fn delete_desktop(conn: &DbConn, id: &str) -> AppResult<()> {
    conn.execute("DELETE FROM desktops WHERE id = ?1", [id])?;
    Ok(())
}

pub fn list_desktop_groups(conn: &DbConn) -> AppResult<Vec<DesktopGroup>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, parent_id, sort_order, created_at \
         FROM desktop_groups ORDER BY sort_order ASC, name ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(DesktopGroup {
            id: row.get(0)?,
            name: row.get(1)?,
            parent_id: row.get(2)?,
            sort_order: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn upsert_desktop_group(conn: &DbConn, g: &DesktopGroup) -> AppResult<()> {
    conn.execute(
        "INSERT INTO desktop_groups (id, name, parent_id, sort_order, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(id) DO UPDATE SET \
            name = excluded.name, \
            parent_id = excluded.parent_id, \
            sort_order = excluded.sort_order",
        rusqlite::params![g.id, g.name, g.parent_id, g.sort_order, g.created_at],
    )?;
    Ok(())
}

/// 删除桌面分组（不级联：其下桌面/子分组的悬空引用由前端清理，与 sessions 一致）。
pub fn delete_desktop_group(conn: &DbConn, id: &str) -> AppResult<()> {
    conn.execute("DELETE FROM desktop_groups WHERE id = ?1", [id])?;
    Ok(())
}

fn row_to_desktop(row: &rusqlite::Row<'_>) -> rusqlite::Result<Desktop> {
    let port: i64 = row.get(4)?;
    Ok(Desktop {
        id: row.get(0)?,
        name: row.get(1)?,
        protocol: row.get(2)?,
        host: row.get(3)?,
        port: port as u16,
        username: row.get(5)?,
        credential_id: row.get(6)?,
        group_id: row.get(7)?,
        desktop_size: row.get(8)?,
        sort_order: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}
