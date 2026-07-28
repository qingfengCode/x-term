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

use serde::{Deserialize, Serialize};
use sqlx::mysql::{MySqlPoolOptions, MySqlRow};
use sqlx::{Column, MySqlPool, Row};
use std::sync::Arc;
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;

use russh::client::Handle;

use crate::error::{AppError, AppResult};
use crate::ssh::client::ClientHandler;
use crate::ssh::session::ResolvedCredential;
use crate::storage::db::DbConn;
use crate::storage::secure::CredentialVault;
use crate::storage::sessions_repo::Session;

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

// ===========================================================================
// 数据结构
// ===========================================================================

/// 一条已建立的 MySQL 连接（实际是一个 pool）。
///
/// `_tunnel_handle` 在直连时为 `None`；SSH 隧道模式下保存本地 listener 的
/// accept 循环任务句柄，[`MySqlConn::close`] 时 abort 以释放端口与 SSH channel。
pub struct MySqlConn {
    pub pool: MySqlPool,
    /// SSH 隧道模式下保存 accept 循环句柄；drop 时 abort。
    _tunnel_handle: Option<tokio::task::JoinHandle<()>>,
}

impl MySqlConn {
    /// 执行一条 SQL，返回 [`QueryResult`]。
    ///
    /// 对于返回结果集的语句（SELECT/SHOW/EXPLAIN/DESC/WITH 等）走 `fetch_all`，
    /// 仅保留前 `limit` 行；其余语句走 `execute` 取影响行数。
    pub async fn execute(&self, sql: &str, limit: u32) -> AppResult<QueryResult> {
        if is_query_stmt(sql) {
            let rows: Vec<MySqlRow> = sqlx::query(sql).fetch_all(&self.pool).await?;
            // 列名：从第一行取；若无行则无法拿到列（MySQL 在 0 行时 columns 为空）。
            // 实际上 fetch_all 返回的 Vec<MySqlRow> 每行都有 columns()，但为空时只能
            // 尝试从第一行；取不到列就给空数组。
            let columns: Vec<String> = rows
                .first()
                .map(|r| {
                    r.columns()
                        .iter()
                        .map(|c| c.name().to_string())
                        .collect()
                })
                .unwrap_or_default();

            let n = rows.len();
            let mut out_rows: Vec<Vec<String>> = Vec::with_capacity(n.min(limit as usize));
            for (i, row) in rows.iter().enumerate() {
                if i as u32 >= limit {
                    break;
                }
                let mut vals: Vec<String> = Vec::with_capacity(row.columns().len());
                for idx in 0..row.columns().len() {
                    vals.push(cell_to_string(row, idx));
                }
                out_rows.push(vals);
            }
            Ok(QueryResult {
                columns,
                rows: out_rows,
                affected: n as u64,
            })
        } else {
            let res = sqlx::query(sql).execute(&self.pool).await?;
            Ok(QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                affected: res.rows_affected(),
            })
        }
    }

    /// 关闭连接：先关闭 pool，再 abort 隧道 accept 循环（如有）。
    pub async fn close(self) {
        self.pool.close().await;
        if let Some(h) = self._tunnel_handle {
            h.abort();
        }
    }
}

/// 查询结果（命令返回 / 事件 payload 共用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    pub columns: Vec<String>,
    /// 每行每列的值已 `to_string`；BLOB 等无法 decode 为 String 的列填 `"<binary>"`。
    pub rows: Vec<Vec<String>>,
    /// 非 SELECT 语句的影响行数（SELECT 为行数）。
    pub affected: u64,
}

// ===========================================================================
// 连接
// ===========================================================================

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
        .connect(&url)
        .await?;
    Ok(MySqlConn {
        pool,
        _tunnel_handle: None,
    })
}

