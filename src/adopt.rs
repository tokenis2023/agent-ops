//! 宿主采纳层：写入软规则、检查接入状态。
//!
//! 这里刻意只做“软引导”：通过项目规则文件让 Agent 优先选择 fsops。
//! 真正隐藏宿主内置工具需要各宿主自己的 hook / 工具策略，不能只靠 MCP。

use crate::io;
use crate::paths::FsPath;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const BEGIN: &str = "<!-- fsops:begin v1 -->";
const END: &str = "<!-- fsops:end -->";
const BLOCK_VERSION: &str = "1";

const RULE_BODY: &str = "## 文件操作：优先使用 fsops\n\n\
本项目的文件读取、搜索、编辑、写入、元信息查询，优先使用 fsops MCP 工具：\n\n\
- 读取：`fs_read`（替代 read_file / cat / head / tail / Get-Content）\n\
- 搜索内容：`fs_grep`（替代 grep / rg / Select-String）\n\
- 查找文件：`fs_glob`（替代 find / Glob / Get-ChildItem）\n\
- 修改：`fs_edit`（替代 patch / Edit / apply_patch）\n\
- 覆盖或新建：`fs_write`（替代 write_file / Write）\n\
- 元信息：`fs_stat`（替代 stat / ls / Get-Item）\n\
- 执行命令：`fs_exec`（argv 数组，不经过 shell；替代 PowerShell 的 cmd 字符串）\n\
- 文件管理：`fs_file`（copy/move/remove/mkdir/convert/archive/extract）\n\n\
规则：\n\
- 只要 fsops 可用，就不要用宿主内置文件工具或 shell 的 cat/head/tail/grep/find/sed 处理文本。\n\
- DOCX/PDF 正文都可以直接用 `fs_read` 读取（PDF 只读）；`fs_write` 可整体重建 DOCX。\n\
- 局部修改只用 `fs_edit`，不要整文件读回再 `fs_write`；不知道确切原文时用 `pattern`/`replacement`，改完用 `diff=true` 自查。\n\
- 读取方式（单文件、多文件异构、按模式取块、日志末尾）、搜索分页与统计口径、repo map，都写在对应工具的描述里；发调用前先看描述，不要靠试错拼参数。\n\
- 同一次回复里可以并行发起多个只读调用。\n\
- 跑 cargo/git/npm/python 用 `fs_exec` 的 argv 数组，不要用 PowerShell 拼命令行；复制/移动/删除/建目录用 `fs_file`。\n\
- 仅当 fsops 不可用、超时，或目标是图片/不支持的二进制（旧版 `.doc`、压缩包等）时才回退。\n\
- 如果 fsops 工具尚未出现在当前工具列表，先调用宿主的工具发现（如 tool_search）加载 fs_* schema；不要先用内置 Read/Bash 试一遍。\n\
- 如果宿主不支持工具发现或加载失败，直接用 `fsops read/grep/glob/stat/call` CLI，仍然不要退回 cat/grep/find。\n";

const CURSOR_FRONTMATTER: &str =
    "---\ndescription: fsops 文件操作优先级\nglobs:\nalwaysApply: true\n---\n\n";

#[derive(Clone, Copy)]
struct Target {
    key: &'static str,
    file: &'static str,
    frontmatter: &'static str,
}

const TARGETS: &[Target] = &[
    Target {
        key: "agents",
        file: "AGENTS.md",
        frontmatter: "",
    },
    Target {
        key: "claude",
        file: "CLAUDE.md",
        frontmatter: "",
    },
    Target {
        key: "cursor",
        file: ".cursor/rules/fsops.mdc",
        frontmatter: CURSOR_FRONTMATTER,
    },
    Target {
        key: "windsurf",
        file: ".windsurfrules",
        frontmatter: "",
    },
    Target {
        key: "cline",
        file: ".clinerules",
        frontmatter: "",
    },
    Target {
        key: "gemini",
        file: "GEMINI.md",
        frontmatter: "",
    },
    Target {
        key: "copilot",
        file: ".github/copilot-instructions.md",
        frontmatter: "",
    },
];

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Install,
    Remove,
    Check,
}

