//! 堡垒机会话文件上传的**纯逻辑**部分（base64 分块 / 远端命令拼装 / 输出解析）。
//!
//! 为什么走 base64 而不是 SFTP：堡垒机模式下我们只有一条**进到目标主机的
//! 交互式 shell**（PTY）——绑定的 SSH 连接是堡垒机本身，不是资产主机，没有到
//! 目标主机的 SFTP 通道。所以把本地文件 base64 后分块 `printf` 追加到远端临时
//! 文件，最后 `base64 -d` 解码，并用字节数（尽力再加 sha256）校验结果。
//!
//! # 分块为什么这么小
//!
//! 交互式 shell 的终端输入队列（内核 N_TTY）只有 4096 字节。每一块必须以
//! **单行命令**写入，且整行长度必须 < 4096——否则当 shell 正忙于执行上一条命令
//! （此时不读 stdin）时，队列会溢出并**静默丢弃字符**。本模块的做法是：整行
//! 长度 = [`CHUNK_CHARS`] + 固定开销（约 60 字节），稳定小于 4096；并且调用方
//! 每块都等哨兵确认（意味着 shell 已回到提示符、重新开始读 stdin）才写下一条。
//! 两者结合，从构造上消除丢字符的可能。
//!
//! 代价是吞吐受往返延迟限制（每块约 2.2 KiB 原始数据），因此设了
//! [`MAX_UPLOAD_BYTES`] 上限；更大文件建议改用其它通道或拆包传输。
//!
//! 本模块只放纯函数（便于单测）；PTY 读写编排见 `bastion::upload_file`。

use std::path::Path;

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};

/// 单块 base64 字符数。
///
/// 整行 = `printf '%s' '<本块>' >> '<临时文件>'`，长度约 [`CHUNK_CHARS`] + 60，
/// 必须小于终端输入队列的 4096 字节（见模块注释）。
pub const CHUNK_CHARS: usize = 3000;

/// 单个文件大小上限。
///
/// 受分块往返限制（每块约 2.2 KiB），上限取 32 MiB——按常见局域网/堡垒机往返
/// 估算仍在 15 分钟传输超时内；配置文件、脚本、几 MiB 的二进制都够用。
pub const MAX_UPLOAD_BYTES: u64 = 32 * 1024 * 1024;

/// 数值回显前缀（`wc -c` 的结果）。
///
/// 所有需要读回数值的命令都刻意把结果包成 `XTSIZE=<数字>`：哨兵回显里也会带上
/// 这段命令原文，若直接"取输出里的第一个数字"，在回显剥离失败时（长命令在
/// readline 里换行可能插入空格，此时 `clean_window` 会放弃剥离）可能把命令原文
/// 里的数字（如临时文件名 `.xterm-up-12345678.b64`）误当成结果。加前缀 + 取
/// **最后一次**出现，可保证读到的永远是命令的执行结果。
const SIZE_PREFIX: &str = "XTSIZE=";
/// 哈希回显前缀（`sha256sum` 的结果）。
const SHA_PREFIX: &str = "XTSHA=";

/// 上传前的准备结果（本地文件内容 + 远端命令所需信息）。
pub struct PreparedUpload {
    /// 文件字节数。
    pub size: u64,
    /// 本地文件 sha256（小写十六进制；远端无 sha256sum 时跳过比对）。
    pub sha256: String,
    /// 整份 base64 文本。
    pub b64: String,
    /// 远端临时文件路径（分块追加的目标，收尾时删除）。
    pub temp_path: String,
}

/// 读取并准备本地文件（base64 编码 + sha256 + 生成远端临时文件名）。
pub fn prepare(local_path: &Path) -> AppResult<PreparedUpload> {
    let meta = std::fs::metadata(local_path).map_err(|e| {
        AppError::InvalidInput(format!("读取本地文件失败 {}: {e}", local_path.display()))
    })?;
    if !meta.is_file() {
        return Err(AppError::InvalidInput(format!(
            "本地路径不是普通文件：{}",
            local_path.display()
        )));
    }
    let size = meta.len();
    if size > MAX_UPLOAD_BYTES {
        return Err(AppError::InvalidInput(format!(
            "文件过大（{} 字节）：经会话 shell 上传的上限为 {} MiB。请改用其它通道，或先在本地拆分后再传",
            size,
            MAX_UPLOAD_BYTES / 1024 / 1024
        )));
    }
    let bytes = std::fs::read(local_path)
        .map_err(|e| AppError::InvalidInput(format!("读取本地文件失败: {e}")))?;
    Ok(PreparedUpload {
        size,
        sha256: hex::encode(Sha256::digest(&bytes)),
        b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        temp_path: format!(
            "/tmp/.xterm-up-{}.b64",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ),
    })
}

/// 追加一块 base64 到远端临时文件（单行，长度受控）。
pub fn append_command(piece: &str, temp_path: &str) -> String {
    format!("printf '%s' '{piece}' >> {}", shell_quote(temp_path))
}

/// 校验远端临时文件的字节数（应等于 base64 文本长度）。
pub fn temp_size_command(temp_path: &str) -> String {
    format!(
        "echo \"{SIZE_PREFIX}$(wc -c < {})\"",
        shell_quote(temp_path)
    )
}

/// 解码临时文件到目标路径、删除临时文件，并回显目标文件字节数。
pub fn finalize_command(temp_path: &str, target_path: &str) -> String {
    let temp = shell_quote(temp_path);
    let target = shell_quote(target_path);
    format!(
        "base64 -d < {temp} > {target} && rm -f {temp} && echo \"{SIZE_PREFIX}$(wc -c < {target})\""
    )
}

