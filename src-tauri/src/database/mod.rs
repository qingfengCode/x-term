//! 业务数据库（MySQL / PostgreSQL）连接管理。
//!
//! 注意：与 [`crate::storage::db`]（SQLite，存配置）区分。本模块管理对用户
//! 数据库服务的运行时连接，供 SQL 控制台和 AI 工具使用。
//!
//! - [`mysql`] / [`postgres`]：各方言的连接与执行实现。
//! - [`DbConnHandle`]：统一句柄——按 profile 的 kind 包装具体连接，
//!   命令层 / AI 工具 / MCP 通过它无差别地执行 SQL 与元数据查询。
pub mod mysql;
pub mod postgres;
pub mod profiles;
pub mod script_split;
pub mod sqlite;

use std::sync::Arc;

use russh::client::Handle;
use serde::{Deserialize, Serialize};
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;

use crate::error::{AppError, AppResult};
use crate::ssh::client::ClientHandler;
use crate::ssh::session::ResolvedCredential;
use crate::state::AppState;
use crate::storage::db::DbConn;
use crate::storage::secure::CredentialVault;
use crate::storage::sessions_repo::Session;

pub use mysql::MySqlConn;
pub use postgres::PgConn;
pub use sqlite::SqliteConn;

/// 查询结果（命令返回 / 事件 payload 共用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    pub columns: Vec<String>,
    /// 每行每列的值已 `to_string`；BLOB 等无法 decode 为 String 的列填 `"<binary>"`。
    pub rows: Vec<Vec<String>>,
    /// 非 SELECT 语句的影响行数（SELECT 为返回的行数）。
    pub affected: u64,
    /// 结果是否被 limit 截断（查询实际返回超过 limit 行时置 true）。
    ///
    /// `serde(default)` 保证旧前端/旧数据缺字段也能解析。
    #[serde(default)]
    pub truncated: bool,
}

// ===========================================================================
// 能力开关（借鉴 uniTerm DBCapabilities，强类型化）
// ===========================================================================

/// 方言能力描述：前端按它显隐 UI（结构页的注释列、自增按钮、建库入口等），
/// 是多厂商差异的**唯一**前端感知点——前端不写 `if kind ===`。
///
/// 只下发不回传（前端只读），仅 derive Serialize。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbCapabilities {
    /// 列是否支持 COMMENT。
    pub supports_comment: bool,
    /// 是否支持自增列。
    pub supports_auto_increment: bool,
    /// 是否支持 CREATE DATABASE（SQLite 单文件无库概念）。
    pub supports_create_database: bool,
    /// 是否支持多库浏览（同一连接列出并切换多个数据库）。
    pub multiple_databases: bool,
    /// 建表/改列 UI 的类型建议列表。
    pub column_types: &'static [&'static str],
}

/// MySQL 能力（基线）。
pub const MYSQL_CAPS: DbCapabilities = DbCapabilities {
    supports_comment: true,
    supports_auto_increment: true,
    supports_create_database: true,
    multiple_databases: true,
    column_types: &[
        "INT", "BIGINT", "VARCHAR(255)", "TEXT", "DATETIME", "TIMESTAMP", "DECIMAL(10,2)",
        "DOUBLE", "BOOLEAN", "JSON",
    ],
};

/// PostgreSQL 能力。
pub const POSTGRES_CAPS: DbCapabilities = DbCapabilities {
    supports_comment: true,
    supports_auto_increment: true, // SERIAL/IDENTITY
    supports_create_database: true,
    multiple_databases: false, // 连接绑定单库，切库需重连
    column_types: &[
        "INTEGER", "BIGINT", "SERIAL", "TEXT", "VARCHAR(255)", "TIMESTAMPTZ", "NUMERIC(10,2)",
        "DOUBLE PRECISION", "BOOLEAN", "JSONB", "UUID",
    ],
};

