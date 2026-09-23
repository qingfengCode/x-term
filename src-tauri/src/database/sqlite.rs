//! SQLite 本地文件连接。
//!
//! 复用存储层已有的 rusqlite（bundled）。rusqlite 是同步 API：连接包
//! `Arc<tokio::sync::Mutex>`，所有查询经 `spawn_blocking` 在阻塞线程池执行，
//! 不阻塞 tokio worker。
//!
//! 语义对齐 MySQL/PG 变体：`execute` 返回统一的 [`QueryResult`]（值转 String、
//! NULL → 空串）；`describe_table` 用 `PRAGMA table_info` 拼出与 MySQL
//! `DESCRIBE` 同构的 Field/Type/Null/Default 四列，前端与 AI 上下文无差别消费。

use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::Mutex;

use crate::error::{AppError, AppResult};

pub use super::QueryResult;

/// 一个打开的 SQLite 数据库文件。
pub struct SqliteConn {
    conn: Arc<Mutex<Connection>>,
    /// 数据库文件路径（展示/日志用）。
    pub path: String,
}

impl SqliteConn {
    /// 打开（或创建）一个本地 SQLite 数据库文件。
    ///
    /// 启用 WAL（并发读写更友好）、`busy_timeout`（与存储层一致：锁竞争时等待
    /// 而非立即返回 SQLITE_BUSY——用户可能同时用其它工具打开同一文件）与
    /// foreign_keys（与存储层 PRAGMA 一致）。
    pub fn open(path: &str) -> AppResult<Self> {
        let conn = Connection::open(path)
            .map_err(|e| AppError::Storage(format!("打开 SQLite 数据库失败: {e}")))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| AppError::Storage(format!("设置 WAL 失败: {e}")))?;
        // 忙等待上限（毫秒）：默认 0 会在其它连接/进程持写锁时立刻 SQLITE_BUSY。
        conn.pragma_update(None, "busy_timeout", 5000)
            .map_err(|e| AppError::Storage(format!("设置 busy_timeout 失败: {e}")))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| AppError::Storage(format!("启用外键约束失败: {e}")))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: path.to_string(),
        })
    }

    /// 执行一条 SQL（SELECT 返回行，非 SELECT 返回影响行数）。
    pub async fn execute(&self, sql: &str, limit: u32) -> AppResult<QueryResult> {
        let conn = self.conn.clone();
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            exec_on(&conn, &sql, limit)
        })
        .await
        .map_err(|e| AppError::Storage(format!("后台任务失败: {e}")))?
    }

    /// 列出表与视图（sqlite_master，排除内部表）。
    pub async fn list_tables(&self) -> AppResult<Vec<String>> {
        let qr = self
            .execute(
                "SELECT name FROM sqlite_master \
                 WHERE type IN ('table','view') \
                   AND name NOT LIKE 'sqlite_%' ORDER BY name",
                10_000,
            )
            .await?;
        Ok(qr.rows.into_iter().filter_map(|mut r| r.pop()).collect())
    }

    /// 表结构：PRAGMA table_info 映射为 MySQL DESCRIBE 同构的四列。
    pub async fn describe_table(&self, table: &str) -> AppResult<QueryResult> {
        // PRAGMA 不能参数化，表名走统一标识符校验 + 双引号包裹
        // （SQLite 标准引用形式；反引号虽兼容但不用于 PRAGMA 参数位）。
        let quoted = super::quote_ident(table, '"')?;
        let qr = self
            .execute(&format!("PRAGMA table_info({quoted})"), 1_000)
            .await?;
        // sqlite_master 行序：cid|name|type|notnull|dflt_value|pk
        let mut rows = Vec::with_capacity(qr.rows.len());
        for r in &qr.rows {
            // r = [cid, name, type, notnull, dflt_value, pk]
            if r.len() < 6 {
                continue;
            }
            let notnull = r[3] == "1";
            let pk = r[5] != "0";
            let mut t = r[2].clone();
            if pk && t.eq_ignore_ascii_case("integer") {
                t.push_str(" (rowid 主键)");
            }
            rows.push(vec![
                r[1].clone(),                            // Field
                t,                                       // Type
                if notnull { "NO".into() } else { "YES".into() }, // Null
                r[4].clone(),                            // Default
            ]);
        }
        Ok(QueryResult {
            columns: vec![
                "Field".into(),
                "Type".into(),
                "Null".into(),
                "Default".into(),
            ],
            affected: rows.len() as u64,
            rows,
            truncated: false,
        })
    }

    /// 建表 DDL（sqlite_master 的原始 sql 列）。
    pub async fn table_ddl(&self, table: &str) -> AppResult<String> {
        // 标识符校验后按字符串字面量比较（内部单引号再转义，双保险）。
        super::validate_ident(table)?;
        let lit = format!("'{}'", table.replace('\'', "''"));
        let qr = self
            .execute(
                &format!("SELECT sql FROM sqlite_master WHERE name = {lit}"),
                1,
            )
            .await?;
        Ok(qr
            .rows
            .first()
            .and_then(|r| r.first().cloned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("-- 无法获取 {table} 的建表语句"))
    )

    }

    /// 关闭（丢弃共享句柄；连接随最后一个 Arc 释放）。
    pub async fn close(&self) {}
}

