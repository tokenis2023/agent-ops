//! Claude Code 硬适配：安装 PreToolUse hook，并处理 hook 输入。
//!
//! 只有 Claude Code 这类提供 hook 的宿主才能做到“内置工具被拒”。
//! fsops 自己不隐藏任何宿主工具；这里通过宿主的 PreToolUse 协议拒绝文本读取。

use crate::io;
use crate::paths::FsPath;
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const TOOLS: &[&str] = &["Read", "Grep", "Glob"];
const NON_TEXT_EXT: &[&str] = &[
    "doc", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "png", "jpg", "jpeg", "gif",
    "webp", "bmp", "ico", "tif", "tiff", "svg", "zip", "gz", "tgz", "7z", "rar", "exe", "dll",
    "bin", "so", "dylib", "class", "jar", "woff", "woff2", "ttf", "otf", "mp3", "mp4", "wav",
    "webm", "mov",
];

pub fn hook(args: &[String]) -> i32 {
    let host = args.first().map(String::as_str).unwrap_or("claude");
    if host != "claude" {
        eprintln!("fsops hook: 目前只支持 claude");
        return 2;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0;
    }
    let Ok(value) = serde_json::from_str::<Value>(&input) else {
        return 0; // 输入不合法时放行，避免 hook 自身把宿主卡死
    };
    if let Some(reason) = claude_decision(&value) {
        let payload = json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason
            }
        });
        println!("{payload}");
    }
    0
}

fn claude_decision(input: &Value) -> Option<String> {
    let tool = input.get("tool_name").and_then(Value::as_str)?;
    match tool {
        "Read" => {
            let path = input
                .pointer("/tool_input/file_path")
                .and_then(Value::as_str)
                .or_else(|| input.pointer("/tool_input/path").and_then(Value::as_str));
            if path.map(is_non_text).unwrap_or(false) {
                None
            } else {
                Some(
                    "文本文件请改用 mcp__fsops__fs_read（支持行范围、编码与换行保护）。\
内置 Read 仅用于图片和旧版 .doc/二进制。"
                        .to_string(),
                )
            }
        }
        "Grep" => {
            Some("文本搜索请改用 mcp__fsops__fs_grep（支持分页、预算和稳定分组）。".to_string())
        }
        "Glob" => {
            Some("查找文件请改用 mcp__fsops__fs_glob（尊重 .gitignore，支持预算）。".to_string())
        }
        _ => None,
    }
}

fn is_non_text(path: &str) -> bool {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    matches!(ext.as_deref(), Some(e) if NON_TEXT_EXT.contains(&e))
}

pub fn run(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", usage());
        return 0;
    }
    let opts = match parse(args) {
        Ok(v) => v,
        Err(e) => {
            println!("E_BAD_ARGS {e}\n\n{}", usage());
            return 2;
        }
    };
    match run_at(&opts) {
        Ok(code) => code,
        Err(e) => {
            println!("E_HARDEN {e}");
            2
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Install,
    Remove,
    Check,
}

struct Opts {
    settings: PathBuf,
    mode: Mode,
    dry_run: bool,
    user: bool,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    if args.first().map(String::as_str) != Some("claude") {
        return Err("目前只支持: fsops harden claude".into());
    }
    let mut root = std::env::current_dir().map_err(|e| format!("无法获取当前目录: {e}"))?;
    let mut user = false;
    let mut mode = Mode::Install;
    let mut mode_set = false;
    let mut dry_run = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--user" => user = true,
            "--dry-run" => dry_run = true,
            "--remove" => {
                if mode_set && mode != Mode::Remove {
                    return Err("--remove 不能与 --check 同时使用".into());
                }
                mode = Mode::Remove;
                mode_set = true;
            }
            "--check" => {
                if mode_set && mode != Mode::Check {
                    return Err("--check 不能与 --remove 同时使用".into());
                }
                mode = Mode::Check;
                mode_set = true;
            }
            "--path" => {
                if user {
                    return Err("--path 与 --user 不能同时使用".into());
                }
                i += 1;
                let p = args.get(i).ok_or_else(|| "--path 需要目录".to_string())?;
                root = FsPath::parse(p)
                    .map_err(|e| format!("E_BAD_PATH {e}"))?
                    .native()
                    .to_path_buf();
            }
            other => return Err(format!("未知参数 {other}")),
        }
        i += 1;
    }
    if mode == Mode::Check && dry_run {
        return Err("--check 与 --dry-run 不能同时使用".into());
    }
    let settings = if user {
        home_dir()
            .ok_or_else(|| "无法定位用户目录".to_string())?
            .join(".claude")
            .join("settings.json")
    } else {
        root.join(".claude").join("settings.json")
    };
    Ok(Opts {
        settings,
        mode,
        dry_run,
        user,
    })
}

