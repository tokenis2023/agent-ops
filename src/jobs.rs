//! fs_exec 后台任务：启动后立即返回 job id，轮询拿增量输出。
//!
//! 只在长驻的 MCP 服务进程内有效；`fsops call` 每次都是新进程，无法跨调用保存任务。

use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const MAX_JOBS: usize = 16;
const MAX_BUFFER: usize = 64 * 1024;
const MAX_KEPT_JOBS: usize = 64;
const MAX_TIMEOUT_MS: usize = 3_600_000;

struct Job {
    child: Child,
    pid: u32,
    started: Instant,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    cursor_out: usize,
    cursor_err: usize,
    dropped_out: usize,
    dropped_err: usize,
    killed: bool,
    timed_out: bool,
}

fn jobs() -> &'static Mutex<HashMap<u32, Job>> {
    static JOBS: OnceLock<Mutex<HashMap<u32, Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn disabled() -> Option<String> {
    DISABLED
        .load(Ordering::Relaxed)
        .then(|| "E_NO_JOB 后台任务只在 MCP 会话内可用；fsops call 每次都是新进程".to_string())
}

/// `fsops call` 是一次性进程，后台任务无法跨调用存活。
pub fn disable() {
    DISABLED.store(true, Ordering::Relaxed);
}

static DISABLED: AtomicBool = AtomicBool::new(false);

fn next_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub fn start(args: &Value) -> String {
    if let Some(e) = disabled() {
        return e;
    }
    let (mut command, program) = match crate::exec::build_command(args) {
        Ok(pair) => pair,
        Err(e) => return e,
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return format!("E_NOT_FOUND program={program}：{e}；检查 PATH 或写绝对路径"),
    };
    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout 已管道化");
    let stderr = child.stderr.take().expect("stderr 已管道化");
    let out_buf = Arc::new(Mutex::new(Vec::new()));
    let err_buf = Arc::new(Mutex::new(Vec::new()));
    let out_dropped = Arc::new(AtomicUsize::new(0));
    let err_dropped = Arc::new(AtomicUsize::new(0));
    pump(stdout, Arc::clone(&out_buf), Arc::clone(&out_dropped));
    pump(stderr, Arc::clone(&err_buf), Arc::clone(&err_dropped));

    let id = next_id();
    {
        let Ok(mut map) = jobs().lock() else {
            crate::exec::kill_tree(pid);
            return "E_JOB 任务表不可用".into();
        };
        let mut running = 0usize;
        for job in map.values_mut() {
            if !matches!(job.child.try_wait(), Ok(Some(_))) {
                running += 1;
            }
        }
        if running >= MAX_JOBS {
            crate::exec::kill_tree(pid);
            return format!("E_JOB_LIMIT 后台任务已达上限 {MAX_JOBS} 个；先 kill 不需要的任务");
        }
        if map.len() >= MAX_KEPT_JOBS {
            let mut finished: Vec<(u32, Instant)> = map
                .iter_mut()
                .filter_map(|(key, job)| {
                    matches!(job.child.try_wait(), Ok(Some(_))).then(|| (*key, job.started))
                })
                .collect();
            finished.sort_by_key(|(_, started)| *started);
            for (key, _) in finished.into_iter().take(map.len() - MAX_KEPT_JOBS + 1) {
                map.remove(&key);
            }
        }
        map.insert(
            id,
            Job {
                child,
                pid,
                started: Instant::now(),
                stdout: out_buf,
                stderr: err_buf,
                cursor_out: 0,
                cursor_err: 0,
                dropped_out: 0,
                dropped_err: 0,
                killed: false,
                timed_out: false,
            },
        );
    }
    let timeout_ms = crate::tools::arg_usize(args, "timeout_ms", 0).min(MAX_TIMEOUT_MS);
    if timeout_ms > 0 {
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(timeout_ms as u64));
            if let Ok(mut map) = jobs().lock() {
                if let Some(job) = map.get_mut(&id) {
                    if matches!(job.child.try_wait(), Ok(None)) {
                        job.timed_out = true;
                        crate::exec::kill_tree(job.pid);
                    }
                }
            }
        });
    }
    format!("ok job={id} pid={pid} 会话内后台任务已启动（fs_exec job={id} 轮询，list=true 列出全部，kill=true 结束）")
}

