//! 匹配引擎：把一条实际执行的 SQL 与索引记录打分匹配，输出源码位置。
//!
//! 打分信号（按判别力排序）：
//! 1. 操作类型（verb）：SELECT/INSERT/UPDATE/DELETE，同一发射点几乎只发
//!    同一类语句，是准必要条件——不同则强降权（×0.15），避免 SELECT 查询
//!    命中 INSERT 代码这类跨类型误报
//! 2. 表 Jaccard：慢 SQL 的表名是确定性指纹
//! 3. 列包含度：查询列在记录列中的覆盖率
//! 4. 结构相似度：骨架剔除 `?` 占位符后的 Jaccard（占位符成串出现，
//!    混入会稀释信号）

use crate::model::Index;
use crate::model::SqlRecord;
use crate::sqlfeat::{
    extract_table_aliases, fragment_recall, jaccard_structure, normalize_sql, sql_verb,
};
use std::collections::BTreeSet;

/// 一条匹配结果。
#[derive(Debug, Clone)]
pub struct MatchHit {
    pub record: SqlRecord,
    pub score: f64,
    pub reasons: Vec<String>,
}

/// 匹配一条实际执行的 SQL。
pub fn find_sql(index: &Index, query_sql: &str, top_n: usize) -> Vec<MatchHit> {
    let (q_sk, q_tables, q_cols) = normalize_sql(query_sql);
    let q_verb = sql_verb(&q_sk);
    let q_pairs = extract_table_aliases(&q_sk);
    let mut hits: Vec<MatchHit> = Vec::new();

    for rec in &index.records {
        let (sk_sim, tbl_j, col_j) = similarity(&q_sk, &q_tables, &q_cols, rec);

        // 固定文本片段召回（最强信号）：查询按 ? 切出的 ≥8 字符片段必须
        // 逐字出现在源码模板中（模板是执行 SQL 的超集）。
        // mp_builtin 记录是合成占位 SQL，不参与片段匹配。
        let frag = if rec.mp_builtin {
            None
        } else {
            fragment_recall(query_sql, &rec.sql)
        };
        if tbl_j == 0.0 && sk_sim < 0.5 && frag.map_or(true, |f| f < 0.5) {
            continue; // 表/结构/固定文本三者都不像，剪枝
        }

        // 表 + 别名组合：双方都有别名时，别名一致的 (表,别名) 对是更强指纹
        let r_pairs = extract_table_aliases(&rec.skeleton);
        let tbl_signal = if !q_pairs.is_empty() && !r_pairs.is_empty() {
            let pair_j = set_jaccard(&q_pairs, &r_pairs);
            0.6 * tbl_j + 0.4 * pair_j
        } else {
            tbl_j
        };

        // 权重：有片段信号时以片段为主；无（查询太短/合成记录）回退旧权重
        let mut score = match frag {
            Some(f) => 0.45 * f + 0.30 * tbl_signal + 0.15 * col_j + 0.10 * sk_sim,
            None => 0.50 * tbl_signal + 0.25 * col_j + 0.25 * sk_sim,
        };

        // 操作类型门控
        let r_verb = sql_verb(&rec.skeleton);
        let (verb_factor, verb_reason): (f64, String) = match (q_verb, r_verb) {
            (Some(q), Some(r)) if q == r => (1.0, format!("操作类型一致（{}）", q.to_uppercase())),
            (Some(q), Some(r)) => (
                0.15,
                format!(
                    "⚠️ 操作类型不符：查询 {} / 记录 {}（强降权）",
                    q.to_uppercase(),
                    r.to_uppercase()
                ),
            ),
            (Some(_), None) | (None, Some(_)) => (0.7, "操作类型单边未知（降权）".to_string()),
            (None, None) => (1.0, String::new()),
        };
        score *= verb_factor;

        // 表名是慢 SQL 最强指纹：完全不重合则软降权（保留可见性，不误杀
        // 表提取失败的正确记录，但确保有表重合的候选排在前面）
        if tbl_signal == 0.0 {
            score *= 0.5;
        }

        if rec.mp_builtin {
            score *= 0.6; // 保守占位记录降权
        }
        if rec.dynamic {
            score *= 1.05; // 动态模板本就只含形状，略补偿
        }
        score = score.min(1.0);
        if score <= 0.03 {
            continue;
        }
        let mut reasons = Vec::new();
        if !verb_reason.is_empty() {
            reasons.push(verb_reason);
        }
        if let Some(f) = frag {
            let frags = crate::sqlfeat::fixed_fragments(query_sql);
            let hit = (f * frags.len() as f64).round() as usize;
            reasons.push(format!("固定文本片段 {}/{} 命中（{:.0}%）", hit, frags.len(), f * 100.0));
        }
        if !q_pairs.is_empty() && !r_pairs.is_empty() {
            let pair_j = set_jaccard(&q_pairs, &r_pairs);
            reasons.push(format!("表+别名匹配 {:.0}%", pair_j * 100.0));
        } else if tbl_j > 0.0 {
            reasons.push(format!("表匹配 {:.0}%", tbl_j * 100.0));
        }
        if col_j > 0.0 {
            reasons.push(format!("列匹配 {:.0}%", col_j * 100.0));
        }
        if sk_sim > 0.0 {
            reasons.push(format!("结构相似 {:.0}%", sk_sim * 100.0));
        }
        hits.push(MatchHit {
            record: rec.clone(),
            score,
            reasons,
        });
    }

    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(top_n);
    hits
}

