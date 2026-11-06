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
            "description": "读取文件。优先用本工具替代宿主 read_file/cat/head/tail/Get-Content。默认只返回前 12KB 并给出续读方式；支持行范围、多文件、DOCX/PDF 正文、自动识别 GBK/UTF-16/BOM 与 CRLF。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "文件路径，如 /d/repo/app/src/main.rs"},
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "一次读多个文件（最多 20 个）"},
                    "start": {"type": "integer", "description": "起始行（1 基）"},
                    "end": {"type": "integer", "description": "结束行（含）；用 block=true 时可省略"},
                    "block": {"type": "boolean", "description": "返回包含 start 行的整个代码块（自动向前找块起始、向后配平结束）；配合 fs_grep 的行号用，命中行在函数内部也能展开"},
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
            "description": "精确替换文本（old→new）。优先替代 patch/Edit/apply_patch。同文件多处用 edits，多文件用 files；全部预检通过才写入。自动处理 CRLF/LF 差异并保留原编码；DOCX 不支持精确替换，请用 fs_write 整体重建。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "单文件路径；批量时改用 files"},
                    "edits": {
                        "type": "array",
                        "description": "单文件的修改列表",
                        "items": edit_item_schema()
                    },
                    "files": {
                        "type": "array",
                        "description": "多文件批量编辑；全部文件预检成功后逐文件原子写入，最多 50 个",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "edits": {"type": "array", "items": edit_item_schema()},
                                "expect_hash": {"type": "string", "description": "该文件的并发保护指纹"}
                            },
                            "required": ["path", "edits"]
                        }
                    },
                    "dry_run": {"type": "boolean", "description": "只校验不落盘"},
                    "diff": {"type": "boolean", "description": "附带被替换的原文/新文片段（`@@ 行号` + `-原文` / `+新文`）；默认关闭，改结构后想自查时打开，可省一次 fs_read"},
                    "normalize_eol": {"type": "boolean", "description": "混合换行时显式归一为 LF"},
                    "expect_hash": {"type": "string", "description": "单文件并发保护：文件已变则拒绝写入"}
                },
                "required": []
            }
        }),
        json!({
            "name": "fs_write",
            "description": "写入文件。优先替代 write_file/Write。原子替换、自动建目录、保留原有编码与换行风格；文本外也支持生成 DOCX（纯文本段落，整体重建）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "normalize_eol": {"type": "boolean", "description": "新文件或原文件为混合换行时，显式归一为 LF；已有稳定文件始终沿用原换行"},
                    "expect_hash": {"type": "string", "description": "并发保护：文件已变则拒绝覆盖"},
                    "dry_run": {"type": "boolean", "description": "只做路径/编码/换行/hash 预检，不创建也不覆盖文件"}
                },
                "required": ["path", "content"]
            }
        }),
        json!({
            "name": "fs_glob",
            "description": "按 glob 找文件。优先替代 find/Glob/Get-ChildItem。尊重 .gitignore，自动跳过 node_modules/target 等。默认只给路径，detail=true 附大小与时间。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "搜索根目录，默认当前目录"},
                    "pattern": {"type": "string", "description": "如 **/*.rs，多个用逗号分隔；省略表示全部"},
                    "dirs": {"type": "boolean", "description": "是否包含目录"},
                    "detail": {"type": "boolean", "description": "是否附大小与修改时间（默认只给路径，省 token）"},
                    "limit": {"type": "integer", "description": "最多返回条数，默认 200；达到后提前停止"},
                    "time_budget_ms": {"type": "integer", "description": "目录遍历时间预算，默认 5000 毫秒；超时返回已找到结果并标注"},
                    "no_ignore": {"type": "boolean", "description": "忽略 .gitignore 与 node_modules/target 等默认跳过项；只在明确要查依赖源码或构建产物时用"},
                    "budget": {"type": "integer", "description": "最终响应最多字节数，默认 8192"}
                },
                "required": []
            }
        }),
        json!({
            "name": "fs_grep",
            "description": "正则流式搜索。优先替代 grep/rg/Select-String。按文件稳定分组，支持文本和 DOCX/PDF 正文，output 可选 files/count/matches/snippets；截断时返回 next_offset。",
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
                    "time_budget_ms": {"type": "integer", "description": "文件与目录遍历总时间预算，默认 5000 毫秒"},
                    "no_ignore": {"type": "boolean", "description": "忽略 .gitignore 与 node_modules/target 等默认跳过项；只在明确要搜依赖源码时用"}
                },
                "required": ["pattern"]
            }
        }),
        json!({
            "name": "fs_stat",
            "description": "查看路径元信息。优先替代 stat/ls/Get-Item。返回类型、大小、修改时间、换行风格、编码、内容指纹。",
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

fn read_document(
    fp: &FsPath,
    format: crate::documents::Format,
) -> Result<crate::documents::Extracted, String> {
    let bytes = io::read_bytes(&fp.wide())?;
    let text = crate::documents::extract_bytes(format, &bytes)?;
    Ok(crate::documents::Extracted {
        text,
        size: bytes.len() as u64,
        hash: io::hash_hex(&bytes),
    })
}

/// 合并重叠或相邻的区间：重叠的 ranges 会把同一批行输出多次，纯属浪费 token。
/// 保持调用方给定的先后顺序（乱序 ranges 仍按原顺序输出）。
fn merge_ranges(ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (from, to) in ranges {
        // 只与已输出的区间比较；乱序时不做全局排序，保持用户预期。
        if let Some(last) = out.last_mut() {
            if from <= last.1 {
                last.1 = last.1.max(to);
                continue;
            }
            if from == last.1 + 1 {
                last.1 = to;
                continue;
            }
        }
        out.push((from, to));
    }
    out
}

/// 求多个 POSIX 路径的公共目录前缀（含结尾 `/`）；没有公共目录时返回 None。
fn common_dir_prefix(paths: &[String]) -> Option<String> {
    if paths.len() < 2 {
        return None;
    }
    let first = paths.first()?;
    let mut cut = first.rfind('/')?;
    for p in &paths[1..] {
        let mut i = 0;
        let bytes_a = first.as_bytes();
        let bytes_b = p.as_bytes();
        while i < cut && i < p.len() && bytes_a[i] == bytes_b[i] {
            i += 1;
        }
        cut = cut.min(i);
    }
    if cut == 0 {
        return None;
    }
    let prefix = &first[..cut];
    // 收尾到最后一个 `/`，保证前缀是完整的目录
    let trimmed = prefix.rfind('/').map(|i| &prefix[..=i])?;
    if trimmed.len() <= 1 {
        return None;
    }
    Some(trimmed.to_string())
}

/// 估算某一行完整渲染需要多少字节，用于在 E_LINE_TOO_LONG 里给出可执行的 budget 数值。
fn line_need_bytes(text: &str, line_no: usize) -> usize {
    text.split('\n')
        .nth(line_no.saturating_sub(1))
        .map(|l| l.trim_end_matches('\r').len())
        .unwrap_or(0)
}

/// 找到包含 `start` 行的整个代码块，返回 (起始行, 结束行)，均 1 基。
///
/// 两种情形：
/// - `start` 行自己开启一个跨行块 → 就是它本身（保持原有行为）
/// - `start` 行在块内部（grep 的典型输出）→ 向前找最近的未闭合 `{` 所在行
///
/// 找不到时不猜测，调用方退回普通 start/end 语义。
fn enclosing_block(lines: &[&str], start: usize) -> Option<(usize, usize)> {
    // 情形一：本行开启跨行块。单行内自平衡（如字符串里有 `{hash}`）不算开启。
    if let Some(end) = block_end(lines, start) {
        if end > start {
            return Some((start, end));
        }
    }
    // 情形二：向前扫描。反向统计花括号，第一个"未闭合的 {" 就是块起始行。
    let idx = start.checked_sub(1)?;
    if idx == 0 {
        return None;
    }
    let mut acc: i64 = 0;
    let mut opener = None;
    for i in (0..idx).rev() {
        let line = lines[i];
        let opens = line.matches('{').count() as i64;
        let closes = line.matches('}').count() as i64;
        acc += closes - opens;
        if acc < 0 {
            opener = Some(i + 1); // 转回 1 基
            break;
        }
    }
    let from = opener?;
    let to = block_end(lines, from)?;
    if to < start {
        return None;
    }
    Some((from, to))
}

/// 从起始行按括号配平推算代码块的结束行。
///
/// 模型从 `fs_grep` 得知"fn foo 在第 N 行"后，无法预知函数有多长：
/// 猜小了要重读，猜大了浪费字节。这个函数让 `fs_read block=true` 直接给出边界。
/// 只对花括号语言有意义；找不到配平或起始行没有 `{` 时返回 None，调用方退回原逻辑。
fn block_end(lines: &[&str], start: usize) -> Option<usize> {
    let idx = start.checked_sub(1)?;
    let first = lines.get(idx)?;
    // 起始行必须开启一个块；否则（如纯声明、注释）不做猜测。
    if !first.contains('{') {
        return None;
    }
    let mut depth: i64 = 0;
    let mut opened = false;
    for (offset, line) in lines[idx..].iter().enumerate() {
        // 跳过字符串与注释里的花括号是过度工程；按常见格式化代码足够准。
        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if opened && depth <= 0 {
            return Some(start + offset);
        }
        // 防御：超过 2000 行还没闭合，说明判断不可靠。
        if offset > 2000 {
            return None;
        }
    }
    None
}

/// `fs_edit` 单条修改的 schema。
///
/// `edits` 与 `files[].edits` 必须同形：分开手写会出现一处更新、另一处漂移，
/// 模型在 `files` 里只能看到一个空对象，只能靠猜字段名。
fn edit_item_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "old": {"type": "string", "description": "原文片段（需唯一，除非 all=true）"},
            "new": {"type": "string", "description": "替换内容，空串表示删除"},
            "all": {"type": "boolean", "description": "替换全部出现位置"},
            "start": {"type": "integer", "description": "限定替换只发生在第 start..=end 行内（1 基）；配合 all=true 只改某个函数内的重复字面量"},
            "end": {"type": "integer", "description": "行范围结束行（含）；省略表示到文件末尾"}
        },
        "required": ["old"]
    })
}

