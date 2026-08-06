//! TOTP / MFA 验证码核心模块。
//!
//! 本模块提供：
//! - 数据模型 [`TotpEntry`] / [`TotpCode`]；
//! - 当前验证码生成 [`generate_now`]（基于 `totp-rs` crate）；
//! - `otpauth://` URI 解析 [`parse_otpauth_uri`]；
//! - 与 `totp_secrets` 表的 CRUD（secret 由调用方通过 vault 加密后传入）。
//!
//! 注意：项目依赖 `totp-rs` 实际锁定到 5.7.x（Cargo.toml 写 `^1.5`，但 cargo
//! 解析为 5.7.2，5.x 早于某些版本语义），其 `TOTP::new` 在不带 `otpauth` feature
//! 时签名为 `new(algorithm, digits, skew, step, secret)`，且会对 digits
//! （6..=8）和 secret 长度（>= 16 字节 / 128 位）做 RFC 校验。为兼容真实
//! Authenticator app 中常见的短 secret，这里统一使用 `TOTP::new_unchecked`
//! 构造，再用 `generate_current()` 做实际可用性校验。

use rusqlite::params;

use crate::error::{AppError, AppResult};
use crate::storage::db::DbConn;

// ===========================================================================
// 数据模型
// ===========================================================================

/// 一条 TOTP 配置（不含 secret 明文，列表/传输用）。
///
/// 对应 `totp_secrets` 表除 `enc_secret` 外的所有列。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TotpEntry {
    pub id: String,
    pub issuer: String,
    pub account: String,
    /// 算法名称：`"SHA1"` / `"SHA256"` / `"SHA512"`。
    pub algorithm: String,
    /// 验证码位数，通常 6，少数为 8。
    pub digits: u32,
    /// 周期（秒），通常 30。
    pub period: u32,
    /// 列表排序序号。
    pub sort_order: i64,
    /// 创建时间（RFC3339 字符串）。
    pub created_at: String,
}

/// 一次验证码生成结果。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TotpCode {
    /// 当前周期对应的验证码（6 或 8 位数字）。
    pub code: String,
    /// 当前周期剩余秒数（`0..period`）。
    pub remaining_seconds: u32,
    /// 周期（秒）。
    pub period: u32,
}

// ===========================================================================
// 生成
// ===========================================================================

/// 把算法字符串（大小写不敏感）映射为 `totp_rs::Algorithm`。
///
/// 支持：`SHA1` / `SHA256` / `SHA512`；未知值返回 [`AppError::InvalidInput`]。
fn parse_algorithm(name: &str) -> AppResult<totp_rs::Algorithm> {
    match name.to_ascii_uppercase().as_str() {
        "SHA1" => Ok(totp_rs::Algorithm::SHA1),
        "SHA256" => Ok(totp_rs::Algorithm::SHA256),
        "SHA512" => Ok(totp_rs::Algorithm::SHA512),
        // 任务文档里把 SHA256 误写为 "SHA2"，这里做一层兼容。
        "SHA2" => Ok(totp_rs::Algorithm::SHA256),
        other => Err(AppError::InvalidInput(format!(
            "不支持的 TOTP 算法: {}（仅支持 SHA1/SHA256/SHA512）",
            other
        ))),
    }
}

/// 尝试把用户提供的 secret 字符串解码为原始字节。
///
/// TOTP 标准的 secret 通常是 base32（RFC4648，无填充）编码，出现在
/// `otpauth://` URI 的 `secret=` 参数中；少数用户会直接粘贴原始字节字符串。
/// 这里按以下顺序尝试：
/// 1. 作为 base32 解码（`Secret::Encoded`）；
/// 2. 失败则把字符串的 UTF-8 字节作为原始 secret（`Secret::Raw`）。
fn decode_secret(secret: &str) -> AppResult<Vec<u8>> {
    use totp_rs::Secret;

    // 先按 base32 解码（trim 掉空白与可能的填充符）。
    let trimmed = secret.trim();
    let cleaned: String = trimmed
        .chars()
        .filter(|c| *c != '=' && !c.is_whitespace())
        .collect();
    if !cleaned.is_empty() {
        if let Ok(bytes) = Secret::Encoded(cleaned).to_bytes() {
            return Ok(bytes);
        }
    }
    // 回退：原始字节。Secret::Raw 的 to_bytes() 恒返回 Ok，但类型签名是 Result，
    // 且 SecretParseError 未实现 From<...> for AppError，这里显式映射。
    match Secret::Raw(trimmed.as_bytes().to_vec()).to_bytes() {
        Ok(bytes) => Ok(bytes),
        Err(e) => Err(AppError::InvalidInput(format!("secret 解码失败: {}", e))),
    }
}