fn similarity(
    q_sk: &[String],
    q_tables: &BTreeSet<String>,
    q_cols: &BTreeSet<String>,
    rec: &SqlRecord,
) -> (f64, f64, f64) {
    let r_tables: BTreeSet<String> = rec.tables.iter().cloned().collect();
    let tbl_j = set_jaccard(q_tables, &r_tables);
    let r_cols: BTreeSet<String> = rec
        .columns
        .iter()
        .filter(|c| c.as_str() != "*")
        .cloned()
        .collect();
    let col_j = containment(q_cols, &r_cols);
    let sk_sim = jaccard_structure(q_sk, &rec.skeleton);
    (sk_sim, tbl_j, col_j)
}

fn set_jaccard<T>(a: &BTreeSet<T>, b: &BTreeSet<T>) -> f64
where
    T: Ord,
{
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.union(b).count();
    inter as f64 / union.max(1) as f64
}

/// 包含度：|a∩b| / |a|（查询列多数出现在记录列里即高分，记录列更多不算惩罚）。
fn containment<T>(q: &BTreeSet<T>, r: &BTreeSet<T>) -> f64
where
    T: Ord,
{
    if q.is_empty() {
        return 0.0;
    }
    q.intersection(r).count() as f64 / q.len() as f64
}

/// 渲染匹配结果为人读文本。
pub fn render_hits(index: &Index, query_sql: &str, hits: &[MatchHit]) -> String {
    if hits.is_empty() {
        return format!("没找到与该 SQL 匹配的源码位置。\n提示：先运行 `contextgate scan <项目根>` 生成索引，再重试。");
    }
    let mut out = vec![format!("查询 SQL：{}", query_sql), String::new()];
    for (i, h) in hits.iter().enumerate() {
        let rec = &h.record;
        let verb = sql_verb(&rec.skeleton)
            .map(|v| format!(" [{}]", v.to_uppercase()))
            .unwrap_or_default();
        out.push(format!(
            "{}. [{}] {} — {}{}（{}:{}）",
            i + 1,
            h.score,
            rec.id,
            rec.kind,
            verb,
            rec.file,
            rec.line
        ));
        out.push(format!("   SQL 模板: {}", abbrev(&rec.sql, 120)));
        if !rec.tables.is_empty() {
            out.push(format!("   涉及表: {}", rec.tables.join(", ")));
        }
        if !rec.columns.is_empty() {
            out.push(format!("   触碰列: {}", rec.columns.join(", ")));
        }
        if let Some(entry) = index.reverse.get(&rec.id) {
            if !entry.direct_callers.is_empty() {
                out.push(format!("   直接调用者: {}", entry.direct_callers.join(", ")));
            }
            if !entry.routes.is_empty() {
                let rs = entry
                    .routes
                    .iter()
                    .map(|r| format!("{} {}", r.method, r.path))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push(format!("   上游路由: {}", rs));
            }
        }
        if !h.reasons.is_empty() {
            out.push(format!("   依据: {}", h.reasons.join("；")));
        }
        if rec.mp_builtin {
            out.push("   ⚠️ MyBatis-Plus 内置方法（保守记录，`*` 为全列上界）".to_string());
        }
        if rec.dynamic {
            out.push("   ⚠️ 动态 SQL 模板（实际执行会随条件增减片段）".to_string());
        }
        out.push(String::new());
    }
    out.join("\n")
}

