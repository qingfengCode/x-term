//! SQLite 数据库连接池与迁移。
//!
//! 使用 `r2d2` + `r2d2_sqlite` 在应用启动时建立连接池，所有命令处理器通过
//! `&DbConn`（即 `PooledConnection<SqliteConnectionManager>`）访问数据库。
//!
//! 连接初始化时启用：
//! - WAL 模式：提高并发读写性能；
//! - `busy_timeout = 5000ms`：在锁竞争时短暂等待而非立即报错。

use std::path::Path;

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

use crate::error::AppResult;

/// SQLite 连接池类型别名。
pub type DbPool = Pool<SqliteConnectionManager>;

/// 从池中取出的连接（`&mut Connection` 的来源）。
pub type DbConn = r2d2::PooledConnection<SqliteConnectionManager>;

/// 数据库文件名。
const DB_FILENAME: &str = "xterm.db";

/// 初始化数据库连接池。
///
/// 在 `app_data_dir` 下创建/打开 `xterm.db`，对每个新连接执行 PRAGMA 设置
/// （WAL、busy_timeout），并运行一次数据库迁移（建表）。
///
/// # 参数
/// - `app_data_dir`: 应用数据目录（由 [`crate::storage::json_store::app_data_dir`] 提供）。
///
/// # 返回
/// 配置好的连接池。
pub fn init_pool(app_data_dir: &Path) -> AppResult<DbPool> {
    // 确保目录存在。
    std::fs::create_dir_all(app_data_dir)?;

    let db_path = app_data_dir.join(DB_FILENAME);
    let manager = SqliteConnectionManager::file(&db_path).with_init(|c| {
        // 每个新连接都执行 PRAGMA。
        c.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA synchronous = NORMAL;\
             PRAGMA busy_timeout = 5000;\
             PRAGMA foreign_keys = ON;",
        )
    });

    let pool = Pool::builder()
        .build(manager)
        .map_err(|e| crate::error::AppError::Storage(format!("无法建立数据库连接池: {}", e)))?;

    // 对首个连接运行迁移。
    {
        let conn = pool
            .get()
            .map_err(|e| crate::error::AppError::Storage(format!("无法获取数据库连接: {}", e)))?;
        run_migrations(&conn)?;
    }

    Ok(pool)
}

