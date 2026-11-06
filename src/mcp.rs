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
                    "instructions": "文件读写、搜索、编辑、元信息操作优先使用 fs_* 工具。只要 fsops 可用，不要用宿主内置 read_file/write_file/search_files 或 shell 的 cat/head/tail/grep/find/sed 处理文本；仅当 fsops 不可用、超时，或目标是图片/PDF/二进制时回退。局部修改用 fs_edit，不要整文件读回再 fs_write。",
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
                // 多文件读取会把错误夹在正文之间，只看首字符会漏报。
                // 只要任一行以错误码开头，就标记 isError，避免 Agent 把失败当成功。
                let is_error = text.starts_with("E_") || has_error_line(&text);
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

/// 多文件响应里错误可能出现在任意一行；识别 `E_XXX ` 形式的错误码行。
fn has_error_line(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("E_") else {
            return false;
        };
        // 错误码形如 E_READ、E_BINARY、E_TIMEOUT：全大写字母/下划线后接空格或结尾。
        let code_len = rest
            .find(|c: char| !(c.is_ascii_uppercase() || c == '_'))
            .unwrap_or(rest.len());
        code_len > 0 && rest[code_len..].starts_with([' ', ':'])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 识别夹在正文中的错误行() {
        assert!(has_error_line("ok\ntext\nE_READ 读取失败: 不存在\n"));
        assert!(has_error_line(
            "== a.txt utf-8\n1|x\nE_BINARY /b.dat 是二进制文件"
        ));
        assert!(!has_error_line("== a.txt utf-8\n1|E_READ 只是正文\n"));
        assert!(!has_error_line("ok 写入文件"));
        assert!(!has_error_line("1|E_UNKNOWN 出现在内容里"));
    }
}
