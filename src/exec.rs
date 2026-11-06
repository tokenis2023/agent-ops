//! fs_exec：用 argv 数组直接 spawn，不经过 shell 文本解析。
//!
//! 目的不是替代 shell 的全部能力，而是消掉 Agent 最常见的几类事故：
//! 引号/插值、2>&1 把 stderr 当异常、$LASTEXITCODE、编码猜测。

use serde_json::Value;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_MS: usize = 120_000;
const MAX_TIMEOUT_MS: usize = 600_000;
const DEFAULT_MAX_OUTPUT: usize = 32 * 1024;

/// 构建 Command：前台与后台任务共用同一套参数解析。
pub fn build_command(args: &Value) -> Result<(Command, String), String> {
    let Some(program) = crate::tools::arg_str(args, "program") else {
        return Err("E_BAD_ARGS fs_exec 需要 program".into());
    };
    let argv: Vec<String> = args
        .get("args")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let shell = crate::tools::arg_bool(args, "shell", false);
    let stdin_text = crate::tools::arg_str(args, "stdin");
    let mut command = if shell {
        // 只在显式要求时走 cmd /c：管道/重定向才需要，代价是回到文本解析。
        let mut line = program.to_string();
        for arg in &argv {
            line.push(' ');
            line.push_str(&quote_for_cmd(arg));
        }
        let mut c = Command::new("cmd");
        c.arg("/c").arg(line);
        c
    } else {
        let mut c = Command::new(program);
        c.args(&argv);
        c
    };
    if let Some(dir) = crate::tools::arg_str(args, "cwd") {
        command.current_dir(dir);
    }
    if let Some(env) = args.get("env").and_then(Value::as_object) {
        for (key, value) in env {
            if let Some(text) = value.as_str() {
                command.env(key, text);
            }
        }
    }
    command
        .stdin(if stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok((command, program.to_string()))
}

pub fn run(args: &Value) -> String {
    // 后台任务：{job:N} 轮询/终止；{background:true} 启动。
    if let Some(job_id) = args.get("job").and_then(Value::as_u64) {
        return crate::jobs::poll_or_kill(job_id as u32, args);
    }
    if crate::tools::arg_bool(args, "background", false) {
        return crate::jobs::start(args);
    }
    let check = crate::tools::arg_bool(args, "check", false);
    let timeout_ms =
        crate::tools::arg_usize(args, "timeout_ms", DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS);
    let max_output = crate::tools::arg_usize(args, "max_output", DEFAULT_MAX_OUTPUT).max(1024);
    let stdin_text = crate::tools::arg_str(args, "stdin").map(str::to_string);
    let (mut command, program) = match build_command(args) {
        Ok(pair) => pair,
        Err(e) => return e,
    };

    let started = Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return format!("E_NOT_FOUND program={program}：{e}；检查 PATH 或写绝对路径");
        }
    };
    if let Some(text) = stdin_text {
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(text.as_bytes());
        }
    }
    let stdout = child.stdout.take().expect("stdout 已管道化");
    let stderr = child.stderr.take().expect("stderr 已管道化");
    let (tx, rx) = mpsc::channel::<(usize, Vec<u8>, usize)>();
    spawn_reader(stdout, max_output, tx.clone(), 0);
    spawn_reader(stderr, max_output, tx, 1);

    let mut collected: [Option<(Vec<u8>, usize)>; 2] = [None, None];
    let mut exit_code: Option<i32> = None;
    let mut timed_out = false;
    let deadline = started + Duration::from_millis(timeout_ms as u64);
    loop {
        while let Ok((index, data, total)) = rx.try_recv() {
            collected[index] = Some((data, total));
        }
        if exit_code.is_none() {
            if let Ok(Some(status)) = child.try_wait() {
                exit_code = Some(status.code().unwrap_or(-1));
            }
        }
        if exit_code.is_some() && collected.iter().all(Option::is_some) {
            break;
        }
        if exit_code.is_none() && !timed_out && Instant::now() >= deadline {
            timed_out = true;
            kill_tree(child.id());
        }
        if timed_out && Instant::now() > deadline + Duration::from_secs(5) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let elapsed = started.elapsed().as_millis();
    let (out_bytes, out_total) = collected[0].take().unwrap_or_default();
    let (err_bytes, err_total) = collected[1].take().unwrap_or_default();
    let out_text = crate::text::decode(&out_bytes).text;
    let err_text = crate::text::decode(&err_bytes).text;

    if timed_out {
        let mut text = format!(
            "E_TIMEOUT program={program} 超过 {timeout_ms}ms，已结束进程树（duration={elapsed}ms）\n"
        );
        push_stream(&mut text, "stdout", &out_text, out_bytes.len(), out_total);
        push_stream(&mut text, "stderr", &err_text, err_bytes.len(), err_total);
        return text;
    }

    // check=true：非零退出按错误返回；默认保持 ok，测试失败不是工具错误。
    let exit = exit_code.unwrap_or(-1);
    let mut text = if check && exit != 0 {
        format!("E_EXIT code={exit} {elapsed}ms stdout={out_total}B stderr={err_total}B\n")
    } else {
        format!("ok exit={exit} {elapsed}ms stdout={out_total}B stderr={err_total}B\n")
    };
    push_stream(&mut text, "stdout", &out_text, out_bytes.len(), out_total);
    push_stream(&mut text, "stderr", &err_text, err_bytes.len(), err_total);
    if out_total == 0 && err_total == 0 {
        text.push_str("（无输出）\n");
    }
    text
}

