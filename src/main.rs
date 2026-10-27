//! ContextGate (Rust)：SQL ↔ 源码 静态索引。
//!
//! 用法：
//!   contextgate scan <项目根> [-o index.json]   扫描项目，生成索引（默认 <项目根>/.contextgate/index.json）
//!   contextgate find <index.json> "<SQL>"      用实际执行的 SQL 反查源码位置
//!   contextgate find <项目根> "<SQL>"          便捷形式：自动加载 <项目根>/.contextgate/index.json

mod matcher;
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
contextgate find <index.json 或 项目根> \"<SQL>\"\n\n\
示例:\n  \
contextgate scan examples/demo-project\n  \
contextgate find examples/demo-project \"UPDATE users SET wallet_balance = 100.5 WHERE id = 42\""
    );
}

fn default_index_path(root: &Path) -> PathBuf {
    root.join(".contextgate").join("index.json")
}

fn cmd_scan(args: &[String]) {
    if args.is_empty() {
        eprintln!("用法: contextgate scan <项目根> [-o index.json]");
        std::process::exit(1);
    }
    let root = PathBuf::from(&args[0]);
    if !root.is_dir() {
        eprintln!("项目根不是目录: {}", root.display());
        std::process::exit(1);
    }
    let out = match args.iter().position(|a| a == "-o") {
        Some(i) if i + 1 < args.len() => PathBuf::from(&args[i + 1]),
        _ => default_index_path(&root),
    };

    let t0 = std::time::Instant::now();
    let index = scan::scan_project(&root);
    store::save(&index, &out).unwrap_or_else(|e| {
        eprintln!("保存索引失败: {e}");
        std::process::exit(1);
    });
    println!(
        "扫描完成: {} 条 SQL 记录 / {} 条调用边 / {} 条路由 / {} 个实体 -> {}（{}ms）",
        index.records.len(),
        index.call_graph.len(),
        index.routes.len(),
        index.entities.len(),
        out.display(),
        t0.elapsed().as_millis()
    );
}

fn cmd_find(args: &[String]) {
    if args.len() < 2 {
        eprintln!("用法: contextgate find <index.json 或 项目根> \"<SQL>\"");
        std::process::exit(1);
    }
    let target = PathBuf::from(&args[0]);
    let query = args[1..].join(" ");
    // 传 index.json 或 项目根都行
    let index_path = if target.is_file() {
        target.clone()
    } else {
        default_index_path(&target)
    };
    if !index_path.is_file() {
        eprintln!(
            "索引不存在: {}\n先运行: contextgate scan {}",
            index_path.display(),
            target.display()
        );
        std::process::exit(1);
    }
    let index = store::load(&index_path).unwrap_or_else(|e| {
        eprintln!("加载索引失败: {e}");
        std::process::exit(1);
    });
    let hits = matcher::find_sql(&index, &query, 8);
    println!("{}", matcher::render_hits(&index, &query, &hits));
}
