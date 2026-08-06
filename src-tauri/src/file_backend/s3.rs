//! S3（及兼容存储：MinIO / OSS / COS / R2）文件后端。
//!
//! 基于 reqwest 手写 AWS Signature V4 签名，走 S3 REST API，不引入 AWS SDK。
//! 兼容任何实现 ListObjectsV2 / GetObject / PutObject / DeleteObject / HeadObject /
//! CopyObject 的对象存储服务。
//!
//! ## 路径语义
//! 连接时绑定一个 bucket，所有方法传入的 `path` 即对象 key（自动去掉前导 `/`）。
//! 对象存储无真正的目录，本实现按惯例处理：
//! - `list_dir(prefix)`：以 `/` 分隔的逻辑前缀列举（delimiter=`/`），返回同层
//!   文件 + 子"目录"（CommonPrefixes）。
//! - `mkdir`：写入 0 字节占位对象 `<path>/.dirkeep`，使目录在前端可见。
//! - `remove_dir`：列出前缀下所有对象并批量删除。
//! - `rename`：复制 + 删除原对象（S3 无原子 rename）。

use std::path::Path;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::{Client, Method};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};
use crate::file_backend::{FileBackend, FileEntry, FileMeta, ProgressCb};

/// HMAC-SHA256 类型别名（SigV4 签名用）。
type HmacSha256 = Hmac<Sha256>;

/// SHA-256 十六进制摘要。
fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// 不做任何编码保留的字符集（用于 query 值/路径段编码）。
///
/// S3 / SigV4 要求的编码相对保守：字母数字和 `-_.~` 不编码，其余按 `%HH` 编码。
const S3_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// S3 文件后端配置。
#[derive(Debug, Clone)]
pub struct S3Config {
    /// Endpoint，如 `https://s3.amazonaws.com` 或 `https://minio.local:9000`。
    pub endpoint: String,
    /// Region（aws-east-1 等）。MinIO 可填任意非空值。
    pub region: String,
    /// Bucket 名。
    pub bucket: String,
    /// Access key。
    pub access_key: String,
    /// Secret key。
    pub secret_key: String,
    /// 寻址风格：true=path-style（`<endpoint>/<bucket>/<key>`，MinIO/兼容存储默认）；
    /// false=virtual-hosted-style（`<scheme>://<bucket>.<host>/<key>`，AWS S3 默认）。
    /// 默认 true。带端口/路径前缀的自定义 endpoint 应保持 true。
    pub path_style: bool,
}

/// S3 后端实例。
pub struct S3Backend {
    config: S3Config,
    client: Client,
}

