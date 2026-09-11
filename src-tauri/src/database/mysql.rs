//! MySQL 连接与查询。
//!
//! 本模块提供两种连接方式：
//! - [`connect_direct`]：直连 MySQL（`mysql://user:pass@host:port/db`）。
//! - [`connect_via_ssh`]：先建立到跳板机的 SSH 连接，再通过
//!   `channel_open_direct_tcpip` 把本地随机端口桥接到远程 MySQL 端口，
//!   sqlx 连本地端口（透明走 SSH 隧道）。
//!
//! # SSH 隧道实现说明
//! russh 的 channel 不是网络地址，sqlx 不能直接用。方案：在 `127.0.0.1:0`
//! 起一个 `TcpListener`，对每条入站 TCP 连接（即 sqlx pool 中的每条连接）
//! 开一个新的 `channel_open_direct_tcpip`，spawn `copy_bidirectional` 桥接。
//! accept 循环句柄保存在 [`MySqlConn::_tunnel_handle`]，连接关闭时 abort。
//!
//! 为避免 SSH 上开太多 channel，pool 大小限制为 2。

use sqlx::mysql::types::MySqlTime;
use sqlx::mysql::{MySqlPoolOptions, MySqlRow};
use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::types::{Decimal, JsonValue};
use sqlx::{Column, Connection, MySqlPool, Row};
use std::time::Duration;

use crate::error::{AppError, AppResult};
use crate::ssh::session::ResolvedCredential;
use crate::state::AppState;
use crate::storage::sessions_repo::Session;

pub use super::QueryResult;
use super::url_encode_component;

// ===========================================================================
// 标识符解析
// ===========================================================================

/// 把表标识符解析为可安全拼接进 SQL 的反引号限定名。
///
/// 支持两种输入：
/// - `table`（无点）→ `` `table` ``
/// - `db.table`（恰好一个点）→ `` `db`.`table` ``
///
/// 校验规则：`db` / `table` 两段都只允许 `[A-Za-z0-9_]`（非空），禁止空白、
/// 分号、反引号、注释符、`-`、`/` 等——防止 `` DESCRIBE `a`.`b` `` 这类拼接被注入。
/// 注意：旧实现把整个 `db.table` 包进一对反引号（`` `db.table` ``），MySQL 会把它
/// 当成"默认库下一张叫 db.table 的表"，在默认库为空时报 1046 No database selected。
/// 本函数按点拆分、分别反引号包裹，正确表达限定名语义。
///
/// 非法输入返回 `Err`（含可读错误信息）。
pub fn qualify_table_identifier(table: &str) -> AppResult<String> {
    let parts: Vec<&str> = table.split('.').collect();
    if parts.is_empty() || parts.len() > 2 {
        return Err(AppError::InvalidInput(format!("非法表标识符: {table}")));
    }
    // 每段必须非空且仅含 [A-Za-z0-9_]。
    for p in &parts {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(AppError::InvalidInput(format!("非法表标识符: {table}")));
        }
    }
    Ok(match parts.len() {
        1 => format!("`{}`", parts[0]),
        _ => format!("`{}`.`{}`", parts[0], parts[1]),
    })
}

/// 校验库名是否可安全拼进 `` USE `db` ``。
///
/// 规则与 [`qualify_table_identifier`] 一致：非空且仅含 `[A-Za-z0-9_]`，
/// 禁止空白、分号、反引号、注释符等，防止 USE 拼接被注入。
pub fn validate_database_identifier(db: &str) -> AppResult<()> {
    if db.is_empty() || !db.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(AppError::InvalidInput(format!("非法库名: {db}")));
    }
    Ok(())
}

