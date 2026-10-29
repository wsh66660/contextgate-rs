//! 索引持久化：JSON 读写。
//!
//! 落盘形态对内存形态做两处体积优化（load 时由 `hydrate` 还原）：
//! 1. `skeleton` 不序列化——可由 sql 即时派生，是索引体积的大头
//! 2. 重复 SQL 文本去重——首次出现存原文，其余以 `@p{n}` 占位（`sql_pool`）

use crate::model::Index;
use std::collections::HashMap;
use std::path::Path;

/// 保存索引到 JSON 文件（自动去重 SQL 文本）。
pub fn save(index: &Index, path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let deduped = dedup_sql(index);
    let json = serde_json::to_string_pretty(&deduped)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(path, json)
}

/// 从 JSON 文件加载索引（自动还原去重池 + 派生 skeleton）。
pub fn load(path: &Path) -> std::io::Result<Index> {
    let text = std::fs::read_to_string(path)?;
    let index: Index = serde_json::from_str(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(hydrate(index))
}

/// 序列化前的去重视图：出现 ≥2 次的 sql 全部替换为 `@p{n}` 占位，
/// 原文只在 `sql_pool` 里存一份（N 份重复 -> 1 份原文 + N 个占位符）。
fn dedup_sql(index: &Index) -> Index {
    let mut out = index.clone();
    // 先统计重复
    let mut count: HashMap<&str, usize> = HashMap::new();
    for rec in &out.records {
        *count.entry(rec.sql.as_str()).or_default() += 1;
    }
    if count.values().all(|&c| c == 1) {
        return out; // 无重复，零开销
    }
    // 再统一替换（含首次出现）
    let mut markers: HashMap<String, String> = HashMap::new(); // sql 原文 -> 占位符
    for rec in &mut out.records {
        let marker = markers.get(&rec.sql).cloned();
        match marker {
            Some(m) => rec.sql = m,
            None => {
                let m = format!("@p{}", markers.len());
                out.sql_pool.insert(m.clone(), rec.sql.clone());
                markers.insert(rec.sql.clone(), m.clone());
                rec.sql = m;
            }
        }
    }
    out
}

/// 落盘形态 -> 内存形态：还原去重池中的 SQL 原文，并由 sql 派生 skeleton。
pub fn hydrate(mut index: Index) -> Index {
    for rec in &mut index.records {
        if let Some(orig) = index.sql_pool.get(rec.sql.as_str()) {
            rec.sql = orig.clone();
        }
        if rec.skeleton.is_empty() {
            rec.skeleton = crate::sqlfeat::normalize_sql(&rec.sql).0;
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SqlRecord, *};

    fn sample() -> Index {
        let mut index = Index::new("t", "now".into());
        let (sk, tables, cols) = normalize_sql("UPDATE users SET wallet_balance = ? WHERE id = ?");
        let mk = |id: &str,
                  sk: Vec<String>,
                  tables: std::collections::BTreeSet<String>,
                  cols: std::collections::BTreeSet<String>| SqlRecord {
            id: id.into(),
            kind: "annotation-update".into(),
            sql: "UPDATE users SET wallet_balance = ? WHERE id = ?".into(),
            skeleton: sk,
            tables: tables.into_iter().collect(),
            columns: cols.into_iter().collect(),
            file: "src/UserMapper.java".into(),
            line: 24,
            owner: "UserMapper".into(),
            method: "addBalance".into(),
            mp_builtin: false,
            dynamic: false,
        };
        // 两条同 sql 记录（模拟多环境重复），触发去重
        index.records.push(mk("A#x", sk.clone(), tables.clone(), cols.clone()));
        index.records.push(mk("B#y", sk, tables, cols));
        index
    }

    #[test]
    fn roundtrip_dedups_sql_and_derives_skeleton() {
        // 优化②③回归：落盘不含 skeleton、重复 sql 只存一份，load 后完全还原
        let path = std::env::temp_dir().join(format!("cg-store-test-{}.json", std::process::id()));
        let index = sample();
        save(&index, &path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("\"skeleton\""), "skeleton 不应落盘");
        assert_eq!(
            raw.matches("UPDATE users SET wallet_balance").count(),
            1,
            "重复 sql 应只存一份原文，其余为占位符"
        );
        assert!(raw.contains("@p0"), "重复处应为占位符");
        assert!(
            !raw.contains("\"sql\": \"UPDATE users"),
            "重复 sql 的原文应只存在于去重池（记录侧全部占位化）"
        );

        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.records.len(), 2);
        for rec in &loaded.records {
            assert_eq!(rec.sql, "UPDATE users SET wallet_balance = ? WHERE id = ?");
            assert!(!rec.skeleton.is_empty(), "load 后 skeleton 应已派生");
        }
        assert_eq!(loaded.records[0].skeleton, loaded.records[1].skeleton);
    }

    #[test]
    fn legacy_index_with_skeleton_still_loads() {
        // 旧版索引（带 skeleton 字段、无 sql_pool）必须能读
        let legacy = r#"{
            "project": "old", "generated_at": "2026-01-01T00:00:00Z",
            "records": [{
                "id": "A#x", "kind": "xml",
                "sql": "SELECT id FROM t WHERE uid = ?",
                "skeleton": ["select", "id", "from", "t", "where", "uid", "=", "?"],
                "tables": ["t"], "columns": ["id", "uid"],
                "file": "a.xml", "line": 1, "owner": "A", "method": "x",
                "mp_builtin": false, "dynamic": false
            }],
            "call_graph": [], "routes": [], "entities": []
        }"#;
        let path = std::env::temp_dir().join(format!("cg-legacy-test-{}.json", std::process::id()));
        std::fs::write(&path, legacy).unwrap();
        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.records[0].skeleton.len(), 8);
        assert_eq!(loaded.records[0].sql, "SELECT id FROM t WHERE uid = ?");
    }

    use crate::sqlfeat::normalize_sql;
}
