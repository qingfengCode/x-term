//! 凭据保险库：基于口令的 AES-256-GCM 加密存储。
//!
//! 设计要点：
//! - 主密钥通过 Argon2id 从用户口令 + 随机 salt 派生（32 字节）。
//! - `master.key` 文件中保存 `{ salt, verifier }`，其中 `verifier` 是用主密钥加密的
//!   固定明文 `X-TERM-OK`。`unlock` 时解密 `verifier` 与之比对，以验证口令正确。
//! - 实际凭据内容（密码、私钥等）由调用方作为 [`EncryptedBlob`] 存储到数据库的
//!   `credentials.enc_data` 字段（base64(JSON(blob))）。
//!
//! 安全说明：本模块提供的是“静态数据加密”，主密钥不落盘，仅在进程内存中持有。
//! 应用退出后，未持有正确口令的攻击者无法解密凭据。

use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::error::{AppError, AppResult};

/// 主密钥文件名。
const MASTER_KEY_FILENAME: &str = "master.key";
/// 固定的验证明文。
const VERIFIER_PLAINTEXT: &[u8] = b"X-TERM-OK";

// ===========================================================================
// 加密数据结构
// ===========================================================================

/// 加密后的数据块。
///
/// - `salt`: Argon2 派生时所用的随机 salt（每个凭据可独立生成，复用主密钥）；
///   简化实现中，凭据加密复用 `CredentialVault` 中已有的主密钥，salt 字段保留以便扩展
///   但当前不参与每次加密（设为空）。
/// - `nonce`: AES-256-GCM 的 12 字节随机 nonce，**每次加密必须随机重新生成**。
/// - `ciphertext`: 密文（含 GCM 认证标签）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedBlob {
    pub salt: Vec<u8>,
    pub nonce: [u8; 12],
    pub ciphertext: Vec<u8>,
}

/// master.key 文件内容。
#[derive(Debug, Serialize, Deserialize)]
struct MasterKeyFile {
    /// Argon2 派生主密钥所用的 salt。
    salt: Vec<u8>,
    /// 用主密钥加密 `VERIFIER_PLAINTEXT` 得到的 blob（仅含 nonce + ciphertext）。
    verifier: VerifierBlob,
}

/// verifier 是用已知主密钥加密固定明文，故不需要 salt。
#[derive(Debug, Serialize, Deserialize)]
struct VerifierBlob {
    nonce: [u8; 12],
    ciphertext: Vec<u8>,
}

// ===========================================================================
// CredentialVault
// ===========================================================================

/// 已解锁的凭据保险库，持有内存中的主密钥。
///
/// 主密钥为 32 字节，由 Argon2id 从用户口令派生。Drop 时通过 [`Zeroize`] 清零。
pub struct CredentialVault {
    master_key: [u8; 32],
}

impl std::fmt::Debug for CredentialVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialVault")
            .field("master_key", &"[redacted]")
            .finish()
    }
}

impl Clone for CredentialVault {
    fn clone(&self) -> Self {
        CredentialVault {
            master_key: self.master_key,
        }
    }
}

impl Drop for CredentialVault {
    fn drop(&mut self) {
        self.master_key.zeroize();
    }
}

impl CredentialVault {
    /// 返回主密钥的引用（仅供本模块内部使用）。
    fn key(&self) -> &Key<Aes256Gcm> {
        // aes-gcm 0.10 的 Key<Aes256Gcm> 是 GenericArray<u8, U32>，
        // 可以从 32 字节切片构造；这里 master_key 长度恒为 32。
        Key::<Aes256Gcm>::from_slice(&self.master_key)
    }

    /// 加密明文，返回 [`EncryptedBlob`]。
    pub fn encrypt(&self, plaintext: &[u8]) -> AppResult<EncryptedBlob> {
        let cipher = Aes256Gcm::new(self.key());
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| AppError::Crypto(format!("AES-GCM 加密失败: {}", e)))?;

        Ok(EncryptedBlob {
            // 复用 vault 主密钥，每条凭据无需独立 salt。
            salt: Vec::new(),
            nonce: nonce_bytes,
            ciphertext,
        })
    }

    /// 解密 [`EncryptedBlob`]。
    pub fn decrypt(&self, blob: &EncryptedBlob) -> AppResult<Vec<u8>> {
        let cipher = Aes256Gcm::new(self.key());
        let nonce = Nonce::from_slice(&blob.nonce);
        let plaintext = cipher
            .decrypt(nonce, blob.ciphertext.as_ref())
            .map_err(|e| AppError::Crypto(format!("AES-GCM 解密失败: {}", e)))?;
        Ok(plaintext)
    }

    /// 便捷方法：加密 UTF-8 字符串。
    pub fn encrypt_str(&self, s: &str) -> AppResult<EncryptedBlob> {
        self.encrypt(s.as_bytes())
    }

    /// 便捷方法：解密为 UTF-8 字符串。
    pub fn decrypt_str(&self, blob: &EncryptedBlob) -> AppResult<String> {
        let bytes = self.decrypt(blob)?;
        String::from_utf8(bytes)
            .map_err(|e| AppError::Crypto(format!("解密结果不是合法 UTF-8: {}", e)))
    }

    /// 便捷方法：把 [`EncryptedBlob`] 编码为 base64 字符串（用于存入数据库）。
    pub fn encode_blob(blob: &EncryptedBlob) -> String {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let json = serde_json::to_string(blob).unwrap_or_default();
        STANDARD.encode(json)
    }

    /// 便捷方法：从 base64 字符串解码出 [`EncryptedBlob`]。
    pub fn decode_blob(s: &str) -> AppResult<EncryptedBlob> {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let json = STANDARD
            .decode(s)
            .map_err(|e| AppError::Crypto(format!("base64 解码失败: {}", e)))?;
        let blob: EncryptedBlob = serde_json::from_slice(&json)?;
        Ok(blob)
    }
}

