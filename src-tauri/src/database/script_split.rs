//! SQL 脚本按语句切分（字节级状态机）。
//!
//! 服务"运行 SQL 脚本"能力：把整段脚本切成 `Vec<ScriptStatement>`（语句文本
//! + 1-based 起始行号），逐条执行、失败时按行号定位。
//!
//! 借鉴 uniTerm scriptsplit.go（Apache-2.0）并补齐其缺失的三块方言能力：
//! - PostgreSQL dollar-quoting（`$tag$ ... $tag$`，函数体常见）；
//! - SQL Server `GO`（行首整词，大小写不敏感）；
//! - MySQL `DELIMITER`（存储过程体合并为一条语句）。
//!
//! 通用规则：
//! - 单/双/反引号字符串：字面量内的分号不切分；`\` 转义（反引号内不转义，
//!   符合 MySQL 语义）；双写引号 = 转义引号；
//! - 行注释：`--`（MySQL 要求后跟空白）、`#`（仅 MySQL）；块注释
//!   `/* ... */`：不进入语句文本（行号照计）。

/// 一条切分出的语句。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptStatement {
    /// 语句文本（已去注释、首尾空白；不含结尾分隔符）。
    pub sql: String,
    /// 语句首字符所在行号（1-based，以原始脚本计）。
    pub line: u32,
}

/// SQL 方言（决定额外切分规则）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    /// MySQL 系（含 MariaDB）：支持 DELIMITER，反引号字符串内不转义。
    MySql,
    /// PostgreSQL 系：支持 dollar-quoting。
    Postgres,
    /// SQL Server：支持行首 GO 批分隔符。
    SqlServer,
    /// SQLite：通用规则（同 MySQL，无 DELIMITER 需求但无害）。
    Sqlite,
}

impl SqlDialect {
    fn from_kind(kind: &str) -> Self {
        match kind {
            "postgres" => Self::Postgres,
            "sqlserver" => Self::SqlServer,
            "sqlite" => Self::Sqlite,
            _ => Self::MySql,
        }
    }
}

/// 切分一段 SQL 脚本。空语句自动跳过。
pub fn split_sql_script(script: &str, kind: &str) -> Vec<ScriptStatement> {
    split_inner(script, SqlDialect::from_kind(kind))
}

/// 剥离 SQL 开头的前导空白与注释（`--` / `#` 行注释、`/* */` 块注释），
/// 返回剩余部分的切片（零拷贝）。供"首关键字分类"场景使用（如
/// `is_query_stmt`）：带前导注释的 `-- 注释\nSELECT ...` 若不剥注释，
/// 首 token 是 `--`，会被当成非查询走 execute 路径、结果集被静默排空。
/// 无需处理字符串字面量——合法语句不会以引号开头。
pub fn strip_leading_comments(sql: &str) -> &str {
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i < bytes.len() && bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            let mut j = i + 2;
            while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                j += 1;
            }
            i = (j + 2).min(bytes.len());
            continue;
        }
        return &sql[i..];
    }
}