impl S3Backend {
    /// 创建一个新的 S3 后端实例（不发起连接，首次请求时建连）。
    ///
    /// region 为空时默认 `us-east-1`（AWS 标准 region；MinIO 等兼容存储接受任意非空值），
    /// 避免空 region 导致 SigV4 签名失败。
    pub fn new(config: S3Config) -> AppResult<Self> {
        if config.endpoint.trim().is_empty() {
            return Err(AppError::InvalidInput("S3 endpoint 不能为空".into()));
        }
        if config.bucket.trim().is_empty() {
            return Err(AppError::InvalidInput("S3 bucket 不能为空".into()));
        }
        if config.access_key.is_empty() || config.secret_key.is_empty() {
            return Err(AppError::InvalidInput(
                "S3 access_key / secret_key 不能为空".into(),
            ));
        }
        let region = if config.region.trim().is_empty() {
            "us-east-1".to_string()
        } else {
            config.region.trim().to_string()
        };
        // 注意：不能用 `timeout()`（总请求时限，包含 body 读取）——大文件下载/
        // 上传必然超过 120s 被掐断。用 `read_timeout()`（单次读取停滞检测）兜底，
        // 既不会误杀长传输，又能检测到连接卡死。
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| AppError::Storage(format!("创建 HTTP 客户端失败: {}", e)))?;
        Ok(Self {
            config: S3Config { region, ..config },
            client,
        })
    }

    /// 规范化 object key：去掉前导 `/`，保证非空时以无前导斜杠开头。
    fn normalize_key(path: &str) -> String {
        let trimmed = path.trim_start_matches('/');
        trimmed.to_string()
    }

    /// 解析 endpoint 为 (scheme, host_with_port)。
    /// 用 reqwest::Url 统一处理，兼容大小写 scheme、带端口/路径前缀的 endpoint。
    /// 失败时回退到手工 trim 解析（兼容旧逻辑）。
    fn parse_endpoint(&self) -> (String, String) {
        let ep = self.config.endpoint.trim();
        if let Ok(url) = reqwest::Url::parse(ep) {
            let scheme = url.scheme().to_lowercase();
            let host = url.host_str().map(|h| h.to_lowercase()).unwrap_or_default();
            let port = url.port();
            // 保留 endpoint 里 path 前缀（部分自建存储 endpoint 带 /prefix）。
            let path = url.path().trim_end_matches('/');
            let host_with_port = match port {
                Some(p) => format!("{}:{}{}", host, p, path),
                None => format!("{}{}", host, path),
            };
            return (scheme, host_with_port);
        }
        // 回退：手工解析（与旧逻辑一致）。
        let lower = ep.to_ascii_lowercase();
        let (scheme, rest) = if let Some(r) = lower.strip_prefix("https://") {
            ("https", r)
        } else if let Some(r) = lower.strip_prefix("http://") {
            ("http", r)
        } else {
            ("https", ep)
        };
        (scheme.into(), rest.trim_end_matches('/').to_string())
    }

    /// 签名用的 host 头值（含端口）。
    /// - path-style：endpoint 的 host（如 `minio.local:9000`）。
    /// - virtual-hosted：`<bucket>.<host>`（如 `mybucket.s3.amazonaws.com`）。
    fn signing_host(&self) -> String {
        let (_, host) = self.parse_endpoint();
        if self.config.path_style {
            host
        } else {
            // virtual-hosted：去掉 endpoint 里的 path 前缀，bucket 插到 host 前。
            let bare_host = host.split('/').next().unwrap_or(&host);
            let path = host.strip_prefix(bare_host).unwrap_or("");
            format!("{}.{}{}", self.config.bucket, bare_host, path)
        }
    }

    /// 完整请求 URL（reqwest 发请求用）。
    /// - path-style：`<endpoint>/<bucket>/<key>`。
    /// - virtual-hosted：`<scheme>://<bucket>.<host>/<key>`。
    fn object_url(&self, key: &str) -> String {
        let (scheme, host) = self.parse_endpoint();
        let key = Self::normalize_key(key);
        let encoded: String = if key.is_empty() {
            String::new()
        } else {
            key.split('/')
                .map(|seg| utf8_percent_encode(seg, S3_ENCODE_SET).to_string())
                .collect::<Vec<_>>()
                .join("/")
        };
        if self.config.path_style {
            if encoded.is_empty() {
                format!("{}://{}/{}", scheme, host, self.config.bucket)
            } else {
                format!("{}://{}/{}/{}", scheme, host, self.config.bucket, encoded)
            }
        } else {
            // virtual-hosted：URL host 用 <bucket>.<host>，path 不含 bucket。
            let bare_host = host.split('/').next().unwrap_or(&host);
            let path = host.strip_prefix(bare_host).unwrap_or("");
            if encoded.is_empty() {
                format!("{}://{}.{}{}", scheme, self.config.bucket, bare_host, path)
            } else {
                format!(
                    "{}://{}.{}{}/{}",
                    scheme, self.config.bucket, bare_host, path, encoded
                )
            }
        }
    }

    /// SigV4 canonical URI（path-style 含 `/<bucket>`，virtual-hosted 仅 `/` + key）。
    fn canonical_uri(&self, key: &str) -> String {
        let key_norm = Self::normalize_key(key);
        let encoded: String = if key_norm.is_empty() {
            String::new()
        } else {
            key_norm
                .split('/')
                .map(|seg| utf8_percent_encode(seg, S3_ENCODE_SET).to_string())
                .collect::<Vec<_>>()
                .join("/")
        };
        if self.config.path_style {
            if encoded.is_empty() {
                format!("/{}", self.config.bucket)
            } else {
                format!("/{}/{}", self.config.bucket, encoded)
            }
        } else {
            // virtual-hosted：bucket 已在 host 头里，canonical URI 不含 bucket。
            if encoded.is_empty() {
                "/".to_string()
            } else {
                format!("/{}", encoded)
            }
        }
    }

    /// 发起一个已签名请求，返回 reqwest RequestBuilder（调用方负责 .send()）。
    ///
    /// `payload_sha`：PUT 请求体的 SHA-256 十六进制；无 body 传空字符串的 sha256
    /// 即 `e3b0c44...`（UNSIGNED-PAYLOAD 模式也可，这里用完整签名更通用）。
    async fn signed_request(
        &self,
        method: Method,
        key: &str,
        query: &[(String, String)],
        headers_extra: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    ) -> AppResult<reqwest::Response> {
        let now: DateTime<Utc> = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_short = now.format("%Y%m%d").to_string();

        // body & payload hash
        let body_bytes = body.unwrap_or_default();
        let payload_hash = sha256_hex(&body_bytes);
        // host 与 canonical URI 由 path_style/virtual-hosted 共同决定（统一解析）。
        let host = self.signing_host();
        let canonical_uri = self.canonical_uri(key);

        // 规范化 query：按 key 排序，编码。
        let mut q_sorted: Vec<(String, String)> = query.to_vec();
        q_sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let canonical_query = q_sorted
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    utf8_percent_encode(k, S3_ENCODE_SET),
                    utf8_percent_encode(v, S3_ENCODE_SET)
                )
            })
            .collect::<Vec<_>>()
            .join("&");

        // 构造请求 URL：query 串用与签名完全相同的编码直接拼到 URL 上。
        // 不能用 reqwest 的 .query()——它按 form-urlencoded 编码（空格→+、~→%7E、
        // *→保留），与 SigV4 canonical query（空格→%20、~→保留、*→%2A）不一致，
        // 路径/前缀含这些字符时会导致签名不匹配。
        let mut request_url = self.object_url(key);
        if !canonical_query.is_empty() {
            request_url.push('?');
            request_url.push_str(&canonical_query);
        }
        let mut req = self.client.request(method.clone(), request_url);
        req = req
            .header("host", &host)
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", &payload_hash);
        for (k, v) in &headers_extra {
            req = req.header(k.to_lowercase(), v);
        }

        // canonical headers：取本次实际发送的 headers（host / x-amz-* / extra）。
        let mut canonical_headers_list: Vec<(String, String)> = vec![
            ("host".into(), host.to_string()),
            ("x-amz-content-sha256".into(), payload_hash.clone()),
            ("x-amz-date".into(), amz_date.clone()),
        ];
        for (k, v) in &headers_extra {
            canonical_headers_list.push((k.to_lowercase(), v.clone()));
        }
        canonical_headers_list.sort_by(|a, b| a.0.cmp(&b.0));
        let canonical_headers = canonical_headers_list
            .iter()
            .map(|(k, v)| format!("{}:{}\n", k, v.trim()))
            .collect::<String>();
        let signed_headers = canonical_headers_list
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");

        // canonical request
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method.as_str(),
            canonical_uri,
            canonical_query,
            canonical_headers,
            signed_headers,
            payload_hash
        );

        // string to sign
        let credential_scope = format!("{}/{}/s3/aws4_request", date_short, self.config.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            credential_scope,
            sha256_hex(canonical_request.as_bytes())
        );

        // signing key chain: kSecret → kDate → kRegion → kService → kSigning
        let signing_key = {
            let k_secret = format!("AWS4{}", self.config.secret_key);
            let k_date = hmac_sha256(k_secret.as_bytes(), date_short.as_bytes());
            let k_region = hmac_sha256(&k_date, self.config.region.as_bytes());
            let k_service = hmac_sha256(&k_region, b"s3");
            hmac_sha256(&k_service, b"aws4_request")
        };
        let signature = hex::encode(hmac_sha256_vec(&signing_key, string_to_sign.as_bytes()));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.config.access_key, credential_scope, signed_headers, signature
        );

        req = req.header("authorization", authorization);
        if !body_bytes.is_empty() {
            req = req.body(body_bytes);
        }

        req.send()
            .await
            .map_err(|e| AppError::Storage(format!("S3 请求失败: {}", e)))
    }

    /// 流式 PUT（UNSIGNED-PAYLOAD），用于大文件上传避免整文件入内存。
    ///
    /// 与 [`signed_request`] 共用 SigV4 签名算法，但 payload hash 固定为
    /// `UNSIGNED-PAYLOAD`（S3 标准约定：服务端不校验 body 的 SHA256），body 用
    /// reqwest 的流式 `Body`（由调用方包装文件读取流）。这样上传几个 GB 文件
    /// 也不会 OOM，且可在读取过程中回调进度。
    async fn signed_streaming_put(
        &self,
        key: &str,
        stream_body: reqwest::Body,
        content_length: u64,
    ) -> AppResult<reqwest::Response> {
        let now: DateTime<Utc> = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_short = now.format("%Y%m%d").to_string();
        let payload_hash = "UNSIGNED-PAYLOAD".to_string();
        let host = self.signing_host();
        let canonical_uri = self.canonical_uri(key);

        let mut req = self.client.request(Method::PUT, self.object_url(key));
        req = req
            .header("host", &host)
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header("content-length", content_length.to_string());

        let canonical_headers_list: Vec<(String, String)> = vec![
            ("host".into(), host.clone()),
            ("x-amz-content-sha256".into(), payload_hash.clone()),
            ("x-amz-date".into(), amz_date.clone()),
        ];
        let canonical_headers = canonical_headers_list
            .iter()
            .map(|(k, v)| format!("{}:{}\n", k, v.trim()))
            .collect::<String>();
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";

        let canonical_request = format!(
            "PUT\n{}\n\n{}\n{}\n{}",
            canonical_uri, canonical_headers, signed_headers, payload_hash
        );
        let credential_scope = format!("{}/{}/s3/aws4_request", date_short, self.config.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            credential_scope,
            sha256_hex(canonical_request.as_bytes())
        );
        let signing_key = {
            let k_secret = format!("AWS4{}", self.config.secret_key);
            let k_date = hmac_sha256(k_secret.as_bytes(), date_short.as_bytes());
            let k_region = hmac_sha256(&k_date, self.config.region.as_bytes());
            let k_service = hmac_sha256(&k_region, b"s3");
            hmac_sha256(&k_service, b"aws4_request")
        };
        let signature = hex::encode(hmac_sha256_vec(&signing_key, string_to_sign.as_bytes()));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.config.access_key, credential_scope, signed_headers, signature
        );

        req = req.header("authorization", authorization);
        req = req.body(stream_body);
        req.send()
            .await
            .map_err(|e| AppError::Storage(format!("S3 流式上传请求失败: {}", e)))
    }

    /// HEAD object → 返回文件大小和最后修改时间。空 key（bucket 根）直接返回 None。
    async fn head_object(&self, key: &str) -> AppResult<Option<(u64, Option<String>)>> {
        if key.is_empty() {
            return Ok(None);
        }
        let resp = self
            .signed_request(Method::HEAD, key, &[], vec![], None)
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(self.err_from_response("HEAD", key, resp).await);
        }
        let size = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let modified = resp
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        Ok(Some((size, modified)))
    }

    /// 把 reqwest 错误响应转成 AppError（附带状态码和响应体片段）。
    /// 把 S3 错误响应转成 AppError。优先解析 XML 里的 `<Code>` / `<Message>`，
    /// 把常见错误码映射为人类可读中文提示；解析失败回退到状态码 + 响应体预览。
    async fn err_from_response(&self, op: &str, key: &str, resp: reqwest::Response) -> AppError {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // 尝试解析 S3 错误 XML：<Error><Code>...</Code><Message>...</Message></Error>。
        #[derive(Debug, Deserialize)]
        #[serde(rename = "Error")]
        struct S3Error {
            #[serde(rename = "Code", default)]
            code: String,
            #[serde(rename = "Message", default)]
            message: String,
        }
        if let Ok(err) = quick_xml::de::from_str::<S3Error>(&body) {
            let hint = s3_error_hint(&err.code);
            let detail = if hint.is_empty() {
                err.message
            } else {
                format!("{}（{}）", hint, err.message)
            };
            return AppError::Storage(format!(
                "S3 {} `{}` 失败: HTTP {} [{}] {}",
                op, key, status, err.code, detail
            ));
        }
        // 回退：非 XML 响应（如反代返回 HTML），保留预览便于排查。
        let preview: String = body.chars().take(200).collect();
        AppError::Storage(format!(
            "S3 {} `{}` 失败: HTTP {} | {}",
            op, key, status, preview
        ))
    }
}

