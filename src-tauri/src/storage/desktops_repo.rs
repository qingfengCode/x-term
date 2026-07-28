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
    pub sort_order: i64,
    pub created_at: String,
    pub updated_at: String,
}

pub fn list_desktops(conn: &DbConn) -> AppResult<Vec<Desktop>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, protocol, host, port, username, credential_id, sort_order, \
         created_at, updated_at FROM desktops ORDER BY sort_order ASC, name ASC",
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
        "SELECT id, name, protocol, host, port, username, credential_id, sort_order, \
         created_at, updated_at FROM desktops WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map([id], row_to_desktop)?;
    if let Some(row) = rows.next() {
        Ok(Some(row?))
    } else {
        Ok(None)
    }
}

pub fn upsert_desktop(conn: &DbConn, d: &Desktop) -> AppResult<()> {
    conn.execute(
        "INSERT INTO desktops (id, name, protocol, host, port, username, credential_id, \
         sort_order, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
         ON CONFLICT(id) DO UPDATE SET \
            name = excluded.name, \
            protocol = excluded.protocol, \
            host = excluded.host, \
            port = excluded.port, \
            username = excluded.username, \
            credential_id = excluded.credential_id, \
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
        sort_order: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}
