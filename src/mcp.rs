//! MCP (Model Context Protocol) stdio server：把 scan/find 暴露为 MCP tool。
//!
//! 协议：stdin/stdout 上的换行分隔 JSON-RPC 2.0（MCP stdio 传输）。
//! 只需支持 initialize / notifications/initialized / tools/list / tools/call
//! 四个方法面，手写协议层即可，不引入 SDK 依赖，保住单文件二进制形态。
//!
//! 客户端配置：`{"command": "contextgate", "args": ["mcp"]}`

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;

/// 两个 tool 的定义（含 JSON Schema，供客户端渲染参数表单）。
fn tool_defs() -> Value {
    json!([
    {
        "name": "contextgate_scan",
        "description": "扫描 Spring Boot + MyBatis 项目，构建 SQL↔源码索引（.contextgate/index.json）。调用 contextgate_find 前需先执行；代码变更后重新执行。秒级完成，不编译项目、不连数据库。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "project_root": { "type": "string", "description": "项目根目录绝对路径" }
            },
            "required": ["project_root"]
        }
    },
    {
        "name": "contextgate_find",
        "description": "用实际执行的 SQL 反查源码位置：file:line、Class#method、直接调用者、上游 HTTP 路由。sql 原样粘贴（含参数值、别名），不要剥限定符或重排列——匹配引擎自己折叠参数，越原样命中越准。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "项目根目录或 index.json 路径" },
                "sql": { "type": "string", "description": "实际执行的 SQL（原样）" }
            },
            "required": ["target", "sql"]
        }
    }
    ])
}

/// MCP stdio 主循环。
pub fn run() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // 非法行忽略，保持服务存活
        };
        // 通知（无 id）按协议不回包
        let id = match msg.get("id") {
            Some(v) => v.clone(),
            None => continue,
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": msg.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2024-11-05")),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "contextgate", "version": env!("CARGO_PKG_VERSION") }
            })),
            "tools/list" => Ok(json!({ "tools": tool_defs() })),
            "tools/call" => handle_tool_call(&msg),
            "ping" => Ok(json!({})),
            _ => Err(format!("method not found: {method}")),
        };
        let resp = match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": e }
            }),
        };
        let _ = writeln!(stdout, "{resp}");
        let _ = stdout.flush();
    }
}

/// tools/call：分发到 scan / find。tool 执行失败按协议以 isError 结果返回
/// （而非 JSON-RPC error），客户端才能把错误信息展示给模型。
fn handle_tool_call(msg: &Value) -> Result<Value, String> {
    let name = msg
        .pointer("/params/name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
    let call = |r: Result<String, String>| match r {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
    };
    match name.as_str() {
        "contextgate_scan" => {
            let root = str_arg(&args, "project_root")
                .ok_or("缺少参数 project_root（项目根目录）")?;
            let root = PathBuf::from(root);
            let out = args
                .get("out")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(".contextgate").join("index.json"));
            Ok(call(crate::do_scan(&root, &out)))
        }
        "contextgate_find" => {
            let target = str_arg(&args, "target").ok_or("缺少参数 target（项目根或 index.json 路径）")?;
            let sql = str_arg(&args, "sql").ok_or("缺少参数 sql（实际执行的 SQL）")?;
            let target = PathBuf::from(target);
            let index_path = crate::resolve_index_path(&target);
            Ok(call(crate::do_find(&index_path, sql)))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}