/// 把 S3 错误码映射为人类可读中文提示（空串表示无映射）。
fn s3_error_hint(code: &str) -> &'static str {
    match code {
        "InvalidAccessKeyId" => "AccessKey 无效",
        "SignatureDoesNotMatch" => "签名不匹配（SecretKey 错误或 region 不符）",
        "AccessDenied" => "权限不足",
        "NoSuchBucket" => "Bucket 不存在",
        "NoSuchKey" => "对象不存在",
        "InvalidBucketName" => "Bucket 名称非法",
        "PermanentRedirect" | "BucketRegionError" => "endpoint 或 region 不匹配",
        _ => "",
    }
}

/// HMAC-SHA256：key + data → 字节数组。
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度 key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// HMAC-SHA256：key(Vec) + data → 字节数组（参数顺序辅助函数）。
fn hmac_sha256_vec(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac_sha256(key, data)
}

// ===========================================================================
// FileBackend 实现
// ===========================================================================

/// ListObjectsV2 XML 响应（quick-xml 反序列化）。
#[derive(Debug, Deserialize)]
#[serde(rename = "ListBucketResult")]
struct ListBucketResult {
    #[serde(rename = "Contents", default)]
    contents: Vec<ObjectElem>,
    #[serde(rename = "CommonPrefixes", default)]
    common_prefixes: Vec<CommonPrefix>,
}