/// 运行数据库迁移（建表）。
///
/// 所有 `CREATE TABLE IF NOT EXISTS`，可重复执行。
pub fn run_migrations(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sessions (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            group_id        TEXT,
            host            TEXT NOT NULL,
            port            INTEGER NOT NULL,
            username        TEXT NOT NULL,
            auth_type       TEXT NOT NULL,
            credential_id   TEXT,
            key_path        TEXT,
            jump_session_id TEXT,
            startup_script  TEXT,
            tags            TEXT,
            color           TEXT,
            sort_order      INTEGER NOT NULL DEFAULT 0,
            created_at      TEXT NOT NULL,
            updated_at      TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS groups (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            parent_id   TEXT,
            sort_order  INTEGER NOT NULL DEFAULT 0,
            created_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS credentials (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            enc_data    TEXT NOT NULL,
            created_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS history (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id  TEXT NOT NULL,
            command     TEXT NOT NULL,
            exit_code   INTEGER,
            run_at      TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_history_session ON history(session_id);
        CREATE INDEX IF NOT EXISTS idx_history_run_at ON history(run_at);

        CREATE TABLE IF NOT EXISTS forward_rules (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            session_id      TEXT NOT NULL,
            kind            TEXT NOT NULL,
            local_host      TEXT NOT NULL,
            local_port      INTEGER NOT NULL,
            remote_host     TEXT NOT NULL,
            remote_port     INTEGER NOT NULL,
            auto_start      INTEGER NOT NULL DEFAULT 0,
            created_at      TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS logs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            level       TEXT NOT NULL,
            message     TEXT NOT NULL,
            created_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS db_profiles (
            id                      TEXT PRIMARY KEY,
            name                    TEXT NOT NULL,
            kind                    TEXT NOT NULL,
            host                    TEXT NOT NULL,
            port                    INTEGER NOT NULL,
            username                TEXT NOT NULL,
            default_database        TEXT,
            credential_id           TEXT,
            ssh_session_config_id   TEXT,
            group_id                TEXT,
            created_at              TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS db_groups (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            parent_id   TEXT,
            sort_order  INTEGER NOT NULL DEFAULT 0,
            created_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS desktops (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            protocol    TEXT NOT NULL,
            host        TEXT NOT NULL,
            port        INTEGER NOT NULL,
            username    TEXT,
            credential_id TEXT,
            sort_order  INTEGER NOT NULL DEFAULT 0,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS totp_secrets (
            id          TEXT PRIMARY KEY,
            issuer      TEXT NOT NULL,
            account     TEXT NOT NULL,
            -- 加密后的 secret blob（base64 编码的 EncryptedBlob，与 credentials 同机制）。
            enc_secret  TEXT NOT NULL,
            algorithm   TEXT NOT NULL DEFAULT 'SHA1',
            digits      INTEGER NOT NULL DEFAULT 6,
            period      INTEGER NOT NULL DEFAULT 30,
            sort_order  INTEGER NOT NULL DEFAULT 0,
            created_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS file_accounts (
            id            TEXT PRIMARY KEY,
            name          TEXT NOT NULL,
            kind          TEXT NOT NULL DEFAULT 's3',
            endpoint      TEXT NOT NULL,
            region        TEXT NOT NULL DEFAULT '',
            bucket        TEXT NOT NULL DEFAULT '',
            credential_id TEXT,
            path_style    INTEGER NOT NULL DEFAULT 1,
            sort_order    INTEGER NOT NULL DEFAULT 0,
            created_at    TEXT NOT NULL,
            updated_at    TEXT NOT NULL
        );
        ",
    )?;

    // --- 幂等列迁移 ---
    // SQLite 不支持 ALTER TABLE ... ADD COLUMN IF NOT EXISTS，用 pragma_table_info 计数守卫。
    // 给 sessions 加 protocol 列（ssh/telnet/rdp/vnc），默认 ssh（兼容已有数据）。
    add_column_if_missing(conn, "sessions", "protocol", "TEXT NOT NULL DEFAULT 'ssh'")?;
    // 给 credentials 加 kind 列（password/private_key_text），避免列表时解密每个 blob。
    add_column_if_missing(
        conn,
        "credentials",
        "kind",
        "TEXT NOT NULL DEFAULT 'password'",
    )?;
    // 给 sessions 加 space_id 列：会话所属空间（"local" 或 JumpServer 空间 id）。
    add_column_if_missing(
        conn,
        "sessions",
        "space_id",
        "TEXT NOT NULL DEFAULT 'local'",
    )?;
    // 给 db_profiles 加 group_id 列：数据库连接所属分组。
    add_column_if_missing(conn, "db_profiles", "group_id", "TEXT")?;
    // 给 file_accounts 加 path_style 列（true=path-style 默认，false=virtual-hosted）。
    add_column_if_missing(
        conn,
        "file_accounts",
        "path_style",
        "INTEGER NOT NULL DEFAULT 1",
    )?;

    Ok(())
}

/// 幂等加列：用 pragma_table_info 检查列是否已存在，不存在则 ALTER TABLE ADD COLUMN。
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    type_def: &str,
) -> AppResult<()> {
    let sql = format!(
        "SELECT COUNT(*) FROM pragma_table_info('{}') WHERE name='{}'",
        table, column
    );
    let exists: i64 = conn.query_row(&sql, [], |r| r.get(0))?;
    if exists == 0 {
        let alter = format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, type_def);
        conn.execute(&alter, [])?;
        log::info!("[db] 已添加列: {}.{}", table, column);
    }
    Ok(())
}
