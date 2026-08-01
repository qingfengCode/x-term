//! 应用自更新模块（自定义更新器）。
//!
//! 面向内网自建服务器的更新方案，无需代码签名：
//! 1. [`check`]：从清单 URL 拉取 [`UpdateManifest`]，与当前版本比对，返回是否有新版。
//! 2. [`download`]：流式下载安装包到本地，边下载边 emit 进度事件，可选 sha256 校验。
//! 3. [`install_and_exit`]：拉起安装器（NSIS）并退出当前进程，由安装器完成覆盖升级。
//!
//! 清单格式（JSON）：
//! ```json
//! { "version": "0.2.0", "notes": "…", "url": "https://…/setup.exe", "sha256": "…" }
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::AppHandle;
use tokio::io::AsyncWriteExt;

use crate::error::{AppError, AppResult};
use crate::events::{self, UpdateProgressEvent};

/// 清单请求 / 下载分块读取的超时上限。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 进度事件节流：累计下载超过该字节数才再次 emit（避免刷屏）。
const PROGRESS_EMIT_INTERVAL: u64 = 128 * 1024;

/// 全局复用的 HTTP 客户端（rustls，连接池复用）。
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("X-Term/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("构建 reqwest 客户端失败")
});

// ===========================================================================
// 清单
// ===========================================================================

/// 更新清单：自建服务器上的 `update.json`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateManifest {
    /// 远端最新版本号（如 `0.2.0`）。
    pub version: String,
    /// 更新日志 / 发布说明（可含多行）。
    #[serde(default)]
    pub notes: String,
    /// 安装包下载地址。
    pub url: String,
    /// 安装包 sha256（十六进制，可选）。提供时下载后会校验完整性。
    #[serde(default)]
    pub sha256: Option<String>,
}

/// 比较两个 `x.y.z` 形式的版本号，判断 `remote` 是否严格大于 `current`。
///
/// 逐段按数值比较；段数不足按 0 处理；无法解析的段按 0 处理。
/// 这样 `0.10.0 > 0.9.0` 成立，且对带前缀（如 `v0.2.0`）也能容错。
pub fn is_newer(remote: &str, current: &str) -> bool {
    fn parse(v: &str) -> Vec<u64> {
        v.trim()
            .trim_start_matches(['v', 'V'])
            .split('.')
            .map(|s| s.trim().parse::<u64>().unwrap_or(0))
            .collect()
    }
    let r = parse(remote);
    let c = parse(current);
    let len = r.len().max(c.len());
    for i in 0..len {
        let ri = *r.get(i).unwrap_or(&0);
        let ci = *c.get(i).unwrap_or(&0);
        if ri != ci {
            return ri > ci;
        }
    }
    false
}

/// 拉取清单并比对版本。
///
/// - 返回 `Ok(Some(manifest))`：有可用更新；
/// - 返回 `Ok(None)`：清单可达但当前已是最新；
/// - 返回 `Err`：网络 / 解析失败。
pub async fn check(manifest_url: &str, current_version: &str) -> AppResult<Option<UpdateManifest>> {
    let resp = HTTP
        .get(manifest_url)
        .send()
        .await
        .map_err(|e| AppError::Update(format!("请求更新清单失败: {}", e)))?;
    if !resp.status().is_success() {
        return Err(AppError::Update(format!(
            "更新清单返回状态码 {}",
            resp.status()
        )));
    }
    let manifest: UpdateManifest = resp
        .json()
        .await
        .map_err(|e| AppError::Update(format!("解析更新清单失败: {}", e)))?;
    if is_newer(&manifest.version, current_version) {
        Ok(Some(manifest))
    } else {
        Ok(None)
    }
}

// ===========================================================================
// 下载
// ===========================================================================

