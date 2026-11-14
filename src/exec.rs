//! fs_exec：用 argv 数组直接 spawn，不经过 shell 文本解析。
//!
//! 目的不是替代 shell 的全部能力，而是消掉 Agent 最常见的几类事故：
//! 引号/插值、2>&1 把 stderr 当异常、$LASTEXITCODE、编码猜测。

use serde_json::Value;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_MS: usize = 120_000;
const MAX_TIMEOUT_MS: usize = 600_000;
const DEFAULT_MAX_OUTPUT: usize = 32 * 1024;

/// 把 cwd（缺省为进程 cwd）向上各级的 node_modules/.bin 接进 PATH，nearest 优先。
/// 仓库本地 CLI（electron-vite、vitest、tsc 等）因此不必写绝对路径；
/// 用户显式给的 env.PATH 仍在其后覆盖，保持原有控制权。
fn prepend_repo_bin(command: &mut Command, args: &Value) {
    let start = match crate::tools::arg_str(args, "cwd") {
        Some(dir) => match crate::paths::FsPath::parse(dir) {
            Ok(p) => p.native().to_path_buf(),
            Err(_) => return,
        },
        None => match std::env::current_dir() {
            Ok(p) => p,
            Err(_) => return,
        },
    };
    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut prefix = String::new();
    for dir in start.ancestors() {
        let bin = dir.join("node_modules").join(".bin");
        if bin.is_dir() {
            if !prefix.is_empty() {
                prefix.push_str(sep);
            }
            prefix.push_str(&bin.to_string_lossy());
        }
    }
    if prefix.is_empty() {
        return;
    }
    let existing = std::env::var("PATH").unwrap_or_default();
    command.env("PATH", format!("{prefix}{sep}{existing}"));
}

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
        // 与文件工具同一套路径解析：/c/...、~/、相对路径都接受。
        let cwd =
            crate::paths::FsPath::parse(dir).map_err(|e| format!("E_BAD_ARGS cwd `{dir}`：{e}"))?;
        if !cwd.is_dir() {
            return Err(format!(
                "E_BAD_ARGS cwd `{dir}` 不存在或不是目录（解析为 {}）",
                cwd.display()
            ));
        }
        command.current_dir(cwd.wide());
    }
    prepend_repo_bin(&mut command, args);
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

/// 一次性调用（CLI）入口：没有宿主可断开，取消信号永远不置位。
pub fn run(args: &Value) -> String {
    let never = AtomicBool::new(false);
    run_with_cancel(args, &never)
}