struct IntegrateOpts {
    root: PathBuf,
    targets: Vec<&'static Target>,
    mode: Mode,
    dry_run: bool,
}

struct Outcome {
    file: &'static str,
    status: String,
    ok: bool,
}

pub fn integrate(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", integrate_usage());
        return 0;
    }
    let opts = match parse_integrate(args) {
        Ok(v) => v,
        Err(e) => {
            println!("E_BAD_ARGS {e}\n\n{}", integrate_usage());
            return 2;
        }
    };

    let mut outcomes = Vec::new();
    for target in &opts.targets {
        match run_target(&opts, target) {
            Ok(o) => outcomes.push(o),
            Err(e) => {
                println!("E_INTEGRATE {e}");
                return 2;
            }
        }
    }

    println!(
        "fsops integrate — 软规则 v{}（{}）",
        BLOCK_VERSION,
        match (opts.mode, opts.dry_run) {
            (Mode::Check, _) => "检查",
            (Mode::Remove, true) => "预演移除",
            (Mode::Remove, false) => "移除",
            (Mode::Install, true) => "预演",
            (Mode::Install, false) => "写入",
        }
    );
    for o in &outcomes {
        println!("  {:<8} {}", o.status, o.file);
    }
    if opts.mode == Mode::Check {
        let failed = outcomes.iter().filter(|o| !o.ok).count();
        if failed > 0 {
            println!("E_RULE_CHECK {failed} 个规则文件缺失或不是当前版本");
            return 2;
        }
        println!("规则检查通过。");
    } else if opts.dry_run {
        println!("预演完成，未修改文件。");
    } else {
        println!("完成。规则只影响会读取这些文件的宿主；强制接管仍需宿主 hook 或专用 profile。");
    }
    0
}

fn parse_integrate(args: &[String]) -> Result<IntegrateOpts, String> {
    let mut rules = false;
    let mut mode = Mode::Install;
    let mut mode_set = false;
    let mut dry_run = false;
    let mut all = false;
    let mut target_arg: Option<String> = None;
    let mut root = std::env::current_dir().map_err(|e| format!("无法获取当前目录: {e}"))?;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--rules" => rules = true,
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
            "--dry-run" => dry_run = true,
            "--all" => all = true,
            "--target" => {
                i += 1;
                target_arg = Some(
                    args.get(i)
                        .ok_or_else(|| "--target 需要值，如 agents,claude 或 all".to_string())?
                        .clone(),
                );
            }
            "--path" => {
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

    // `--check` / `--remove` / `--dry-run` 已隐含"对规则块操作"，
    // 再强制要求 --rules 只是多一次试错；--rules 作为唯一子模式可以推出来。
    let rules = rules || mode != Mode::Install || dry_run;
    if !rules {
        return Err("需要 --rules；用法见 fsops integrate --help".into());
    }
    if mode == Mode::Check && dry_run {
        return Err("--check 与 --dry-run 不能同时使用".into());
    }

    let targets = select_targets(all, target_arg.as_deref())?;
    Ok(IntegrateOpts {
        root,
        targets,
        mode,
        dry_run,
    })
}

fn select_targets(all: bool, target_arg: Option<&str>) -> Result<Vec<&'static Target>, String> {
    if all {
        return Ok(TARGETS.iter().collect());
    }
    let raw = target_arg.unwrap_or("agents,claude");
    if raw.trim() == "all" {
        return Ok(TARGETS.iter().collect());
    }
    let mut selected = Vec::new();
    for key in raw.split(',') {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let target = TARGETS.iter().find(|t| t.key == key).ok_or_else(|| {
            format!(
                "未知 target {key}；可选 {} 或 all",
                TARGETS.iter().map(|t| t.key).collect::<Vec<_>>().join(",")
            )
        })?;
        if !selected.iter().any(|t: &&Target| t.key == target.key) {
            selected.push(target);
        }
    }
    if selected.is_empty() {
        return Err("没有选择任何 target".into());
    }
    Ok(selected)
}

