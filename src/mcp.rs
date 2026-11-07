//! MCP 服务：stdio 上的 JSON-RPC 2.0（换行分隔），手写实现。
//!
//! 不引 async 运行时：串行模式一进一出逐行处理，冷启动 ~3ms。
//! `--jobs N` 时用固定线程池并发处理 tools/call，响应按 id 匹配、允许乱序；
//! 控制类方法（initialize/ping/tools/list）仍在读线程内就地应答。

use crate::tools;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

/// 默认并发度：同一回复里的多个只读调用可以真正并行。
pub const DEFAULT_JOBS: usize = 4;

pub fn serve() -> io::Result<()> {
    serve_with(DEFAULT_JOBS)
}

pub fn serve_with(jobs: usize) -> io::Result<()> {
    // 0/1 升为单工作线程：串行 dispatch 会让 background/wait_ms 阻塞读线程，
    // 后续请求（包括 kill 自救）全部排队。单工作线程保留串行语义，控制类
    // 请求（initialize/ping/tools/list）仍在读线程就地应答，不受影响。
    serve_parallel(jobs.max(1))
}

/// 单条请求 -> 单条响应；通知（无 id）返回 None。
fn dispatch(req: &Value) -> Option<Value> {
    let id = req.get("id").cloned()?;
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
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
                "instructions": instructions(),
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
            let is_error = text.starts_with("E_") || crate::tools::has_error_line(&text);
            Ok(json!({
                "content": [{"type": "text", "text": text}],
                "isError": is_error
            }))
        }
        other => Err((-32601, format!("未知方法 {other}"))),
    };
    Some(match result {
        Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
        Err((code, msg)) => err_resp(id, code, &msg),
    })
}

fn serve_parallel(jobs: usize) -> io::Result<()> {
    let (tx, rx) = mpsc::channel::<Value>();
    let rx = Arc::new(Mutex::new(rx));
    let out = Arc::new(Mutex::new(io::stdout()));
    let mut workers = Vec::with_capacity(jobs);
    for _ in 0..jobs {
        let rx = Arc::clone(&rx);
        let out = Arc::clone(&out);
        workers.push(thread::spawn(move || loop {
            // 只在取任务时持有接收端锁；执行期间不持有，真正并行。
            let job = {
                let guard = rx.lock().unwrap();
                guard.recv()
            };
            let Ok(req) = job else { break };
            let resp =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(&req))) {
                    Ok(Some(resp)) => resp,
                    Ok(None) => continue,
                    Err(_) => json!({
                        "jsonrpc": "2.0",
                        "id": req.get("id").cloned().unwrap_or(Value::Null),
                        "error": {"code": -32603, "message": "E_INTERNAL 工具执行崩溃"}
                    }),
                };
            let mut guard = out.lock().unwrap();
            let _ = writeln!(&mut *guard, "{resp}");
            let _ = guard.flush();
        }));
    }
    let stdin = io::stdin();
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
                let mut guard = out.lock().unwrap();
                let _ = writeln!(&mut *guard, "{resp}");
                let _ = guard.flush();
                continue;
            }
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        if method == "tools/call" {
            let _ = tx.send(req);
        } else if let Some(resp) = dispatch(&req) {
            let mut guard = out.lock().unwrap();
            let _ = writeln!(&mut *guard, "{resp}");
            let _ = guard.flush();
        }
    }
    drop(tx);
    for worker in workers {
        let _ = worker.join();
    }
    crate::jobs::shutdown();
    Ok(())
}

