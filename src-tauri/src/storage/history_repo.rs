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

/// 新增一条历史（同命令去重：先删旧行再插入，使每条命令只保留最新一次执行）。
///
/// 与 [`add_history`] 的区别：补全建议按命令文本匹配，同命令保留多行只会
/// 让建议列表重复；"删旧插新"同时实现了天然的 LRU 语义（最近使用的命令
/// id 最大，排在 [`list_recent_history`] 结果最前）。
pub fn add_history_dedup(conn: &DbConn, entry: &HistoryEntry) -> AppResult<i64> {
    // 事务包裹：DELETE+INSERT 原子化。连接来自 r2d2 池，两条语句若落在
    // 不同连接上交错执行（两个终端并发记录同一命令），会留下重复行，
    // 破坏"每条命令只保留最新一次"的去重语义。
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM history WHERE command = ?1",
        rusqlite::params![entry.command],
    )?;
    tx.execute(
        "INSERT INTO history (session_id, command, exit_code, run_at) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            entry.session_id,
            entry.command,
            entry.exit_code,
            entry.run_at
        ],
    )?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(id)
}

/// 全局最近历史（跨所有会话，按命令去重），按新→旧排列，最多 `limit` 条。
///
/// 终端补全建议的数据源：shell 的 Ctrl+R 式体验是全局的，不区分会话。
/// 去重用 `GROUP BY command` 取每组最大 id（即最近一次执行）。
pub fn list_recent_history(conn: &DbConn, limit: u32) -> AppResult<Vec<HistoryEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, command, exit_code, run_at FROM history \
         WHERE id IN (SELECT MAX(id) FROM history GROUP BY command) \
         ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![limit as i64], row_to_entry)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 删除一条历史（补全弹窗的删除按钮）。
pub fn delete_history(conn: &DbConn, id: i64) -> AppResult<()> {
    conn.execute("DELETE FROM history WHERE id = ?1", rusqlite::params![id])?;
    Ok(())
}

/// 新增一条历史，返回新分配的自增 id。
pub fn add_history(conn: &DbConn, entry: &HistoryEntry) -> AppResult<i64> {
    conn.execute(
        "INSERT INTO history (session_id, command, exit_code, run_at) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            entry.session_id,
            entry.command,
            entry.exit_code,
            entry.run_at
        ],
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

    let rows = stmt.query_map(rusqlite::params![session_id, limit as i64], row_to_entry)?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 关键字模糊搜索历史（在所有会话范围内匹配 command），按 `run_at` 降序。
pub fn search_history(conn: &DbConn, keyword: &str, limit: u32) -> AppResult<Vec<HistoryEntry>> {
    // LIKE 通配符转义：用户搜 "%" / "_" 时按字面匹配，而不是当成任意字符通配
    // （否则搜 "50%" 会命中所有含 "50" 的纪录）。
    let escaped = keyword
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("%{}%", escaped);
    let mut stmt = conn.prepare(
        "SELECT id, session_id, command, exit_code, run_at \
         FROM history WHERE command LIKE ?1 ESCAPE '\\' \
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