/// SQLite 能力。
pub const SQLITE_CAPS: DbCapabilities = DbCapabilities {
    supports_comment: false,
    supports_auto_increment: false, // INTEGER PRIMARY KEY 隐式 rowid，无独立语法
    supports_create_database: false,
    multiple_databases: false,
    column_types: &["INTEGER", "REAL", "TEXT", "BLOB", "NUMERIC", "BOOLEAN", "DATETIME"],
};

// ===========================================================================
// 标识符校验与引用（统一入口，防各方言实现漏检）
// ===========================================================================

/// 标识符统一校验：非空、≤128 字节、无 NUL/控制字符、不含 `..`、`/`、`\`
/// （防路径穿越与截断攻击；比各方言历史上的"仅字母数字下划线"更宽——允许
/// 合法的中文/点号表名，由引用转义兜底）。
pub fn validate_ident(name: &str) -> AppResult<()> {
    if name.is_empty() || name.len() > 128 {
        return Err(AppError::InvalidInput(format!("非法标识符（长度）: {name}")));
    }
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        return Err(AppError::InvalidInput(format!("非法标识符（控制符/路径符）: {name}")));
    }
    if name.contains("..") {
        return Err(AppError::InvalidInput(format!("非法标识符（..）: {name}")));
    }
    Ok(())
}

/// 标识符引用：先校验（[`validate_ident`]），再用给定引号字符包裹并转义
/// 内部同款引号（加倍）。MySQL/SQLite 反引号、PG 双引号、SQL Server 方括号。
pub fn quote_ident(name: &str, quote: char) -> AppResult<String> {
    validate_ident(name)?;
    let doubled = match quote {
        '`' => name.replace('`', "``"),
        '"' => name.replace('"', "\"\""),
        '\'' => name.replace('\'', "''"),
        _ => name.to_string(),
    };
    Ok(format!("{quote}{doubled}{quote}"))
}

// ===========================================================================
// 统一连接句柄
// ===========================================================================

/// 统一的业务数据库连接句柄：命令层 / AI 工具 / MCP 持有它执行 SQL，
/// 内部按 profile.kind 落到 MySQL 或 PostgreSQL 实现。
///
/// 用 enum 而非 trait 对象：方法集合小且稳定，enum 分发零开销、无
/// async-trait 装箱。内部是 `Arc`，克隆只复制引用。
#[derive(Clone)]
pub enum DbConnHandle {
    MySql(Arc<MySqlConn>),
    Postgres(Arc<PgConn>),
    Sqlite(Arc<SqliteConn>),
}

