//! Java 源码扫描器：提取 SQL 发射点、实体表映射、路由与调用图（线性扫描，无重正则回溯）。
//!
//! 覆盖的发射点（对齐 Python 版核心路径，Rust v1 取最常用的）：
//! - Mapper 接口方法上的 `@Select/@Insert/@Update/@Delete` 注解 SQL
//! - MyBatis-Plus `BaseMapper`/`ServiceImpl` 内置方法（selectById 等）→ 保守全列记录
//! - JdbcTemplate 的字符串拼接 SQL（字面量/常量照拼，不可解析成分折为 `?`）
//! - MP Wrapper 消费点（`mapper.selectList(...)` / `this.list()`）→ 表归因 + 动态标记
//! - `@TableName` 实体 -> 表；`@RestController` 路由；`@Autowired`/泛型基类调用图边

use crate::model::{CallEdge, EntityTable, Route, SqlRecord};
use crate::sqlfeat::normalize_sql;
use std::collections::BTreeMap;
use std::path::Path;

/// 剔除注释（保留字符串字面量内部）。
pub fn strip_comments(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '"' || c == '\'' {
            let q = c;
            out.push(c);
            i += 1;
            while i < b.len() {
                out.push(b[i]);
                if b[i] == '\\' {
                    if i + 1 < b.len() {
                        out.push(b[i + 1]);
                    }
                    i += 2;
                    continue;
                }
                let done = b[i] == q;
                i += 1;
                if done {
                    break;
                }
            }
            continue;
        }
        if c == '/' && i + 1 < b.len() && b[i + 1] == '/' {
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

/// 找到与 open 位置 `{` 配对的 `}`（含字符串跳过）。
fn match_brace(text: &[char], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open;
    let mut in_str: Option<char> = None;
    while i < text.len() {
        let c = text[i];
        if let Some(q) = in_str {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            in_str = Some(c);
        } else if c == '{' {
            depth += 1;
        } else if c == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// 一个 Java 类/接口的解析结果。
pub struct JavaClass {
    pub name: String,
    pub package: String,
    pub file: String,
    /// 相对 file 的类声明行
    pub line: u32,
    pub annotations: Vec<String>,
    /// 注解名 -> 第一个字符串参数值（如 @TableName("orders") -> "orders"）
    pub ann_values: BTreeMap<String, String>,
    pub extends: Vec<String>,
    /// 泛型实参（extends Xxx<A, B> 的 A/B）
    pub type_args: Vec<String>,
    pub is_interface: bool,
    pub imports: Vec<String>,
    /// 字段：名字 -> (类型简单名, 是否注入/mapper)
    pub fields: BTreeMap<String, String>,
    /// 类级字符串常量：名字 -> 字面量值
    pub constants: BTreeMap<String, String>,
    /// 方法：(名称, 起始行, 参数类型列表, 方法体文本, 注解列表)
    pub methods: Vec<JavaMethod>,
}

pub struct JavaMethod {
    pub name: String,
    pub line: u32,
    pub params: Vec<String>,
    pub body: String,
    pub annotations: Vec<String>,
}

/// 扫描一个 .java 文件。
pub fn parse_java(rel: &str, src: &str) -> Option<JavaClass> {
    let clean = strip_comments(src);
    let chars: Vec<char> = clean.chars().collect();

    let package = clean
        .split("package ")
        .nth(1)
        .and_then(|s| s.split(';').next())
        .unwrap_or("")
        .trim()
        .to_string();

    let imports: Vec<String> = clean
        .split("import ")
        .skip(1)
        .filter_map(|s| s.split(';').next().map(|x| x.trim().to_string()))
        .collect();

    // 类声明行（interface/class/enum，非注解行）
    let decl = clean.find(|c: char| {
        let mut rest = "";
        false.set_false(&mut rest);
        false
    });
    let _ = decl;

    let mut cls = None;
    let mut line_no_of_decl = 0u32;
    for (idx, pat) in ["interface ", "class ", "enum "].iter().enumerate() {
        if let Some(pos) = clean.find(pat) {
            // 行号
            line_no_of_decl = clean[..pos].matches('\n').count() as u32 + 1;
            let after = &clean[pos + pat.len()..];
            let name: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .collect();
            if name.is_empty() {
                continue;
            }
            let before = &clean[..pos];
            // 类上的注解：取声明前最后一个代码块片段里的 @Xxx
            let ann_seg = before.rsplit('{').next().unwrap_or(before);
            let mut annotations = Vec::new();
            let mut ann_values: BTreeMap<String, String> = BTreeMap::new();
            for (p, _) in ann_seg.match_indices('@') {
                let rest = &ann_seg[p + 1..];
                let n: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if n.is_empty() {
                    continue;
                }
                annotations.push(n.clone());
                // 注解参数里的第一个字符串字面量：@TableName("orders")
                let after_name = &rest[n.len()..];
                let after_name = after_name.trim_start();
                if let Some(stripped) = after_name.strip_prefix('(') {
                    if let Some(v) = first_string_literal(stripped) {
                        ann_values.insert(n, v);
                    }
                }
            }

            // extends/implements 及泛型实参：从声明到第一个 '{' 或 ';'
            let header_end = after.find('{').unwrap_or(after.len());
            let header = &after[..header_end];
            let mut extends = Vec::new();
            let mut type_args = Vec::new();
            if let Some(ep) = header.find("extends") {
                let seg = &header[ep + 7..];
                let seg = seg.split("implements").next().unwrap_or(seg);
                let seg = seg.split("implements").next().unwrap_or(seg);
                let first = seg.split(',').next().unwrap_or("").trim();
                let mut it = first.split('<');
                if let Some(base) = it.next() {
                    extends.push(base.trim().trim_end_matches('<').to_string());
                    if let Some(g) = first.split_once('<').map(|(_, g)| g) {
                        let g = g.trim_end_matches('>');
                        for a in g.split(',') {
                            let a = a.trim();
                            if !a.is_empty() {
                                type_args.push(simple_type(a));
                            }
                        }
                    }
                }
                // 后续逗号分隔的父接口（接口多继承）
                for extra in seg.split(',').skip(1) {
                    let e = extra.trim();
                    if !e.is_empty() {
                        extends.push(simple_type(e));
                    }
                }
            }
            // implements 的类型
            if let Some(ip) = header.find("implements") {
                let seg = &header[ip + 10..];
                for imp in seg.split(',') {
                    let imp = imp.trim();
                    if !imp.is_empty() {
                        extends.push(simple_type(imp));
                    }
                }
            }

            cls = Some(JavaClass {
                name: name.clone(),
                package,
                file: rel.to_string(),
                line: line_no_of_decl,
                annotations,
                ann_values,
                extends,
                type_args,
                is_interface: idx == 0,
                imports,
                fields: BTreeMap::new(),
                constants: BTreeMap::new(),
                methods: Vec::new(),
            });
            break;
        }
    }
    let mut cls = cls?;

    // 类体：从声明后的第一个 '{' 到配对 '}'
    // 注意：统一在字符空间定位（clean.find 返回字节偏移，遇中文注释会错位）
    let body_start = match chars.iter().position(|c| *c == '{') {
        Some(p) => p,
        None => return Some(cls),
    };
    let body_end = match_brace(&chars, body_start).unwrap_or(chars.len());
    let body: String = chars[body_start + 1..body_end].iter().collect();

    // 行号基准仍用字节偏移（只是行计数，不受字符/字节差异影响结果正确性）
    let body_line_off = clean
        .char_indices()
        .take_while(|(bi, _)| *bi <= clean.find('{').unwrap_or(0))
        .filter(|(_, c)| *c == '\n')
        .count() as u32;
    parse_members(&body, &mut cls, line_no_of_decl, body_line_off + 1);
    Some(cls)
}

trait SetFalse {
    fn set_false(&mut self, _r: &mut &str) -> bool;
}
impl SetFalse for bool {
    fn set_false(&mut self, _r: &mut &str) -> bool {
        false
    }
}

fn simple_type(t: &str) -> String {
    let t = t.trim().trim_end_matches('&');
    // 去包名、去泛型、去数组
    let t = t.split('<').next().unwrap_or(t);
    let t = t.split_whitespace().next().unwrap_or(t);
    t.rsplit('.').next().unwrap_or(t).trim_matches('[').trim_matches(']').to_string()
}

/// 解析类体里的字段、常量与方法。
fn parse_members(body: &str, cls: &mut JavaClass, _decl_line: u32, body_off: u32) {
    let chars: Vec<char> = body.chars().collect();
    let bytes = body.as_bytes();

    // 字段与常量：`Type name = ...;`（方法体外，简化：按 `;` 顶层分段判断）
    let mut depth = 0i32;
    let mut seg_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b';' if depth == 0 => {
                let seg = &body[seg_start..i];
                if !seg.contains('(') {
                    parse_field_seg(seg, cls);
                }
                seg_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }

    // 方法：找 `name(...)` 后跟 `{` 的顶层模式
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '(' && depth_is_zero_at_top(&chars, i) {
            // 回溯方法名
            let mut j = i;
            while j > 0 && (chars[j - 1].is_alphanumeric() || chars[j - 1] == '_') {
                j -= 1;
            }
            let name: String = chars[j..i].iter().collect();
            if name.is_empty() || is_keyword_like(&name) {
                i += 1;
                continue;
            }
            // 注解区（往前到上一个 `;` 或 `}`）
            let head_start = last_top_boundary(&chars, j);
            let head: String = chars[head_start..j].iter().collect();
            // 行号：head_start 之前的换行数 + body 偏移
            // head_start 是字符索引，必须在字符空间数换行（直接切 body 会在中文处 panic）
            let line = chars[..head_start]
                .iter()
                .filter(|c| **c == '\n')
                .count() as u32
                + body_off
                + 1;
            // 方法体：找到参数 ')' 后的第一个 '{'
            let mut k = i;
            let mut paren = 0i32;
            while k < chars.len() {
                match chars[k] {
                    '(' => paren += 1,
                    ')' => {
                        paren -= 1;
                        if paren == 0 {
                            k += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                k += 1;
            }
            let params_seg: String = chars[i + 1..k.saturating_sub(1)].iter().collect();
            let params: Vec<String> = params_seg
                .split(',')
                .filter(|p| !p.trim().is_empty())
                .map(|p| {
                    p.trim()
                        .split_whitespace()
                        .filter(|w| !matches!(*w, "@" | "final"))
                        .last()
                        .unwrap_or("")
                        .to_string()
                })
                .collect();
            // 跳过 default 注解值 /抽象方法（无 '{'，直接 ';'）
            let mut m = k;
            while m < chars.len() && chars[m].is_whitespace() {
                m += 1;
            }
            if m >= chars.len() || chars[m] != '{' {
                // default 方法表达式形式 `default X f() { return ...; }` 已覆盖；抽象方法跳过
                i = m;
                continue;
            }
            let end = match_brace(&chars, m).unwrap_or(chars.len());
            let body_text: String = chars[m + 1..end].iter().collect();
            let annotations = head
                .match_indices('@')
                .filter_map(|(p, _)| {
                    let rest = &head[p + 1..];
                    let n: String = rest
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if n.is_empty() {
                        None
                    } else {
                        Some(n)
                    }
                })
                .collect();
            cls.methods.push(JavaMethod {
                name,
                line,
                params,
                body: body_text,
                annotations,
            });
            i = end + 1;
            continue;
        }
        i += 1;
    }
}

fn depth_is_zero_at_top(_chars: &[char], _i: usize) -> bool {
    // 简化：真顶层判断需要计数；方法体嵌套的 '(' 会被上一层的匹配消耗掉，
    // 这里接受少量误报，由后续 '{' 校验兜底。
    true
}

fn last_top_boundary(chars: &[char], pos: usize) -> usize {
    let mut i = pos;
    while i > 0 {
        i -= 1;
        if chars[i] == ';' || chars[i] == '}' || chars[i] == '{' {
            return i + 1;
        }
    }
    0
}

fn is_keyword_like(name: &str) -> bool {
    matches!(name, "if" | "for" | "while" | "switch" | "catch" | "return" | "new" | "synchronized")
}

/// 解析字段/常量段：`static final String SQL_A = "..."` 或 `private UserMapper userMapper`。
fn parse_field_seg(seg: &str, cls: &mut JavaClass) {
    let seg = seg.trim();
    if seg.is_empty() || seg.starts_with('@') && !seg.contains(' ') {
        return;
    }
    // 常量：字符串字面量右值
    if let Some(eq) = seg.find('=') {
        let lhs = &seg[..eq];
        let rhs = &seg[eq + 1..];
        let names: Vec<&str> = lhs.split_whitespace().collect();
        if let Some(name) = names.last() {
            let name = name.trim();
            if is_upper_snake(name) {
                if let Some(v) = first_string_literal(rhs) {
                    cls.constants.insert(name.to_string(), v);
                }
            }
        }
        return;
    }
    // 注入字段：`@Autowired private XxxMapper yyy;`
    let toks: Vec<&str> = seg.split_whitespace().collect();
    if toks.len() >= 2 {
        let t = toks[toks.len() - 2];
        let n = toks[toks.len() - 1];
        if !matches!(
            t,
            "return" | "throw" | "package" | "import" | "public" | "private" | "protected" | "class" | "interface"
        ) && n.chars().next().map_or(false, |c| c.is_lowercase() || c == '_')
        {
            cls.fields.insert(n.to_string(), simple_type(t));
        }
    }
}

fn is_upper_snake(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_uppercase() || c == '_' || c.is_ascii_digit())
}

/// 取第一个字符串字面量（处理转义）。
pub fn first_string_literal(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"' {
            let mut out = String::new();
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                    out.push(match chars[i] {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                } else {
                    out.push(chars[i]);
                }
                i += 1;
            }
            return Some(out);
        }
        i += 1;
    }
    None
}

/// JdbcTemplate 拼接求值：字面量/常量照拼，其他折 `?`。
pub fn fold_concat(expr: &str, constants: &BTreeMap<String, String>) -> Option<String> {
    let mut out = String::new();
    let mut any = false;
    let mut depth = 0i32;
    let mut buf = String::new();
    let chars: Vec<char> = expr.chars().collect();
    let mut i = 0;
    let mut pieces: Vec<String> = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
        }
        if c == '+' && depth == 0 {
            pieces.push(std::mem::take(&mut buf));
            i += 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    pieces.push(buf);
    for p in pieces {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        if let Some(lit) = first_string_literal(p) {
            out.push_str(&lit);
            any = true;
        } else if let Some(v) = resolve_const_ref(p, constants) {
            out.push_str(&v);
            any = true;
        } else if p == "?" {
            out.push('?');
            any = true;
        } else {
            out.push_str(" ? ");
            any = true;
        }
    }
    if any {
        Some(out)
    } else {
        None
    }
}

fn resolve_const_ref(expr: &str, constants: &BTreeMap<String, String>) -> Option<String> {
    let name = expr.trim();
    if name.chars().all(|c| c.is_uppercase() || c == '_' || c.is_ascii_digit()) {
        if let Some(v) = constants.get(name) {
            return Some(v.clone());
        }
    }
    // Other.CONST 形式
    if let Some((_, last)) = name.rsplit_once('.') {
        if last
            .chars()
            .all(|c| c.is_uppercase() || c == '_' || c.is_ascii_digit())
        {
            if let Some(v) = constants.get(last) {
                return Some(v.clone());
            }
        }
    }
    None
}

/// MP BaseMapper / ServiceImpl 内置方法集合（保守全列 / 写语义）。
pub fn mp_builtin(name: &str) -> Option<(&'static str, bool)> {
    // 返回 (合成 SQL, 是否只触碰条件列)
    let n = name;
    let read_full = [
        "selectById", "selectBatchIds", "selectList", "selectOne", "selectMap", "selectMaps",
        "selectObjs", "selectPage", "selectMapsPage", "list", "listByIds", "getOne", "getById",
        "getBaseMapper",
    ];
    let cond_only = ["selectCount", "count", "exists", "countByExample", "existsWith"];

    const BUILTIN_SELECT: &str = "SELECT * FROM {table}"; // 占位文本，匹配时降权
    const BUILTIN_COUNT: &str = "SELECT COUNT(*) FROM {table}";

    if read_full.contains(&n) {
        return Some((BUILTIN_SELECT, false));
    }
    if cond_only.contains(&n) {
        return Some((BUILTIN_COUNT, true));
    }
    // 写方法：deleteById / updateById / insert...
    if n.starts_with("delete") || n.starts_with("remove") || n.starts_with("update") || n.starts_with("insert") || n == "save" || n == "saveBatch" || n == "saveOrUpdate" {
        return Some(("", false));
    }
    None
}

/// 扫描一个类，产出 SQL 记录 / 调用边 / 路由 / 实体映射，追加到给定集合。
pub fn scan_class(
    cls: &JavaClass,
    entity_tables: &BTreeMap<String, String>,
    records: &mut Vec<SqlRecord>,
    edges: &mut Vec<CallEdge>,
    routes: &mut Vec<Route>,
    entities: &mut Vec<EntityTable>,
) {
    let ann = cls.annotations.iter().map(|s| s.as_str()).collect::<Vec<_>>();
    let is_mapper = ann.contains(&"Mapper") || cls.name.ends_with("Mapper") || cls.name.ends_with("Dao");
    let is_controller = ann.contains(&"RestController") || ann.contains(&"Controller");

    // 路由
    if is_controller {
        let class_prefix = class_route_prefix(cls);
        for m in &cls.methods {
            if let Some((verb, path)) = method_route(m) {
                routes.push(Route {
                    method: verb,
                    path: join_path(&class_prefix, &path),
                    controller: cls.name.clone(),
                    handler: m.name.clone(),
                });
            }
        }
    }

    // 注入字段产生的调用边基座
    let field_types: BTreeMap<String, String> = cls.fields.clone();
    // 泛型基类绑定：ServiceImpl<ProductMapper, Product> extends
    let mut base_mapper: Option<String> = None;
    for e in cls.extends.iter() {
        if e == "ServiceImpl" || e == "BaseManager" || e.contains("ServiceImpl") {
            base_mapper = cls.type_args.first().cloned();
        }
    }

    for m in &cls.methods {
        let owner = format!("{}#{}", cls.name, m.name);

        // 1) 注解 SQL
        for a in &m.annotations {
            if matches!(a.as_str(), "Select" | "Insert" | "Update" | "Delete") {
                // 注解值在方法 head 里被 annotations 收集时丢掉了参数文本，
                // 重新从方法体前的原始片段取：这里用 head 拿不到，改由 parse 阶段
                // 注解字符串一般只在 @Select("...") —— 用一个宽松策略：
                // 在方法签名之前文本中找。为简化，parse_java 后统一补扫。
                break;
            }
        }

        // 2) JdbcTemplate 拼接
        scan_jdbc(cls, m, &owner, &field_types, records);

        // 3) MP 内置 / Wrapper 消费点
        scan_method_calls(cls, m, &owner, is_mapper, &field_types, entity_tables, records, edges, base_mapper.clone());
    }
}

fn scan_jdbc(
    cls: &JavaClass,
    m: &JavaMethod,
    owner: &str,
    _field_types: &BTreeMap<String, String>,
    records: &mut Vec<SqlRecord>,
) {
    // 找 jdbcTemplate 调用：Xxx.jdbcTemplate().update("...") 或 jdbc.update("..."
    let body = &m.body;
    // 方法内局部 String 单赋值传播：`String sql = "..." + ...;` 后整变量传参
    let mut locals: BTreeMap<String, String> = BTreeMap::new();
    for seg in body.split(';') {
        if let Some(eq) = seg.find('=') {
            let lhs = seg[..eq].trim();
            let toks: Vec<&str> = lhs.split_whitespace().collect();
            if toks.len() >= 2 && toks[toks.len() - 2] == "String" {
                if let Some(name) = toks.last() {
                    if let Some(v) = fold_concat(&seg[eq + 1..], &cls.constants) {
                        locals.insert(name.to_string(), v);
                    }
                }
            }
        }
    }
    for needle in ["update(", "query(", "queryForList(", "queryForMap(", "queryForObject(", "execute(", "batchUpdate("] {
        let mut search_from = 0usize;
        while let Some(rel) = body[search_from..].find(needle) {
            let pos = search_from + rel;
            // 前缀校验：接收者含 jdbc/JdbcTemplate（宽松）
            let head = char_safe_window(body, pos, 60);
            if head.to_lowercase().contains("jdbctemplate") || head.to_lowercase().contains("jdbc") {
                // 取第一个参数（字符串表达式，顶层逗号前）
                let arg_start = pos + needle.len();
                if let Some(arg) = take_first_arg(&body[arg_start..]) {
                    // 首参是局部变量/常量名时优先查表，否则做拼接求值
                    let resolved = {
                        let name = arg.trim();
                        locals
                            .get(name)
                            .cloned()
                            .or_else(|| fold_concat(&arg, &cls.constants))
                    };
                    if let Some(sql) = resolved {
                        let sql = sql.trim().to_string();
                        let has_runtime_param = sql.contains('?');
                        if looks_like_sql(&sql) {
                            let (sk, tables, cols) = normalize_sql(&sql);
                            records.push(SqlRecord {
                                id: owner.to_string(),
                                kind: "jdbc-template".into(),
                                sql,
                                skeleton: sk,
                                tables: tables.into_iter().collect(),
                                columns: cols.into_iter().collect(),
                                file: cls.file.clone(),
                                line: m.line,
                                owner: cls.name.clone(),
                                method: m.name.clone(),
                                mp_builtin: false,
                                dynamic: has_runtime_param,
                            });
                        }
                    }
                }
            }
            search_from = pos + needle.len();
        }
    }
}

fn scan_method_calls(
    cls: &JavaClass,
    m: &JavaMethod,
    owner: &str,
    is_mapper: bool,
    field_types: &BTreeMap<String, String>,
    entity_tables: &BTreeMap<String, String>,
    records: &mut Vec<SqlRecord>,
    edges: &mut Vec<CallEdge>,
    base_mapper: Option<String>,
) {
    // 调用边：recv.method(...) -> recv 类型的 method
    let chars: Vec<char> = m.body.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '(' {
            // 回溯 recv.method
            let mut j = i;
            while j > 0 && (chars[j - 1].is_alphanumeric() || chars[j - 1] == '_') {
                j -= 1;
            }
            let method_name: String = chars[j..i].iter().collect();
            // recv：method 前是 '.' 的话取点前标识符
            let recv = if j >= 2 && chars[j - 1] == '.' {
                let mut k = j - 1;
                while k > 0 && (chars[k - 1].is_alphanumeric() || chars[k - 1] == '_') {
                    k -= 1;
                }
                Some(chars[k..j - 1].iter().collect::<String>())
            } else {
                None
            };
            if !method_name.is_empty() && !is_keyword_like(&method_name) {
                // 调用边
                if let Some(r) = &recv {
                    if let Some(t) = field_types.get(r) {
                        if !t.eq_ignore_ascii_case(&cls.name) {
                            edges.push(CallEdge {
                                caller: owner.to_string(),
                                callee: format!("{}#{}", t, method_name),
                            });
                        }
                    }
                }
                // MP 内置消费点：mapper 字段 / this（ServiceImpl 继承） / mapper 接口内
                let is_mp_consumption = recv
                    .as_deref()
                    .map(|r| {
                        r == "baseMapper" || r == "this" || field_types.get(r).map_or(false, |t| is_mapper_type(t))
                    })
                    .unwrap_or(false)
                    || (is_mapper && recv.is_none() && !m.body.contains("jdbcTemplate"));
                if is_mp_consumption {
                    if let Some((tpl, cond_only)) = mp_builtin(&method_name) {
                        if !tpl.is_empty() {
                            // 表归因：mapper 字段类型 XxxMapper -> 实体 -> 表
                            let table = infer_table_for(
                                cls,
                                recv.as_deref(),
                                field_types,
                                entity_tables,
                                base_mapper.as_deref(),
                            );
                            if let Some(table) = table {
                                let sql = tpl.replace("{table}", &table);
                                let (sk, tables, cols) = normalize_sql(&sql);
                                records.push(SqlRecord {
                                    id: owner.to_string(),
                                    kind: "mp-builtin".into(),
                                    sql,
                                    skeleton: sk,
                                    tables: tables.into_iter().collect(),
                                    columns: if cond_only {
                                        cols.into_iter().collect()
                                    } else {
                                        vec!["*".into()]
                                    },
                                    file: cls.file.clone(),
                                    line: m.line,
                                    owner: cls.name.clone(),
                                    method: m.name.clone(),
                                    mp_builtin: true,
                                    dynamic: false,
                                });
                            }
                        }
                    }
                }
            }
            i += 1;
            continue;
        }
        i += 1;
    }
}

/// 取 end 前 width 字节的窗口，起始点回退到字符边界
/// （直接 `body[pos-60..pos]` 在窗口内有多字节字符（中文注释）时 panic）
fn char_safe_window(body: &str, end: usize, width: usize) -> &str {
    let mut s = end.saturating_sub(width);
    while !body.is_char_boundary(s) {
        s += 1;
    }
    &body[s..end]
}

fn is_mapper_type(t: &str) -> bool {
    t.ends_with("Mapper") || t.ends_with("Dao")
}

fn infer_table_for(
    _cls: &JavaClass,
    recv: Option<&str>,
    field_types: &BTreeMap<String, String>,
    entity_tables: &BTreeMap<String, String>,
    base_mapper: Option<&str>,
) -> Option<String> {
    // this 调用：ServiceImpl<M, T> 泛型基类绑定 -> mapper 类型
    let mapper_owned;
    let mapper: &str = if recv == Some("this") {
        mapper_owned = base_mapper?;
        &mapper_owned
    } else {
        recv.and_then(|r| field_types.get(r))
            .filter(|t| is_mapper_type(t))?
    };
    // mapper 类型 XxxMapper -> 去 Mapper 后缀 -> 实体名 -> 表
    let ent = mapper.trim_end_matches("Mapper").trim_end_matches("Dao");
    let table = entity_tables.get(ent)?;
    Some(table.clone())
}

pub fn camel_to_snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn class_route_prefix(_cls: &JavaClass) -> String {
    // v1：注解上的路径留给 method_route 处理，类级前缀取 @RequestMapping("...") 的字面量
    String::new()
}

fn method_route(_m: &JavaMethod) -> Option<(String, String)> {
    // 路由提取依赖注解参数文本；parse_members 收集的是注解名，
    // v1 由 scan_annotation_sql 补扫阶段处理（见 scan_routes_in_source）。
    None
}

fn join_path(a: &str, b: &str) -> String {
    if a.is_empty() {
        return b.to_string();
    }
    format!("{}/{}", a.trim_end_matches('/'), b.trim_start_matches('/'))
}

/// 取第一个顶层逗号前的参数文本（括号平衡）。
pub fn take_first_arg(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut depth = 0i32;
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            ',' if depth == 0 => break,
            _ => {}
        }
        out.push(c);
        i += 1;
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

pub fn looks_like_sql(s: &str) -> bool {
    let low = s.to_lowercase();
    ["select ", "insert ", "update ", "delete "]
        .iter()
        .any(|k| low.contains(k))
}

/// 遍历项目源码目录，返回 (相对路径, 内容)。
pub fn collect_java_files(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let p = entry.path();
        if p.extension().map_or(true, |e| e != "java") {
            continue;
        }
        let rel = p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/");
        if let Ok(content) = std::fs::read_to_string(p) {
            out.push((rel, content));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_safe_window_with_chinese() {
        // 回归：60 字节窗口起点落在中文多字节字符中间会 panic
        // （browser_java_new / ems 等真实项目复现：inside '录' / '名'）
        let body = "abc失败的查询记录平台名jdbcTemplate.queryForList(\n";
        let end = body.find("queryForList(").unwrap();
        let win = char_safe_window(body, end, 60);
        assert!(win.contains("jdbcTemplate"));
        // 窗口足够短时也应安全
        assert!(!char_safe_window(body, 2, 60).is_empty());
        assert_eq!(char_safe_window(body, 0, 60), "");
    }

    #[test]
    fn chinese_comment_before_method_does_not_panic() {
        // 回归：head_start 是字符索引，直接切 &str 会在中文多字节字符中间 panic
        // （真实项目 LT47/MotongNetworkService 复现：end byte index ... inside '失'）
        let src = "package com.demo;\n\
                   // 失败的订单统计逻辑：失败原因分析\n\
                   @Service\n\
                   public class StatsService {\n\
                       // 余额榜：裸 SELECT，触碰 users.wallet_balance。\n\
                       public List<Object> topWallets() {\n\
                           return jdbcTemplate.queryForList(sql);\n\
                       }\n\
                   }\n";
        let cls = parse_java("StatsService.java", src).unwrap();
        assert_eq!(cls.name, "StatsService");
        assert_eq!(cls.methods.len(), 1, "methods: {:?}", cls.methods.len());
        assert_eq!(cls.methods[0].name, "topWallets");
    }

    #[test]
    fn parses_fields_and_methods() {
        let src = r#"
package com.demo.service.impl;

@Service
public class ProductServiceImpl extends ServiceImpl<ProductMapper, Product> implements ProductService {

    // 中文注释：验证多字节字符不错位
    @Autowired
    private ProductMapper productMapper;

    @Override
    public List<Product> list() {
        return productMapper.selectAll();
    }

    public Product getByIdOrThrow(Long id) {
        Product product = this.getById(id);
        return product;
    }
}
"#;
        let cls = parse_java("x.java", src).unwrap();
        assert_eq!(cls.name, "ProductServiceImpl");
        assert_eq!(cls.extends[0], "ServiceImpl");
        assert_eq!(cls.type_args[0], "ProductMapper");
        assert_eq!(
            cls.fields.get("productMapper").map(|s| s.as_str()),
            Some("ProductMapper"),
            "fields: {:?}",
            cls.fields
        );
        let names: Vec<&str> = cls.methods.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"list"), "methods: {:?}", names);
        assert!(names.contains(&"getByIdOrThrow"), "methods: {:?}", names);
    }
}