/// 流式下载安装包到 `dest_dir`，返回落地文件路径。
///
/// 进度通过 [`events::UPDATE_PROGRESS`] 事件推送（节流后）。若清单提供 sha256，
/// 下载完成后校验；不一致则删除文件并报错。
pub async fn download(
    app: &AppHandle,
    manifest: &UpdateManifest,
    dest_dir: &Path,
) -> AppResult<PathBuf> {
    tokio::fs::create_dir_all(dest_dir)
        .await
        .map_err(|e| AppError::Update(format!("创建下载目录失败: {}", e)))?;

    // 文件名取 URL 末段，兜底用版本号命名。
    let file_name = manifest
        .url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("X-Term_{}_setup.exe", manifest.version));
    let dest = dest_dir.join(file_name);
    // 临时文件：下载完整 + 校验通过后再原子改名，避免半成品被当作可用安装包。
    let tmp = dest.with_extension("part");

    let resp = HTTP
        .get(&manifest.url)
        .send()
        .await
        .map_err(|e| AppError::Update(format!("下载安装包失败: {}", e)))?;
    if !resp.status().is_success() {
        return Err(AppError::Update(format!(
            "下载安装包返回状态码 {}",
            resp.status()
        )));
    }

    let total = resp.content_length().unwrap_or(0);
    let mut stream = resp.bytes_stream();
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| AppError::Update(format!("创建临时文件失败: {}", e)))?;

    let mut received: u64 = 0;
    let mut last_emitted: u64 = 0;
    let mut hasher = Sha256::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| AppError::Update(format!("下载中断: {}", e)))?;
        received += chunk.len() as u64;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| AppError::Update(format!("写入安装包失败: {}", e)))?;

        // 节流 emit：首次、达到间隔、或结束时推送。
        if received - last_emitted >= PROGRESS_EMIT_INTERVAL {
            last_emitted = received;
            emit_progress(app, received, total);
        }
    }
    file.flush()
        .await
        .map_err(|e| AppError::Update(format!("刷新安装包失败: {}", e)))?;
    drop(file);

    // 收尾：补发 100% 进度。
    emit_progress(app, received, total);

    // 完整性校验。
    if let Some(expected) = manifest.sha256.as_deref().filter(|s| !s.is_empty()) {
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(AppError::Update(format!(
                "安装包校验失败：期望 sha256 {}，实际 {}",
                expected, actual
            )));
        }
    }

    // 覆盖旧文件（若存在）后原子改名。
    let _ = tokio::fs::remove_file(&dest).await;
    tokio::fs::rename(&tmp, &dest)
        .await
        .map_err(|e| AppError::Update(format!("保存安装包失败: {}", e)))?;

    Ok(dest)
}

/// 计算并广播一次下载进度。
fn emit_progress(app: &AppHandle, received: u64, total: u64) {
    let percent = if total > 0 {
        ((received as f64 / total as f64) * 100.0).round().clamp(0.0, 100.0) as u8
    } else {
        0
    };
    events::emit(
        app,
        events::UPDATE_PROGRESS,
        UpdateProgressEvent { received, total, percent },
    );
}

// ===========================================================================
// 安装
// ===========================================================================

/// 拉起安装器并退出当前进程。
///
/// Windows 下用 `cmd /C start "" <installer>` 让安装器在独立进程运行，随后退出本应用，
/// 由 NSIS 安装器接管覆盖升级。
pub fn install_and_exit(app: &AppHandle, installer: &Path) -> AppResult<()> {
    if !installer.exists() {
        return Err(AppError::Update(format!(
            "安装包不存在: {}",
            installer.display()
        )));
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &installer.to_string_lossy()])
            .spawn()
            .map_err(|e| AppError::Update(format!("启动安装器失败: {}", e)))?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new(installer)
            .spawn()
            .map_err(|e| AppError::Update(format!("启动安装器失败: {}", e)))?;
    }

    // 触发 Tauri 正常退出流程（含清理），由安装器接管升级。
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("v0.2.0", "0.1.0")); // 容忍 v 前缀
        assert!(!is_newer("0.1.0", "0.1.0")); // 相等
        assert!(!is_newer("0.1.0", "0.2.0")); // 降级
        assert!(!is_newer("0.1", "0.1.0")); // 段数不足按 0
    }
}