impl DbConnHandle {
    /// 数据库类型标识（与 profile.kind 归一化后的值一致）。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::MySql(_) => "mysql",
            Self::Postgres(_) => "postgres",
            Self::Sqlite(_) => "sqlite",
        }
    }

    /// 方言能力（前端 UI 显隐的单一事实来源）。
    pub fn capabilities(&self) -> &'static DbCapabilities {
        match self {
            Self::MySql(_) => &MYSQL_CAPS,
            Self::Postgres(_) => &POSTGRES_CAPS,
            Self::Sqlite(_) => &SQLITE_CAPS,
        }
    }

    /// 生成分页浏览 SQL（"浏览模式"：只生成文本不执行，前端把它放进编辑器
    /// 执行；翻页重新生成——方言差异的收敛点，借鉴 uniTerm DefaultTableQuery）。
    ///
    /// MySQL/PG/SQLite：`LIMIT n [OFFSET m]`；未来 SQL Server 走
    /// `ORDER BY (SELECT NULL) OFFSET ... FETCH`。
    pub fn default_table_query(&self, table: &str, limit: u32, offset: u32) -> AppResult<String> {
        let limit = limit.clamp(1, 1000);
        // 限定名（db.table / schema.table）必须按点拆开**分段引用**——整体包进
        // 一对引号会被当成单个列名（老实现踩过的坑：MySQL `db.table` 整体反
        // 引号 → No database selected）。
        let quote_char = match self {
            Self::Postgres(_) => '"',
            _ => '`',
        };
        let parts: Vec<&str> = table.split('.').collect();
        if parts.is_empty() || parts.len() > 2 {
            return Err(AppError::InvalidInput(format!("非法表标识符: {table}")));
        }
        let mut q = Vec::with_capacity(parts.len());
        for p in parts {
            q.push(quote_ident(p, quote_char)?);
        }
        let quoted = q.join(".");
        let offset_clause = if offset > 0 {
            format!(" OFFSET {offset}")
        } else {
            String::new()
        };
        Ok(format!(
            "SELECT * FROM {quoted} LIMIT {limit}{offset_clause};"
        ))
    }

    /// 当前库（schema）。
    pub fn current_db(&self) -> Option<String> {
        match self {
            Self::MySql(c) => c.current_db(),
            Self::Postgres(c) => c.current_db(),
            // SQLite 单文件无库概念，用文件名展示。
            Self::Sqlite(c) => Some(
                std::path::Path::new(&c.path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "main".into()),
            ),
        }
    }

    /// 执行一条 SQL。
    ///
    /// MySQL 自动带上连接的当前库（每条查询前 `USE \`db\``，pool 语义下
    /// 保证落在当前库）；PG 的库在连接层已确定（切库 = 重连）。
    pub async fn execute(&self, sql: &str, limit: u32) -> AppResult<QueryResult> {
        match self {
            Self::MySql(c) => {
                let db = c.current_db();
                c.execute(sql, limit, db.as_deref()).await
            }
            Self::Postgres(c) => c.execute(sql, limit).await,
            Self::Sqlite(c) => c.execute(sql, limit).await,
        }
    }

    /// 切换当前库：MySQL 仅记录 current_db（查询时自动 USE）；
    /// PostgreSQL 换库重连（pool 整体替换，失败保留旧连接）；
    /// SQLite 无库概念（幂等成功）。
    pub async fn use_database(&self, db: Option<&str>) -> AppResult<()> {
        match self {
            Self::MySql(c) => {
                if let Some(d) = db {
                    mysql::validate_database_identifier(d)?;
                }
                c.set_current_db(db.map(|s| s.to_string()));
                Ok(())
            }
            Self::Postgres(c) => c.use_database(db).await,
            Self::Sqlite(_) => Ok(()),
        }
    }

    /// 关闭连接（pool + 隧道）。
    pub async fn close(&self) {
        match self {
            Self::MySql(c) => c.close().await,
            Self::Postgres(c) => c.close().await,
            Self::Sqlite(c) => c.close().await,
        }
    }

    // -----------------------------------------------------------------------
    // 元数据（命令层与 AI 工具共用，按方言生成 SQL）
    // -----------------------------------------------------------------------

    /// 列出服务器上所有可访问的数据库。
    pub async fn list_databases(&self) -> AppResult<Vec<String>> {
        match self {
            // SQLite 单文件即一个"库"，用文件名（去扩展名）展示。
            Self::Sqlite(_) => Ok(vec![self
                .current_db()
                .unwrap_or_else(|| "main".into())
                .trim_end_matches(".db")
                .trim_end_matches(".sqlite")
                .to_string()]),
            _ => {
                let sql = match self {
                    Self::MySql(_) => "SHOW DATABASES",
                    Self::Postgres(_) => "SELECT datname FROM pg_database \
                                          WHERE datallowconn AND NOT datistemplate ORDER BY 1",
                    Self::Sqlite(_) => unreachable!(),
                };
                let qr = self.execute(sql, 1_000).await?;
                Ok(qr.rows.into_iter().filter_map(|mut r| r.pop()).collect())
            }
        }
    }

    /// 列出表。
    ///
    /// MySQL：`SHOW TABLES [FROM db]`；PG：information_schema（表名以
    /// `schema.table` 返回，非当前库时临时连目标库查询）；SQLite：sqlite_master。
    pub async fn list_tables(&self, database: Option<&str>) -> AppResult<Vec<String>> {
        match self {
            Self::MySql(_) => {
                let sql = match database {
                    Some(db) => {
                        mysql::validate_database_identifier(db)?;
                        format!("SHOW TABLES FROM `{}`", db)
                    }
                    None => "SHOW TABLES".into(),
                };
                let qr = self.execute(&sql, 10_000).await?;
                Ok(qr.rows.into_iter().filter_map(|mut r| r.pop()).collect())
            }
            Self::Postgres(c) => c.list_tables(database, 10_000).await,
            Self::Sqlite(c) => c.list_tables().await,
        }
    }

    /// 表结构（MySQL `DESCRIBE`；PG information_schema，列名对齐
    /// Field/Type/Null/Default，便于前端与 AI 上下文统一消费）。
    ///
    /// `table` 支持 `db.table`（PG 为 `schema.table`）限定名或仅 `table`。
    pub async fn describe_table(&self, table: &str) -> AppResult<QueryResult> {
        match self {
            Self::MySql(_) => {
                let qualified = mysql::qualify_table_identifier(table)?;
                let sql = format!("DESCRIBE {qualified}");
                self.execute(&sql, 1000).await
            }
            Self::Postgres(_) => {
                let (schema_sql, table_lit) = pg_split_qualified(table)?;
                let sql = format!(
                    "SELECT column_name AS \"Field\", data_type AS \"Type\", \
                     is_nullable AS \"Null\", column_default AS \"Default\" \
                     FROM information_schema.columns \
                     WHERE lower(table_name) = lower('{table_lit}'){schema_sql} \
                     ORDER BY ordinal_position"
                );
                self.execute(&sql, 1000).await
            }
            Self::Sqlite(c) => c.describe_table(table).await,
        }
    }

    /// 获取建表 DDL（AI 拖表附加表结构上下文）。
    ///
    /// MySQL `SHOW CREATE TABLE`；PG 无对应语句，按 pg_catalog 生成近似
    /// DDL（列定义 + 主键，不含索引/外键/注释）；SQLite 读 sqlite_master 原文。
    pub async fn table_ddl(&self, database: Option<&str>, table: &str) -> AppResult<String> {
        match self {
            Self::Sqlite(c) => c.table_ddl(table).await,
            Self::MySql(_) => {
                // 严格的标识符白名单校验（与旧 db_show_create_table 一致）。
                let ident_ok = |s: &str| {
                    !s.is_empty()
                        && s.chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$'))
                };
                if !ident_ok(table) {
                    return Err(AppError::InvalidInput(format!("非法表名: {table}")));
                }
                if let Some(db) = database {
                    if !ident_ok(db) {
                        return Err(AppError::InvalidInput(format!("非法库名: {db}")));
                    }
                }
                let qualified = match database {
                    Some(db) => format!("`{}`.`{}`", db, table),
                    None => format!("`{}`", table),
                };
                let sql = format!("SHOW CREATE TABLE {qualified}");
                let qr = self.execute(&sql, 1).await?;
                if qr.rows.is_empty() || qr.rows[0].len() < 2 {
                    return Ok(format!("-- 无法获取 {qualified} 的建表语句"));
                }
                Ok(qr.rows[0][1].clone())
            }
            Self::Postgres(_) => {
                let (schema_sql, table_lit) = pg_split_qualified(table)?;
                let _ = database; // PG 的表名自带 schema 前缀；database 由连接决定。
                // schema_sql 里的 table_schema 列只存在于 information_schema，
                // pg_catalog 侧需改用 pg_namespace.nspname 等价限定。
                let ns_sql = schema_sql.replace("table_schema", "n.nspname");
                // 列定义（format_type 带长度，如 varchar(255)）。
                let col_sql = format!(
                    "SELECT a.attname, format_type(a.atttypid, a.atttypmod) AS typ, \
                     a.attnotnull, pg_get_expr(ad.adbin, ad.adrelid) AS dflt \
                     FROM pg_attribute a \
                     LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
                     WHERE a.attrelid = ( \
                          SELECT c.oid FROM pg_class c \
                          JOIN pg_namespace n ON n.oid = c.relnamespace \
                          WHERE lower(c.relname) = lower('{table_lit}'){ns_sql} \
                          LIMIT 1) \
                       AND a.attnum > 0 AND NOT a.attisdropped \
                     ORDER BY a.attnum"
                );
                // 主键列（按键内顺序）。
                let pk_sql = format!(
                    "SELECT kcu.column_name \
                     FROM information_schema.table_constraints tc \
                     JOIN information_schema.key_column_usage kcu \
                       ON tc.constraint_name = kcu.constraint_name \
                      AND tc.table_schema = kcu.table_schema \
                     WHERE tc.constraint_type = 'PRIMARY KEY' \
                       AND lower(tc.table_name) = lower('{table_lit}'){schema_sql} \
                     ORDER BY kcu.ordinal_position"
                );
                let cols = self.execute(&col_sql, 1000).await?;
                if cols.rows.is_empty() {
                    return Ok(format!("-- 无法获取 {table_lit} 的建表语句（表不存在）"));
                }
                let pks = self.execute(&pk_sql, 100).await?;
                let mut lines: Vec<String> = Vec::with_capacity(cols.rows.len() + 1);
                for r in &cols.rows {
                    // r = [列名, 类型, notnull("t"/"f"), 默认值(可能空)]。
                    let name = r.first().cloned().unwrap_or_default();
                    let typ = r.get(1).cloned().unwrap_or_default();
                    let not_null = r.get(2).map(|v| v == "t").unwrap_or(false);
                    let dflt = r.get(3).cloned().unwrap_or_default();
                    let mut line = format!("  {} {}", quote_pg_ident(&name), typ);
                    if not_null {
                        line.push_str(" NOT NULL");
                    }
                    if !dflt.is_empty() {
                        line.push_str(&format!(" DEFAULT {dflt}"));
                    }
                    lines.push(line);
                }
                if !pks.rows.is_empty() {
                    let cols_txt: Vec<String> = pks
                        .rows
                        .iter()
                        .map(|r| quote_pg_ident(r.first().map(String::as_str).unwrap_or("")))
                        .collect();
                    lines.push(format!("  PRIMARY KEY ({})", cols_txt.join(", ")));
                }
                Ok(format!(
                    "CREATE TABLE {} (\n{}\n);\n-- 由 pg_catalog 生成，不含索引/外键/注释",
                    quote_pg_ident(table),
                    lines.join(",\n")
                ))
            }
        }
    }
}

