//! MCP 服务：stdio 上的 JSON-RPC 2.0（换行分隔），手写实现。
//!
//! 不引 async 运行时：一进一出逐行处理，冷启动 ~3ms，
//! 对"每次工具调用都起一次进程"的场景，这比 Node 实现快一个数量级。

use crate::tools;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

pub fn serve() -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let resp = err_resp(Value::Null, -32700, &format!("JSON 解析失败: {e}"));
                writeln!(stdout, "{resp}")?;
                stdout.flush()?;
                continue;
            }
        };
        // 通知（无 id）不回复
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        if id.is_none() {
            continue;
        }
        let id = id.unwrap();

        let result = match method {
            "initialize" => {
                let requested = req
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2025-06-18");
                Ok(json!({
                    "protocolVersion": requested,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "fsops", "version": crate::VERSION},
                    "instructions": "Windows 上按 Linux 习惯读写文件：支持 /c/... 与 ~ 路径、GBK/UTF-16/CRLF 自动识别；返回体有字节预算，读长文件请按提示用 start 续读。"
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools::tool_defs()})),
            "tools/call" => {
                let name = req
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = req
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let text = tools::call(name, &args);
                let is_error = text.starts_with("E_");
                Ok(json!({
                    "content": [{"type": "text", "text": text}],
                    "isError": is_error
                }))
            }
            other => Err((-32601, format!("未知方法 {other}"))),
        };

        let resp = match result {
            Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
            Err((code, msg)) => err_resp(id, code, &msg),
        };
        writeln!(stdout, "{resp}")?;
        stdout.flush()?;
    }
    Ok(())
}

fn err_resp(id: Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}
