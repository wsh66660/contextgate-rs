//! 扫描编排：Java + MyBatis XML -> Index（含逆向索引）。

pub mod java;
pub mod xml;

use crate::model::{CallEdge, Index, ReverseEntry, SqlRecord};
use crate::sqlfeat::normalize_sql;
use java::{camel_to_snake, parse_java, scan_class};
use std::collections::BTreeMap;
use std::path::Path;

/// 扫描项目根，产出完整索引。
pub fn scan_project(root: &Path) -> Index {
    let project = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| root.to_string_lossy().to_string());

    let java_files = java::collect_java_files(root);

    // 第一遍：解析所有类，建实体 -> 表映射
    let mut classes = Vec::new();
    for (rel, src) in &java_files {
        if let Some(cls) = parse_java(rel, src) {
            classes.push(cls);
        }
    }
    let mut entity_tables: BTreeMap<String, String> = BTreeMap::new();
    let mut entities = Vec::new();
    for cls in &classes {
        let is_entity = cls.annotations.contains(&"TableName".to_string())
            || ["entity", "domain", "model", "po"]
                .iter()
                .any(|p| cls.package.split('.').any(|seg| seg == *p));
        let is_special = cls.annotations.contains(&"Mapper".to_string())
            || cls.annotations.contains(&"RestController".to_string())
            || cls.annotations.contains(&"Controller".to_string())
            || cls.name.ends_with("Mapper")
            || cls.name.ends_with("Dao")
            || cls.name.ends_with("Controller")
            || cls.name.ends_with("Example")
            || cls.name.ends_with("Service");
        if is_entity && !is_special {
            let table = cls
                .ann_values
                .get("TableName")
                .cloned()
                .unwrap_or_else(|| camel_to_snake(&cls.name));
            entity_tables.insert(cls.name.clone(), table.clone());
            entities.push(crate::model::EntityTable {
                entity: cls.name.clone(),
                table,
                file: cls.file.clone(),
            });
        }
    }

    // 第二遍：SQL 记录 + 调用图 + 路由
    let mut records: Vec<SqlRecord> = Vec::new();
    let mut edges: Vec<CallEdge> = Vec::new();
    let mut routes = Vec::new();
    for (rel, src) in &java_files {
        if let Some(cls) = classes.iter().find(|c| &c.file == rel) {
            let class_name = cls.name.clone();
            scan_class(cls, &entity_tables, &mut records, &mut edges, &mut routes, &mut entities);
            xml::scan_annotation_sql(rel, src, &class_name, &mut records);
            xml::scan_routes(rel, src, &class_name, &mut routes);
        }
    }

    // 第三遍：MyBatis XML
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.path().extension().map_or(false, |x| x == "xml")
        })
    {
        let p = entry.path();
        if let Ok(content) = std::fs::read_to_string(p) {
            if !content.contains("<mapper") {
                continue;
            }
            let (ns, stmts) = xml::parse_xml_mapper(&content);
            let rel = p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/");
            let ns_simple = ns
                .as_deref()
                .and_then(|n| n.rsplit('.').next())
                .unwrap_or("UnknownMapper")
                .to_string();
            for (id, text, line) in stmts {
                let (sk, tables, cols) = normalize_sql(&text);
                records.push(SqlRecord {
                    id: format!("{}#{}", ns_simple, id),
                    kind: "xml".into(),
                    sql: text.trim().to_string(),
                    skeleton: sk,
                    tables: tables.into_iter().collect::<Vec<String>>(),
                    columns: cols.into_iter().collect::<Vec<String>>(),
                    file: rel.clone(),
                    line,
                    owner: ns_simple.clone(),
                    method: id.clone(),
                    mp_builtin: false,
                    dynamic: true,
                });
                // XML 语句属于 mapper 接口方法 -> 建一条 mapper 方法边
                edges.push(CallEdge {
                    caller: format!("{}#{}", ns_simple, id),
                    callee: format!("{}#{}", ns_simple, id),
                });
            }
        }
    }

    let mut index = Index::new(&project, now_string());
    index.records = records;
    index.call_graph = edges;
    index.routes = routes;
    index.entities = entities;
    build_reverse_index(&mut index);
    index
}

/// 逆向索引：mapper 方法 -> 直接调用者 + 可达路由（BFS 向上）。
pub fn build_reverse_index(index: &mut Index) {
    // caller -> callees
    let mut graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in &index.call_graph {
        if e.caller != e.callee {
            graph
                .entry(e.caller.clone())
                .or_default()
                .push(e.callee.clone());
        }
    }
    // 翻转：callee -> callers
    let mut rev: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in &index.call_graph {
        if e.caller != e.callee {
            rev.entry(e.callee.clone()).or_default().push(e.caller.clone());
        }
    }
    // 路由 -> (controller, handler) 直接建节点
    let route_of = |node: &str| -> Vec<crate::model::Route> {
        let (c, h) = node.split_once('#').unwrap_or((node, ""));
        index
            .routes
            .iter()
            .filter(|r| r.controller == c && (h.is_empty() || r.handler == h))
            .cloned()
            .collect()
    };

    let mut out = BTreeMap::new();
    let keys: Vec<String> = rev.keys().cloned().collect();
    for target in keys {
        // BFS 向上
        let mut ancestors = std::collections::BTreeSet::new();
        let mut stack: Vec<String> = rev.get(&target).cloned().unwrap_or_default();
        let mut seen: std::collections::BTreeSet<String> = stack.iter().cloned().collect();
        while let Some(node) = stack.pop() {
            ancestors.insert(node.clone());
            if let Some(ups) = rev.get(&node) {
                for up in ups {
                    if seen.insert(up.clone()) {
                        stack.push(up.clone());
                    }
                }
            }
        }
        let direct = rev.get(&target).cloned().unwrap_or_default();
        let mut entry = ReverseEntry {
            direct_callers: direct,
            routes: Vec::new(),
        };
        // 路由：祖先里的 controller#handler
        for a in &ancestors {
            entry.routes.extend(route_of(a));
        }
        // mapper 方法本身若是 controller handler（少见）
        if entry.routes.is_empty() {
            entry.routes = route_of(&target);
        }
        entry.routes.sort_by(|a, b| (&a.method, &a.path).cmp(&(&b.method, &b.path)));
        entry.routes.dedup_by(|a, b| a.method == b.method && a.path == b.path && a.controller == b.controller);
        entry.direct_callers.sort();
        entry.direct_callers.dedup();
        out.insert(target, entry);
    }
    index.reverse = out;
}

fn now_string() -> String {
    // 无 chrono 依赖：用系统时间秒转 UTC 简易 ISO（足够标识生成时间）
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let (y, mo, d) = civil_from_days(days as i64);
    let rem = secs % 86400;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        mo,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