/// 按引号与注释外的顶层 `;` 切分 SQL（方言感知，供只读校验等逐语句复核
/// 场景复用）：字符串字面量（单/双/反引号，双写转义）与注释内的分号
/// 不是语句边界——裸 `split(';')` 会把 `WHERE c = 'a;b'` 的残段 `b'` 或
/// 注释里的分号后文切出来，首关键字判定必失败，导致合法只读查询被误拒。
pub fn top_level_fragments<'a>(sql: &'a str, kind: &str) -> Vec<&'a str> {
    let dialect = SqlDialect::from_kind(kind);
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b';' {
            out.push(&sql[start..i]);
            start = i + 1;
            i += 1;
            continue;
        }
        // 行注释 --：MySQL 要求 `--` 后跟空白/控制字符才是注释（`SELECT 3--1`
        // 是减法），PG/SQLite/SQL Server 的 `--` 恒为注释——与 split_inner 同规则。
        let dash_comment = b == b'-'
            && bytes.get(i + 1) == Some(&b'-')
            && match bytes.get(i + 2) {
                None => true,
                Some(&c) => !matches!(dialect, SqlDialect::MySql) || c.is_ascii_whitespace(),
            };
        // # 行注释仅 MySQL：PG 的 `#` 是 XOR 运算符，若按注释吞到行尾，
        // `SELECT a # b; DROP ...` 的 DROP 会被藏进注释——只读校验被绕过。
        let hash_comment = b == b'#' && matches!(dialect, SqlDialect::MySql);
        if dash_comment || hash_comment {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // 块注释：跳到闭合 `*/`（未闭合则到串尾）。
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        if b == b'\'' || b == b'"' || b == b'`' {
            // 跳过整个字面量（双写引号 = 转义）。
            let quote = b;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == quote {
                    if bytes.get(i + 1) == Some(&quote) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out.push(&sql[start..]);
    out
}

fn split_inner(script: &str, dialect: SqlDialect) -> Vec<ScriptStatement> {
    let bytes = script.as_bytes();
    let mut out = Vec::new();
    // 当前语句缓冲与起始行。
    let mut cur = String::new();
    let mut cur_line: u32 = 0;
    let mut line: u32 = 1;
    // 行首（自上一换行后未消费任何非空白内容）——DELIMITER / GO 的识别条件。
    let mut at_line_start = true;
    let mut has_content = false; // 当前语句是否有非空白内容

    macro_rules! flush {
        () => {
            let sql = cur.trim().to_string();
            if !sql.is_empty() {
                out.push(ScriptStatement {
                    sql,
                    line: cur_line,
                });
            }
            cur.clear();
            has_content = false;
        };
    }

    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];

        // 行注释：-- 或 #（MySQL 风格；PG/SQLite/SQL Server 的 -- 同样支持）。
        // MySQL（及标准 SQL）要求 `--` 后跟空白/控制字符才是注释：
        // `SELECT 3--1` 中 `--1` 是减法而非注释；PG/SQLite/SQL Server 的
        // `--` 恒为注释，无需该条件。
        let dash_comment = b == b'-'
            && bytes.get(i + 1) == Some(&b'-')
            && match bytes.get(i + 2) {
                None => true, // 行尾（输入结束）
                Some(&c) => {
                    !matches!(dialect, SqlDialect::MySql) || c.is_ascii_whitespace()
                }
            };
        // # 行注释仅 MySQL：PG 的 `#` 是 XOR 运算符、SQLite/SQL Server 均无此
        // 注释——按通用注释处理会把 PG 的 `SET c = a # b WHERE id=1` 连同
        // WHERE 子句一起吞掉，语句以无 WHERE 形式被切开执行。
        if dash_comment || (b == b'#' && matches!(dialect, SqlDialect::MySql)) {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue; // 下一轮处理 \n（行号在下方统一推进）
        }

        // 块注释。
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    line += 1;
                    at_line_start = true;
                }
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }

        // 字符串字面量（单/双/反引号）。
        if b == b'\'' || b == b'"' || b == b'`' {
            if !has_content {
                cur_line = line;
                has_content = true;
            }
            let quote = b;
            // 反斜杠转义仅 MySQL 方言支持（PG 默认 standard_conforming_strings=
            // on、SQLite/SQL Server 标准模式下 `\` 无转义语义，`\'` 就是闭引号）；
            // 反引号内任何方言都不转义（MySQL 语义）。
            let backslash_escapes = dialect == SqlDialect::MySql && quote != b'`';
            cur.push(b as char);
            i += 1;
            loop {
                if i >= bytes.len() {
                    break;
                }
                let c = bytes[i];
                if c == b'\\' && backslash_escapes && i + 1 < bytes.len() {
                    // 保留转义序列原文（\x 两字符）。
                    cur.push('\\');
                    cur.push(bytes[i + 1] as char);
                    if bytes[i + 1] == b'\n' {
                        line += 1;
                        at_line_start = true;
                    }
                    i += 2;
                    continue;
                }
                if c == quote {
                    // 双写引号 = 转义。
                    if i + 1 < bytes.len() && bytes[i + 1] == quote {
                        cur.push(quote as char);
                        cur.push(quote as char);
                        i += 2;
                        continue;
                    }
                    cur.push(quote as char);
                    i += 1;
                    break;
                }
                if c == b'\n' {
                    line += 1;
                    at_line_start = true;
                }
                let (ch, w) = decode_char(bytes, i);
                cur.push(ch);
                i += w;
            }
            at_line_start = false;
            continue;
        }

        // PostgreSQL dollar-quoting：$tag$ ... $tag$（tag 为空或标识符字符）。
        if dialect == SqlDialect::Postgres && b == b'$' {
            if let Some((tag, tag_len)) = dollar_quote_tag(bytes, i) {
                if !has_content {
                    cur_line = line;
                    has_content = true;
                }
                // 整段（含首尾标记）原样保留：标记本身是 ASCII，按字节复制即可。
                for k in 0..tag_len {
                    cur.push(bytes[i + k] as char);
                }
                i += tag_len;
                // 寻找结束标记（内容按完整字符复制，行号照计）。
                let mut j = i;
                while j < bytes.len() {
                    if bytes[j] == b'$' && bytes[j..].starts_with(tag.as_bytes()) {
                        let end = j + tag.as_bytes().len();
                        let mut k = i;
                        while k < end {
                            if bytes[k] == b'\n' {
                                line += 1;
                            }
                            let (ch, w) = decode_char(bytes, k);
                            cur.push(ch);
                            k += w;
                        }
                        i = end;
                        break;
                    }
                    j += 1;
                }
                if j >= bytes.len() {
                    // 未闭合：剩余全部并入当前语句。
                    let mut k = i;
                    while k < bytes.len() {
                        if bytes[k] == b'\n' {
                            line += 1;
                        }
                        let (ch, w) = decode_char(bytes, k);
                        cur.push(ch);
                        k += w;
                    }
                    i = bytes.len();
                }
                at_line_start = false;
                continue;
            }
        }

        // MySQL DELIMITER：行首且当前语句无内容时识别。
        if dialect == SqlDialect::MySql
            && at_line_start
            && !has_content
            && starts_with_ignore_case(&bytes[i..], b"delimiter")
        {
            // 取该行剩余部分为新分隔符。
            let line_rest_end = bytes[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map(|p| i + p)
                .unwrap_or(bytes.len());
            let new_delim = String::from_utf8_lossy(&bytes[i + "delimiter".len()..line_rest_end])
                .trim()
                .to_string();
            // DELIMITER 行本身不作为语句输出。
            i = line_rest_end;
            // 后续分隔符切换：进入"自定义分隔符模式"。
            return split_with_delimiter(
                &bytes[i..],
                line,
                new_delim.as_bytes().to_vec(),
                out,
            );
        }

        // SQL Server GO：行首整词（前后为空白/行尾）。
        if dialect == SqlDialect::SqlServer
            && at_line_start
            && starts_with_ignore_case(&bytes[i..], b"go")
        {
            let after = i + 2;
            let next = bytes.get(after).copied();
            if matches!(next, None | Some(b'\n') | Some(b'\r') | Some(b' ') | Some(b'\t')) {
                // GO 行 = 语句边界：当前语句收尾（若非空）。
                flush!();
                at_line_start = true;
                i = after;
                continue;
            }
        }

        match b {
            b'\n' => {
                line += 1;
                at_line_start = true;
                cur.push('\n');
                i += 1;
            }
            b';' => {
                // 语句边界。
                flush!();
                at_line_start = false;
                i += 1;
            }
            b if b.is_ascii_whitespace() => {
                cur.push(b as char);
                i += 1;
            }
            _ => {
                if !has_content {
                    cur_line = line;
                    has_content = true;
                }
                let (ch, w) = decode_char(bytes, i);
                cur.push(ch);
                at_line_start = false;
                i += w;
            }
        }
    }
    flush!();
    out
}