fn run_target(opts: &IntegrateOpts, target: &'static Target) -> Result<Outcome, String> {
    let path = opts.root.join(target.file);
    let current = read_optional(&path)?;

    if opts.mode == Mode::Check {
        let (status, ok) = check_outcome(current.as_deref(), target)?;
        return Ok(Outcome {
            file: target.file,
            status,
            ok,
        });
    }

    if opts.mode == Mode::Remove {
        let Some(text) = current else {
            return Ok(Outcome {
                file: target.file,
                status: "无规则块".into(),
                ok: true,
            });
        };
        let Some(new_text) = remove_block(&text)? else {
            return Ok(Outcome {
                file: target.file,
                status: "无规则块".into(),
                ok: true,
            });
        };
        let status = if opts.dry_run {
            "将移除"
        } else {
            "已移除"
        };
        if !opts.dry_run {
            let only_generated = new_text.trim().is_empty()
                || (!target.frontmatter.is_empty() && new_text.trim() == target.frontmatter.trim());
            if only_generated {
                if let Err(e) = fs::remove_file(&path) {
                    return Err(format!("E_REMOVE 无法删除 {}: {e}", path.display()));
                }
            } else {
                write_text(&path, &new_text)?;
            }
        }
        return Ok(Outcome {
            file: target.file,
            status: status.into(),
            ok: true,
        });
    }

    let existed = current.is_some();
    let old = current.unwrap_or_default();
    let old_status = if old.trim().is_empty() {
        "missing"
    } else {
        block_status(&old)
    };
    let new_text = upsert_block(&old, target)?;
    let changed = old.is_empty() || old != new_text;
    let status = if !changed {
        "无变化"
    } else if !existed {
        if opts.dry_run {
            "将创建"
        } else {
            "已创建"
        }
    } else if old_status == "missing" {
        if opts.dry_run {
            "将追加"
        } else {
            "已追加"
        }
    } else if opts.dry_run {
        "将更新"
    } else {
        "已更新"
    };
    if changed && !opts.dry_run {
        write_text(&path, &new_text)?;
    }
    Ok(Outcome {
        file: target.file,
        status: status.into(),
        ok: true,
    })
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| format!("E_ENCODING {} 不是 UTF-8", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("E_READ 无法读取 {}: {e}", path.display())),
    }
}

fn write_text(path: &Path, text: &str) -> Result<(), String> {
    let raw = path.to_string_lossy().to_string();
    let fp = FsPath::parse(&raw)?;
    io::atomic_write(&fp, text.as_bytes())
}

fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn with_eol(text: &str, eol: &str) -> String {
    if eol == "\n" {
        text.to_string()
    } else {
        text.replace('\n', eol)
    }
}

fn block_text(eol: &str) -> String {
    with_eol(&format!("{BEGIN}\n{RULE_BODY}{END}\n"), eol)
}

fn upsert_block(content: &str, target: &Target) -> Result<String, String> {
    let eol = eol_of(content);
    let block = block_text(eol);
    if content.trim().is_empty() {
        let prefix = with_eol(target.frontmatter, eol);
        return Ok(format!("{prefix}{block}"));
    }

    let starts: Vec<usize> = content
        .match_indices("<!-- fsops:begin")
        .map(|(i, _)| i)
        .collect();
    if starts.len() > 1 {
        return Err(format!("E_RULE_BLOCK {} 含多个起始标记", target.file));
    }
    if let Some(&start) = starts.first() {
        if !content[start..].starts_with(BEGIN) {
            return Err(format!("E_RULE_BLOCK {} 的规则块版本不受支持", target.file));
        }
        let end_rel = content[start..]
            .find(END)
            .ok_or_else(|| format!("E_RULE_BLOCK {} 缺少结束标记", target.file))?;
        let end = start + end_rel + END.len();
        let before = trim_end_newlines(&content[..start]);
        let after = trim_start_newlines(&content[end..]);
        let mut out = String::new();
        out.push_str(before);
        if !out.is_empty() {
            out.push_str(eol);
            out.push_str(eol);
        }
        out.push_str(&block);
        if !after.is_empty() {
            if !out.ends_with(eol) {
                out.push_str(eol);
            }
            out.push_str(after);
            if !out.ends_with('\n') {
                out.push_str(eol);
            }
        }
        return Ok(out);
    }

    let mut out = trim_end_newlines(content).to_string();
    if !out.is_empty() {
        out.push_str(eol);
        out.push_str(eol);
    }
    out.push_str(&block);
    Ok(out)
}

