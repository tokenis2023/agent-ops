//! MCP 服务：stdio 上的 JSON-RPC 2.0（换行分隔），手写实现。
//!
//! 不引 async 运行时：串行模式一进一出逐行处理，冷启动 ~3ms。
//! `--jobs N` 时用固定线程池并发处理 tools/call，响应按 id 匹配、允许乱序；
//! 控制类方法（initialize/ping/tools/list）仍在读线程内就地应答。

use crate::tools;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

/// 默认并发度：同一回复里的多个只读调用可以真正并行。
pub const DEFAULT_JOBS: usize = 4;

/// 宿主断开信号：MCP 层置位后，正在跑的前台 fs_exec 结束进程树并返回 E_ABORTED。
/// 用全局量是因为 `tools::call` 的签名被 schema 生成和 CLI 共用，不宜为一个信号加参数。
static CANCEL: AtomicBool = AtomicBool::new(false);

/// 供 fs_exec 查询的取消信号。
pub fn cancel_flag() -> &'static AtomicBool {
    &CANCEL
}

/// 宿主是否还连着。
///
/// 只靠「写响应失败」发现断开是不够的：一次长命令跑着的时候没有任何响应可写，
/// 宿主走了我们也发现不了，进程和子进程照样活到 timeout（实测：宿主关掉输出端
/// 40 秒后，fsops 与 ping 都还活着）。
///
/// 判据是「管道还能不能写」：对端关掉后写入必然失败，而 PeekNamedPipe 在
/// **普通文件** stdout 上本来就报 ERROR_ACCESS_DENIED(5)，分不清「不是管道」和
/// 「断了」，会误杀；另外对端关闭后 GetLastError 仍可能是 0，所以只看返回值、
/// 不匹配错误码。
///
/// 探针是零长度写：对健康管道恒成功且不产生任何字节，对协议透明。
#[cfg(windows)]
fn host_alive() -> bool {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> *mut c_void;
        fn GetFileType(handle: *mut c_void) -> u32;
        fn WriteFile(
            handle: *mut c_void,
            buffer: *const u8,
            len: u32,
            written: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const FILE_TYPE_PIPE: u32 = 3;
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if handle.is_null() {
        return true;
    }
    // 非管道（重定向到文件/控制台）时没有对端可判断，交给写响应失败兜底。
    if unsafe { GetFileType(handle) } != FILE_TYPE_PIPE {
        return true;
    }
    let mut written = 0u32;
    unsafe {
        WriteFile(
            handle,
            std::ptr::null(),
            0,
            &mut written,
            std::ptr::null_mut(),
        ) != 0
    }
}

/// 非 Windows：项目主要面向 Windows，这里保守认为宿主还在（写失败兜底）。
#[cfg(not(windows))]
fn host_alive() -> bool {
    true
}

/// 宿主断开检测线程：前台命令在跑时没有响应可写，只能主动探测。
/// 250ms 一次：宿主断开后最多这么久就收工，对命令本身的 CPU 影响可忽略。
fn watch_host() {
    thread::spawn(|| loop {
        thread::sleep(std::time::Duration::from_millis(250));
        if !host_alive() {
            CANCEL.store(true, Ordering::Relaxed);
            return;
        }
    });
}

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
    // 写响应时宿主可能已经断开（管道关闭 / 管道变空）。原先忽略写错误，
    // 进程会一直读到 stdin EOF 才退；而宿主侧的超时/中断不一定关 stdin，
    // 于是留下一批活着但没人读的 fsops 进程。写失败即视为对端已走，广播关闭。
    //
    // 触发条件只有「写响应失败」：那是宿主真的走了（管道关/变空）的确证。
    // stdin EOF 不能当信号——一次性 `fsops call` 与 e2e 客户端都是写完就关 stdin，
    // 此时排队请求仍须执行完，否则会把正常结果误判成 E_ABORTED。
    let broken = Arc::new(AtomicBool::new(false));
    watch_host();
    let mut workers = Vec::with_capacity(jobs);
    for _ in 0..jobs {
        let rx = Arc::clone(&rx);
        let out = Arc::clone(&out);
        let broken = Arc::clone(&broken);
        workers.push(thread::spawn(move || loop {
            // 只在取任务时持有接收端锁；执行期间不持有，真正并行。
            let job = {
                let guard = rx.lock().unwrap();
                guard.recv()
            };
            let Ok(req) = job else { break };
            if broken.load(Ordering::Relaxed) {
                break;
            }
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
            if !write_line(&out, &resp) {
                broken.store(true, Ordering::Relaxed);
                // 宿主已走：让正在跑的前台命令立刻收工，不必干等到自己的 timeout。
                CANCEL.store(true, Ordering::Relaxed);
                break;
            }
        }));
    }
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        if broken.load(Ordering::Relaxed) {
            break;
        }
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
                if !write_line(&out, &resp) {
                    broken.store(true, Ordering::Relaxed);
                    break;
                }
                continue;
            }
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        if method == "tools/call" {
            let _ = tx.send(req);
        } else if let Some(resp) = dispatch(&req) {
            if !write_line(&out, &resp) {
                broken.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
    // stdin EOF 本身不算宿主已走：一次性 `fsops call` 与 e2e 客户端都是写完就关，
    // 管道里排队的请求仍然要执行完。真断开由上方的写失败分支处理。
    drop(tx);
    for worker in workers {
        let _ = worker.join();
    }
    crate::jobs::shutdown();
    Ok(())
}

/// 写一行 JSON-RPC 响应。返回 false 表示对端已断开（管道关闭或变空）。
fn write_line(out: &Arc<Mutex<io::Stdout>>, resp: &Value) -> bool {
    let mut guard = out.lock().unwrap();
    if writeln!(&mut *guard, "{resp}").is_err() {
        return false;
    }
    guard.flush().is_ok()
}

fn err_resp(id: Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

/// initialize 返回的系统级引导。
///
/// 只负责「该用 fsops、什么时候回退、哪些是跨工具策略」三件事；
/// 参数级事实（参数名、互斥、默认预算、统计口径）写在 `tools.rs` 的工具描述里，
/// 宿主忽略本字段时也能靠 tools/list 拼出合法调用。
/// 与 `adopt::RULE_BODY` 分工相同：本字段是短路由，RULE_BODY 是项目规则文件。
pub fn instructions() -> &'static str {
    "文件读写、搜索、编辑、元信息操作优先使用 fs_* 工具。只要 fsops 可用，不要用宿主内置 read_file/write_file/search_files 或 shell 的 cat/head/tail/grep/find/sed 处理文本；仅当 fsops 不可用、超时，或目标是图片/不支持的二进制时回退。DOCX/PDF 正文走 fs_read。局部修改用 fs_edit，不要整文件读回再 fs_write；不知道确切原文时用 pattern/replacement，改完用 diff=true 自查。同一次回复里可以并行发起多个只读调用；fs_read 的四种常用模式、fs_glob 的 repo map、fs_grep 的统计口径都写在各自描述里，发调用前先看。跑 cargo/git/npm 用 fs_exec 的 argv 数组，文件复制/移动/删除/建目录用 fs_file，不要用 PowerShell 拼命令行。前台 fs_exec 保持 2 分钟内（宿主对单次工具调用有更短超时）；更长的构建/测试用 background=true 拿 job id，再用 {\"job\":N,\"wait_ms\":60000} 一次等到退出，别反复短轮询。"
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
        // 参数级事实（reads/focus/map）不再抄进引导，但必须指向工具描述，
        // 否则模型既看不到又不能自己想到——实测这类能力只在参数里时不会被主动使用。
        for pointer in [
            "fs_read 的四种常用模式",
            "repo map",
            "统计口径",
            "发调用前先看",
        ] {
            assert!(text.contains(pointer), "引导缺少描述指针 {pointer}: {text}");
        }
        assert!(
            text.contains("fs_exec") && text.contains("fs_file"),
            "{text}"
        );
    }

    #[test]
    fn 引导不复制工具描述里的参数枚举() {
        // P2：引导与工具描述 47% 字符重复时，两处都要每轮付费。
        // 这里钉住“参数名只住在一处”：引导不再列 reads=/focus=/map=true，
        // 但对应事实必须能在工具描述里找到（由 tools::tests 验证）。
        let text = instructions();
        for duplicated in ["reads=[", "focus=", "map=true", "block=true"] {
            assert!(
                !text.contains(duplicated),
                "引导又抄回了参数枚举 {duplicated}: {text}"
            );
        }
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
