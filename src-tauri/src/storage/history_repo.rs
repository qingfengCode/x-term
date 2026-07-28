//! 命令历史的记录与检索。
//!
//! 历史按会话维度存储，并提供关键字搜索。每条记录包含执行时间（`run_at`，由调用方
//! 以 ISO-8601 字符串形式提供）和可选的退出码（`exit_code`）。

use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::storage::db::DbConn;

/// 一条命令历史记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// 自增主键，新增时由数据库分配，故为 `Option`。
    pub id: Option<i64>,
    pub session_id: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub run_at: String,
}

/// 新增一条历史，返回新分配的自增 id。
pub fn add_history(conn: &DbConn, entry: &HistoryEntry) -> AppResult<i64> {
    conn.execute(
        "INSERT INTO history (session_id, command, exit_code, run_at) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![entry.session_id, entry.command, entry.exit_code, entry.run_at],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 列出某会话最近的历史，按 `run_at` 降序（最新在前），最多 `limit` 条。
pub fn list_history(conn: &DbConn, session_id: &str, limit: u32) -> AppResult<Vec<HistoryEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, command, exit_code, run_at \
         FROM history WHERE session_id = ?1 \
         ORDER BY run_at DESC, id DESC LIMIT ?2",
    )?;

    let rows = stmt.query_map(
        rusqlite::params![session_id, limit as i64],
        row_to_entry,
    )?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 关键字模糊搜索历史（在所有会话范围内匹配 command），按 `run_at` 降序。
pub fn search_history(conn: &DbConn, keyword: &str, limit: u32) -> AppResult<Vec<HistoryEntry>> {
    let pattern = format!("%{}%", keyword);
    let mut stmt = conn.prepare(
        "SELECT id, session_id, command, exit_code, run_at \
         FROM history WHERE command LIKE ?1 \
         ORDER BY run_at DESC, id DESC LIMIT ?2",
    )?;

    let rows = stmt.query_map(rusqlite::params![pattern, limit as i64], row_to_entry)?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 把一行数据库记录映射为 [`HistoryEntry`]。
fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
    Ok(HistoryEntry {
        id: Some(row.get(0)?),
        session_id: row.get(1)?,
        command: row.get(2)?,
        exit_code: row.get(3)?,
        run_at: row.get(4)?,
    })
}