// ===========================================================================
// kind / 标识符辅助
// ===========================================================================

/// profile.kind 归一化：`postgres`/`postgresql`/`pg` → `postgres`；
/// `sqlite`/`sqlite3` → `sqlite`；其余（含历史空值）按 `mysql` 处理。
pub fn normalize_kind(kind: &str) -> &'static str {
    match kind.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" | "pg" => "postgres",
        "sqlite" | "sqlite3" => "sqlite",
        _ => "mysql",
    }
}

/// 把 PG 表标识符（`table` 或 `schema.table`）拆为
/// （schema 过滤 SQL 片段, 表名字面量）。
///
/// 每段只允许 `[A-Za-z0-9_]`（与 MySQL 侧白名单一致），防止拼进
/// information_schema / `::regclass` 的字面量被注入。未限定 schema 时
/// 限定在 search_path 内（`current_schemas(false)`），避免命中系统表。
fn pg_split_qualified(table: &str) -> AppResult<(String, String)> {
    let parts: Vec<&str> = table.split('.').collect();
    if parts.is_empty() || parts.len() > 2 {
        return Err(AppError::InvalidInput(format!("非法表标识符: {table}")));
    }
    for p in &parts {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(AppError::InvalidInput(format!("非法表标识符: {table}")));
        }
    }
    Ok(match parts.len() {
        1 => (
            " AND table_schema = ANY (current_schemas(false))".to_string(),
            parts[0].to_string(),
        ),
        _ => (
            format!(" AND lower(table_schema) = lower('{}')", parts[0]),
            parts[1].to_string(),
        ),
    })
}

