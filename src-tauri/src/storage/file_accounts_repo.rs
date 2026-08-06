//! 文件账号（file_accounts）的 CRUD 与 S3 凭据读取。
//!
//! 一条 file_account 记录一个远程文件存储连接（当前仅 S3 / 兼容存储）。
//! 凭据（access_key / secret_key）复用 `credentials` 表（由 `credential_save` 写入），
//! blob 明文 JSON 为两层结构：外层 `{"kind":"s3_credential","value":"<内层JSON>"}`，
//! 内层 value 为 `{"access_key":"...","secret_key":"..."}`。
//! 通过 [`fetch_s3_credential`] 解密读取（范式同 `database::mysql::fetch_mysql_password`）。

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::storage::db::DbConn;
use crate::storage::secure::CredentialVault;

// ===========================================================================
// 数据模型
// ===========================================================================

/// 一个文件账号配置（对应 file_accounts 表一行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileAccount {
    pub id: String,
    pub name: String,
    /// 后端种类：当前固定 `"s3"`。
    pub kind: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub credential_id: Option<String>,
    /// 寻址风格：true=path-style（默认），false=virtual-hosted-style。
    /// 带端口/路径前缀的自定义 endpoint（MinIO 等）应保持 true。
    #[serde(default = "default_path_style")]
    pub path_style: bool,
    pub sort_order: i64,
    pub created_at: String,
    pub updated_at: String,
}

fn default_path_style() -> bool {
    true
}

/// `credentials.enc_data` 解密后的外层 JSON 结构（credential_save 统一格式）。
///
/// S3 凭据约定：`kind = "s3_credential"`，`value` 为 JSON 字符串
/// `{"access_key":"...","secret_key":"..."}`（复用 credential_save，避免新增命令）。
#[derive(Debug, Deserialize)]
struct CredentialPayload {
    kind: String,
    value: String,
}

/// S3 凭据内层 JSON（存在 CredentialPayload.value 里）。
#[derive(Debug, Deserialize)]
struct S3CredentialValue {
    access_key: String,
    secret_key: String,
}

// ===========================================================================
// CRUD
// ===========================================================================

/// 列出所有文件账号，按 `sort_order`、`name` 升序排列。
pub fn list_file_accounts(conn: &DbConn) -> AppResult<Vec<FileAccount>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, endpoint, region, bucket, credential_id, \
         path_style, sort_order, created_at, updated_at \
         FROM file_accounts ORDER BY sort_order ASC, name ASC",
    )?;
    let rows = stmt.query_map([], row_to_account)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 根据 id 取单个文件账号；不存在返回 `None`。
pub fn get_file_account(conn: &DbConn, id: &str) -> AppResult<Option<FileAccount>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, endpoint, region, bucket, credential_id, \
         path_style, sort_order, created_at, updated_at \
         FROM file_accounts WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map([id], row_to_account)?;
    if let Some(r) = rows.next() {
        Ok(Some(r?))
    } else {
        Ok(None)
    }
}

/// 新增或更新文件账号（UPSERT）。
pub fn upsert_file_account(conn: &DbConn, a: &FileAccount) -> AppResult<()> {
    conn.execute(
        "INSERT INTO file_accounts (id, name, kind, endpoint, region, bucket, \
         credential_id, path_style, sort_order, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
         ON CONFLICT(id) DO UPDATE SET \
         name=excluded.name, kind=excluded.kind, endpoint=excluded.endpoint, \
         region=excluded.region, bucket=excluded.bucket, \
         credential_id=excluded.credential_id, path_style=excluded.path_style, \
         sort_order=excluded.sort_order, updated_at=excluded.updated_at",
        params![
            a.id,
            a.name,
            a.kind,
            a.endpoint,
            a.region,
            a.bucket,
            a.credential_id,
            a.path_style as i64,
            a.sort_order,
            a.created_at,
            a.updated_at,
        ],
    )?;
    Ok(())
}

/// 删除文件账号（按 id）。
pub fn delete_file_account(conn: &DbConn, id: &str) -> AppResult<()> {
    conn.execute("DELETE FROM file_accounts WHERE id = ?1", params![id])?;
    Ok(())
}

/// 从 `credentials` 表取出指定 id 的加密 blob，解密并解析为 S3 凭据。
///
/// 约定凭据由 `credential_save` 写入：`kind = "s3_credential"`，`value` 为 JSON
/// 字符串 `{"access_key":"...","secret_key":"..."}`。返回 `(access_key, secret_key)`。
pub fn fetch_s3_credential(
    conn: &DbConn,
    cred_id: &str,
    vault: &CredentialVault,
) -> AppResult<(String, String)> {
    let enc_data: String = match conn.query_row(
        "SELECT enc_data FROM credentials WHERE id = ?1",
        params![cred_id],
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
    let payload: CredentialPayload = serde_json::from_str(&plain)
        .map_err(|e| AppError::Auth(format!("解析 S3 凭据 payload 失败: {}", e)))?;
    if payload.kind != "s3_credential" {
        return Err(AppError::Auth(format!(
            "凭据类型不匹配：期望 s3_credential，实际 {}",
            payload.kind
        )));
    }
    let value: S3CredentialValue = serde_json::from_str(&payload.value)
        .map_err(|e| AppError::Auth(format!("解析 S3 凭据 value 失败: {}", e)))?;
    Ok((value.access_key, value.secret_key))
}

// ===========================================================================
// 辅助
// ===========================================================================

/// 把一行映射为 [`FileAccount`]。
fn row_to_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileAccount> {
    let path_style: i64 = row.get(7)?;
    Ok(FileAccount {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        endpoint: row.get(3)?,
        region: row.get(4)?,
        bucket: row.get(5)?,
        credential_id: row.get(6)?,
        path_style: path_style != 0,
        sort_order: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}
