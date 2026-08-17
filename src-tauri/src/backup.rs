//! 数据备份（导出/导入）核心模块：基于口令的加密文件格式。
//!
//! 设计要点：
//! - 备份文件使用**用户独立输入的备份密码**加密（与保险库主密码相互独立），
//!   这样备份可以安全地迁移到另一台机器（新机器没有旧主密钥也能解密）。
//! - 密钥由 Argon2id 从备份密码 + **随机 salt** 派生（32 字节），明文载荷用
//!   AES-256-GCM 加密。外层 JSON 只含元数据（格式版本、时间戳、KDF 参数），
//!   全部用户数据都在密文中。
//! - 加密实现与 [`crate::storage::secure`] 同源（aes-gcm 0.10 + argon2 0.5），
//!   不引入额外依赖。
//!
//! 文件结构（`.xtermbackup`，整体是一个 JSON 文本文件）：
//! ```json
//! {
//!   "format": "x-term-backup",
//!   "version": 1,
//!   "appVersion": "0.1.15",
//!   "createdAt": "2026-08-08T12:00:00Z",
//!   "kdf": { "algorithm": "argon2id", "salt": "<base64>", "m": 19456, "t": 2, "p": 1 },
//!   "nonce": "<base64 12B>",
//!   "ciphertext": "<base64>"
//! }
//! ```

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::error::{AppError, AppResult};

/// 备份文件格式标识。
pub const BACKUP_FORMAT: &str = "x-term-backup";
/// 备份文件格式版本。
pub const BACKUP_VERSION: u32 = 1;

/// Argon2id 派生参数：与保险库主密钥派生一致（m = 19456 KiB, t = 2, p = 1）。
const KDF_M: u32 = 19456;
const KDF_T: u32 = 2;
const KDF_P: u32 = 1;

/// 解密时接受的 KDF 参数上限。
///
/// 解密参数来自备份文件（攻击者可控）：不做上限的话，构造 m=数 GiB 的恶意
/// `.xtermbackup` 文件即可让应用在密码校验前分配数 GB 内存 → OOM/崩溃
/// （本地 DoS）。上限远高于正常导出值（m=19456, t=2, p=1），合法备份不受影响。
const KDF_M_MAX: u32 = 1024 * 1024; // 1 GiB
const KDF_T_MAX: u32 = 10;
const KDF_P_MAX: u32 = 4;

// ===========================================================================
// 加密文件结构
// ===========================================================================

/// 备份文件外层结构（明文元数据 + 密文）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupFile {
    /// 固定 `"x-term-backup"`，用于识别文件类型。
    pub format: String,
    /// 格式版本号。
    pub version: u32,
    /// 导出时的应用版本。
    pub app_version: String,
    /// 导出时间（RFC3339）。
    pub created_at: String,
    /// 密钥派生参数（解密时必须使用同一参数）。
    pub kdf: KdfParams,
    /// AES-256-GCM 的 12 字节随机 nonce。
    pub nonce: [u8; 12],
    /// 密文（含 GCM 认证标签），即加密后的 [`BackupPayload`] JSON。
    pub ciphertext: Vec<u8>,
}

/// Argon2id 派生参数（随文件保存，便于未来调整参数后仍能解密旧文件）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KdfParams {
    /// 恒为 `"argon2id"`。
    pub algorithm: String,
    /// 派生密钥用的随机 salt（16 字节）。
    pub salt: Vec<u8>,
    /// 内存成本（KiB）。
    pub m: u32,
    /// 迭代次数。
    pub t: u32,
    /// 并行度。
    pub p: u32,
}

// ===========================================================================
// 明文载荷结构
// ===========================================================================