/// `fs_exec list=true`：列出当前会话的后台任务（id、pid、状态、时长）。
/// job id 一旦丢失就再也轮询不到，这里给一条自救路径。
pub fn list() -> String {
    if let Some(e) = disabled() {
        return e;
    }
    let Ok(mut map) = jobs().lock() else {
        return "E_JOB 任务表不可用".into();
    };
    let keys: Vec<u32> = map.keys().copied().collect();
    if keys.is_empty() {
        return "ok jobs 0 个任务".into();
    }
    let mut lines = Vec::new();
    let mut running = 0usize;
    for key in keys {
        let Some(job) = map.get_mut(&key) else {
            continue;
        };
        let elapsed = job.started.elapsed().as_millis();
        match job.child.try_wait() {
            Ok(Some(status)) => lines.push(format!(
                "job={key} pid={} exit={}（已结束）{elapsed}ms",
                job.pid,
                status.code().unwrap_or(-1)
            )),
            _ => {
                running += 1;
                let flag = if job.killed {
                    "（终止中）"
                } else if job.timed_out {
                    "（超时）"
                } else {
                    ""
                };
                lines.push(format!(
                    "job={key} pid={} running{flag} {elapsed}ms",
                    job.pid
                ));
            }
        }
    }
    let mut text = format!("ok jobs {} 个任务（{} 运行中）", lines.len(), running);
    for line in lines {
        text.push('\n');
        text.push_str(&line);
    }
    text
}

pub fn poll_or_kill(job_id: u32, args: &Value) -> String {
    if let Some(e) = disabled() {
        return e;
    }
    let kill = crate::tools::arg_bool(args, "kill", false);
    let Ok(mut map) = jobs().lock() else {
        return "E_JOB 任务表不可用".into();
    };
    let Some(job) = map.get_mut(&job_id) else {
        return format!("E_NO_JOB 没有任务 {job_id}；后台任务只在当前 MCP 会话内有效");
    };
    let mut exit_code: Option<i32> = None;
    if kill && !job.killed {
        crate::exec::kill_tree(job.pid);
        job.killed = true;
        if let Ok(status) = job.child.wait() {
            exit_code = Some(status.code().unwrap_or(-1));
        }
    } else if let Ok(Some(status)) = job.child.try_wait() {
        exit_code = Some(status.code().unwrap_or(-1));
    }
    let elapsed = job.started.elapsed().as_millis();
    let (out_delta, out_kept) = take_delta(&job.stdout, &mut job.cursor_out);
    let (err_delta, err_kept) = take_delta(&job.stderr, &mut job.cursor_err);
    let mut text = match exit_code {
        Some(code) => {
            format!("ok job={job_id} exit={code} {elapsed}ms stdout={out_kept}B stderr={err_kept}B")
        }
        None => format!("ok job={job_id} running {elapsed}ms pid={}", job.pid),
    };
    if job.killed {
        text.push_str("（已终止）");
    }
    if job.timed_out {
        text.push_str("（超时）");
    }
    text.push('\n');
    push_delta(&mut text, "stdout", &out_delta, job.dropped_out);
    push_delta(&mut text, "stderr", &err_delta, job.dropped_err);
    text
}

/// MCP 服务退出时结束所有仍在运行的任务，避免留下孤儿进程。
pub fn shutdown() {
    let Ok(mut map) = jobs().lock() else {
        return;
    };
    for (_, mut job) in map.drain() {
        if matches!(job.child.try_wait(), Ok(None)) {
            crate::exec::kill_tree(job.pid);
            let _ = job.child.wait();
        }
    }
}

fn take_delta(buffer: &Arc<Mutex<Vec<u8>>>, cursor: &mut usize) -> (String, usize) {
    let guard = buffer.lock().unwrap();
    let data = guard.get(*cursor..).unwrap_or(&[]);
    // 进程可能把多字节字符拆在两次读取之间：末尾不完整的 UTF-8 序列留到下次，
    // 否则 decode 会按 GBK 猜出错误字符，游标推进后再也补不回来。
    let usable = match std::str::from_utf8(data) {
        Ok(_) => data.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => data.len(),
    };
    let delta = crate::text::decode(&data[..usable]).text;
    *cursor += usable;
    (delta, guard.len())
}