#[derive(Debug, Deserialize)]
struct ObjectElem {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "Size")]
    size: String,
    #[serde(rename = "LastModified")]
    last_modified: String,
}

#[derive(Debug, Deserialize)]
struct CommonPrefix {
    #[serde(rename = "Prefix")]
    prefix: String,
}

// ===========================================================================
// rename 辅助方法（不属于 trait，是 S3Backend 的内部实现）
// ===========================================================================

impl S3Backend {
    /// 单对象重命名：CopyObject + DeleteObject（S3 无原子 rename）。
    async fn rename_object(&self, src: &str, dst: &str) -> AppResult<()> {
        // x-amz-copy-source 头值里的 key 必须按 segment 做 URL 编码（与 object_url
        // 一致），否则 key 含空格/中文等字符时复制请求会解析失败或签名不匹配。
        let src_encoded = src
            .split('/')
            .map(|seg| utf8_percent_encode(seg, S3_ENCODE_SET).to_string())
            .collect::<Vec<_>>()
            .join("/");
        let copy_source = format!("/{}/{}", self.config.bucket, src_encoded);
        let resp = self
            .signed_request(
                Method::PUT,
                dst,
                &[],
                vec![("x-amz-copy-source".into(), copy_source)],
                None,
            )
            .await?;
        if !resp.status().is_success() {
            return Err(self.err_from_response("rename(copy)", dst, resp).await);
        }
        let resp = self
            .signed_request(Method::DELETE, src, &[], vec![], None)
            .await?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(self.err_from_response("rename(delete)", src, resp).await);
        }
        Ok(())
    }

    /// 目录前缀重命名：列出 `src_prefix` 下所有对象（含 .dirkeep），逐个 copy 到
    /// `dst_prefix` 对应位置，再逐个删除源对象。非原子，但避免静默丢失目录内容。
    ///
    /// 前缀下没有任何对象（目录不存在）时返回 [`AppError::NotFound`]，避免
    /// "重命名不存在的目录却显示成功"。
    async fn rename_prefix(&self, src_prefix: &str, dst_prefix: &str) -> AppResult<()> {
        let mut found_any = false;
        let mut continuation: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".into(), "2".into()),
                ("prefix".into(), src_prefix.to_string()),
            ];
            if let Some(token) = &continuation {
                query.push(("continuation-token".into(), token.clone()));
            }
            let resp = self
                .signed_request(Method::GET, "", &query, vec![], None)
                .await?;
            if !resp.status().is_success() {
                return Err(self
                    .err_from_response("rename(list)", src_prefix, resp)
                    .await);
            }
            let xml = resp
                .text()
                .await
                .map_err(|e| AppError::Storage(format!("读取 rename 列表失败: {}", e)))?;
            let result: ListBucketResult = quick_xml::de::from_str(&xml)
                .map_err(|e| AppError::Storage(format!("解析 rename 列表失败: {}", e)))?;

            if !result.contents.is_empty() {
                found_any = true;
            }
            for obj in &result.contents {
                let rel = obj.key.strip_prefix(src_prefix).unwrap_or(&obj.key);
                let dst_key = format!("{}{}", dst_prefix, rel);
                self.rename_object(&obj.key, &dst_key).await?;
            }

            #[derive(Debug, Deserialize)]
            #[serde(rename = "ListBucketResult")]
            struct Trunc {
                #[serde(rename = "IsTruncated")]
                is_truncated: String,
                #[serde(rename = "NextContinuationToken")]
                next_token: Option<String>,
            }
            let trunc: Trunc = quick_xml::de::from_str(&xml)
                .map_err(|e| AppError::Storage(format!("解析分页信息失败: {}", e)))?;
            if trunc.is_truncated == "true" {
                continuation = trunc.next_token;
            } else {
                break;
            }
        }
        if !found_any {
            return Err(AppError::NotFound(format!("`{}` 不存在", src_prefix)));
        }
        Ok(())
    }
}