/// 解密后的完整备份载荷（所有用户数据）。
///
/// 凭据 / TOTP secret 在此处以**明文**形式存在——它们整体受备份文件的
/// AES-256-GCM 加密保护；导出时由本机保险库解密，导入时用目标机保险库重新加密。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupPayload {
    pub sessions: Vec<crate::storage::sessions_repo::Session>,
    pub groups: Vec<crate::storage::sessions_repo::Group>,
    /// 凭据明文（密码 / 私钥文本）。
    #[serde(default)]
    pub credentials: Vec<PlainCredential>,
    pub db_profiles: Vec<crate::database::profiles::DbProfile>,
    pub db_groups: Vec<crate::database::profiles::DbGroup>,
    pub forward_rules: Vec<crate::commands::forward::ForwardRule>,
    pub desktops: Vec<crate::storage::desktops_repo::Desktop>,
    /// TOTP 配置明文（含 secret）。
    #[serde(default)]
    pub totp_secrets: Vec<PlainTotp>,
    pub file_accounts: Vec<crate::storage::file_accounts_repo::FileAccount>,
    /// `settings.json` 的原始内容（不存在则为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<serde_json::Value>,
    /// `mcp.json` 的原始内容（不存在则为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<serde_json::Value>,
}

/// 明文凭据（`credentials` 表解密后的内容）。
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlainCredential {
    pub id: String,
    pub name: String,
    /// `"password"` / `"private_key_text"` / `"mysql_password"` / `"s3_credential"` 等。
    pub kind: String,
    pub created_at: String,
    /// 明文内容（密码 / 私钥文本 / 内层 JSON 字符串）。
    pub value: String,
    /// 私钥 passphrase（非私钥类型为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
}

// 手写 Debug：`value` / `passphrase` 是明文机密，派生实现会原样打进日志，这里脱敏。
impl std::fmt::Debug for PlainCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlainCredential")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("created_at", &self.created_at)
            .field("value", &"***")
            .field("passphrase", &self.passphrase.as_ref().map(|_| "***"))
            .finish()
    }
}

/// 明文 TOTP 配置（`totp_secrets` 表解密后的内容）。
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlainTotp {
    pub id: String,
    pub issuer: String,
    pub account: String,
    pub algorithm: String,
    pub digits: u32,
    pub period: u32,
    pub sort_order: i64,
    pub created_at: String,
    /// 明文 secret（base32 或原始字符串，与 totp_add 输入一致）。
    pub secret: String,
}

// 手写 Debug：`secret` 是明文机密，脱敏展示。
impl std::fmt::Debug for PlainTotp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlainTotp")
            .field("id", &self.id)
            .field("issuer", &self.issuer)
            .field("account", &self.account)
            .field("algorithm", &self.algorithm)
            .field("digits", &self.digits)
            .field("period", &self.period)
            .field("sort_order", &self.sort_order)
            .field("created_at", &self.created_at)
            .field("secret", &"***")
            .finish()
    }
}

// ===========================================================================
// 加密 / 解密
// ===========================================================================

/// 用备份密码加密载荷，生成 [`BackupFile`]。
pub fn encrypt_payload(
    payload: &BackupPayload,
    password: &str,
    app_version: &str,
) -> AppResult<BackupFile> {
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);

    let key = derive_key(password, &salt, KDF_M, KDF_T, KDF_P)?;
    let mut key_guard = KeyGuard(key);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_guard.0));

    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let plaintext = serde_json::to_string(payload)?;
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| AppError::Crypto(format!("AES-GCM 加密失败: {}", e)))?;
    key_guard.0.zeroize();

    Ok(BackupFile {
        format: BACKUP_FORMAT.into(),
        version: BACKUP_VERSION,
        app_version: app_version.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        kdf: KdfParams {
            algorithm: "argon2id".into(),
            salt: salt.to_vec(),
            m: KDF_M,
            t: KDF_T,
            p: KDF_P,
        },
        nonce: nonce_bytes,
        ciphertext,
    })
}

/// 解析并解密备份文件内容，返回明文载荷。
///
/// 密码错误（AES-GCM 认证失败）、文件损坏或格式不兼容都会返回错误。
pub fn decrypt_payload(content: &str, password: &str) -> AppResult<BackupPayload> {
    Ok(decrypt_full(content, password)?.1)
}

