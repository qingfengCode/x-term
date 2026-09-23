//! DB（MySQL / PostgreSQL）相关的 Tauri 命令。
//!
//! 命令一览：
//! - [`db_list_profiles`] / [`db_save_profile`] / [`db_delete_profile`]：profile CRUD。
//! - [`db_connect`]：按 profile 建立连接（按 kind 分发方言，直连或 SSH 隧道），
//!   返回 connId。
//! - [`db_disconnect`]：断开连接。
//! - [`db_exec_sql`]：执行 SQL，结果通过 `db:query_result` 事件推送。
//! - [`db_list_tables`] / [`db_list_databases`] / [`db_describe_table`] /
//!   [`db_show_create_table`]：元数据（按方言生成 SQL，见
//!   [`crate::database::DbConnHandle`]）。
//!
//! 所有在本模块内访问 SQLite 的命令一律 async + `spawn_blocking`：Tauri 的非
//! async 命令在**主线程**执行，取连接（池耗尽等待上限 5s）或 SQLite 写锁等待
//! （`busy_timeout` 5000ms）会直接冻结整个窗口。

use std::time::Instant;

use tauri::{AppHandle, Manager, State};

use crate::database::profiles::{list_db_profiles, upsert_db_profile, DbGroup, DbProfile};
use crate::database::DbConnHandle;
use crate::error::{AppError, AppResult};
use crate::events::{emit, DbQueryResultEvent, DB_QUERY_RESULT};
use crate::state::AppState;
use crate::storage::sessions_repo::get_session;

// ===========================================================================
// profile CRUD
// ===========================================================================

#[tauri::command]
pub async fn db_list_profiles(state: State<'_, AppState>) -> AppResult<Vec<DbProfile>> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        list_db_profiles(&conn)
    })
    .await
    .map_err(|e| AppError::Storage(format!("读取数据库连接列表任务失败: {}", e)))?
}

#[tauri::command]
pub async fn db_save_profile(profile: DbProfile, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        upsert_db_profile(&conn, &profile)
    })
    .await
    .map_err(|e| AppError::Storage(format!("保存数据库连接任务失败: {}", e)))?
}

#[tauri::command]
pub async fn db_delete_profile(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::database::profiles::delete_db_profile(&conn, &id)
    })
    .await
    .map_err(|e| AppError::Storage(format!("删除数据库连接任务失败: {}", e)))?
}

// ===========================================================================
// DB 分组 CRUD
// ===========================================================================

#[tauri::command]
pub async fn db_list_groups(state: State<'_, AppState>) -> AppResult<Vec<DbGroup>> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::database::profiles::list_db_groups(&conn)
    })
    .await
    .map_err(|e| AppError::Storage(format!("读取数据库分组任务失败: {}", e)))?
}

#[tauri::command]
pub async fn db_save_group(group: DbGroup, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::database::profiles::upsert_db_group(&conn, &group)
    })
    .await
    .map_err(|e| AppError::Storage(format!("保存数据库分组任务失败: {}", e)))?
}

#[tauri::command]
pub async fn db_delete_group(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.conn()?;
        crate::database::profiles::delete_db_group(&conn, &id)
    })
    .await
    .map_err(|e| AppError::Storage(format!("删除数据库分组任务失败: {}", e)))?
}

// ===========================================================================
// 连接管理
// ===========================================================================

