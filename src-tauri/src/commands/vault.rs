//! 凭据保险库（vault）相关命令。
//!
//! 负责保险库的创建、解锁、状态查询，以及凭据（密码/私钥文本）的保存、读取、删除。
//!
//! 凭据在数据库中以加密 blob 形式存于 `credentials` 表，明文 JSON 结构见
//! [`CredentialInput`]。

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::storage::secure::CredentialVault;

/// 凭据保险库是否已存在（用户是否设置过主密码）。
#[tauri::command]
pub fn vault_exists(state: State<'_, AppState>) -> AppResult<bool> {
    Ok(CredentialVault::exists(&state.data_dir))
}

/// 首次创建保险库（设置主密码）。
///
/// 要求保险库尚不存在。
#[tauri::command]
pub fn vault_create(passphrase: String, state: State<'_, AppState>) -> AppResult<()> {
    if CredentialVault::exists(&state.data_dir) {
        return Err(AppError::InvalidInput("保险库已存在，请使用解锁功能".into()));
    }
    if passphrase.len() < 6 {
        return Err(AppError::InvalidInput("主密码至少 6 位".into()));
    }
    let vault = CredentialVault::create(&state.data_dir, &passphrase)?;
    state.set_vault(vault);
    Ok(())
}

/// 用主密码解锁已存在的保险库。
#[tauri::command]
pub fn vault_unlock(passphrase: String, state: State<'_, AppState>) -> AppResult<()> {
    let vault = CredentialVault::unlock(&state.data_dir, &passphrase)?;
    state.set_vault(vault);
    Ok(())
}

/// 查询保险库是否已解锁（运行时状态）。
#[tauri::command]
pub fn vault_unlocked(state: State<'_, AppState>) -> AppResult<bool> {
    Ok(state.vault_ready())
}

// ---------------------------------------------------------------------------
// 凭据 CRUD
// ---------------------------------------------------------------------------

/// 前端提交的凭据输入。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInput {
    /// 可选 id；为空时自动生成。
    pub id: Option<String>,
    /// 显示名称。
    pub name: String,
    /// `"password"` 或 `"private_key_text"`。
    pub kind: String,
    /// 明文内容。
    pub value: String,
    /// 私钥的 passphrase（密码类型忽略）。
    #[serde(default)]
    pub passphrase: Option<String>,
}

/// 凭据查询返回（不含明文）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialView {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub created_at: String,
}

/// 保存一条凭据（加密后入库）。返回凭据 id。
#[tauri::command]
pub fn credential_save(input: CredentialInput, state: State<'_, AppState>) -> AppResult<String> {
    // 取保险库（持锁到序列化完成为止）。
    let vault_guard = state.vault_read()?;
    let vault = vault_guard
        .as_ref()
        .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?;

    // 构造明文 payload。
    let payload = serde_json::json!({
        "kind": input.kind,
        "value": input.value,
        "passphrase": input.passphrase,
    });
    let payload_str = serde_json::to_string(&payload)?;
    let blob = vault.encrypt_str(&payload_str)?;
    let enc_data = CredentialVault::encode_blob(&blob);

    let id = input.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let now = chrono::Utc::now().to_rfc3339();
    drop(vault_guard); // 释放锁再操作 DB。

    let conn = state.conn()?;
    conn.execute(
        "INSERT INTO credentials (id, name, enc_data, created_at, kind) VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, enc_data = excluded.enc_data, kind = excluded.kind",
        rusqlite::params![id, input.name, enc_data, now, input.kind],
    )?;
    Ok(id)
}

/// 列出所有凭据（不含明文，直接读 DB 的 kind 列，不解密）。
#[tauri::command]
pub fn credential_list(state: State<'_, AppState>) -> AppResult<Vec<CredentialView>> {
    let conn = state.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, created_at FROM credentials ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        let kind: String = r.get(2).unwrap_or_else(|_| "password".into());
        Ok(CredentialView {
            id: r.get(0)?,
            name: r.get(1)?,
            kind,
            created_at: r.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 重命名凭据（仅改名称，不动密文）。
#[tauri::command]
pub fn credential_rename(id: String, name: String, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    conn.execute(
        "UPDATE credentials SET name = ?1 WHERE id = ?2",
        rusqlite::params![name, id],
    )?;
    Ok(())
}

/// 取得一条凭据的明文（仅用于内部测试 / 高级场景；前端 UI 通常不需要）。
#[tauri::command]
pub fn credential_get(id: String, state: State<'_, AppState>) -> AppResult<String> {
    let vault_guard = state.vault_read()?;
    let vault = vault_guard
        .as_ref()
        .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?;

    let conn = state.conn()?;
    let enc_data = match conn.query_row(
        "SELECT enc_data FROM credentials WHERE id = ?1",
        [&id],
        |r| r.get::<_, String>(0),
    ) {
        Ok(s) => s,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(AppError::NotFound(format!("凭据 {} 不存在", id)));
        }
        Err(e) => return Err(e.into()),
    };
    let blob = CredentialVault::decode_blob(&enc_data)?;
    let plain = vault.decrypt_str(&blob)?;
    Ok(plain)
}

/// 删除一条凭据。
#[tauri::command]
pub fn credential_delete(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    conn.execute("DELETE FROM credentials WHERE id = ?1", [&id])?;
    Ok(())
}
