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

/// 结果文本的总字节上限。
///
/// 单个单元格可达 LONGTEXT 级别，limit 行 × 大单元格能拼出数百 MB 字符串
/// （同时推给模型 / IPC / 日志）。超限后按整行截断并给出明确提示。
const MAX_FORMAT_BYTES: usize = 256 * 1024;

/// 返回不超过 `max` 的最大 UTF-8 字符边界（std 的 `floor_char_boundary`
/// 尚未稳定，这里自带一份等价实现）。
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

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
    let mut shown = 0usize;
    let mut size_capped = false;
    for row in &qr.rows {
        let mut line = row.join(" | ");
        line.push('\n');
        if out.len() + line.len() > MAX_FORMAT_BYTES {
            // 当前行会顶穿总量上限：按字符边界截断该行后停止，避免静默
            // 生成巨型字符串（也避免完全无输出）。
            let budget = MAX_FORMAT_BYTES.saturating_sub(out.len());
            let cut = floor_char_boundary(&line, budget);
            if cut > 0 {
                out.push_str(&line[..cut]);
                out.push_str("…(单行超长已截断)\n");
                shown += 1;
            }
            size_capped = true;
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if qr.rows.is_empty() {
        out.push_str("(无数据)\n");
    }
    if size_capped {
        // 面向模型的截断提示：明确告知是"输出体积"而非"行数"受限，
        // 避免模型误以为结果完整。
        out.push_str(&format!(
            "显示前 {shown} 行（输出超过 {MAX_FORMAT_BYTES} 字节上限已截断：请减少返回列、缩小查询范围或分批查询）\n"
        ));
    } else if qr.truncated {
        // 面向模型的截断提示：否则模型会把"仅前 limit 行"误当作完整结果。
        out.push_str(&format!(
            "显示前 {} 行（结果已截断：查询实际返回超过 {} 行；请减少返回列、缩小查询范围或分批查询）\n",
            qr.rows.len(),
            qr.rows.len()
        ));
    } else {
        out.push_str(&format!("共 {} 行", qr.rows.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 截断标记必须反映到格式化文本里（面向模型的提示），
    /// 否则模型会把"仅前 limit 行"误当作完整结果。
    #[test]
    fn format_query_result_marks_truncation() {
        let rows = vec![vec!["1".into(), "a".into()], vec!["2".into(), "b".into()]];
        let qr = QueryResult {
            columns: vec!["id".into(), "name".into()],
            rows,
            affected: 2,
            truncated: true,
        };
        let out = format_query_result(&qr);
        assert!(out.contains("已截断"), "截断提示缺失: {out}");
        assert!(out.contains("分批查询"), "应引导模型缩小查询范围: {out}");
        assert!(!out.contains("共 2 行"), "截断时不应显示误导性的完整行数: {out}");
    }

    /// 未截断时保持原有格式（不出现截断提示）。
    #[test]
    fn format_query_result_plain() {
        let qr = QueryResult {
            columns: vec!["id".into()],
            rows: vec![vec!["1".into()]],
            affected: 1,
            truncated: false,
        };
        let out = format_query_result(&qr);
        assert!(out.contains("共 1 行"));
        assert!(!out.contains("已截断"));
    }
}