// ===========================================================================
// 构造 / 解锁
// ===========================================================================

/// 返回 master.key 文件路径。
fn master_key_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(MASTER_KEY_FILENAME)
}

/// 用 Argon2id 从口令 + salt 派生 32 字节主密钥。
fn derive_key(passphrase: &str, salt: &[u8]) -> AppResult<[u8; 32]> {
    // 使用 Argon2id 的默认参数（m = 19456 KiB, t = 2, p = 1），兼顾安全性与启动速度。
    let params = Params::default();
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, &mut out)
        .map_err(|e| AppError::Crypto(format!("Argon2 密钥派生失败: {}", e)))?;
    Ok(out)
}

/// 生成随机 salt（16 字节）。
fn random_salt() -> [u8; 16] {
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    salt
}

impl CredentialVault {
    /// 首次创建保险库：生成随机 salt，派生主密钥，并把 `{salt, verifier}` 写盘。
    ///
    /// 如果 master.key 已存在，将返回 [`AppError::InvalidInput`] 以避免覆盖现有数据。
    pub fn create(app_data_dir: &Path, passphrase: &str) -> AppResult<CredentialVault> {
        std::fs::create_dir_all(app_data_dir)?;
        let path = master_key_path(app_data_dir);
        if path.exists() {
            return Err(AppError::InvalidInput(
                "凭据保险库已存在，请使用 unlock 而不是 create".to_string(),
            ));
        }

        let salt = random_salt();
        let master_key = derive_key(passphrase, &salt)?;
        let vault = CredentialVault { master_key };

        // 生成 verifier：用主密钥加密固定明文。
        let verifier = vault.encrypt(VERIFIER_PLAINTEXT)?;
        let verifier_blob = VerifierBlob {
            nonce: verifier.nonce,
            ciphertext: verifier.ciphertext,
        };

        let file = MasterKeyFile {
            salt: salt.to_vec(),
            verifier: verifier_blob,
        };
        let content = serde_json::to_string(&file)?;
        std::fs::write(&path, content)?;

        Ok(vault)
    }

    /// 用口令解锁已有保险库。
    ///
    /// 读取 master.key，用 salt 派生主密钥，并解密 verifier 与固定明文比对；
    /// 比对失败则返回 [`AppError::Auth`]（口令错误）。
    pub fn unlock(app_data_dir: &Path, passphrase: &str) -> AppResult<CredentialVault> {
        let path = master_key_path(app_data_dir);
        if !path.exists() {
            return Err(AppError::NotFound(
                "凭据保险库不存在，请先创建".to_string(),
            ));
        }

        let content = std::fs::read_to_string(&path)?;
        let file: MasterKeyFile = serde_json::from_str(&content)?;

        let master_key = derive_key(passphrase, &file.salt)?;
        let vault = CredentialVault { master_key };

        // 验证口令：解密 verifier 应得到固定明文。
        // 错误口令会导致 AES-GCM 认证失败（解密报错）或明文不匹配，二者都视为口令错误。
        let blob = EncryptedBlob {
            salt: Vec::new(),
            nonce: file.verifier.nonce,
            ciphertext: file.verifier.ciphertext,
        };
        match vault.decrypt(&blob) {
            Ok(decrypted) if decrypted == VERIFIER_PLAINTEXT => Ok(vault),
            _ => Err(AppError::Auth(
                "口令错误，无法解锁凭据保险库".to_string(),
            )),
        }
    }

    /// 判断保险库是否已存在。
    pub fn exists(app_data_dir: &Path) -> bool {
        master_key_path(app_data_dir).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_encrypt_decrypt() {
        let tmp = std::env::temp_dir().join("xterm_secure_test_roundtrip");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let vault = CredentialVault::create(&tmp, "correct horse battery staple").unwrap();
        let blob = vault.encrypt_str("hello world").unwrap();
        assert_eq!(vault.decrypt_str(&blob).unwrap(), "hello world");

        // base64 编解码往返。
        let encoded = CredentialVault::encode_blob(&blob);
        let decoded = CredentialVault::decode_blob(&encoded).unwrap();
        assert_eq!(vault.decrypt_str(&decoded).unwrap(), "hello world");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn unlock_wrong_passphrase_fails() {
        let tmp = std::env::temp_dir().join("xterm_secure_test_unlock");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        CredentialVault::create(&tmp, "right password").unwrap();
        let err = CredentialVault::unlock(&tmp, "wrong password").unwrap_err();
        assert!(matches!(err, AppError::Auth(_)));

        // 正确口令可解锁。
        let vault = CredentialVault::unlock(&tmp, "right password").unwrap();
        let blob = vault.encrypt_str("secret").unwrap();
        assert_eq!(vault.decrypt_str(&blob).unwrap(), "secret");

        std::fs::remove_dir_all(&tmp).ok();
    }
}
