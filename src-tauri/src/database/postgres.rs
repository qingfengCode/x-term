//! PostgreSQL 连接与查询。
//!
//! 结构与 [`crate::database::mysql`] 对称：
//! - [`connect_direct`]：直连（`postgres://user:pass@host:port/db`）。
//! - [`connect_via_ssh`]：先建立到跳板机的 SSH 连接（隧道复用
//!   [`super::open_ssh_tunnel`]），sqlx 连本地随机端口。
//!
//! # 与 MySQL 的关键差异
//! - **切库（USE）**：PG 没有 `USE` 语句，database 在连接握手时指定。
//!   [`PgConn::use_database`] 通过「同参数重连到新库并整体替换 pool」实现
//!   切库——SSH 隧道模式下本地 listener 保持存活（重连的仍是本地随机端口），
//!   不需要重建隧道。
//! - **元数据**：走 `pg_database` / `information_schema` / `pg_catalog`，
//!   见 [`DbConnHandle`](super::DbConnHandle) 的元数据方法。
//! - **结果集语句**：SELECT/WITH/EXPLAIN/SHOW/TABLE/VALUES/DESCRIBE
//!   （TABLE / VALUES 是 PG 特有的隐式查询形式）。

use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::types::{Decimal, JsonValue, Uuid};
use sqlx::{Column, Connection, PgPool, Row};
use std::time::Duration;
use tokio::task::JoinHandle;

use crate::error::{AppError, AppResult};
use crate::ssh::session::ResolvedCredential;
use crate::state::AppState;
use crate::storage::sessions_repo::Session;

use super::mysql::QueryResult;
use super::url_encode_component;

// ===========================================================================
// 数据结构
// ===========================================================================

/// 一条已建立的 PostgreSQL 连接（实际是一个 pool）。
///
/// `pool` 用 RwLock 包裹：PG 切库 = 换库重连 + 整体替换 pool
/// （见 [`PgConn::use_database`]）。`params` 保存建连参数（隧道模式下
/// host 是本地随机端口地址），重连时复用。`_tunnel_handle` 直连时为
/// `None`；SSH 隧道模式保存本地 listener 的 accept 循环句柄，
/// [`PgConn::close`] 时 abort 以释放端口与 SSH channel。
pub struct PgConn {
    pool: tokio::sync::RwLock<PgPool>,
    /// SSH 隧道模式下保存 accept 循环句柄；close 时 abort。
    _tunnel_handle: Option<JoinHandle<()>>,
    /// 建连参数（切库重连用）。
    params: PgParams,
    /// pool 大小上限（直连 8 / 隧道 2）。
    max_connections: u32,
    /// 当前库。None 表示未指定（连接到服务器默认库）。
    current_db: parking_lot::Mutex<Option<String>>,
}

/// 建连参数（密码仅驻留内存，随连接生命周期丢弃）。
#[derive(Clone)]
struct PgParams {
    host: String,
    port: u16,
    username: String,
    password: String,
    /// profile 的默认库：切库传 None 时回落到这里（与 MySQL 语义一致），
    /// 而不是 PG 服务器的默认库（用户同名库）。
    default_db: Option<String>,
}

impl PgConn {
    /// 当前库。
    pub fn current_db(&self) -> Option<String> {
        self.current_db.lock().clone()
    }

    /// 执行一条 SQL，返回 [`QueryResult`]。
    ///
    /// 返回结果集的语句走流式逐行读取（防大表 OOM，超 limit 提前 break +
    /// flush 排空），其余语句取影响行数。切库由连接层完成（重连），
    /// 本方法无需像 MySQL 那样带 `USE`。
    pub async fn execute(&self, sql: &str, limit: u32) -> AppResult<QueryResult> {
        let pool = self.pool.read().await;
        let mut conn = pool.acquire().await?;
        exec_on_conn(&mut conn, sql, limit).await
    }

