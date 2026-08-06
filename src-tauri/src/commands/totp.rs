//! TOTP / MFA 验证码管理命令。
//!
//! 这些 `#[tauri::command]` 暴露给前端：
//! - [`totp_list`]：列出所有已存的 TOTP 配置（不含 secret）；
//! - [`totp_add`]：新增一条 TOTP（支持 base32 secret 或 `otpauth://` URI 自动识别）；
//! - [`totp_delete`]：删除一条；
//! - [`totp_generate`]：为已存的 entry 实时生成当前验证码（每次实时，不缓存）；
//! - [`totp_generate_for_secret`]：临时生成（添加对话框预览用，不入库）；
//! - [`totp_fill_terminal`]：把当前验证码作为字节写入指定终端会话的 PTY（不追加换行）。
//!
//! secret 在数据库中始终以 vault 加密 blob 存储；明文仅在内存中短暂存在。

use serde::Deserialize;
use tauri::State;

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::storage::secure::CredentialVault;
use crate::totp;

/// 前端提交的新增 TOTP 输入。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TotpAddInput {
    pub issuer: String,
    pub account: String,
    /// base32 secret 或完整的 `otpauth://` URI（自动识别）。
    pub secret: String,
    #[serde(default)]
    pub algorithm: Option<String>,
    #[serde(default)]
    pub digits: Option<u32>,
    #[serde(default)]
    pub period: Option<u32>,
}

/// 列出所有 TOTP 配置（不含 secret），按 sort_order / issuer / account 排序。
#[tauri::command]
pub fn totp_list(state: State<'_, AppState>) -> AppResult<Vec<totp::TotpEntry>> {
    let conn = state.conn()?;
    totp::list_totp(&conn)
}

/// 新增一条 TOTP 配置。
///
/// - 若 `secret` 以 `otpauth://` 开头，按 URI 解析（忽略 input 中除 secret 外的字段，
///   除非 URI 未提供则回退到 input）；
/// - 否则按 input 字段构造，缺省值：algorithm=SHA1, digits=6, period=30；
/// - 入库前先 `generate_now` 校验 secret 可用，失败返回 InvalidInput；
/// - 通过 vault 加密 secret 后入库。返回不含 secret 的 entry。
#[tauri::command]
pub fn totp_add(input: TotpAddInput, state: State<'_, AppState>) -> AppResult<totp::TotpEntry> {
    // 1. 构造 entry + 待加密的明文 secret。
    let (mut entry, plain_secret) = if input.secret.trim_start().starts_with("otpauth://") {
        let parsed = totp::parse_otpauth_uri(&input.secret)?;
        // URI 优先；若 URI 没给 issuer/account 而 input 给了，用 input 补。
        let issuer = if parsed.issuer.is_empty() {
            input.issuer.clone()
        } else {
            parsed.issuer
        };
        let account = if parsed.account.is_empty() {
            input.account.clone()
        } else {
            parsed.account
        };
        // 从 URI 中提取 secret 原文（解析时只校验存在，这里再取一次）。
        let secret_str = extract_secret_from_uri(&input.secret)
            .ok_or_else(|| AppError::InvalidInput("otpauth URI 缺少 secret 参数".into()))?;
        (
            totp::TotpEntry {
                id: String::new(),
                issuer,
                account,
                algorithm: parsed.algorithm,
                digits: parsed.digits,
                period: parsed.period,
                sort_order: 0,
                created_at: String::new(),
            },
            secret_str,
        )
    } else {
        let algorithm = input
            .algorithm
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "SHA1".into());
        let digits = input.digits.unwrap_or(6);
        let period = input.period.unwrap_or(30);
        (
            totp::TotpEntry {
                id: String::new(),
                issuer: input.issuer.clone(),
                account: input.account.clone(),
                algorithm,
                digits,
                period,
                sort_order: 0,
                created_at: String::new(),
            },
            input.secret.clone(),
        )
    };

    // 2. 校验 secret 可生成码。
    totp::generate_now(&plain_secret, &entry.algorithm, entry.digits, entry.period)
        .map_err(|_| AppError::InvalidInput("无效的 TOTP secret：无法生成验证码".into()))?;

    // 3. vault 加密。
    let vault_guard = state.vault_read()?;
    let vault = vault_guard
        .as_ref()
        .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?;
    let blob = vault.encrypt_str(&plain_secret)?;
    let enc_secret = CredentialVault::encode_blob(&blob)?;
    drop(vault_guard); // 释放读锁后再操作 DB。

    // 4. 生成 id 并入库。
    entry.id = uuid::Uuid::new_v4().to_string();
    entry.created_at = chrono::Utc::now().to_rfc3339();

    let conn = state.conn()?;
    totp::upsert_totp(&conn, &entry, &enc_secret)?;

    Ok(entry)
}