fn remove_block(content: &str) -> Result<Option<String>, String> {
    let starts: Vec<usize> = content
        .match_indices("<!-- fsops:begin")
        .map(|(i, _)| i)
        .collect();
    if starts.is_empty() {
        return Ok(None);
    }
    if starts.len() > 1 {
        return Err("E_RULE_BLOCK 文件含多个起始标记".into());
    }
    let start = starts[0];
    let end_rel = content[start..]
        .find(END)
        .ok_or_else(|| "E_RULE_BLOCK 缺少结束标记".to_string())?;
    let end = start + end_rel + END.len();
    let eol = eol_of(content);
    let before = trim_end_newlines(&content[..start]);
    let after = trim_start_newlines(&content[end..]);
    let mut out = String::new();
    out.push_str(before);
    if !out.is_empty() && !after.is_empty() {
        out.push_str(eol);
        out.push_str(eol);
    }
    out.push_str(after);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(eol);
    }
    Ok(Some(out))
}

fn trim_end_newlines(text: &str) -> &str {
    text.trim_end_matches(['\r', '\n'])
}

fn trim_start_newlines(text: &str) -> &str {
    text.trim_start_matches(['\r', '\n'])
}

/// 判定规则文件当前状态，供 `integrate --check` 与测试共用。
///
/// 只校验标记不够：fsops 升级后规则正文会变，标记仍是 v1 却内容陈旧。
/// 因此标记正确时还要比对"现在写入会得到什么"。
fn check_outcome(current: Option<&str>, target: &Target) -> Result<(String, bool), String> {
    let text = match current {
        None => return Ok(("缺失".to_string(), false)),
        Some(t) => t,
    };
    let st = block_status(text);
    if st != "ok" {
        let label = match st {
            "stale" => "过期",
            "broken" => "损坏",
            _ => "无块",
        };
        return Ok((label.to_string(), false));
    }
    let expected = upsert_block(text, target)?;
    if expected == text {
        Ok(("ok".to_string(), true))
    } else {
        Ok(("过期".to_string(), false))
    }
}

fn rule_checks(root: &Path) -> Vec<RuleCheck> {
    TARGETS
        .iter()
        .map(|target| {
            let path = root.join(target.file);
            let content = read_optional(&path).ok().flatten();
            // 区分「文件不存在」与「文件存在但没有 fsops 块」：
            // 前者可以直接新建，后者必须追加。都报"缺失"会诱导模型整文件覆盖，
            // 把用户已有的 AGENTS.md / CLAUDE.md 内容写没。
            let status = match content.as_deref() {
                None => "missing",
                Some(text) => match block_status(text) {
                    "missing" => "no_block",
                    other => other,
                },
            };
            RuleCheck {
                file: target.file,
                path,
                status,
            }
        })
        .collect()
}

fn block_status(content: &str) -> &'static str {
    let starts = content
        .match_indices("<!-- fsops:begin")
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    match starts.len() {
        0 => "missing",
        1 => {
            let start = starts[0];
            if !content[start..].starts_with(BEGIN) {
                "stale"
            } else if content[start..].find(END).is_some() {
                "ok"
            } else {
                "broken"
            }
        }
        _ => "broken",
    }
}

fn integrate_usage() -> String {
    "fsops integrate — 向项目写入 fsops 软规则\n\n\
用法:\n\
  fsops integrate --rules [--target agents,claude|cursor|all] [--path DIR]\n\
  fsops integrate --rules --remove [--target ...]\n\
  fsops integrate --rules --check [--target ...]\n\n\
说明:\n\
  --rules       必需；写入受管规则块\n\
  --target      默认 agents,claude；可选 cursor,windsurf,cline,gemini,copilot,all\n\
  --dry-run     只显示将做什么，不写文件\n\
  --remove      移除受管规则块，保留用户内容\n\
  --check       检查规则块是否为当前版本，缺失或过期时退出码 2\n\n\
规则块用 <!-- fsops:begin v1 --> 和 <!-- fsops:end --> 标记，可重复执行。"
        .to_string()
}

