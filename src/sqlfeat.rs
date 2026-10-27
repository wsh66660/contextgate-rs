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

/// 单字符运算符/占位符 token（非标识符）；单字母标识符（如别名 o、列 t）是合法的
fn is_op_token(t: &str) -> bool {
    matches!(t, "=" | ">" | "<" | "!" | "?" | "*")
}

/// 结构相似度里的标识符判定：非关键字、非运算符 token、首字符字母数字/下划线
fn is_struct_ident(t: &str) -> bool {
    !is_keyword(t)
        && !is_op_token(t)
        && t.chars()
            .next()
            .map_or(false, |c| c.is_ascii_alphanumeric() || c == '_')
}

/// 骨架的结构相似度：只比较真实标识符（表名/列名），剔除样板 SQL 关键字
/// 与 `?`/`*`/比较符等无判别力 token。
///
/// 若混入 select/from/where 等关键字，两条不同表的 SELECT 会因共享样板
/// 而虚高（orders 查询撞上 users 记录）；只比标识符后，相似度才反映
/// 「是否触碰同样的表/列」这一真实信号。
pub fn jaccard_structure(a: &[String], b: &[String]) -> f64 {
    let sa: BTreeSet<&String> = a.iter().filter(|t| is_struct_ident(t.as_str())).collect();
    let sb: BTreeSet<&String> = b.iter().filter(|t| is_struct_ident(t.as_str())).collect();
    if sa.is_empty() && sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    inter as f64 / union.max(1) as f64
}

/// 提取 SQL 操作类型（首 token 为 select/insert/update/delete 时）。
/// 同一发射点几乎只发同一类语句——是反查里最强的准必要条件信号。
pub fn sql_verb(tokens: &[String]) -> Option<&'static str> {
    match tokens.first()?.as_str() {
        "select" => Some("select"),
        "insert" => Some("insert"),
        "update" => Some("update"),
        "delete" => Some("delete"),
        _ => None,
    }
}

/// 把 SQL 文本折成骨架文本：字符串字面量/数字/`#{}`/`${}` → `?`，
/// 去反引号，小写，空白压缩。查询侧（实际执行的 SQL）与索引侧
/// （源码模板）走同一折叠，固定文本片段才能逐字比对。
pub fn skeleton_text(sql: &str) -> String {
    let b: Vec<char> = sql.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // 字符串字面量 -> ?
        if c == '\'' || c == '"' {
            let q = c;
            i += 1;
            while i < b.len() && b[i] != q {
                if b[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            out.push('?');
            continue;
        }
        // 反引号直接丢
        if c == '`' {
            i += 1;
            continue;
        }
        // #{} / ${} 占位符 -> ?
        if (c == '#' || c == '$') && i + 1 < b.len() && b[i + 1] == '{' {
            i += 2;
            while i < b.len() && b[i] != '}' {
                i += 1;
            }
            i += 1;
            out.push('?');
            continue;
        }
        // 限定符 alias.col -> col：两侧统一剥前缀，片段才能逐字比对
        // （慢日志里通常是 t.id，手动简化后是 id）
        if c == '.' {
            let prev_ident = out
                .chars()
                .next_back()
                .map_or(false, |p| p.is_ascii_alphanumeric() || p == '_');
            let nc = if i + 1 < b.len() { b[i + 1] } else { '\0' };
            let next_ident =
                nc.is_ascii_alphabetic() || nc == '_'
                    || (nc == '`'
                        && i + 2 < b.len()
                        && (b[i + 2].is_ascii_alphabetic() || b[i + 2] == '_'));
            if prev_ident && next_ident {
                let mut n = 0;
                for p in out.chars().rev() {
                    if p.is_ascii_alphanumeric() || p == '_' {
                        n += 1;
                    } else {
                        break;
                    }
                }
                out.truncate(out.len().saturating_sub(n));
            }
            i += 1;
            continue;
        }
        // 独立数字 -> ?（标识符内部的数字不折：col1 保持原样）
        if c.is_ascii_digit()
            && out
                .chars()
                .next_back()
                .map_or(true, |p| !(p.is_alphanumeric() || p == '_' || p == '.'))
        {
            out.push('?');
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == '.') {
                i += 1;
            }
            continue;
        }
        if c.is_whitespace() {
            if out.chars().next_back() != Some(' ') {
                out.push(' ');
            }
        } else {
            out.extend(c.to_lowercase());
        }
        i += 1;
    }
    out.trim().to_string()
}

/// 高辨识度固定文本片段：骨架文本按 `?` 切分后 ≥8 字符的片段。
/// 执行 SQL 的这些片段必须逐字出现在源码模板中——比 token 相似度可靠。
pub fn fixed_fragments(sql: &str) -> Vec<String> {
    skeleton_text(sql)
        .split('?')
        .map(|s| s.trim().to_string())
        .filter(|s| s.len() >= 8)
        .collect()
}

