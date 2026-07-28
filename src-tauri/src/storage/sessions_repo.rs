//! 会话（Session）和分组（Group）的 CRUD 操作。
//!
//! 所有函数都接受一个 `&DbConn`（来自 [`crate::storage::db`] 的连接），便于在命令处理器
//! 中以连接为单位操作。`auth_type` 在数据库中以 TEXT 存储，值为
//! `"Password"` / `"PrivateKey"` / `"Agent"`，与 [`AuthType`] 的 `as_str` 一致。

use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::storage::db::DbConn;

// ===========================================================================
// 数据模型
// ===========================================================================

/// 会话认证方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
// 注意：此处必须用 PascalCase（"Password"/"PrivateKey"/"Agent"），与 `as_str()`、
// 数据库存储格式以及前端 TS 枚举 (`AuthType`) 保持一致。之前用 camelCase 会导致
// 前端调用 save_session 时 serde 反序列化失败：
// "unknown variant `Password`, expected one of `password`, `privateKey`, `agent`"。
#[serde(rename_all = "PascalCase")]
pub enum AuthType {
    Password,
    PrivateKey,
    Agent,
}

impl AuthType {
    /// 转为数据库存储用的字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            AuthType::Password => "Password",
            AuthType::PrivateKey => "PrivateKey",
            AuthType::Agent => "Agent",
        }
    }

    /// 从数据库存储的字符串解析。
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "Password" => Some(AuthType::Password),
            "PrivateKey" => Some(AuthType::PrivateKey),
            "Agent" => Some(AuthType::Agent),
            _ => None,
        }
    }
}

/// 一个 SSH 会话定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub name: String,
    pub group_id: Option<String>,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_type: AuthType,
    pub credential_id: Option<String>,
    pub key_path: Option<String>,
    pub jump_session_id: Option<String>,
    pub startup_script: Option<String>,
    pub tags: Option<String>,
    pub color: Option<String>,
    pub sort_order: i64,
    pub created_at: String,
    pub updated_at: String,
    /// 协议：ssh / telnet / rdp / vnc。默认 ssh（兼容旧数据）。
    #[serde(default = "default_protocol")]
    pub protocol: String,
    /// 所属空间 id："local"（本地空间）或 JumpServer 空间 id。
    #[serde(default = "default_space_id")]
    pub space_id: String,
}

fn default_protocol() -> String {
    "ssh".into()
}

fn default_space_id() -> String {
    "local".into()
}

/// 一个会话分组（用于树形组织）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub sort_order: i64,
    pub created_at: String,
}

// ===========================================================================
// Session CRUD
// ===========================================================================

/// 列出所有会话，按 `sort_order`、`name` 升序排列。
pub fn list_sessions(conn: &DbConn) -> AppResult<Vec<Session>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, group_id, host, port, username, auth_type, credential_id, \
         key_path, jump_session_id, startup_script, tags, color, sort_order, \
         created_at, updated_at, protocol, space_id \
         FROM sessions ORDER BY sort_order ASC, name ASC",
    )?;

    let rows = stmt.query_map([], row_to_session)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 根据 id 取单个会话；不存在则返回 `None`。
pub fn get_session(conn: &DbConn, id: &str) -> AppResult<Option<Session>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, group_id, host, port, username, auth_type, credential_id, \
         key_path, jump_session_id, startup_script, tags, color, sort_order, \
         created_at, updated_at, protocol, space_id \
         FROM sessions WHERE id = ?1",
    )?;

    let mut rows = stmt.query_map([id], row_to_session)?;
    if let Some(row) = rows.next() {
        Ok(Some(row?))
    } else {
        Ok(None)
    }
}

/// 插入或更新（基于 `id` 主键）一个会话。
pub fn upsert_session(conn: &DbConn, s: &Session) -> AppResult<()> {
    conn.execute(
        "INSERT INTO sessions (id, name, group_id, host, port, username, auth_type, \
         credential_id, key_path, jump_session_id, startup_script, tags, color, \
         sort_order, created_at, updated_at, protocol, space_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18) \
         ON CONFLICT(id) DO UPDATE SET \
            name = excluded.name, \
            group_id = excluded.group_id, \
            host = excluded.host, \
            port = excluded.port, \
            username = excluded.username, \
            auth_type = excluded.auth_type, \
            credential_id = excluded.credential_id, \
            key_path = excluded.key_path, \
            jump_session_id = excluded.jump_session_id, \
            startup_script = excluded.startup_script, \
            tags = excluded.tags, \
            color = excluded.color, \
            sort_order = excluded.sort_order, \
            updated_at = excluded.updated_at, \
            protocol = excluded.protocol, \
            space_id = excluded.space_id",
        rusqlite::params![
            s.id,
            s.name,
            s.group_id,
            s.host,
            s.port as i64,
            s.username,
            s.auth_type.as_str(),
            s.credential_id,
            s.key_path,
            s.jump_session_id,
            s.startup_script,
            s.tags,
            s.color,
            s.sort_order,
            s.created_at,
            s.updated_at,
            s.protocol,
            s.space_id,
        ],
    )?;
    Ok(())
}

/// 删除指定会话。
pub fn delete_session(conn: &DbConn, id: &str) -> AppResult<()> {
    conn.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
    Ok(())
}

/// 把一行数据库记录映射为 [`Session`]。
fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let auth_type_str: String = row.get(6)?;
    let auth_type = AuthType::from_str(&auth_type_str).unwrap_or(AuthType::Password);
    let port: i64 = row.get(4)?;
    Ok(Session {
        id: row.get(0)?,
        name: row.get(1)?,
        group_id: row.get(2)?,
        host: row.get(3)?,
        port: port as u16,
        username: row.get(5)?,
        auth_type,
        credential_id: row.get(7)?,
        key_path: row.get(8)?,
        jump_session_id: row.get(9)?,
        startup_script: row.get(10)?,
        tags: row.get(11)?,
        color: row.get(12)?,
        sort_order: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
        protocol: row.get(16).unwrap_or_else(|_| "ssh".to_string()),
        space_id: row.get(17).unwrap_or_else(|_| "local".to_string()),
    })
}

// ===========================================================================
// Group CRUD
// ===========================================================================

/// 列出所有分组，按 `sort_order`、`name` 升序排列。
pub fn list_groups(conn: &DbConn) -> AppResult<Vec<Group>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, parent_id, sort_order, created_at \
         FROM groups ORDER BY sort_order ASC, name ASC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(Group {
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

/// 插入或更新一个分组。
pub fn upsert_group(conn: &DbConn, g: &Group) -> AppResult<()> {
    conn.execute(
        "INSERT INTO groups (id, name, parent_id, sort_order, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(id) DO UPDATE SET \
            name = excluded.name, \
            parent_id = excluded.parent_id, \
            sort_order = excluded.sort_order",
        rusqlite::params![g.id, g.name, g.parent_id, g.sort_order, g.created_at],
    )?;
    Ok(())
}

/// 删除指定分组。
pub fn delete_group(conn: &DbConn, id: &str) -> AppResult<()> {
    conn.execute("DELETE FROM groups WHERE id = ?1", [id])?;
    Ok(())
}