    /// 切换当前库：同参数重连到新库并替换 pool。
    ///
    /// PG 的 database 在握手时指定，运行期无法切换——这里重建 pool（隧道
    /// listener 复用，无需重新建隧道）。`db` 为 None 时回落到 profile 的
    /// 默认库（与 MySQL `set_current_db(None)` 语义一致）。重连失败（库不
    /// 存在 / 无权限）时返回错误且**保留旧 pool**，连接仍可用。
    pub async fn use_database(&self, db: Option<&str>) -> AppResult<()> {
        if let Some(d) = db {
            // 库名不进 SQL（只进连接 URL，已做百分号编码），这里仅挡掉
            // 空白/控制字符等明显非法值。
            if d.is_empty() || d.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(AppError::InvalidInput(format!("非法库名: {d}")));
            }
        }
        let target = db.or(self.params.default_db.as_deref());
        let url = build_pg_url(
            &self.params.host,
            self.params.port,
            &self.params.username,
            &self.params.password,
            target,
        );
        let new_pool = PgPoolOptions::new()
            .max_connections(self.max_connections)
            .acquire_timeout(PG_CONNECT_TIMEOUT)
            .connect(&url)
            .await
            .map_err(|e| {
                log::warn!("[pg] 切库重连失败 ({}): {}", target.unwrap_or("(默认)"), e);
                AppError::Storage(format!("切换到库 {} 失败: {e}", target.unwrap_or("(默认)")))
            })?;
        let mut guard = self.pool.write().await;
        let old = std::mem::replace(&mut *guard, new_pool);
        old.close().await;
        *self.current_db.lock() = target.map(|s| s.to_string());
        Ok(())
    }

    /// 列出指定库的表（`schema.table` 形式，排除系统 schema）。
    ///
    /// `database` 为 None 或等于当前库时用现有 pool；否则临时连到目标库
    /// 查询（查完即关）——与 MySQL `SHOW TABLES FROM db` 的"不切库列
    /// 其他库"语义对齐。
    pub async fn list_tables(&self, database: Option<&str>, limit: u32) -> AppResult<Vec<String>> {
        const SQL: &str = "SELECT table_schema || '.' || table_name AS name \
                           FROM information_schema.tables \
                           WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
                           ORDER BY 1";
        let cur = self.current_db();
        if database.is_none() || cur.as_deref() == database {
            let qr = self.execute(SQL, limit).await?;
            return Ok(qr.rows.into_iter().filter_map(|mut r| r.pop()).collect());
        }
        let db = database.unwrap_or("");
        if db.is_empty() || db.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(AppError::InvalidInput(format!("非法库名: {db}")));
        }
        // 临时连接目标库（max=1，查完即关）。
        let url = build_pg_url(
            &self.params.host,
            self.params.port,
            &self.params.username,
            &self.params.password,
            Some(db),
        );
        let tmp = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(PG_CONNECT_TIMEOUT)
            .connect(&url)
            .await
            .map_err(|e| AppError::Storage(format!("连接库 {db} 失败: {e}")))?;
        let mut conn = tmp.acquire().await?;
        let qr = exec_on_conn(&mut conn, SQL, limit).await;
        tmp.close().await;
        let qr = qr?;
        Ok(qr.rows.into_iter().filter_map(|mut r| r.pop()).collect())
    }

    /// 关闭连接：先关闭 pool，再 abort 隧道 accept 循环（如有）。
    pub async fn close(&self) {
        let pool = self.pool.read().await.clone();
        pool.close().await;
        if let Some(h) = &self._tunnel_handle {
            h.abort();
        }
    }
}

// ===========================================================================
// 连接
// ===========================================================================