fn push_stream(text: &mut String, name: &str, content: &str, kept: usize, total: usize) {
    if content.trim().is_empty() {
        return;
    }
    text.push_str(&format!("--- {name} ---\n"));
    text.push_str(content);
    if !content.ends_with('\n') {
        text.push('\n');
    }
    if total > kept {
        text.push_str(&format!("… {name} 截断：显示前 {kept}B / 共 {total}B\n"));
    }
}

fn spawn_reader(
    mut stream: impl Read + Send + 'static,
    cap: usize,
    tx: mpsc::Sender<(usize, Vec<u8>, usize)>,
    index: usize,
) {
    thread::spawn(move || {
        let mut kept: Vec<u8> = Vec::new();
        let mut total = 0usize;
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    if kept.len() < cap {
                        let room = cap - kept.len();
                        kept.extend_from_slice(&buffer[..n.min(room)]);
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send((index, kept, total));
    });
}

pub(crate) fn kill_tree(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// shell=true 时的尽力转义：含空格或引号才加引号，内部引号双写。
fn quote_for_cmd(arg: &str) -> String {
    if arg.is_empty() || arg.contains(' ') || arg.contains('\t') || arg.contains('"') {
        format!("\"{}\"", arg.replace('"', "\"\""))
    } else {
        arg.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 捕获退出码与stdout() {
        let out = run(&json!({"program": "cmd", "args": ["/c", "echo hello"]}));
        assert!(out.starts_with("ok exit=0"), "{out}");
        assert!(out.contains("hello"), "{out}");
    }

    #[test]
    fn 非零退出不是错误() {
        let out = run(&json!({"program": "cmd", "args": ["/c", "exit 3"]}));
        assert!(out.starts_with("ok exit=3"), "{out}");
    }

    #[test]
    fn check开启时非零退出是错误() {
        let out = run(&json!({"program": "cmd", "args": ["/c", "exit 3"], "check": true}));
        assert!(out.starts_with("E_EXIT"), "{out}");
        assert!(out.contains("code=3"), "{out}");
        // 默认行为不变：非零退出仍是 ok
        let off = run(&json!({"program": "cmd", "args": ["/c", "exit 3"]}));
        assert!(off.starts_with("ok exit=3"), "{off}");
    }

    #[test]
    fn 区分stdout与stderr() {
        let out = run(&json!({"program": "cmd", "args": ["/c", "echo out & echo err 1>&2"]}));
        assert!(
            out.contains("--- stdout ---") && out.contains("out"),
            "{out}"
        );
        assert!(
            out.contains("--- stderr ---") && out.contains("err"),
            "{out}"
        );
    }

    #[test]
    fn 参数含空格不被拆分() {
        let out = run(&json!({"program": "cmd", "args": ["/c", "echo", "a b"]}));
        assert!(out.contains("a b"), "{out}");
    }

    #[test]
    fn 超时结束进程树() {
        let started = std::time::Instant::now();
        let out = run(
            &json!({"program": "cmd", "args": ["/c", "ping", "-n", "10", "127.0.0.1"], "timeout_ms": 500}),
        );
        assert!(out.starts_with("E_TIMEOUT"), "{out}");
        assert!(started.elapsed().as_secs() < 5, "超时后应立即返回");
    }

    #[test]
    fn 程序不存在返回可执行错误() {
        let out = run(&json!({"program": "definitely-not-a-real-program-xyz"}));
        assert!(out.starts_with("E_NOT_FOUND"), "{out}");
        assert!(out.contains("PATH"), "{out}");
    }
}