fn push_delta(text: &mut String, name: &str, delta: &str, dropped: usize) {
    if !delta.trim().is_empty() {
        text.push_str(&format!("--- {name}（新增）---\n"));
        text.push_str(delta);
        if !delta.ends_with('\n') {
            text.push('\n');
        }
    }
    if dropped > 0 {
        text.push_str(&format!(
            "… {name} 已丢弃 {dropped}B（超过 {MAX_BUFFER}B 缓冲上限）\n"
        ));
    }
}

fn pump(
    mut stream: impl Read + Send + 'static,
    buffer: Arc<Mutex<Vec<u8>>>,
    dropped: Arc<AtomicUsize>,
) {
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let mut guard = buffer.lock().unwrap();
                    let room = MAX_BUFFER.saturating_sub(guard.len());
                    if room > 0 {
                        guard.extend_from_slice(&chunk[..n.min(room)]);
                    }
                    if n > room {
                        dropped.fetch_add(n - room, Ordering::Relaxed);
                    }
                }
                Err(_) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn job_id(text: &str) -> u32 {
        text.split("job=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|id| id.parse().ok())
            .unwrap_or_else(|| panic!("无法解析 job id: {text}"))
    }

    #[test]
    fn 启动轮询到退出() {
        let started =
            start(&json!({"program": "cmd", "args": ["/c", "ping", "-n", "2", "127.0.0.1"]}));
        assert!(started.starts_with("ok job="), "{started}");
        let id = job_id(&started);
        let mut text = String::new();
        for _ in 0..50 {
            text = poll_or_kill(id, &json!({}));
            if text.contains("exit=") {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(text.contains("exit=0"), "{text}");
        assert!(text.contains("stdout"), "应返回输出: {text}");
    }

    #[test]
    fn kill结束进程树() {
        let started =
            start(&json!({"program": "cmd", "args": ["/c", "ping", "-n", "10", "127.0.0.1"]}));
        let id = job_id(&started);
        std::thread::sleep(Duration::from_millis(200));
        let text = poll_or_kill(id, &json!({"kill": true}));
        assert!(text.contains("exit="), "{text}");
        assert!(text.contains("已终止"), "{text}");
    }

    #[test]
    fn 超时自动终止() {
        let started = start(&json!({
            "program": "cmd",
            "args": ["/c", "ping", "-n", "10", "127.0.0.1"],
            "timeout_ms": 300
        }));
        let id = job_id(&started);
        std::thread::sleep(Duration::from_millis(1200));
        let text = poll_or_kill(id, &json!({}));
        assert!(text.contains("exit="), "{text}");
        assert!(text.contains("超时"), "{text}");
    }

    #[test]
    fn 增量输出不切断多字节字符() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let mut cursor = 0usize;
        // "中" 的 UTF-8 是 E4 B8 AD；先写前两个字节，模拟进程一次只刷新半个字符。
        buffer.lock().unwrap().extend_from_slice(&[0xE4, 0xB8]);
        let (first, _) = take_delta(&buffer, &mut cursor);
        assert_eq!(first, "", "不完整字符不应解码成替换字符：{first}");
        assert_eq!(cursor, 0, "游标必须停在完整字符边界");
        buffer.lock().unwrap().push(0xAD);
        let (second, _) = take_delta(&buffer, &mut cursor);
        assert_eq!(second, "中");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn 列出后台任务() {
        let started = start(&json!({
            "program": "cmd",
            "args": ["/c", "ping", "-n", "2", "127.0.0.1"]
        }));
        let id = job_id(&started);
        let text = list();
        assert!(text.starts_with("ok jobs"), "{text}");
        assert!(
            text.contains(&id.to_string()),
            "列表应含刚启动的任务: {text}"
        );
        let text = poll_or_kill(id, &json!({"kill": true}));
        assert!(text.contains("已终止"), "{text}");
    }

    #[test]
    fn 未知任务返回可执行错误() {
        let text = poll_or_kill(99999, &json!({}));
        assert!(text.starts_with("E_NO_JOB"), "{text}");
        assert!(text.contains("MCP"), "{text}");
    }
}
