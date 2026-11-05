//! 工具层：6 个工具的定义与执行。
//!
//! 设计原则：工具面窄（少而准），返回短（带续读提示），错误直给（带候选行）。
//! 所有工具名以 `fs_` 开头，避免和宿主自带文件工具撞名。

#![allow(clippy::items_after_test_module)]

use crate::edit::{self, EditSpec};
use crate::io;
use crate::paths::FsPath;
use crate::render::{self, RenderOpts};
use crate::search::{self, GrepOpts, GrepOutput};
use crate::walk::{self, WalkOpts};
use serde_json::{json, Value};

pub fn tool_defs() -> Vec<Value> {
    vec![
        json!({
            "name": "fs_read",
            "description": "读取文件（POSIX 路径）。默认只返回前 12KB 并给出续读方式；支持行范围、多文件、自动识别 GBK/UTF-16/BOM 与 CRLF。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "文件路径，如 /d/repo/app/src/main.rs"},
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "一次读多个文件（最多 20 个）"},
                    "start": {"type": "integer", "description": "起始行（1 基）"},
                    "end": {"type": "integer", "description": "结束行（含）"},
                    "ranges": {
                        "type": "array",
                        "description": "单文件多段行区间，如 [[1,2],[8,9]]；优先于 start/end",
                        "items": {
                            "type": "array",
                            "items": {"type": "integer", "minimum": 1},
                            "minItems": 2,
                            "maxItems": 2
                        }
                    },
                    "budget": {"type": "integer", "description": "最多返回字节数，默认 12288"},
                    "max_line": {"type": "integer", "description": "每行最多字符数，默认 240；0 表示不截断长行"}
                },
                "required": []
            }
        }),
        json!({
            "name": "fs_edit",
            "description": "精确替换文本（old→new）。同文件多处用 edits，多文件用 files；全部预检通过才写入。自动处理 CRLF/LF 差异并保留原编码；失败时返回最接近的行号与内容。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "单文件路径；批量时改用 files"},
                    "edits": {
                        "type": "array",
                        "description": "单文件的修改列表",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old": {"type": "string", "description": "原文片段（需唯一，除非 all=true）"},
                                "new": {"type": "string", "description": "替换内容，空串表示删除"},
                                "all": {"type": "boolean", "description": "替换全部出现位置"}
                            },
                            "required": ["old"]
                        }
                    },
                    "files": {
                        "type": "array",
                        "description": "多文件批量编辑；全部文件预检成功后逐文件原子写入，最多 50 个",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "edits": {"type": "array", "items": {"type": "object"}},
                                "expect_hash": {"type": "string", "description": "该文件的并发保护指纹"}
                            },
                            "required": ["path", "edits"]
                        }
                    },
                    "dry_run": {"type": "boolean", "description": "只校验不落盘"},
                    "normalize_eol": {"type": "boolean", "description": "混合换行时显式归一为 LF"},
                    "expect_hash": {"type": "string", "description": "单文件并发保护：文件已变则拒绝写入"}
                },
                "required": []
            }
        }),
        json!({
            "name": "fs_write",
            "description": "写入文件（原子替换、自动建目录、保留原有编码与换行风格）。适合新建文件或整体覆盖。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "normalize_eol": {"type": "boolean", "description": "新文件或原文件为混合换行时，显式归一为 LF；已有稳定文件始终沿用原换行"},
                    "expect_hash": {"type": "string", "description": "并发保护：文件已变则拒绝覆盖"}
                },
                "required": ["path", "content"]
            }
        }),
        json!({
            "name": "fs_glob",
            "description": "按 glob 找文件（尊重 .gitignore，自动跳过 node_modules/target 等）。默认只给路径，detail=true 附大小与时间。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "搜索根目录，默认当前目录"},
                    "pattern": {"type": "string", "description": "如 **/*.rs，多个用逗号分隔；省略表示全部"},
                    "dirs": {"type": "boolean", "description": "是否包含目录"},
                    "detail": {"type": "boolean", "description": "是否附大小与修改时间（默认只给路径，省 token）"},
                    "limit": {"type": "integer", "description": "最多返回条数，默认 200；达到后提前停止"},
                    "budget": {"type": "integer", "description": "最终响应最多字节数，默认 8192"}
                },
                "required": []
            }
        }),
        json!({
            "name": "fs_grep",
            "description": "正则流式搜索文件内容并按文件稳定分组。output 可选 files/count/matches/snippets；截断时返回 next_offset。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Rust/PCRE 风格正则"},
                    "path": {"type": "string", "description": "搜索根目录，默认当前目录"},
                    "glob": {"type": "string", "description": "只看匹配的文件，如 **/*.ts"},
                    "ignore_case": {"type": "boolean"},
                    "output": {
                        "type": "string",
                        "enum": ["files", "count", "matches", "snippets"],
                        "description": "files 只给路径；count 给路径和本页可见命中数；matches 给命中行；snippets 默认带 2 行上下文"
                    },
                    "context": {"type": "integer", "description": "上下文行数（最多 5）；matches 默认 0，snippets 默认 2"},
                    "limit": {"type": "integer", "description": "本页最多命中数，默认 100；必须大于 0"},
                    "max_per_file": {"type": "integer", "description": "每个文件最多返回的命中数，默认不限制；省略部分会在尾注说明"},
                    "offset": {"type": "integer", "description": "跳过前 N 个命中，用于 next_offset 续取"},
                    "parallel": {"type": "boolean", "description": "大仓库并行扫描；输出与串行一致，默认 false"},
                    "budget": {"type": "integer", "description": "最终响应最多字节数，默认 8192"},
                    "time_budget_ms": {"type": "integer", "description": "文件与目录遍历总时间预算，默认 5000 毫秒"}
                },
                "required": ["pattern"]
            }
        }),
        json!({
            "name": "fs_stat",
            "description": "查看路径元信息：类型、大小、修改时间、换行风格、编码、内容指纹。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "paths": {"type": "array", "items": {"type": "string"}},
                    "detail": {
                        "type": "string",
                        "enum": ["meta", "hash", "full"],
                        "description": "meta 只查元信息不读内容；hash 读一次给指纹；full 额外统计行数，默认 full"
                    }
                },
                "required": []
            }
        }),
    ]
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn arg_usize(args: &Value, key: &str, default: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(default)
}

fn arg_bool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

pub fn call(name: &str, args: &Value) -> String {
    match name {
        "fs_read" => read(args),
        "fs_edit" => edit_tool(args),
        "fs_write" => write(args),
        "fs_glob" => glob_tool(args),
        "fs_grep" => grep_tool(args),
        "fs_stat" => stat(args),
        other => format!("E_UNKNOWN_TOOL 未知工具 {other}"),
    }
}