#[async_trait]
impl FileBackend for S3Backend {
    async fn list_dir(&self, path: &str) -> AppResult<Vec<FileEntry>> {
        // path 作为 prefix。约定：以 `/` 结尾或为空表示列目录；否则视为列父目录。
        let mut prefix = Self::normalize_key(path);
        if !prefix.is_empty() && !prefix.ends_with('/') {
            // 把路径视作目录前缀（补 `/`）。
            prefix.push('/');
        }

        let mut entries: Vec<FileEntry> = Vec::new();
        // ListObjectsV2 单页最多 1000 条，超过后必须用 continuation-token 翻页，
        // 否则目录条目多的列表会不完整（静默缺失）。
        let mut continuation: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".into(), "2".into()),
                ("delimiter".into(), "/".into()),
                ("prefix".into(), prefix.clone()),
            ];
            if let Some(token) = &continuation {
                query.push(("continuation-token".into(), token.clone()));
            }
            let resp = self
                .signed_request(Method::GET, "", &query, vec![], None)
                .await?;
            if !resp.status().is_success() {
                return Err(self.err_from_response("list_dir", &prefix, resp).await);
            }
            let xml = resp
                .text()
                .await
                .map_err(|e| AppError::Storage(format!("读取 list_dir 响应失败: {}", e)))?;
            let result: ListBucketResult = quick_xml::de::from_str(&xml).map_err(|e| {
                AppError::Storage(format!(
                    "解析 list_dir 响应失败: {} | 预览: {}",
                    e,
                    { xml.chars().take(160).collect::<String>() }
                ))
            })?;