/// 建立连接（含首条连接握手）的超时上限。与 MySQL 侧一致（15s），
/// 避免主机不可达时前端干等默认 30s。
const PG_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 直连 PostgreSQL。
pub async fn connect_direct(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    database: Option<&str>,
) -> AppResult<PgConn> {
    let url = build_pg_url(host, port, username, password, database);
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(PG_CONNECT_TIMEOUT)
        .connect(&url)
        .await
        .map_err(|e| {
            log::warn!("[pg connect_direct] 连接 PostgreSQL 失败 ({}:{}): {}", host, port, e);
            AppError::Storage(format!("连接 PostgreSQL 失败（{}:{}，15 秒超时）：{e}", host, port))
        })?;
    Ok(PgConn {
        pool: tokio::sync::RwLock::new(pool),
        _tunnel_handle: None,
        params: PgParams {
            host: host.to_string(),
            port,
            username: username.to_string(),
            password: password.to_string(),
            default_db: database.map(|s| s.to_string()),
        },
        max_connections: 8,
        current_db: parking_lot::Mutex::new(database.map(|s| s.to_string())),
    })
}

/// 通过 SSH 隧道连 PostgreSQL（隧道建立复用 [`super::open_ssh_tunnel`]）。
#[allow(clippy::too_many_arguments)]
pub async fn connect_via_ssh(
    ssh_session_config: &Session,
    resolved_credential: ResolvedCredential,
    pg_host: &str,
    pg_port: u16,
    pg_user: &str,
    pg_pass: &str,
    pg_db: Option<&str>,
    state: AppState,
) -> AppResult<PgConn> {
    // 1+2. 建立 SSH 连接并在本地随机端口起桥接 listener。
    let (tunnel_handle, local_port) =
        super::open_ssh_tunnel(ssh_session_config, resolved_credential, pg_host, pg_port, state)
            .await?;

    // 3. sqlx 连本地端口。SSH 隧道下限制 pool 大小，避免开过多 channel。
    let url = build_pg_url("127.0.0.1", local_port, pg_user, pg_pass, pg_db);
    let pool = match PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(PG_CONNECT_TIMEOUT)
        .connect(&url)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            // 连接失败：abort 隧道 accept 循环，避免监听端口与 SSH handle 泄漏。
            tunnel_handle.abort();
            log::warn!(
                "[pg connect_via_ssh] 经隧道连接 PostgreSQL 失败 ({}:{}): {}",
                pg_host,
                pg_port,
                e
            );
            return Err(AppError::Storage(format!(
                "经 SSH 隧道连接 PostgreSQL 失败（{}:{}，15 秒超时）：{e}",
                pg_host, pg_port
            )));
        }
    };

    Ok(PgConn {
        pool: tokio::sync::RwLock::new(pool),
        _tunnel_handle: Some(tunnel_handle),
        params: PgParams {
            host: "127.0.0.1".into(),
            port: local_port,
            username: pg_user.to_string(),
            password: pg_pass.to_string(),
            default_db: pg_db.map(|s| s.to_string()),
        },
        max_connections: 2,
        current_db: parking_lot::Mutex::new(pg_db.map(|s| s.to_string())),
    })
}

// ===========================================================================
// 辅助
// ===========================================================================

/// 在一条已 checkout 的连接上执行 SQL（流式读结果集，防大表 OOM）。
///
/// 与 [`crate::database::mysql::MySqlConn::execute`] 的流式逻辑一致：只保留
/// 前 `limit` 行后提前 break，再用 flush 排空剩余结果集，保证连接回池后
/// 无协议残留。
async fn exec_on_conn(
    conn: &mut sqlx::postgres::PgConnection,
    sql: &str,
    limit: u32,
) -> AppResult<QueryResult> {
    if is_query_stmt(sql) {
        use futures::TryStreamExt;
        let mut stream = sqlx::query(sql).fetch(&mut *conn);
        let mut columns: Vec<String> = Vec::new();
        let mut out_rows: Vec<Vec<String>> = Vec::with_capacity(limit.min(1024) as usize);
        let mut seen: u64 = 0;
        let mut truncated = false;
        while let Some(row) = stream.try_next().await? {
            seen += 1;
            if columns.is_empty() {
                columns = row
                    .columns()
                    .iter()
                    .map(|c| c.name().to_string())
                    .collect();
            }
            if seen > limit as u64 {
                truncated = true;
                break;
            }
            let mut vals: Vec<String> = Vec::with_capacity(row.columns().len());
            for idx in 0..row.columns().len() {
                vals.push(cell_to_string(&row, idx));
            }
            out_rows.push(vals);
        }
        if truncated {
            drop(stream);
            conn.flush().await?;
        }
        let affected = out_rows.len() as u64;
        Ok(QueryResult {
            columns,
            rows: out_rows,
            affected,
            truncated,
        })
    } else {
        let res = sqlx::query(sql).execute(&mut *conn).await?;
        Ok(QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            affected: res.rows_affected(),
            truncated: false,
        })
    }
}

