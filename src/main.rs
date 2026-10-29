//! ContextGate (Rust)：SQL ↔ 源码 静态索引。
//!
//! 用法：
//!   contextgate scan <项目根> [-o index.json]   扫描项目，生成索引（默认 <项目根>/.contextgate/index.json）
//!   contextgate find <index.json> "<SQL>"      用实际执行的 SQL 反查源码位置
//!   contextgate find <项目根> "<SQL>"          便捷形式：自动加载 <项目根>/.contextgate/index.json
//!   contextgate mcp                            MCP stdio server（scan/find 暴露为 tool）

mod matcher;
mod mcp;
mod model;
mod scan;
mod sqlfeat;
mod store;

use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        std::process::exit(1);
    }
    match args[0].as_str() {
        "scan" => cmd_scan(&args[1..]),
        "find" => cmd_find(&args[1..]),
        "mcp" => mcp::run(),
        "--help" | "-h" | "help" => print_usage(),
        other => {
            eprintln!("未知子命令: {other}");
            print_usage();
            std::process::exit(1);
        }
    }
}

fn print_usage() {
    println!(
        "ContextGate (Rust) — SQL ↔ 源码 静态索引\n\n\
用法:\n  contextgate scan <项目根> [-o index.json]\n  \
contextgate find <index.json 或 项目根> \"<SQL>\"\n  \
contextgate mcp            （MCP stdio server）\n\n\
示例:\n  \
contextgate scan examples/demo-project\n  \
contextgate find examples/demo-project \"UPDATE users SET wallet_balance = 100.5 WHERE id = 42\""
    );
}

fn default_index_path(root: &Path) -> PathBuf {
    root.join(".contextgate").join("index.json")
}

/// 传 index.json 或 项目根都行，归一成 index.json 路径。
fn resolve_index_path(target: &Path) -> PathBuf {
    if target.is_file() {
        target.to_path_buf()
    } else {
        default_index_path(target)
    }
}

/// 执行扫描并保存，返回人读摘要。CLI 与 MCP 共用。
fn do_scan(root: &Path, out: &Path) -> Result<String, String> {
    if !root.is_dir() {
        return Err(format!("项目根不是目录: {}", root.display()));
    }
    let t0 = std::time::Instant::now();
    let index = scan::scan_project(root);
    store::save(&index, out).map_err(|e| format!("保存索引失败: {e}"))?;
    Ok(format!(
        "扫描完成: {} 条 SQL 记录 / {} 条调用边 / {} 条路由 / {} 个实体 -> {}（{}ms）",
        index.records.len(),
        index.call_graph.len(),
        index.routes.len(),
        index.entities.len(),
        out.display(),
        t0.elapsed().as_millis()
    ))
}

/// 加载索引并反查，返回渲染文本。CLI 与 MCP 共用。
fn do_find(index_path: &Path, query: &str) -> Result<String, String> {
    if !index_path.is_file() {
        return Err(format!(
            "索引不存在: {}（先对项目根运行 scan）",
            index_path.display()
        ));
    }
    let index = store::load(index_path).map_err(|e| format!("加载索引失败: {e}"))?;
    Ok(matcher::render_hits(
        &index,
        query,
        &matcher::find_sql(&index, query, 8),
    ))
}

fn cmd_scan(args: &[String]) {
    if args.is_empty() {
        eprintln!("用法: contextgate scan <项目根> [-o index.json]");
        std::process::exit(1);
    }
    let root = PathBuf::from(&args[0]);
    let out = match args.iter().position(|a| a == "-o") {
        Some(i) if i + 1 < args.len() => PathBuf::from(&args[i + 1]),
        _ => default_index_path(&root),
    };
    match do_scan(&root, &out) {
        Ok(summary) => println!("{summary}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

fn cmd_find(args: &[String]) {
    if args.len() < 2 {
        eprintln!("用法: contextgate find <index.json 或 项目根> \"<SQL>\"");
        std::process::exit(1);
    }
    let index_path = resolve_index_path(Path::new(&args[0]));
    let query = args[1..].join(" ");
    match do_find(&index_path, &query) {
        Ok(text) => println!("{text}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
