//! DB（MySQL）相关的 Tauri 命令。
//!
//! 命令一览：
//! - [`db_list_profiles`] / [`db_save_profile`] / [`db_delete_profile`]：profile CRUD。
//! - [`db_connect`]：按 profile 建立连接（直连或 SSH 隧道），返回 connId。
//! - [`db_disconnect`]：断开连接。
//! - [`db_exec_sql`]：执行 SQL，结果通过 `db:query_result` 事件推送。
//! - [`db_list_tables`]：列出表。
//! - [`db_describe_table`]：表结构。

use std::sync::Arc;
use std::time::Instant;

use tauri::{AppHandle, State};

use crate::database::mysql::{connect_direct, connect_via_ssh, MySqlConn};
use crate::database::profiles::{list_db_profiles, upsert_db_profile, DbGroup, DbProfile};
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
///
/// 同步的 SQLite 查询与 vault 解析放进 `spawn_blocking`：async 命令直接在
/// tokio worker 线程上执行同步 IO，会话并发时会造成运行时抖动。
#[tauri::command]
pub async fn db_connect(profile_id: String, state: State<'_, AppState>) -> AppResult<String> {
    // 1+2. 取 profile、解析 MySQL 密码（同步 IO → 阻塞线程池）。
    let st = state.inner().clone();
    let (profile, mysql_pass) = tokio::task::spawn_blocking(move || -> AppResult<_> {
        let profile = {
            let conn = st.conn()?;
            crate::database::profiles::get_db_profile(&conn, &profile_id)?
                .ok_or_else(|| AppError::NotFound(format!("DB profile {} 不存在", profile_id)))?
        };
        let mysql_pass = {
            let cred_id = profile
                .credential_id
                .as_ref()
                .ok_or_else(|| AppError::Auth("DB profile 缺少 credential_id".to_string()))?;
            let vault_guard = st.vault_read()?;
            let vault = vault_guard
                .as_ref()
                .ok_or_else(|| AppError::Auth("保险库未解锁".to_string()))?
                .clone();
            drop(vault_guard);
            let conn = st.conn()?;
            crate::database::mysql::fetch_mysql_password(&conn, cred_id, &vault)?
        };
        Ok((profile, mysql_pass))
    })
    .await
    .map_err(|e| AppError::Storage(format!("后台任务失败: {}", e)))??;

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
            state.inner().clone(),
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
    state.mysql_conns.lock().insert(conn_id.clone(), Arc::new(conn_obj));

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

/// 切换连接的当前库（schema）。前端点库节点 / 新建库标签时调用。
///
/// 之后该连接上的所有查询都会自动带 `USE \`db\``（见 [`MySqlConn::execute`]），
/// SQL 里无需再写库前缀。传 `None` 清除（回落到连接 URL 里的默认库）。
#[tauri::command]
pub async fn db_use_database(
    conn_id: String,
    database: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<()> {
    if let Some(db) = &database {
        crate::database::mysql::validate_database_identifier(db)?;
    }
    let conn_obj = state
        .mysql_conns
        .lock()
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {} 不存在", conn_id)))?;
    conn_obj.set_current_db(database);
    Ok(())
}

// ===========================================================================
// SQL 执行
// ===========================================================================

/// 只读模式强制校验（后端兜底；前端判定可被 `WITH`/`SELECT INTO OUTFILE` 等
/// 形式绕过，必须由后端按实际收到的 SQL 复核）。
///
/// 逐条检查输入里的每条非空语句：必须通过 [`crate::ai::tools::is_readonly_sql`]
/// （SELECT/SHOW/EXPLAIN/DESCRIBE/DESC，WITH 引导的 CTE 由 sql_first_keyword
/// 解析成主语句关键字后再判定），并拒绝 `SELECT ... INTO OUTFILE/DUMPFILE`
/// （会在服务器文件系统上写文件）。违反任一即返回错误。
fn enforce_read_only(sql: &str) -> AppResult<()> {
    for stmt in sql.split(';') {
        let s = stmt.trim();
        if s.is_empty() {
            continue;
        }
        if !crate::ai::tools::is_readonly_sql(s) {
            let kw = s.split_whitespace().next().unwrap_or("?");
            return Err(AppError::Auth(format!(
                "只读模式不允许执行写操作（首关键字 {}）",
                kw
            )));
        }
        let upper = s.to_ascii_uppercase();
        if upper.contains("INTO OUTFILE") || upper.contains("INTO DUMPFILE") {
            return Err(AppError::Auth(
                "只读模式不允许 SELECT INTO OUTFILE/DUMPFILE".to_string(),
            ));
        }
    }
    Ok(())
}

/// 执行一条 SQL，结果通过 `db:query_result` 事件推送（前端用 queryId 匹配）。
///
/// 注意：本命令始终返回 `Ok(())`，查询错误也通过事件的 `error` 字段返回，
/// 以便前端通过同一回调拿到成功/失败。
#[tauri::command]
pub async fn db_exec_sql(
    conn_id: String,
    sql: String,
    query_id: String,
    read_only: bool,
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<()> {
    // SQL 全文可能含敏感数据（INSERT/UPDATE 的明文值），日志只记前 200 字符。
    let sql_log: String = sql.chars().take(200).collect();
    log::info!(
        "[db_exec_sql] 收到请求: conn_id={}, query_id={}, sql={}{}",
        conn_id,
        query_id,
        sql_log,
        if sql.chars().count() > 200 { "…" } else { "" }
    );

    // USE 语句拦截：pool 语义下直接执行 USE 只对单条连接生效，必须由本层记录
    // 当前库，并在每次查询前自动带上（见 MySqlConn::execute 的 db 参数）。
    if let Some(use_db) = crate::database::mysql::parse_use_statement(&sql) {
        let set_res: AppResult<()> = (|| {
            if let Some(db) = &use_db {
                crate::database::mysql::validate_database_identifier(db)?;
            }
            let conn_obj = state
                .mysql_conns
                .lock()
                .get(&conn_id)
                .cloned()
                .ok_or_else(|| AppError::NotFound(format!("DB 连接 {} 不存在", conn_id)))?;
            conn_obj.set_current_db(use_db.clone());
            Ok(())
        })();
        let event = match set_res {
            Ok(()) => {
                log::info!(
                    "[db_exec_sql] USE 切换当前库: {}",
                    use_db.as_deref().unwrap_or("(空)")
                );
                DbQueryResultEvent {
                    query_id,
                    columns: Vec::new(),
                    rows: Vec::new(),
                    affected: 0,
                    truncated: false,
                    error: None,
                    elapsed_ms: 0,
                }
            }
            Err(e) => DbQueryResultEvent {
                query_id,
                columns: Vec::new(),
                rows: Vec::new(),
                affected: 0,
                truncated: false,
                error: Some(e.to_string()),
                elapsed_ms: 0,
            },
        };
        emit(&app, DB_QUERY_RESULT, event);
        return Ok(());
    }

    // 只读模式强制校验：USE 放行（纯切库无副作用），其余语句逐条复核。
    if read_only {
        if let Err(e) = enforce_read_only(&sql) {
            log::info!("[db_exec_sql] 只读模式拦截: query_id={}, {}", query_id, e);
            let event = DbQueryResultEvent {
                query_id,
                columns: Vec::new(),
                rows: Vec::new(),
                affected: 0,
                truncated: false,
                error: Some(e.to_string()),
                elapsed_ms: 0,
            };
            emit(&app, DB_QUERY_RESULT, event);
            return Ok(());
        }
    }

    // 取出 conn 句柄（Arc 克隆，不持有锁跨 await）；mysql_conns 的值是
    // Arc<MySqlConn>，多个命令可并发操作同一连接，不会互相 remove/insert 竞争。
    let conn_obj = state
        .mysql_conns
        .lock()
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {} 不存在", conn_id)))?;
    log::info!("[db_exec_sql] 取出 conn 成功，开始执行");

    let start = Instant::now();
    // 带当前库执行：连接上的每条查询都先 USE，保证落在当前库上。
    let cur_db = conn_obj.current_db();
    let res = conn_obj.execute(&sql, 1000, cur_db.as_deref()).await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    log::info!(
        "[db_exec_sql] 执行完成, 耗时 {}ms, 结果: {}",
        elapsed_ms,
        if res.is_ok() { "ok" } else { "err" }
    );

    let event = match res {
        Ok(qr) => DbQueryResultEvent {
            query_id,
            columns: qr.columns,
            rows: qr.rows,
            affected: qr.affected,
            truncated: qr.truncated,
            error: None,
            elapsed_ms,
        },
        Err(e) => DbQueryResultEvent {
            query_id,
            columns: Vec::new(),
            rows: Vec::new(),
            affected: 0,
            truncated: false,
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
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {} 不存在", conn_id)))?;

    // 构造 SQL：指定库时用 SHOW TABLES FROM <db>。库名走统一白名单校验
    // （与 db_use_database / db_show_create_table 一致，黑名单易随修改失效）。
    let sql = match &database {
        Some(db) => {
            crate::database::mysql::validate_database_identifier(db)?;
            format!("SHOW TABLES FROM `{}`", db)
        }
        None => "SHOW TABLES".into(),
    };
    // 未指定库时按连接当前库执行（USE 自动带上，SHOW TABLES 即当前库的表）。
    let cur_db = conn_obj.current_db();
    let res = conn_obj.execute(&sql, 10_000, cur_db.as_deref()).await;

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
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {} 不存在", conn_id)))?;

    let res = conn_obj.execute("SHOW DATABASES", 1_000, None).await;

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
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {} 不存在", conn_id)))?;

    // 未限定库名的 DESCRIBE（`DESCRIBE \`table\``）按连接当前库执行。
    let cur_db = conn_obj.current_db();
    let res = conn_obj.execute(&sql, 1000, cur_db.as_deref()).await;

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
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {} 不存在", conn_id)))?;

    // 未限定库名时按连接当前库执行。
    let cur_db = conn_obj.current_db();
    let res = conn_obj.execute(&sql, 1, cur_db.as_deref()).await;

    let result = res?;
    // SHOW CREATE TABLE 返回一行两列：[表名, DDL 文本]。
    if result.rows.is_empty() || result.rows[0].len() < 2 {
        return Ok(format!("-- 无法获取 {} 的建表语句", qualified));
    }
    Ok(result.rows[0][1].clone())
}
