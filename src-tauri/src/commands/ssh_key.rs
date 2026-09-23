//! SSH 密钥对生成。
//!
//! 在「密钥管理」页生成 ED25519 / RSA 密钥对：私钥以 PKCS#8 PEM 序列化
//! （可选 PBKDF2 口令加密），复用 [`credential_save`] 加密存入凭据保险库；
//! 命令只返回公钥行与 SHA256 指纹，不返回私钥明文。

use serde::{Deserialize, Serialize};
use tauri::State;

use russh::keys::key::{KeyPair, PublicKey, SignatureHash};
use russh::keys::{encode_pkcs8_pem, encode_pkcs8_pem_encrypted, PublicKeyBase64};

use crate::commands::vault::{credential_save, CredentialInput};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// RSA 允许的位数。
const RSA_BITS_ALLOWED: [usize; 3] = [2048, 3072, 4096];
/// RSA 默认位数。
const RSA_BITS_DEFAULT: usize = 3072;
/// 口令加密私钥的 PBKDF2 迭代次数（SHA256 + AES-256-CBC）。
const PKCS8_ROUNDS: u32 = 100_000;

/// 密钥生成请求。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshKeyGenerateInput {
    /// 凭据名称（同时用作公钥注释）。
    pub name: String,
    /// 算法："ed25519" 或 "rsa"。
    pub algorithm: String,
    /// RSA 位数（仅 algorithm=rsa 时有效，默认 3072）。
    #[serde(default)]
    pub bits: Option<usize>,
    /// 私钥口令（可选；空串视为无口令）。
    #[serde(default)]
    pub passphrase: Option<String>,
}

/// 生成结果（不含私钥明文）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratedKeyInfo {
    /// 凭据 id。
    pub id: String,
    /// 公钥行："ssh-ed25519 AAAA... <名称>"。
    pub public_key: String,
    /// SHA256 指纹："SHA256:xxx"。
    pub fingerprint: String,
}

/// 生成 SSH 密钥对并存入凭据保险库。
///
/// 步骤：参数校验 + 保险库解锁检查（快失败，避免密钥生成完成后才发现无法
/// 保存）→ 阻塞线程池生成并序列化（RSA 需秒级，不阻塞主线程）→ 复用
/// `credential_save` 加密入库 → 返回公钥行与指纹。
#[tauri::command]
pub async fn ssh_key_generate(
    input: SshKeyGenerateInput,
    state: State<'_, AppState>,
) -> AppResult<GeneratedKeyInfo> {
    // 1. 参数校验。
    let name = input.name.trim();
    if name.is_empty() {
        return Err(AppError::InvalidInput("名称不能为空".into()));
    }
    let algorithm = input.algorithm.to_ascii_lowercase();
    if algorithm != "ed25519" && algorithm != "rsa" {
        return Err(AppError::InvalidInput(format!(
            "不支持的算法: {}",
            input.algorithm
        )));
    }
    let bits = if algorithm == "rsa" {
        let bits = input.bits.unwrap_or(RSA_BITS_DEFAULT);
        if !RSA_BITS_ALLOWED.contains(&bits) {
            return Err(AppError::InvalidInput(format!(
                "RSA 位数仅支持 {:?}",
                RSA_BITS_ALLOWED
            )));
        }
        bits
    } else {
        0
    };
    let passphrase = input.passphrase.filter(|p| !p.is_empty());

    // 2. 保险库解锁检查（快失败）。
    {
        let _guard = state.vault_read()?;
    }

    // 3. 阻塞线程池中生成并序列化（RSA 3072/4096 需要数秒）。
    let passphrase_for_gen = passphrase.clone();
    let (private_pem, pub_line, fingerprint) = tauri::async_runtime::spawn_blocking(move || {
        generate_key_pair(&algorithm, bits, passphrase_for_gen.as_deref())
    })
    .await
    .map_err(|e| AppError::Ssh(format!("密钥生成任务失败: {}", e)))??;

    // 4. 复用 credential_save 加密入库（其自身在阻塞线程池执行）。
    let id = credential_save(
        CredentialInput {
            id: None,
            name: name.to_string(),
            kind: "private_key_text".into(),
            value: private_pem,
            passphrase,
        },
        state,
    )
    .await?;

    Ok(GeneratedKeyInfo {
        id,
        public_key: format!("{} {}", pub_line, name),
        fingerprint,
    })
}