/// 自定义分隔符模式（MySQL DELIMITER 之后）：分号不再切分，仅切新分隔符。
/// `out` 携带 DELIMITER 之前已切出的语句（通常为空）。
fn split_with_delimiter(
    bytes: &[u8],
    mut line: u32,
    delimiter: Vec<u8>,
    mut out: Vec<ScriptStatement>,
) -> Vec<ScriptStatement> {
    if delimiter.is_empty() {
        // 空分隔符（非法输入）：退回分号模式继续。
        return split_inner(&String::from_utf8_lossy(bytes), SqlDialect::MySql);
    }
    let mut cur = String::new();
    let mut cur_line = line;
    let mut has_content = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        // 分隔符前缀匹配即切分（语句文本中出现分隔符 = 边界，本就是其语义）。
        if bytes[i..].starts_with(&delimiter) {
            let sql = cur.trim().to_string();
            if !sql.is_empty() {
                out.push(ScriptStatement { sql, line: cur_line });
            }
            cur.clear();
            has_content = false;
            i += delimiter.len();
            // 跳过该行剩余空白/换行。
            while i < bytes.len() && (bytes[i] == b'\n' || bytes[i] == b'\r' || bytes[i] == b' ') {
                if bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            continue;
        }
        // 恢复 `DELIMITER ;`：**仅当当前语句无内容**时识别（防止
        // `SELECT delimiter FROM t` 这类把列名当命令、语句被错误切分）。
        if (b == b'D' || b == b'd') && !has_content {
            if starts_with_ignore_case(&bytes[i..], b"delimiter") {
                let line_rest_end = bytes[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map(|p| i + p)
                    .unwrap_or(bytes.len());
                let next_delim =
                    String::from_utf8_lossy(&bytes[i + "delimiter".len()..line_rest_end])
                        .trim()
                        .to_string();
                i = line_rest_end;
                if next_delim == ";" {
                    // 退回普通模式处理剩余部分。
                    let rest = split_inner(&String::from_utf8_lossy(&bytes[i..]), SqlDialect::MySql);
                    out.extend(rest);
                    return out;
                }
                return split_with_delimiter(&bytes[i..], line, next_delim.into_bytes(), out);
            }
        }
        // 字符串字面量：整段复制（内部不匹配分隔符）。DELIMITER 模式仅
        // MySQL，反引号外的反斜杠转义有效。
        if b == b'\'' || b == b'"' || b == b'`' {
            if !has_content {
                cur_line = line;
                has_content = true;
            }
            let quote = b;
            let backslash_escapes = quote != b'`';
            cur.push(b as char);
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                if c == b'\\' && backslash_escapes && i + 1 < bytes.len() {
                    cur.push('\\');
                    cur.push(bytes[i + 1] as char);
                    if bytes[i + 1] == b'\n' {
                        line += 1;
                    }
                    i += 2;
                    continue;
                }
                if c == quote {
                    if bytes.get(i + 1) == Some(&quote) {
                        cur.push(quote as char);
                        cur.push(quote as char);
                        i += 2;
                        continue;
                    }
                    cur.push(quote as char);
                    i += 1;
                    break;
                }
                if c == b'\n' {
                    line += 1;
                }
                let (ch, w) = decode_char(bytes, i);
                cur.push(ch);
                i += w;
            }
            continue;
        }
        // 行注释：复制到行尾（注释内的分隔符文本不是边界）。`--` 后须跟
        // 空白（MySQL 语义，与主状态机一致）。
        let dash_comment = b == b'-'
            && bytes.get(i + 1) == Some(&b'-')
            && match bytes.get(i + 2) {
                None => true,
                Some(&c) => c.is_ascii_whitespace(),
            };
        if dash_comment || b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                let (ch, w) = decode_char(bytes, i);
                cur.push(ch);
                i += w;
            }
            continue; // \n 由下方统一处理
        }
        // 块注释：复制到 */（注释内的分隔符文本不是边界）。
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                if bytes[i] == b'\n' {
                    line += 1;
                }
                let (ch, w) = decode_char(bytes, i);
                cur.push(ch);
                i += w;
            }
            if i + 1 < bytes.len() {
                cur.push('*');
                cur.push('/');
                i += 2;
            } else if i < bytes.len() {
                // 未闭合：复制残留的 `*`，外层循环收尾。
                let (ch, w) = decode_char(bytes, i);
                cur.push(ch);
                i += w;
            }
            continue;
        }
        if b == b'\n' {
            line += 1;
            if !has_content {
                cur_line = line;
            }
            cur.push('\n');
            i += 1;
        } else {
            if !has_content && !b.is_ascii_whitespace() {
                cur_line = line;
                has_content = true;
            }
            let (ch, w) = decode_char(bytes, i);
            cur.push(ch);
            i += w;
        }
    }
    let sql = cur.trim().to_string();
    if !sql.is_empty() {
        out.push(ScriptStatement { sql, line: cur_line });
    }
    out
}