/// 识别 `USE <库名>` 语句（库名允许反引号包裹），返回目标库名。
///
/// - `None`：不是 USE 语句（交给正常执行流程）；
/// - `Some(None)`：`USE` 后没有库名（语法错误，由上层报错）；
/// - `Some(Some(db))`：切换目标库。
///
/// `USE` 不能走 prepared statement 协议（MySQL 1295），且 pool 语义下裸 USE
/// 只对单条连接生效——因此所有执行入口（控制台 / AI 工具）都应拦截 USE 并
/// 更新连接的 current_db，而不是把语句发给 MySQL。
pub fn parse_use_statement(sql: &str) -> Option<Option<String>> {
    // 剥掉行首注释（-- / # / /* */），防 `-- 注释\nUSE x` 绕过关键字识别。
    let mut t = sql.trim_start();
    loop {
        if let Some(rest) = t.strip_prefix("--") {
            t = rest
                .split_once('\n')
                .map(|(_, after)| after)
                .unwrap_or("")
                .trim_start();
        } else if let Some(rest) = t.strip_prefix('#') {
            t = rest
                .split_once('\n')
                .map(|(_, after)| after)
                .unwrap_or("")
                .trim_start();
        } else if let Some(rest) = t.strip_prefix("/*") {
            match rest.find("*/") {
                Some(end) => t = rest[end + 2..].trim_start(),
                None => return None, // 未闭合块注释：不算 USE
            }
        } else {
            break;
        }
    }
    // 首个关键字必须是 USE。
    let (kw, rest) = match t.split_once(char::is_whitespace) {
        Some((k, r)) => (k, r.trim_start()),
        None => (t, ""),
    };
    if !kw.eq_ignore_ascii_case("USE") {
        return None;
    }
    // 库名 token：到空白或分号为止，去掉可能包裹的反引号（MySQL）或
    // 双引号（PostgreSQL `USE "db"` 习惯写法；本函数为两方言共用的拦截器）。
    let mut end = 0;
    for (i, c) in rest.char_indices() {
        if c.is_whitespace() || c == ';' {
            end = i;
            break;
        }
        end = i + c.len_utf8();
    }
    let name = rest[..end]
        .trim_matches(|c| c == '`' || c == '"')
        .to_string();
    if name.is_empty() {
        return Some(None); // `USE` 后没有库名：语法错误，由上层报错
    }
    // 尾部只允许空 / 分号 / 注释（`USE db;` / `USE db -- 说明` / `USE db /* 说明 */`）。
    // `USE db; SELECT 1` 这类多语句不是纯 USE——必须返回 None 交给正常执行
    // 流程（sqlx 会报多语句语法错误），否则后面的语句被本层静默吞掉，
    // 用户既看不到结果也看不到报错。
    let tail = rest[end..].trim_start();
    if !tail.is_empty() && !is_use_tail_ok(tail) {
        return None;
    }
    Some(Some(name))
}

/// USE 语句尾部的合法性：分号（后面可再有注释）或直接是注释（`--` / `#` / `/* */`）。
fn is_use_tail_ok(tail: &str) -> bool {
    let mut t = tail.trim_start();
    if let Some(rest) = t.strip_prefix(';') {
        t = rest.trim_start();
    }
    if t.is_empty() {
        return true;
    }
    t.starts_with("--") || t.starts_with('#') || t.starts_with("/*")
}

// ===========================================================================
// 数据结构
// ===========================================================================

/// 一条已建立的 MySQL 连接（实际是一个 pool）。
///
/// `current_db` 是连接的"当前库"（schema）：pool 的每条连接在查询前都会先
/// 执行 `USE \`current_db\``，因此对 pool 语义稳定（与连接复用无关）。
/// `_tunnel_handle` 在直连时为 `None`；SSH 隧道模式下保存本地 listener 的
/// accept 循环任务句柄，[`MySqlConn::close`] 时 abort 以释放端口与 SSH channel。
pub struct MySqlConn {
    pub pool: MySqlPool,
    /// SSH 隧道模式下保存 accept 循环句柄；drop 时 abort。
    _tunnel_handle: Option<tokio::task::JoinHandle<()>>,
    /// 当前库（schema）。由 `db_use_database` / `USE` 语句更新。
    current_db: parking_lot::Mutex<Option<String>>,
}

impl MySqlConn {
    /// 当前库（schema）。
    pub fn current_db(&self) -> Option<String> {
        self.current_db.lock().clone()
    }

    /// 设置当前库（schema）。None 表示清除（回落到连接 URL 里的默认库）。
    pub fn set_current_db(&self, db: Option<String>) {
        *self.current_db.lock() = db;
    }