/// 通过 SSH 隧道连 MySQL。
///
/// 流程：
/// 1. 用 `ssh_session_config` 建立 SSH 连接（凭据由 `resolved_credential` 提供）。
/// 2. 在 `127.0.0.1:0` 起本地 listener，对每条入站 TCP 连接开一个新的
///    `channel_open_direct_tcpip(mysql_host, mysql_port)`，spawn 双向桥接。
/// 3. sqlx 连本地 listener 的随机端口。
///
/// `app` 用于 SSH 事件 handler 与日志。
#[allow(clippy::too_many_arguments)]
pub async fn connect_via_ssh(
    ssh_session_config: &Session,
    resolved_credential: ResolvedCredential,
    mysql_host: &str,
    mysql_port: u16,
    mysql_user: &str,
    mysql_pass: &str,
    mysql_db: Option<&str>,
    app: tauri::AppHandle,
) -> AppResult<MySqlConn> {
    // 1. 建立 SSH 连接。
    let handle = crate::ssh::client::connect_direct(
        &ssh_session_config.host,
        ssh_session_config.port,
        &ssh_session_config.username,
        resolved_credential.auth_method,
        app,
    )
    .await?;

    // 2. 本地随机端口 listener。
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| AppError::Ssh(format!("绑定本地隧道监听失败: {}", e)))?;
    let local_port = listener
        .local_addr()
        .map_err(|e| AppError::Ssh(format!("获取本地端口失败: {}", e)))?
        .port();

    // handle 未实现 Clone，包成 Arc，accept 循环里每条入站连接克隆一份。
    let handle_arc: Arc<Handle<ClientHandler>> = Arc::new(handle);
    let remote_host = mysql_host.to_string();
    let remote_port = mysql_port as u32;

    let tunnel_handle = tokio::spawn(async move {
        loop {
            let accept = listener.accept().await;
            let (mut tcp, peer) = match accept {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("MySQL 隧道 accept 失败: {}", e);
                    continue;
                }
            };

            let handle = handle_arc.clone();
            let remote_host = remote_host.clone();
            tokio::spawn(async move {
                let origin_host = peer.ip().to_string();
                let origin_port = peer.port() as u32;

                let channel = match handle
                    .channel_open_direct_tcpip(
                        remote_host.clone(),
                        remote_port,
                        origin_host,
                        origin_port,
                    )
                    .await
                {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("MySQL 隧道 channel_open_direct_tcpip 失败: {}", e);
                        return;
                    }
                };

                let mut stream = channel.into_stream();
                if let Err(e) = copy_bidirectional(&mut tcp, &mut stream).await {
                    log::warn!("MySQL 隧道桥接出错: {}", e);
                }
            });
        }
    });

    // 3. sqlx 连本地端口。
    let url = build_mysql_url(
        "127.0.0.1",
        local_port,
        mysql_user,
        mysql_pass,
        mysql_db,
    );
    // SSH 隧道下限制 pool 大小，避免开过多 channel。
    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await?;

    Ok(MySqlConn {
        pool,
        _tunnel_handle: Some(tunnel_handle),
    })
}

// ===========================================================================
// 凭据解析
// ===========================================================================

/// `credentials.enc_data` 解密后的明文 JSON 结构（mysql_password 用）。
#[derive(Debug, Deserialize)]
struct MysqlCredentialData {
    kind: String,
    value: String,
}

/// 从 `credentials` 表取出指定 id 的加密 blob，解密并解析为 MySQL 密码。
///
/// 约定凭据 JSON 形如 `{"kind":"mysql_password","value":"<密码>"}`。
pub fn fetch_mysql_password(
    conn: &DbConn,
    cred_id: &str,
    vault: &CredentialVault,
) -> AppResult<String> {
    let enc_data: String = match conn.query_row(
        "SELECT enc_data FROM credentials WHERE id = ?1",
        rusqlite::params![cred_id],
        |r| r.get::<_, String>(0),
    ) {
        Ok(s) => s,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(AppError::NotFound(format!("凭据 {} 不存在", cred_id)));
        }
        Err(e) => return Err(e.into()),
    };
    let blob = CredentialVault::decode_blob(&enc_data)?;
    let plain = vault.decrypt_str(&blob)?;
    let data: MysqlCredentialData = serde_json::from_str(&plain)?;
    if data.kind != "mysql_password" {
        return Err(AppError::Auth(format!(
            "凭据类型不匹配：期望 mysql_password，实际 {}",
            data.kind
        )));
    }
    Ok(data.value)
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
/// 优先尝试 `Option<String>` decode（覆盖大多数 MySQL 类型：INT/VARCHAR/
/// TEXT/DATETIME/DECIMAL 等）；失败则回退 `"<binary>"`（如 BLOB）。
fn cell_to_string(row: &MySqlRow, idx: usize) -> String {
    match row.try_get::<Option<String>, _>(idx) {
        Ok(Some(s)) => s,
        Ok(None) => String::new(),
        Err(_) => {
            // 尝试常见非 String 类型：i64/u64/f64/bool/Vec<u8>。
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
            if let Ok(v) = row.try_get::<Option<Vec<u8>>, _>(idx) {
                return v
                    .map(|bytes| {
                        String::from_utf8_lossy(&bytes).into_owned()
                    })
                    .unwrap_or_default();
            }
            "<binary>".to_string()
        }
    }
}

/// 构造 `mysql://user:pass@host:port/db` URL，密码做百分号编码。
fn build_mysql_url(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    database: Option<&str>,
) -> String {
    let mut url = String::from("mysql://");
    url.push_str(&url_encode(username));
    url.push(':');
    url.push_str(&url_encode_password(password));
    url.push('@');
    url.push_str(host);
    url.push(':');
    url.push_str(&port.to_string());
    url.push('/');
    url.push_str(database.unwrap_or(""));
    url
}

/// 对密码做最小百分号编码：把 URI 保留/不安全字符转义。
///
/// 项目未引入 `percent-encoding` crate，这里手工处理一组常见字符即可满足
/// MySQL 密码 URL 编码需求。未被列入的可打印 ASCII 原样保留。
fn url_encode_password(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'@' | b':' | b'#' | b'?' | b'/' | b'%' | b' ' | b'&' | b'+' | b'=' => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
            // 非可打印 ASCII 或 > 127：UTF-8 字节按需编码。
            0x00..=0x1F | 0x7F..=0xFF => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
            _ => out.push(b as char),
        }
    }
    out
}

/// 通用 URL 用户名编码（用户名一般不含特殊字符，复用同一套规则）。
fn url_encode(s: &str) -> String {
    url_encode_password(s)
}