/// PG 标识符双引号包裹（内部双引号转义，防标识符本身含 `"`）。
fn quote_pg_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

// ===========================================================================
// 凭据解析
// ===========================================================================

/// `credentials.enc_data` 解密后的明文 JSON 结构（数据库密码用）。
#[derive(Debug, Deserialize)]
struct DbCredentialData {
    kind: String,
    value: String,
}

/// 数据库密码凭据的合法 kind 集合。
/// - `mysql_password`：历史值（多数据库支持前全部 profile）；
/// - `db_password`：统一值（MySQL / PostgreSQL 共用）。
const DB_CRED_KINDS: [&str; 2] = ["mysql_password", "db_password"];

/// 从 `credentials` 表取出指定 id 的加密 blob，解密并解析为数据库密码。
///
/// 约定凭据 JSON 形如 `{"kind":"db_password","value":"<密码>"}`。
pub fn fetch_db_password(
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
            return Err(AppError::NotFound(format!("凭据 {cred_id} 不存在")));
        }
        Err(e) => return Err(e.into()),
    };
    let blob = CredentialVault::decode_blob(&enc_data)?;
    let plain = vault.decrypt_str(&blob)?;
    let data: DbCredentialData = serde_json::from_str(&plain)?;
    if !DB_CRED_KINDS.contains(&data.kind.as_str()) {
        return Err(AppError::Auth(format!(
            "凭据类型不匹配：期望 {:?}，实际 {}",
            DB_CRED_KINDS,
            data.kind
        )));
    }
    Ok(data.value)
}

