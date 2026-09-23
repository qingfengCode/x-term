//! 数据迁移（备份导出 / 导入）命令。
//!
//! 三个命令：
//! - [`backup_export`]：聚合全部用户数据 → 用备份密码加密 → 写入指定文件；
//! - [`backup_inspect`]：解密文件返回元数据（导入前预览 / 校验密码），不落任何数据；
//! - [`backup_import`]：解密文件 → 按合并或覆盖模式写入本机。
//!
//! 凭据与 TOTP secret 在本机以保险库（vault）加密 blob 存储；导出时需保险库
//! 已解锁才能解出明文打包，导入时用目标机保险库重新加密。因此：
//! - 导出时 vault 未解锁 → 凭据 / TOTP 段跳过，在摘要中报告（不阻塞导出）；
//! - 导入时文件含凭据 / TOTP 且 vault 未解锁 → 直接报错中止（避免凭据静默丢失）。

use rusqlite::Transaction;
use serde_json::Value;
use tauri::{Manager, State};

use crate::backup::{
    decrypt_full, decrypt_payload, encrypt_payload, BackupCounts, BackupInfo, BackupPayload,
    BackupSummary, PlainCredential, PlainTotp,
};
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::storage::secure::CredentialVault;

/// `settings.json` / `mcp.json` 文件名（位于应用数据目录下）。
const MCP_CONFIG_FILENAME: &str = "mcp.json";
const SETTINGS_FILENAME: &str = "settings.json";

// ===========================================================================
// 导出
// ===========================================================================

/// 把全部用户数据导出为加密备份文件。返回各段条目数摘要。
///
/// async + spawn_blocking：全表读取 + 逐条 AES-GCM 解密 + Argon2 派生 + 全量
/// JSON 序列化 + 写盘，大数据量下可达数百毫秒~数秒，不能在主线程执行。
#[tauri::command]
pub async fn backup_export(
    path: String,
    password: String,
    state: State<'_, AppState>,
) -> AppResult<BackupSummary> {
    if password.len() < 6 {
        return Err(AppError::InvalidInput("备份密码至少 6 位".into()));
    }
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        backup_export_inner(&path, &password, &state)
    })
    .await
    .map_err(|e| AppError::Ssh(format!("备份导出任务失败: {}", e)))?
}

/// [`backup_export`] 的执行主体（阻塞线程池内运行）。
fn backup_export_inner(
    path: &str,
    password: &str,
    state: &AppState,
) -> AppResult<BackupSummary> {
    let conn = state.conn()?;
    let mut payload = BackupPayload {
        sessions: crate::storage::sessions_repo::list_sessions(&conn)?,
        groups: crate::storage::sessions_repo::list_groups(&conn)?,
        db_profiles: crate::database::profiles::list_db_profiles(&conn)?,
        db_groups: crate::database::profiles::list_db_groups(&conn)?,
        desktops: crate::storage::desktops_repo::list_desktops(&conn)?,
        file_accounts: crate::storage::file_accounts_repo::list_file_accounts(&conn)?,
        forward_rules: list_forward_rules(&conn)?,
        ..Default::default()
    };

    // 凭据 / TOTP：需要保险库已解锁才能解出明文。
    let mut credentials_skipped = 0usize;
    let mut totp_skipped = 0usize;
    if state.vault_ready() {
        let guard = state.vault_read()?;
        let vault = guard
            .as_ref()
            .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?;
        payload.credentials = export_credentials(&conn, vault)?;
        payload.totp_secrets = export_totp_secrets(&conn, vault)?;
        drop(guard);
    } else {
        let cred_count = count_rows(&conn, "SELECT COUNT(*) FROM credentials")?;
        let totp_count = count_rows(&conn, "SELECT COUNT(*) FROM totp_secrets")?;
        credentials_skipped = cred_count;
        totp_skipped = totp_count;
        log::info!(
            "[backup] 保险库未解锁，跳过 {} 条凭据 / {} 条 TOTP",
            credentials_skipped,
            totp_skipped
        );
    }

    // settings.json / mcp.json 原样打包（整个文件已被备份密码加密）。
    payload.settings = read_json_value(&state.settings_path.join(SETTINGS_FILENAME))?;
    payload.mcp = read_json_value(&state.settings_path.join(MCP_CONFIG_FILENAME))?;

    let file = encrypt_payload(&payload, password, env!("CARGO_PKG_VERSION"))?;
    let content = serde_json::to_string_pretty(&file)?;
    std::fs::write(path, content)?;

    Ok(BackupSummary {
        path: path.to_string(),
        counts: BackupCounts::from_payload(&payload),
        credentials_skipped,
        totp_skipped,
        overwritten: false,
    })
}