// ───────────────────────── doctor ─────────────────────────

struct RuleCheck {
    file: &'static str,
    path: PathBuf,
    status: &'static str,
}

struct HostCheck {
    name: &'static str,
    config: Option<PathBuf>,
    registered: bool,
}

pub fn doctor(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", doctor_usage());
        return 0;
    }
    let mut json_out = false;
    let mut check = false;
    let mut root = match std::env::current_dir() {
        Ok(v) => v,
        Err(e) => {
            println!("E_DOCTOR 无法获取当前目录: {e}");
            return 2;
        }
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json_out = true,
            "--check" => check = true,
            "--path" => {
                i += 1;
                match args.get(i) {
                    Some(p) => match FsPath::parse(p) {
                        Ok(fp) => root = fp.native().to_path_buf(),
                        Err(e) => {
                            println!("E_BAD_PATH {e}");
                            return 2;
                        }
                    },
                    None => {
                        println!("E_BAD_ARGS --path 需要目录");
                        return 2;
                    }
                }
            }
            other => {
                println!("E_BAD_ARGS 未知参数 {other}\n\n{}", doctor_usage());
                return 2;
            }
        }
        i += 1;
    }

    let rules = rule_checks(&root);
    let hosts = detect_hosts(&root);
    let any_rule = rules.iter().any(|r| r.status == "ok");
    let any_mcp = hosts.iter().any(|h| h.registered);
    let claude_hook =
        crate::harden::claude_hook_installed(&root.join(".claude").join("settings.json"))
            || home_dir()
                .map(|h| {
                    crate::harden::claude_hook_installed(&h.join(".claude").join("settings.json"))
                })
                .unwrap_or(false);
    let ok = any_rule && any_mcp;
    let enforcement = if claude_hook {
        "hard-claude"
    } else if any_rule {
        "soft"
    } else {
        "none"
    };
    let binary = std::env::current_exe()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| "fsops.exe".into());

    if json_out {
        let payload = json!({
            "version": crate::VERSION,
            "binary": binary,
            "root": root.to_string_lossy().replace('\\', "/"),
            "rules": rules.iter().map(|r| json!({
                "file": r.file,
                "path": r.path.to_string_lossy().replace('\\', "/"),
                "status": r.status,
                "version": if r.status == "ok" { BLOCK_VERSION } else { "" }
            })).collect::<Vec<Value>>(),
            "hosts": hosts.iter().map(|h| json!({
                "name": h.name,
                "config": h.config.as_ref().map(|p| p.to_string_lossy().replace('\\', "/")),
                "registered": h.registered
            })).collect::<Vec<Value>>(),
            "claude_hook": claude_hook,
            "enforcement": enforcement,
            "ok": ok
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
        );
    } else {
        println!("fsops {} — 接入诊断", crate::VERSION);
        println!("binary: {binary}");
        println!("root:   {}", root.to_string_lossy().replace('\\', "/"));
        println!("\n规则文件（软引导）");
        for r in &rules {
            let status = match r.status {
                "ok" => "ok",
                "stale" => "过期",
                "broken" => "损坏",
                "no_block" => "无块",
                _ => "缺失",
            };
            println!("  {:<6} {}", status, r.file);
        }
        println!("\n宿主 MCP");
        for h in &hosts {
            match &h.config {
                Some(path) => println!(
                    "  {:<10} {:<6} {}",
                    h.name,
                    if h.registered {
                        "已注册"
                    } else {
                        "未注册"
                    },
                    path.to_string_lossy().replace('\\', "/")
                ),
                None => println!("  {:<10} 未检测到", h.name),
            }
        }
        println!("\n硬接管");
        println!(
            "  {:<10} {}",
            "Claude Code",
            if claude_hook {
                "已安装"
            } else {
                "未安装"
            }
        );
        println!(
            "\n结论: {}",
            if ok {
                "软引导已生效，且检测到至少一个宿主已注册 fsops。"
            } else if any_rule {
                "规则已写入，但没有检测到已注册 fsops 的宿主。"
            } else if any_mcp {
                "MCP 已注册，但当前目录没有 fsops 规则块。"
            } else {
                "尚未接入：没有规则块，也没有检测到已注册 fsops 的宿主。"
            }
        );
        if claude_hook {
            println!("Claude Code 已启用 PreToolUse 硬接管。");
        }
        if !any_rule {
            println!("下一步: fsops integrate --rules");
        }
        println!("注意: 软引导不能隐藏宿主内置工具；强制接管需要宿主 hook 或专用 profile。");
    }

    if check && !ok {
        println!("E_DOCTOR 检查未通过");
        return 2;
    }
    0
}