/// 解析并解密备份文件，返回文件头元数据与明文载荷（预览用）。
pub fn decrypt_full(content: &str, password: &str) -> AppResult<(BackupFile, BackupPayload)> {
    let file: BackupFile = serde_json::from_str(content)
        .map_err(|e| AppError::InvalidInput(format!("不是有效的备份文件: {}", e)))?;

    if file.format != BACKUP_FORMAT {
        return Err(AppError::InvalidInput(format!(
            "不是 X-Term 备份文件（format = {}）",
            file.format
        )));
    }
    if file.version > BACKUP_VERSION {
        return Err(AppError::InvalidInput(format!(
            "备份文件版本 {} 高于当前支持的版本 {}，请升级应用后再导入",
            file.version, BACKUP_VERSION
        )));
    }
    // KDF 参数校验（见 KDF_*_MAX 说明）：防止恶意文件用超大内存参数在密码
    // 校验前触发数 GB 内存分配导致 OOM；算法字段也校验，防算法混淆。
    if file.kdf.algorithm != "argon2id" {
        return Err(AppError::InvalidInput(format!(
            "不支持的备份 KDF 算法: {}（仅支持 argon2id）",
            file.kdf.algorithm
        )));
    }
    if file.kdf.m > KDF_M_MAX || file.kdf.t > KDF_T_MAX || file.kdf.p > KDF_P_MAX {
        return Err(AppError::InvalidInput(format!(
            "备份文件 KDF 参数超出安全上限（m={}, t={}, p={}；上限 m≤{} KiB, t≤{}, p≤{}）",
            file.kdf.m, file.kdf.t, file.kdf.p, KDF_M_MAX, KDF_T_MAX, KDF_P_MAX
        )));
    }

    let key = derive_key(password, &file.kdf.salt, file.kdf.m, file.kdf.t, file.kdf.p)?;
    let mut key_guard = KeyGuard(key);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_guard.0));
    let nonce = Nonce::from_slice(&file.nonce);
    let plaintext = cipher
        .decrypt(nonce, file.ciphertext.as_ref())
        .map_err(|_| AppError::Auth("备份密码错误或文件已损坏".into()))?;
    key_guard.0.zeroize();

    let payload: BackupPayload = serde_json::from_slice(&plaintext)
        .map_err(|e| AppError::InvalidInput(format!("备份内容解析失败: {}", e)))?;
    Ok((file, payload))
}

/// 从口令 + salt 派生 32 字节密钥（Argon2id）。
fn derive_key(passphrase: &str, salt: &[u8], m: u32, t: u32, p: u32) -> AppResult<[u8; 32]> {
    let params = Params::new(m, t, p, None)
        .map_err(|e| AppError::Crypto(format!("Argon2 参数无效: {}", e)))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, &mut out)
        .map_err(|e| AppError::Crypto(format!("Argon2 密钥派生失败: {}", e)))?;
    Ok(out)
}

/// 临时持有派生密钥，Drop 时清零。
struct KeyGuard([u8; 32]);

impl Drop for KeyGuard {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

// ===========================================================================
// 摘要结构（命令层返回给前端）
// ===========================================================================

/// 各数据段的条目数（导出 / 导入 / 预览共用）。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupCounts {
    pub sessions: usize,
    pub groups: usize,
    pub credentials: usize,
    pub db_profiles: usize,
    pub db_groups: usize,
    pub forward_rules: usize,
    pub desktops: usize,
    pub totp_secrets: usize,
    pub file_accounts: usize,
    /// 是否包含 settings.json。
    pub has_settings: bool,
    /// 是否包含 mcp.json。
    pub has_mcp: bool,
}