/// 片段召回率：查询的固定文本片段有多少比例出现在记录模板骨架中。
/// 查询太短（无 ≥8 字符片段）时返回 None，调用方回退旧权重。
pub fn fragment_recall(query_sql: &str, template_sql: &str) -> Option<f64> {
    let frags = fixed_fragments(query_sql);
    if frags.is_empty() {
        return None;
    }
    let tmpl = skeleton_text(template_sql);
    let hit = frags.iter().filter(|f| tmpl.contains(f.as_str())).count();
    Some(hit as f64 / frags.len() as f64)
}

/// 提取「表名 -> 别名」对（FROM t [AS] a / JOIN t a）。
/// 表+别名组合是高辨识指纹：同表多处查询中，别名能区分具体哪一条。
pub fn extract_table_aliases(toks: &[String]) -> BTreeSet<(String, String)> {
    let is_ident = |t: &String| is_struct_ident(t.as_str());
    let mut out = BTreeSet::new();
    let mut i = 0usize;
    while i + 2 < toks.len() {
        if (toks[i] == "from" || toks[i] == "join") && is_ident(&toks[i + 1]) {
            let t = toks[i + 1].clone();
            let a = if toks[i + 2] == "as" && i + 3 < toks.len() && is_ident(&toks[i + 3]) {
                Some(toks[i + 3].clone())
            } else if is_ident(&toks[i + 2]) {
                Some(toks[i + 2].clone())
            } else {
                None
            };
            if let Some(a) = a {
                out.insert((t, a));
            }
            i += 3;
            continue;
        }
        i += 1;
    }
    out
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
    fn skeleton_text_folds_literals_and_numbers() {
        let s = skeleton_text("SELECT id FROM users WHERE name = 'Tom' AND age > 18 LIMIT 10");
        assert_eq!(
            s,
            "select id from users where name = ? and age > ? limit ?"
        );
        // 标识符内部数字不折
        let s2 = skeleton_text("SELECT col1, v2 FROM t");
        assert_eq!(s2, "select col1, v2 from t");
        // 反引号去除
        let s3 = skeleton_text("SELECT `order` FROM t");
        assert_eq!(s3, "select order from t");
        // mybatis 占位符
        let s4 = skeleton_text("SELECT id FROM t WHERE uid = #{userId}");
        assert_eq!(s4, "select id from t where uid = ?");
        // 限定符剥前缀：t.id -> id（两侧统一）
        let s5 = skeleton_text("SELECT t.id, o.name FROM orders o JOIN user u ON u.id = t.uid");
        assert_eq!(s5, "select id, name from orders o join user u on id = uid");
        // 版本号形态不误剥：点后是数字不视为限定符
        let s6 = skeleton_text("SELECT v FROM t WHERE v = 'v1.2'");
        assert_eq!(s6, "select v from t where v = ?");
    }

    #[test]
    fn fragment_recall_hits_true_template() {
        let tmpl = "SELECT id, app_name FROM xxl_job_group WHERE id = #{id}";
        let q = "SELECT id, app_name FROM xxl_job_group WHERE id = 5";
        assert_eq!(fragment_recall(q, tmpl), Some(1.0));
        // 不同表：固定文本不出现
        let other = "SELECT id, app_name FROM xxl_job_log WHERE id = #{id}";
        assert_eq!(fragment_recall(q, other), Some(0.0));
        // 太短/无固定文本的查询无片段 -> None（回退旧权重）
        assert_eq!(fragment_recall("SELECT 1", tmpl), None);
    }

    #[test]
    fn dynamic_template_contains_executed_fragments() {
        // 动态模板是执行 SQL 的超集：if 分支内的固定文本也应在模板里。
        // （生产路径中模板先经 flatten_dynamic_tags 拍平，这里用拍平后形态）
        let tmpl = "SELECT id FROM t   WHERE    AND b = #{b}  ";
        let q = "SELECT id FROM t WHERE AND b = 9";
        assert_eq!(fragment_recall(q, tmpl), Some(1.0));
    }

    #[test]
    fn extract_table_aliases_basic() {
        let (sk, _, _) = normalize_sql(
            "SELECT o.id FROM orders o JOIN payment p ON o.id = p.oid LEFT JOIN user u ON u.id = o.uid",
        );
        let pairs = extract_table_aliases(&sk);
        assert!(pairs.contains(&("orders".to_string(), "o".to_string())));
        assert!(pairs.contains(&("payment".to_string(), "p".to_string())));
        assert!(pairs.contains(&("user".to_string(), "u".to_string())));
        // 无别名不产对
        let (sk2, _, _) = normalize_sql("SELECT * FROM orders");
        assert!(extract_table_aliases(&sk2).is_empty());
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