/// 创建空目标文件并回显其字节数（本地文件为 0 字节时走这条，绕开 base64 链路）。
pub fn create_empty_command(target_path: &str) -> String {
    let target = shell_quote(target_path);
    format!(": > {target} && echo \"{SIZE_PREFIX}$(wc -c < {target})\"")
}

/// 尽力校验目标文件的 sha256（远端没有 sha256sum 时回显 `NOHASH`）。
pub fn sha256_command(target_path: &str) -> String {
    let target = shell_quote(target_path);
    format!(
        "command -v sha256sum >/dev/null 2>&1 && echo \"{SHA_PREFIX}$(sha256sum < {target})\" || echo NOHASH"
    )
}

/// 失败清理：删除远端临时文件（调用方忽略其结果）。
pub fn cleanup_command(temp_path: &str) -> String {
    format!("rm -f {}", shell_quote(temp_path))
}

/// POSIX 单引号转义（路径含空格 / 特殊字符时不破坏命令）。
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 解析命令回显的字节数（取**最后一次**出现的 `XTSIZE=` 之后紧跟的数字）。
///
/// 取最后一次：哨兵回显里也会出现命令原文（含 `XTSIZE=` 字样，后面跟的是
/// `$(wc -c …` 而非数字），真正结果总在其后。命中回显那处时解析出空串 → 返回
/// None → 调用方按"命令没执行/失败"处理，绝不会读到一个错误的数字。
pub fn parse_size(out: &str) -> Option<u64> {
    let idx = out.rfind(SIZE_PREFIX)?;
    let rest = &out[idx + SIZE_PREFIX.len()..];
    let digits: String = rest
        .chars()
        .skip_while(|c: &char| c.is_whitespace())
        .take_while(|c: &char| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// 解析 `sha256sum` 回显里的哈希（`XTSHA=` 之后首个 64 位十六进制 token）。
///
/// `NOHASH`（远端无 sha256sum）或未执行（回显里只有 `XTSHA=` 后跟 `$(...)`）
/// 都返回 None。
pub fn parse_sha256(out: &str) -> Option<String> {
    let idx = out.rfind(SHA_PREFIX)?;
    let rest = &out[idx + SHA_PREFIX.len()..];
    rest.split_whitespace()
        .find(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_line_stays_within_tty_buffer() {
        // 最坏情况：整行长度（含哨兵后缀）必须小于终端输入队列的 4096 字节，
        // 否则 shell 忙于执行上一条命令时会丢字符。
        let piece = "A".repeat(CHUNK_CHARS);
        let line = append_command(&piece, "/tmp/.xterm-up-12345678.b64");
        // 哨兵后缀 `; echo __XTERM_BASTION_<16 位十六进制>__` 约 41 字节。
        let full = line.len() + 41;
        assert!(full < 4096, "整行 {full} 字节，超过终端输入队列上限");
        assert!(full < 3900, "整行 {full} 字节，余量不足");
    }

    #[test]
    fn commands_quote_paths() {
        assert_eq!(
            append_command("QUJD", "/tmp/a.b64"),
            "printf '%s' 'QUJD' >> '/tmp/a.b64'"
        );
        assert_eq!(
            finalize_command("/tmp/a.b64", "/opt/my app/x.bin"),
            "base64 -d < '/tmp/a.b64' > '/opt/my app/x.bin' && rm -f '/tmp/a.b64' \
             && echo \"XTSIZE=$(wc -c < '/opt/my app/x.bin')\""
        );
        // 路径里的单引号按 POSIX 规则转义（终止-转义-续接），命令不被截断。
        assert_eq!(shell_quote("/a'b"), "'/a'\\''b'");
        assert_eq!(
            create_empty_command("/tmp/x"),
            ": > '/tmp/x' && echo \"XTSIZE=$(wc -c < '/tmp/x')\""
        );
    }

    #[test]
    fn parses_prefixed_size_not_echoed_digits() {
        // 正常情况：只有命令输出。
        assert_eq!(parse_size("XTSIZE=12345\n"), Some(12345));
        assert_eq!(parse_size("XTSIZE=0"), Some(0));

        // 回显未被剥离（长命令在 readline 里换行时可能发生）时必须取执行结果，
        // 而不是命令原文里的数字——这里临名文件名刻意全为数字，复现该场景。
        let with_echo = "echo \"XTSIZE=$(wc -c < '/tmp/.xterm-up-12345678.b64')\"\r\n\
                         XTSIZE=4096\r\nroot@web01:~# ";
        assert_eq!(parse_size(with_echo), Some(4096));

        // 命令未执行（只有回显、没有结果）→ None，而不是误读文件名里的 12345678。
        let echo_only = "echo \"XTSIZE=$(wc -c < '/tmp/.xterm-up-12345678.b64')\"\r\nroot@web01:~# ";
        assert_eq!(parse_size(echo_only), None);

        assert_eq!(parse_size("no prefix here 123"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn parses_prefixed_sha256() {
        let hash = "a".repeat(64);
        assert_eq!(parse_sha256(&format!("XTSHA={hash}  -\n")), Some(hash.clone()));
        // 回显 + 结果：取结果。
        assert_eq!(
            parse_sha256(&format!(
                "echo \"XTSHA=$(sha256sum < '/x')\"\r\nXTSHA={hash}  -\r\n"
            )),
            Some(hash.clone())
        );
        // 远端没有 sha256sum → NOHASH → None（跳过哈希校验，只信字节数）。
        assert_eq!(parse_sha256("NOHASH\n"), None);
        assert_eq!(parse_sha256(""), None);
        // 长度对但不是十六进制 → 不认。
        assert_eq!(parse_sha256(&format!("XTSHA={}", "z".repeat(64))), None);
    }
}