fn run_at(opts: &Opts) -> Result<i32, String> {
    let current = read_settings(&opts.settings)?;
    let mut settings = current.clone().unwrap_or_else(|| json!({}));
    let exe = std::env::current_exe()
        .map_err(|e| format!("无法获取 fsops 路径: {e}"))?
        .to_string_lossy()
        .to_string();
    let label = match opts.mode {
        Mode::Install => "安装",
        Mode::Remove => "移除",
        Mode::Check => "检查",
    };
    println!(
        "fsops harden claude — {}{}",
        label,
        if opts.dry_run { "（预演）" } else { "" }
    );
    println!("settings: {}", opts.settings.display());

    if opts.user {
        println!("范围: 当前用户（会影响所有项目）");
    } else {
        println!("范围: 当前项目");
    }

    let statuses = match opts.mode {
        Mode::Install => ensure_hooks(&mut settings, &exe)?,
        Mode::Remove => remove_hooks(&mut settings),
        Mode::Check => check_hooks(&settings),
    };
    for (tool, status) in &statuses {
        println!("  {:<6} {}", tool, status);
    }

    let failed = match opts.mode {
        Mode::Check => statuses.iter().any(|(_, s)| s != "已安装"),
        _ => false,
    };

    if opts.mode == Mode::Check {
        if failed {
            println!("E_HARDEN Claude Code hook 未完整安装");
            return Ok(2);
        }
        println!("Claude Code 硬接管已安装。");
        return Ok(0);
    }

    if opts.dry_run {
        println!("预演完成，未修改文件。");
        return Ok(0);
    }

    if opts.mode == Mode::Remove && current.is_none() {
        println!("没有 .claude/settings.json，无需移除。");
        return Ok(0);
    }
    if settings != current.unwrap_or_else(|| json!({})) {
        write_settings(&opts.settings, &settings)?;
        println!("已写入 {}", opts.settings.display());
    } else {
        println!("无变化。");
    }
    if opts.mode == Mode::Install {
        println!("提示: 规则文件仍需 fsops integrate --rules --target claude");
    }
    Ok(0)
}

fn read_settings(path: &Path) -> Result<Option<Value>, String> {
    match fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes)
                .map_err(|_| format!("E_ENCODING {} 不是 UTF-8", path.display()))?;
            let value: Value = serde_json::from_str(&text)
                .map_err(|e| format!("E_SETTINGS {} JSON 解析失败: {e}", path.display()))?;
            if !value.is_object() {
                return Err(format!("E_SETTINGS {} 顶层不是对象", path.display()));
            }
            Ok(Some(value))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("E_READ 无法读取 {}: {e}", path.display())),
    }
}

fn write_settings(path: &Path, value: &Value) -> Result<(), String> {
    let mut text =
        serde_json::to_string_pretty(value).map_err(|e| format!("E_SETTINGS 序列化失败: {e}"))?;
    text.push('\n');
    let fp = FsPath::parse(&path.to_string_lossy())?;
    io::atomic_write(&fp, text.as_bytes())
}

