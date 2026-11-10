//! 命令行入口：同一个二进制既能当 MCP 服务，也能直接当 Unix 风格工具用。
//!
//! 用法示例：
//!   fsops read /d/repo/app/src/main.rs --start 40
//!   fsops grep "fn main" --path /d/repo/app --glob "**/*.rs"
//!   fsops call fs_edit '{"path":"/d/a.txt","edits":[{"old":"a","new":"b"}]}'
//!   fsops install claude

use crate::mcp::DEFAULT_JOBS;
use crate::tools;
use serde_json::{json, Value};
use std::io::Read;

pub fn run(args: &[String]) -> i32 {
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[1..];
    let out = match cmd {
        "mcp" => {
            return match mcp_jobs(rest) {
                Ok(jobs) => crate::mcp::serve_with(jobs).map(|_| 0).unwrap_or(1),
                Err(e) => {
                    println!("{e}");
                    2
                }
            }
        }
        "read" => cmd_read(rest),
        "grep" => cmd_grep(rest),
        "glob" => cmd_glob(rest),
        "stat" => cmd_stat(rest),
        "call" => cmd_call(rest),
        "schema" => serde_json::to_string_pretty(&tools::tool_defs())
            .unwrap_or_else(|_| "E_INTERNAL schema 序列化失败".into()),
        "install" => return install(rest),
        "integrate" => return crate::adopt::integrate(rest),
        "doctor" => return crate::adopt::doctor(rest),
        "harden" => return crate::harden::run(rest),
        "hook" => return crate::harden::hook(rest),
        "upgrade" => return crate::upgrade::run(rest),
        "version" | "--version" | "-V" => format!("fsops {}", crate::VERSION),
        "help" | "--help" | "-h" | "" => usage(),
        other => format!("E_UNKNOWN_CMD 未知命令 {other}（用 fsops help 查看）"),
    };
    println!("{out}");
    if out.starts_with("E_") || crate::tools::has_error_line(&out) {
        2
    } else {
        0
    }
}

fn usage() -> String {
    format!(
        "fsops {} — 面向 AI Agent 的 Windows 文件操作层

用法:
  fsops mcp [--jobs N]                   以 MCP 服务运行（stdio；默认 4 并发，0/1 为单工作线程）
  fsops read <path> [--start N] [--end N] [--budget N]
  fsops grep <pattern> [--path DIR] [--glob G] [--context N] [--ignore-case]
  fsops glob [pattern] [--path DIR] [--dirs] [--limit N]
  fsops stat <path...>
  fsops call <tool> '<json 参数>'
  fsops schema                              输出全部工具的 MCP schema（JSON）
  fsops install <claude|hermes|codex|json>  生成或写入 MCP 配置
  fsops integrate --rules [--target ...]     向项目写入 fsops 软规则
  fsops doctor [--json]                      检查规则与宿主接入状态
  fsops harden claude [--path DIR|--user]    安装 Claude Code PreToolUse 接管
  fsops upgrade [--check] [--force]          自更新：查最新 Release、SHA-256 校验后替换
  fsops version

路径写法: /d/repo/x  ~/x  D:\\repo\\x  相对路径",
        crate::VERSION
    )
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// fsops mcp --jobs N：默认 4；0/1 表示串行。
fn mcp_jobs(args: &[String]) -> Result<usize, String> {
    let Some(raw) = flag(args, "--jobs") else {
        return Ok(DEFAULT_JOBS);
    };
    match raw.parse::<usize>() {
        Ok(n) if n <= 64 => Ok(n),
        _ => Err(format!("E_BAD_ARGS --jobs 需要 0-64 的整数，当前 {raw}")),
    }
}

fn positional(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a.starts_with("--") {
            skip = true;
            continue;
        }
        out.push(a.clone());
    }
    out
}

fn cmd_read(args: &[String]) -> String {
    let p = match positional(args).first() {
        Some(p) => p.clone(),
        None => return "E_BAD_ARGS 需要文件路径".into(),
    };
    let mut v = json!({"path": p});
    if let Some(s) = flag(args, "--start") {
        v["start"] = json!(s.parse::<u64>().unwrap_or(1));
    }
    if let Some(e) = flag(args, "--end") {
        v["end"] = json!(e.parse::<u64>().unwrap_or(u64::MAX));
    }
    if let Some(b) = flag(args, "--budget") {
        v["budget"] = json!(b.parse::<u64>().unwrap_or(32768));
    }
    tools::call("fs_read", &v)
}

fn cmd_grep(args: &[String]) -> String {
    let pat = match positional(args).first() {
        Some(p) => p.clone(),
        None => return "E_BAD_ARGS 需要正则".into(),
    };
    let mut v = json!({"pattern": pat});
    if let Some(p) = flag(args, "--path") {
        v["path"] = json!(p);
    }
    if let Some(g) = flag(args, "--glob") {
        v["glob"] = json!(g);
    }
    if let Some(c) = flag(args, "--context") {
        v["context"] = json!(c.parse::<u64>().unwrap_or(0));
    }
    if has(args, "--ignore-case") || has(args, "-i") {
        v["ignore_case"] = json!(true);
    }
    tools::call("fs_grep", &v)
}