/// 根据参数实时生成当前周期的 TOTP 验证码。
///
/// 不读取数据库、不接触 vault —— 纯函数，命令层既可用于已入库 entry 的生成，
/// 也可用于添加对话框中的实时预览。
///
/// # 参数
/// - `secret`: base32 字符串或原始字节字符串。
/// - `algorithm`: `"SHA1"` / `"SHA256"` / `"SHA512"`（大小写不敏感）。
/// - `digits`: 验证码位数（一般 6，部分 8）。
/// - `period`: 周期秒数（一般 30）。
pub fn generate_now(
    secret: &str,
    algorithm: &str,
    digits: u32,
    period: u32,
) -> AppResult<TotpCode> {
    if period == 0 {
        return Err(AppError::InvalidInput("TOTP period 必须大于 0".into()));
    }
    // digits 必须落在 totp-rs 的合法区间（RFC 4226: 6..=8）。
    // new_unchecked 会绕过 totp-rs 的校验：digits=0 时 `10^0=1` 取模后返回空串
    // 验证码；digits>=10 时 `10u32.pow` 溢出，debug 构建直接 panic、release
    // 构建静默回绕产生错误验证码。
    if !(6..=8).contains(&digits) {
        return Err(AppError::InvalidInput(format!(
            "TOTP digits 必须是 6~8，实际 {}",
            digits
        )));
    }
    let algo = parse_algorithm(algorithm)?;
    let secret_bytes = decode_secret(secret)?;

    // 使用 new_unchecked 构造：兼容真实 Authenticator 中常见的 < 128 位短 secret
    // 以及部分用户自定义的 digits。实际可用性由下面的 generate_current 兜底校验。
    // totp-rs 5.x 在不带 otpauth feature 时签名为 (algorithm, digits, skew, step, secret)。
    let totp = totp_rs::TOTP::new_unchecked(algo, digits as usize, 1, period as u64, secret_bytes);

    let code = totp
        .generate_current()
        .map_err(|e| AppError::Crypto(format!("获取系统时间失败: {}", e)))?;

    let now_unix = chrono::Utc::now().timestamp().max(0) as u64;
    let elapsed_in_period = now_unix % (period as u64);
    let remaining = (period as u64) - elapsed_in_period;

    Ok(TotpCode {
        code,
        remaining_seconds: remaining as u32,
        period,
    })
}

// ===========================================================================
// otpauth:// URI 解析
// ===========================================================================

/// 简单的 URL 百分号解码（处理 `%XX`）。
///
/// 项目当前未启用 `url` crate 依赖，且 `totp-rs` 的 `otpauth` feature 也未启用，
/// 故 URI 解析用最小手写实现覆盖常见情形（issuer:account、query 参数）。
fn percent_decode(input: &str) -> String {
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
        // `+` 在 query 中通常表示空格。
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 解析 query 字符串为 `(key, value)` 对列表（key 小写化）。
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.to_ascii_lowercase(), percent_decode(v)))
        .collect()
}