/// 从 `credentials` 表导出全部凭据明文（需保险库已解锁）。
fn export_credentials(conn: &rusqlite::Connection, vault: &CredentialVault) -> AppResult<Vec<PlainCredential>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, enc_data, created_at, kind FROM credentials ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4).unwrap_or_else(|_| "password".into()),
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (id, name, enc_data, created_at, kind) = row?;
        // 解密明文 JSON：{"kind":..,"value":..,"passphrase":..}
        let blob = CredentialVault::decode_blob(&enc_data)?;
        let plain = vault.decrypt_str(&blob)?;
        let v: Value = serde_json::from_str(&plain)
            .map_err(|e| AppError::Crypto(format!("凭据 {} 内容解析失败: {}", id, e)))?;
        out.push(PlainCredential {
            id,
            name,
            kind,
            created_at,
            value: v.get("value").and_then(Value::as_str).unwrap_or_default().to_string(),
            passphrase: v.get("passphrase").and_then(Value::as_str).map(String::from),
        });
    }
    Ok(out)
}

/// 从 `totp_secrets` 表导出全部 TOTP 明文 secret（需保险库已解锁）。
fn export_totp_secrets(conn: &rusqlite::Connection, vault: &CredentialVault) -> AppResult<Vec<PlainTotp>> {
    let mut stmt = conn.prepare(
        "SELECT id, issuer, account, enc_secret, algorithm, digits, period, sort_order, created_at \
         FROM totp_secrets ORDER BY sort_order",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, String>(8)?,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (id, issuer, account, enc_secret, algorithm, digits, period, sort_order, created_at) = row?;
        let blob = CredentialVault::decode_blob(&enc_secret)?;
        let secret = vault.decrypt_str(&blob)?;
        out.push(PlainTotp {
            id,
            issuer,
            account,
            algorithm,
            digits: digits as u32,
            period: period as u32,
            sort_order,
            created_at,
            secret,
        });
    }
    Ok(out)
}

