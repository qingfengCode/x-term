//! DB（MySQL）相关的 Tauri 命令。
//!
//! 命令一览：
//! - [`db_list_profiles`] / [`db_save_profile`] / [`db_delete_profile`]：profile CRUD。
//! - [`db_connect`]：按 profile 建立连接（直连或 SSH 隧道），返回 connId。
//! - [`db_disconnect`]：断开连接。
//! - [`db_exec_sql`]：执行 SQL，结果通过 `db:query_result` 事件推送。
//! - [`db_list_tables`]：列出表。
//! - [`db_describe_table`]：表结构。

use std::time::Instant;

use tauri::{AppHandle, State};

use crate::database::mysql::{connect_direct, connect_via_ssh, MySqlConn};
use crate::database::profiles::{list_db_profiles, upsert_db_profile, DbProfile, DbGroup};
use crate::error::{AppError, AppResult};
use crate::events::{emit, DbQueryResultEvent, DB_QUERY_RESULT};
use crate::state::AppState;
use crate::storage::sessions_repo::get_session;

// ===========================================================================
// profile CRUD
// ===========================================================================

#[tauri::command]
pub fn db_list_profiles(state: State<'_, AppState>) -> AppResult<Vec<DbProfile>> {
    let conn = state.conn()?;
    list_db_profiles(&conn)
}

#[tauri::command]
pub fn db_save_profile(profile: DbProfile, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    upsert_db_profile(&conn, &profile)
}

#[tauri::command]
pub fn db_delete_profile(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    crate::database::profiles::delete_db_profile(&conn, &id)
}

// ===========================================================================
// DB 分组 CRUD
// ===========================================================================

#[tauri::command]
pub fn db_list_groups(state: State<'_, AppState>) -> AppResult<Vec<DbGroup>> {
    let conn = state.conn()?;
    crate::database::profiles::list_db_groups(&conn)
}

#[tauri::command]
pub fn db_save_group(group: DbGroup, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    crate::database::profiles::upsert_db_group(&conn, &group)
}

#[tauri::command]
pub fn db_delete_group(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    crate::database::profiles::delete_db_group(&conn, &id)
}

// ===========================================================================
// 连接管理
// ===========================================================================