/// 建立数据库连接，返回 connId。
///
/// 按 profile.kind 分发到 MySQL / PostgreSQL 实现；根据 profile 是否设置了
/// `ssh_session_config_id` 选择直连或 SSH 隧道。建好的 [`DbConnHandle`]
/// 存入 `state.db_conns`。
///
/// 同步的 SQLite 查询与 vault 解析放进 `spawn_blocking`：async 命令直接在
/// tokio worker 线程上执行同步 IO，会话并发时会造成运行时抖动。
#[tauri::command]
pub async fn db_connect(profile_id: String, state: State<'_, AppState>) -> AppResult<String> {
    // SQLite（本地文件）无需凭据，走独立分支：profile.host 即文件路径。
    {
        let sqlite_path = {
            let conn = state.conn()?;
            crate::database::profiles::get_db_profile(&conn, &profile_id)?
                .filter(|p| crate::database::normalize_kind(&p.kind) == "sqlite")
                .map(|p| p.host.clone())
        };
        if let Some(path) = sqlite_path {
            if path.trim().is_empty() {
                return Err(AppError::InvalidInput(
                    "SQLite profile 的 host 字段需填数据库文件路径".into(),
                ));
            }
            let conn_obj = DbConnHandle::Sqlite(std::sync::Arc::new(
                crate::database::sqlite::SqliteConn::open(&path)?,
            ));
            let conn_id = uuid::Uuid::new_v4().to_string();
            state.db_conns.lock().insert(conn_id.clone(), conn_obj);
            log::info!("[db_connect] SQLite 已连接: {path}");
            return Ok(conn_id);
        }
    }

    // 1+2. 取 profile、解析数据库密码（同步 IO → 阻塞线程池）。
    let st = state.inner().clone();
    let (profile, db_pass) = tokio::task::spawn_blocking(move || -> AppResult<_> {
        let profile = {
            let conn = st.conn()?;
            crate::database::profiles::get_db_profile(&conn, &profile_id)?
                .ok_or_else(|| AppError::NotFound(format!("DB profile {profile_id} 不存在")))?
        };
        let db_pass = {
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
            crate::database::fetch_db_password(&conn, cred_id, &vault)?
        };
        Ok((profile, db_pass))
    })
    .await
    .map_err(|e| AppError::Storage(format!("后台任务失败: {}", e)))??;

    // 3. 建立连接（SSH 隧道参数在两种方言下共用，仅建连函数不同）。
    let conn_obj: DbConnHandle = match crate::database::normalize_kind(&profile.kind) {
        "postgres" => {
            let pg = if let Some(ssh_id) = &profile.ssh_session_config_id {
                let (ssh_config, resolved) = resolve_ssh(&state, ssh_id)?;
                crate::database::postgres::connect_via_ssh(
                    &ssh_config,
                    resolved,
                    &profile.host,
                    profile.port,
                    &profile.username,
                    &db_pass,
                    profile.default_database.as_deref(),
                    state.inner().clone(),
                )
                .await?
            } else {
                crate::database::postgres::connect_direct(
                    &profile.host,
                    profile.port,
                    &profile.username,
                    &db_pass,
                    profile.default_database.as_deref(),
                )
                .await?
            };
            DbConnHandle::Postgres(std::sync::Arc::new(pg))
        }
        _ => {
            let my = if let Some(ssh_id) = &profile.ssh_session_config_id {
                let (ssh_config, resolved) = resolve_ssh(&state, ssh_id)?;
                crate::database::mysql::connect_via_ssh(
                    &ssh_config,
                    resolved,
                    &profile.host,
                    profile.port,
                    &profile.username,
                    &db_pass,
                    profile.default_database.as_deref(),
                    state.inner().clone(),
                )
                .await?
            } else {
                crate::database::mysql::connect_direct(
                    &profile.host,
                    profile.port,
                    &profile.username,
                    &db_pass,
                    profile.default_database.as_deref(),
                )
                .await?
            };
            DbConnHandle::MySql(std::sync::Arc::new(my))
        }
    };

    // 4. 登记。
    let conn_id = uuid::Uuid::new_v4().to_string();
    state.db_conns.lock().insert(conn_id.clone(), conn_obj);

    Ok(conn_id)
}

/// 解析 SSH 隧道所需的会话配置与凭据（MySQL / PG 共用）。
fn resolve_ssh(
    state: &State<'_, AppState>,
    ssh_id: &str,
) -> AppResult<(crate::storage::sessions_repo::Session, crate::ssh::session::ResolvedCredential)> {
    let ssh_config = {
        let conn = state.conn()?;
        get_session(&conn, ssh_id)?
            .ok_or_else(|| AppError::NotFound(format!("SSH 会话 {ssh_id} 不存在")))?
    };
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
    Ok((ssh_config, resolved))
}

/// 断开连接。
#[tauri::command]
pub async fn db_disconnect(conn_id: String, state: State<'_, AppState>) -> AppResult<()> {
    let conn_obj = state
        .db_conns
        .lock()
        .remove(&conn_id)
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {conn_id} 不存在")))?;
    conn_obj.close().await;
    Ok(())
}

/// 切换连接的当前库（schema）。前端点库节点 / 新建库标签时调用。
///
/// MySQL：记录 current_db，之后该连接上的所有查询都自动带
/// `USE \`db\``（见 [`crate::database::mysql::MySqlConn::execute`]）。
/// PostgreSQL：换库重连（pool 替换，见 [`crate::database::postgres::PgConn::use_database`]）。
/// 传 `None` 清除（回落到 profile 的默认库）。
#[tauri::command]
pub async fn db_use_database(
    conn_id: String,
    database: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let conn_obj = state
        .db_conns
        .lock()
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {conn_id} 不存在")))?;
    conn_obj.use_database(database.as_deref()).await?;
    Ok(())
}