/// 在已持有连接上执行（阻塞线程池内）。
fn exec_on(conn: &Connection, sql: &str, limit: u32) -> AppResult<QueryResult> {
    let limit = limit.max(1) as usize;
    // 先按语句首关键字判断是否返回行（rusqlite 的 prepare 对 PRAGMA 等也返回列，
    // 统一尝试 prepare；错误再走 execute 路径）。
    let trimmed = sql.trim_start();
    let first_kw = trimmed
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let returns_rows = matches!(
        first_kw.as_str(),
        "SELECT" | "PRAGMA" | "WITH" | "EXPLAIN" | "VALUES" | "TABLE"
    );
    if returns_rows {
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| AppError::Storage(format!("SQL 预处理失败: {e}")))?;
        let col_count = stmt.column_count();
        let columns: Vec<String> = (0..col_count)
            .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
            .collect();
        let mut rows_out: Vec<Vec<String>> = Vec::new();
        let mut rows = stmt
            .query([])
            .map_err(|e| AppError::Storage(format!("SQL 执行失败: {e}")))?;
        let mut truncated = false;
        while let Some(row) = rows.next().map_err(row_err)? {
            if rows_out.len() >= limit {
                truncated = true;
                break;
            }
            let mut vals = Vec::with_capacity(col_count);
            for i in 0..col_count {
                let v = match row.get_ref(i) {
                    Ok(rusqlite::types::ValueRef::Null) => String::new(),
                    Ok(rusqlite::types::ValueRef::Integer(n)) => n.to_string(),
                    Ok(rusqlite::types::ValueRef::Real(f)) => f.to_string(),
                    Ok(rusqlite::types::ValueRef::Text(t)) => {
                        String::from_utf8_lossy(t).into_owned()
                    }
                    Ok(rusqlite::types::ValueRef::Blob(b)) => {
                        format!("<binary {} bytes>", b.len())
                    }
                    Err(e) => return Err(row_err(e)),
                };
                vals.push(v);
            }
            rows_out.push(vals);
        }
        let affected = rows_out.len() as u64;
        Ok(QueryResult {
            columns,
            rows: rows_out,
            affected,
            truncated,
        })
    } else {
        let affected = conn
            .execute(sql, [])
            .map_err(|e| AppError::Storage(format!("SQL 执行失败: {e}")))?;
        Ok(QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            affected: affected as u64,
            truncated: false,
        })
    }
}

fn row_err(e: rusqlite::Error) -> AppError {
    AppError::Storage(format!("读取行失败: {e}"))
}
