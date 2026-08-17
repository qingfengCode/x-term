//! 应用数据目录管理及 JSON 文件的原子读写。
//!
//! 应用数据目录在 Windows 下为 `%APPDATA%/x-term`，在 macOS 下为
//! `~/Library/Application Support/x-term`，在 Linux 下为 `~/.local/share/x-term`
//! （由 [`dirs::data_dir`] 给出平台基目录）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::{AppError, AppResult};

/// 应用在用户数据目录下使用的子目录名。
const APP_DIR_NAME: &str = "x-term";

/// 全局写锁：序列化所有 JSON 写入（文件小、低频），避免并发写同一文件时
/// load-modify-save 互相覆盖或两个进程内写入共用 tmp 文件名互相踩踏。
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// tmp 文件序号：保证每次写入使用唯一临时文件名，防止并发写时同路径
/// `<path>.tmp` 互相覆盖（一个写一半另一个 rename，产生损坏文件）。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 返回应用数据目录，并确保它存在。
///
/// - Windows: `%APPDATA%/x-term`
/// - macOS:   `~/Library/Application Support/x-term`
/// - Linux:   `~/.local/share/x-term`
pub fn app_data_dir() -> AppResult<PathBuf> {
    let base =
        dirs::data_dir().ok_or_else(|| AppError::Config("无法确定系统应用数据目录".to_string()))?;
    let dir = base.join(APP_DIR_NAME);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 从 JSON 文件读取并反序列化。
///
/// 文件不存在时返回 [`AppError::InvalidInput`]，调用方可以据此判断并使用默认值或
/// 创建新文件；如需更便利的版本，请使用 [`read_json_or_default`]。
pub fn read_json<T: DeserializeOwned>(path: &Path) -> AppResult<T> {
    if !path.exists() {
        return Err(AppError::InvalidInput(format!(
            "文件不存在: {}",
            path.display()
        )));
    }
    let content = std::fs::read_to_string(path)?;
    let value = serde_json::from_str(&content)?;
    Ok(value)
}

/// 从 JSON 文件读取；文件不存在或解析失败时返回 `T::default()`。
///
/// 文件**存在但解析失败**（损坏）时，先把损坏文件改名为
/// `<path>.corrupt-<pid>-<seq>` 留证并记录日志，再返回默认值——绝不静默吞掉：
/// 若直接返回默认值，下次保存会把损坏文件覆盖，原配置永久丢失且无备份。
/// 改名留证后，用户可以事后从 .corrupt 文件恢复，或至少定位损坏原因。
pub fn read_json_or_default<T>(path: &Path) -> AppResult<T>
where
    T: Default + DeserializeOwned,
{
    if !path.exists() {
        return Ok(T::default());
    }
    let content = std::fs::read_to_string(path)?;
    match serde_json::from_str::<T>(&content) {
        Ok(v) => Ok(v),
        Err(e) => {
            // 损坏：改名留证而非静默丢弃。
            let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let backup = path.with_extension(format!("corrupt-{}-{}", std::process::id(), seq));
            if std::fs::rename(path, &backup).is_ok() {
                log::warn!(
                    "配置文件 `{}` 解析失败（{}），已改名为 `{}` 留证，本次使用默认配置",
                    path.display(),
                    e,
                    backup.display()
                );
            } else {
                log::warn!(
                    "配置文件 `{}` 解析失败（{}），且改名留证失败，本次使用默认配置（原文件将保留在磁盘上）",
                    path.display(),
                    e
                );
            }
            Ok(T::default())
        }
    }
}

/// 把值序列化为 JSON 并原子化写入。
///
/// 写入流程：先写到唯一临时文件（`"<path>.tmp-<pid>-<seq>"`），再两阶段替换：
/// 1. 若目标已存在，先把它 rename 成唯一备份名 `"<path>.bak-<pid>-<seq>"`
///    （rename 不删除任何数据，这一步失败则目标文件原样保留、直接报错）；
/// 2. 把临时文件 rename 到目标路径（rename 在同一文件系统上是原子的）；
/// 3. 成功后删除 .bak 备份。
///
/// 相比"先删除目标再 rename"（Windows 上 rename 不能覆盖已存在文件，旧实现
/// 先删目标），两阶段方案在任意一步之间崩溃都不会出现"目标文件消失"的窗口：
/// 目标路径要么是旧内容、要么是完整新内容，最多残留 .bak/.tmp 文件。
/// 所有写入经全局 [`WRITE_LOCK`] 串行化，且 tmp/bak 文件名每次唯一，
/// 避免并发写同一路径时互相覆盖。
pub fn write_json<T: Serialize>(path: &Path, v: &T) -> AppResult<()> {
    // 全局串行化写（内容小、调用低频，全局锁足够）。
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // 确保父目录存在。
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = path.with_extension(format!("tmp-{}-{}", std::process::id(), seq));
    {
        let content = serde_json::to_string_pretty(v)?;
        std::fs::write(&tmp_path, content)?;
    }

    // 阶段 1：目标已存在时先挪到 .bak（唯一名，不会覆盖任何已有文件）。
    let bak_path = path.with_extension(format!("bak-{}-{}", std::process::id(), seq));
    if path.exists() {
        std::fs::rename(path, &bak_path)?;
    }

    // 阶段 2：临时文件 rename 到目标路径。
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        // 失败：把旧文件从 .bak 挪回原位（best-effort），并清理临时文件，
        // 保证目标路径不因本次失败而丢失数据。
        if bak_path.exists() {
            let _ = std::fs::rename(&bak_path, path);
        }
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }

    // 阶段 3：新文件已就位，删除备份。
    if bak_path.exists() {
        if let Err(e) = std::fs::remove_file(&bak_path) {
            // 删不掉只留一份 .bak 残件（内容为旧数据），不影响正确性。
            log::warn!("删除旧文件备份 `{}` 失败: {}", bak_path.display(), e);
        }
    }
    Ok(())
}
