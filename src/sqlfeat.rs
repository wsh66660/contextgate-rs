//! SQL 特征提取与归一化。
//!
//! 查询侧（拿到一条实际执行的 SQL）与索引侧（扫描出的 SQL 模板）
//! 走同一套归一化，保证两边可比：
//! - 字符串字面量 / 数字字面量 / `?` / `#{}` / `${}` 统一折叠为 `?`
//! - 空白、括号、逗号归一，注释剔除
//! - 提取表（FROM/JOIN/INTO/UPDATE 之后）与列（SELECT 列表、WHERE/SET 标识符）

use std::collections::BTreeSet;

const SQL_KEYWORDS: &[&str] = &[
    "select", "from", "where", "and", "or", "not", "in", "like", "insert", "into", "values",
    "update", "set", "delete", "join", "left", "right", "inner", "outer", "on", "group", "by",
    "order", "having", "limit", "offset", "as", "distinct", "exists", "between", "is", "null",
    "asc", "desc", "case", "when", "then", "else", "end", "union", "all", "count", "sum", "avg",
    "min", "max",
];

pub fn is_keyword(tok: &str) -> bool {
    SQL_KEYWORDS.contains(&tok)
}

/// 剔除 SQL 注释（-- 与 /* */，保留字符串内部）。
fn strip_comments(sql: &str) -> String {
    let b: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    let mut in_str: Option<char> = None;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = in_str {
            out.push(c);
            if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            in_str = Some(c);
            out.push(c);
            i += 1;
            continue;
        }
        if c == '-' && i + 1 < b.len() && b[i + 1] == '-' {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < b.len() && b[i + 1] == '*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 把一条 SQL（模板或实际执行）归一化：
/// 返回 (token 骨架, 表集合, 列集合)。
pub fn normalize_sql(sql: &str) -> (Vec<String>, BTreeSet<String>, BTreeSet<String>) {
    let text = strip_comments(sql);
    // MyBatis 占位符先折为 ?
    let text = text.replace("#{", "?").replace("${", "?");
    let b: Vec<char> = text.chars().collect();
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 0;

    // 候选标识符出现的位置（供列提取）
    let mut raw_tokens: Vec<String> = Vec::new();

    let mut flush = |cur: &mut String, tokens: &mut Vec<String>, raw: &mut Vec<String>| {
        if !cur.is_empty() {
            let t = cur.clone();
            tokens.push(t.clone());
            raw.push(t);
            cur.clear();
        }
    };

    while i < b.len() {
        let c = b[i];
        if c == '\'' || c == '"' {
            // 字符串字面量 -> ?
            flush(&mut cur, &mut tokens, &mut raw_tokens);
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            tokens.push("?".into());
            raw_tokens.push("?".into());
            continue;
        }
        if c == '#' && i + 1 < b.len() && b[i + 1] == '{' {
            // #{param} 已替换为 ?{...}，吞到 }
            i += 2;
            while i < b.len() && b[i] != '}' {
                i += 1;
            }
            i += 1;
            tokens.push("?".into());
            raw_tokens.push("?".into());
            continue;
        }
        if c == '?' {
            flush(&mut cur, &mut tokens, &mut raw_tokens);
            tokens.push("?".into());
            raw_tokens.push("?".into());
            i += 1;
            continue;
        }
        if c.is_alphanumeric() || c == '_' || c == '.' || c == '`' || c == '*' {
            if c != '`' {
                cur.extend(c.to_lowercase());
            }
            i += 1;
            continue;
        }
        flush(&mut cur, &mut tokens, &mut raw_tokens);
        // 比较符作为独立 token（供列提取判断 `col =` / `col >` 形态）
        if matches!(c, '=' | '>' | '<' | '!') {
            tokens.push(c.to_string());
            raw_tokens.push(c.to_string());
        }
        i += 1;
    }
    flush(&mut cur, &mut tokens, &mut raw_tokens);

    // 纯数字 token 折叠为 ?（字面量值不可作特征）
    for t in tokens.iter_mut().chain(raw_tokens.iter_mut()) {
        if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) {
            *t = "?".into();
        }
    }

    let tables = extract_tables(&raw_tokens);
    let columns = extract_columns(&raw_tokens);
    (tokens, tables, columns)
}

/// 从 token 流提取表名：跟在 from / join / into / update / 后面的非关键字标识符。
pub fn extract_tables(toks: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if matches!(t.as_str(), "from" | "join" | "into" | "update") && i + 1 < toks.len() {
            let mut j = i + 1;
            // 跳过括号（子查询）
            if toks[j] == "(" {
                i += 1;
                continue;
            }
            let cand = toks[j].split('.').next_back().unwrap_or("").to_string();
            if !cand.is_empty() && !is_keyword(&cand) && cand != "?" {
                out.insert(cand);
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
    out
}

/// 从 token 流提取列名：
/// - select 列表里的非聚合/关键字标识符
/// - where/set 中 `col ?`、`col =`、`col like`、`col in` 左侧标识符
pub fn extract_columns(toks: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    // SELECT 列表：从 select 到「它之后的」from（DELETE ... IN (SELECT) 等语句里
    // from 可能整体出现在 select 之前，旧逻辑直接切 [s+1..e] 会越界 panic）
    if let Some(s) = toks.iter().position(|t| t == "select") {
        if let Some(e) = toks.iter().position(|t| t == "from").filter(|&p| p > s) {
            for t in &toks[s + 1..e] {
                let bare = t.split('.').next_back().unwrap_or("");
                if bare == "*" || bare == "?" || bare.is_empty() {
                    continue;
                }
                if is_keyword(bare) {
                    continue; // 关键字/聚合函数名不算列
                }
                out.insert(bare.to_string());
            }
        }
    }
    // WHERE/ON/SET 侧：标识符后跟比较符或 in/like/between
    let mut i = 1;
    while i < toks.len() {
        let prev = &toks[i - 1];
        let t = &toks[i];
        let after_where_like = matches!(
            prev.as_str(),
            "where" | "and" | "or" | "on" | "set" | "having" | "by" | "(" | ","
        );
        let next_is_cmp = i + 1 < toks.len()
            && matches!(
                toks[i + 1].as_str(),
                "?" | "=" | "like" | "in" | "between" | "is" | ">" | "<" | "!=" | ">=" | "<="
            );
        if after_where_like && next_is_cmp {
            let bare = t.split('.').next_back().unwrap_or("");
            if !bare.is_empty() && !is_keyword(bare) && bare != "?" && bare != "(" {
                out.insert(bare.to_string());
            }
        }
        i += 1;
    }
    out
}

/// 两个 token 序列的 Jaccard 相似度（token 多重集按集合算，够用）。
pub fn jaccard(a: &[String], b: &[String]) -> f64 {
    let sa: BTreeSet<&String> = a.iter().collect();
    let sb: BTreeSet<&String> = b.iter().collect();
    if sa.is_empty() && sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    inter as f64 / union.max(1) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_literals_and_placeholders() {
        let (sk, tables, cols) = normalize_sql(
            "SELECT id, name FROM sys_user WHERE name = 'tom' AND age > 18 LIMIT 10",
        );
        assert!(tables.contains("sys_user"));
        assert!(cols.contains("id") && cols.contains("name") && cols.contains("age"));
        assert!(sk.contains(&"?".to_string()));
        assert!(!sk.iter().any(|t| t == "tom" || t == "18"));
    }

    #[test]
    fn mybatis_placeholder_normalized() {
        let (sk, tables, cols) = normalize_sql(
            "SELECT o.* FROM biz_order o WHERE o.user_id = #{userId} ORDER BY o.id DESC",
        );
        assert!(tables.contains("biz_order"));
        assert!(cols.contains("user_id"));
        assert!(sk.contains(&"?".to_string()));
    }

    #[test]
    fn delete_in_subselect_does_not_panic() {
        // 回归：from 在 select 之前出现时 [s+1..e] 切片越界 panic
        // （xxl-job 清理型 SQL：DELETE FROM xxl_job_log WHERE id IN (SELECT ...)）
        let (sk, tables, _) = normalize_sql(
            "DELETE FROM xxl_job_log WHERE id IN (SELECT id FROM xxl_job_log WHERE gmt_create < ?)",
        );
        assert!(tables.contains("xxl_job_log"));
        assert!(!sk.is_empty());
    }

    #[test]
    fn table_after_join_and_update() {
        let (_, t1, _) = normalize_sql("SELECT * FROM a JOIN b ON a.id=b.aid");
        assert!(t1.contains("a") && t1.contains("b"));
        let (_, t2, _) = normalize_sql("UPDATE sys_user SET name = ? WHERE id = ?");
        assert!(t2.contains("sys_user"));
        assert!(normalize_sql("INSERT INTO t(x) VALUES (1)").1.contains("t"));
    }
}