            // 文件条目：跳过目录占位对象（.dirkeep）和与 prefix 完全相等的 key。
            for obj in result.contents {
                if obj.key == prefix || obj.key.ends_with(".dirkeep") {
                    continue;
                }
                // 仅保留同层（key 相对 prefix 的剩余部分不含 `/`）。
                let rel = obj.key.strip_prefix(&prefix).unwrap_or(&obj.key);
                if rel.is_empty() || rel.contains('/') {
                    continue;
                }
                entries.push(FileEntry {
                    name: rel.to_string(),
                    is_dir: false,
                    size: obj.size.parse::<u64>().unwrap_or(0),
                    modified: Some(obj.last_modified.clone()),
                });
            }
            // 子目录（CommonPrefixes）。
            for cp in result.common_prefixes {
                let rel = cp.prefix.strip_prefix(&prefix).unwrap_or(&cp.prefix);
                let name = rel.trim_end_matches('/').to_string();
                if !name.is_empty() {
                    entries.push(FileEntry {
                        name,
                        is_dir: true,
                        size: 0,
                        modified: None,
                    });
                }
            }

            // 分页：IsTruncated=true 时带 NextContinuationToken 继续翻页。
            #[derive(Debug, Deserialize)]
            #[serde(rename = "ListBucketResult")]
            struct Trunc {
                #[serde(rename = "IsTruncated")]
                is_truncated: String,
                #[serde(rename = "NextContinuationToken")]
                next_token: Option<String>,
            }
            let trunc: Trunc = quick_xml::de::from_str(&xml)
                .map_err(|e| AppError::Storage(format!("解析 list_dir 分页信息失败: {}", e)))?;
            if trunc.is_truncated == "true" {
                continuation = trunc.next_token;
            } else {
                break;
            }
        }
        Ok(entries)
    }

    async fn stat(&self, path: &str) -> AppResult<FileMeta> {
        let key = Self::normalize_key(path);
        // 先按对象查；找不到再按"目录占位"查。
        if let Some((size, modified)) = self.head_object(&key).await? {
            return Ok(FileMeta {
                size,
                is_dir: false,
                modified,
            });
        }
        // 视为目录：列出该前缀是否有内容。
        let mut prefix = key.clone();
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        let query = vec![
            ("list-type".into(), "2".into()),
            ("prefix".into(), prefix.clone()),
            ("max-keys".into(), "1".into()),
        ];
        let resp = self
            .signed_request(Method::GET, "", &query, vec![], None)
            .await?;
        if !resp.status().is_success() {
            return Err(self.err_from_response("stat", &key, resp).await);
        }
        let xml = resp
            .text()
            .await
            .map_err(|e| AppError::Storage(format!("读取 stat 响应失败: {}", e)))?;
        let result: ListBucketResult = quick_xml::de::from_str(&xml)
            .map_err(|e| AppError::Storage(format!("解析 stat 响应失败: {}", e)))?;
        if result.contents.is_empty() {
            return Err(AppError::NotFound(format!("`{}` 不存在", path)));
        }
        Ok(FileMeta {
            size: 0,
            is_dir: true,
            modified: None,
        })
    }

    async fn download(
        &self,
        remote: &str,
        local_path: &Path,
        progress: ProgressCb,
    ) -> AppResult<()> {
        let key = Self::normalize_key(remote);
        let resp = self
            .signed_request(Method::GET, &key, &[], vec![], None)
            .await?;
        if !resp.status().is_success() {
            return Err(self.err_from_response("download", &key, resp).await);
        }
        let total = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::File::create(local_path).await.map_err(|e| {
            AppError::Io(std::io::Error::new(
                e.kind(),
                format!("无法创建本地文件 `{}`: {}", local_path.display(), e),
            ))
        })?;

        // 流式写盘；中途任意错误都清理已创建的残文件，避免留下半截文件误导用户。
        let write_result: AppResult<()> = async {
            let mut stream = resp.bytes_stream();
            use futures::StreamExt;
            let mut transferred: u64 = 0;
            while let Some(chunk) = stream.next().await {
                let chunk =
                    chunk.map_err(|e| AppError::Storage(format!("读取下载流失败: {}", e)))?;
                file.write_all(&chunk).await.map_err(|e| {
                    AppError::Io(std::io::Error::new(
                        e.kind(),
                        format!("写入本地文件失败: {}", e),
                    ))
                })?;
                transferred += chunk.len() as u64;
                progress(transferred, total);
            }
            file.flush().await.map_err(|e| {
                AppError::Io(std::io::Error::new(
                    e.kind(),
                    format!("刷新本地文件失败: {}", e),
                ))
            })?;
            Ok(())
        }
        .await;

        match write_result {
            Ok(()) => Ok(()),
            Err(e) => {
                // 关闭句柄后删除残文件（忽略删除自身的错误）。
                drop(file);
                let _ = tokio::fs::remove_file(local_path).await;
                Err(e)
            }
        }
    }

    async fn upload(&self, local_path: &Path, remote: &str, progress: ProgressCb) -> AppResult<()> {
        let key = Self::normalize_key(remote);
        let meta = tokio::fs::metadata(local_path).await.map_err(|e| {
            AppError::Io(std::io::Error::new(
                e.kind(),
                format!("无法读取文件信息 `{}`: {}", local_path.display(), e),
            ))
        })?;
        let total = meta.len();

        // 流式上传：把文件读取流包装成带进度的 Stream，再交给 reqwest Body。
        // 避免 tokio::fs::read 整文件入内存（大文件会 OOM）。
        let file = tokio::fs::File::open(local_path).await.map_err(|e| {
            AppError::Io(std::io::Error::new(
                e.kind(),
                format!("无法打开本地文件 `{}`: {}", local_path.display(), e),
            ))
        })?;

        let progress_for_stream = progress.clone();
        let chunk_total = total;
        let stream = async_stream::stream! {
            use tokio::io::AsyncReadExt;
            let mut reader = file;
            let mut transferred: u64 = 0;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        yield Err(std::io::Error::new(e.kind(), format!("读取本地文件失败: {}", e)));
                        return;
                    }
                };
                transferred += n as u64;
                progress_for_stream(transferred, chunk_total);
                yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n]));
            }
        };
        let body = reqwest::Body::wrap_stream(stream);

        let resp = self.signed_streaming_put(&key, body, total).await?;
        if !resp.status().is_success() {
            return Err(self.err_from_response("upload", &key, resp).await);
        }
        Ok(())
    }

    async fn rename(&self, oldpath: &str, newpath: &str) -> AppResult<()> {
        let src = Self::normalize_key(oldpath);
        let dst = Self::normalize_key(newpath);

        // 判定源是单对象还是"目录"（前缀）：
        // 先 HEAD 单对象；命中则走单对象 copy+delete；否则按前缀递归处理。
        let is_single = self.head_object(&src).await?.is_some();

        if is_single {
            return self.rename_object(&src, &dst).await;
        }
        // 视为目录前缀：src/dst 补 `/`。
        let mut src_prefix = src.clone();
        if !src_prefix.ends_with('/') {
            src_prefix.push('/');
        }
        let mut dst_prefix = dst.clone();
        if !dst_prefix.ends_with('/') {
            dst_prefix.push('/');
        }
        self.rename_prefix(&src_prefix, &dst_prefix).await
    }

    async fn mkdir(&self, path: &str) -> AppResult<()> {
        // 写入 0 字节占位对象使目录在前端可见。
        let mut key = Self::normalize_key(path);
        if !key.ends_with('/') {
            key.push('/');
        }
        key.push_str(".dirkeep");
        let resp = self
            .signed_request(Method::PUT, &key, &[], vec![], Some(Vec::new()))
            .await?;
        if !resp.status().is_success() {
            return Err(self.err_from_response("mkdir", &key, resp).await);
        }
        Ok(())
    }

    async fn remove_file(&self, path: &str) -> AppResult<()> {
        let key = Self::normalize_key(path);
        let resp = self
            .signed_request(Method::DELETE, &key, &[], vec![], None)
            .await?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(self.err_from_response("remove_file", &key, resp).await);
        }
        Ok(())
    }

    async fn remove_dir(&self, path: &str) -> AppResult<()> {
        // 列出前缀下所有对象，用 DeleteObjects 批量接口删除（一次最多 1000 个，远快于逐个 DELETE）。
        let mut prefix = Self::normalize_key(path);
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        let mut continuation: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".into(), "2".into()),
                ("prefix".into(), prefix.clone()),
            ];
            if let Some(token) = &continuation {
                query.push(("continuation-token".into(), token.clone()));
            }
            let resp = self
                .signed_request(Method::GET, "", &query, vec![], None)
                .await?;
            if !resp.status().is_success() {
                return Err(self
                    .err_from_response("remove_dir(list)", &prefix, resp)
                    .await);
            }
            let xml = resp
                .text()
                .await
                .map_err(|e| AppError::Storage(format!("读取 remove_dir 列表失败: {}", e)))?;
            let result: ListBucketResult = quick_xml::de::from_str(&xml)
                .map_err(|e| AppError::Storage(format!("解析 remove_dir 列表失败: {}", e)))?;

            // 批量删除本页对象（DeleteObjects 一次上限 1000，ListObjectsV2 默认每页 1000）。
            if !result.contents.is_empty() {
                self.delete_objects_batch(
                    &result
                        .contents
                        .iter()
                        .map(|o| o.key.as_str())
                        .collect::<Vec<_>>(),
                )
                .await?;
            }

            #[derive(Debug, Deserialize)]
            #[serde(rename = "ListBucketResult")]
            struct Trunc {
                #[serde(rename = "IsTruncated")]
                is_truncated: String,
                #[serde(rename = "NextContinuationToken")]
                next_token: Option<String>,
            }
            let trunc: Trunc = quick_xml::de::from_str(&xml)
                .map_err(|e| AppError::Storage(format!("解析分页信息失败: {}", e)))?;
            if trunc.is_truncated == "true" {
                continuation = trunc.next_token;
            } else {
                break;
            }
        }
        Ok(())
    }
}