/// fs_exec 唯一入口。
///
/// 先处理后台任务（`{list:true}` / `{job:N}` / `background:true`）——它们不跑前台进程，
/// 也就不需要取消信号；其余走前台执行。
///
/// `cancel` 被置位时结束前台进程树并返回 E_ABORTED：MCP 层在宿主断开时置位，
/// 否则一次 2 分钟的构建会把 fsops 和子进程都留住，宿主已经走了也没人读结果。
pub fn run_with_cancel(args: &Value, cancel: &AtomicBool) -> String {
    // 后台任务：{job:N} 轮询/终止；{list:true} 列出全部任务。
    if crate::tools::arg_bool(args, "list", false) {
        return crate::jobs::list();
    }
    if let Some(job_id) = args.get("job").and_then(Value::as_u64) {
        return crate::jobs::poll_or_kill(job_id as u32, args);
    }
    if crate::tools::arg_bool(args, "background", false) {
        return crate::jobs::start(args);
    }
    let check = crate::tools::arg_bool(args, "check", false);
    let timeout_ms =
        crate::tools::arg_usize(args, "timeout_ms", DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS);
    let requested_max_output = crate::tools::arg_usize(args, "max_output", DEFAULT_MAX_OUTPUT);
    // 钳制到 1024 是为了别把输出切成不可用的碎片；但不能静默——回显实际生效值。
    const MIN_MAX_OUTPUT: usize = 1024;
    let max_output = requested_max_output.max(MIN_MAX_OUTPUT);
    let clamp_note = if max_output == requested_max_output {
        String::new()
    } else {
        format!(" max_output={max_output}B（请求 {requested_max_output}B，已按最小值执行）")
    };
    let stdin_text = crate::tools::arg_str(args, "stdin").map(str::to_string);
    let (mut command, program) = match build_command(args) {
        Ok(pair) => pair,
        Err(e) => return e,
    };

    let started = Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            // 相对/ POSIX 形式的 program 会被 CreateProcess 按宿主进程目录解析，
            // 不随 cwd 走；直接点明，避免模型按 cwd 猜路径白跑一轮。
            let hint = if program_is_relative(&program) {
                "；program 需原生绝对路径（如 D:/repo/x.exe）或交给 PATH；相对路径不随 cwd 解析"
            } else {
                "；检查 PATH 或写绝对路径"
            };
            return format!("E_NOT_FOUND program={program}：{e}{hint}");
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
    let mut aborted = false;
    // 取消之后不再等管道排空：宿主已经走了，谁读结果不重要。
    // 超时则仍要读空，因为结果还有人要。
    let mut abort_deadline: Option<Instant> = None;
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
        if exit_code.is_none() && !aborted && cancel.load(Ordering::Relaxed) {
            aborted = true;
            abort_deadline = Some(Instant::now() + Duration::from_millis(200));
            kill_tree(child.id());
        }
        if abort_deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        if exit_code.is_none() && !timed_out && !aborted && Instant::now() >= deadline {
            timed_out = true;
            kill_tree(child.id());
        }
        // 结束信号之后仍要读空管道：孙进程可能还握着句柄，等不到 EOF 会一直转
        if (timed_out || aborted) && Instant::now() > deadline + Duration::from_secs(5) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let elapsed = started.elapsed().as_millis();
    let (out_bytes, out_total) = collected[0].take().unwrap_or_default();
    let (err_bytes, err_total) = collected[1].take().unwrap_or_default();
    let out_text = crate::text::decode(&out_bytes).text;
    let err_text = crate::text::decode(&err_bytes).text;

    if aborted {
        let mut text = format!(
            "E_ABORTED program={program} 宿主已断开，提前结束进程树（duration={elapsed}ms）\n"
        );
        push_stream(&mut text, "stdout", &out_text, out_bytes.len(), out_total);
        push_stream(&mut text, "stderr", &err_text, err_bytes.len(), err_total);
        return text;
    }

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
        format!(
            "E_EXIT code={exit} {elapsed}ms stdout={out_total}B stderr={err_total}B{clamp_note}\n"
        )
    } else if exit != 0 {
        // check=false 时非零退出不算失败，但前缀绝不能是 ok：
        // 模型扫到 ok 会把编译/测试失败当成功。
        format!(
            "exit={exit} 非零 {elapsed}ms stdout={out_total}B stderr={err_total}B{clamp_note}\n"
        )
    } else {
        format!("ok exit=0 {elapsed}ms stdout={out_total}B stderr={err_total}B{clamp_note}\n")
    };
    push_stream(&mut text, "stdout", &out_text, out_bytes.len(), out_total);
    push_stream(&mut text, "stderr", &err_text, err_bytes.len(), err_total);
    if out_total == 0 && err_total == 0 {
        text.push_str("（无输出）\n");
    }
    text
}