fn fsops_hook_command(exe: &str) -> String {
    format!("\"{}\" hook claude", exe)
}

fn is_fsops_hook(command: &str) -> bool {
    command.contains("hook claude") && command.to_ascii_lowercase().contains("fsops")
}

fn ensure_hooks(settings: &mut Value, exe: &str) -> Result<Vec<(String, String)>, String> {
    let obj = settings
        .as_object_mut()
        .ok_or_else(|| "E_SETTINGS 顶层不是对象".to_string())?;
    if !obj.contains_key("hooks") {
        obj.insert("hooks".into(), json!({}));
    }
    let hooks = obj
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "E_SETTINGS hooks 不是对象".to_string())?;
    if !hooks.contains_key("PreToolUse") {
        hooks.insert("PreToolUse".into(), json!([]));
    }
    let pre = hooks
        .get_mut("PreToolUse")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "E_SETTINGS hooks.PreToolUse 不是数组".to_string())?;
    let command = fsops_hook_command(exe);
    let mut out = Vec::new();
    for tool in TOOLS {
        let mut changed = false;
        let mut found = false;
        for entry in pre.iter_mut() {
            if entry.get("matcher").and_then(Value::as_str) != Some(*tool) {
                continue;
            }
            let Some(hook_list) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            for hook in hook_list.iter_mut() {
                if hook.get("type").and_then(Value::as_str) != Some("command") {
                    continue;
                }
                let Some(cmd) = hook.get("command").and_then(Value::as_str) else {
                    continue;
                };
                if is_fsops_hook(cmd) {
                    found = true;
                    if cmd != command {
                        hook["command"] = json!(command);
                        changed = true;
                    }
                }
            }
        }
        if !found {
            pre.push(json!({
                "matcher": tool,
                "hooks": [{"type": "command", "command": command}]
            }));
            out.push((tool.to_string(), "已安装".into()));
        } else if changed {
            out.push((tool.to_string(), "已更新".into()));
        } else {
            out.push((tool.to_string(), "无变化".into()));
        }
    }
    Ok(out)
}

fn remove_hooks(settings: &mut Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(obj) = settings.as_object_mut() else {
        return out;
    };
    let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) else {
        return TOOLS
            .iter()
            .map(|t| (t.to_string(), "未安装".into()))
            .collect();
    };
    let Some(pre) = hooks.get_mut("PreToolUse").and_then(Value::as_array_mut) else {
        return TOOLS
            .iter()
            .map(|t| (t.to_string(), "未安装".into()))
            .collect();
    };
    for tool in TOOLS {
        let was_installed = tool_installed_in(pre, tool);
        pre.retain_mut(|entry| {
            if entry.get("matcher").and_then(Value::as_str) != Some(*tool) {
                return true;
            }
            let Some(hook_list) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            hook_list.retain(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .map(|cmd| !is_fsops_hook(cmd))
                    .unwrap_or(true)
            });
            !hook_list.is_empty()
        });
        let now_installed = tool_installed_in(pre, tool);
        out.push((
            tool.to_string(),
            if was_installed && !now_installed {
                "已移除".into()
            } else {
                "无变化".into()
            },
        ));
    }
    if pre.is_empty() {
        hooks.remove("PreToolUse");
    }
    if hooks.is_empty() {
        obj.remove("hooks");
    }
    out
}

fn tool_installed(settings: &Value, tool: &str) -> bool {
    settings
        .pointer("/hooks/PreToolUse")
        .and_then(Value::as_array)
        .map(|entries| tool_installed_in(entries, tool))
        .unwrap_or(false)
}