/// 删除一条 TOTP 配置。
#[tauri::command]
pub fn totp_delete(id: String, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    totp::delete_totp(&conn, &id)
}

/// 为已存的 entry 实时生成当前验证码（每次实时，不缓存）。
#[tauri::command]
pub fn totp_generate(id: String, state: State<'_, AppState>) -> AppResult<totp::TotpCode> {
    // 取 entry + enc_secret。
    let conn = state.conn()?;
    let (entry, enc_secret) = totp::get_totp_secret_enc(&conn, &id)?;
    drop(conn); // 释放连接，避免持锁过久。

    // 解密 secret。
    let vault_guard = state.vault_read()?;
    let vault = vault_guard
        .as_ref()
        .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?;
    let blob = CredentialVault::decode_blob(&enc_secret)?;
    let plain_secret = vault.decrypt_str(&blob)?;
    drop(vault_guard);

    // 实时生成。
    totp::generate_now(&plain_secret, &entry.algorithm, entry.digits, entry.period)
}

/// 临时生成验证码（不存库）——添加对话框中预览用。
///
/// 纯函数：不需要 vault / DB。
#[tauri::command]
pub fn totp_generate_for_secret(
    secret: String,
    algorithm: String,
    digits: u32,
    period: u32,
) -> AppResult<totp::TotpCode> {
    totp::generate_now(&secret, &algorithm, digits, period)
}

/// 把当前验证码作为字节写入指定终端会话的 PTY。
///
/// 不追加换行（让用户决定是否回车，某些密码框要求只填不提交）。
/// 找不到 `instance_id` 对应的会话或写入失败时返回错误。
#[tauri::command]
pub async fn totp_fill_terminal(
    id: String,
    instance_id: String,
    state: State<'_, AppState>,
) -> AppResult<()> {
    // 1. 生成当前码（复用 totp_generate 的逻辑）。
    let (entry, enc_secret) = {
        let conn = state.conn()?;
        totp::get_totp_secret_enc(&conn, &id)?
    };
    let plain_secret = {
        let vault_guard = state.vault_read()?;
        let vault = vault_guard
            .as_ref()
            .ok_or_else(|| AppError::Auth("保险库未解锁".into()))?;
        let blob = CredentialVault::decode_blob(&enc_secret)?;
        vault.decrypt_str(&blob)?
    };
    let code = totp::generate_now(&plain_secret, &entry.algorithm, entry.digits, entry.period)?;

    // 2. 找到终端会话并写入。
    let terminals = state.terminals.lock();
    match terminals.get(&instance_id) {
        Some(session) => session.write(code.code.into_bytes()),
        None => Err(AppError::NotFound(format!(
            "找不到终端会话: {}",
            instance_id
        ))),
    }
}

// ===========================================================================
// 内部辅助
// ===========================================================================

/// 从 `otpauth://` URI 的 query 中提取 `secret` 参数原值。
///
/// [`totp::parse_otpauth_uri`] 解析后会丢弃 secret（出于安全考虑不放入 entry），
/// 这里单独提取用于后续加密入库。
fn extract_secret_from_uri(uri: &str) -> Option<String> {
    let after_query = uri.split_once('?').map(|(_, q)| q)?;
    for part in after_query.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            if k.eq_ignore_ascii_case("secret") {
                return Some(percent_decode_uri(v));
            }
        }
    }
    None
}

/// 对 URI query value 做简单的百分号解码（与 totp::percent_decode 行为一致）。
fn percent_decode_uri(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