fn format_label(format: crate::documents::Format) -> &'static str {
    match format {
        crate::documents::Format::Docx => "docx",
        crate::documents::Format::Pdf => "pdf",
    }
}

/// 行数：空文件算 0 行，末尾换行不额外算一行。
/// 旧实现用 `split('\n').count()`，空文件会报“1行”，让模型误以为有内容。
fn line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut lines = text.split('\n').count();
    if text.ends_with('\n') {
        lines -= 1;
    }
    lines
}

fn doc_format_error(format: crate::documents::Format) -> &'static str {
    match format {
        crate::documents::Format::Docx => "E_DOCX",
        crate::documents::Format::Pdf => "E_PDF",
    }
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

/// 参数类型的中文名，用于给出可执行的错误信息。
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "布尔值",
        Value::Number(_) => "数字",
        Value::String(_) => "字符串",
        Value::Array(_) => "数组",
        Value::Object(_) => "对象",
    }
}

/// 按 schema 校验参数类型。
///
/// 为什么必须有：类型写错时旧代码会用 `unwrap_or(default)` 静默退回默认值，
/// 模型以为参数生效了却得到不同行为——这比直接报错危险得多。
fn validate_args(name: &str, args: &Value) -> Option<String> {
    let def = tool_defs().into_iter().find(|d| d["name"] == name)?;
    let props = def["inputSchema"]["properties"].as_object()?;
    let obj = args.as_object()?;
    for (key, value) in obj {
        let Some(spec) = props.get(key) else { continue };
        let wanted = spec["type"].as_str().unwrap_or("");
        let ok = match wanted {
            "string" => value.is_string(),
            "integer" | "number" => value.is_u64() || value.is_i64() || value.is_f64(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            _ => true,
        };
        if !ok {
            let expected = match wanted {
                "string" => "字符串",
                "integer" | "number" => "数字",
                "boolean" => "布尔值",
                "array" => "数组",
                "object" => "对象",
                other => other,
            };
            return Some(format!(
                "E_BAD_ARGS {}.{} 需要{}，收到 {}",
                name,
                key,
                expected,
                type_name(value)
            ));
        }
    }
    None
}

pub fn call(name: &str, args: &Value) -> String {
    // 参数类型写错必须报错，不能静默退回默认值。
    if let Some(err) = validate_args(name, args) {
        return err;
    }
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
    let budget_explicit = args.get("budget").is_some();
    let mut budget = arg_usize(args, "budget", render::DEFAULT_BUDGET);
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
    // 静默截断会让模型以为读到了全部文件；超上限必须显式拒绝。
    if targets.len() > 20 {
        return render::truncate_utf8(
            &format!(
                "E_BAD_ARGS 一次最多读 20 个文件，当前 {} 个；请分批读取，或改用 fs_grep 定位后只读命中文件",
                targets.len()
            ),
            budget.max(200),
        )
        .to_string();
    }
    // 重复路径会让同一份内容在上下文里出现两次，纯属浪费 token。
    let mut seen: Vec<String> = Vec::new();
    targets.retain(|t| {
        let key = FsPath::parse(t)
            .map(|p| p.display())
            .unwrap_or_else(|_| t.clone());
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });

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
        merge_ranges(parsed)
    } else {
        vec![(start, end)]
    };

    // 显式行区间是“要这些行”的契约；预算没显式给时，按行数估算，
    // 避免 20 行中文被 12KB 字节预算截成十几行。仍保留 64KB 上限。
    if !budget_explicit {
        let requested_lines = if args.get("ranges").is_some() {
            ranges
                .iter()
                .map(|(from, to)| to.saturating_sub(*from).saturating_add(1))
                .sum()
        } else if args.get("end").is_some() {
            end.saturating_sub(start).saturating_add(1)
        } else {
            0
        };
        if requested_lines > 0 && requested_lines <= 500 {
            let per_line = if max_line == 0 {
                4096
            } else {
                max_line.saturating_mul(4).saturating_add(16)
            };
            budget = budget
                .max(requested_lines.saturating_mul(per_line))
                .min(64 * 1024);
        }
    }

    // 多文件时每个文件至少要放得下一个文件头（约 80 字节）+ 几行正文。
    // 否则会为每个文件各报一条 E_LINE_TOO_LONG，既没用又吃掉全部预算。
    if targets.len() > 1 {
        let per_file = budget / targets.len();
        if per_file < 120 {
            return render::truncate_utf8(
                &format!(
                    "E_BUDGET {} 个文件只有 {} 字节预算（每个约 {}），放不下文件头；请增大 budget 或分批读取",
                    targets.len(),
                    budget,
                    per_file
                ),
                4096,
            )
            .to_string();
        }
    }

    // 多文件且路径有公共目录时，只写一次前缀，各文件只显示剩余部分。
    // 只在确实能省下可观字节时启用，避免给普通场景增加噪声。
    let common_prefix = if targets.len() > 1 {
        let shown: Vec<String> = targets
            .iter()
            .filter_map(|t| FsPath::parse(t).ok().map(|fp| fp.display().to_string()))
            .collect();
        let dir_prefix = common_dir_prefix(&shown);
        // 只在前缀本身足够长（≥24 字符，约相当于两层以上真实目录）时才考虑省略。
        // 这样 /d/code/fsops/src/ 这类常见仓库路径不会触发，
        // 只有深层路径或长临时目录才启用，避免给普通场景增加噪声。
        let worth = dir_prefix.as_ref().map(|p| p.len() >= 24).unwrap_or(false);
        if worth {
            dir_prefix
        } else {
            None
        }
    } else {
        None
    };

    let mut out = String::new();
    // 超大文件分支内部已用 converge_budget 严格控制了长度；
    // 标记后跳过最外层的二次硬切，否则会把续读参数 `end=` 切掉。
    let mut budget_enforced = false;
    // 用省略前缀时必须显式声明，否则 `== a.txt` 会被误读成相对当前目录。
    if let Some(prefix) = &common_prefix {
        out.push_str(&format!("… 以下路径省略公共前缀 {prefix}\n"));
    }
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
            if m.kind == "file"
                && m.size > 32 * 1024 * 1024
                && !crate::documents::is_document(fp.native())
            {
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
                        "E_TOO_LARGE {} {}，超大文件一次只支持一个连续 start/end 区间；请先用 fs_grep 定位行号，再用 start/end 读取该区间\n",
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
                // 已写入 out 的全部内容（含公共前缀声明、路径头、元信息）都要扣掉。
                let header_len = out.len();
                let want = if targets.len() > 1 {
                    (budget / targets.len().max(1)).saturating_sub(usize::from(i > 0))
                } else {
                    budget
                };
                // 文件头已经写进 out，必须从正文预算里扣掉，
                // 否则正文按完整 budget 收敛，加上文件头就超了。
                let want = want.saturating_sub(header_len);
                // 尾注顺序很重要：续读参数最关键，必须优先占预算；
                // 长行提示次要，放后面。若续读参数仍被挤压，下面会报 E_BUDGET，
                // 而不是输出 `… 至 506/90001 行 | 续读: start=507 ` 这种缺 end 的碎片。
                // 闭包返回「正文 + 尾注」整体，让 converge_budget 自己收敛。
                // converge 保证返回值 ≤ budget，因此外层不必再硬切（硬切会吃掉 `end=`）。
                // 尾部预留 8 字节余量：converge 按字节收敛，尾注中的多字节字符
                // 可能让最后一轮刚好越过边界。
                let mut rendered_last = 0usize;
                let mut rendered_total = 0usize;
                let mut line_cut = false;
                let inner_budget = want.saturating_sub(8).max(1);
                let target = render::converge_budget(inner_budget, |bb| {
                    let r = render::render_window(
                        &window.text,
                        RenderOpts {
                            start: 1,
                            end: window.lines,
                            budget: bb,
                            max_line,
                            line_offset: window.first_line.saturating_sub(1),
                            ..Default::default()
                        },
                    );
                    rendered_last = r.last_line;
                    rendered_total = r.total_lines;
                    line_cut = r.line_cut;
                    if r.body.is_empty() && !r.continuation.is_empty() {
                        let line_no = window.first_line + r.continuation[0].0.saturating_sub(1);
                        let need = line_need_bytes(&window.text, r.continuation[0].0);
                        return format!(
                            "E_LINE_TOO_LONG 第 {} 行约 {} 字节，当前 budget={} 放不下；请设 max_line=0 并把 budget 提到至少 {}",
                            line_no, need, bb, need + 200
                        );
                    }
                    let mut text = r.body;
                    // 续读参数优先拼（预算紧张时最先保住它）。
                    if rendered_last > 0 && rendered_last < rendered_total {
                        let absolute_last = window.first_line + rendered_last.saturating_sub(1);
                        text.push_str(&format!(
                            "… 至 {}/{} 行 | 续读: start={} end={}\n",
                            absolute_last,
                            window.total_lines,
                            absolute_last + 1,
                            end
                        ));
                    }
                    if line_cut {
                        let need = line_need_bytes(&window.text, 1);
                        text.push_str(&format!(
                            "… 有长行被截断；读全文用 max_line=0，并需 budget≈{}\n",
                            need + 200
                        ));
                    }
                    text
                });
                // 兜底：续读参数被挤压时明确报错，不给缺 end 的碎片。
                let broken = target.contains("续读:")
                    && !(target.contains("start=") && target.contains("end="));
                if broken {
                    out.push_str(&format!(
                        "E_BUDGET 续读参数被预算截断；请把 budget 提到至少 {}\n",
                        want + 200
                    ));
                } else {
                    out.push_str(&target);
                }
                budget_enforced = true;
                continue;
            }
        }
        let snapshot = if let Some(format) = crate::documents::format_of(fp.native()) {
            match read_document(&fp, format) {
                Ok(doc) => io::Snapshot {
                    bytes: Vec::new(),
                    doc: crate::text::Doc {
                        text: doc.text,
                        encoding: format_label(format),
                        eol: crate::text::Eol::Lf,
                        bom: false,
                        size: doc.size,
                    },
                    hash: doc.hash,
                },
                Err(e) => {
                    out.push_str(&format!("{e}\n"));
                    continue;
                }
            }
        } else {
            match io::read_snapshot(&fp) {
                Ok(s) => s,
                Err(e) => {
                    out.push_str(&format!("{e}\n"));
                    continue;
                }
            }
        };
        let doc = &snapshot.doc;
        let total_lines = line_count(&doc.text);
        // block=true：返回包含 start 行的整个代码块。
        // grep 给出的行号通常在函数内部，直接按 start 展开只能拿到那一行；
        // 这里先向外找到块起始行，再按括号配平确定结束行。
        let per_file_ranges = if arg_bool(args, "block", false) && args.get("ranges").is_none() {
            let lines: Vec<&str> = doc.text.split('\n').collect();
            match enclosing_block(&lines, start) {
                Some((from, to)) => vec![(from, to)],
                None => ranges.clone(),
            }
        } else {
            ranges.clone()
        };
        // 多文件时把公共目录前缀只写一次：深目录下 5 个文件能把 2KB 的路径开销
        // 压到几百字节，而这些字节对模型判断毫无信息量。
        let mut header = if targets.len() > 1 {
            match &common_prefix {
                Some(prefix) => {
                    let shown = fp.display().to_string();
                    let rest = shown.strip_prefix(prefix.as_str()).unwrap_or(&shown);
                    format!("== {rest} ")
                }
                None => format!("== {} ", fp.display()),
            }
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
                &per_file_ranges,
                RenderOpts {
                    budget: body_budget,
                    max_line,
                    ..Default::default()
                },
            );
            if r.body.is_empty() && !r.continuation.is_empty() {
                let line_no = r.continuation[0].0;
                let need = line_need_bytes(&doc.text, line_no);
                return format!(
                    "E_LINE_TOO_LONG 第 {} 行约 {} 字节，当前 budget={} 放不下；请设 max_line=0 并把 budget 提到至少 {}",
                    line_no, need, body_budget, need + 200
                );
            }
            if r.body.is_empty() && r.continuation.is_empty() && r.total_lines > 0 {
                return format!(
                    "E_RANGE {} 区间超出文件范围（共{}行）",
                    fp.display(),
                    r.total_lines
                );
            }
            target.push_str(&r.body);
            // 部分区间完全越界时明确提示，否则模型会以为那些行本来就不存在。
            if let Some(raw) = args.get("ranges").and_then(Value::as_array) {
                if r.total_lines > 0 {
                    let dropped: Vec<String> = raw
                        .iter()
                        .filter_map(Value::as_array)
                        .filter_map(|pair| {
                            if pair.len() != 2 {
                                return None;
                            }
                            let from = pair[0].as_u64()? as usize;
                            let to = pair[1].as_u64()? as usize;
                            (from > r.total_lines).then(|| format!("[{from},{to}]"))
                        })
                        .collect();
                    if !dropped.is_empty() {
                        target.push_str(&format!(
                            "… 区间 {} 超出文件范围（共{}行），已跳过\n",
                            dropped.join(" "),
                            r.total_lines
                        ));
                    }
                }
            }
            if r.line_cut {
                // 每行都提示会白白吃掉预算，这里只说明一次。
                // 同时给出真实成本：单行文件常达上百 KB，只说"用 max_line=0"
                // 会让模型撞上 E_LINE_TOO_LONG。
                let need = line_need_bytes(&doc.text, 1.max(r.first_line));
                target.push_str(&format!(
                    "… 有长行被截断；读全文用 max_line=0，并需 budget≈{}\n",
                    need + 200
                ));
            }
            if !r.continuation.is_empty() {
                target.push_str(&format!(
                    "… {}-{}/{} 行 | 续读: fs_read 同 path ",
                    r.first_line.max(1),
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
    if budget_enforced {
        // 超大文件路径自己已控制长度，不再二次硬切。
        return out.trim_end().to_string();
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
    fn 工具描述声明替代目标() {
        let defs = tool_defs();
        let desc = |name: &str| {
            defs.iter()
                .find(|d| d["name"] == name)
                .and_then(|d| d["description"].as_str())
                .unwrap_or("")
        };
        assert!(desc("fs_read").contains("替代宿主 read_file"));
        assert!(desc("fs_edit").contains("替代 patch"));
        assert!(desc("fs_write").contains("替代 write_file"));
        assert!(desc("fs_glob").contains("替代 find"));
        assert!(desc("fs_grep").contains("替代 grep"));
        assert!(desc("fs_stat").contains("替代 stat"));
    }

    #[test]
    fn 重叠区间合并后不重复输出() {
        let path = 临时文本文件("1\n2\n3\n4\n5\n6\n7\n8\n");
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let out = read(&json!({
            "path": posix_path,
            "ranges": [[1, 5], [3, 8]]
        }));
        // 合并成 1-8，每行只出现一次
        for i in 1..=8 {
            assert_eq!(
                out.matches(&format!("{i}|")).count(),
                1,
                "第 {i} 行重复: {out}"
            );
        }
        assert!(!out.contains("… 区间"), "合并后不应有越界提示: {out}");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn 部分区间越界时明确提示() {
        let path = 临时文本文件("1\n2\n3\n");
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let out = read(&json!({
            "path": posix_path,
            "ranges": [[1, 2], [100, 200]]
        }));
        assert!(out.contains("超出文件范围"), "越界区间应被提示: {out}");
        assert!(out.contains("共3行"), "{out}");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn 编辑符号链接保留链接并修改目标() {
        let dir = std::env::temp_dir().join(format!("fsops-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.txt");
        let link = dir.join("link.txt");
        fs::write(&real, "target content\n").unwrap();
        #[cfg(windows)]
        {
            let ok = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    &link.to_string_lossy(),
                    &real.to_string_lossy(),
                ])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !ok {
                let _ = fs::remove_dir_all(&dir);
                return; // 无权限建符号链接时跳过
            }
        }
        #[cfg(not(windows))]
        {
            if std::os::unix::fs::symlink(&real, &link).is_err() {
                let _ = fs::remove_dir_all(&dir);
                return;
            }
        }

        let fp = FsPath::parse(&link.to_string_lossy()).unwrap();
        io::atomic_write(&fp, b"viaLink content\n").unwrap();

        let still_link = std::fs::symlink_metadata(&link)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        let real_text = fs::read_to_string(&real).unwrap_or_default();
        let _ = fs::remove_dir_all(&dir);
        assert!(still_link, "符号链接被替换成了普通文件");
        assert_eq!(real_text, "viaLink content\n", "目标文件未更新");
    }

    #[test]
    fn 只读文件拒绝编辑并给原因() {
        let path = 临时文本文件("line one\nline two\n");
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&path, perms).unwrap();
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let out = edit_tool(&json!({
            "path": posix_path,
            "edits": [{"old": "line two", "new": "changed"}]
        }));
        // 恢复可写，否则临时文件删不掉
        let mut perms = fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(&path, perms).unwrap();
        let _ = fs::remove_file(path);
        assert!(
            out.starts_with("E_READONLY"),
            "应明确报只读而不是 E_BATCH_PARTIAL: {out}"
        );
        assert!(!out.contains("E_BATCH_PARTIAL"));
    }

    #[test]
    fn 截断尾注的行号范围与实际输出一致() {
        let content = (1..=200)
            .map(|i| format!("line{i} {}", "x".repeat(60)))
            .collect::<Vec<_>>()
            .join("\n");
        let path = 临时文本文件(&content);
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let out = read(&json!({
            "path": posix_path,
            "budget": 600
        }));
        let _ = fs::remove_file(path);

        // 尾注必须写 1-<实际最后行>，不能按固定 10 行倒推
        let note = out.lines().find(|l| l.contains("续读")).unwrap_or("");
        assert!(note.starts_with("… 1-"), "起始行应为 1: {note}");
        // 正文最后一行号必须与尾注结束行一致（尾注本身也含 `|`，要先排除）
        let last_body = out
            .lines()
            .rev()
            .find(|l| l.contains('|') && !l.starts_with('…'))
            .unwrap_or("");
        let line_no = last_body.split('|').next().unwrap_or("");
        assert!(
            note.contains(&format!("1-{line_no}/")),
            "尾注 {note} 与正文末行 {line_no} 不一致"
        );
    }

    #[test]
    fn 空文件报零行而不是一行() {
        let path = 临时文本文件("");
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let out = read(&json!({"path": posix_path}));
        assert!(out.contains(" 0B 0行 "), "空文件应报 0 行: {out}");
        assert!(
            !out.contains("1|\n") && !out.ends_with("1|"),
            "不应输出空行内容: {out}"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn 重复路径只读一次() {
        let path = 临时文本文件("x\n");
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let out = read(&json!({"paths": [posix_path, posix_path]}));
        assert_eq!(out.matches("1|x").count(), 1, "重复路径被读了两次: {out}");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn 稀疏零字节的小文件仍判为二进制() {
        // 8 字节、仅 1 个 NUL：旧实现会误判成 UTF-16 文本。
        assert!(crate::text::is_binary(b"ABC\x00\x01DEF"));
        // 2 字节的 a\0 也无法证明是 UTF-16，按二进制拒绝更安全。
        assert!(crate::text::is_binary(b"a\x00"));
    }

    #[test]
    fn 二进制文件被拒绝当文本读写() {
        let bin = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec();
        assert!(crate::text::is_binary(&bin));
        assert!(!crate::text::is_binary("普通 UTF-8 文本".as_bytes()));
        assert!(!crate::text::is_binary(&[]));
        // UTF-16 文本天然含 NUL，不能误判成二进制
        let utf16: Vec<u8> = "中文 abc"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert!(!crate::text::is_binary(&utf16), "UTF-16 不应判为二进制");
        // 纯中文的 UTF-16 带 BOM：字节流里几乎没有零字节，也必须认作文本
        let mut cn_utf16 = vec![0xFF, 0xFE];
        cn_utf16.extend(
            "中文内容\n第二行\n"
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes()),
        );
        assert!(
            !crate::text::is_binary(&cn_utf16),
            "带 BOM 的纯中文 UTF-16 不应判为二进制"
        );
        // UTF-8 BOM 同样应放行
        let mut utf8_bom = vec![0xEF, 0xBB, 0xBF];
        utf8_bom.extend("中文".as_bytes());
        assert!(!crate::text::is_binary(&utf8_bom));
    }

    #[test]
    fn 显式行区间自动按行数放宽预算() {
        let content = (1..=20)
            .map(|i| format!("第{i}行{}", "中".repeat(250)))
            .collect::<Vec<_>>()
            .join("\n");
        let path = 临时文本文件(&content);
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let all = read(&json!({"path": posix_path, "start": 1, "end": 20}));
        assert!(
            all.contains("20|"),
            "20 行应完整返回: {}",
            &all[..all.len().min(300)]
        );
        assert!(!all.contains("续读"), "行区间不应因默认预算截断");
        let capped = read(&json!({
            "path": posix_path, "start": 1, "end": 20, "budget": 2000
        }));
        assert!(capped.contains("续读"), "显式 budget 仍应生效");
        let _ = fs::remove_file(path);
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
    fn 极小预算下错误响应保持可解析() {
        // 契约变更：错误消息不再被截成 `E_BAD_ARGS` 这样的碎片。
        // 极小 budget 下仍保留能读懂错误的最短长度；正常内容仍严格受 budget 约束。
        for budget in 0..24 {
            let output = read(&json!({"budget": budget}));
            assert!(output.starts_with("E_BAD_ARGS"), "{output}");
            assert!(
                output.len() >= 20,
                "budget={budget} 时错误被截得无法解析: {output}"
            );
            assert!(std::str::from_utf8(output.as_bytes()).is_ok());
        }
        // 非错误内容仍必须严格不超过 budget
        let path = 临时文本文件("a\nb\nc\n");
        let posix_path = path.to_string_lossy().replace('\\', "/");
        for budget in [40, 80, 200] {
            let output = read(&json!({"path": posix_path, "budget": budget}));
            if !output.starts_with("E_") {
                assert!(
                    output.len() <= budget,
                    "budget={budget}，内容超限 {} 字节: {output}",
                    output.len()
                );
            }
        }
        let _ = fs::remove_file(path);
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
    fn 参数类型错误给出可执行提示() {
        // 类型写错时旧实现会静默退回默认值，模型以为生效却得到别的行为。
        let path = 临时文本文件("a\nb\n");
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let cases = [
            (json!({"path": posix_path, "budget": "big"}), "需要数字"),
            (json!({"path": posix_path, "start": "2"}), "需要数字"),
            (json!({"path": 42}), "需要字符串"),
        ];
        for (args, want) in cases {
            let out = call("fs_read", &args);
            assert!(out.starts_with("E_BAD_ARGS"), "{out}");
            assert!(out.contains(want), "缺少类型说明: {out}");
        }
        let _ = fs::remove_file(path);

        let out = call(
            "fs_write",
            &json!({
                "path": "/tmp/fsops-type-probe.txt",
                "content": 123
            }),
        );
        assert!(out.contains("content 需要字符串"), "{out}");
    }

    #[test]
    fn 漏写new时拒绝而不是当作删除() {
        // new 有默认值时，漏参数会静默变成"删除内容"，对 AI 是数据风险。
        let path = 临时文本文件("keep me\n");
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let out = edit_tool(&json!({
            "path": posix_path,
            "edits": [{"old": "keep me"}]
        }));
        let content = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(path);
        assert!(out.starts_with("E_BAD_ARGS"), "应报参数错误: {out}");
        assert!(out.contains("old 与 new"), "{out}");
        assert_eq!(content, "keep me\n", "文件不应被改动");
    }

    #[test]
    fn diff参数输出改动片段() {
        // 改完结构想自查时，diff=true 省掉一次 fs_read 回读。
        let dir = 临时目录("edit-diff");
        let f = dir.join("a.rs");
        fs::write(&f, "fn f() {\n    let timeout = 30;\n}\n").unwrap();
        let fp = FsPath::parse(&f.to_string_lossy()).unwrap();

        let plain = edit_tool(&json!({
            "path": fp.display(),
            "edits": [{"old": "let timeout = 30", "new": "let timeout = 60"}]
        }));
        assert!(!plain.contains("@@"), "默认不应输出 diff: {plain}");

        let with_diff = edit_tool(&json!({
            "path": fp.display(),
            "edits": [{"old": "let timeout = 60", "new": "let timeout = 90"}],
            "diff": true
        }));
        assert!(with_diff.contains("@@ 2"), "{with_diff}");
        assert!(with_diff.contains("-let timeout = 60"), "{with_diff}");
        assert!(with_diff.contains("+let timeout = 90"), "{with_diff}");

        // dry_run + diff：能看到将改什么，且不落盘
        let before = fs::read_to_string(&f).unwrap();
        let dry = edit_tool(&json!({
            "path": fp.display(),
            "edits": [{"old": "let timeout = 90", "new": "let timeout = 120"}],
            "diff": true,
            "dry_run": true
        }));
        assert!(dry.contains("@@ 2"), "{dry}");
        assert_eq!(
            fs::read_to_string(&f).unwrap(),
            before,
            "dry_run 不应改文件"
        );
    }

    #[test]
    fn 同一行多处修改只算一次行数() {
        // `+N -M` 表达"改动了几行"，不是"执行了几次替换"。
        // 旧实现同一行改 3 处会报 +3 -3。
        let dir = 临时目录("edit-same-line");
        let f = dir.join("same.txt");
        fs::write(&f, "fn foo(a: i32) -> i32 { a }\n").unwrap();
        let fp = FsPath::parse(&f.to_string_lossy()).unwrap();
        let out = edit_tool(&json!({
            "path": fp.display(),
            "edits": [
                {"old": "foo", "new": "bar"},
                {"old": "i32", "new": "u64", "all": true}
            ]
        }));
        assert!(out.contains("3 处修改"), "替换次数仍是 3: {out}");
        assert!(out.contains("(+1 -1)"), "只有 1 行变化: {out}");
        assert!(out.contains("@ 1 "), "行号只标 1: {out}");

        // 跨行 + 同行混合：a 拆成两行(+2 -1) + b 单行(+1 -1) = +3 -2
        let m = dir.join("mix.txt");
        fs::write(&m, "a\nb\nc\n").unwrap();
        let fp = FsPath::parse(&m.to_string_lossy()).unwrap();
        let out = edit_tool(&json!({
            "path": fp.display(),
            "edits": [
                {"old": "a", "new": "A1\nA2"},
                {"old": "b", "new": "B"}
            ]
        }));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("(+3 -2)"), "跨行+同行应为 +3 -2: {out}");
    }

    #[test]
    fn 差异统计反映真实行数变化() {
        let dir = 临时目录("edit-diff");
        // 单行替换：+1 -1，行号只标一个
        let single = dir.join("single.txt");
        fs::write(&single, "aaa\nbbb\nccc\n").unwrap();
        let fp = FsPath::parse(&single.to_string_lossy()).unwrap();
        let out = edit_tool(&json!({
            "path": fp.display(),
            "edits": [{"old": "bbb", "new": "XXX"}]
        }));
        assert!(out.contains("(+1 -1)"), "单行替换应为 +1 -1: {out}");
        assert!(out.contains("@ 2 "), "行号应只标 2: {out}");

        // 跨行合并：+1 -2
        let merge = dir.join("merge.txt");
        fs::write(&merge, "a\nb\nc\nd\n").unwrap();
        let fp = FsPath::parse(&merge.to_string_lossy()).unwrap();
        let out = edit_tool(&json!({
            "path": fp.display(),
            "edits": [{"old": "b\nc", "new": "BC"}]
        }));
        assert!(out.contains("(+1 -2)"), "两行合一应为 +1 -2: {out}");

        // 一行拆成三行：+3 -1，行号区间 1-3
        let split = dir.join("split.txt");
        fs::write(&split, "a\nb\n").unwrap();
        let fp = FsPath::parse(&split.to_string_lossy()).unwrap();
        let out = edit_tool(&json!({
            "path": fp.display(),
            "edits": [{"old": "a", "new": "X\nY\nZ"}]
        }));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("(+3 -1)"), "一行拆三行应为 +3 -1: {out}");
        assert!(out.contains("@ 1-3 "), "行号区间应为 1-3: {out}");
    }

    #[test]
    fn 深路径多文件读取省略公共前缀() {
        let deep = std::env::temp_dir().join("fsops-prefix-probe");
        let mut dir = deep.clone();
        for _ in 0..12 {
            dir = dir.join("deep-directory-padding-segment");
        }
        let _ = fs::remove_dir_all(&deep);
        fs::create_dir_all(&dir).unwrap();
        let names = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
        let paths: Vec<String> = names
            .iter()
            .map(|n| {
                let p = dir.join(n);
                fs::write(&p, "content\n").unwrap();
                FsPath::parse(&p.to_string_lossy())
                    .unwrap()
                    .display()
                    .to_string()
            })
            .collect();

        let out = read(&json!({ "paths": paths }));
        // 浅路径（仓库 src 这类）不应触发省略
        let shallow: Vec<String> = ["io.rs", "text.rs", "edit.rs"]
            .iter()
            .map(|n| {
                let p = dir.join(n);
                fs::write(&p, "content\n").unwrap();
                FsPath::parse(&p.to_string_lossy())
                    .unwrap()
                    .display()
                    .to_string()
            })
            .collect();
        let _ = fs::remove_dir_all(&deep);

        assert!(out.contains("省略公共前缀"), "深路径应省略公共前缀: {out}");
        // 每个文件仍能看到自己的可辨识后缀，且正文完整
        for n in names {
            assert!(out.contains(n), "缺少 {n}: {out}");
        }
        assert!(out.contains("content"), "正文丢失: {out}");
        // 单文件不省略
        let single = read(&json!({ "path": shallow[0] }));
        assert!(!single.contains("省略公共前缀"), "{single}");
    }

    #[test]
    fn 参数超上限时显式拒绝而不是静默截断() {
        // 静默截断会让模型以为处理了全部输入。
        let dir = 临时目录("over-limit");
        let mut many = Vec::new();
        for i in 0..25 {
            let p = dir.join(format!("f{i}.txt"));
            fs::write(&p, "x\n").unwrap();
            many.push(
                FsPath::parse(&p.to_string_lossy())
                    .unwrap()
                    .display()
                    .to_string(),
            );
        }
        let out = read(&json!({ "paths": many, "budget": 4096 }));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("E_BAD_ARGS"), "超上限应拒绝: {out}");
        assert!(out.contains("最多读 20 个文件"), "{out}");
        assert!(out.contains("当前 25 个"), "{out}");

        // context 超上限不再静默夹到 5
        let out = grep_tool(&json!({
            "path": "/tmp",
            "pattern": "x",
            "context": 9
        }));
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
        assert!(out.contains("context 最大 5"), "{out}");
    }

    #[test]
    fn 多文件预算不足时给一条提示而不是逐文件报错() {
        // 旧实现把预算平分后每个文件各报一条 E_LINE_TOO_LONG，
        // 20 个文件会输出 348 字节的错误垃圾且被截成半截。
        let dir = 临时目录("tiny-budget");
        let mut files = Vec::new();
        for i in 0..20 {
            let p = dir.join(format!("f{i}.txt"));
            fs::write(&p, format!("content{i}\n")).unwrap();
            files.push(
                FsPath::parse(&p.to_string_lossy())
                    .unwrap()
                    .display()
                    .to_string(),
            );
        }
        let out = read(&json!({ "paths": files, "budget": 400 }));
        // 预算充足时可以正常读
        let ok = read(&json!({ "paths": files, "budget": 5000 }));
        let _ = fs::remove_dir_all(&dir);

        assert!(out.starts_with("E_BUDGET"), "应给一条预算提示: {out}");
        assert!(out.contains("放不下文件头"), "{out}");
        assert_eq!(
            out.matches("E_LINE_TOO_LONG").count(),
            0,
            "不应逐文件刷错误: {out}"
        );
        assert_eq!(ok.matches("E_LINE_TOO_LONG").count(), 0, "{ok}");
        assert!(ok.contains("content0"), "预算充足应能读到内容");
    }

    #[test]
    fn 读取失败时错误里带出具体路径() {
        let dir = 临时目录("read-path-err");
        let good = dir.join("good.txt");
        let missing = dir.join("missing.txt");
        fs::write(&good, "ok\n").unwrap();
        let good_fp = FsPath::parse(&good.to_string_lossy()).unwrap();
        let missing_fp = FsPath::parse(&missing.to_string_lossy()).unwrap();
        let _ = fs::remove_dir_all(&dir);

        let err = read(&json!({ "path": missing_fp.display() }));
        assert!(err.starts_with("E_READ"), "{err}");
        assert!(
            err.contains(&missing_fp.display().to_string()),
            "错误必须带出具体路径: {err}"
        );

        // 多文件部分失败时也要能定位到失败的那个
        let out = read(&json!({
            "paths": [good_fp.display(), missing_fp.display()]
        }));
        assert!(
            out.contains(&missing_fp.display().to_string()),
            "多文件错误必须指明是哪个文件: {out}"
        );
    }

    #[test]
    fn 长行截断时提示一次全文读法() {
        let content = "x".repeat(5000);
        let path = 临时文本文件(&content);
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let out = read(&json!({
            "path": posix_path,
            "max_line": 80
        }));
        // 不截断时不应有提示
        let full = read(&json!({
            "path": posix_path,
            "max_line": 0
        }));
        let _ = fs::remove_file(path);

        assert_eq!(
            out.matches("max_line=0").count(),
            1,
            "截断提示只应出现一次: {out}"
        );
        assert!(out.contains("有长行被截断"), "{out}");
        assert!(!full.contains("有长行被截断"), "未截断时不应提示: {full}");
    }

    #[test]
    fn 每个声明参数都有对应实现分支() {
        // 防止再出现「schema 声明了 dry_run 但代码没实现」或「实现了但模型看不到」。
        // 这里只校验最容易漏的布尔/可选开关，必须同时出现在 schema 与参数读取里。
        let src = include_str!("tools.rs");
        let defs = tool_defs();
        let mut problems = Vec::new();
        for def in &defs {
            let name = def["name"].as_str().unwrap_or("");
            let props = def["inputSchema"]["properties"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            for key in props.keys() {
                // 实际调用形如 arg_bool(args, "dirs", false)，引号后跟逗号而不是右括号，
                // 因此只匹配到引号为止。
                let used = [
                    format!("arg_str(args, \"{key}\""),
                    format!("arg_bool(args, \"{key}\""),
                    format!("arg_usize(args, \"{key}\""),
                    format!("args.get(\"{key}\")"),
                    format!("\"{key}\" =>"),
                ]
                .iter()
                .any(|pat| src.contains(pat.as_str()))
                    || key == "path"
                    || key == "content"
                    || key == "pattern"
                    || key == "old"
                    || key == "new"
                    || key == "all";
                if !used {
                    problems.push(format!("{name}.{key}"));
                }
            }
        }
        assert!(
            problems.is_empty(),
            "schema 声明了但代码从未读取的参数: {problems:?}"
        );
        // 反向：dry_run 必须在 fs_write 与 fs_edit 的 schema 里可见
        for tool in ["fs_write", "fs_edit"] {
            let def = defs
                .iter()
                .find(|d| d["name"] == tool)
                .unwrap_or_else(|| panic!("缺少 {tool}"));
            assert!(
                def["inputSchema"]["properties"]["dry_run"].is_object(),
                "{tool} 的 schema 缺少 dry_run"
            );
        }
    }

    #[test]
    fn 长行截断提示给出可执行的budget() {
        // 只说"读全文用 max_line=0"是死路：单行文件常达上百 KB，
        // 模型照做会撞上 E_LINE_TOO_LONG，然后在两条消息之间来回试。
        let content = "var a=1;".repeat(20000); // 约 160KB 单行
        let path = 临时文本文件(&content);
        let posix_path = path.to_string_lossy().replace('\\', "/");

        let default_out = read(&json!({"path": posix_path}));
        let hint_out = read(&json!({"path": posix_path, "max_line": 0}));
        let _ = fs::remove_file(path);

        assert!(default_out.contains("有长行被截断"), "{default_out}");
        assert!(
            default_out.contains("budget≈"),
            "尾注必须给出所需 budget: {default_out}"
        );
        assert!(hint_out.contains("E_LINE_TOO_LONG"), "{hint_out}");
        assert!(
            hint_out.contains("至少"),
            "错误必须给出可执行的 budget 数值: {hint_out}"
        );
    }

    #[test]
    fn block参数按括号配平定位代码块() {
        // 模型从 grep 得知行号后无法预知函数多长：猜小了要重读，猜大了浪费字节。
        let content = "\
fn outer() {
    let x = 1;
    if x > 0 {
        println!(\"hi\");
    }
    let y = 2;
}
fn next_fn() {
    let z = 3;
}
";
        let path = 临时文本文件(content);
        let posix_path = path.to_string_lossy().replace('\\', "/");
        // 从第 1 行开始应定位到第 7 行（outer 的闭合括号）
        let out = read(&json!({
            "path": posix_path,
            "start": 1,
            "block": true,
            "budget": 100000
        }));
        // grep 典型输出：命中行在函数内部（第 2 行是 let，没有 {），
        // 应向外扩展到整个 outer，而不是只给这一行。
        let inside = read(&json!({
            "path": posix_path,
            "start": 2,
            "block": true,
            "budget": 100000
        }));
        // 命中行在嵌套块内部（第 4 行在 if 里）→ 扩展为最近的 if 块
        let nested = read(&json!({
            "path": posix_path,
            "start": 4,
            "block": true,
            "budget": 100000
        }));
        let _ = fs::remove_file(path);

        let body: Vec<&str> = out.lines().filter(|l| l.contains('|')).collect();
        assert_eq!(body.len(), 7, "应读到 outer 的 7 行: {out}");
        assert!(body[6].starts_with("7|"), "末行应为第 7 行: {out}");
        assert!(!out.contains("fn next_fn"), "不应越界到下一个函数: {out}");

        let ib: Vec<&str> = inside.lines().filter(|l| l.contains('|')).collect();
        assert_eq!(ib.len(), 7, "块内命中应扩展到整个 outer: {inside}");
        assert!(
            ib[0].starts_with("1|fn outer"),
            "应从 outer 开头开始: {inside}"
        );

        let nb: Vec<&str> = nested.lines().filter(|l| l.contains('|')).collect();
        assert_eq!(nb.len(), 3, "嵌套块内命中应扩展到最近的 if: {nested}");
        assert!(nb[0].starts_with("3|"), "应从 if 行开始: {nested}");
    }

    #[test]
    fn count模式默认不受limit截断() {
        // count 是"统计"：受 limit=100 截断会得出错误数字（203 处被报成 100）。
        let dir = std::env::temp_dir().join(format!("fsops-count-full-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("many.txt"), "needle\n".repeat(150)).unwrap();
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();

        // 默认（不传 limit）应给出完整的 150
        let out = grep_tool(&json!({
            "path": root.display(),
            "pattern": "needle",
            "output": "count"
        }));
        // 显式 limit=100 仍截断并标注
        let capped = grep_tool(&json!({
            "path": root.display(),
            "pattern": "needle",
            "output": "count",
            "limit": 100
        }));
        // matches 模式默认仍受 limit 约束
        let matches = grep_tool(&json!({
            "path": root.display(),
            "pattern": "needle",
            "output": "matches"
        }));
        let _ = fs::remove_dir_all(&dir);

        assert!(
            out.contains("many.txt:150"),
            "默认 count 应给完整数字: {out}"
        );
        assert!(!out.contains("本页可见"), "未截断不应标注: {out}");
        assert!(capped.contains("本页可见"), "显式 limit 仍应标注: {capped}");
        assert!(
            matches.contains("next_offset"),
            "matches 仍受 limit 约束: {matches}"
        );
    }

    #[test]
    fn glob预算截断时说明已给多少条() {
        // 只报"输出截断"而不说给了多少/共多少，模型无法判断要不要继续。
        let dir = std::env::temp_dir().join(format!("fsops-glob-trunc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..120 {
            fs::write(dir.join(format!("file-with-name-{i:03}.txt")), "x").unwrap();
        }
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();
        let out = glob_tool(&json!({
            "path": root.display(),
            "pattern": "*.txt",
            "limit": 500,
            "budget": 900
        }));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("输出截断"), "{out}");
        assert!(out.contains("已给"), "应说明已给多少条: {out}");
        assert!(out.contains("120"), "应给出总匹配数: {out}");
    }

    #[test]
    fn 写入目录时报目录错误而不是拒绝访问() {
        // 旧实现会在 rename 阶段报 `E_RENAME 拒绝访问`，模型看不出真正原因。
        let dir = 临时目录("write-to-dir");
        let fp = FsPath::parse(&dir.to_string_lossy()).unwrap();
        let out = write(&json!({
            "path": fp.display(),
            "content": "x\n"
        }));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("E_IS_DIR"), "应报目录错误: {out}");
        assert!(out.contains("是目录"), "{out}");
        assert!(!out.contains("E_RENAME"), "不应泄漏底层 rename 错误: {out}");
    }

    #[test]
    fn 符号链接stat报出目标与大小() {
        // 链接自身 size 是 0；只报 0B 会让模型以为目标文件是空的。
        let dir = 临时目录("stat-link");
        let real = dir.join("real.txt");
        let link = dir.join("link.txt");
        fs::write(&real, "content\n").unwrap();
        let made = std::process::Command::new("cmd")
            .args([
                "/c",
                "mklink",
                &link.to_string_lossy(),
                &real.to_string_lossy(),
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !made {
            let _ = fs::remove_dir_all(&dir);
            return; // 无权限建符号链接时跳过
        }
        let fp = FsPath::parse(&link.to_string_lossy()).unwrap();
        let out = stat(&json!({ "path": fp.display(), "detail": "full" }));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains(" link "), "应标记为 link: {out}");
        assert!(out.contains("->"), "应给出目标: {out}");
        // 目标大小必须出现（链接自身是 0B，目标是 8B）
        let after_arrow = out.split("->").nth(1).unwrap_or("");
        assert!(after_arrow.contains('B'), "箭头后应给出目标大小: {out}");
        assert!(!out.contains("//?/"), "不应泄漏长路径前缀: {out}");
    }

    #[test]
    fn 写入dry_run不落盘() {
        let dir = std::env::temp_dir().join(format!("fsops-wdry-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let keep = dir.join("keep.txt");
        fs::write(&keep, "IMPORTANT DATA\n").unwrap();
        let fp = FsPath::parse(&keep.to_string_lossy()).unwrap();

        let out = write(&json!({
            "path": fp.display(),
            "content": "CLOBBERED\n",
            "dry_run": true
        }));
        assert!(out.starts_with("ok dry_run"), "{out}");
        assert_eq!(
            fs::read_to_string(&keep).unwrap(),
            "IMPORTANT DATA\n",
            "dry_run 不应改动已有文件"
        );

        let fresh = dir.join("brand-new.txt");
        let fp2 = FsPath::parse(&fresh.to_string_lossy()).unwrap();
        let out = write(&json!({
            "path": fp2.display(),
            "content": "hi\n",
            "dry_run": true
        }));
        let created = fresh.exists();
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("ok dry_run"), "{out}");
        assert!(!created, "dry_run 不应创建新文件");
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
    fn 超大文件续读尾注不被预算截断() {
        // 大区间强制截断：尾注必须同时含 start= 和 end=，
        // 且总长度不超 budget（此前会出现 `续读: start=112` 缺 end 的碎片）。
        use std::io::Write;
        let dir = 临时目录("read-large-note");
        let path = dir.join("large.txt");
        let line = format!("{}\n", "y".repeat(1023));
        let mut file = fs::File::create(&path).unwrap();
        for _ in 0..34_000 {
            file.write_all(line.as_bytes()).unwrap();
        }
        file.sync_all().unwrap();
        drop(file);

        let mut problems = Vec::new();
        for budget in [700usize, 1000, 2000, 2048] {
            let out = read(&json!({
                "path": path.to_string_lossy(),
                "start": 100,
                "end": 2000,
                "max_line": 50,
                "budget": budget
            }));
            if out.len() > budget {
                problems.push(format!("budget={budget} 超限 {} 字节", out.len()));
            }
            if out.contains("续读") && !(out.contains("start=") && out.contains("end=")) {
                problems.push(format!("budget={budget} 续读参数被截断: {out}"));
            }
        }
        fs::remove_dir_all(dir).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn 读取区间完全越界时给出错误() {
        let path = 临时文本文件("one\ntwo\nthree\n");
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let output = read(&json!({"path": posix_path, "start": 99, "end": 120}));
        fs::remove_file(path).unwrap();

        assert!(output.starts_with("E_RANGE"), "{output}");
        assert!(
            output.contains("超出文件范围") && output.contains("共3行"),
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
    fn edit_schema两处嵌套结构一致() {
        // `edits` 与 `files[].edits` 分开手写会漂移；
        // 嵌套处若只剩 {"type":"object"}，模型只能猜字段名。
        let defs = tool_defs();
        let edit = defs
            .iter()
            .find(|def| def["name"] == "fs_edit")
            .expect("缺少 fs_edit");
        let props = &edit["inputSchema"]["properties"];
        let single = &props["edits"]["items"];
        let nested = &props["files"]["items"]["properties"]["edits"]["items"];
        assert_eq!(
            single["properties"], nested["properties"],
            "两处 items 必须同形"
        );
        assert_eq!(nested["required"], json!(["old"]));
        for name in ["old", "new", "all", "start", "end"] {
            assert!(
                nested["properties"][name].is_object(),
                "files[].edits 缺少字段 {name}"
            );
        }
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
        // 错误消息保证可解析，因此按 ERROR_MIN_BUDGET 而非原始 budget 判定
        assert!(
            output.len() <= crate::render::ERROR_MIN_BUDGET.max(30),
            "预算超限: {output}"
        );
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
    fn grep极小预算下错误响应保持可解析() {
        // 契约变更：错误消息保留能读懂的最短长度，不再被截成 `E_BAD_ARGS`。
        for budget in 0..24 {
            let output = grep_tool(&json!({ "budget": budget }));
            assert!(output.starts_with("E_BAD_ARGS"), "{output}");
            assert!(
                output.len() >= 20,
                "budget={budget} 时错误被截得无法解析: {output}"
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
    fn grep_报告被跳过的大文件() {
        let tmp = std::env::temp_dir().join(format!("fsops-grep-large-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("small.txt"), "needle\n").unwrap();
        let big = "x".repeat(9 * 1024 * 1024);
        fs::write(tmp.join("big.log"), &big).unwrap();

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let r = crate::search::grep(
            &root,
            &GrepOpts {
                pattern: "needle".into(),
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&tmp);

        assert_eq!(r.match_count, 1, "只有小文件命中");
        assert_eq!(r.files_skipped_large, 1, "大文件必须被计数而不是静默跳过");
        assert!(r.largest_skipped >= 9 * 1024 * 1024);
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
    // 参数错误要给出可执行的修法，而不是漏出 serde 内部诊断。
    let edits: Vec<EditSpec> = serde_json::from_value(value.clone()).map_err(|e| {
        if value.is_null() {
            "E_BAD_ARGS 缺少 edits；需要 [{\"old\":\"原文\",\"new\":\"替换\"}, ...]".to_string()
        } else if let Some(list) = value.as_array() {
            let bad = list.iter().position(|item| {
                item.get("old").and_then(Value::as_str).is_none()
                    || item.get("new").and_then(Value::as_str).is_none()
            });
            match bad {
                Some(i) => format!(
                    "E_BAD_ARGS edits[{}] 必须同时有字符串 old 与 new；当前={}",
                    i,
                    list.get(i).map(|v| v.to_string()).unwrap_or_default()
                ),
                None => format!("E_BAD_ARGS edits 解析失败: {e}"),
            }
        } else {
            "E_BAD_ARGS edits 必须是数组，每项形如 {\"old\":\"原文\",\"new\":\"替换\"}".to_string()
        }
    })?;
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
    // 写前预检：只读文件在 rename 阶段才会失败，提前拒绝并给出原因，
    // 避免把“一处都没写”误报成 E_BATCH_PARTIAL。
    if fp
        .native()
        .metadata()
        .map(|m| m.permissions().readonly())
        .unwrap_or(false)
    {
        return Err(format!(
            "E_READONLY {} 是只读文件；先去掉只读属性再编辑",
            fp.display()
        ));
    }
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
            if let Some(format) = crate::documents::format_of(fp.native()) {
                return format!(
                    "{} 不支持精确替换 {}；请用 fs_write 整体重建",
                    doc_format_error(format),
                    fp.display()
                );
            }
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
        if let Some(format) = crate::documents::format_of(fp.native()) {
            return format!(
                "{} 不支持精确替换 {}；请用 fs_write 整体重建",
                doc_format_error(format),
                fp.display()
            );
        }
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
            if arg_bool(args, "diff", false) {
                out.push_str(&render_hunks(&item.result.hunks));
            }
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
        let mut out = format!(
            "ok {} 处修改 (+{} -{}) @ {}  {}  hash={}",
            item.result.applied,
            item.result.added_lines,
            item.result.removed_lines,
            format_loci(&item.result.loci),
            item.fp.display(),
            io::hash_hex(&item.bytes)
        );
        if arg_bool(args, "diff", false) {
            out.push_str(&render_hunks(&item.result.hunks));
        }
        return out;
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
        if arg_bool(args, "diff", false) {
            out.push_str(&render_hunks(&item.result.hunks));
        }
    }
    out
}

/// 把改动渲染成紧凑 @@ 片段；`fs_edit diff=true` 用它省掉一次回读。
fn render_hunks(hunks: &[crate::edit::Hunk]) -> String {
    const MAX_LINES: usize = 60;
    let mut out = String::new();
    let mut shown = 0usize;
    for h in hunks {
        if shown >= MAX_LINES {
            break;
        }
        out.push_str(&format!("\n@@ {}", h.line));
        for l in h.old.split('\n') {
            if shown >= MAX_LINES {
                break;
            }
            out.push_str(&format!("\n-{l}"));
            shown += 1;
        }
        for l in h.new.split('\n') {
            if shown >= MAX_LINES {
                break;
            }
            out.push_str(&format!("\n+{l}"));
            shown += 1;
        }
    }
    if shown >= MAX_LINES {
        out.push_str("\n… diff 已截断");
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
        // 类型写错时也要说清是类型问题，否则模型会以为漏传了参数而反复重试。
        None => match args.get("content") {
            Some(other) => {
                return format!(
                    "E_BAD_ARGS content 必须是字符串；当前是 {}",
                    type_name(other)
                )
            }
            None => return "E_BAD_ARGS 缺少 content".into(),
        },
    };
    let fp = match FsPath::parse(path) {
        Ok(p) => p,
        Err(e) => return format!("E_BAD_PATH {e}"),
    };
    let _write_lock = match io::lock_path(&fp, 3000) {
        Ok(lock) => lock,
        Err(e) => return e,
    };

    // DOCX 只支持整体重建，不做文本编辑；这避免把 Word 的复杂格式静默拍平。
    if let Some(format) = crate::documents::format_of(fp.native()) {
        if format == crate::documents::Format::Pdf {
            return format!(
                "E_PDF 不支持写入 {}；PDF 目前只读，请生成新的 DOCX 或文本文件",
                fp.display()
            );
        }
        if let Some(expect) = arg_str(args, "expect_hash") {
            if !fp.exists() {
                return "E_STALE 目标文件不存在，无法校验 hash".into();
            }
            let bytes = match io::read_bytes(&fp.wide()) {
                Ok(bytes) => bytes,
                Err(e) => return e,
            };
            if io::hash_hex(&bytes) != expect {
                return format!("E_STALE 文件已变化（当前 hash={}）", io::hash_hex(&bytes));
            }
        }
        if arg_bool(args, "dry_run", false) {
            return format!(
                "ok dry_run: 将重建 DOCX {}（{}字符，未写入）",
                fp.display(),
                content.chars().count()
            );
        }
        return match crate::documents::write_docx(&fp, content) {
            Ok(out) => out,
            Err(e) => e,
        };
    }

    // 已有文件：沿用其编码与换行风格，避免把 CRLF 仓库改成 LF
    let existing = if fp.exists() && !fp.is_dir() {
        // 不能吞掉 E_BINARY：二进制文件被当文本读回再写会直接毁掉内容。
        match io::read_snapshot(&fp) {
            Ok(snapshot) => Some(snapshot),
            Err(e) if e.starts_with("E_BINARY") => return e,
            Err(_) => None,
        }
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

    if arg_bool(args, "dry_run", false) {
        // 预检已完成（路径、编码、换行、hash），这里不落盘。
        let note = if existing.is_some() {
            "将覆盖已有文件"
        } else {
            "将新建文件"
        };
        return format!(
            "ok dry_run: {} {} {}行 hash={}（{note}，未写入）",
            fp.display(),
            io::human_size(bytes.len() as u64),
            line_count(content),
            io::hash_hex(&bytes)
        );
    }

    if let Err(e) = io::atomic_write(&fp, &bytes) {
        return e;
    }
    format!(
        "ok 写入 {} {} {}行 hash={}",
        fp.display(),
        io::human_size(bytes.len() as u64),
        line_count(content),
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
        time_budget_ms: arg_usize(args, "time_budget_ms", 5000) as u128,
        no_ignore: arg_bool(args, "no_ignore", false),
    };
    match walk::glob_with_report(&root, pattern, &opts) {
        Ok((entries, report)) => {
            let timed_out = report.timed_out;
            if entries.is_empty() {
                if timed_out {
                    format!(
                        "E_TIMEOUT 遍历 {} 用尽 {}ms 时间预算（已访问 {} 项），未找到匹配；\
                         缩小 path 或加更具体的 pattern 再试",
                        root.display(),
                        opts.time_budget_ms,
                        report.entries_visited
                    )
                } else {
                    format!("ok 无匹配（{} pattern={pattern}）", root.display())
                }
            } else {
                let body = walk::render_entries(&entries, arg_bool(args, "detail", false));
                let body = body.trim_end();
                let note = if timed_out {
                    format!(
                        "\n… 时间预算用尽（{}ms，已访问 {} 项），结果可能不完整",
                        opts.time_budget_ms, report.entries_visited
                    )
                } else {
                    String::new()
                };
                let allowed = budget.saturating_sub(note.len());
                let rendered = if body.len() <= allowed {
                    body.to_string()
                } else {
                    // 截断时说明"已输出多少 / 共多少"，否则模型无法判断
                    // 是"全部就这些"还是"被切掉了很多"。
                    let room = allowed.saturating_sub(80);
                    let shown = if room == 0 {
                        0
                    } else {
                        render::truncate_utf8(body, room).lines().count()
                    };
                    let marker = if report.stopped && entries.len() >= opts.limit {
                        format!(
                            "\n… 输出截断：已给 {shown} 条，limit={}；减小 detail、加 pattern 或增大 budget",
                            opts.limit
                        )
                    } else {
                        format!(
                            "\n… 输出截断：已给 {shown} 条（共 {} 条匹配）；增大 budget",
                            entries.len()
                        )
                    };
                    if room == 0 {
                        render::truncate_utf8(body, allowed).to_string()
                    } else {
                        format!("{}{marker}", render::truncate_utf8(body, room))
                    }
                };
                format!("{rendered}{note}")
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
        Some(value) if value > 5 => {
            // 静默夹到 5 会让模型以为拿到了 9 行上下文；超限直接报错。
            return render::truncate_utf8(
                &format!("E_BAD_ARGS context 最大 5，当前 {value}"),
                budget.max(120),
            )
            .to_string();
        }
        Some(value) => value as usize,
        None if output == GrepOutput::Snippets => 2,
        None => 0,
    };
    let mut limit = arg_usize(args, "limit", 100);
    if limit == 0 {
        return render::truncate_utf8("E_BAD_ARGS limit 必须大于 0；省略表示默认 100", budget)
            .to_string();
    }
    // count 模式是"统计"，受 limit 截断会得出错误的数字
    // （例如 203 处命中被 limit=100 截成 100）。它只有每个文件一行输出，
    // 预算由 budget 兜底，因此不受 limit 约束。
    if output == GrepOutput::Count && args.get("limit").is_none() {
        limit = usize::MAX;
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
        no_ignore: arg_bool(args, "no_ignore", false),
        ..Default::default()
    };
    match search::grep(&root, &opts) {
        Ok(r) => {
            // 大文件被跳过时必须回报，否则模型会把“没搜到”当成“不存在”。
            let skip_note = if r.files_skipped_large > 0 {
                format!(
                    "\n… 跳过 {} 个超过 {} 的文件（最大 {}）：需要时用 fs_read 直接读取",
                    r.files_skipped_large,
                    io::human_size(opts.max_file_size),
                    io::human_size(r.largest_skipped)
                )
            } else {
                String::new()
            };
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
                    &format!("ok 无命中（扫描 {} 个文件）{}", r.files_scanned, skip_note),
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
                if !skip_note.is_empty() {
                    text.push_str(&skip_note);
                }
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
            if let Some(format) = crate::documents::format_of(fp.native()) {
                let doc = match read_document(&fp, format) {
                    Ok(doc) => doc,
                    Err(e) => {
                        out.push_str(&format!("{e}\n"));
                        continue;
                    }
                };
                if detail == "hash" {
                    out.push_str(&format!(
                        "{} file {} {} {} LF {} hash={}\n",
                        fp.display(),
                        io::human_size(m.size),
                        io::fmt_time(m.mtime),
                        format_label(format),
                        if m.readonly { "ro" } else { "rw" },
                        doc.hash
                    ));
                } else {
                    out.push_str(&format!(
                        "{} file {} {} {} LF {} {}行 hash={}\n",
                        fp.display(),
                        io::human_size(m.size),
                        io::fmt_time(m.mtime),
                        format_label(format),
                        if m.readonly { "ro" } else { "rw" },
                        line_count(&doc.text),
                        doc.hash
                    ));
                }
                continue;
            }
            if let Ok(bytes) = io::read_bytes(&fp.wide()) {
                if crate::text::is_binary(&bytes) {
                    out.push_str(&format!(
                        "{} file {} {} binary {} hash={}\n",
                        fp.display(),
                        io::human_size(m.size),
                        io::fmt_time(m.mtime),
                        if m.readonly { "ro" } else { "rw" },
                        io::hash_hex(&bytes)
                    ));
                    continue;
                }
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
                            line_count(&d.text),
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
            // 符号链接自身 size 是 0，只报 0B 会让模型以为目标文件是空的。
            match &m.link {
                Some((target, target_size)) => out.push_str(&format!(
                    "{} {} {} {} -> {} {}\n",
                    fp.display(),
                    m.kind,
                    io::human_size(m.size),
                    io::fmt_time(m.mtime),
                    target,
                    io::human_size(*target_size)
                )),
                None => out.push_str(&format!(
                    "{} {} {} {}\n",
                    fp.display(),
                    m.kind,
                    io::human_size(m.size),
                    io::fmt_time(m.mtime)
                )),
            }
        }
    }
    out.trim_end().to_string()
}