/// 从 `forward_rules` 表读取全部规则。
fn list_forward_rules(conn: &rusqlite::Connection) -> AppResult<Vec<crate::commands::forward::ForwardRule>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, session_id, kind, local_host, local_port, remote_host, \
         remote_port, auto_start, created_at FROM forward_rules ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        let auto: i64 = r.get(8)?;
        Ok(crate::commands::forward::ForwardRule {
            id: r.get(0)?,
            name: r.get(1)?,
            session_id: r.get(2)?,
            kind: r.get(3)?,
            local_host: r.get(4)?,
            local_port: r.get(5)?,
            remote_host: r.get(6)?,
            remote_port: r.get(7)?,
            auto_start: auto != 0,
            created_at: r.get(9)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 读取 JSON 文件为 Value；文件不存在返回 None，解析失败报错。
fn read_json_value(path: &std::path::Path) -> AppResult<Option<Value>> {
    let txt = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let v: Value = serde_json::from_str(&txt)
        .map_err(|e| AppError::Config(format!("读取 {} 失败: {}", path.display(), e)))?;
    Ok(Some(v))
}

fn count_rows(conn: &rusqlite::Connection, sql: &str) -> AppResult<usize> {
    let n: i64 = conn.query_row(sql, [], |r| r.get(0))?;
    Ok(n as usize)
}

// ===========================================================================
// 预览
// ===========================================================================

/// 解密备份文件并返回元数据（条目数 / 是否含凭据），不写入任何数据。
///
/// 导入前调用：既校验备份密码，也向用户展示文件内容。
///
/// async + spawn_blocking：整文件读取 + Argon2 派生 + AES-GCM 解密是重操作，
/// 且前端每次点"预览"都会触发，不能在主线程执行。
#[tauri::command]
pub async fn backup_inspect(path: String, password: String) -> AppResult<BackupInfo> {
    tauri::async_runtime::spawn_blocking(move || {
        let content = std::fs::read_to_string(&path)?;
        let (file, payload) = decrypt_full(&content, &password)?;
        Ok(BackupInfo {
            version: file.version,
            app_version: file.app_version,
            created_at: file.created_at,
            counts: BackupCounts::from_payload(&payload),
            has_credentials: !payload.credentials.is_empty(),
            has_totp: !payload.totp_secrets.is_empty(),
        })
    })
    .await
    .map_err(|e| AppError::Ssh(format!("备份预览任务失败: {}", e)))?
}

// ===========================================================================
// 导入
// ===========================================================================

/// 从加密备份文件导入数据。
///
/// - `mode = "merge"`（默认）：按 id upsert，保留本机已有数据；
/// - `mode = "overwrite"`：清空全部相关表后写入（同一事务，失败自动回滚）。
///
/// 文件包含凭据 / TOTP 且保险库未解锁时**报错中止**（不会静默丢弃凭据）。
///
/// `force`：备份不含任何凭据/TOTP 且用覆盖模式时，会清空本机全部凭据与 TOTP
/// 且无法从备份恢复（多半是导出时保险库未解锁导致的残缺备份）。此时默认报错，
/// 前端弹二次确认后传 `force = true` 才放行。
#[tauri::command]
pub async fn backup_import(
    path: String,
    password: String,
    mode: String,
    force: bool,
    state: State<'_, AppState>,
) -> AppResult<BackupSummary> {
    // async + spawn_blocking：解密 + 事务写入 + 配置文件写回均为重操作。
    let app = state.app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        backup_import_inner(&path, &password, &mode, force, &state)
    })
    .await
    .map_err(|e| AppError::Ssh(format!("备份导入任务失败: {}", e)))?
}

/// [`backup_import`] 的执行主体（阻塞线程池内运行）。
fn backup_import_inner(
    path: &str,
    password: &str,
    mode: &str,
    force: bool,
    state: &AppState,
) -> AppResult<BackupSummary> {
    let content = std::fs::read_to_string(path)?;
    let payload = decrypt_payload(&content, password)?;
    let overwrite = mode == "overwrite";

    // 凭据 / TOTP 需要保险库：先校验，未解锁直接中止。
    let has_secrets = !payload.credentials.is_empty() || !payload.totp_secrets.is_empty();

    // 覆盖模式 + 无凭据备份 + 未强制确认：本机凭据会被静默清空，先拦截。
    if overwrite && !has_secrets && !force {
        let conn = state.conn()?;
        let cred_count = count_rows(&conn, "SELECT COUNT(*) FROM credentials")?;
        let totp_count = count_rows(&conn, "SELECT COUNT(*) FROM totp_secrets")?;
        if cred_count > 0 || totp_count > 0 {
            return Err(AppError::InvalidInput(format!(
                "该备份不包含凭据/TOTP（导出时保险库可能未解锁），覆盖导入将清空本机现有的 {} 条凭据、{} 条 TOTP 且无法恢复，请确认后重试",
                cred_count, totp_count
            )));
        }
    }

    if has_secrets && !state.vault_ready() {
        return Err(AppError::Auth(
            "备份文件包含凭据（密码/私钥），请先创建或解锁凭据保险库再导入".into(),
        ));
    }
    let vault = if has_secrets {
        let guard = state.vault_read()?;
        let v = guard
            .as_ref()
            .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?
            .clone();
        drop(guard);
        Some(v)
    } else {
        None
    };

    // 事务内写入全部表（覆盖模式失败自动回滚，不会留下半套数据）。
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    apply_payload(&tx, &payload, overwrite, vault.as_ref())?;
    tx.commit()?;
    drop(conn);

    // settings.json / mcp.json 原子写回（不影响正在运行的内存配置，重启后生效）。
    if let Some(settings) = &payload.settings {
        crate::storage::json_store::write_json(
            &state.settings_path.join(SETTINGS_FILENAME),
            settings,
        )?;
    }
    if let Some(mcp) = &payload.mcp {
        crate::storage::json_store::write_json(
            &state.settings_path.join(MCP_CONFIG_FILENAME),
            mcp,
        )?;
    }

    Ok(BackupSummary {
        path: path.to_string(),
        counts: BackupCounts::from_payload(&payload),
        credentials_skipped: 0,
        totp_skipped: 0,
        overwritten: overwrite,
    })
}