/// 判断 SQL 是否为"返回结果集"的语句（需要流式读取）。
///
/// TABLE（`TABLE t` ≡ `SELECT * FROM t`）与 VALUES 是 PG 特有的隐式查询
/// 形式；DESC/DESCRIBE 在 PG 侧不可用（元数据命令走 information_schema），
/// 保留匹配是为了与 MySQL 侧判定一致（误判只会走 execute 分支报语法错误）。
fn is_query_stmt(sql: &str) -> bool {
    let trimmed = sql.trim_start();
    let first = trimmed
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim_end_matches('(');
    let upper = first.to_uppercase();
    matches!(
        upper.as_str(),
        "SELECT" | "SHOW" | "EXPLAIN" | "DESC" | "DESCRIBE" | "WITH" | "TABLE" | "VALUES"
    )
}

/// 把一个 cell 转为字符串。
///
/// PG 是强类型协议：`try_get::<String>` 只接受文本类 OID，数值/时间/uuid/
/// json/数组等必须按各自类型逐级尝试解码，全部失败回退 `"<binary>"`。
fn cell_to_string(row: &PgRow, idx: usize) -> String {
    // 文本类。
    match row.try_get::<Option<String>, _>(idx) {
        Ok(Some(s)) => return s,
        Ok(None) => return String::new(),
        Err(_) => {}
    }
    // 布尔。
    if let Ok(v) = row.try_get::<Option<bool>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    // 数值类：smallint/int/bigint/real/double。
    if let Ok(v) = row.try_get::<Option<i16>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<i32>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<f32>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    // 时间类：timestamp/timestamptz/date/time/interval。
    if let Ok(v) = row.try_get::<Option<NaiveDateTime>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<DateTime<Utc>>, _>(idx) {
        return v.map(|x| x.naive_utc().to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<NaiveDate>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<NaiveTime>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    // INTERVAL：PgInterval（months/days/microseconds），拼成可读文本。
    if let Ok(v) = row.try_get::<Option<sqlx::postgres::types::PgInterval>, _>(idx) {
        return v
            .map(|iv| {
                let days = iv.days + iv.months * 30;
                let secs = iv.microseconds / 1_000_000;
                let micros = iv.microseconds % 1_000_000;
                format!("{days}d {secs}.{micros:06}s")
            })
            .unwrap_or_default();
    }
    // NUMERIC。
    if let Ok(v) = row.try_get::<Option<Decimal>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    // JSON / JSONB：紧凑 JSON 文本。
    if let Ok(v) = row.try_get::<Option<JsonValue>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    // UUID。
    if let Ok(v) = row.try_get::<Option<Uuid>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    // 一维文本数组（text[]/varchar[] 等）。
    if let Ok(v) = row.try_get::<Option<Vec<String>>, _>(idx) {
        return v.map(|x| format!("[{}]", x.join(","))).unwrap_or_default();
    }
    // bytea 兜底：按 UTF-8 lossy 解码。
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(idx) {
        return v
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
    }
    "<binary>".to_string()
}

/// 构造 `postgres://user:pass@host:port/db` URL。
///
/// 用户名/密码/库名做百分号编码（库名可含 `-` 等字符）。
fn build_pg_url(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    database: Option<&str>,
) -> String {
    let mut url = String::from("postgres://");
    url.push_str(&url_encode_component(username));
    url.push(':');
    url.push_str(&url_encode_component(password));
    url.push('@');
    url.push_str(host);
    url.push(':');
    url.push_str(&port.to_string());
    url.push('/');
    url.push_str(&url_encode_component(database.unwrap_or("")));
    url
}