fn tool_installed_in(entries: &[Value], tool: &str) -> bool {
    entries.iter().any(|entry| {
        entry.get("matcher").and_then(Value::as_str) == Some(tool)
            && entry
                .get("hooks")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter().any(|hook| {
                        hook.get("command")
                            .and_then(Value::as_str)
                            .map(is_fsops_hook)
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
    })
}

fn check_hooks(settings: &Value) -> Vec<(String, String)> {
    TOOLS
        .iter()
        .map(|tool| {
            (
                tool.to_string(),
                if tool_installed(settings, tool) {
                    "已安装"
                } else {
                    "未安装"
                }
                .into(),
            )
        })
        .collect()
}

/// 给 doctor 用：指定 settings.json 是否完整安装了三个 fsops hook。
pub fn claude_hook_installed(path: &Path) -> bool {
    read_settings(path)
        .ok()
        .flatten()
        .map(|settings| TOOLS.iter().all(|tool| tool_installed(&settings, tool)))
        .unwrap_or(false)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn usage() -> String {
    "fsops harden — 安装宿主级文件工具接管\n\n\
用法:\n\
  fsops harden claude [--path DIR] [--user] [--dry-run]\n\
  fsops harden claude --check [--path DIR] [--user]\n\
  fsops harden claude --remove [--path DIR] [--user]\n\n\
说明:\n\
  claude     安装 Claude Code PreToolUse hook：拦截内置 Read/Grep/Glob，引导到 fs_*\n\
  --user     写入 ~/.claude/settings.json；默认写项目 .claude/settings.json\n\
  --path     指定项目根目录\n\
  --dry-run  只显示将做什么\n\
  --check    检查 hook 是否完整，缺失时退出码 2\n\
  --remove   只移除 fsops 安装的 hook，保留其它配置\n\n\
注意: 图片/旧版 .doc/二进制仍放行内置 Read；文本、DOCX、PDF 要求走 fs_read。"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(tool: &str, path: &str) -> Value {
        json!({"tool_name": tool, "tool_input": {"file_path": path}})
    }

    #[test]
    fn 文本读取被拒绝() {
        assert!(claude_decision(&input("Read", "/repo/src/main.rs")).is_some());
    }

    #[test]
    fn pdf和图片放行() {
        assert!(claude_decision(&input("Read", "/repo/a.doc")).is_none());
        assert!(claude_decision(&input("Read", "/repo/a.PNG")).is_none());
        assert!(claude_decision(&input("Read", "/repo/a.zip")).is_none());
    }

    #[test]
    fn docx和pdf读取被要求走fsops() {
        assert!(claude_decision(&input("Read", "/repo/a.docx")).is_some());
        assert!(claude_decision(&input("Read", "/repo/a.pdf")).is_some());
    }

    #[test]
    fn grep和glob被拒绝() {
        assert!(claude_decision(&json!({"tool_name": "Grep", "tool_input": {}})).is_some());
        assert!(claude_decision(&json!({"tool_name": "Glob", "tool_input": {}})).is_some());
        assert!(claude_decision(&json!({"tool_name": "Bash", "tool_input": {}})).is_none());
    }

    #[test]
    fn 安装hook幂等且保留其他设置() {
        let mut settings = json!({"permissions": {"deny": ["Bash(rm *)"]}});
        let first = ensure_hooks(&mut settings, r"C:\tools\fsops.exe").unwrap();
        assert_eq!(first.len(), 3);
        assert_eq!(settings["permissions"]["deny"][0], "Bash(rm *)");
        let second = ensure_hooks(&mut settings, r"C:\tools\fsops.exe").unwrap();
        assert!(second.iter().all(|(_, s)| s == "无变化"));
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 3);
        assert_eq!(
            check_hooks(&settings)
                .iter()
                .filter(|(_, s)| s == "已安装")
                .count(),
            3
        );
    }

    #[test]
    fn 移除hook保留其他hook() {
        let mut settings = json!({
            "hooks": {"PreToolUse": [
                {"matcher": "Read", "hooks": [{"type": "command", "command": "\"C:\\tools\\fsops.exe\" hook claude"}]},
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "echo ok"}]}
            ]}
        });
        let out = remove_hooks(&mut settings);
        assert!(out.iter().any(|(t, s)| t == "Read" && s == "已移除"));
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["matcher"], "Bash");
    }
}