// ===========================================================================
// SSH 隧道（MySQL / PostgreSQL 共用）
// ===========================================================================

/// 建立到跳板机的 SSH 连接，并在本地随机端口起桥接 listener。
///
/// russh 的 channel 不是网络地址，sqlx 不能直接用：在 `127.0.0.1:0` 起
/// `TcpListener`，对每条入站 TCP 连接开一个新的 `channel_open_direct_tcpip`，
/// spawn `copy_bidirectional` 双向桥接。返回 (accept 循环句柄, 本地端口)，
/// 调用方连接本地端口即可透明走 SSH 隧道；连接关闭时 abort 句柄。
pub(crate) async fn open_ssh_tunnel(
    ssh_session_config: &Session,
    resolved_credential: ResolvedCredential,
    remote_host: &str,
    remote_port: u16,
    state: AppState,
) -> AppResult<(tokio::task::JoinHandle<()>, u16)> {
    // 1. 建立 SSH 连接。
    let handle = crate::ssh::client::connect_direct(
        &ssh_session_config.host,
        ssh_session_config.port,
        &ssh_session_config.username,
        &ssh_session_config.id,
        resolved_credential.auth_method,
        state,
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
    let remote_host = remote_host.to_string();
    let remote_port = remote_port as u32;

    let tunnel_handle = tokio::spawn(async move {
        loop {
            let accept = listener.accept().await;
            let (mut tcp, peer) = match accept {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("数据库隧道 accept 失败: {}", e);
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
                        log::warn!("数据库隧道 channel_open_direct_tcpip 失败: {}", e);
                        return;
                    }
                };

                let mut stream = channel.into_stream();
                if let Err(e) = copy_bidirectional(&mut tcp, &mut stream).await {
                    log::warn!("数据库隧道桥接出错: {}", e);
                }
            });
        }
    });

    Ok((tunnel_handle, local_port))
}

// ===========================================================================
// URL 编码（MySQL / PostgreSQL 共用）
// ===========================================================================

/// 对 URL 组件（用户名/密码/库名）做最小百分号编码。
///
/// 项目未引入 `percent-encoding` crate，这里手工处理一组常见字符即可。
/// 未列入的可打印 ASCII 原样保留。
pub(crate) fn url_encode_component(s: &str) -> String {
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