fn cmd_glob(args: &[String]) -> String {
    let mut v = json!({});
    if let Some(p) = positional(args).first() {
        v["pattern"] = json!(p);
    }
    if let Some(p) = flag(args, "--path") {
        v["path"] = json!(p);
    }
    if let Some(l) = flag(args, "--limit") {
        v["limit"] = json!(l.parse::<u64>().unwrap_or(200));
    }
    if has(args, "--dirs") {
        v["dirs"] = json!(true);
    }
    tools::call("fs_glob", &v)
}

fn cmd_stat(args: &[String]) -> String {
    let paths = positional(args);
    if paths.is_empty() {
        return "E_BAD_ARGS 需要路径".into();
    }
    tools::call("fs_stat", &json!({"paths": paths}))
}

/// 读取 `fsops call` 的 JSON 载荷：`-` 或 `--stdin` 表示从标准输入读。
///
/// 进程参数在 Windows 上有约 32 KB 上限，大段 `content`/`edits` 必须走 stdin；
/// 插件（DSH 等）也用它传参，避免命令行长度问题。
fn read_call_payload(arg: Option<&str>, stdin: &mut dyn Read) -> Result<String, String> {
    match arg {
        Some("-") | Some("--stdin") => {
            let mut buf = String::new();
            stdin
                .read_to_string(&mut buf)
                .map_err(|e| format!("E_BAD_ARGS 读取标准输入失败: {e}"))?;
            Ok(buf)
        }
        Some(value) => Ok(value.to_string()),
        None => Ok("{}".into()),
    }
}

fn cmd_call(args: &[String]) -> String {
    let name = match args.first() {
        Some(n) => n.clone(),
        None => return "E_BAD_ARGS 用法: fsops call <tool> '<json>'".into(),
    };
    let mut stdin = std::io::stdin();
    let raw = match read_call_payload(args.get(1).map(String::as_str), &mut stdin) {
        Ok(raw) => raw,
        Err(e) => return e,
    };
    let value: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return format!("E_BAD_ARGS JSON 解析失败: {e}"),
    };
    // 一次性进程没有长驻任务表：后台任务在这里必须明确拒绝，避免留下孤儿进程。
    crate::jobs::disable();
    tools::call(&name, &value)
}

/// 生成各 agent 的 MCP 配置。只打印，不改别人的配置文件。
fn install(args: &[String]) -> i32 {
    let target = args.first().map(String::as_str).unwrap_or("json");
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| "fsops.exe".into());

    match target {
        "claude" => println!(
            "claude mcp add fsops -- \"{exe}\" mcp\n# 或写入项目 .mcp.json:\n{}",
            json!({"mcpServers":{"fsops":{"command":exe,"args":["mcp"]}}})
        ),
        "hermes" => println!(
            "在 ~/.hermes/config.yaml 的 mcp_servers 下加:\n{}",
            serde_json::to_string_pretty(&json!({
                "fsops": {"command": exe, "args": ["mcp"], "enabled": true}
            }))
            .unwrap_or_default()
        ),
        "codex" => println!(
            "在 ~/.codex/config.toml 末尾加:\n\n[mcp_servers.fsops]\ncommand = \"{}\"\nargs = [\"mcp\"]",
            exe.replace('\\', "\\\\")
        ),
        _ => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "mcpServers": {"fsops": {"command": exe, "args": ["mcp"]}}
            }))
            .unwrap_or_default()
        ),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::{mcp_jobs, read_call_payload};
    use std::io::Cursor;

    #[test]
    fn call载荷从标准输入读取() {
        let mut input = Cursor::new(b"{\"path\":\"/d/a.txt\"}".to_vec());
        let payload = read_call_payload(Some("-"), &mut input).expect("读取 stdin 不应失败");
        assert_eq!(payload, "{\"path\":\"/d/a.txt\"}");
    }

    #[test]
    fn call载荷保留位置参数与缺省空对象() {
        let mut input = Cursor::new(Vec::new());
        assert_eq!(read_call_payload(Some("{}"), &mut input).unwrap(), "{}");
        assert_eq!(read_call_payload(None, &mut input).unwrap(), "{}");
    }

    #[test]
    fn mcp参数解析jobs() {
        assert_eq!(mcp_jobs(&[] as &[String]).unwrap(), 4);
        assert_eq!(
            mcp_jobs(&["--jobs".to_string(), "8".to_string()]).unwrap(),
            8
        );
        assert_eq!(
            mcp_jobs(&["--jobs".to_string(), "1".to_string()]).unwrap(),
            1
        );
        assert!(mcp_jobs(&["--jobs".to_string(), "x".to_string()]).is_err());
        assert!(mcp_jobs(&["--jobs".to_string(), "999".to_string()]).is_err());
    }

    #[test]
    fn call载荷支持长别名stdin() {
        let mut input = Cursor::new(b"{\"pattern\":\"x\"}".to_vec());
        assert_eq!(
            read_call_payload(Some("--stdin"), &mut input).unwrap(),
            "{\"pattern\":\"x\"}"
        );
    }
}