// ===========================================================================
// 批量删除辅助（不属于 trait，S3Backend 内部实现）
// ===========================================================================

impl S3Backend {
    /// S3 DeleteObjects 批量删除：POST /<bucket>?delete=，body 为 `<Delete>` XML。
    /// 一次最多 1000 个对象（S3 规范上限）。返回所有失败项的汇总错误。
    ///
    /// 与逐个 DELETE 相比，1000 个对象只需一次 HTTP 往返，万级对象场景快一个量级。
    async fn delete_objects_batch(&self, keys: &[&str]) -> AppResult<()> {
        if keys.is_empty() {
            return Ok(());
        }
        // 构造请求体：<Delete><Object><Key>...</Key></Object>...<Quiet>false</Quiet></Delete>
        // Quiet=false 让服务端返回每个对象的结果，便于检测失败。
        let mut body = String::from("<Delete><Quiet>false</Quiet>");
        for k in keys {
            // 对 key 做 XML 转义（& < > " '）。
            let escaped = k
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            body.push_str(&format!("<Object><Key>{}</Key></Object>", escaped));
        }
        body.push_str("</Delete>");

        let resp = self
            .signed_request(
                Method::POST,
                "",
                &[("delete".into(), "".into())],
                vec![(
                    "content-md5".into(),
                    // S3 要求 DeleteObjects 请求带 Content-MD5（或 x-amz-content-sha256 已签，
                    // 多数兼容存储不强制 MD5，但 AWS 强制）。这里计算 MD5 base64。
                    md5_base64(body.as_bytes()),
                )],
                Some(body.into_bytes()),
            )
            .await?;
        if !resp.status().is_success() {
            return Err(self
                .err_from_response("delete_objects_batch", "", resp)
                .await);
        }
        // 解析响应里的失败项：<DeleteResult><Error><Key>...</Key><Code>...</Code>...</Error></DeleteResult>
        let resp_xml = resp
            .text()
            .await
            .map_err(|e| AppError::Storage(format!("读取 DeleteObjects 响应失败: {}", e)))?;
        #[derive(Debug, Deserialize)]
        #[serde(rename = "DeleteResult")]
        struct DeleteResult {
            #[serde(rename = "Error", default)]
            errors: Vec<DeleteError>,
        }
        #[derive(Debug, Deserialize)]
        struct DeleteError {
            #[serde(rename = "Key", default)]
            key: String,
            #[serde(rename = "Code", default)]
            code: String,
            #[serde(rename = "Message", default)]
            message: String,
        }
        let result: DeleteResult = quick_xml::de::from_str(&resp_xml)
            .map_err(|e| AppError::Storage(format!("解析 DeleteObjects 响应失败: {}", e)))?;
        if !result.errors.is_empty() {
            let detail = result
                .errors
                .iter()
                .take(5)
                .map(|e| format!("{}[{}]: {}", e.key, e.code, e.message))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(AppError::Storage(format!(
                "部分对象删除失败（共{}个）: {}",
                result.errors.len(),
                detail
            )));
        }
        Ok(())
    }
}

/// 计算 MD5 的 base64 编码（S3 DeleteObjects 要求的 Content-MD5 头）。
fn md5_base64(data: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let digest = md5_hash(data);
    STANDARD.encode(digest)
}

/// MD5 摘要（手写或引入 crate）。用 sha2 已有的依赖生态不可行，这里引入 md-5。
fn md5_hash(data: &[u8]) -> [u8; 16] {
    use md5::{Digest as _, Md5};
    let mut h = Md5::new();
    h.update(data);
    let digest = h.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest);
    out
}