    /// 执行一条 SQL，返回 [`QueryResult`]。
    ///
    /// `db` 为连接的当前库（schema）：非空时先在同一连接上执行 `USE \`db\``，
    /// 再执行用户 SQL——pool 连接复用导致裸 `USE` 只对单条连接生效，因此每次
    /// 查询都携带 USE，只要与查询落在同一个 checkout 上，语义就稳定。
    ///
    /// 对于返回结果集的语句（SELECT/SHOW/EXPLAIN/DESC/WITH 等）走 `fetch_all`，
    /// 仅保留前 `limit` 行；其余语句走 `execute` 取影响行数。
    pub async fn execute(&self, sql: &str, limit: u32, db: Option<&str>) -> AppResult<QueryResult> {
        // 取一条连接；USE 与查询必须在同一连接上执行。
        let mut conn = self.pool.acquire().await?;
        if let Some(db) = db {
            validate_database_identifier(db)?;
            // `USE` 不支持 prepared statement 协议（MySQL 1295 HY000 "This command
            // is not supported in the prepared statement protocol yet"），必须走文本
            // 协议（COM_QUERY）：raw_sql 的 arguments 为 None，sqlx 即用文本协议；
            // sqlx::query 即使不带参数也会走 COM_STMT_PREPARE，USE 必然报 1295。
            let use_sql = format!("USE `{}`", db);
            sqlx::Executor::execute(&mut *conn, sqlx::raw_sql(&use_sql))
                .await
                .map_err(|e| AppError::Storage(format!("切换到库 `{db}` 失败: {e}")))?;
        }
        if is_query_stmt(sql) {
            // 流式逐行读取，而不是 fetch_all：fetch_all 会把整个结果集一次性
            // 读进内存，超大表（百万行+）直接 OOM。这里只保留前 limit 行后
            // 提前 break，再用 flush 把连接上未读完的剩余结果集排空，保证
            // 连接回池后无协议残留（sqlx 不会自动排空，见 pool release 路径）。
            use futures::TryStreamExt;
            let mut stream = sqlx::query(sql).fetch(&mut *conn);
            let mut columns: Vec<String> = Vec::new();
            let mut out_rows: Vec<Vec<String>> = Vec::with_capacity(limit as usize);
            let mut seen: u64 = 0;
            let mut truncated = false;
            while let Some(row) = stream.try_next().await? {
                seen += 1;
                // 列名：从第一行取；若 0 行则无法拿到列（MySQL 在 0 行时
                // columns 为空），保持空数组。
                if columns.is_empty() {
                    columns = row
                        .columns()
                        .iter()
                        .map(|c| c.name().to_string())
                        .collect();
                }
                if seen > limit as u64 {
                    // 超过 limit 上限：停止读取（不占内存）。
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
                // 流提前结束会释放对连接的借用；drop 后连接上仍有未读行，
                // 用 flush 排空剩余结果集、状态复位后再回池。
                drop(stream);
                conn.flush().await?;
            }
            // 只统计实际返回的行数（截断后即 limit 行）。无法得知全量行数
            // ——截断与否由 truncated 标记表达，避免"显示 100 行却报 101 行"。
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

    /// 关闭连接：先关闭 pool，再 abort 隧道 accept 循环（如有）。
    /// 取 `&self` 即可（pool.close / handle.abort 都是 &self），支持 Arc 共享调用。
    pub async fn close(&self) {
        self.pool.close().await;
        if let Some(h) = &self._tunnel_handle {
            h.abort();
        }
    }
}

// ===========================================================================
// 连接
// ===========================================================================

/// 建立连接（含首条连接握手）的超时上限。
///
/// sqlx 的 `PoolOptions::connect` 受 `acquire_timeout` 约束，默认 30s：主机
/// 不可达 / 防火墙丢包 / DNS 挂起时前端要干等 30s 才见到报错（表现为"展开
/// 实例要等很久"）。这里收紧到 15s，与 SSH 默认连接超时（15s）一致；空闲
/// pool 的运行时 acquire 不受影响（立即返回）。
const MYSQL_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 直连 MySQL。
pub async fn connect_direct(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    database: Option<&str>,
) -> AppResult<MySqlConn> {
    let url = build_mysql_url(host, port, username, password, database);
    let pool = MySqlPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(MYSQL_CONNECT_TIMEOUT)
        .connect(&url)
        .await
        .map_err(|e| {
            log::warn!("[connect_direct] 连接 MySQL 失败 ({}:{}): {}", host, port, e);
            AppError::Storage(format!("连接 MySQL 失败（{}:{}，15 秒超时）：{e}", host, port))
        })?;
    Ok(MySqlConn {
        pool,
        _tunnel_handle: None,
        current_db: parking_lot::Mutex::new(database.map(|s| s.to_string())),
    })
}

/// 通过 SSH 隧道连 MySQL。
///
/// 流程：
/// 1. 用 `ssh_session_config` 建立 SSH 连接（凭据由 `resolved_credential` 提供）。
/// 2. 在 `127.0.0.1:0` 起本地 listener，对每条入站 TCP 连接开一个新的
///    `channel_open_direct_tcpip(mysql_host, mysql_port)`，spawn 双向桥接
///    （见 [`super::open_ssh_tunnel`]）。
/// 3. sqlx 连本地 listener 的随机端口。
///
/// `state` 用于 SSH 事件 handler、日志与二次认证挑战注册。
#[allow(clippy::too_many_arguments)]
pub async fn connect_via_ssh(
    ssh_session_config: &Session,
    resolved_credential: ResolvedCredential,
    mysql_host: &str,
    mysql_port: u16,
    mysql_user: &str,
    mysql_pass: &str,
    mysql_db: Option<&str>,
    state: AppState,
) -> AppResult<MySqlConn> {
    // 1+2. 建立 SSH 连接并在本地随机端口起桥接 listener。
    let (tunnel_handle, local_port) =
        super::open_ssh_tunnel(ssh_session_config, resolved_credential, mysql_host, mysql_port, state)
            .await?;

    // 3. sqlx 连本地端口。
    let url = build_mysql_url("127.0.0.1", local_port, mysql_user, mysql_pass, mysql_db);
    // SSH 隧道下限制 pool 大小，避免开过多 channel。
    let pool = match MySqlPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(MYSQL_CONNECT_TIMEOUT)
        .connect(&url)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            // 连接失败：必须 abort 隧道 accept 循环并断开 SSH，否则 accept 任务
            // 带着监听端口与 SSH handle 一直泄漏到进程退出。
            tunnel_handle.abort();
            log::warn!(
                "[connect_via_ssh] 经隧道连接 MySQL 失败 ({}:{}): {}",
                mysql_host,
                mysql_port,
                e
            );
            return Err(AppError::Storage(format!(
                "经 SSH 隧道连接 MySQL 失败（{}:{}，15 秒超时）：{e}",
                mysql_host, mysql_port
            )));
        }
    };

    Ok(MySqlConn {
        pool,
        _tunnel_handle: Some(tunnel_handle),
        current_db: parking_lot::Mutex::new(mysql_db.map(|s| s.to_string())),
    })
}

// ===========================================================================
// 辅助
// ===========================================================================

/// 判断 SQL 是否为"返回结果集"的语句（需要走 `fetch_all`）。
///
/// 取 SQL 去掉前导空白与注释后的首个关键字，不区分大小写。
fn is_query_stmt(sql: &str) -> bool {
    let trimmed = sql.trim_start();
    // 取第一个空白前的 token。
    let first = trimmed
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim_end_matches('(');
    let upper = first.to_uppercase();
    matches!(
        upper.as_str(),
        "SELECT" | "SHOW" | "EXPLAIN" | "DESC" | "DESCRIBE" | "WITH"
    )
}

/// 把一个 cell 转为字符串。
///
/// sqlx 的 `String` 解码只兼容少量文本类型（VARCHAR/TEXT/CHAR/ENUM/BLOB 等），
/// TIME/DATE/DATETIME/TIMESTAMP/JSON/DECIMAL 等列直接 `try_get<String>` 会因
/// 类型不兼容而报错。这里按列类型逐级尝试解码，全部失败才回退 `"<binary>"`。
fn cell_to_string(row: &MySqlRow, idx: usize) -> String {
    // 文本类：VARCHAR/TEXT/CHAR/ENUM/BLOB 等。
    match row.try_get::<Option<String>, _>(idx) {
        Ok(Some(s)) => return s,
        // NULL 值：任何列都能以 Option 形式解出 None，直接返回空串。
        Ok(None) => return String::new(),
        Err(_) => {}
    }
    // 数值类：INT/BIGINT/FLOAT/DOUBLE；u64 额外覆盖 YEAR、BIT。
    if let Ok(v) = row.try_get::<Option<i64>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<u64>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    // 日期时间类：DATETIME/DATE/TIME/TIMESTAMP。
    if let Ok(v) = row.try_get::<Option<NaiveDateTime>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<DateTime<Utc>>, _>(idx) {
        // TIMESTAMP 仅 NaiveDateTime 不兼容，落到这里；naive 即服务器返回的原始值。
        return v.map(|x| x.naive_utc().to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<NaiveDate>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    // TIME：先试标准 00:00:00 形式；负数/超 24h 的间隔值回退 MySqlTime。
    if let Ok(v) = row.try_get::<Option<NaiveTime>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<MySqlTime>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    // JSON：以紧凑 JSON 文本展示。
    if let Ok(v) = row.try_get::<Option<JsonValue>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    // DECIMAL：二进制协议下以十进制数字字符串传输。
    if let Ok(v) = row.try_get::<Option<Decimal>, _>(idx) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    // 二进制兜底：BLOB 等按 UTF-8 lossy 解码。
    if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(idx) {
        return v
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
    }
    "<binary>".to_string()
}

/// 构造 `mysql://user:pass@host:port/db` URL，密码做百分号编码
/// （见 [`super::url_encode_component`]）。
fn build_mysql_url(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    database: Option<&str>,
) -> String {
    let mut url = String::from("mysql://");
    url.push_str(&url_encode_component(username));
    url.push(':');
    url.push_str(&url_encode_component(password));
    url.push('@');
    url.push_str(host);
    url.push(':');
    url.push_str(&port.to_string());
    url.push('/');
    url.push_str(database.unwrap_or(""));
    url
}