/// 带路径分隔符但不是 Windows 原生绝对路径（相对路径、/d/... 形式）。
fn program_is_relative(program: &str) -> bool {
    let p = program.trim();
    let has_separator = p.contains('/') || p.contains('\\');
    let native_absolute = p.starts_with("\\\\")
        || (p.len() >= 2 && p.as_bytes()[1] == b':' && p.as_bytes()[0].is_ascii_alphabetic());
    has_separator && !native_absolute
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
    use std::sync::Arc;

    #[test]
    fn 宿主断开时前台命令被提前结束() {
        // 宿主取消/超时后不会等我们跑完；原先只能等 timeout_ms 到点，
        // 期间 fsops 进程和子进程都活着（真实事故：20 秒的测试进程拖住整个会话回收）。
        let flag = Arc::new(AtomicBool::new(false));
        let watcher = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            watcher.store(true, Ordering::Relaxed);
        });
        let began = Instant::now();
        let out = run_with_cancel(
            &json!({
                "program": "cmd",
                "args": ["/c", "ping -n 6 127.0.0.1 > nul"],
                "timeout_ms": 30000
            }),
            &flag,
        );
        let elapsed = began.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "应在宿主断开后很快返回，实际 {elapsed:?}: {out}"
        );
        assert!(
            out.contains("E_ABORTED") || out.starts_with("exit="),
            "{out}"
        );
    }

    #[test]
    fn 捕获退出码与stdout() {
        let out = run(&json!({"program": "cmd", "args": ["/c", "echo hello"]}));
        assert!(out.starts_with("ok exit=0"), "{out}");
        assert!(out.contains("hello"), "{out}");
    }

    #[test]
    fn 非零退出不是错误() {
        let out = run(&json!({"program": "cmd", "args": ["/c", "exit 3"]}));
        // 非零退出在 check=false 时不算失败，但前缀不能是 ok：
        // 模型扫到 ok 会把编译失败当成功。
        assert!(out.starts_with("exit=3"), "{out}");
        assert!(!out.contains("ok exit=3"), "{out}");
    }

    #[test]
    fn max_output被钳制时回显实际值() {
        // 静默抬高参数会让模型以为自己的 200B 生效了，看到的却是 1024B。
        let out = run(&json!({"program": "cmd", "args": ["/c", "echo hi"], "max_output": 200}));
        assert!(out.starts_with("ok"), "{out}");
        assert!(out.contains("max_output=1024B"), "应回显实际生效值: {out}");
        assert!(out.contains("请求 200B"), "应说明请求值: {out}");
    }

    #[test]
    fn max_output未钳制时不加噪音() {
        let out = run(&json!({"program": "cmd", "args": ["/c", "echo hi"]}));
        assert!(!out.contains("max_output="), "默认值不该回显: {out}");
    }

    #[test]
    fn check开启时非零退出是错误() {
        let out = run(&json!({"program": "cmd", "args": ["/c", "exit 3"], "check": true}));
        assert!(out.starts_with("E_EXIT"), "{out}");
        assert!(out.contains("code=3"), "{out}");
        // 默认行为不变：非零退出仍不算失败，但不再冠以 ok。
        let off = run(&json!({"program": "cmd", "args": ["/c", "exit 3"]}));
        assert!(off.starts_with("exit=3"), "{off}");
        assert!(!off.contains("ok exit=3"), "{off}");
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

    #[test]
    fn 相对program路径提示不随cwd解析() {
        let out = run(&json!({
            "program": "no-such-dir/fsops.exe",
            "cwd": std::env::temp_dir().to_string_lossy(),
        }));
        assert!(out.starts_with("E_NOT_FOUND"), "{out}");
        assert!(out.contains("不随 cwd"), "{out}");
        assert!(out.contains("绝对路径"), "{out}");
    }

    #[test]
    fn cwd接受posix与原生两种形式() {
        // 模型的路径习惯是 /c/...；cwd 不能只认原生形式，否则要白跑一轮。
        let dir = std::env::temp_dir().join(format!("fsops-exec-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let native = dir.to_string_lossy().to_string();
        let mut chars = native.chars();
        let drive = chars.next().unwrap().to_ascii_lowercase();
        let rest = chars.as_str().trim_start_matches(':').replace('\\', "/");
        let posix = format!("/{drive}{rest}");
        let name = dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_ascii_lowercase();

        for cwd in [posix, native] {
            let out = run(&json!({"program": "cmd", "args": ["/c", "cd"], "cwd": cwd}));
            assert!(out.starts_with("ok exit=0"), "cwd={cwd}: {out}");
            assert!(
                out.to_ascii_lowercase().contains(&name),
                "cwd={cwd} 未进入目标目录: {out}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cwd向上各级的node_modules_bin进path() {
        let root = std::env::temp_dir().join(format!("fsops-repo-bin-{}", std::process::id()));
        let bin = root.join("node_modules").join(".bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("fsops-probe.exe"), b"").unwrap();
        let nested = root.join("packages").join("app");
        std::fs::create_dir_all(&nested).unwrap();
        let out = run(&json!({
            "program": "where.exe",
            "args": ["fsops-probe.exe"],
            "cwd": nested.to_string_lossy(),
        }));
        assert!(out.contains(".bin"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cwd不存在时给出清晰错误() {
        let missing =
            std::env::temp_dir().join(format!("fsops-exec-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let out = run(&json!({
            "program": "cmd",
            "args": ["/c", "cd"],
            "cwd": missing.to_string_lossy(),
        }));
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
        assert!(out.contains("cwd"), "{out}");
    }
}