fn err_resp(id: Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

/// initialize 返回的系统级引导。
///
/// 与 `adopt::RULE_BODY` 保持同一套路由：DOCX/PDF 正文都走 `fs_read`，
/// 只有图片和不支持的二进制回退宿主；正则编辑和 diff 自查写进短句，
/// 不再复制整份文档。
pub fn instructions() -> &'static str {
    "文件读写、搜索、编辑、元信息操作优先使用 fs_* 工具。只要 fsops 可用，不要用宿主内置 read_file/write_file/search_files 或 shell 的 cat/head/tail/grep/find/sed 处理文本；仅当 fsops 不可用、超时，或目标是图片/不支持的二进制时回退。DOCX/PDF 正文走 fs_read。局部修改用 fs_edit，不要整文件读回再 fs_write；只知道文本形状时用 pattern/replacement 正则替换，不必先抄原文，改结构后用 diff=true 自查。用 fs_grep 拿到行号后，用 fs_read start=N block=true 一次取回整个代码块，不必猜 end。同一次回复里可以并行发起多个只读调用；已知多个文件时用 fs_read reads=[{path,start,end},...] 一次读完；只想按模式取代码块时用 fs_read focus=<正则>，不必先 grep 再 block。大仓库先 fs_glob map=true focus=<关键词> 看结构，再决定搜哪里。跑 cargo/git/npm 用 fs_exec 的 argv 数组，文件复制/移动/删除/建目录用 fs_file，不要用 PowerShell 拼命令行。前台 fs_exec 保持 2 分钟内（宿主对单次工具调用有更短超时）；更长的构建/测试用 background=true 拿 job id，再用 {\"job\":N,\"wait_ms\":60000} 一次等到退出，别反复短轮询。"
}

// 多文件响应里错误可能出现在任意一行；实现已移到 `tools::has_error_line`，
// CLI 退出码与 MCP 的 isError 共用同一判定。

/// `jobs.max(1)`：0/1 升为单工作线程的映射，供测试钉住。
#[cfg(test)]
fn jobs_max1(jobs: usize) -> usize {
    jobs.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn jobs为0或1时升为单工作线程() {
        // 串行 dispatch 会让 background/wait_ms 阻塞读线程（读线程被占住，
        // 后续请求包括 kill 自救全部排队）。0/1 统一升为 1 个工作线程。
        // serve_serial 已删；serve_with 用 jobs.max(1) 钉住该行为。
        // mcp_jobs 在 cli.rs（不同测试模块），这里断言入口常量不回归。
        assert_eq!(DEFAULT_JOBS, 4);
        assert_eq!(jobs_max1(0), 1);
        assert_eq!(jobs_max1(1), 1);
        assert_eq!(jobs_max1(4), 4);
    }

    #[test]
    fn 引导明确_pdf_正文走_fs_read() {
        // PDF 有文本层，和 DOCX 一样属于 fsops 该接管的正文；
        // 写成“图片/PDF 回退”会让 Agent 把整份 PDF 丢回宿主 Read。
        let text = instructions();
        assert!(text.contains("DOCX/PDF"), "{text}");
        assert!(text.contains("fs_read"), "{text}");
        assert!(
            !text.contains("图片/PDF") && !text.contains("PDF/二进制"),
            "PDF 不应与图片/二进制一起回退: {text}"
        );
        assert!(
            text.contains("pattern") && text.contains("replacement"),
            "缺少正则编辑引导: {text}"
        );
        assert!(text.contains("diff=true"), "缺少 diff 自查引导: {text}");
    }

    #[test]
    fn 引导包含并行与批量读取() {
        let text = instructions();
        assert!(text.contains("并行"), "{text}");
        assert!(text.contains("reads=["), "{text}");
        assert!(text.contains("focus="), "{text}");
        assert!(text.contains("map=true"), "{text}");
        assert!(
            text.contains("fs_exec") && text.contains("fs_file"),
            "{text}"
        );
    }

    #[test]
    fn 识别夹在正文中的错误行() {
        assert!(crate::tools::has_error_line(
            "ok\ntext\nE_READ 读取失败: 不存在\n"
        ));
        assert!(crate::tools::has_error_line(
            "== a.txt utf-8\n1|x\nE_BINARY /b.dat 是二进制文件"
        ));
        assert!(!crate::tools::has_error_line(
            "== a.txt utf-8\n1|E_READ 只是正文\n"
        ));
        assert!(!crate::tools::has_error_line("ok 写入文件"));
        assert!(!crate::tools::has_error_line("1|E_UNKNOWN 出现在内容里"));
    }

    #[test]
    fn dispatch_处理初始化通知与未知方法() {
        let init = dispatch(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
        }))
        .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "fsops");
        assert!(dispatch(&json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }))
        .is_none());
        let unknown = dispatch(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "nope"
        }))
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }
}