impl BackupCounts {
    /// 从载荷统计各段条数。
    pub fn from_payload(p: &BackupPayload) -> Self {
        Self {
            sessions: p.sessions.len(),
            groups: p.groups.len(),
            credentials: p.credentials.len(),
            db_profiles: p.db_profiles.len(),
            db_groups: p.db_groups.len(),
            forward_rules: p.forward_rules.len(),
            desktops: p.desktops.len(),
            totp_secrets: p.totp_secrets.len(),
            file_accounts: p.file_accounts.len(),
            has_settings: p.settings.is_some(),
            has_mcp: p.mcp.is_some(),
        }
    }
}

/// 导入前预览信息（`backup_inspect` 返回，不落任何数据）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupInfo {
    /// 文件格式版本。
    pub version: u32,
    /// 导出时的应用版本。
    pub app_version: String,
    /// 导出时间（RFC3339）。
    pub created_at: String,
    /// 各段条目数。
    pub counts: BackupCounts,
    /// 是否包含凭据（需要保险库解锁才能导入）。
    pub has_credentials: bool,
    /// 是否包含 TOTP secret（需要保险库解锁才能导入）。
    pub has_totp: bool,
}

/// 导出 / 导入完成后的摘要。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSummary {
    /// 文件路径。
    pub path: String,
    /// 各段条目数。
    pub counts: BackupCounts,
    /// 导出时因保险库未解锁而跳过的凭据数（导入恒为 0）。
    pub credentials_skipped: usize,
    /// 导出时因保险库未解锁而跳过的 TOTP 数（导入恒为 0）。
    pub totp_skipped: usize,
    /// 是否为覆盖导入。
    pub overwritten: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sessions_repo::{AuthType, Session};

    fn sample_payload() -> BackupPayload {
        BackupPayload {
            sessions: vec![Session {
                id: "s1".into(),
                name: "测试主机".into(),
                group_id: None,
                host: "10.0.0.1".into(),
                port: 22,
                username: "root".into(),
                auth_type: AuthType::Password,
                credential_id: Some("c1".into()),
                key_path: None,
                jump_session_id: None,
                startup_script: None,
                tags: None,
                color: None,
                sort_order: 0,
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                protocol: "ssh".into(),
                space_id: "local".into(),
            }],
            credentials: vec![PlainCredential {
                id: "c1".into(),
                name: "root 密码".into(),
                kind: "password".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                value: "s3cr3t".into(),
                passphrase: None,
            }],
            settings: Some(serde_json::json!({ "terminal": { "fontSize": 14 } })),
            ..Default::default()
        }
    }

    #[test]
    fn round_trip_encrypt_decrypt() {
        let payload = sample_payload();
        let file = encrypt_payload(&payload, "backup pass 123", "0.1.15").unwrap();

        // 外层只含元数据，用户数据必须在密文中。
        assert_eq!(file.format, BACKUP_FORMAT);
        assert_eq!(file.version, BACKUP_VERSION);
        assert!(!file.ciphertext.is_empty());

        let json = serde_json::to_string(&file).unwrap();
        let decrypted = decrypt_payload(&json, "backup pass 123").unwrap();

        assert_eq!(decrypted.sessions.len(), 1);
        assert_eq!(decrypted.sessions[0].host, "10.0.0.1");
        assert_eq!(decrypted.credentials[0].value, "s3cr3t");
        assert_eq!(decrypted.settings, payload.settings);
    }

    #[test]
    fn wrong_password_fails() {
        let payload = sample_payload();
        let file = encrypt_payload(&payload, "right password", "0.1.15").unwrap();
        let json = serde_json::to_string(&file).unwrap();
        let err = decrypt_payload(&json, "wrong password").unwrap_err();
        assert!(matches!(err, AppError::Auth(_)));
    }

    #[test]
    fn rejects_non_backup_content() {
        let err = decrypt_payload("{\"hello\":1}", "any").unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    #[test]
    fn rejects_future_version() {
        let payload = sample_payload();
        let mut file = encrypt_payload(&payload, "pass", "0.1.15").unwrap();
        file.version = BACKUP_VERSION + 1;
        let json = serde_json::to_string(&file).unwrap();
        let err = decrypt_payload(&json, "pass").unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }
}