/// 把载荷写入数据库（事务内执行）。
///
/// `overwrite = true` 时先清空相关表；凭据 / TOTP 用目标机保险库重新加密。
fn apply_payload(
    tx: &Transaction<'_>,
    payload: &BackupPayload,
    overwrite: bool,
    vault: Option<&CredentialVault>,
) -> AppResult<()> {
    if overwrite {
        for table in [
            "totp_secrets",
            "file_accounts",
            "desktops",
            "forward_rules",
            "db_groups",
            "db_profiles",
            "credentials",
            "groups",
            "sessions",
        ] {
            tx.execute(&format!("DELETE FROM {}", table), [])?;
        }
    }

    // 凭据 / TOTP：先重加密（密钥在内存中，同步代码直接使用）。
    let enc_credentials: Vec<(String, String, String, String, String)> = payload
        .credentials
        .iter()
        .map(|c| {
            let vault = vault.ok_or_else(|| {
                AppError::Auth("保险库未解锁，无法导入凭据".into())
            })?;
            let plain = serde_json::json!({
                "kind": c.kind,
                "value": c.value,
                "passphrase": c.passphrase,
            });
            let blob = vault.encrypt_str(&serde_json::to_string(&plain)?)?;
            Ok((
                c.id.clone(),
                c.name.clone(),
                CredentialVault::encode_blob(&blob)?,
                c.created_at.clone(),
                c.kind.clone(),
            ))
        })
        .collect::<AppResult<Vec<_>>>()?;

    let enc_totp: Vec<(String, String, String, String, String, i64, i64, i64, String)> = payload
        .totp_secrets
        .iter()
        .map(|t| {
            let vault = vault.ok_or_else(|| {
                AppError::Auth("保险库未解锁，无法导入 TOTP".into())
            })?;
            let blob = vault.encrypt_str(&t.secret)?;
            Ok((
                t.id.clone(),
                t.issuer.clone(),
                t.account.clone(),
                CredentialVault::encode_blob(&blob)?,
                t.algorithm.clone(),
                t.digits as i64,
                t.period as i64,
                t.sort_order,
                t.created_at.clone(),
            ))
        })
        .collect::<AppResult<Vec<_>>>()?;

    // sessions
    for s in &payload.sessions {
        tx.execute(
            "INSERT INTO sessions (id, name, group_id, host, port, username, auth_type, \
             credential_id, key_path, jump_session_id, startup_script, tags, color, \
             sort_order, created_at, updated_at, protocol, space_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18) \
             ON CONFLICT(id) DO UPDATE SET \
                name = excluded.name, group_id = excluded.group_id, host = excluded.host, \
                port = excluded.port, username = excluded.username, \
                auth_type = excluded.auth_type, credential_id = excluded.credential_id, \
                key_path = excluded.key_path, jump_session_id = excluded.jump_session_id, \
                startup_script = excluded.startup_script, tags = excluded.tags, \
                color = excluded.color, sort_order = excluded.sort_order, \
                updated_at = excluded.updated_at, protocol = excluded.protocol, \
                space_id = excluded.space_id",
            rusqlite::params![
                s.id, s.name, s.group_id, s.host, s.port as i64, s.username,
                s.auth_type.as_str(), s.credential_id, s.key_path, s.jump_session_id,
                s.startup_script, s.tags, s.color, s.sort_order, s.created_at,
                s.updated_at, s.protocol, s.space_id,
            ],
        )?;
    }

    // groups
    for g in &payload.groups {
        tx.execute(
            "INSERT INTO groups (id, name, parent_id, sort_order, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(id) DO UPDATE SET \
                name = excluded.name, parent_id = excluded.parent_id, \
                sort_order = excluded.sort_order",
            rusqlite::params![g.id, g.name, g.parent_id, g.sort_order, g.created_at],
        )?;
    }

    // credentials
    for (id, name, enc_data, created_at, kind) in &enc_credentials {
        tx.execute(
            "INSERT INTO credentials (id, name, enc_data, created_at, kind) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(id) DO UPDATE SET \
                name = excluded.name, enc_data = excluded.enc_data, kind = excluded.kind",
            rusqlite::params![id, name, enc_data, created_at, kind],
        )?;
    }

    // db_profiles
    for p in &payload.db_profiles {
        tx.execute(
            "INSERT OR REPLACE INTO db_profiles \
             (id, name, kind, host, port, username, default_database, \
              credential_id, ssh_session_config_id, group_id, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                p.id, p.name, p.kind, p.host, p.port as i64, p.username,
                p.default_database, p.credential_id, p.ssh_session_config_id,
                p.group_id, p.created_at,
            ],
        )?;
    }

    // db_groups
    for g in &payload.db_groups {
        tx.execute(
            "INSERT OR REPLACE INTO db_groups (id, name, parent_id, sort_order, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![g.id, g.name, g.parent_id, g.sort_order, g.created_at],
        )?;
    }

    // forward_rules
    for r in &payload.forward_rules {
        tx.execute(
            "INSERT INTO forward_rules (id, name, session_id, kind, local_host, local_port, \
             remote_host, remote_port, auto_start, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(id) DO UPDATE SET \
                name = excluded.name, session_id = excluded.session_id, \
                kind = excluded.kind, local_host = excluded.local_host, \
                local_port = excluded.local_port, remote_host = excluded.remote_host, \
                remote_port = excluded.remote_port, auto_start = excluded.auto_start",
            rusqlite::params![
                r.id, r.name, r.session_id, r.kind, r.local_host, r.local_port as i64,
                r.remote_host, r.remote_port as i64, r.auto_start as i64, r.created_at,
            ],
        )?;
    }

    // desktops
    for d in &payload.desktops {
        tx.execute(
            "INSERT INTO desktops (id, name, protocol, host, port, username, credential_id, \
             sort_order, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(id) DO UPDATE SET \
                name = excluded.name, protocol = excluded.protocol, host = excluded.host, \
                port = excluded.port, username = excluded.username, \
                credential_id = excluded.credential_id, sort_order = excluded.sort_order, \
                updated_at = excluded.updated_at",
            rusqlite::params![
                d.id, d.name, d.protocol, d.host, d.port as i64, d.username,
                d.credential_id, d.sort_order, d.created_at, d.updated_at,
            ],
        )?;
    }

    // totp_secrets
    for (id, issuer, account, enc_secret, algorithm, digits, period, sort_order, created_at) in
        &enc_totp
    {
        tx.execute(
            "INSERT INTO totp_secrets (id, issuer, account, enc_secret, algorithm, digits, \
             period, sort_order, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(id) DO UPDATE SET \
                issuer = excluded.issuer, account = excluded.account, \
                enc_secret = excluded.enc_secret, algorithm = excluded.algorithm, \
                digits = excluded.digits, period = excluded.period, \
                sort_order = excluded.sort_order",
            rusqlite::params![id, issuer, account, enc_secret, algorithm, digits, period, sort_order, created_at],
        )?;
    }

    // file_accounts
    for a in &payload.file_accounts {
        tx.execute(
            "INSERT INTO file_accounts (id, name, kind, endpoint, region, bucket, \
             credential_id, path_style, sort_order, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
             ON CONFLICT(id) DO UPDATE SET \
                name = excluded.name, kind = excluded.kind, endpoint = excluded.endpoint, \
                region = excluded.region, bucket = excluded.bucket, \
                credential_id = excluded.credential_id, path_style = excluded.path_style, \
                sort_order = excluded.sort_order, updated_at = excluded.updated_at",
            rusqlite::params![
                a.id, a.name, a.kind, a.endpoint, a.region, a.bucket, a.credential_id,
                a.path_style as i64, a.sort_order, a.created_at, a.updated_at,
            ],
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::run_migrations;
    use rusqlite::params;

    /// 在临时目录创建 vault 并返回。
    fn test_vault(dir: &std::path::Path) -> CredentialVault {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        CredentialVault::create(dir, "vault pass").unwrap()
    }

    /// 在内存库中插入一条凭据（明文密码为 `value`）。
    fn insert_credential(conn: &rusqlite::Connection, vault: &CredentialVault, id: &str, value: &str) {
        let plain = format!(r#"{{"kind":"password","value":"{}","passphrase":null}}"#, value);
        let blob = vault.encrypt_str(&plain).unwrap();
        let enc = CredentialVault::encode_blob(&blob).unwrap();
        conn.execute(
            "INSERT INTO credentials (id, name, enc_data, created_at, kind) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, "测试凭据", enc, "2026-01-01T00:00:00Z", "password"],
        )
        .unwrap();
    }

    /// 跨机迁移链路：机 A 导出明文 → 机 B 用新 vault 重加密导入 → 读回一致。
    #[test]
    fn export_then_import_on_new_machine() {
        // 机 A：库 + vault + 一条凭据。
        let mut conn_a = rusqlite::Connection::open_in_memory().unwrap();
        run_migrations(&conn_a).unwrap();
        let dir_a = std::env::temp_dir().join("xterm_backup_test_a");
        let vault_a = test_vault(&dir_a);
        insert_credential(&conn_a, &vault_a, "c1", "hunter2");
        insert_credential(&conn_a, &vault_a, "c2", "hunter2");

        // 机 A 导出明文凭据。
        let creds = export_credentials(&conn_a, &vault_a).unwrap();
        assert_eq!(creds.len(), 2);
        assert_eq!(creds[0].value, "hunter2");

        // 机 B：全新库 + 新 vault（模拟换机，没有 A 的主密钥）。
        let mut conn_b = rusqlite::Connection::open_in_memory().unwrap();
        run_migrations(&conn_b).unwrap();
        let dir_b = std::env::temp_dir().join("xterm_backup_test_b");
        let vault_b = test_vault(&dir_b);

        // 机 B 已有一条同 id（c1）的旧凭据 → 合并导入时应更新而不是新增。
        insert_credential(&conn_b, &vault_b, "c1", "old-value");

        // 机 B 导入（合并模式）。
        let payload = BackupPayload {
            credentials: creds,
            ..Default::default()
        };
        let tx = conn_b.transaction().unwrap();
        apply_payload(&tx, &payload, false, Some(&vault_b)).unwrap();
        tx.commit().unwrap();

        // 机 B 读回：用 B 自己的 vault 解密，明文一致，且条目数没有增加。
        let creds_b = export_credentials(&conn_b, &vault_b).unwrap();
        assert_eq!(creds_b.len(), 2);
        assert!(creds_b.iter().all(|c| c.value == "hunter2"), "同 id 条目应被覆盖更新");

        // 覆盖模式：清空后只剩新数据。
        let updated = BackupPayload {
            credentials: vec![PlainCredential {
                id: "c1".into(),
                name: "改名".into(),
                kind: "password".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                value: "new-secret".into(),
                passphrase: None,
            }],
            ..Default::default()
        };
        let tx = conn_b.transaction().unwrap();
        apply_payload(&tx, &updated, true, Some(&vault_b)).unwrap();
        tx.commit().unwrap();
        let creds_b = export_credentials(&conn_b, &vault_b).unwrap();
        assert_eq!(creds_b.len(), 1);
        assert_eq!(creds_b[0].value, "new-secret");

        std::fs::remove_dir_all(&dir_a).ok();
        std::fs::remove_dir_all(&dir_b).ok();
    }

    /// 未解锁（vault 为 None）时导入含凭据的载荷应报错而不是 panic。
    #[test]
    fn import_credentials_without_vault_fails() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let payload = BackupPayload {
            credentials: vec![PlainCredential {
                id: "c1".into(),
                name: "x".into(),
                kind: "password".into(),
                created_at: "".into(),
                value: "secret".into(),
                passphrase: None,
            }],
            ..Default::default()
        };
        let tx = conn.transaction().unwrap();
        let err = apply_payload(&tx, &payload, false, None).unwrap_err();
        assert!(matches!(err, AppError::Auth(_)));
    }
}
