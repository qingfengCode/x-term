//! 公共工具函数。
//!
//! 抽取各模块共用的辅助函数，避免重复实现。

use once_cell::sync::Lazy;
use regex::Regex;

use crate::database::mysql::QueryResult;

// ===========================================================================
// ANSI 转义剥离
// ===========================================================================

static CSI_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap());
static OSC_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").unwrap());
static SINGLE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\x1b[@-_]").unwrap());

/// 剥离常见的 ANSI 转义序列（CSI、OSC、单字符）。
///
/// 用于把 exec 输出中残留的颜色/光标控制序列去掉，得到干净文本。
pub fn strip_ansi(s: &str) -> String {
    let s = CSI_RE.replace_all(s, "");
    let s = OSC_RE.replace_all(&s, "");
    let s = SINGLE_RE.replace_all(&s, "");
    s.into_owned()
}

// ===========================================================================
// 查询结果格式化
// ===========================================================================

/// 把 [`QueryResult`] 格式化成可读的对齐文本表格。
///
/// 首行列名，分隔线，每行数据，末尾行数。
pub fn format_query_result(qr: &QueryResult) -> String {
    if qr.columns.is_empty() {
        // 非查询语句。
        return format!("OK，影响 {} 行", qr.affected);
    }
    let mut out = String::new();
    out.push_str(&qr.columns.join(" | "));
    out.push('\n');
    out.push_str(&"-".repeat(qr.columns.iter().map(|c| c.len() + 3).sum::<usize>()));
    out.push('\n');
    for row in &qr.rows {
        out.push_str(&row.join(" | "));
        out.push('\n');
    }
    if qr.rows.is_empty() {
        out.push_str("(无数据)\n");
    }
    out.push_str(&format!("共 {} 行", qr.rows.len()));
    out
}