fn doctor_usage() -> String {
    "fsops doctor — 检查 fsops 的规则与宿主接入状态\n\n\
用法:\n\
  fsops doctor [--path DIR] [--json] [--check]\n\n\
说明:\n\
  --path DIR   检查指定项目目录，默认当前目录\n\
  --json       输出机器可读 JSON\n\
  --check      没有规则块或没有宿主注册时退出码 2\n\n\
doctor 只做只读检查，不修改任何配置。"
        .to_string()
}

fn detect_hosts(root: &Path) -> Vec<HostCheck> {
    let home = home_dir();
    vec![
        check_host(
            "hermes",
            vec![
                hermes_home().map(|h| h.join("config.yaml")),
                home.as_ref().map(|h| h.join(".hermes").join("config.yaml")),
            ],
            is_yaml_config,
        ),
        check_host(
            "claude",
            vec![
                home.as_ref().map(|h| h.join(".claude.json")),
                Some(root.join(".mcp.json")),
                Some(root.join(".claude").join("settings.local.json")),
                Some(root.join(".claude").join("settings.json")),
            ],
            is_json_config,
        ),
        check_host(
            "codex",
            vec![
                home.as_ref().map(|h| h.join(".codex").join("config.toml")),
                Some(root.join(".codex").join("config.toml")),
            ],
            is_toml_config,
        ),
        check_host(
            "cursor",
            vec![
                home.as_ref().map(|h| h.join(".cursor").join("mcp.json")),
                Some(root.join(".cursor").join("mcp.json")),
            ],
            is_json_config,
        ),
    ]
}