/// 从 `pos` 解析 dollar-quote 标记 `$tag$`：返回 (完整标记 "$tag$", 标记字节长)。
fn dollar_quote_tag(bytes: &[u8], pos: usize) -> Option<(String, usize)> {
    debug_assert_eq!(bytes[pos], b'$');
    let mut j = pos + 1;
    while j < bytes.len()
        && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
    {
        j += 1;
    }
    if j < bytes.len() && bytes[j] == b'$' {
        let tag = std::str::from_utf8(&bytes[pos..=j]).ok()?;
        Some((tag.to_string(), j - pos + 1))
    } else {
        None
    }
}

/// 前缀匹配（大小写不敏感）。
fn starts_with_ignore_case(haystack: &[u8], prefix: &[u8]) -> bool {
    haystack.len() >= prefix.len()
        && haystack[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// 从 `idx` 解码一个完整 UTF-8 字符，返回 `(字符, 字节宽度)`。
///
/// 调用方必须按返回的**宽度**推进游标——逐字节推进会把多字节字符（中文）
/// 的续字节喂给本函数，产生 U+FFFD 乱码。
/// `idx` 落在字符中间或非法序列时返回 `(U+FFFD, 1)`（与输入损坏程度一致）。
fn decode_char(bytes: &[u8], idx: usize) -> (char, usize) {
    let max = (idx + 4).min(bytes.len());
    for end in (idx + 1)..=max {
        if let Ok(s) = std::str::from_utf8(&bytes[idx..end]) {
            if let Some(c) = s.chars().next() {
                return (c, end - idx);
            }
        }
    }
    (char::REPLACEMENT_CHARACTER, 1)
}

// ===========================================================================
// 单元测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sqls(script: &str, kind: &str) -> Vec<String> {
        split_sql_script(script, kind)
            .into_iter()
            .map(|s| s.sql)
            .collect()
    }

    /// 基本多语句切分 + 行号。
    #[test]
    fn basic_split() {
        let stmts = split_sql_script("SELECT 1;\nSELECT 2;", "mysql");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0].sql, "SELECT 1");
        assert_eq!(stmts[0].line, 1);
        assert_eq!(stmts[1].sql, "SELECT 2");
        assert_eq!(stmts[1].line, 2);
    }

    /// 字符串内的分号不切分；注释内的分号也不切分。
    #[test]
    fn semicolons_in_strings_and_comments() {
        let out = sqls(
            "SELECT 'a;b' AS x; -- comment; here\nSELECT 2; /* block ; */ SELECT 3;",
            "mysql",
        );
        assert_eq!(out, vec!["SELECT 'a;b' AS x", "SELECT 2", "SELECT 3"]);
    }

    /// 双写引号转义 + 反斜杠转义。
    #[test]
    fn quote_escaping() {
        let out = sqls(r#"INSERT INTO t VALUES ('it''s; ok', "d;q"); SELECT 2;"#, "mysql");
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("it''s; ok"));
        assert_eq!(out[1], "SELECT 2");
    }

    /// MySQL DELIMITER：存储过程体合并为一条。
    #[test]
    fn mysql_delimiter_procedure() {
        let script = "DROP PROCEDURE IF EXISTS p;\nDELIMITER $$\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 1; SELECT 2;\nEND$$\nDELIMITER ;\nCALL p();";
        let out = sqls(script, "mysql");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], "DROP PROCEDURE IF EXISTS p");
        assert!(out[1].starts_with("CREATE PROCEDURE"));
        assert!(out[1].contains("SELECT 1; SELECT 2;"));
        assert!(out[1].ends_with("END"));
        assert_eq!(out[2], "CALL p()");
    }

    /// PostgreSQL dollar-quoting：函数体内的分号不切分。
    #[test]
    fn postgres_dollar_quoting() {
        let script =
            "CREATE FUNCTION f() RETURNS int AS $$\nBEGIN\n  RETURN 1; -- inner;\nEND;\n$$ LANGUAGE plpgsql;\nSELECT f();";
        let out = sqls(script, "postgres");
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("LANGUAGE plpgsql"));
        assert!(out[0].contains("RETURN 1; -- inner;"));
        assert_eq!(out[1], "SELECT f()");
    }

    /// SQL Server GO 批分隔符（行首整词、大小写不敏感）。
    #[test]
    fn sqlserver_go() {
        let script = "SELECT 1\ngo\nSELECT 'GOLDEN' AS g; -- go not at line start? \nSELECT 3\nGO\nSELECT 4";
        let out = sqls(script, "sqlserver");
        // SELECT 1 | SELECT 'GOLDEN'...; SELECT 3 | SELECT 4（GO 切批，; 仍切语句）
        assert_eq!(
            out,
            vec![
                "SELECT 1",
                "SELECT 'GOLDEN' AS g",
                "SELECT 3",
                "SELECT 4",
            ]
        );
    }

    /// 非 PG 方言不识别 dollar-quoting（$$ 内分号照切）。
    #[test]
    fn dollar_quote_only_for_pg() {
        let out = sqls("SELECT $$a;b$$", "mysql");
        // MySQL 视角：$$a 与 b$$ 被分号切开。
        assert_eq!(out, vec!["SELECT $$a", "b$$"]);
    }

    /// 行号跨注释/字符串推进。
    #[test]
    fn line_numbers_across_comments() {
        let stmts = split_sql_script("-- c1\n# c2\nSELECT\n1;\nSELECT 2;", "mysql");
        assert_eq!(stmts[0].line, 3);
        assert_eq!(stmts[1].line, 5);
    }

    /// 空脚本 / 纯注释。
    #[test]
    fn empty_and_comments_only() {
        assert!(split_sql_script("", "mysql").is_empty());
        assert!(split_sql_script("-- nothing\n/* here */", "mysql").is_empty());
    }

    /// 回归：多字节字符（中文）必须完整保留——旧实现按字节推进 + lossy
    /// 取首字符，中文字符串/列名会变成一串 U+FFFD 乱码。
    #[test]
    fn multibyte_characters_preserved() {
        let out = sqls("SELECT '中文数据' AS 备注; -- 中文注释\nSELECT 中文列 FROM 中文表;", "mysql");
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("中文数据"), "字符串内中文被损坏: {:?}", out[0]);
        assert!(out[0].contains("备注"));
        assert!(out[1].contains("中文列"));
        assert!(out[1].contains("中文表"));
        // 无 U+FFFD。
        assert!(!out.iter().any(|s| s.contains('\u{FFFD}')));
    }

    /// 回归：PG dollar-quoting 函数体内的中文同样完整保留。
    #[test]
    fn multibyte_in_dollar_quote() {
        let script = "CREATE FUNCTION f() RETURNS text AS $$\nBEGIN\n  RETURN '中文';\nEND;\n$$ LANGUAGE plpgsql;\nSELECT 2;";
        let out = sqls(script, "postgres");
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("中文"));
        assert!(!out[0].contains('\u{FFFD}'));
    }

    /// 回归：DELIMITER 模式下语句中的 delimiter **列名**不得触发分隔符切换
    /// （旧实现无"语句为空"守卫，`SELECT delimiter FROM t` 被错误切分）。
    #[test]
    fn delimiter_as_column_name_not_confused() {
        let script = "DELIMITER $$\nCREATE TRIGGER trg BEFORE INSERT ON logs FOR EACH ROW\nBEGIN\n  SET NEW.delimiter = ',';\nEND$$\nDELIMITER ;\nSELECT delimiter FROM t;";
        let out = sqls(script, "mysql");
        // 触发器体一条 + SELECT 一条；触发器体内的 delimiter 列名不切分。
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("SET NEW.delimiter = ',';"));
        assert_eq!(out[1], "SELECT delimiter FROM t");
    }

    /// 回归：DELIMITER 模式下字符串/注释内的分隔符文本不是语句边界
    /// （旧实现裸字节前缀匹配，`SET msg = 'Total: $$'` 被从字符串中间截断）。
    #[test]
    fn delimiter_inside_string_and_comment_not_split() {
        let script = "DELIMITER $$\nCREATE PROCEDURE p()\nBEGIN\n  SET msg = 'Total: $$';\n  SELECT 1; -- $$ here\nEND$$\nDELIMITER ;\nSELECT 2;";
        let out = sqls(script, "mysql");
        assert_eq!(out.len(), 2, "字符串/注释内的 $$ 不应切分: {out:?}");
        assert!(out[0].contains("'Total: $$'"));
        assert!(out[0].ends_with("END"));
        assert_eq!(out[1], "SELECT 2");
    }

    /// 回归：MySQL 方言 `--` 后无空白是减法而非注释（PG/SQLite 恒为注释）。
    #[test]
    fn mysql_dash_dash_without_whitespace_is_minus() {
        assert_eq!(sqls("SELECT 3--1;", "mysql"), vec!["SELECT 3--1"]);
        assert_eq!(sqls("SELECT 3--1;", "postgres"), vec!["SELECT 3"]);
        // 行尾（输入结束）的 -- 仍是注释。
        assert_eq!(sqls("SELECT 3--", "mysql"), vec!["SELECT 3"]);
    }

    /// 回归：反斜杠转义仅 MySQL 方言——PG/SQLite 标准模式下 `\'` 就是闭引号。
    #[test]
    fn backslash_escape_is_mysql_only() {
        // PG：'C:\temp\' 是完整字符串，分号正常切分。
        let out = sqls("SELECT 'C:\\temp\\'; SELECT 2;", "postgres");
        assert_eq!(out, vec!["SELECT 'C:\\temp\\'", "SELECT 2"]);
        // MySQL：\' 被视为转义，字符串吞掉后面的分号（与 mysql 客户端一致）。
        let out = sqls("SELECT 'a\\'; SELECT 2;", "mysql");
        assert_eq!(out.len(), 1);
    }

    /// top_level_fragments：字符串内的分号不切分（供只读校验复用）。
    #[test]
    fn top_level_fragments_skip_quoted_semicolons() {
        let frags = top_level_fragments("SELECT * FROM t WHERE c = 'a;b'; SELECT 2", "mysql");
        assert_eq!(frags, vec!["SELECT * FROM t WHERE c = 'a;b'", " SELECT 2"]);
        // 双写引号转义。
        let frags = top_level_fragments("SELECT 'it''s;ok'; SELECT 2", "mysql");
        assert_eq!(frags.len(), 2);
    }

    /// 回归（只读误拒）：注释里的分号不是语句边界——`-- note; later` 若被
    /// 切开，残段 `later` 的首关键字判定必失败，合法只读查询被整条拒绝。
    #[test]
    fn top_level_fragments_skip_comment_semicolons() {
        let frags = top_level_fragments("SELECT 1 -- note; later\n", "mysql");
        assert_eq!(frags, vec!["SELECT 1 -- note; later\n"]);
        let frags = top_level_fragments("/* a; b */ SELECT 1; SELECT 2", "postgres");
        assert_eq!(frags.len(), 2);
        // MySQL `#` 行注释内的分号同样不切分。
        let frags = top_level_fragments("SELECT 1 # note; later\n; SELECT 2", "mysql");
        assert_eq!(frags.len(), 2);
        assert!(frags[0].contains("# note; later"));
    }

    /// 回归（只读绕过防护）：PG 的 `#` 是 XOR 运算符而非注释——若按注释吞到
    /// 行尾，`SELECT a # b; DROP ...` 的 DROP 会被藏进注释、只读校验被绕过。
    #[test]
    fn top_level_fragments_hash_is_not_comment_outside_mysql() {
        let frags = top_level_fragments("SELECT 1 # 2; DROP TABLE t", "postgres");
        assert_eq!(frags.len(), 2);
        assert_eq!(frags[1].trim(), "DROP TABLE t");
    }

    /// strip_leading_comments：前导空白/行注释/块注释逐层剥离，取首个有效
    /// 关键字场景（is_query_stmt 分类用）。
    #[test]
    fn strip_leading_comments_variants() {
        assert_eq!(strip_leading_comments("  \n SELECT 1"), "SELECT 1");
        assert_eq!(strip_leading_comments("-- note\nSELECT 1"), "SELECT 1");
        assert_eq!(strip_leading_comments("# note\nSELECT 1"), "SELECT 1");
        assert_eq!(strip_leading_comments("/* a */ /* b */ SELECT 1"), "SELECT 1");
        // 非前导注释不剥离。
        assert_eq!(strip_leading_comments("SELECT 1 -- tail"), "SELECT 1 -- tail");
        // 全注释 → 空串。
        assert_eq!(strip_leading_comments("-- only comment\n"), "");
    }
}