fn read(args: &Value) -> String {
    let budget = arg_usize(args, "budget", render::DEFAULT_BUDGET);
    let mut targets: Vec<String> = Vec::new();
    if let Some(p) = arg_str(args, "path") {
        targets.push(p.to_string());
    }
    if let Some(arr) = args.get("paths").and_then(Value::as_array) {
        for v in arr.iter().filter_map(Value::as_str) {
            targets.push(v.to_string());
        }
    }
    if targets.is_empty() {
        return render::truncate_utf8("E_BAD_ARGS 需要 path 或 paths", budget).to_string();
    }
    targets.truncate(20);

    let start = arg_usize(args, "start", 1);
    let end = args
        .get("end")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(usize::MAX);
    let max_line = arg_usize(args, "max_line", render::MAX_LINE_CHARS);
    let ranges = if let Some(values) = args.get("ranges").and_then(Value::as_array) {
        let mut parsed = Vec::with_capacity(values.len());
        for value in values {
            let pair = value.as_array();
            let range = pair.and_then(|pair| {
                if pair.len() == 2 {
                    Some((pair[0].as_u64()? as usize, pair[1].as_u64()? as usize))
                } else {
                    None
                }
            });
            match range {
                Some((from, to)) if from > 0 && from <= to => parsed.push((from, to)),
                _ => {
                    return render::truncate_utf8(
                        "E_BAD_ARGS ranges 每项必须是 [起始行,结束行]，且 1<=起始行<=结束行",
                        budget,
                    )
                    .to_string()
                }
            }
        }
        if parsed.is_empty() {
            return render::truncate_utf8("E_BAD_ARGS ranges 不能为空", budget).to_string();
        }
        parsed
    } else {
        vec![(start, end)]
    };

    let mut out = String::new();
    for (i, t) in targets.iter().enumerate() {
        // 每个后续结果（包括错误）都与前一文件正文分行。
        if i > 0 && !out.ends_with('\n') {
            out.push('\n');
        }
        let fp = match FsPath::parse(t) {
            Ok(p) => p,
            Err(e) => {
                out.push_str(&format!("E_BAD_PATH {e}\n"));
                continue;
            }
        };
        // 超大文件流式读取：只加载一个连续行区间，hash 与总行数边扫边算。
        if let Ok(m) = io::meta(&fp) {
            if m.kind == "file" && m.size > 32 * 1024 * 1024 {
                if args.get("ranges").is_some() {
                    out.push_str(&format!(
                        "E_TOO_LARGE {} {}，超大文件一次只支持一个连续 start/end 区间\n",
                        fp.display(),
                        io::human_size(m.size)
                    ));
                    continue;
                }
                if end == usize::MAX {
                    out.push_str(&format!(
                        "E_TOO_LARGE {} {}，请指定 end 后再按区间读取\n",
                        fp.display(),
                        io::human_size(m.size)
                    ));
                    continue;
                }
                let large_start = start.max(1);
                let window = match io::read_large_window(&fp, large_start, end) {
                    Ok(window) => window,
                    Err(e) => {
                        out.push_str(&format!("{e}\n"));
                        continue;
                    }
                };
                if window.lines == 0 {
                    out.push_str(&format!(
                        "E_RANGE {} 区间超出文件范围（共{}行）\n",
                        fp.display(),
                        window.total_lines
                    ));
                    continue;
                }
                if targets.len() > 1 {
                    if i > 0 {
                        out.push('\n');
                    }
                    out.push_str(&format!("== {} ", fp.display()));
                } else {
                    out.push_str(&format!("{} ", fp.display()));
                }
                out.push_str(&format!(
                    "{} {} {} {}行 hash={}\n",
                    window.encoding,
                    window.eol.as_str(),
                    io::human_size(window.size),
                    window.total_lines,
                    window.hash
                ));
                let want = if targets.len() > 1 {
                    (budget / targets.len().max(1)).saturating_sub(usize::from(i > 0))
                } else {
                    budget
                };
                let target = render::converge_budget(want, |body_budget| {
                    let r = render::render_window(
                        &window.text,
                        RenderOpts {
                            start: 1,
                            end: window.lines,
                            budget: body_budget,
                            max_line,
                            line_offset: window.first_line.saturating_sub(1),
                            ..Default::default()
                        },
                    );
                    if r.body.is_empty() && !r.continuation.is_empty() {
                        return format!(
                            "E_LINE_TOO_LONG 第 {} 行无法放入当前响应；请增大 budget 或调小 max_line",
                            window.first_line + r.continuation[0].0.saturating_sub(1)
                        );
                    }
                    let mut text = r.body;
                    if r.last_line < r.total_lines {
                        let absolute_last = window.first_line + r.last_line.saturating_sub(1);
                        text.push_str(&format!(
                            "… 至 {}/{} 行 | 续读: start={} end={}\n",
                            absolute_last,
                            window.total_lines,
                            absolute_last + 1,
                            end
                        ));
                    }
                    text
                });
                if target.contains("续读:") && !target.contains(" end=") {
                    out.push_str("E_BUDGET 续读参数被预算截断；请增大 budget\n");
                } else {
                    out.push_str(&target);
                }
                continue;
            }
        }
        let snapshot = match io::read_snapshot(&fp) {
            Ok(s) => s,
            Err(e) => {
                out.push_str(&format!("{e}\n"));
                continue;
            }
        };
        let doc = &snapshot.doc;
        let total_lines = doc.text.split('\n').count();
        let mut header = if targets.len() > 1 {
            format!("== {} ", fp.display())
        } else {
            format!("{} ", fp.display())
        };
        header.push_str(&format!(
            "{} {} {} {}行 hash={}\n",
            doc.encoding,
            doc.eol.as_str(),
            io::human_size(doc.size),
            total_lines,
            snapshot.hash
        ));

        let want = if targets.len() > 1 {
            (budget / targets.len().max(1)).saturating_sub(usize::from(i > 0))
        } else {
            budget
        };
        let target = render::converge_budget(want, |body_budget| {
            let mut target = header.clone();
            let r = render::render_ranges(
                &doc.text,
                &ranges,
                RenderOpts {
                    budget: body_budget,
                    max_line,
                    ..Default::default()
                },
            );
            if r.body.is_empty() && !r.continuation.is_empty() {
                return format!(
                    "E_LINE_TOO_LONG 第 {} 行无法放入当前响应；请增大 budget 或调小 max_line",
                    r.continuation[0].0
                );
            }
            if r.body.is_empty() && r.continuation.is_empty() {
                return format!(
                    "E_RANGE {} 区间超出文件范围（共{}行）",
                    fp.display(),
                    r.total_lines
                );
            }
            target.push_str(&r.body);
            if !r.continuation.is_empty() {
                target.push_str(&format!(
                    "… {}-{}/{} 行 | 续读: fs_read 同 path ",
                    r.last_line.saturating_sub(9).max(1),
                    r.last_line,
                    r.total_lines,
                ));
                if args.get("ranges").is_some() {
                    target.push_str(&format!(
                        "ranges={}\n",
                        serde_json::to_string(&r.continuation).unwrap()
                    ));
                } else if args.get("end").and_then(Value::as_u64).is_some() {
                    target.push_str(&format!(
                        "start={} end={}\n",
                        r.continuation[0].0, r.continuation[0].1
                    ));
                } else {
                    target.push_str(&format!("start={}\n", r.continuation[0].0));
                }
            }
            target
        });
        out.push_str(&target);
    }
    render::truncate_utf8(out.trim_end(), budget).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn 临时文本文件(content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "fsops-read-{}-{}.txt",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, content).unwrap();
        path
    }

    fn 临时目录(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "fsops-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn 单文件多区间读取() {
        let path = 临时文本文件("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let output = read(&json!({
            "path": posix_path,
            "ranges": [[1, 2], [8, 9]]
        }));
        fs::remove_file(path).unwrap();

        assert!(output.contains("1|1\n2|2\n"), "{output}");
        assert!(output.contains("8|8\n9|9"), "{output}");
        assert!(!output.contains("3|3\n"), "{output}");
        assert!(tool_defs()[0]["inputSchema"]["properties"]["ranges"].is_object());
    }

    #[test]
    fn 零长行限制返回完整长行() {
        let long = "x".repeat(500);
        let path = 临时文本文件(&long);
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let output = read(&json!({
            "path": posix_path,
            "max_line": 0
        }));
        fs::remove_file(path).unwrap();

        assert!(output.contains(&format!("1|{long}")), "输出未保留完整长行");
        assert!(!output.contains("…(+"), "{output}");
        assert!(tool_defs()[0]["inputSchema"]["properties"]["max_line"].is_object());
    }

    #[test]
    fn 总响应预算包含头部正文和续读提示() {
        let content = (1..=80)
            .map(|i| format!("第{i}行-中文🙂-{}", "内容".repeat(8)))
            .collect::<Vec<_>>()
            .join("\n");
        let path = 临时文本文件(&content);
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let output = read(&json!({
            "path": posix_path,
            "budget": 400,
            "max_line": 0
        }));
        fs::remove_file(path).unwrap();

        assert!(output.len() <= 400, "实际 {} 字节: {output}", output.len());
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
        assert!(output.contains("hash="), "缺少头部元信息: {output}");
        assert!(output.contains("续读: fs_read"), "缺少续读提示: {output}");
        assert!(output.contains("start="), "缺少续读参数: {output}");
    }

    #[test]
    fn 极小预算下错误响应也不超限() {
        for budget in 0..24 {
            let output = read(&json!({"budget": budget}));
            assert!(
                output.len() <= budget,
                "budget={budget}，实际 {} 字节: {output}",
                output.len()
            );
            assert!(std::str::from_utf8(output.as_bytes()).is_ok());
        }
    }

    #[test]
    fn 多文件编辑预检失败时所有文件都不变() {
        let dir = 临时目录("edit-multi-stale");
        let first = dir.join("first.txt");
        let second = dir.join("second.txt");
        fs::write(&first, "alpha\n").unwrap();
        fs::write(&second, "beta\n").unwrap();
        let output = edit_tool(&json!({
            "files": [
                {
                    "path": first.to_string_lossy(),
                    "edits": [{"old": "alpha", "new": "changed"}]
                },
                {
                    "path": second.to_string_lossy(),
                    "expect_hash": "deadbeefdeadbeef",
                    "edits": [{"old": "beta", "new": "changed"}]
                }
            ]
        }));
        let first_body = fs::read_to_string(&first).unwrap();
        let second_body = fs::read_to_string(&second).unwrap();
        fs::remove_dir_all(dir).unwrap();

        assert!(output.starts_with("E_BATCH"), "{output}");
        assert_eq!(first_body, "alpha\n", "预检失败仍修改了第一个文件");
        assert_eq!(second_body, "beta\n", "预检失败仍修改了第二个文件");
    }

    #[test]
    fn 多文件编辑一次写入并返回摘要() {
        let dir = 临时目录("edit-multi-ok");
        let first = dir.join("first.txt");
        let second = dir.join("second.txt");
        fs::write(&first, "alpha\n").unwrap();
        fs::write(&second, "beta\ngamma\n").unwrap();
        let output = edit_tool(&json!({
            "files": [
                {
                    "path": first.to_string_lossy(),
                    "edits": [{"old": "alpha", "new": "changed"}]
                },
                {
                    "path": second.to_string_lossy(),
                    "edits": [
                        {"old": "beta", "new": "changed"},
                        {"old": "gamma", "new": "removed"}
                    ]
                }
            ]
        }));
        let first_body = fs::read_to_string(&first).unwrap();
        let second_body = fs::read_to_string(&second).unwrap();
        fs::remove_dir_all(dir).unwrap();

        assert!(output.starts_with("ok 2 个文件"), "{output}");
        assert_eq!(first_body, "changed\n");
        assert_eq!(second_body, "changed\nremoved\n");
    }

    #[test]
    fn 多文件dry_run不落盘() {
        let dir = 临时目录("edit-multi-dry");
        let first = dir.join("first.txt");
        let second = dir.join("second.txt");
        fs::write(&first, "alpha\n").unwrap();
        fs::write(&second, "beta\n").unwrap();
        let output = edit_tool(&json!({
            "dry_run": true,
            "files": [
                {"path": first.to_string_lossy(), "edits": [{"old": "alpha", "new": "x"}]},
                {"path": second.to_string_lossy(), "edits": [{"old": "beta", "new": "y"}]}
            ]
        }));
        let first_body = fs::read_to_string(&first).unwrap();
        let second_body = fs::read_to_string(&second).unwrap();
        fs::remove_dir_all(dir).unwrap();

        assert!(output.starts_with("ok dry_run"), "{output}");
        assert_eq!(first_body, "alpha\n");
        assert_eq!(second_body, "beta\n");
    }

    #[test]
    fn 多文件编辑拒绝重复路径() {
        let dir = 临时目录("edit-multi-dup");
        let file = dir.join("same.txt");
        fs::write(&file, "alpha\n").unwrap();
        let path = file.to_string_lossy();
        let output = edit_tool(&json!({
            "files": [
                {"path": path, "edits": [{"old": "alpha", "new": "one"}]},
                {"path": path, "edits": [{"old": "alpha", "new": "two"}]}
            ]
        }));
        let body = fs::read_to_string(&file).unwrap();
        fs::remove_dir_all(dir).unwrap();

        assert!(output.starts_with("E_BATCH"), "{output}");
        assert_eq!(body, "alpha\n");
    }

    #[test]
    fn 超大文件按区间读取() {
        use std::io::Write;
        let dir = 临时目录("read-large");
        let path = dir.join("large.txt");
        let line = format!("{}\n", "x".repeat(1023));
        let mut file = fs::File::create(&path).unwrap();
        for _ in 0..34_000 {
            file.write_all(line.as_bytes()).unwrap();
        }
        file.sync_all().unwrap();
        drop(file);
        assert!(fs::metadata(&path).unwrap().len() > 32 * 1024 * 1024);

        let output = read(&json!({
            "path": path.to_string_lossy(),
            "start": 100,
            "end": 110,
            "budget": 6000
        }));
        let small = read(&json!({
            "path": path.to_string_lossy(),
            "start": 100,
            "end": 110,
            "budget": 2048
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(output.contains("100|"), "{output}");
        assert!(output.contains("110|"), "{output}");
        assert!(!output.contains("E_TOO_LARGE"), "{output}");
        assert!(output.len() <= 6000, "实际 {} 字节", output.len());
        assert!(
            small.starts_with("E_BUDGET") || small.contains(" end=110"),
            "续读参数被截断: {small}"
        );
        assert!(small.len() <= 2048, "实际 {} 字节", small.len());
    }

    #[test]
    fn 读取区间完全越界时给出错误() {
        let path = 临时文本文件("one\ntwo\nthree\n");
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let output = read(&json!({"path": posix_path, "start": 99, "end": 120}));
        fs::remove_file(path).unwrap();

        assert!(output.starts_with("E_RANGE"), "{output}");
        assert!(
            output.contains("超出文件范围") && output.contains("共4行"),
            "{output}"
        );
    }

    #[test]
    fn glob_输出受预算限制() {
        let dir = 临时目录("glob-budget");
        for index in 0..30 {
            fs::write(
                dir.join(format!("文件名-{index:02}-{}.txt", "长".repeat(12))),
                "x",
            )
            .unwrap();
        }
        let output = glob_tool(&json!({
            "path": dir.to_string_lossy(),
            "pattern": "**/*.txt",
            "limit": 30,
            "budget": 300
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(output.len() <= 300, "实际 {} 字节", output.len());
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
        assert!(output.contains("截断"), "缺少截断提示: {output}");
    }

    #[test]
    fn stat_meta模式只返回元信息() {
        let dir = 临时目录("stat-meta");
        let file = dir.join("a.txt");
        fs::write(&file, "alpha\nbeta\n").unwrap();
        let output = stat(&json!({
            "path": file.to_string_lossy(),
            "detail": "meta"
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(output.contains("file"), "{output}");
        assert!(
            !output.contains("hash="),
            "meta 模式不应读内容算 hash: {output}"
        );
        assert!(
            !output.contains("行"),
            "meta 模式不应读内容算行数: {output}"
        );
    }

    #[test]
    fn 写入已有文件时沿用原有换行() {
        let dir = 临时目录("write-preserve-eol");
        let crlf = dir.join("crlf.txt");
        let lf = dir.join("lf.txt");
        fs::write(&crlf, b"one\r\ntwo\r\n").unwrap();
        fs::write(&lf, b"one\ntwo\n").unwrap();

        let crlf_out = write(&json!({
            "path": crlf.to_string_lossy(),
            "content": "alpha\nbeta\n"
        }));
        let lf_out = write(&json!({
            "path": lf.to_string_lossy(),
            "content": "alpha\r\nbeta\r\n"
        }));
        let crlf_bytes = fs::read(&crlf).unwrap();
        let lf_bytes = fs::read(&lf).unwrap();
        fs::remove_dir_all(dir).unwrap();

        assert!(crlf_out.starts_with("ok"), "{crlf_out}");
        assert!(lf_out.starts_with("ok"), "{lf_out}");
        assert_eq!(crlf_bytes, b"alpha\r\nbeta\r\n");
        assert_eq!(lf_bytes, b"alpha\nbeta\n");
    }

    #[test]
    fn 写已有稳定换行文件时接受混合内容并归一到文件风格() {
        let dir = 临时目录("write-normalize-existing");
        let file = dir.join("lf.txt");
        fs::write(&file, b"one\ntwo\n").unwrap();

        let output = write(&json!({
            "path": file.to_string_lossy(),
            "content": "alpha\r\nbeta\n"
        }));
        let body = fs::read(&file).unwrap();
        fs::remove_dir_all(dir).unwrap();

        assert!(output.starts_with("ok"), "{output}");
        assert_eq!(body, b"alpha\nbeta\n");
    }

    #[test]
    fn glob_以单文件为根时只返回该文件() {
        let dir = 临时目录("glob-single-root");
        let wanted = dir.join("wanted.txt");
        fs::write(&wanted, "x").unwrap();
        fs::write(dir.join("sibling.txt"), "x").unwrap();
        let root = FsPath::parse(&wanted.to_string_lossy()).unwrap();

        let got = crate::walk::glob(&root, "", &WalkOpts::default()).unwrap();
        fs::remove_dir_all(dir).unwrap();

        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "wanted.txt");
    }

    #[test]
    fn 多文件后续错误不粘连前一文件正文() {
        let first = 临时文本文件("first\n");
        let second = 临时文本文件("second\n");
        let missing = first.with_extension("missing");
        let paths = vec![
            first.to_string_lossy().replace('\\', "/"),
            missing.to_string_lossy().replace('\\', "/"),
            second.to_string_lossy().replace('\\', "/"),
        ];
        let output = read(&json!({ "paths": paths }));
        fs::remove_file(first).unwrap();
        fs::remove_file(second).unwrap();

        let lines = output.lines().collect::<Vec<_>>();
        assert!(lines.contains(&"1|first"), "错误粘进前一文件正文: {output}");
        assert!(lines.contains(&"1|second"), "后续文件正文丢失: {output}");
        assert!(
            lines.iter().any(|line| line.starts_with("E_READ")),
            "错误没有独立成行: {output}"
        );
    }

    #[test]
    fn 显式结束行保留在续读参数中() {
        let content = (1..=10)
            .map(|line| format!("{line}-{}", "x".repeat(80)))
            .collect::<Vec<_>>()
            .join("\n");
        let path = 临时文本文件(&content);
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let output = read(&json!({
            "path": posix_path,
            "start": 2,
            "end": 4,
            "budget": 340,
            "max_line": 0
        }));
        fs::remove_file(path).unwrap();

        assert!(output.len() <= 340, "实际 {} 字节: {output}", output.len());
        assert!(output.contains("2|2-"), "{output}");
        assert!(
            output.contains("续读: fs_read 同 path start=3 end=4"),
            "续读丢失原结束行，重放会越过范围: {output}"
        );
    }

    #[test]
    fn 超长单行无正文时返回可调预算错误() {
        let path = 临时文本文件(&"字".repeat(500));
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let output = read(&json!({
            "path": posix_path,
            "budget": 300,
            "max_line": 0
        }));
        fs::remove_file(path).unwrap();

        assert!(output.len() <= 300, "实际 {} 字节: {output}", output.len());
        assert!(output.starts_with("E_LINE_TOO_LONG"), "{output}");
        assert!(
            output.contains("budget") && output.contains("max_line"),
            "{output}"
        );
        assert!(
            !output.contains("start=1"),
            "空续读会让重放永不前进: {output}"
        );
    }

    #[test]
    fn 长路径小预算保留多区间续读参数() {
        let path = std::env::temp_dir().join(format!(
            "fsops-read-{}-{}-{}.txt",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            "长".repeat(60)
        ));
        let content = (1..=100)
            .map(|line| format!("{line}-{}", "字".repeat(20)))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, content).unwrap();
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let budget = posix_path.len() + 180;
        let output = read(&json!({
            "path": posix_path,
            "ranges": [[1, 2], [4, 5]],
            "budget": budget,
            "max_line": 0
        }));
        fs::remove_file(path).unwrap();

        assert!(
            output.len() <= budget,
            "实际 {} 字节: {output}",
            output.len()
        );
        assert!(
            output.contains("同 path ranges="),
            "长路径挤占预算后续读参数丢失: {output}"
        );
    }

    #[test]
    fn 多区间截断后按剩余区间续读() {
        let content = (1..=30)
            .map(|i| format!("{i}-{}", "字".repeat(20)))
            .collect::<Vec<_>>()
            .join("\n");
        let path = 临时文本文件(&content);
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let output = read(&json!({
            "path": posix_path,
            "ranges": [[1, 10], [15, 25]],
            "budget": 400,
            "max_line": 0
        }));
        fs::remove_file(path).unwrap();

        assert!(output.len() <= 400, "实际 {} 字节", output.len());
        let shown = output
            .lines()
            .filter_map(|line| line.split_once('|'))
            .filter_map(|(number, _)| number.parse::<usize>().ok())
            .collect::<Vec<_>>();
        let expected = (1..=10).chain(15..=25).collect::<Vec<_>>();
        assert_eq!(
            shown,
            expected[..shown.len()],
            "区间输出跳跃或乱序: {output}"
        );

        let marker = "ranges=";
        let ranges_json = output
            .split(marker)
            .nth(1)
            .unwrap_or_else(|| panic!("缺少 ranges 续读参数: {output}"));
        let continuation: Vec<(usize, usize)> = serde_json::from_str(ranges_json).unwrap();
        let mut skip = shown.len();
        let mut expected_continuation = Vec::new();
        for (start, end) in [(1usize, 10usize), (15, 25)] {
            let len = end - start + 1;
            if skip >= len {
                skip -= len;
            } else {
                expected_continuation.push((start + skip, end));
                skip = 0;
            }
        }
        assert_eq!(continuation, expected_continuation, "{output}");
    }

    #[test]
    fn grep_schema暴露输出分页与预算参数() {
        let defs = tool_defs();
        assert_eq!(defs.len(), 6);
        let grep = defs
            .iter()
            .find(|def| def["name"] == "fs_grep")
            .expect("缺少 fs_grep");
        let props = &grep["inputSchema"]["properties"];
        assert_eq!(
            props["output"]["enum"],
            json!(["files", "count", "matches", "snippets"])
        );
        for name in [
            "max_per_file",
            "offset",
            "parallel",
            "budget",
            "time_budget_ms",
        ] {
            assert!(props[name].is_object(), "schema 缺少 {name}");
        }
    }

    #[test]
    fn grep_files模式只返回文件名() {
        let dir = 临时目录("grep-files");
        fs::write(dir.join("a.txt"), "needle\n").unwrap();
        fs::write(dir.join("b.txt"), "needle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        let output = grep_tool(&json!({
            "pattern": "needle",
            "path": path,
            "output": "files",
            "limit": 50
        }));
        fs::remove_dir_all(dir).unwrap();

        assert_eq!(output, "a.txt\nb.txt", "{output}");
    }

    #[test]
    fn grep_count模式返回路径和命中数() {
        let dir = 临时目录("grep-count");
        fs::write(dir.join("a.txt"), "needle\nneedle\n").unwrap();
        fs::write(dir.join("b.txt"), "needle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        let output = grep_tool(&json!({
            "pattern": "needle",
            "path": path,
            "output": "count"
        }));
        fs::remove_dir_all(dir).unwrap();

        assert_eq!(output, "a.txt:2\nb.txt:1", "{output}");
    }

    #[test]
    fn grep_snippets默认带上下文() {
        let dir = 临时目录("grep-snippets");
        fs::write(dir.join("a.txt"), "top1\ntop2\nneedle\nafter1\nafter2\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        let output = grep_tool(&json!({
            "pattern": "needle",
            "path": path,
            "output": "snippets"
        }));
        fs::remove_dir_all(dir).unwrap();

        assert_eq!(
            output, "a.txt\n1-top1\n2-top2\n3:needle\n4-after1\n5-after2",
            "{output}"
        );
    }

    #[test]
    fn grep默认输出保留显式上下文() {
        let dir = 临时目录("grep-default-context");
        fs::write(dir.join("a.txt"), "before\nneedle\nafter\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        let output = grep_tool(&json!({
            "pattern": "needle",
            "path": path,
            "context": 1
        }));
        fs::remove_dir_all(dir).unwrap();

        assert_eq!(output, "a.txt\n1-before\n2:needle\n3-after", "{output}");
    }

    #[test]
    fn grep截断返回前进的next_offset() {
        let dir = 临时目录("grep-next");
        fs::write(dir.join("a.txt"), "needle\n").unwrap();
        fs::write(dir.join("b.txt"), "needle\n").unwrap();
        fs::write(dir.join("c.txt"), "needle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        let first = grep_tool(&json!({
            "pattern": "needle",
            "path": path,
            "limit": 1
        }));
        let second = grep_tool(&json!({
            "pattern": "needle",
            "path": path,
            "limit": 1,
            "offset": 1
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(first.contains("next_offset=1"), "{first}");
        assert!(!first.contains("b.txt"), "第一页越过 limit: {first}");
        assert!(second.contains("next_offset=2"), "{second}");
        assert!(!second.contains("a.txt"), "重放未前进: {second}");
    }

    #[test]
    fn grep预算不足时不得推进offset() {
        let dir = 临时目录("grep-budget-empty");
        fs::write(dir.join("a.txt"), "needle\nneedle\nneedle\nneedle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        let output = grep_tool(&json!({
            "pattern": "needle",
            "path": path,
            "output": "snippets",
            "context": 5,
            "budget": 30
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(output.starts_with("E_BUDGET"), "{output}");
        assert!(
            !output.contains("next_offset"),
            "空页不得推进 offset: {output}"
        );
        assert!(output.len() <= 30, "预算超限: {output}");
    }

    #[test]
    fn grep_offset超出时不误报无命中() {
        let dir = 临时目录("grep-offset-over");
        fs::write(dir.join("a.txt"), "needle\nneedle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        let output = grep_tool(&json!({
            "pattern": "needle",
            "path": path,
            "offset": 99
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(output.contains("offset=99"), "{output}");
        assert!(!output.contains("ok 无命中"), "{output}");
    }

    #[test]
    fn grep_limit_0直接报错() {
        let output = grep_tool(&json!({"pattern": "needle", "limit": 0}));
        assert!(output.starts_with("E_BAD_ARGS"), "{output}");
    }

    #[test]
    fn grep_可以直接搜索单个文件() {
        let dir = 临时目录("grep-single-file");
        let file = dir.join("only.txt");
        fs::write(&file, "before\nneedle\nafter\n").unwrap();
        let path = file.to_string_lossy().replace('\\', "/");

        let output = grep_tool(&json!({
            "pattern": "needle",
            "path": path
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(output.contains("only.txt"), "{output}");
        assert!(output.contains("2:needle"), "{output}");
    }

    #[test]
    fn grep末页不返回不存在的next_offset() {
        let dir = 临时目录("grep-last-page");
        fs::write(dir.join("a.txt"), "needle\n").unwrap();
        fs::write(dir.join("b.txt"), "needle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        let output = grep_tool(&json!({
            "pattern": "needle",
            "path": path,
            "limit": 2
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(
            !output.contains("next_offset"),
            "末页不应再给续取: {output}"
        );
    }

    #[test]
    fn grep每文件上限省略时给出说明() {
        let dir = 临时目录("grep-max-note");
        fs::write(dir.join("a.txt"), "needle\nneedle\nneedle\nneedle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        let output = grep_tool(&json!({
            "pattern": "needle",
            "path": path,
            "max_per_file": 2
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(output.contains("max_per_file"), "{output}");
        assert!(output.contains("2"), "缺少省略数量: {output}");
    }

    #[test]
    fn 读取失败返回错误码前缀() {
        let missing = std::env::temp_dir().join("fsops-does-not-exist-读取测试.txt");
        let output = read(&json!({"path": missing.to_string_lossy()}));
        assert!(output.starts_with("E_READ"), "{output}");
    }

    #[test]
    fn grep最终响应含上下文与尾注不超字节预算() {
        let dir = 临时目录("grep-budget");
        let content = (1..=30)
            .map(|i| format!("上下文🙂 {i} needle {}", "内容".repeat(8)))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.join("长文件名-中文.txt"), content).unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        let output = grep_tool(&json!({
            "pattern": "needle",
            "path": path,
            "output": "snippets",
            "context": 1,
            "limit": 100,
            "budget": 320
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(output.len() <= 320, "实际 {} 字节: {output}", output.len());
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
        let next = output
            .split("next_offset=")
            .nth(1)
            .and_then(|tail| tail.split_whitespace().next())
            .and_then(|number| number.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("缺少 next_offset: {output}"));
        assert!(next > 0, "重放 offset 未前进: {output}");
    }

    #[test]
    fn grep极小预算下错误响应也不超限() {
        for budget in 0..24 {
            let output = grep_tool(&json!({ "budget": budget }));
            assert!(
                output.len() <= budget,
                "budget={budget}，实际 {} 字节: {output}",
                output.len()
            );
            assert!(std::str::from_utf8(output.as_bytes()).is_ok());
        }
    }

    #[test]
    fn 搜索输出按文件路径稳定排序并分组() {
        let tmp = std::env::temp_dir().join(format!("fsops-grep-order-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("a.txt"), "needle\n").unwrap();
        fs::write(tmp.join("B.txt"), "needle\n").unwrap();

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let r = search::grep(
            &root,
            &GrepOpts {
                pattern: "needle".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let paths: Vec<&str> = r.hits.iter().map(|hit| hit.path.as_str()).collect();
        assert_eq!(paths, vec!["B.txt", "a.txt"], "文件顺序不稳定: {paths:?}");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn 时间预算为零时目录遍历也不启动() {
        let tmp = std::env::temp_dir().join(format!("fsops-grep-timeout-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("empty/nested")).unwrap();
        fs::write(tmp.join("a.txt"), "needle\n").unwrap();

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let r = search::grep(
            &root,
            &GrepOpts {
                pattern: "needle".into(),
                glob: "**/*.txt".into(),
                time_budget_ms: 0,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(r.timed_out);
        assert!(r.truncated);
        assert_eq!(r.entries_visited, 0);
        assert_eq!(r.files_scanned, 0);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn limit_0不返回正文命中() {
        let tmp = std::env::temp_dir().join(format!("fsops-grep-zero-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("a.txt"), "needle\n").unwrap();

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let r = search::grep(
            &root,
            &GrepOpts {
                pattern: "needle".into(),
                limit: 0,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(r.hits.is_empty(), "limit=0 仍返回正文命中");
        assert!(r.truncated);
        let _ = fs::remove_dir_all(&tmp);
    }
}

struct PreparedEdit {
    fp: FsPath,
    bytes: Vec<u8>,
    result: edit::EditResult,
}

struct EditRequest {
    path: String,
    fp: FsPath,
    edits: Vec<EditSpec>,
    expect_hash: Option<String>,
    normalize_eol: bool,
}

fn parse_edits(value: &Value) -> Result<Vec<EditSpec>, String> {
    let edits: Vec<EditSpec> = serde_json::from_value(value.clone())
        .map_err(|e| format!("E_BAD_ARGS edits 解析失败: {e}"))?;
    if edits.is_empty() {
        return Err("E_BAD_ARGS edits 不能为空".into());
    }
    Ok(edits)
}

/// 完成全部预检并生成待写字节；调用方必须先持有该路径的跨进程写锁。
fn prepare_edit(
    fp: &FsPath,
    edits: &[EditSpec],
    expect_hash: Option<&str>,
    normalize_eol: bool,
) -> Result<PreparedEdit, String> {
    let io::Snapshot { doc, hash, .. } = io::read_snapshot(fp)?;
    let eol = if doc.eol == crate::text::Eol::Mixed {
        if normalize_eol {
            crate::text::Eol::Lf
        } else {
            return Err(
                "E_MIXED_EOL 文件包含混合换行；请显式设置 normalize_eol=true 归一为 LF".into(),
            );
        }
    } else {
        doc.eol
    };
    if let Some(expect) = expect_hash {
        if hash != expect {
            return Err(format!(
                "E_STALE 文件已变化（当前 hash={hash}），请重新 fs_read 后再改"
            ));
        }
    }
    let (new_text, result) = edit::apply(&doc.text, eol, edits)?;
    // dry_run 也要完成编码预检，不能绕过不可表示字符校验。
    let bytes = crate::text::encode(&doc, &new_text)?;
    Ok(PreparedEdit {
        fp: fp.clone(),
        bytes,
        result,
    })
}

fn format_loci(loci: &[String]) -> String {
    if loci.len() <= 8 {
        return loci.join(",");
    }
    format!("{} … 共{}处", loci[..8].join(","), loci.len())
}

fn edit_tool(args: &Value) -> String {
    let dry_run = arg_bool(args, "dry_run", false);
    let normalize_eol = arg_bool(args, "normalize_eol", false);
    let batch = args.get("files").and_then(Value::as_array);
    let batch_mode = batch.is_some();
    let mut requests = Vec::new();

    if let Some(files) = batch {
        if args.get("path").is_some() || args.get("edits").is_some() {
            return "E_BAD_ARGS path/edits 与 files 不能混用".into();
        }
        if files.is_empty() {
            return "E_BAD_ARGS files 不能为空".into();
        }
        if files.len() > 50 {
            return "E_BAD_ARGS 一次最多编辑 50 个文件".into();
        }
        if args.get("expect_hash").is_some() {
            return "E_BAD_ARGS 批量模式请在每个文件内使用 expect_hash".into();
        }
        let mut seen: Vec<String> = Vec::new();
        for (index, item) in files.iter().enumerate() {
            let path = match item.get("path").and_then(Value::as_str) {
                Some(path) if !path.is_empty() => path,
                _ => return format!("E_BATCH 第{}个文件缺少 path", index + 1),
            };
            let fp = match FsPath::parse(path) {
                Ok(fp) => fp,
                Err(e) => return format!("E_BATCH {path}: E_BAD_PATH {e}"),
            };
            let shown = fp.display();
            if seen.contains(&shown) {
                return format!("E_BATCH 重复路径 {shown}；同一文件请合并到一组 edits");
            }
            seen.push(shown);
            let edits = match parse_edits(item.get("edits").unwrap_or(&Value::Null)) {
                Ok(edits) => edits,
                Err(e) => return format!("E_BATCH {path}: {e}"),
            };
            let expect = item.get("expect_hash").and_then(Value::as_str);
            requests.push(EditRequest {
                path: path.to_string(),
                fp,
                edits,
                expect_hash: expect.map(str::to_string),
                normalize_eol: normalize_eol || arg_bool(item, "normalize_eol", false),
            });
        }
    } else {
        let path = match arg_str(args, "path") {
            Some(path) => path,
            None => return "E_BAD_ARGS 需要 path+edits 或 files".into(),
        };
        let edits = match parse_edits(args.get("edits").unwrap_or(&Value::Null)) {
            Ok(edits) => edits,
            Err(e) => return e,
        };
        let fp = match FsPath::parse(path) {
            Ok(fp) => fp,
            Err(e) => return format!("E_BAD_PATH {e}"),
        };
        requests.push(EditRequest {
            path: path.to_string(),
            fp,
            edits,
            expect_hash: arg_str(args, "expect_hash").map(str::to_string),
            normalize_eol,
        });
    }

    // 按路径排序后统一加锁，避免两个批量任务以相反顺序互相等待。
    let mut lock_order: Vec<usize> = (0..requests.len()).collect();
    lock_order.sort_by_key(|&index| requests[index].fp.display());
    let mut _locks = Vec::with_capacity(requests.len());
    for index in lock_order {
        match io::lock_path(&requests[index].fp, 3000) {
            Ok(lock) => _locks.push(lock),
            Err(e) if batch_mode => {
                return format!("E_BATCH {}: {e}", requests[index].path);
            }
            Err(e) => return e,
        }
    }

    let mut prepared = Vec::with_capacity(requests.len());
    for request in &requests {
        match prepare_edit(
            &request.fp,
            &request.edits,
            request.expect_hash.as_deref(),
            request.normalize_eol,
        ) {
            Ok(done) => prepared.push(done),
            Err(e) if batch_mode => return format!("E_BATCH {}: {e}", request.path),
            Err(e) => return e,
        }
    }

    let file_count = prepared.len();
    let applied: usize = prepared.iter().map(|item| item.result.applied).sum();
    let added: i64 = prepared.iter().map(|item| item.result.added_lines).sum();
    let removed: i64 = prepared.iter().map(|item| item.result.removed_lines).sum();

    if dry_run {
        let mut out = format!(
            "ok dry_run: {} 个文件 {} 处将修改 (+{} -{})",
            file_count, applied, added, removed
        );
        for item in &prepared {
            out.push_str(&format!(
                "\n{} @ {}",
                item.fp.display(),
                format_loci(&item.result.loci)
            ));
        }
        return out;
    }

    for (index, item) in prepared.iter().enumerate() {
        if let Err(e) = io::atomic_write(&item.fp, &item.bytes) {
            return format!(
                "E_BATCH_PARTIAL 已写入 {}/{}；失败 {}: {e}",
                index,
                file_count,
                item.fp.display()
            );
        }
    }

    if file_count == 1 {
        let item = &prepared[0];
        return format!(
            "ok {} 处修改 (+{} -{}) @ {}  {}  hash={}",
            item.result.applied,
            item.result.added_lines,
            item.result.removed_lines,
            format_loci(&item.result.loci),
            item.fp.display(),
            io::hash_hex(&item.bytes)
        );
    }

    let mut out = format!(
        "ok {} 个文件 {} 处修改 (+{} -{})",
        file_count, applied, added, removed
    );
    for item in &prepared {
        out.push_str(&format!(
            "\n{} @ {} hash={}",
            item.fp.display(),
            format_loci(&item.result.loci),
            io::hash_hex(&item.bytes)
        ));
    }
    out
}

fn write(args: &Value) -> String {
    let path = match arg_str(args, "path") {
        Some(p) => p,
        None => return "E_BAD_ARGS 缺少 path".into(),
    };
    let content = match args.get("content").and_then(Value::as_str) {
        Some(c) => c,
        None => return "E_BAD_ARGS 缺少 content".into(),
    };
    let fp = match FsPath::parse(path) {
        Ok(p) => p,
        Err(e) => return format!("E_BAD_PATH {e}"),
    };
    let _write_lock = match io::lock_path(&fp, 3000) {
        Ok(lock) => lock,
        Err(e) => return e,
    };

    // 已有文件：沿用其编码与换行风格，避免把 CRLF 仓库改成 LF
    let existing = if fp.exists() && !fp.is_dir() {
        io::read_snapshot(&fp).ok()
    } else {
        None
    };
    // 已有文件沿用原换行；新文件沿用内容自身的稳定换行。
    // 目标风格已知时，混合内容也直接归一到目标风格，避免无谓拒绝。
    let content_eol = crate::text::detect_eol(content);
    let target_eol = match &existing {
        Some(snapshot) if snapshot.doc.eol == crate::text::Eol::Mixed => {
            if !arg_bool(args, "normalize_eol", false) {
                return "E_MIXED_EOL 原文件包含混合换行；请显式设置 normalize_eol=true 归一为 LF"
                    .into();
            }
            crate::text::Eol::Lf
        }
        Some(snapshot) => snapshot.doc.eol,
        None if content_eol == crate::text::Eol::Mixed => {
            if !arg_bool(args, "normalize_eol", false) {
                return "E_MIXED_EOL 待写内容包含混合换行；请显式设置 normalize_eol=true 归一为 LF"
                    .into();
            }
            crate::text::Eol::Lf
        }
        None => content_eol,
    };
    if let Some(expect) = arg_str(args, "expect_hash") {
        match &existing {
            Some(snapshot) => {
                if snapshot.hash != expect {
                    return format!("E_STALE 文件已变化（当前 hash={}）", snapshot.hash);
                }
            }
            None => return "E_STALE 目标文件不存在，无法校验 hash".into(),
        }
    }

    let normalized_content = crate::text::from_lf(&crate::text::to_lf(content), target_eol);
    let content = normalized_content.as_str();
    let bytes = match &existing {
        Some(snapshot) => {
            let doc2 = crate::text::Doc {
                text: content.to_string(),
                ..snapshot.doc.clone()
            };
            match crate::text::encode(&doc2, content) {
                Ok(bytes) => bytes,
                Err(e) => return e,
            }
        }
        None => content.as_bytes().to_vec(),
    };

    if let Err(e) = io::atomic_write(&fp, &bytes) {
        return e;
    }
    format!(
        "ok 写入 {} {} {}行 hash={}",
        fp.display(),
        io::human_size(bytes.len() as u64),
        content.split('\n').count(),
        io::hash_hex(&bytes)
    )
}

fn glob_tool(args: &Value) -> String {
    let budget = arg_usize(args, "budget", 8 * 1024);
    let root_s = arg_str(args, "path").unwrap_or(".");
    let root = match FsPath::parse(root_s) {
        Ok(p) => p,
        Err(e) => return format!("E_BAD_PATH {e}"),
    };
    let pattern = arg_str(args, "pattern").unwrap_or("");
    let limit = arg_usize(args, "limit", 200);
    if limit == 0 {
        return "E_BAD_ARGS limit 必须大于 0；省略表示默认 200".into();
    }
    let opts = WalkOpts {
        include_dirs: arg_bool(args, "dirs", false),
        limit,
        follow_links: false,
    };
    match walk::glob(&root, pattern, &opts) {
        Ok(entries) => {
            if entries.is_empty() {
                format!("ok 无匹配（{} pattern={pattern}）", root.display())
            } else {
                let body = walk::render_entries(&entries, arg_bool(args, "detail", false));
                let body = body.trim_end();
                if body.len() <= budget {
                    return body.to_string();
                }
                let marker = "\n… 输出截断：减小 limit 或添加 pattern";
                let room = budget.saturating_sub(marker.len());
                if room == 0 {
                    return render::truncate_utf8(body, budget).to_string();
                }
                format!("{}{marker}", render::truncate_utf8(body, room))
            }
        }
        Err(e) => e,
    }
}

fn grep_tool(args: &Value) -> String {
    let budget = arg_usize(args, "budget", 8 * 1024);
    let pattern = match arg_str(args, "pattern") {
        Some(p) => p.to_string(),
        None => {
            return render::truncate_utf8("E_BAD_ARGS 缺少 pattern", budget).to_string();
        }
    };
    let output = match arg_str(args, "output").unwrap_or("matches") {
        "files" => GrepOutput::Files,
        "count" => GrepOutput::Count,
        "matches" => GrepOutput::Matches,
        "snippets" => GrepOutput::Snippets,
        other => {
            return render::truncate_utf8(
                &format!("E_BAD_ARGS 未知 output={other}；可选 files/count/matches/snippets"),
                budget,
            )
            .to_string()
        }
    };
    let context = match args.get("context").and_then(Value::as_u64) {
        Some(value) => value as usize,
        None if output == GrepOutput::Snippets => 2,
        None => 0,
    }
    .min(5);
    let limit = arg_usize(args, "limit", 100);
    if limit == 0 {
        return render::truncate_utf8("E_BAD_ARGS limit 必须大于 0；省略表示默认 100", budget)
            .to_string();
    }
    let root = match FsPath::parse(arg_str(args, "path").unwrap_or(".")) {
        Ok(p) => p,
        Err(e) => return render::truncate_utf8(&format!("E_BAD_PATH {e}"), budget).to_string(),
    };
    let opts = GrepOpts {
        pattern,
        glob: arg_str(args, "glob").unwrap_or("").to_string(),
        ignore_case: arg_bool(args, "ignore_case", false),
        context,
        limit,
        offset: arg_usize(args, "offset", 0),
        max_per_file: arg_usize(args, "max_per_file", usize::MAX),
        budget,
        parallel: arg_bool(args, "parallel", false),
        time_budget_ms: arg_usize(args, "time_budget_ms", 5000) as u128,
        ..Default::default()
    };
    match search::grep(&root, &opts) {
        Ok(r) => {
            if r.hits.is_empty() {
                if r.timed_out {
                    return render::truncate_utf8(
                        &format!(
                            "E_TIMEOUT 时间预算用尽（扫描 {} 个文件）：缩小 path 或加 glob 再试",
                            r.files_scanned
                        ),
                        budget,
                    )
                    .to_string();
                }
                if r.offset_skipped > 0 {
                    let text = format!(
                        "ok 本页无结果：offset={} 已跳过 {} 条命中",
                        opts.offset, r.offset_skipped
                    );
                    return render::truncate_utf8(&text, budget).to_string();
                }
                if r.omitted_per_file > 0 {
                    let text = format!(
                        "ok 无可见命中：max_per_file 省略了 {} 条",
                        r.omitted_per_file
                    );
                    return render::truncate_utf8(&text, budget).to_string();
                }
                return render::truncate_utf8(
                    &format!("ok 无命中（扫描 {} 个文件）", r.files_scanned),
                    budget,
                )
                .to_string();
            }
            let mut rendered_matches = 0usize;
            let mut render_cut = false;
            let output = render::converge_budget(budget, |body_budget| {
                let rendered = search::render_mode(&r, output, body_budget);
                rendered_matches = rendered.matches_rendered;
                render_cut = rendered.truncated;
                let mut text = rendered.text.trim_end().to_string();
                if r.timed_out {
                    text.push_str(&format!(
                        "\n… 时间预算用尽（已扫 {} 个文件）：缩小 path 或加 glob 再试",
                        r.files_scanned
                    ));
                }
                if r.omitted_per_file > 0 {
                    text.push_str(&format!(
                        "\n… 省略 {} 条（max_per_file={}）",
                        r.omitted_per_file, opts.max_per_file
                    ));
                }
                if (r.truncated || render_cut || r.timed_out) && rendered_matches > 0 {
                    let next = opts.offset.saturating_add(rendered_matches);
                    text.push_str(&format!("\n… next_offset={next}"));
                }
                text.trim_end().to_string()
            });
            if rendered_matches == 0 && r.match_count > 0 {
                return render::truncate_utf8(
                    "E_BUDGET 命中存在但预算不足以显示；请增大 budget",
                    budget,
                )
                .to_string();
            }
            output
        }
        Err(e) => render::truncate_utf8(&e, budget).to_string(),
    }
}

fn stat(args: &Value) -> String {
    let detail = arg_str(args, "detail").unwrap_or("full");
    if !matches!(detail, "meta" | "hash" | "full") {
        return "E_BAD_ARGS detail 仅支持 meta/hash/full".into();
    }
    let mut targets: Vec<String> = Vec::new();
    if let Some(p) = arg_str(args, "path") {
        targets.push(p.to_string());
    }
    if let Some(arr) = args.get("paths").and_then(Value::as_array) {
        for v in arr.iter().filter_map(Value::as_str) {
            targets.push(v.to_string());
        }
    }
    if targets.is_empty() {
        return "E_BAD_ARGS 需要 path 或 paths".into();
    }
    let mut out = String::new();
    for t in targets.iter().take(50) {
        let fp = match FsPath::parse(t) {
            Ok(p) => p,
            Err(e) => {
                out.push_str(&format!("E_BAD_PATH {e}\n"));
                continue;
            }
        };
        let m = match io::meta(&fp) {
            Ok(m) => m,
            Err(e) => {
                out.push_str(&format!("{e}\n"));
                continue;
            }
        };
        if m.kind == "file" {
            if detail == "meta" {
                out.push_str(&format!(
                    "{} file {} {} {}\n",
                    fp.display(),
                    io::human_size(m.size),
                    io::fmt_time(m.mtime),
                    if m.readonly { "ro" } else { "rw" }
                ));
                continue;
            }
            match io::read_snapshot(&fp) {
                Ok(snapshot) => {
                    let d = &snapshot.doc;
                    if detail == "hash" {
                        out.push_str(&format!(
                            "{} file {} {} {} {} {} hash={}\n",
                            fp.display(),
                            io::human_size(m.size),
                            io::fmt_time(m.mtime),
                            d.encoding,
                            d.eol.as_str(),
                            if m.readonly { "ro" } else { "rw" },
                            snapshot.hash
                        ));
                    } else {
                        out.push_str(&format!(
                            "{} file {} {} {} {} {} {}行 hash={}\n",
                            fp.display(),
                            io::human_size(m.size),
                            io::fmt_time(m.mtime),
                            d.encoding,
                            d.eol.as_str(),
                            if m.readonly { "ro" } else { "rw" },
                            d.text.split('\n').count(),
                            snapshot.hash
                        ));
                    }
                }
                Err(e) => out.push_str(&format!(
                    "{} file {}\n{e}\n",
                    fp.display(),
                    io::human_size(m.size)
                )),
            }
        } else {
            out.push_str(&format!(
                "{} {} {} {}\n",
                fp.display(),
                m.kind,
                io::human_size(m.size),
                io::fmt_time(m.mtime)
            ));
        }
    }
    out.trim_end().to_string()
}
