//! DB profile（数据库连接配置）的 CRUD。
//!
//! 与 [`crate::storage::sessions_repo`] 中的 SSH `Session` 类似，DB profile
//! 描述"如何连到一个 MySQL 服务"：主机、端口、用户名、默认库名、关联的凭据
//! （存密码）、可选的 SSH 隧道会话（连到跳板机后再到 MySQL）。
//!
//! 所有数据持久化在 SQLite 的 `db_profiles` 表中（见
//! [`crate::storage::db::run_migrations`]）。本模块只做读写，不解析凭据——
//! 凭据解密在 [`crate::database::mysql`] 中进行。

use rusqlite::params;

use crate::error::AppResult;
use crate::storage::db::DbConn;

// ===========================================================================
// 数据模型
// ===========================================================================

/// 一个数据库连接配置。
///
/// `kind` 目前固定为 `"mysql"`，保留字段以便未来支持 PostgreSQL 等。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbProfile {
    pub id: String,
    pub name: String,
    /// 数据库类型，目前固定 `"mysql"`。
    pub kind: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub default_database: Option<String>,
    /// 关联的凭据 id（`credentials` 表，kind=`"mysql_password"`）。
    pub credential_id: Option<String>,
    /// 可选：通过哪个 SSH 会话配置建立隧道再连 MySQL。
    pub ssh_session_config_id: Option<String>,
    /// 所属分组 id（`db_groups` 表）。
    pub group_id: Option<String>,
    pub created_at: String,
}

/// 数据库连接分组。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbGroup {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub sort_order: i64,
    pub created_at: String,
}

// ===========================================================================
// CRUD
// ===========================================================================

/// 列出全部 DB profile，按名称排序。
pub fn list_db_profiles(conn: &DbConn) -> AppResult<Vec<DbProfile>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, host, port, username, default_database, \
         credential_id, ssh_session_config_id, group_id, created_at \
         FROM db_profiles ORDER BY name ASC",
    )?;
    let rows = stmt.query_map([], row_to_profile)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 按 id 取单个 profile，不存在返回 `None`。
pub fn get_db_profile(conn: &DbConn, id: &str) -> AppResult<Option<DbProfile>> {
    // 用 query_row + 捕获 NoRows 转 None。
    let res = conn.query_row(
        "SELECT id, name, kind, host, port, username, default_database, \
         credential_id, ssh_session_config_id, group_id, created_at \
         FROM db_profiles WHERE id = ?1",
        params![id],
        row_to_profile,
    );
    match res {
        Ok(p) => Ok(Some(p)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 按 name 取单个 profile，不存在返回 `None`。
///
/// 若有同名 profile（name 非唯一约束），返回第一条。
pub fn get_db_profile_by_name(conn: &DbConn, name: &str) -> AppResult<Option<DbProfile>> {
    let res = conn.query_row(
        "SELECT id, name, kind, host, port, username, default_database, \
         credential_id, ssh_session_config_id, group_id, created_at \
         FROM db_profiles WHERE name = ?1 LIMIT 1",
        params![name],
        row_to_profile,
    );
    match res {
        Ok(p) => Ok(Some(p)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 插入或更新（按 `id` 主键 upsert）一个 profile。
pub fn upsert_db_profile(conn: &DbConn, p: &DbProfile) -> AppResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO db_profiles \
         (id, name, kind, host, port, username, default_database, \
          credential_id, ssh_session_config_id, group_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            p.id,
            p.name,
            p.kind,
            p.host,
            p.port,
            p.username,
            p.default_database,
            p.credential_id,
            p.ssh_session_config_id,
            p.group_id,
            p.created_at,
        ],
    )?;
    Ok(())
}

/// 按 id 删除 profile。不存在不报错。
pub fn delete_db_profile(conn: &DbConn, id: &str) -> AppResult<()> {
    conn.execute("DELETE FROM db_profiles WHERE id = ?1", params![id])?;
    Ok(())
}

// ===========================================================================
// 辅助
// ===========================================================================

/// 把一行 rusqlite 行映射为 [`DbProfile`]。
fn row_to_profile(row: &rusqlite::Row<'_>) -> rusqlite::Result<DbProfile> {
    Ok(DbProfile {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        host: row.get(3)?,
        port: row.get::<_, i64>(4)? as u16,
        username: row.get(5)?,
        default_database: row.get(6)?,
        credential_id: row.get(7)?,
        ssh_session_config_id: row.get(8)?,
        group_id: row.get(9)?,
        created_at: row.get(10)?,
    })
}

// ===========================================================================
// DB 分组 CRUD
// ===========================================================================

/// 列出全部数据库分组。
pub fn list_db_groups(conn: &DbConn) -> AppResult<Vec<DbGroup>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, parent_id, sort_order, created_at \
         FROM db_groups ORDER BY sort_order ASC, name ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(DbGroup {
            id: row.get(0)?,
            name: row.get(1)?,
            parent_id: row.get(2)?,
            sort_order: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 插入或更新一个数据库分组。
pub fn upsert_db_group(conn: &DbConn, g: &DbGroup) -> AppResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO db_groups (id, name, parent_id, sort_order, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![g.id, g.name, g.parent_id, g.sort_order, g.created_at],
    )?;
    Ok(())
}

/// 删除数据库分组（不删除其下的 profile，profile 变为无分组）。
pub fn delete_db_group(conn: &DbConn, id: &str) -> AppResult<()> {
    conn.execute("DELETE FROM db_groups WHERE id = ?1", params![id])?;
    Ok(())
}