fn check_host<F>(name: &'static str, candidates: Vec<Option<PathBuf>>, has_fsops: F) -> HostCheck
where
    F: Fn(&str) -> bool,
{
    let mut config = None;
    let mut registered = false;
    for path in candidates.into_iter().flatten() {
        if !path.exists() {
            continue;
        }
        if config.is_none() {
            config = Some(path.clone());
        }
        if let Ok(text) = fs::read_to_string(&path) {
            if has_fsops(&text) {
                registered = true;
            }
        }
    }
    HostCheck {
        name,
        config,
        registered,
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn hermes_home() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("HERMES_HOME") {
        return Some(PathBuf::from(v));
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|p| p.join("hermes"))
}

fn is_json_config(text: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    v.get("mcpServers").and_then(|m| m.get("fsops")).is_some()
        || v.get("servers").and_then(|m| m.get("fsops")).is_some()
        || v.get("mcp")
            .and_then(|m| m.get("servers"))
            .and_then(|m| m.get("fsops"))
            .is_some()
}

fn is_toml_config(text: &str) -> bool {
    text.contains("[mcp_servers.fsops]") || text.contains("[mcp.servers.fsops]")
}

fn is_yaml_config(text: &str) -> bool {
    let mut in_section = false;
    let mut base_indent = 0usize;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if trimmed == "mcp_servers:" {
            in_section = true;
            base_indent = indent;
            continue;
        }
        if in_section {
            if indent <= base_indent {
                in_section = false;
                continue;
            }
            if trimmed.starts_with("fsops:") {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fsops-adopt-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn 规则块可重复写入且不重复() {
        let target = &TARGETS[0];
        let first = upsert_block("# 项目\n", target).unwrap();
        let second = upsert_block(&first, target).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.matches(BEGIN).count(), 1);
        assert!(first.contains("替代 read_file"));
        assert!(first.starts_with("# 项目\n"));
    }

    #[test]
    fn 规则正文覆盖_pdf_与正则编辑() {
        // 规则正文是 Agent 每轮都会读的短引导，必须和 initialize 同路由：
        // PDF 正文走 fs_read；正则编辑与 diff 自查要写明，否则学不到新用法。
        // 参数级事实（reads/focus/map/count 口径）住在工具描述里；
        // 规则正文只负责策略并指向描述，否则两处 ~3KB 文本要每轮付两次。
        assert!(RULE_BODY.contains("DOCX/PDF"), "{RULE_BODY}");
        assert!(RULE_BODY.contains("fs_read"), "{RULE_BODY}");
        assert!(
            !RULE_BODY.contains("图片/PDF") && !RULE_BODY.contains("PDF/二进制"),
            "PDF 不应与图片/二进制一起回退: {RULE_BODY}"
        );
        assert!(
            RULE_BODY.contains("pattern") && RULE_BODY.contains("replacement"),
            "缺少正则编辑引导: {RULE_BODY}"
        );
        assert!(
            RULE_BODY.contains("diff=true"),
            "缺少 diff 自查引导: {RULE_BODY}"
        );
        assert!(RULE_BODY.contains("并行"), "缺少并行读取引导: {RULE_BODY}");
        // 参数事实不在规则正文重复，但必须有指向，否则模型不会主动去看描述。
        for pointer in ["读取方式", "统计口径", "repo map", "工具的描述"] {
            assert!(
                RULE_BODY.contains(pointer),
                "规则正文缺少描述指针 {pointer}: {RULE_BODY}"
            );
        }
        assert!(
            RULE_BODY.contains("fs_exec") && RULE_BODY.contains("fs_file"),
            "缺少执行/文件管理引导: {RULE_BODY}"
        );
    }

    #[test]
    fn 规则正文不重复参数枚举() {
        // P0 实测：instructions ↔ RULE_BODY 字符级重合 47%。
        // 两份都要进上下文，参数枚举只能住一处（工具描述）。
        for duplicated in [
            "reads=[",
            "focus=",
            "map=true",
            "block=true",
            "默认完整统计",
        ] {
            assert!(
                !RULE_BODY.contains(duplicated),
                "规则正文又抄回了 {duplicated}: {RULE_BODY}"
            );
        }
    }

    #[test]
    fn 更新规则块保留用户内容() {
        let target = &TARGETS[0];
        let old = format!("# 项目\n\n{BEGIN}\n旧规则\n{END}\n\n## 其他\n内容\n");
        let new = upsert_block(&old, target).unwrap();
        assert!(new.contains("## 其他\n内容"));
        assert!(!new.contains("旧规则"));
        assert_eq!(new.matches(BEGIN).count(), 1);
    }

    #[test]
    fn 移除规则块保留用户内容() {
        let old = format!("# 项目\n\n{BEGIN}\n规则\n{END}\n\n## 其他\n内容\n");
        let new = remove_block(&old).unwrap().unwrap();
        assert!(!new.contains("fsops:begin"));
        assert!(new.contains("# 项目"));
        assert!(new.contains("## 其他"));
    }

    #[test]
    fn 缺少结束标记时拒绝() {
        let old = format!("# 项目\n{BEGIN}\n规则\n");
        assert!(upsert_block(&old, &TARGETS[0]).is_err());
        assert!(remove_block(&old).is_err());
    }

    #[test]
    fn 保留_crlf() {
        let target = &TARGETS[0];
        let text = upsert_block("# 项目\r\n", target).unwrap();
        assert!(text.contains("\r\n"));
        assert!(!text.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn 新文件写入_cursor_前置元数据() {
        let dir = temp_dir("cursor");
        let opts = IntegrateOpts {
            root: dir.clone(),
            targets: vec![&TARGETS[2]],
            mode: Mode::Install,
            dry_run: false,
        };
        let outcome = run_target(&opts, &TARGETS[2]).unwrap();
        assert_eq!(outcome.status, "已创建");
        let text = fs::read_to_string(dir.join(".cursor/rules/fsops.mdc")).unwrap();
        assert!(text.starts_with("---\ndescription:"));
        assert!(text.contains(BEGIN));
        let remove = IntegrateOpts {
            root: dir.clone(),
            targets: vec![&TARGETS[2]],
            mode: Mode::Remove,
            dry_run: false,
        };
        run_target(&remove, &TARGETS[2]).unwrap();
        assert!(!dir.join(".cursor/rules/fsops.mdc").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn doctor_区分文件缺失与无规则块() {
        // 都报"缺失"会诱导模型用 fs_write 新建 AGENTS.md，
        // 把用户已有的内容整文件覆盖。
        let dir = std::env::temp_dir().join(format!("fsops-rulechk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("AGENTS.md"), "# 用户自己的规则\n\n原有内容\n").unwrap();
        fs::write(dir.join("CLAUDE.md"), format!("{BEGIN}\n{END}\n")).unwrap();

        let checks = rule_checks(&dir);
        let _ = fs::remove_dir_all(&dir);

        let agents = checks.iter().find(|c| c.file == "AGENTS.md").unwrap();
        assert_eq!(agents.status, "no_block", "文件存在但无块应为 no_block");
        // 原文件内容仍在
        let claude = checks.iter().find(|c| c.file == "CLAUDE.md").unwrap();
        assert_eq!(claude.status, "ok");
        let missing = checks.iter().find(|c| c.file == "GEMINI.md").unwrap();
        assert_eq!(missing.status, "missing", "文件不存在才是 missing");
    }

    #[test]
    fn doctor_规则状态识别() {
        assert_eq!(block_status("没有规则"), "missing");
        assert_eq!(block_status(BEGIN), "broken");
        assert_eq!(block_status(&format!("{BEGIN}\n{END}\n")), "ok");
        assert_eq!(block_status("<!-- fsops:begin -->\n旧"), "stale");
        assert_eq!(
            block_status(&format!("{BEGIN}\n{END}\n{BEGIN}\n{END}")),
            "broken"
        );
    }

    #[test]
    fn check_能识别正文过期() {
        // 标记仍是 v1、但正文被删了一行时，只校验标记会误报 ok。
        let target = TARGETS.iter().find(|t| t.key == "agents").unwrap();
        let fresh = upsert_block("", target).unwrap();
        assert_eq!(check_outcome(Some(&fresh), target).unwrap().0, "ok");

        let stale = fresh.replace("- 同一次回复里可以并行发起多个只读调用。\n", "");
        assert_ne!(stale, fresh, "测试前提：正文确实被改动");
        let (label, ok) = check_outcome(Some(&stale), target).unwrap();
        assert_eq!(label, "过期");
        assert!(!ok);

        assert_eq!(check_outcome(None, target).unwrap().0, "缺失");
        assert_eq!(
            check_outcome(Some("用户自己的规则\n"), target).unwrap().0,
            "无块"
        );
    }

    #[test]
    fn integrate_check_remove_无需显式_rules() {
        // --check / --remove 已表明要对规则块操作，再强制 --rules 只是多一次试错。
        assert!(matches!(
            parse_integrate(&["--check".to_string()]).unwrap().mode,
            Mode::Check
        ));
        assert!(matches!(
            parse_integrate(&["--remove".to_string()]).unwrap().mode,
            Mode::Remove
        ));
        // 裸 integrate 仍应提示需要 --rules
        assert!(parse_integrate(&[]).is_err());
    }

    #[test]
    fn integrate_path_支持_posix_盘符() {
        if cfg!(windows) {
            let args = vec![
                "--rules".to_string(),
                "--path".to_string(),
                "/c/tmp/fsops".to_string(),
            ];
            let opts = parse_integrate(&args).unwrap();
            assert_eq!(opts.root, PathBuf::from(r"C:\tmp\fsops"));
        }
    }

    #[test]
    fn 识别_json_toml_yaml_中的_fsops() {
        assert!(is_json_config(
            r#"{"mcpServers":{"fsops":{"command":"fsops"}}}"#
        ));
        assert!(is_toml_config("[mcp_servers.fsops]\ncommand = \"fsops\"\n"));
        assert!(is_yaml_config(
            "mcp_servers:\n  fsops:\n    command: fsops\n    args:\n      - mcp\n"
        ));
        assert!(!is_yaml_config("other:\n  fsops:\n    command: fsops\n"));
    }
}