/// 生成密钥对并序列化（纯计算，供 `spawn_blocking` 调用）。
///
/// 返回 (私钥 PKCS#8 PEM, 公钥行, SHA256 指纹)。
fn generate_key_pair(
    algorithm: &str,
    bits: usize,
    passphrase: Option<&str>,
) -> AppResult<(String, String, String)> {
    // 生成密钥。
    let kp = match algorithm {
        "rsa" => KeyPair::generate_rsa(bits, SignatureHash::SHA2_256)
            .ok_or_else(|| AppError::Ssh("RSA 密钥生成失败".into()))?,
        _ => KeyPair::generate_ed25519()
            .ok_or_else(|| AppError::Ssh("ED25519 密钥生成失败".into()))?,
    };

    // 公钥行（算法名 + base64）与 SHA256 指纹。
    // 注意：RSA 即使签名哈希用 SHA2-256，authorized_keys 行的算法名仍须写
    // "ssh-rsa"（RSA 公钥 blob 的线格式名即 ssh-rsa，签名时再协商 rsa-sha2-*）；
    // 且 PublicKey::fingerprint() 只返回 base64，需自行加 "SHA256:" 前缀。
    let pubkey: PublicKey = kp
        .clone_public_key()
        .map_err(|e| AppError::Ssh(format!("提取公钥失败: {}", e)))?;
    let algo_name = if algorithm == "rsa" {
        "ssh-rsa"
    } else {
        "ssh-ed25519"
    };
    let pub_line = format!("{} {}", algo_name, pubkey.public_key_base64());
    let fingerprint = format!("SHA256:{}", pubkey.fingerprint());

    // 私钥 PKCS#8 PEM（有口令时 PBKDF2 加密）。
    let mut priv_buf = Vec::new();
    let encoded = match passphrase {
        Some(p) => encode_pkcs8_pem_encrypted(&kp, p.as_bytes(), PKCS8_ROUNDS, &mut priv_buf),
        None => encode_pkcs8_pem(&kp, &mut priv_buf),
    };
    encoded.map_err(|e| AppError::Ssh(format!("私钥序列化失败: {}", e)))?;
    let private_pem =
        String::from_utf8(priv_buf).map_err(|e| AppError::Ssh(format!("私钥编码异常: {}", e)))?;

    Ok((private_pem, pub_line, fingerprint))
}

#[cfg(test)]
mod tests {
    use super::generate_key_pair;
    use russh::keys::decode_secret_key;
    use russh::keys::key::KeyPair;

    /// ED25519：生成 → 公钥行/指纹格式正确 → 私钥可被 decode_secret_key 解析。
    #[test]
    fn ed25519_generate_roundtrip() {
        let (pem, pub_line, fingerprint) = generate_key_pair("ed25519", 0, None).expect("生成失败");
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert!(pub_line.starts_with("ssh-ed25519 "));
        assert!(fingerprint.starts_with("SHA256:"));
        let kp = decode_secret_key(&pem, None).expect("解析失败");
        assert!(matches!(kp, KeyPair::Ed25519(_)));
    }

    /// ED25519 + 口令：生成加密 PEM，正确口令可解析，错误/缺失口令失败。
    #[test]
    fn ed25519_generate_encrypted_roundtrip() {
        let (pem, _, _) = generate_key_pair("ed25519", 0, Some("pass123")).expect("生成失败");
        assert!(pem.starts_with("-----BEGIN ENCRYPTED PRIVATE KEY-----"));
        assert!(decode_secret_key(&pem, Some("pass123")).is_ok());
        assert!(decode_secret_key(&pem, Some("wrong")).is_err());
        assert!(decode_secret_key(&pem, None).is_err());
    }

    /// RSA 2048：生成较慢（约 1-2 秒），验证公钥行前缀与私钥可解析。
    #[test]
    fn rsa_generate_roundtrip() {
        let (pem, pub_line, fingerprint) = generate_key_pair("rsa", 2048, None).expect("生成失败");
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert!(pub_line.starts_with("ssh-rsa "));
        assert!(fingerprint.starts_with("SHA256:"));
        assert!(decode_secret_key(&pem, None).is_ok());
    }
}