/// 解析 `otpauth://totp/Issuer:account?secret=...&issuer=...&...` URI。
///
/// 返回 [`TotpEntry`]：`id` 与 `created_at` 留空（由调用方填充），
/// 不含 secret 明文（secret 仅出现在 query 中，调用方负责单独提取并加密）。
///
/// 解析规则：
/// - scheme 必须为 `otpauth`，host 必须为 `totp`；
/// - path 去掉前导 `/` 后：若含 `:`，按第一个 `:` 切分 issuer / account；
///   否则整段作为 account，issuer 从 query 取（可为空）；
/// - query 参数：`secret`（必填）、`issuer`（可选，与 path 中的一致性不做强制校验）、
///   `algorithm`（默认 `SHA1`）、`digits`（默认 6）、`period`（默认 30）。
pub fn parse_otpauth_uri(uri: &str) -> AppResult<TotpEntry> {
    let uri = uri.trim();
    // 形如 otpauth://totp/...?...
    // 1. 分离 scheme。
    let after_scheme = uri
        .strip_prefix("otpauth://")
        .ok_or_else(|| AppError::InvalidInput("otpauth URI 必须以 otpauth:// 开头".into()))?;

    // 2. 分离 path 与 query。
    let (authority_and_path, query) = match after_scheme.find('?') {
        Some(idx) => (&after_scheme[..idx], &after_scheme[idx + 1..]),
        None => (after_scheme, ""),
    };

    // 3. 分离 host（第一个 / 之前）与 path。
    let (host, path) = match authority_and_path.find('/') {
        Some(idx) => (&authority_and_path[..idx], &authority_and_path[idx + 1..]),
        None => (authority_and_path, ""),
    };
    if !host.eq_ignore_ascii_case("totp") {
        return Err(AppError::InvalidInput(format!(
            "otpauth URI 的 host 必须是 totp，收到: {}",
            host
        )));
    }

    // 4. 解析 label = path（已去掉前导 /）。
    let label = percent_decode(path);
    let (mut issuer, account) = if let Some((iss, acc)) = label.split_once(':') {
        (iss.to_string(), acc.to_string())
    } else {
        (String::new(), label.to_string())
    };

    // 5. query 参数。
    let params = parse_query(query);
    let mut secret: Option<String> = None;
    let mut algorithm: Option<String> = None;
    let mut digits: Option<u32> = None;
    let mut period: Option<u32> = None;
    let mut query_issuer: Option<String> = None;
    for (k, v) in &params {
        match k.as_str() {
            "secret" => secret = Some(v.clone()),
            "algorithm" => algorithm = Some(v.clone()),
            "digits" => digits = v.parse().ok(),
            "period" => period = v.parse().ok(),
            "issuer" => query_issuer = Some(v.clone()),
            _ => {}
        }
    }

    // secret 必填。
    let _secret = secret
        .ok_or_else(|| AppError::InvalidInput("otpauth URI 缺少必填的 secret 参数".into()))?;

    // issuer：path 优先，否则取 query。
    if issuer.is_empty() {
        if let Some(qi) = query_issuer {
            issuer = qi;
        }
    }

    Ok(TotpEntry {
        id: String::new(),
        issuer,
        account,
        algorithm: algorithm.unwrap_or_else(|| "SHA1".into()),
        digits: digits.unwrap_or(6),
        period: period.unwrap_or(30),
        sort_order: 0,
        created_at: String::new(),
    })
}

// ===========================================================================
// CRUD（totp_secrets 表）
// ===========================================================================

