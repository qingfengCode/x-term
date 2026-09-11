//! 终端多字符编码（GBK / GB18030 / Big5 / Shift-JIS / EUC-KR）。
//!
//! 转码职责划分：
//! - **输入方向**（前端 UTF-8 按键 → 远端字节）：在本模块 [`encode_input`]
//!   完成，由 `terminal_write` 统一调用；`raw` 旁路保护 ZMODEM 上传的
//!   协议帧/文件内容（二进制不可转码），本地终端（ConPTY 恒 UTF-8）跳过。
//! - **输出方向**（远端字节 → 前端渲染）：由前端 TextDecoder 流式解码
//!   （TerminalPane，位于 ZMODEM 哨兵之后，协议二进制同样不受影响），
//!   后端 reader 不做转换——保证 attach 回放/输出日志/AI 快照与事件流
//!   拿到的都是服务器原始字节。

use encoding_rs::Encoding;

/// 按设置标签解析目标编码；UTF-8 / 空标签 / 未知标签返回 None（无需转码）。
pub fn resolve(label: &str) -> Option<&'static Encoding> {
    let label = label.trim();
    if label.is_empty()
        || label.eq_ignore_ascii_case("utf-8")
        || label.eq_ignore_ascii_case("utf8")
    {
        return None;
    }
    Encoding::for_label(label.as_bytes())
}

/// 键盘输入：UTF-8 字节 → 目标编码字节。
///
/// 前端 TextEncoder 产出的必是完整 UTF-8 序列（onData 一次给整串，不会切断
/// 码点），无需处理残缺多字节。encoding_rs 对不可映射字符默认输出 HTML 数字
/// 字符引用（`&#NNNN;`）——终端没有 HTML 语义，且**绝不能**在编码后的字节上
/// 做文本域清理（GBK 等字节不是合法 UTF-8，`from_utf8_lossy`/字符串正则会把
/// 整批非 ASCII 字符破坏成 U+FFFD）。这里在**编码前的 UTF-8 源文本**上逐字符
/// 预扫描：不可映射字符（emoji 等）替换为 '?'，之后编码永不产生 NCR。
pub fn encode_input(label: &str, utf8_bytes: &[u8]) -> Vec<u8> {
    let Some(encoding) = resolve(label) else {
        return utf8_bytes.to_vec();
    };
    let text = String::from_utf8_lossy(utf8_bytes).into_owned();
    // 快路径：全部可映射（绝大多数输入是 ASCII 命令），单次编码完成。
    let (out, _, had_errors) = encoding.encode(&text);
    if !had_errors {
        return out.into_owned();
    }
    // 慢路径：存在不可映射字符——回到 UTF-8 文本域逐字符清洗后再编码。
    let cleaned: String = text
        .chars()
        .map(|c| {
            let (_, _, err) = encoding.encode(&c.to_string());
            if err {
                '?'
            } else {
                c
            }
        })
        .collect();
    let (out, _, _) = encoding.encode(&cleaned);
    out.into_owned()
}