// ===========================================================================
// SQL 执行
// ===========================================================================

/// 只读模式强制校验（后端兜底；前端判定可被 `WITH`/`SELECT INTO OUTFILE` 等
/// 形式绕过，必须由后端按实际收到的 SQL 复核）。
///
/// 逐条检查输入里的每条非空语句：必须通过 [`crate::ai::tools::is_readonly_sql`]
/// （SELECT/SHOW/EXPLAIN/DESCRIBE/DESC/TABLE/VALUES，WITH 引导的 CTE 由
/// sql_first_keyword 解析成主语句关键字后再判定），并拒绝
/// `SELECT ... INTO OUTFILE/DUMPFILE`（会在服务器文件系统上写文件）。
/// 违反任一即返回错误。
fn enforce_read_only(sql: &str) -> AppResult<()> {
    // 引号感知切分：字符串字面量内的分号不是语句边界（裸 split(';') 会把
    // `WHERE c='a;b'` 的残段 `b'` 切出来，首关键字判定必失败、误拒合法查询）。
    for stmt in crate::database::script_split::top_level_fragments(sql) {
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

    // 取出 conn 句柄（enum 克隆，不持有锁跨 await）；db_conns 的值可克隆，
    // 多个命令可并发操作同一连接，不会互相 remove/insert 竞争。
    let conn_obj = state
        .db_conns
        .lock()
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {conn_id} 不存在")))?;

    // USE 语句拦截：pool 语义下直接执行 USE 只对单条连接生效（MySQL），
    // PG 则没有 USE 语句——统一由本层处理：MySQL 记录当前库、PG 换库重连
    // （见 DbConnHandle::use_database），语句本身不再发给数据库。
    if let Some(use_db) = crate::database::mysql::parse_use_statement(&sql) {
        let set_res = conn_obj.use_database(use_db.as_deref()).await;
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

    // 只读模式强制校验：USE 已放行（纯切库无副作用），其余语句逐条复核。
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

    log::info!("[db_exec_sql] 取出 conn 成功，开始执行");

    let start = Instant::now();
    // MySQL 自动带当前库执行（每条查询前 USE）；PG 的库在连接层已确定。
    let res = conn_obj.execute(&sql, 1000).await;
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

/// 列出表（可选指定库；方言差异由 [`DbConnHandle::list_tables`] 处理）。
#[tauri::command]
pub async fn db_list_tables(
    conn_id: String,
    database: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<Vec<String>> {
    let conn_obj = state
        .db_conns
        .lock()
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {conn_id} 不存在")))?;
    conn_obj.list_tables(database.as_deref()).await
}

/// 列出服务器上所有可访问的数据库。
#[tauri::command]
pub async fn db_list_databases(
    conn_id: String,
    state: State<'_, AppState>,
) -> AppResult<Vec<String>> {
    let conn_obj = state
        .db_conns
        .lock()
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {conn_id} 不存在")))?;
    conn_obj.list_databases().await
}

/// 表结构（方言差异由 [`DbConnHandle::describe_table`] 处理）。
///
/// `table` 支持 `db.table`（PG 为 `schema.table`）限定名或仅 `table`。
#[tauri::command]
pub async fn db_describe_table(
    conn_id: String,
    table: String,
    state: State<'_, AppState>,
) -> AppResult<crate::database::QueryResult> {
    let conn_obj = state
        .db_conns
        .lock()
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {conn_id} 不存在")))?;
    conn_obj.describe_table(&table).await
}

/// 获取建表 DDL（用于 AI 拖表附加表结构上下文）。
///
/// MySQL 为 `SHOW CREATE TABLE` 原文；PG 由 pg_catalog 生成近似 DDL
/// （见 [`DbConnHandle::table_ddl`]）。
#[tauri::command]
pub async fn db_show_create_table(
    conn_id: String,
    database: Option<String>,
    table: String,
    state: State<'_, AppState>,
) -> AppResult<String> {
    let conn_obj = state
        .db_conns
        .lock()
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {conn_id} 不存在")))?;
    conn_obj.table_ddl(database.as_deref(), &table).await
}

// ===========================================================================
// 多厂商扩展（P1）：能力开关 / 浏览模式分页 SQL / 脚本执行
// ===========================================================================

/// 连接的方言能力（前端 UI 显隐的单一事实来源）。
#[tauri::command]
pub fn db_capabilities(
    conn_id: String,
    state: State<'_, AppState>,
) -> AppResult<crate::database::DbCapabilities> {
    let caps = {
        let conns = state.db_conns.lock();
        let conn_obj = conns
            .get(&conn_id)
            .ok_or_else(|| AppError::NotFound(format!("DB 连接 {conn_id} 不存在")))?;
        conn_obj.capabilities().clone()
    };
    Ok(caps)
}

/// 生成"浏览模式"的分页 SELECT（只生成文本，前端放进编辑器执行）。
///
/// 点表浏览数据 / 翻页时调用：方言分页差异（LIMIT/OFFSET vs FETCH vs TOP）
/// 收敛在后端，前端 UI 完全不感知方言。
#[tauri::command]
pub fn db_default_table_query(
    conn_id: String,
    table: String,
    limit: u32,
    offset: u32,
    state: State<'_, AppState>,
) -> AppResult<String> {
    let sql = {
        let conns = state.db_conns.lock();
        let conn_obj = conns
            .get(&conn_id)
            .ok_or_else(|| AppError::NotFound(format!("DB 连接 {conn_id} 不存在")))?;
        conn_obj.default_table_query(&table, limit, offset)?
    };
    Ok(sql)
}

/// 一条脚本语句的执行结果（脚本模式）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptStmtResult {
    pub line: u32,
    /// 语句前 200 字符（失败定位展示）。
    pub sql_preview: String,
    pub affected: u64,
    /// 出错时的错误信息；None = 成功。
    pub error: Option<String>,
}

/// 按方言切分 SQL 脚本并逐条执行（首错即停）。
///
/// 切分器见 [`crate::database::script_split`]（字符串/注释/MySQL DELIMITER/
/// PG dollar-quoting/SQL Server GO）。同步返回每条结果；失败条目带行号。
#[tauri::command]
pub async fn db_execute_script(
    conn_id: String,
    script: String,
    read_only: bool,
    state: State<'_, AppState>,
) -> AppResult<Vec<ScriptStmtResult>> {
    let conn_obj = state
        .db_conns
        .lock()
        .get(&conn_id)
        .cloned()
        .ok_or_else(|| AppError::NotFound(format!("DB 连接 {conn_id} 不存在")))?;

    let kind = conn_obj.kind();
    let stmts = crate::database::script_split::split_sql_script(&script, kind);
    let mut out = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        // USE 拦截（与 db_exec_sql 一致）：MySQL 预处理协议执行 USE 必报 1295，
        // 且只读模式下 USE 属纯切库无副作用，先行放行由句柄层统一处理。
        if let Some(use_db) = crate::database::mysql::parse_use_statement(&stmt.sql) {
            let res = conn_obj.use_database(use_db.as_deref()).await;
            let err = res.err().map(|e| e.to_string());
            out.push(ScriptStmtResult {
                line: stmt.line,
                sql_preview: preview(&stmt.sql),
                affected: 0,
                error: err,
            });
            if out.last().and_then(|r| r.error.as_ref()).is_some() {
                // 切库失败同样首错即停。
                break;
            }
            continue;
        }
        // 只读校验（与 db_exec_sql 同一规则）。
        if read_only {
            if let Err(e) = enforce_read_only(&stmt.sql) {
                out.push(ScriptStmtResult {
                    line: stmt.line,
                    sql_preview: preview(&stmt.sql),
                    affected: 0,
                    error: Some(e.to_string()),
                });
                break;
            }
        }
        match conn_obj.execute(&stmt.sql, 1000).await {
            Ok(qr) => out.push(ScriptStmtResult {
                line: stmt.line,
                sql_preview: preview(&stmt.sql),
                affected: qr.affected,
                error: None,
            }),
            Err(e) => {
                out.push(ScriptStmtResult {
                    line: stmt.line,
                    sql_preview: preview(&stmt.sql),
                    affected: 0,
                    error: Some(e.to_string()),
                });
                // 首错即停（与 uniterm ExecuteScript 语义一致）。
                break;
            }
        }
    }
    Ok(out)
}

/// 语句预览（前 200 字符）。
fn preview(sql: &str) -> String {
    let mut p: String = sql.chars().take(200).collect();
    if sql.chars().count() > 200 {
        p.push('…');
    }
    p
}
