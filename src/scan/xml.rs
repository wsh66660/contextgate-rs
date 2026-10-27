//! XML mapper 解析与注解 SQL / 路由补扫（基于原始文本的轻量正则，
//! 避免 4000 行 Python 版的完整 AST 重建；精度以「反查定位」为目标）。

use crate::model::{Route, SqlRecord};
use crate::sqlfeat::normalize_sql;
use regex::Regex;

/// 解析一个 MyBatis XML：返回 (namespace, [(id, 语句文本, 起始行)])。
pub fn parse_xml_mapper(content: &str) -> (Option<String>, Vec<(String, String, u32)>) {
    let ns = Regex::new(r#"<mapper[^>]*namespace\s*=\s*"([^"]+)""#)
        .unwrap()
        .captures(content)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));

    let mut stmts = Vec::new();
    // regex crate 不支持反向引用，按标签类型逐个匹配
    for tag_name in ["select", "insert", "update", "delete"] {
        let tag = Regex::new(&format!(
            r#"(?s)<{tag_name}\b([^>]*)>(.*?)</{tag_name}>"#
        ))
        .unwrap();
        for cap in tag.captures_iter(content) {
            let id = cap
                .get(1)
                .and_then(|a| {
                    let attrs = a.as_str();
                    Regex::new(r#"id\s*=\s*"([^"]+)""#)
                        .unwrap()
                        .captures(attrs)
                        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
                })
                .unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            // 起始行：用语句开始位置近似
            let start = cap.get(0).map(|m| m.start()).unwrap_or(0);
            let line = content[..start].matches('\n').count() as u32 + 1;
            // 语句体：动态标签拍平为内部文本
            let mut text = cap.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
            text = flatten_dynamic_tags(&text);
            stmts.push((id, text, line));
        }
    }
    (ns, stmts)
}

/// 拍平动态标签：<if test="..."> 保留内部文本；<foreach> 展开为一次迭代形状；
/// <where>/<set>/<trim>/<include> 保留/替换为内部内容。
pub fn flatten_dynamic_tags(xml: &str) -> String {
    let mut text = xml.to_string();
    // include -> 引用的 sql 片段不做跨段解析（v1 留空处理），保留位置即可
    for tag in ["where", "set", "trim", "if", "foreach", "choose", "when", "otherwise"] {
        let re = Regex::new(&format!(r#"(?is)<{tag}\b[^>]*>(.*?)</{tag}>"#)).unwrap();
        text = re
            .replace_all(&text, |c: &regex::Captures| {
                let inner = c.get(1).map(|m| m.as_str()).unwrap_or("");
                // foreach：把 #{item} 形态保留为 ?，重复一次迭代形状
                if tag == "foreach" {
                    let item_re = Regex::new(r##"\$\{[^}]*\}|#\{[^}]*\}"##).unwrap();
                    let inner2 = item_re.replace_all(inner, "?").to_string();
                    format!(" {} ? ", inner2)
                } else {
                    format!(" {} ", inner)
                }
            })
            .to_string();
    }
    // 未知 ${} / #{} 残留 -> ?
    let ph = Regex::new(r#"\$\{[^}]*\}|#\{[^}]*\}"#).unwrap();
    let text = ph.replace_all(&text, "?").to_string();
    // HTML 转义还原
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// 补扫注解 SQL：`@Select("...")` 等后面跟的第一个方法名。
pub fn scan_annotation_sql(
    rel: &str,
    content: &str,
    class_name: &str,
    records: &mut Vec<SqlRecord>,
) {
    let re = Regex::new(r#"(?s)@(Select|Insert|Update|Delete)\s*\(\s*"((?:[^"\\]|\\.)*)"\s*\)"#).unwrap();
    for cap in re.captures_iter(content) {
        let kind_full = cap.get(1).unwrap().as_str().to_lowercase();
        let raw = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let sql_text = raw.replace("\\n", " ").replace("\\\"", "\"");
        if !crate::scan::java::looks_like_sql(&sql_text) {
            continue;
        }
        // 后面的方法名：跳过注解结束到下一个 '(' 之间的标识符
        let end = cap.get(0).unwrap().end();
        let after = &content[end..];
        let mname = after
            .trim_start_matches([' ', '\n', '\r', '\t'])
            .split('(')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .last()
            .unwrap_or("")
            .trim()
            .to_string();
        let method = if mname.is_empty() {
            "?".into()
        } else {
            // 可能是返回类型，取最后一个词；若像类型（首字母大写）则再取下一段
            if mname.chars().next().map_or(false, |c| c.is_uppercase()) {
                after[end_after_type(after)..]
                    .split('(')
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .last()
                    .unwrap_or(&mname)
                    .to_string()
            } else {
                mname
            }
        };
        let line = content[..cap.get(0).unwrap().start()].matches('\n').count() as u32 + 1;
        let (sk, tables, cols) = normalize_sql(&sql_text);
        records.push(SqlRecord {
            id: format!("{}#{}", class_name, method),
            kind: format!("annotation-{}", if kind_full == "select" { "select".to_string() } else { kind_full.clone() }),
            sql: sql_text,
            skeleton: sk,
            tables: tables.into_iter().collect(),
            columns: cols.into_iter().collect(),
            file: rel.to_string(),
            line,
            owner: class_name.to_string(),
            method: method.clone(),
            mp_builtin: false,
            dynamic: raw.contains('#') || raw.contains('$'),
        });
    }
}

fn end_after_type(after: &str) -> usize {
    // 跳过 "java.util.List " 之类的返回类型
    let head = after.split('(').next().unwrap_or("");
    head.len()
}

/// 补扫路由：`@RequestMapping("/x")` / `@GetMapping("...")` 等，后面的方法名。
pub fn scan_routes(rel: &str, content: &str, class_name: &str, routes: &mut Vec<Route>) {
    let is_ctrl = Regex::new(r#"@(RestController|Controller)\b"#)
        .unwrap()
        .is_match(content);
    if !is_ctrl {
        return;
    }
    // 类级前缀
    let class_prefix = Regex::new(r#"@RequestMapping\s*\(\s*(?:value\s*=\s*)?"([^"]+)""#)
        .unwrap()
        .captures(content)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        .unwrap_or_default();

    let re = Regex::new(
        r#"@(Get|Post|Put|Delete|Patch|Request)Mapping\s*\(\s*(?:value\s*=\s*|path\s*=\s*)?"([^"]+)""#,
    )
    .unwrap();
    for cap in re.captures_iter(content) {
        let verb = match cap.get(1).unwrap().as_str() {
            "Request" => "ANY".to_string(),
            v => v.to_uppercase(),
        };
        let path = cap.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
        let end = cap.get(0).unwrap().end();
        let after = &content[end..];
        let handler = after
            .trim_start()
            .split('(')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .last()
            .unwrap_or("")
            .to_string();
        let full = if class_prefix.is_empty() {
            path
        } else {
            format!("{}/{}", class_prefix.trim_end_matches('/'), path.trim_start_matches('/'))
        };
        routes.push(Route {
            method: verb,
            path: full,
            controller: class_name.to_string(),
            handler,
        });
    }
    let _ = rel;
}
