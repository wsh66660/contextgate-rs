//! 匹配引擎：把一条实际执行的 SQL 与索引记录打分匹配，输出源码位置。

use crate::model::Index;
use crate::model::SqlRecord;
use crate::sqlfeat::{jaccard, normalize_sql};
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
    let mut hits: Vec<MatchHit> = Vec::new();

    for rec in &index.records {
        // MP 内置占位记录降权参与（SELECT * FROM x 保守形状），但表归因仍可参考
        let (sk_sim, tbl_j, col_j) = similarity(&q_sk, &q_tables, &q_cols, rec);
        if tbl_j == 0.0 && sk_sim < 0.5 {
            continue; // 表完全对不上且骨架也不像，直接剪枝
        }
        let mut score = 0.5 * tbl_j + 0.2 * col_j + 0.3 * sk_sim;
        if rec.mp_builtin {
            score *= 0.6; // 保守占位记录降权
        }
        if rec.dynamic {
            score *= 1.05; // 动态模板本就只含形状，略补偿
        }
        if score <= 0.05 {
            continue;
        }
        let mut reasons = Vec::new();
        if tbl_j > 0.0 {
            reasons.push(format!("表匹配 {:.0}%", tbl_j * 100.0));
        }
        if col_j > 0.0 {
            reasons.push(format!("列匹配 {:.0}%", col_j * 100.0));
        }
        if sk_sim > 0.0 {
            reasons.push(format!("骨架相似 {:.0}%", sk_sim * 100.0));
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
    let sk_sim = jaccard(q_sk, &rec.skeleton);
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
        out.push(format!(
            "{}. [{}] {} — {}（{}:{}）",
            i + 1,
            h.score,
            rec.id,
            rec.kind,
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
}