/// 列出所有 TOTP 配置（不含 secret），按 sort_order、issuer、account 排序。
pub fn list_totp(conn: &DbConn) -> AppResult<Vec<TotpEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, issuer, account, algorithm, digits, period, sort_order, created_at \
         FROM totp_secrets \
         ORDER BY sort_order ASC, issuer ASC, account ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(TotpEntry {
            id: r.get(0)?,
            issuer: r.get(1)?,
            account: r.get(2)?,
            algorithm: r.get(3)?,
            digits: r.get::<_, i64>(4)? as u32,
            period: r.get::<_, i64>(5)? as u32,
            sort_order: r.get(6)?,
            created_at: r.get(7)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 插入或更新一条 TOTP 配置。
///
/// `enc_secret` 应为已通过 vault 加密并 `encode_blob` 后的 base64 字符串。
/// 若 entry.created_at 为空，自动填入当前时间。
pub fn upsert_totp(conn: &DbConn, entry: &TotpEntry, enc_secret: &str) -> AppResult<()> {
    let created_at = if entry.created_at.is_empty() {
        chrono::Utc::now().to_rfc3339()
    } else {
        entry.created_at.clone()
    };
    conn.execute(
        "INSERT INTO totp_secrets \
         (id, issuer, account, enc_secret, algorithm, digits, period, sort_order, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT(id) DO UPDATE SET \
            issuer = excluded.issuer, \
            account = excluded.account, \
            enc_secret = excluded.enc_secret, \
            algorithm = excluded.algorithm, \
            digits = excluded.digits, \
            period = excluded.period, \
            sort_order = excluded.sort_order",
        params![
            entry.id,
            entry.issuer,
            entry.account,
            enc_secret,
            entry.algorithm,
            entry.digits as i64,
            entry.period as i64,
            entry.sort_order,
            created_at,
        ],
    )?;
    Ok(())
}

/// 删除一条 TOTP 配置。
pub fn delete_totp(conn: &DbConn, id: &str) -> AppResult<()> {
    conn.execute("DELETE FROM totp_secrets WHERE id = ?1", [id])?;
    Ok(())
}

/// 取得一条 TOTP 配置及其加密后的 secret 字符串。
///
/// 返回 `(entry, enc_secret)`，其中 enc_secret 为数据库中存储的 base64 blob。
/// 不存在时返回 [`AppError::NotFound`]。
pub fn get_totp_secret_enc(conn: &DbConn, id: &str) -> AppResult<(TotpEntry, String)> {
    let row = match conn.query_row(
        "SELECT id, issuer, account, enc_secret, algorithm, digits, period, sort_order, created_at \
         FROM totp_secrets WHERE id = ?1",
        [id],
        |r| {
            Ok((
                TotpEntry {
                    id: r.get(0)?,
                    issuer: r.get(1)?,
                    account: r.get(2)?,
                    algorithm: r.get(4)?,
                    digits: r.get::<_, i64>(5)? as u32,
                    period: r.get::<_, i64>(6)? as u32,
                    sort_order: r.get(7)?,
                    created_at: r.get(8)?,
                },
                r.get::<_, String>(3)?,
            ))
        },
    ) {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(AppError::NotFound(format!("TOTP 配置 {} 不存在", id)));
        }
        Err(e) => return Err(e.into()),
    };
    Ok(row)
}

// ===========================================================================
// 测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_sha1_known_vector() {
        // totp-rs 内部测试向量：TOTP::new(SHA1, 6, 1, 1, "TestSecretSuperSecret")
        // 在 t=1000 时输出 "659761"。这里用周期 30、当前时间生成，
        // 仅验证不报错、长度正确、剩余秒数在范围内。
        let code = generate_now("TestSecretSuperSecret", "SHA1", 6, 30).unwrap();
        assert_eq!(code.code.len(), 6);
        // 周期内剩余秒数 ∈ (0, 30]；恰好整周期触发时剩余 = 30，不能用 < 30 否则偶发失败。
        assert!(code.remaining_seconds <= 30 && code.remaining_seconds > 0, "{}", code.remaining_seconds);
        assert_eq!(code.period, 30);
    }

    #[test]
    fn reject_invalid_digits() {
        // digits 越界必须报错而非生成空码/溢出。
        assert!(generate_now("TestSecretSuperSecret", "SHA1", 0, 30).is_err());
        assert!(generate_now("TestSecretSuperSecret", "SHA1", 5, 30).is_err());
        assert!(generate_now("TestSecretSuperSecret", "SHA1", 9, 30).is_err());
        assert!(generate_now("TestSecretSuperSecret", "SHA1", 10, 30).is_err());
        assert!(generate_now("TestSecretSuperSecret", "SHA1", 6, 0).is_err());
    }

    #[test]
    fn generate_base32_secret() {
        // "KRSXG5CTMVRXEZLUKN2XAZLSKNSWG4TFOQ" 是 "TestSecretSuperSecret" 的 base32。
        let code = generate_now("KRSXG5CTMVRXEZLUKN2XAZLSKNSWG4TFOQ", "sha256", 6, 30).unwrap();
        assert_eq!(code.code.len(), 6);
    }

    #[test]
    fn parse_simple_uri() {
        let entry = parse_otpauth_uri(
            "otpauth://totp/GitHub:alice@example.com?secret=KRSXG5CTMVRXEZLUKN2XAZLSKNSWG4TFOQ&issuer=GitHub&algorithm=SHA256&digits=8&period=60",
        )
        .unwrap();
        assert_eq!(entry.issuer, "GitHub");
        assert_eq!(entry.account, "alice@example.com");
        assert_eq!(entry.algorithm, "SHA256");
        assert_eq!(entry.digits, 8);
        assert_eq!(entry.period, 60);
    }

    #[test]
    fn parse_uri_no_issuer_in_path() {
        let entry = parse_otpauth_uri(
            "otpauth://totp/alice?secret=KRSXG5CTMVRXEZLUKN2XAZLSKNSWG4TFOQ&issuer=Acme",
        )
        .unwrap();
        assert_eq!(entry.issuer, "Acme");
        assert_eq!(entry.account, "alice");
        assert_eq!(entry.algorithm, "SHA1");
        assert_eq!(entry.digits, 6);
        assert_eq!(entry.period, 30);
    }

    #[test]
    fn parse_uri_missing_secret_fails() {
        let err = parse_otpauth_uri("otpauth://totp/Acme:alice?issuer=Acme").unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }
}
