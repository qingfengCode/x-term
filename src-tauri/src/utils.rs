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
    if qr.truncated {
        // 面向模型的截断提示：否则模型会把"仅前 limit 行"误当作完整结果。
        out.push_str(&format!(
            "显示前 {} 行（结果已截断：查询实际返回超过 {} 行；可调大 limit 参数重新查询）\n",
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
        assert!(out.contains("limit"), "应引导调大 limit: {out}");
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