fn abbrev(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{}…", cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ReverseEntry;

    fn idx() -> Index {
        let mut index = Index::new("t", "now".into());
        let (sk, tables, cols) = normalize_sql("UPDATE users SET wallet_balance = ? WHERE id = ?");
        index.records.push(SqlRecord {
            id: "UserMapper#addBalance".into(),
            kind: "annotation-update".into(),
            sql: "UPDATE users SET wallet_balance = COALESCE(wallet_balance,0) + ? WHERE id = ?".into(),
            skeleton: sk,
            tables: tables.into_iter().collect(),
            columns: cols.into_iter().collect(),
            file: "src/UserMapper.java".into(),
            line: 24,
            owner: "UserMapper".into(),
            method: "addBalance".into(),
            mp_builtin: false,
            dynamic: false,
        });
        // 同表同列的 SELECT 与 INSERT（构造跨类型误报场景）
        for (id, kind, sql) in [
            ("UserMapper#findById", "annotation-select", "SELECT id, wallet_balance FROM users WHERE id = ?"),
            ("UserMapper#insertUser", "annotation-insert", "INSERT INTO users (id, wallet_balance) VALUES (?, ?)"),
        ] {
            let (sk, tables, cols) = normalize_sql(sql);
            index.records.push(SqlRecord {
                id: id.into(),
                kind: kind.into(),
                sql: sql.into(),
                skeleton: sk,
                tables: tables.into_iter().collect(),
                columns: cols.into_iter().collect(),
                file: "src/UserMapper.java".into(),
                line: 10,
                owner: "UserMapper".into(),
                method: id.split('#').next().unwrap_or("").into(),
                mp_builtin: false,
                dynamic: false,
            });
        }
        index.reverse.insert(
            "UserMapper#addBalance".into(),
            ReverseEntry {
                direct_callers: vec!["WalletService#recharge".into()],
                routes: vec![crate::model::Route {
                    method: "POST".into(),
                    path: "/wallet/recharge".into(),
                    controller: "WalletController".into(),
                    handler: "recharge".into(),
                }],
            },
        );
        index
    }

    #[test]
    fn executed_sql_matches_template() {
        let index = idx();
        let hits = find_sql(
            &index,
            "UPDATE users SET wallet_balance = 100.5 WHERE id = 42",
            5,
        );
        assert!(!hits.is_empty());
        assert_eq!(hits[0].record.id, "UserMapper#addBalance");
        assert!(hits[0].score > 0.5);
    }

    #[test]
    fn different_table_does_not_match() {
        let index = idx();
        let hits = find_sql(&index, "SELECT * FROM orders WHERE id = 1", 5);
        assert!(hits.is_empty() || hits[0].score < 0.3);
    }

    #[test]
    fn select_query_does_not_match_insert() {
        // 回归：SELECT 查询不得命中同表同列的 INSERT 记录（真实用户报告）
        let index = idx();
        let hits = find_sql(
            &index,
            "SELECT id, wallet_balance FROM users WHERE id = 42",
            8,
        );
        assert!(!hits.is_empty());
        // top1 必须是 SELECT 记录
        assert_eq!(hits[0].record.kind, "annotation-select");
        assert!(hits[0].score > 0.8, "select 记录得分应接近满分: {}", hits[0].score);
        // INSERT 记录若出现，必须被强降权
        if let Some(ins) = hits.iter().find(|h| h.record.kind == "annotation-insert") {
            assert!(ins.score < 0.3, "insert 记录得分应显著低于 select: {}", ins.score);
            assert!(ins.score * 2.0 < hits[0].score);
        }
        // UPDATE 记录同样不得排在 SELECT 记录之前
        let upd = hits.iter().find(|h| h.record.kind == "annotation-update");
        if let Some(u) = upd {
            assert!(u.score < hits[0].score);
        }
    }

    #[test]
    fn insert_query_matches_insert_not_select() {
        let index = idx();
        let hits = find_sql(
            &index,
            "INSERT INTO users (id, wallet_balance) VALUES (7, 99.5)",
            8,
        );
        assert!(!hits.is_empty());
        assert_eq!(hits[0].record.kind, "annotation-insert");
    }
}