/// 建立数据库连接，返回 connId。
///
/// 根据 profile 是否设置了 `ssh_session_config_id` 选择直连或 SSH 隧道。
/// 建好的 [`MySqlConn`] 存入 `state.mysql_conns`。
#[tauri::command]
pub async fn db_connect(
    profile_id: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<String> {
    // 1. 取 profile。
    let profile = {
        let conn = state.conn()?;
        crate::database::profiles::get_db_profile(&conn, &profile_id)?
            .ok_or_else(|| AppError::NotFound(format!("DB profile {} 不存在", profile_id)))?
    };

    // 2. 解析 MySQL 密码。
    let mysql_pass = {
        let cred_id = profile.credential_id.as_ref().ok_or_else(|| {
            AppError::Auth("DB profile 缺少 credential_id".to_string())
        })?;
        let vault_guard = state.vault_read()?;
        let vault = vault_guard
            .as_ref()
            .ok_or_else(|| AppError::Auth("保险库未解锁".to_string()))?
            .clone();
        drop(vault_guard);
        let conn = state.conn()?;
        crate::database::mysql::fetch_mysql_password(&conn, cred_id, &vault)?
    };

    // 3. 建立连接。
    let conn_obj: MySqlConn = if let Some(ssh_id) = &profile.ssh_session_config_id {
        // SSH 隧道模式。
        let ssh_config = {
            let conn = state.conn()?;
            get_session(&conn, ssh_id)?
                .ok_or_else(|| AppError::NotFound(format!("SSH 会话 {} 不存在", ssh_id)))?
        };
        // 解析 SSH 凭据。
        let resolved = {
            let vault_guard = state.vault_read()?;
            let vault = vault_guard
                .as_ref()
                .ok_or_else(|| AppError::Auth("保险库未解锁".to_string()))?
                .clone();
            drop(vault_guard);
            let conn = state.conn()?;
            crate::ssh::session::resolve_credential(&ssh_config, &vault, &conn)?
        };

        connect_via_ssh(
            &ssh_config,
            resolved,
            &profile.host,
            profile.port,
            &profile.username,
            &mysql_pass,
            profile.default_database.as_deref(),
            app,
        )
        .await?
    } else {
        // 直连。
        connect_direct(
            &profile.host,
            profile.port,
            &profile.username,
            &mysql_pass,
            profile.default_database.as_deref(),
        )
        .await?
    };

    // 4. 登记。
    let conn_id = uuid::Uuid::new_v4().to_string();
    state.mysql_conns.lock().insert(conn_id.clone(), conn_obj);

    Ok(conn_id)
}

/// 断开连接。
#[tauri::command]
pub async fn db_disconnect(conn_id: String, state: State<'_, AppState>) -> AppResult<()> {
    let conn_obj = state
        .mysql_conns
        .lock()
        .remove(&conn_id)
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {} 不存在", conn_id)))?;
    conn_obj.close().await;
    Ok(())
}

// ===========================================================================
// SQL 执行
// ===========================================================================

/// 执行一条 SQL，结果通过 `db:query_result` 事件推送（前端用 queryId 匹配）。
///
/// 注意：本命令始终返回 `Ok(())`，查询错误也通过事件的 `error` 字段返回，
/// 以便前端通过同一回调拿到成功/失败。
#[tauri::command]
pub async fn db_exec_sql(
    conn_id: String,
    sql: String,
    query_id: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<()> {
    log::info!("[db_exec_sql] 收到请求: conn_id={}, query_id={}, sql={}", conn_id, query_id, sql);
    // 取出 conn（不持有锁跨 await：clone 出 pool 句柄）。
    // mysql_conns 存的是 MySqlConn（含 pool），无法 clone；这里改为先取出整个
    // MySqlConn，执行完再放回。但若并发执行会互相阻塞。MVP 接受这一限制。
    let conn_obj = state
        .mysql_conns
        .lock()
        .remove(&conn_id)
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {} 不存在", conn_id)))?;
    log::info!("[db_exec_sql] 取出 conn 成功，开始执行");

    let start = Instant::now();
    let res = conn_obj.execute(&sql, 1000).await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    log::info!("[db_exec_sql] 执行完成, 耗时 {}ms, 结果: {}", elapsed_ms, if res.is_ok() { "ok" } else { "err" });

    // 无论成功失败都把 conn 放回。
    state.mysql_conns.lock().insert(conn_id.clone(), conn_obj);

    let event = match res {
        Ok(qr) => DbQueryResultEvent {
            query_id,
            columns: qr.columns,
            rows: qr.rows,
            affected: qr.affected,
            error: None,
            elapsed_ms,
        },
        Err(e) => DbQueryResultEvent {
            query_id,
            columns: Vec::new(),
            rows: Vec::new(),
            affected: 0,
            error: Some(e.to_string()),
            elapsed_ms,
        },
    };
    emit(&app, DB_QUERY_RESULT, event);
    log::info!("[db_exec_sql] 已 emit db:query_result, conn_id={}", conn_id);
    Ok(())
}

// ===========================================================================
// 辅助查询
// ===========================================================================

/// 列出表（`SHOW TABLES`，可选指定库 `SHOW TABLES FROM <database>`）。
///
/// `database` 为 None 时列当前库的表（兼容旧行为）；Some 时列指定库。
#[tauri::command]
pub async fn db_list_tables(
    conn_id: String,
    database: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<Vec<String>> {
    let conn_obj = state
        .mysql_conns
        .lock()
        .remove(&conn_id)
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {} 不存在", conn_id)))?;

    // 构造 SQL：指定库时用 SHOW TABLES FROM <db>。库名做简单防注入。
    let sql = match &database {
        Some(db) => {
            if db.chars().any(|c| c.is_whitespace() || c == ';' || c == '-' || c == '/' || c == '`')
            {
                state.mysql_conns.lock().insert(conn_id, conn_obj);
                return Err(AppError::InvalidInput(format!("非法库名: {}", db)));
            }
            format!("SHOW TABLES FROM `{}`", db)
        }
        None => "SHOW TABLES".into(),
    };
    let res = conn_obj.execute(&sql, 10_000).await;
    state.mysql_conns.lock().insert(conn_id, conn_obj);

    let qr = res?;
    // SHOW TABLES 只有一列：表名。
    let tables: Vec<String> = qr.rows.into_iter().filter_map(|mut r| r.pop()).collect();
    Ok(tables)
}

/// 列出服务器上所有可访问的数据库（`SHOW DATABASES`）。
#[tauri::command]
pub async fn db_list_databases(
    conn_id: String,
    state: State<'_, AppState>,
) -> AppResult<Vec<String>> {
    let conn_obj = state
        .mysql_conns
        .lock()
        .remove(&conn_id)
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {} 不存在", conn_id)))?;

    let res = conn_obj.execute("SHOW DATABASES", 1_000).await;
    state.mysql_conns.lock().insert(conn_id, conn_obj);

    let qr = res?;
    let dbs: Vec<String> = qr.rows.into_iter().filter_map(|mut r| r.pop()).collect();
    Ok(dbs)
}

/// 表结构（`DESCRIBE <table>`）。
///
/// `table` 支持 `db.table` 限定名或仅 `table`；标识符校验与限定名拼接由
/// [`crate::database::mysql::qualify_table_identifier`] 统一处理。
#[tauri::command]
pub async fn db_describe_table(
    conn_id: String,
    table: String,
    state: State<'_, AppState>,
) -> AppResult<crate::database::mysql::QueryResult> {
    let qualified = crate::database::mysql::qualify_table_identifier(&table)?;
    let sql = format!("DESCRIBE {qualified}");

    let conn_obj = state
        .mysql_conns
        .lock()
        .remove(&conn_id)
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {} 不存在", conn_id)))?;

    let res = conn_obj.execute(&sql, 1000).await;
    state.mysql_conns.lock().insert(conn_id, conn_obj);

    res
}

/// 获取表的 `SHOW CREATE TABLE` 语句（用于 AI 拖表附加表结构上下文）。
///
/// 返回 DDL 文本（从结果集第一行第二列提取，MySQL 该语句返回
/// `['Table', 'Create Table']` 两列）。
#[tauri::command]
pub async fn db_show_create_table(
    conn_id: String,
    database: Option<String>,
    table: String,
    state: State<'_, AppState>,
) -> AppResult<String> {
    // 严格的标识符白名单校验：字母、数字、下划线、点、$。
    // 注意：禁空白/分号/注释/反引号；库名表名都用反引号包裹后拼接。
    let ident_re = regex::Regex::new(r"^[A-Za-z0-9_.$]+$").unwrap();
    if !ident_re.is_match(&table) {
        return Err(AppError::InvalidInput(format!("非法表名: {}", table)));
    }
    if let Some(db) = &database {
        if !ident_re.is_match(db) {
            return Err(AppError::InvalidInput(format!("非法库名: {}", db)));
        }
    }
    let qualified = match &database {
        Some(db) => format!("`{}`.`{}`", db, table),
        None => format!("`{}`", table),
    };
    let sql = format!("SHOW CREATE TABLE {}", qualified);

    let conn_obj = state
        .mysql_conns
        .lock()
        .remove(&conn_id)
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {} 不存在", conn_id)))?;

    let res = conn_obj.execute(&sql, 1).await;
    state.mysql_conns.lock().insert(conn_id, conn_obj);

    let result = res?;
    // SHOW CREATE TABLE 返回一行两列：[表名, DDL 文本]。
    if result.rows.is_empty() || result.rows[0].len() < 2 {
        return Ok(format!("-- 无法获取 {} 的建表语句", qualified));
    }
    Ok(result.rows[0][1].clone())
}
