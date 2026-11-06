//! 工具层：8 个工具的定义与执行。
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
    let mut defs = vec![
        json!({
            "name": "fs_read",
            "description": "读取文件。优先用本工具替代宿主 read_file/cat/head/tail/Get-Content。默认只返回前 12KB 并给出续读方式；支持行范围、多文件、DOCX/PDF 正文、自动识别 GBK/UTF-16/BOM 与 CRLF。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "文件路径，如 /d/repo/app/src/main.rs"},
                    "paths": {"type": "array", "items": {"type": "string"}, "maxItems": 20, "description": "一次读多个文件（最多 20 个），所有文件共用 start/end"},
                    "reads": {
                        "type": "array",
                        "maxItems": 20,
                        "description": "一次读多个文件的不同区间，每项 {path,start?,end?,ranges?,block?,max_line?,scan?}；与 path/paths/ranges/start/end/block/summary/focus 互斥，预算按项均分",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "start": {"type": "integer"},
                                "end": {"type": "integer"},
                                "ranges": {"type": "array", "items": {"type": "array", "items": {"type": "integer", "minimum": 1}, "minItems": 2, "maxItems": 2}},
                                "block": {"type": "boolean"},
                                "max_line": {"type": "integer"},
                                "scan": {"type": "string", "enum": ["window", "full"]}
                            },
                            "required": ["path"]
                        }
                    },
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
                    "max_line": {"type": "integer", "description": "每行最多字符数，默认 240；0 表示不截断长行"},
                    "scan": {
                        "type": "string",
                        "enum": ["window", "full"],
                        "description": "仅对 >32MB 文件生效：window（默认）只读到请求区间末尾并探测是否还有内容，毫秒级返回，但不给全文 hash/总行数；full 全文件扫描，给精确 hash 与总行数"
                    },
                    "summary": {"type": "boolean", "description": "只输出结构摘要：声明行 + 行号，不展开函数体；行号可直接接 block=true 读实现。与 ranges/block/end 互斥，数据文件与超大文件不支持"},
                    "focus": {"type": "string", "description": "只返回匹配该正则的完整代码块（含上下文），替代「先 fs_grep 再 block=true」两步。仅单文件，与 paths/reads/ranges/start/end/block/summary 互斥"},
                    "force": {"type": "boolean", "description": "绕过未变更短路，强制返回正文。默认情况下，同一 MCP 会话里同文件同 hash 同区间再读只回一行摘要"}
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
                                "expect_hash": {"type": "string", "description": "该文件的并发保护指纹"},
                                "normalize_eol": {"type": "boolean", "description": "该文件含混合换行时，显式允许归一为 LF"}
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
            "description": "按 glob 找文件。优先替代 find/Glob/Get-ChildItem。尊重 .gitignore，自动跳过 node_modules/target 等。默认只给路径，detail=true 附大小与时间；map=true 输出按引用重要性排序的 repo map（文件+符号），一次回答先看哪些文件。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "搜索根目录，默认当前目录"},
                    "pattern": {"type": "string", "description": "如 **/*.rs，多个用逗号分隔；省略表示全部"},
                    "dirs": {"type": "boolean", "description": "是否包含目录"},
                    "detail": {"type": "boolean", "description": "是否附大小与修改时间（默认只给路径，省 token）"},
                    "limit": {"type": "integer", "description": "最多返回条数，默认 200；达到后提前停止"},
                    "offset": {"type": "integer", "description": "跳过前 N 个匹配条目，用于 next_offset 续页"},
                    "time_budget_ms": {"type": "integer", "description": "目录遍历时间预算，默认 5000 毫秒；超时返回已找到结果并标注"},
                    "no_ignore": {"type": "boolean", "description": "忽略 .gitignore 与 node_modules/target 等默认跳过项；只在明确要查依赖源码或构建产物时用"},
                    "budget": {"type": "integer", "description": "最终响应最多字节数，默认 8192"},
                    "map": {"type": "boolean", "description": "输出 repo map：按符号被引用程度、入口点和 focus 排序的文件与符号，一次调用回答先看哪些文件"},
                    "focus": {"type": "string", "description": "map=true 时的关注词，空格分隔；文件名或符号命中会加权"},
                    "max_files": {"type": "integer", "description": "map=true 时最多扫描的文件数，默认 2000"}
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
                        "description": "files 每个匹配文件只给路径；count 给路径和真实行命中数（默认完整统计）；matches 给命中行；snippets 默认带 2 行上下文"
                    },
                    "context": {"type": "integer", "description": "上下文行数（最多 5）；matches 默认 0，snippets 默认 2"},
                    "limit": {"type": "integer", "description": "本页上限：files 按文件，matches/snippets 按命中，count 显式设置时按命中；默认 100（count 省略时完整统计），必须大于 0"},
                    "max_per_file": {"type": "integer", "description": "每个文件最多返回的命中数，默认不限制；省略部分会在尾注说明"},
                    "offset": {"type": "integer", "description": "files 跳过前 N 个匹配文件，其他模式跳过前 N 个命中；用于 next_offset 续取"},
                    "parallel": {"type": "boolean", "description": "大仓库并行扫描；输出与串行一致，默认 false"},
                    "budget": {"type": "integer", "description": "最终响应最多字节数，默认 8192"},
                    "time_budget_ms": {"type": "integer", "description": "文件与目录遍历总时间预算，默认 5000 毫秒"},
                    "max_file_size": {"type": "integer", "description": "只搜索不超过该字节数的文件，默认 8388608（8 MiB）；0 表示不限制。被跳过的文件会在尾注报告"},
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
                    "paths": {"type": "array", "items": {"type": "string"}, "maxItems": 50},
                    "detail": {
                        "type": "string",
                        "enum": ["meta", "hash", "full"],
                        "description": "meta 只查元信息不读内容；hash 读一次给指纹；full 额外统计行数，默认 full"
                    }
                },
                "required": []
            }
        }),
        json!({
            "name": "fs_exec",
            "description": "执行程序（argv 数组，不经过 shell）。替代 PowerShell 跑 cargo/git/npm/python：没有引号/插值、2>&1、ErrorRecord、$LASTEXITCODE 问题。非零退出不是错误；超时会结束整个进程树。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "program": {"type": "string", "description": "可执行文件，如 cargo、git、python；走 PATH。启动新任务时必填，只轮询/终止 job 时可省略"},
                    "args": {"type": "array", "items": {"type": "string"}, "description": "参数数组，每个参数独立，不拼字符串"},
                    "cwd": {"type": "string", "description": "工作目录，默认当前目录"},
                    "timeout_ms": {"type": "integer", "description": "超时毫秒，默认 120000，最大 600000；超时结束进程树"},
                    "stdin": {"type": "string", "description": "传给进程的标准输入"},
                    "env": {"type": "object", "description": "追加/覆盖环境变量，如 {\"RUST_BACKTRACE\":\"1\"}"},
                    "shell": {"type": "boolean", "description": "显式走 cmd /c（需要管道/重定向时才用，会回到文本解析）"},
                    "max_output": {"type": "integer", "description": "每条流最多保留字节，默认 32768"},
                    "background": {"type": "boolean", "description": "后台运行：立即返回 job id，用 job=<id> 轮询输出；适合 dev server/watch 这类长驻命令"},
                    "job": {"type": "integer", "description": "轮询/终止后台任务：传启动时返回的 job id"},
                    "kill": {"type": "boolean", "description": "配合 job 使用：结束该任务的整个进程树"}
                },
                "required": []
            }
        }),
        json!({
            "name": "fs_file",
            "description": "文件管理：copy/move/remove/mkdir/convert/archive/extract。替代 PowerShell 的 Copy-Item/Move-Item/Remove-Item/New-Item 与压缩解压。remove 必须 confirm=true。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "op": {"type": "string", "enum": ["copy", "move", "remove", "mkdir", "convert", "archive", "extract"], "description": "操作类型"},
                    "src": {"type": "string", "description": "copy/move 的源"},
                    "dst": {"type": "string", "description": "copy/move 的目标；可以是目录"},
                    "path": {"type": "string", "description": "remove/mkdir/convert 的目标"},
                    "recursive": {"type": "boolean", "description": "remove 删除目录时必须为 true"},
                    "confirm": {"type": "boolean", "description": "remove 必须显式传 true"},
                    "overwrite": {"type": "boolean", "description": "copy/move 覆盖已存在目标"},
                    "encoding": {"type": "string", "description": "convert 目标编码：utf-8/utf-8-bom/gbk/gb18030/utf-16le/utf-16be"},
                    "eol": {"type": "string", "enum": ["lf", "crlf"], "description": "convert 目标换行"},
                    "dry_run": {"type": "boolean", "description": "只报告不落盘"}
                },
                "required": ["op"]
            }
        }),
    ];
    // schema 与运行时校验一致拒绝未知顶层字段。
    for def in &mut defs {
        def["inputSchema"]["additionalProperties"] = json!(false);
        if let Some(props) = def["inputSchema"]["properties"].as_object_mut() {
            for (key, spec) in props {
                if spec["type"] == "integer" {
                    spec["minimum"] = json!(if matches!(key.as_str(), "start" | "end" | "limit") {
                        1
                    } else {
                        0
                    });
                }
            }
        }
    }
    defs
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
    // start 越过真实行数时不能进入反向扫描：lines 末尾没有对应索引。
    // 回退给调用方输出 E_RANGE，避免整个 MCP 进程 panic。
    if idx >= lines.len() {
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
            "old": {"type": "string", "description": "原文片段（需唯一，除非 all=true）；与 pattern 二选一"},
            "new": {"type": "string", "description": "替换内容，空串表示删除；与 replacement 二选一"},
            "pattern": {"type": "string", "description": "正则模式：替换范围内所有匹配，可用 $1 引用捕获组；不知道确切原文时用它，省掉为抄原文而做的一次 fs_read"},
            "replacement": {"type": "string", "description": "正则替换内容，与 pattern 配套"},
            "all": {"type": "boolean", "description": "替换全部出现位置"},
            "start": {"type": "integer", "minimum": 1, "description": "限定替换只发生在第 start..=end 行内（1 基）；配合 all=true 只改某个函数内的重复字面量"},
            "end": {"type": "integer", "minimum": 1, "description": "行范围结束行（含）；省略表示到文件末尾"}
        },
        "required": []
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

pub(crate) fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

pub(crate) fn arg_usize(args: &Value, key: &str, default: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(default)
}

pub(crate) fn arg_bool(args: &Value, key: &str, default: bool) -> bool {
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

/// 计算字段名编辑距离，给拼写错误提供最接近的合法选项。
fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b_chars.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut current = vec![i + 1];
        for (j, cb) in b_chars.iter().enumerate() {
            current.push(
                (previous[j + 1] + 1)
                    .min(current[j] + 1)
                    .min(previous[j] + usize::from(ca != *cb)),
            );
        }
        previous = current;
    }
    previous[b_chars.len()]
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
        let Some(spec) = props.get(key) else {
            let nearest = props
                .keys()
                .min_by_key(|candidate| edit_distance(key, candidate))
                .expect("工具定义必须有合法字段");
            return Some(format!(
                "E_BAD_ARGS {name}.{key} 是未知字段；最接近的合法字段是 {nearest}"
            ));
        };
        let wanted = spec["type"].as_str().unwrap_or("");
        if wanted == "integer" && value.is_number() && value.as_u64().is_none() {
            return Some(format!(
                "E_BAD_ARGS {name}.{key} 需要非负整数，收到 {value}"
            ));
        }
        let ok = match wanted {
            "string" => value.is_string(),
            "integer" => value.is_u64(),
            "number" => value.is_u64() || value.is_i64() || value.is_f64(),
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
        if wanted == "array" && spec["items"]["type"] == "string" {
            for (index, item) in value.as_array().into_iter().flatten().enumerate() {
                if !item.is_string() {
                    return Some(format!(
                        "E_BAD_ARGS {name}.{key}[{index}] 需要字符串，收到 {}",
                        type_name(item)
                    ));
                }
            }
        }
    }
    None
}

/// 多文件响应里错误可能出现在任意一行；识别 `E_XXX ` 形式的错误码行。
/// CLI 退出码与 MCP 的 isError 共用这一判定，避免一处认为成功、一处认为失败。
pub(crate) fn has_error_line(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("E_") else {
            return false;
        };
        // 错误码形如 E_READ、E_BINARY、E_TIMEOUT：全大写字母/下划线后接空格或结尾。
        let code_len = rest
            .find(|c: char| !(c.is_ascii_uppercase() || c == '_'))
            .unwrap_or(rest.len());
        code_len > 0 && rest[code_len..].starts_with([' ', ':'])
    })
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
        "fs_exec" => crate::exec::run(args),
        "fs_file" => crate::fileops::run(args),
        other => format!("E_UNKNOWN_TOOL 未知工具 {other}"),
    }
}

/// 单行按字符截断；与 fs_read 的 max_line 语义一致，避免中文行被按字节切坏。
fn shorten_line(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return text.to_string();
    }
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str(&format!("…(+{}字符)", total - max_chars));
    out
}

/// 渲染 fs_read summary=true 的结构摘要。
///
/// 输出沿用 fsops 的「行号|内容」契约，因此行号可以直接喂给 block=true。
/// 预算不足时明确说明已显示多少，绝不静默截断成看似完整的清单。
fn render_summary(
    header: &str,
    symbols: &[crate::summary::Symbol],
    total_lines: usize,
    budget: usize,
    max_line: usize,
) -> String {
    let mut out = String::with_capacity(budget.min(8 * 1024));
    out.push_str(header);
    if symbols.is_empty() {
        out.push_str("… 未识别到声明（可能是数据文件或不支持的语言）；请直接 fs_read 或 fs_grep\n");
        return out;
    }
    let full_footer = format!(
        "… {} 个符号（共 {} 行）| 用 fs_read block=true start=<行号> 读实现\n",
        symbols.len(),
        total_lines
    );
    // 尾注优先占位：宁少列几个符号，也不给缺参数的半截续读提示。
    let reserve = full_footer.len().min(160);
    let mut shown = 0usize;
    for symbol in symbols {
        let line = format!("{}|{}\n", symbol.line, shorten_line(&symbol.text, max_line));
        if out.len() + line.len() + reserve > budget {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if shown < symbols.len() {
        out.push_str(&format!(
            "… 已显示 {}/{} 个符号（共 {} 行）| 用 fs_grep 精确定位，或调大 budget\n",
            shown,
            symbols.len(),
            total_lines
        ));
    } else {
        out.push_str(&full_footer);
    }
    out
}

/// fs_read reads=[...]：把每项展开成一次独立读取，预算按项均分。
///
/// 递归复用单文件路径，编码/EOL/block/ranges 等语义全部自动继承；
/// 某一项失败只影响该项，其余照常输出（整体由 MCP 层标记 isError）。
fn read_items(args: &Value) -> String {
    let Some(items) = args.get("reads").and_then(Value::as_array) else {
        return "E_BAD_ARGS reads 必须是数组".into();
    };
    for key in [
        "path", "paths", "ranges", "start", "end", "block", "summary", "focus",
    ] {
        if args.get(key).is_some() {
            return format!(
                "E_BAD_ARGS reads 不能与 {key} 同时使用；每项的区间写在 reads 项内（path/start/end/ranges/block/max_line）"
            );
        }
    }
    if items.is_empty() {
        return "E_BAD_ARGS reads 不能为空".into();
    }
    if items.len() > 20 {
        return format!("E_BAD_ARGS reads 最多 20 项，当前 {} 项", items.len());
    }
    const ALLOWED: [&str; 7] = [
        "path", "start", "end", "ranges", "block", "max_line", "scan",
    ];
    let budget = arg_usize(args, "budget", render::DEFAULT_BUDGET);
    let force = arg_bool(args, "force", false);
    // 预算按项均分；连文件头都放不下时明确拒绝，
    // 避免 20 项各拿 256B 后总输出反超 budget。
    let per_item = budget / items.len();
    if per_item < 120 {
        return format!(
            "E_BUDGET {} 项只有 {budget} 字节预算（每项约 {per_item}），放不下文件头；请增大 budget 或分批读取",
            items.len()
        );
    }
    let mut out = String::new();
    for (index, item) in items.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            out.push_str(&format!(
                "E_BAD_ARGS reads[{index}] 必须是对象 {{path,start,end,...}}\n"
            ));
            continue;
        };
        if obj.get("path").and_then(Value::as_str).is_none() {
            out.push_str(&format!(
                "E_BAD_ARGS reads[{index}] 缺少字符串类型的 path\n"
            ));
            continue;
        }
        let unknown: Vec<&str> = obj
            .keys()
            .filter(|key| !ALLOWED.contains(&key.as_str()))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            out.push_str(&format!(
                "E_BAD_ARGS reads[{index}] 未知字段 {}；可用 path/start/end/ranges/block/max_line/scan\n",
                unknown.join(",")
            ));
            continue;
        }
        let mut single = obj.clone();
        single.insert("budget".to_string(), json!(per_item));
        if force {
            single.insert("force".to_string(), json!(true));
        }
        let text = read(&Value::Object(single));
        if index > 0 && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&text);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

/// focus 最多返回的代码块数；再多应该用 fs_grep 缩小范围。
const MAX_FOCUS_BLOCKS: usize = 12;

/// focus 的计算结果：保留的块、命中总数、是否被上限截断。
struct FocusBlocks {
    blocks: Vec<(usize, usize)>,
    total: usize,
    truncated: bool,
}

/// 把命中行扩成完整代码块并合并重叠/相邻区间。
fn focus_blocks(index: &render::LineIndex<'_>, pattern: &str) -> Result<FocusBlocks, String> {
    let regex = regex::Regex::new(pattern)
        .map_err(|e| format!("E_BAD_REGEX 正则非法: {e}；检查转义，或先用 fs_grep 确认形状"))?;
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for (offset, line) in index.lines().iter().enumerate() {
        if regex.is_match(line) {
            let line_no = offset + 1;
            blocks.push(enclosing_block(index.lines(), line_no).unwrap_or((line_no, line_no)));
        }
    }
    if blocks.is_empty() {
        return Ok(FocusBlocks {
            blocks: Vec::new(),
            total: 0,
            truncated: false,
        });
    }
    let merged = merge_ranges(blocks);
    let total = merged.len();
    let truncated = total > MAX_FOCUS_BLOCKS;
    let mut kept = merged;
    kept.truncate(MAX_FOCUS_BLOCKS);
    Ok(FocusBlocks {
        blocks: kept,
        total,
        truncated,
    })
}

fn read(args: &Value) -> String {
    if args.get("reads").is_some() {
        return read_items(args);
    }
    // focus 只对单文件生效：多文件时它无法确定含义。
    if args.get("focus").is_some()
        && (args.get("paths").is_some()
            || args.get("ranges").is_some()
            || args.get("start").is_some()
            || args.get("end").is_some()
            || arg_bool(args, "block", false)
            || arg_bool(args, "summary", false))
    {
        return "E_BAD_ARGS focus 只能用于单文件 path，且不能与 paths/ranges/start/end/block/summary 同时使用"
            .into();
    }
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
    // 超大文件扫描深度：默认 window；小文件不受影响。
    let scan_full = match arg_str(args, "scan") {
        None | Some("window") => false,
        Some("full") => true,
        Some(other) => {
            return render::truncate_utf8(
                &format!(
                    "E_BAD_ARGS 未知 scan={other}；可选 window（快速窗口）或 full（全文件扫描）"
                ),
                budget,
            )
            .to_string()
        }
    };
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
                if arg_bool(args, "summary", false) {
                    out.push_str(&format!(
                        "E_TOO_LARGE {} {}，超大文件不支持结构摘要；请先用 fs_grep 定位，或用 start/end 分段读取\n",
                        fp.display(),
                        io::human_size(m.size)
                    ));
                    continue;
                }
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
                let window = match io::read_large_window(&fp, large_start, end, scan_full) {
                    Ok(window) => window,
                    Err(e) => {
                        out.push_str(&format!("{e}\n"));
                        continue;
                    }
                };
                if window.lines == 0 {
                    if scan_full {
                        out.push_str(&format!(
                            "E_RANGE {} 区间超出文件范围（共{}行）\n",
                            fp.display(),
                            window.total_lines.unwrap_or(0)
                        ));
                    } else {
                        out.push_str(&format!(
                            "E_RANGE {} 区间 {}-{} 没有可读行（窗口扫描，未全扫）；请先用 fs_grep 定位行号，或用 scan=full 确认总行数\n",
                            fp.display(),
                            large_start,
                            end
                        ));
                    }
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
                if scan_full {
                    out.push_str(&format!(
                        "{} {} {} {}行 hash={} 全文件扫描\n",
                        window.encoding,
                        window.eol.as_str(),
                        io::human_size(window.size),
                        window.total_lines.unwrap_or(0),
                        window.hash.as_deref().unwrap_or("")
                    ));
                } else {
                    // 窗口头必须显式声明“未全扫”，否则模型会把局部行数当成全文件信息。
                    out.push_str(&format!(
                        "{} {} {} 窗口扫描（未全扫，仅本次请求区间）；全文 hash/总行数用 scan=full 或 fs_stat detail=hash\n",
                        window.encoding,
                        window.eol.as_str(),
                        io::human_size(window.size)
                    ));
                }
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
                // 窗口正文同样只建一次行索引，供 converge 多轮重渲染复用。
                let window_index = render::LineIndex::new(&window.text);
                let inner_budget = want.saturating_sub(8).max(1);
                let target = render::converge_budget(inner_budget, |bb| {
                    let r = window_index.render_ranges(
                        &[(1, window.lines)],
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
                            line_no, need, bb, need + 200 + header_len
                        );
                    }
                    let mut text = r.body;
                    // 续读参数优先拼（预算紧张时最先保住它）。
                    if rendered_last > 0 && rendered_last < rendered_total {
                        let absolute_last = window.first_line + rendered_last.saturating_sub(1);
                        if scan_full {
                            text.push_str(&format!(
                                "… 至 {}/{} 行 | 续读: start={} end={}\n",
                                absolute_last,
                                window.total_lines.unwrap_or(0),
                                absolute_last + 1,
                                end
                            ));
                        } else {
                            // 窗口模式不知道总行数，不能写伪造的 `/N 行`。
                            text.push_str(&format!(
                                "… 至 {} 行（窗口扫描，未全扫）| 续读: start={} end={}\n",
                                absolute_last,
                                absolute_last + 1,
                                end
                            ));
                        }
                    } else if !scan_full && window.more && rendered_last > 0 {
                        // 请求区间已全部输出但文件还有内容：给下一窗口（同宽，start≤end 可直接重放）。
                        let absolute_last = window.first_line + rendered_last.saturating_sub(1);
                        let span = end.saturating_sub(large_start).saturating_add(1);
                        let next_end = end.saturating_add(span);
                        text.push_str(&format!(
                            "… 区间 {}-{} 已全部输出，文件还有内容（窗口扫描，未全扫）| 续读: start={} end={}\n",
                            window.first_line,
                            absolute_last,
                            absolute_last + 1,
                            next_end
                        ));
                    }
                    if line_cut {
                        let need = r.max_cut_line_bytes;
                        text.push_str(&format!(
                            "… 有长行被截断；读全文用 max_line=0，并需 budget≈{}\n",
                            need + 200 + header_len
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
        // 自适应预算：小文件一次读完，省掉一次「12KB 截断 → 续读」的往返。
        // 多送几 KB 换掉一整轮上下文重发，划算；显式 budget 永远优先。
        if !budget_explicit
            && targets.len() == 1
            && budget == render::DEFAULT_BUDGET
            && doc.size <= 16 * 1024
        {
            budget = doc.size as usize + 4096;
        }
        // 行索引只建一次：总行数、block 探测和 converge 多轮渲染共用，
        // 避免每次缩预算都把整个文件重新 split 一遍。
        let index = render::LineIndex::new(&doc.text);
        let total_lines = index.total_lines();
        // focus：命中行扩成完整代码块；与 block 类似，但范围由正则决定。
        let focus_info = match arg_str(args, "focus") {
            Some(pattern) => match focus_blocks(&index, pattern) {
                Ok(info) => {
                    if info.blocks.is_empty() {
                        out.push_str(&format!(
                            "E_NO_MATCH {} 没有匹配 {} 的代码块；用 fs_grep 确认模式，或去掉 focus 直接读文件\n",
                            fp.display(),
                            pattern
                        ));
                        continue;
                    }
                    Some((info, pattern.to_string()))
                }
                Err(e) => {
                    out.push_str(&format!("{e}\n"));
                    continue;
                }
            },
            None => None,
        };
        // summary=true：只给结构、不给正文；行号可直接接 block=true 读实现。
        if arg_bool(args, "summary", false) {
            if args.get("ranges").is_some()
                || args.get("end").is_some()
                || arg_bool(args, "block", false)
            {
                out.push_str(&format!(
                    "E_BAD_ARGS {} summary=true 不能与 ranges/block/end 同时使用；摘要本身已给出全部符号的行号\n",
                    fp.display()
                ));
                continue;
            }
            let language = crate::summary::language_of(&fp.display());
            if crate::summary::is_data(language) {
                out.push_str(&format!(
                    "E_BAD_ARGS {} 是数据文件，没有声明可摘要；请直接 fs_read 或用 fs_grep 定位\n",
                    fp.display()
                ));
                continue;
            }
            let symbols = crate::summary::extract(&doc.text, language);
            let summary_budget = if targets.len() > 1 {
                (budget / targets.len().max(1)).saturating_sub(usize::from(i > 0))
            } else {
                budget
            };
            let header = format!(
                "{} {} {} {} {}行 hash={} 结构摘要（仅声明行，未展开函数体）\n",
                fp.display(),
                doc.encoding,
                doc.eol.as_str(),
                io::human_size(doc.size),
                total_lines,
                snapshot.hash
            );
            // never-worse：极短的声明型文件里摘要反而更长时，退回全文更诚实。
            let body_bytes: usize = symbols.iter().map(|s| s.text.len() + 12).sum();
            if body_bytes < doc.text.len() {
                out.push_str(&render_summary(
                    &header,
                    &symbols,
                    total_lines,
                    summary_budget,
                    max_line,
                ));
                continue;
            }
            out.push_str("… summary 不比全文更省（文件较短），以下按全文返回\n");
        }
        // block=true：返回包含 start 行的整个代码块。
        // grep 给出的行号通常在函数内部，直接按 start 展开只能拿到那一行；
        // 这里先向外找到块起始行，再按括号配平确定结束行。
        let per_file_ranges = if let Some((focus, _)) = &focus_info {
            focus.blocks.clone()
        } else if arg_bool(args, "block", false) && args.get("ranges").is_none() {
            match enclosing_block(index.lines(), start) {
                Some((from, to)) => vec![(from, to)],
                None => ranges.clone(),
            }
        } else {
            ranges.clone()
        };
        // 无 end 区间（含 block 未找到代码块的回退）在续读时必须写真实总行数；
        // 把 usize::MAX 原样塞进 ranges 会让模型拿到无法理解、也无法重放的值。
        let per_file_ranges: Vec<(usize, usize)> = per_file_ranges
            .into_iter()
            .map(|(from, to)| {
                let to = if total_lines == 0 {
                    to
                } else {
                    to.min(total_lines)
                };
                (from, to)
            })
            .collect();
        // 未变更短路：同一进程内、同 hash、同 max_line、区间已完整读过时只回摘要。
        // 上下文被压缩后模型可能已经不记得内容，所以摘要里带 hash/区间/总行数/首行锚点。
        let cache_key = fp.display().to_string();
        if !arg_bool(args, "force", false) && args.get("focus").is_none() {
            if let Some(covered) =
                crate::cache::lookup(&cache_key, &snapshot.hash, max_line, &per_file_ranges)
            {
                out.push_str(&crate::cache::stub(
                    &cache_key,
                    &snapshot.hash,
                    &covered,
                    total_lines,
                    index.lines().first().copied().unwrap_or(""),
                ));
                continue;
            }
        }
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
        if let Some((focus, pattern)) = &focus_info {
            if focus.truncated {
                header.push_str(&format!(
                    "… focus={pattern} 命中 {} 个代码块，仅显示前 {MAX_FOCUS_BLOCKS} 块；用 fs_grep 缩小范围\n",
                    focus.total
                ));
            } else {
                header.push_str(&format!(
                    "… focus={pattern} 命中 {} 个代码块\n",
                    focus.total
                ));
            }
        }

        let want = if targets.len() > 1 {
            (budget / targets.len().max(1)).saturating_sub(usize::from(i > 0))
        } else {
            budget
        };
        let mut fully_rendered = false;
        let target = render::converge_budget(want, |body_budget| {
            let mut target = header.clone();
            let r = index.render_ranges(
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
            // 只有整段请求都完整返回且没有长行截断，才敢记进缓存。
            fully_rendered = r.continuation.is_empty() && !r.line_cut;
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
                let need = r.max_cut_line_bytes;
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
                if args.get("ranges").is_some() || arg_bool(args, "block", false) {
                    // block 已在首次请求中固定边界；续读只重放剩余区间，不再探测新块。
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
        if fully_rendered && args.get("focus").is_none() {
            crate::cache::record(&cache_key, &snapshot.hash, max_line, &per_file_ranges);
        }
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

    /// 34MB / 34000 行（每行 1023 字节 + LF），走超大文件流式分支。
    fn 大文件(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::io::Write;
        let dir = 临时目录(tag);
        let path = dir.join("large.txt");
        let line = format!("{}\n", "x".repeat(1023));
        let mut file = fs::File::create(&path).unwrap();
        for _ in 0..34_000 {
            file.write_all(line.as_bytes()).unwrap();
        }
        file.sync_all().unwrap();
        (dir, path)
    }

    /// 从 `续读:` 尾注里取出 start/end，验证可直接重放。
    fn 解析续读(out: &str) -> (usize, usize) {
        let tail = out
            .split("续读:")
            .nth(1)
            .unwrap_or_else(|| panic!("缺少续读尾注: {out}"));
        let grab = |key: &str| {
            tail.split(key)
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|value| value.parse::<usize>().ok())
        };
        (
            grab("start=").unwrap_or_else(|| panic!("续读缺少 start: {out}")),
            grab("end=").unwrap_or_else(|| panic!("续读缺少 end: {out}")),
        )
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
    fn 未知顶层字段拒绝并给出最接近合法字段() {
        let output = call("fs_stat", &json!({"path": ".", "detal": "meta"}));
        assert!(output.starts_with("E_BAD_ARGS"), "{output}");
        assert!(
            output.contains("detal") && output.contains("detail"),
            "{output}"
        );
        for def in tool_defs() {
            assert_eq!(
                def["inputSchema"]["additionalProperties"],
                json!(false),
                "{} schema 未禁止未知字段",
                def["name"]
            );
        }
    }

    #[test]
    fn integer只接受非负整数() {
        for value in [json!(-1), json!(1.5)] {
            let output = call("fs_read", &json!({"path": ".", "start": value}));
            assert!(output.starts_with("E_BAD_ARGS"), "start={value}: {output}");
            assert!(output.contains("非负整数"), "{output}");
        }
        let dir = 临时目录("integer-positive");
        fs::write(dir.join("a.txt"), "x").unwrap();
        let accepted = call(
            "fs_glob",
            &json!({"path": dir.to_string_lossy(), "limit": 1, "offset": 0}),
        );
        let _ = fs::remove_dir_all(dir);
        assert_eq!(accepted, "a.txt", "合法非负整数被拒绝: {accepted}");
        for def in tool_defs() {
            let props = def["inputSchema"]["properties"].as_object().unwrap();
            for (key, spec) in props {
                if spec["type"] == "integer" {
                    assert!(
                        spec["minimum"].as_u64().is_some(),
                        "{}.{key} 缺少 minimum",
                        def["name"]
                    );
                }
            }
        }
        let defs = tool_defs();
        let grep = defs.iter().find(|def| def["name"] == "fs_grep").unwrap();
        assert_eq!(
            grep["inputSchema"]["properties"]["limit"]["minimum"],
            json!(1)
        );
    }

    #[test]
    fn paths非字符串元素显式报错() {
        for tool in ["fs_read", "fs_stat"] {
            let output = call(tool, &json!({"paths": [".", 42]}));
            assert!(output.starts_with("E_BAD_ARGS"), "{tool}: {output}");
            assert!(output.contains("paths[1]"), "{tool}: {output}");
        }
    }

    #[test]
    fn stat超过50个路径时显式拒绝() {
        let output = call("fs_stat", &json!({"paths": vec!["."; 51]}));
        assert!(output.starts_with("E_BAD_ARGS"), "{output}");
        assert!(output.contains("51") && output.contains("50"), "{output}");
        let defs = tool_defs();
        let stat = defs.iter().find(|def| def["name"] == "fs_stat").unwrap();
        assert_eq!(
            stat["inputSchema"]["properties"]["paths"]["maxItems"],
            json!(50)
        );
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
        assert!(out.starts_with("E_BAD_"), "应报参数错误: {out}");
        assert!(out.contains("缺少 new") || out.contains("old+new"), "{out}");
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
    fn 正则词边界命中空位置时拒绝写入() {
        // `\b` 不匹配空串，却在每个词边界命中：旧守卫只看 is_match("")，
        // 会把 4 个边界全替换掉。必须报 E_BAD_REGEX 且文件不变。
        let dir = 临时目录("edit-word-boundary");
        let f = dir.join("a.txt");
        fs::write(&f, "a b\nc d\n").unwrap();
        let fp = FsPath::parse(&f.to_string_lossy()).unwrap();

        let out = edit_tool(&json!({
            "path": fp.display(),
            "edits": [{"pattern": "\\b", "replacement": "X"}]
        }));
        let after = fs::read_to_string(&f).unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert!(out.starts_with("E_BAD_REGEX"), "{out}");
        assert!(out.contains("空位置"), "{out}");
        assert_eq!(after, "a b\nc d\n", "拒绝后文件必须一字不改");
    }

    #[test]
    fn 替换文本引用未知捕获组时拒绝写入() {
        // `(a)` + `$1_x` 会被 regex 当成组名 `1_x`，静默展开成空串。
        let dir = 临时目录("edit-bad-replacement");
        let f = dir.join("a.txt");
        fs::write(&f, "a\n").unwrap();
        let fp = FsPath::parse(&f.to_string_lossy()).unwrap();

        let out = edit_tool(&json!({
            "path": fp.display(),
            "edits": [{"pattern": "(a)", "replacement": "$1_x"}]
        }));
        let after = fs::read_to_string(&f).unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert!(out.starts_with("E_BAD_REPLACEMENT"), "{out}");
        assert!(out.contains("${1}_x"), "应给出正确写法: {out}");
        assert_eq!(after, "a\n", "拒绝后文件必须一字不改");
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
    fn 长行预算提示覆盖响应中最长截断行() {
        let first = "x".repeat(300);
        let longest = "y".repeat(2000);
        let path = 临时文本文件(&format!("{first}\n{longest}\n"));
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let output = read(&json!({
            "path": posix_path, "max_line": 80, "budget": 5000
        }));
        let suggested = output
            .split("budget≈")
            .nth(1)
            .and_then(|tail| tail.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("缺少预算建议: {output}"));
        let replay = read(&json!({
            "path": posix_path, "start": 2, "end": 2,
            "max_line": 0, "budget": suggested
        }));
        let _ = fs::remove_file(path);
        assert!(
            !replay.starts_with("E_LINE_TOO_LONG"),
            "建议预算不可执行: {replay}"
        );
        assert!(replay.contains(&longest), "重放未读到最长行");
    }

    #[test]
    fn 超大文件长行提示也覆盖最长截断行() {
        use std::io::Write;
        let dir = 临时目录("read-large-longest-line");
        let path = dir.join("large.txt");
        let longest = "y".repeat(2000);
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "{}", "x".repeat(300)).unwrap();
        writeln!(file, "{longest}").unwrap();
        let padding = format!("{}\n", "z".repeat(1023));
        for _ in 0..34_000 {
            file.write_all(padding.as_bytes()).unwrap();
        }
        file.sync_all().unwrap();
        drop(file);
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let output = read(&json!({
            "path": posix_path, "start": 1, "end": 2,
            "max_line": 80, "budget": 5000
        }));
        let suggested = output
            .split("budget≈")
            .nth(1)
            .and_then(|tail| tail.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("缺少预算建议: {output}"));
        let replay = read(&json!({
            "path": posix_path, "start": 2, "end": 2,
            "max_line": 0, "budget": suggested
        }));
        fs::remove_dir_all(dir).unwrap();
        assert!(
            !replay.starts_with("E_LINE_TOO_LONG"),
            "建议预算不可执行: {replay}"
        );
        assert!(replay.contains(&longest), "重放未读到最长行");
    }

    #[test]
    fn 每个声明参数都有对应实现分支() {
        // 防止再出现「schema 声明了 dry_run 但代码没实现」或「实现了但模型看不到」。
        // 这里只校验最容易漏的布尔/可选开关，必须同时出现在 schema 与参数读取里。
        let src = concat!(
            include_str!("tools.rs"),
            include_str!("exec.rs"),
            include_str!("fileops.rs"),
            include_str!("jobs.rs")
        );
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
                    format!("path_arg(args, \"{key}\""),
                    format!("args.get(\"{key}\")"),
                    format!(".get(\"{key}\")"),
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
        // 读取缓存会短路同区间重复读；本测试校验 block 逻辑本身，统一 force 绕过。
        let out = read(&json!({
            "path": posix_path,
            "start": 1,
            "block": true,
            "budget": 100000,
            "force": true
        }));
        // grep 典型输出：命中行在函数内部（第 2 行是 let，没有 {），
        // 应向外扩展到整个 outer，而不是只给这一行。
        let inside = read(&json!({
            "path": posix_path,
            "start": 2,
            "block": true,
            "budget": 100000,
            "force": true
        }));
        // 命中行在嵌套块内部（第 4 行在 if 里）→ 扩展为最近的 if 块
        let nested = read(&json!({
            "path": posix_path,
            "start": 4,
            "block": true,
            "budget": 100000,
            "force": true
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
    fn block预算续读固定原块边界() {
        let content = "fn outer() {\n    let x = 1;\n    if x > 0 {\n        println!(\"a\");\n        println!(\"b\");\n        println!(\"c\");\n        println!(\"d\");\n        println!(\"e\");\n    }\n    let y = 2;\n}\n";
        let path = std::env::current_dir().unwrap().join(format!(
            "fsops-block-continuation-{}-{}.rs",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, content).unwrap();
        let posix_path = path.file_name().unwrap().to_string_lossy().to_string();
        let first = read(&json!({
            "path": posix_path,
            "start": 4,
            "block": true,
            "budget": 220
        }));

        let marker = "ranges=";
        let tail = first
            .split(marker)
            .nth(1)
            .unwrap_or_else(|| panic!("缺少固定 ranges 续读: {first}"));
        let ranges: Value = serde_json::from_str(tail.lines().next().unwrap()).unwrap();
        let second = read(&json!({
            "path": posix_path,
            "ranges": ranges,
            "budget": 10000
        }));
        let mut lines = Vec::new();
        for output in [&first, &second] {
            lines.extend(output.lines().filter_map(|line| {
                let (number, _) = line.split_once('|')?;
                number.parse::<usize>().ok()
            }));
        }
        let _ = fs::remove_file(path);
        assert_eq!(
            lines,
            (3..=9).collect::<Vec<_>>(),
            "跨页重复或遗漏: first={first} second={second}"
        );
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
    fn glob完整条目可重放分页且真实lookahead() {
        let dir = 临时目录("glob-pages");
        let expected: Vec<String> = (0..8).map(|i| format!("f{i:02}.txt")).collect();
        for name in &expected {
            fs::write(dir.join(name), "x").unwrap();
        }
        let path = dir.to_string_lossy().replace('\\', "/");
        let mut seen = Vec::new();
        let mut offset = 0usize;
        for _ in 0..10 {
            let output = glob_tool(&json!({
                "path": path, "pattern": "*.txt", "budget": 100,
                "limit": 100, "offset": offset
            }));
            for line in output.lines() {
                if !line.starts_with('…') {
                    assert!(
                        expected.contains(&line.to_string()),
                        "半路径或非法条目: {output}"
                    );
                    seen.push(line.to_string());
                }
            }
            let Some(next) = output
                .split("next_offset=")
                .nth(1)
                .and_then(|tail| tail.split_whitespace().next())
                .and_then(|n| n.parse::<usize>().ok())
            else {
                break;
            };
            assert!(next > offset, "空页或偏移未推进: {output}");
            offset = next;
        }
        assert_eq!(seen, expected, "页间重复或遗漏");

        let exact = glob_tool(&json!({
            "path": path, "pattern": "*.txt", "limit": 8, "budget": 1000
        }));
        assert_eq!(
            exact,
            expected.join("\n"),
            "恰等于 limit 时虚报续页: {exact}"
        );
        fs::remove_dir_all(dir).unwrap();
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
    fn 一次读取只构建一次行索引() {
        // converge 会按超出的字节多轮重渲染；行索引必须只建一次，
        // 否则每次缩预算都要重新 split 整个文件。
        let content = (1..=300)
            .map(|i| format!("line {i:03} {}", "x".repeat(40)))
            .collect::<Vec<_>>()
            .join("\n");
        let path = 临时文本文件(&content);
        render::test_reset_index_builds();
        render::test_reset_converge_rounds();
        let out = read(&json!({
            "path": path.to_string_lossy(),
            "budget": 600
        }));
        let builds = render::test_index_builds();
        let rounds = render::test_converge_rounds();
        fs::remove_file(path).unwrap();

        assert!(out.contains("续读"), "{out}");
        assert!(rounds >= 2, "测试前提：预算应收敛多轮，实际 {rounds}");
        assert_eq!(builds, 1, "一次 fs_read 只能建一次行索引，实际 {builds}");
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
            render::test_reset_index_builds();
            render::test_reset_converge_rounds();
            let out = read(&json!({
                "path": path.to_string_lossy(),
                "start": 100,
                "end": 2000,
                "max_line": 50,
                "budget": budget
            }));
            let builds = render::test_index_builds();
            let rounds = render::test_converge_rounds();
            if builds != 1 {
                problems.push(format!("budget={budget} 建了 {builds} 次行索引"));
            }
            if rounds < 2 {
                problems.push(format!(
                    "budget={budget} 只收敛 {rounds} 轮，测试前提不成立"
                ));
            }
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
    fn fs_exec_schema允许只传job轮询() {
        // 轮询/终止后台任务只需要 job；若 schema 强制 program，
        // 宿主会在发出调用前就拒掉 {"job":1}。
        let defs = tool_defs();
        let def = defs.iter().find(|d| d["name"] == "fs_exec").unwrap();
        let required = def["inputSchema"]["required"].as_array().unwrap();
        assert!(required.is_empty(), "fs_exec.required 应为空: {required:?}");
    }

    #[test]
    fn fs_read_schema暴露scan开关() {
        let defs = tool_defs();
        let read = defs
            .iter()
            .find(|def| def["name"] == "fs_read")
            .expect("缺少 fs_read");
        assert_eq!(
            read["inputSchema"]["properties"]["scan"]["enum"],
            json!(["window", "full"])
        );
    }

    #[test]
    fn fs_read_scan非法值报错小文件行为不变() {
        let path = 临时文本文件("a\nb\n");
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let bad = read(&json!({"path": posix_path, "scan": "fast"}));
        let window = read(&json!({"path": posix_path, "scan": "window"}));
        let _ = fs::remove_file(path);

        assert!(bad.starts_with("E_BAD_ARGS"), "{bad}");
        assert!(bad.contains("scan"), "{bad}");
        assert!(!window.starts_with("E_BAD_ARGS"), "{window}");
        assert!(window.contains("1|a"), "小文件行为不能变: {window}");
        assert!(!window.contains("未全扫"), "小文件不该出现窗口头: {window}");
        assert!(window.contains("hash="), "小文件仍给 hash: {window}");
    }

    #[test]
    fn 超大文件默认窗口扫描不给全量指纹() {
        let (dir, path) = 大文件("read-large-window");
        let out = read(&json!({
            "path": path.to_string_lossy(),
            "start": 100,
            "end": 110,
            "budget": 6000
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(out.contains("未全扫"), "窗口头必须声明未全扫: {out}");
        assert!(!out.contains("hash="), "窗口模式不能给全量 hash: {out}");
        assert!(out.contains("scan=full"), "必须提示升级全扫: {out}");
        assert!(
            out.contains("fs_stat detail=hash"),
            "必须给 hash 替代: {out}"
        );
        assert!(out.contains("100|"), "{out}");
        assert!(out.contains("110|"), "{out}");
    }

    #[test]
    fn 超大文件scan_full保留全扫契约() {
        let (dir, path) = 大文件("read-large-full");
        let out = read(&json!({
            "path": path.to_string_lossy(),
            "start": 100,
            "end": 110,
            "budget": 6000,
            "scan": "full"
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(out.contains("34000行"), "full 必须给精确总行数: {out}");
        assert!(out.contains("hash="), "full 必须给全文 hash: {out}");
        assert!(!out.contains("未全扫"), "{out}");
    }

    #[test]
    fn 超大文件窗口续读尾注可重放() {
        let (dir, path) = 大文件("read-large-continue");
        let out = read(&json!({
            "path": path.to_string_lossy(),
            "start": 100,
            "end": 110,
            "budget": 6000
        }));
        let (next_start, next_end) = 解析续读(&out);
        let replay = read(&json!({
            "path": path.to_string_lossy(),
            "start": next_start,
            "end": next_end,
            "budget": 6000
        }));
        fs::remove_dir_all(dir).unwrap();

        assert_eq!(next_start, 111, "区间全部输出后应从下一行续读: {out}");
        assert!(next_end >= next_start, "续读参数不可重放: {out}");
        assert!(!replay.starts_with("E_RANGE"), "{replay}");
        assert!(replay.contains(&format!("{next_start}|")), "{replay}");
        assert!(replay.contains(&format!("{next_end}|")), "{replay}");
    }

    #[test]
    fn 超大文件窗口预算截断尾注可重放() {
        let (dir, path) = 大文件("read-large-budget");
        let out = read(&json!({
            "path": path.to_string_lossy(),
            "start": 100,
            "end": 2000,
            "max_line": 50,
            "budget": 1000
        }));
        let (next_start, next_end) = 解析续读(&out);
        let replay = read(&json!({
            "path": path.to_string_lossy(),
            "start": next_start,
            "end": next_end,
            "max_line": 50,
            "budget": 1000
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(out.contains("未全扫"), "{out}");
        assert_eq!(next_end, 2000, "预算截断必须固定原 end: {out}");
        assert!(!replay.starts_with("E_RANGE"), "{replay}");
        assert!(replay.contains(&format!("{next_start}|")), "{replay}");
    }

    #[test]
    fn 超大文件窗口区间越界说明未全扫并可升级全扫() {
        let (dir, path) = 大文件("read-large-range");
        let window = read(&json!({
            "path": path.to_string_lossy(),
            "start": 40000,
            "end": 40010
        }));
        let full = read(&json!({
            "path": path.to_string_lossy(),
            "start": 40000,
            "end": 40010,
            "scan": "full"
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(window.starts_with("E_RANGE"), "{window}");
        assert!(
            !window.contains("共0行"),
            "窗口模式不能伪报总行数: {window}"
        );
        assert!(window.contains("未全扫"), "{window}");
        assert!(window.contains("scan=full"), "{window}");
        assert!(full.starts_with("E_RANGE"), "{full}");
        assert!(full.contains("共34000行"), "{full}");
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
    fn glob达到limit后不无限探测且明确未探测完() {
        // 1 个匹配 + 大量后续项：达到 limit 后只能探测有限个后续原始目录项。
        // 探测上限耗尽仍未见下一匹配时，既不能给 next_offset，也不能沉默地当成末页。
        let dir = 临时目录("glob-lookahead");
        fs::write(dir.join("a_match.txt"), "x").unwrap();
        for i in 0..1100 {
            fs::write(dir.join(format!("zzz{i:04}.txt")), "x").unwrap();
        }
        let path = dir.to_string_lossy().replace('\\', "/");
        let output = glob_tool(&json!({
            "path": path, "pattern": "**/a_match.txt", "limit": 1, "budget": 1000
        }));
        fs::remove_dir_all(dir).unwrap();

        assert_eq!(output.lines().next().unwrap(), "a_match.txt", "{output}");
        assert!(
            !output.contains("next_offset"),
            "未探测到下一匹配，不得声明续页: {output}"
        );
        assert!(
            output.contains("未探测完") && output.contains("增大 limit"),
            "必须说明 lookahead 达到上限: {output}"
        );
    }

    #[test]
    fn glob_offset空页说明跳过多少匹配条目() {
        // offset 超出总数时是“本页为空”，不是“没有匹配”。
        let dir = 临时目录("glob-offset-empty");
        fs::write(dir.join("a.txt"), "x").unwrap();
        fs::write(dir.join("b.txt"), "x").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");
        let over = glob_tool(&json!({
            "path": path, "pattern": "*.txt", "offset": 9
        }));

        // 单文件根 offset>0 同样必须说明跳过了哪个匹配条目。
        let file = dir.join("a.txt").to_string_lossy().replace('\\', "/");
        let single = glob_tool(&json!({
            "path": file, "offset": 1
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(
            over.contains("本页无结果") && over.contains("offset=9") && over.contains('2'),
            "目录根空页缺少跳过说明: {over}"
        );
        assert!(!over.contains("无匹配"), "空页不得报无匹配: {over}");
        assert!(
            single.contains("本页无结果") && single.contains("offset=1") && single.contains('1'),
            "单文件根空页缺少跳过说明: {single}"
        );
        assert!(!single.contains("无匹配"), "空页不得报无匹配: {single}");
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
    fn stat普通文本只读取一次() {
        let dir = 临时目录("stat-single-read");
        let file = dir.join("a.txt");
        fs::write(&file, "alpha\nbeta\n").unwrap();
        for detail in ["hash", "full"] {
            io::reset_test_read_count();
            let output = stat(&json!({
                "path": file.to_string_lossy(), "detail": detail
            }));
            assert!(output.contains("hash="), "{output}");
            assert_eq!(io::test_read_count(), 1, "detail={detail} 发生了重复读取");
        }
        fs::remove_dir_all(dir).unwrap();
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
        // pattern 模式下不需要 old，所以两个字段都不强制。
        assert_eq!(nested["required"], json!([]));
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
        assert_eq!(defs.len(), 8);
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
            "max_file_size",
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
    fn grep_files高密度首文件不挤掉其他文件() {
        let dir = 临时目录("grep-files-dense");
        fs::write(dir.join("a.txt"), "needle\n".repeat(100)).unwrap();
        fs::write(dir.join("b.txt"), "needle\n").unwrap();
        fs::write(dir.join("c.txt"), "needle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        for parallel in [false, true] {
            let output = grep_tool(&json!({
                "pattern": "needle",
                "path": path,
                "output": "files",
                "parallel": parallel
            }));
            assert_eq!(
                output, "a.txt\nb.txt\nc.txt",
                "parallel={parallel}: {output}"
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn grep_files按文件分页且末页不虚报续页() {
        let dir = 临时目录("grep-files-pages");
        fs::write(dir.join("a.txt"), "needle\n".repeat(100)).unwrap();
        fs::write(dir.join("b.txt"), "needle\n").unwrap();
        fs::write(dir.join("c.txt"), "needle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        for parallel in [false, true] {
            let first = grep_tool(&json!({
                "pattern": "needle", "path": path, "output": "files",
                "limit": 1, "parallel": parallel
            }));
            let second = grep_tool(&json!({
                "pattern": "needle", "path": path, "output": "files",
                "limit": 1, "offset": 1, "parallel": parallel
            }));
            let last = grep_tool(&json!({
                "pattern": "needle", "path": path, "output": "files",
                "limit": 3, "parallel": parallel
            }));
            assert_eq!(
                first, "a.txt\n… next_offset=1",
                "parallel={parallel}: {first}"
            );
            assert_eq!(
                second, "b.txt\n… next_offset=2",
                "parallel={parallel}: {second}"
            );
            assert_eq!(last, "a.txt\nb.txt\nc.txt", "parallel={parallel}: {last}");
        }
        fs::remove_dir_all(dir).unwrap();
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
    fn files模式max_per_file为零时无可见命中() {
        // max_per_file 是可见性命中上限，必须在 files/count 分支之前生效。
        // max_per_file=0 时 files 不应仍列出文件（串行/并行一致）。
        let dir = 临时目录("grep-files-max0");
        fs::write(dir.join("a.txt"), "needle\nneedle\nneedle\n").unwrap();
        fs::write(dir.join("b.txt"), "needle\nneedle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        for parallel in [false, true] {
            let output = grep_tool(&json!({
                "pattern": "needle", "path": path, "output": "files",
                "max_per_file": 0, "parallel": parallel
            }));
            assert!(
                output.contains("无可见命中"),
                "parallel={parallel}: max_per_file=0 应明确说明无可见命中: {output}"
            );
            assert!(
                !output.contains("a.txt"),
                "parallel={parallel}: max_per_file=0 仍列出了文件: {output}"
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn files模式max_per_file为一仍按文件输出() {
        let dir = 临时目录("grep-files-max1");
        fs::write(dir.join("a.txt"), "needle\nneedle\nneedle\n").unwrap();
        fs::write(dir.join("b.txt"), "needle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        for parallel in [false, true] {
            let output = grep_tool(&json!({
                "pattern": "needle", "path": path, "output": "files",
                "max_per_file": 1, "parallel": parallel
            }));
            assert_eq!(
                output, "a.txt\nb.txt",
                "parallel={parallel}: files 仍应每文件一行"
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn count模式max_per_file省略超出部分并说明() {
        // count 也必须遵守 max_per_file，且不能静默忽略。
        let dir = 临时目录("grep-count-max");
        fs::write(dir.join("a.txt"), "needle\nneedle\nneedle\nneedle\n").unwrap();
        fs::write(dir.join("b.txt"), "needle\nneedle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        for parallel in [false, true] {
            let output = grep_tool(&json!({
                "pattern": "needle", "path": path, "output": "count",
                "max_per_file": 1, "parallel": parallel
            }));
            assert!(
                output.contains("a.txt:1") && output.contains("b.txt:1"),
                "parallel={parallel}: count 应只给每文件 1 条: {output}"
            );
            assert!(
                output.contains("省略") && output.contains("max_per_file=1"),
                "parallel={parallel}: 省略必须给出尾注: {output}"
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn count分页时每文件数字标为本页可见() {
        // offset>0 时页内数字只是“本页可见”，不能读成文件总数。
        let dir = 临时目录("grep-count-page");
        fs::write(dir.join("a.txt"), "needle\nneedle\nneedle\n").unwrap();
        fs::write(dir.join("b.txt"), "needle\nneedle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        for parallel in [false, true] {
            let output = grep_tool(&json!({
                "pattern": "needle", "path": path, "output": "count",
                "offset": 1, "limit": 10, "parallel": parallel
            }));
            assert!(
                output.contains("a.txt:2") && output.contains("b.txt:2"),
                "parallel={parallel}: 页内计数不符: {output}"
            );
            assert!(
                output.contains("本页可见"),
                "parallel={parallel}: offset 分页的数字必须标注: {output}"
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn grep目录遍历超时返回错误码() {
        // 遍历没跑完且没有命中时，不能返回“ok 无命中”。
        let dir = 临时目录("grep-timeout-e");
        fs::write(dir.join("a.txt"), "needle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");
        let output = grep_tool(&json!({
            "pattern": "needle", "path": path, "time_budget_ms": 0
        }));
        fs::remove_dir_all(dir).unwrap();
        assert!(output.starts_with("E_TIMEOUT"), "{output}");
        assert!(output.contains("时间预算"), "{output}");
    }

    #[test]
    fn files模式offset空页用匹配文件单位() {
        let dir = 临时目录("grep-files-offset");
        fs::write(dir.join("a.txt"), "needle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        let output = grep_tool(&json!({
            "pattern": "needle", "path": path, "output": "files", "offset": 5
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(output.contains("本页无结果"), "{output}");
        assert!(
            output.contains("匹配文件"),
            "files 模式的单位是文件，不应写“条命中”: {output}"
        );
        assert!(!output.contains("条命中"), "{output}");
    }

    #[test]
    fn count_offset空页用条命中单位() {
        // count 的 offset 空页写“条命中”，不能生成“个条命中”。
        let dir = 临时目录("grep-count-offset-unit");
        fs::write(dir.join("a.txt"), "needle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        let output = grep_tool(&json!({
            "pattern": "needle", "path": path, "output": "count", "offset": 5
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(output.contains("本页无结果"), "{output}");
        assert!(output.contains("已跳过 1 条命中"), "{output}");
        assert!(!output.contains("个条命中"), "{output}");
    }

    #[test]
    fn block起始行超过文件行数时不崩溃() {
        // 反向扫描块起点时 start 可能越过 lines 末尾；这里必须安全回退为 E_RANGE。
        let path = 临时文本文件("a\nb\n");
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let output = read(&json!({
            "path": posix_path, "start": 5, "block": true
        }));
        let _ = fs::remove_file(path);
        assert!(output.starts_with("E_RANGE"), "{output}");
    }

    #[test]
    fn block未找到代码块时续读范围钳制到总行数() {
        // 纯文本没有花括号：block=true 回退到无 end 区间，
        // 续读 ranges 不能把 usize::MAX 写出去。
        let dir = 临时目录("read-block-plain");
        let file = dir.join("plain.txt");
        let content = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&file, format!("{content}\n")).unwrap();
        let path = file.to_string_lossy().replace('\\', "/");

        let first = read(&json!({
            "path": path, "start": 1, "block": true, "budget": 360
        }));
        assert!(
            !first.contains("18446744073709551615"),
            "续读范围写入了 usize::MAX: {first}"
        );
        let tail = first
            .split("ranges=")
            .nth(1)
            .unwrap_or_else(|| panic!("缺少 ranges 续读: {first}"));
        let ranges: Value = serde_json::from_str(tail.lines().next().unwrap()).unwrap();
        assert_eq!(ranges[0][1], json!(30), "续读范围应钳制到真实总行数");

        let second = read(&json!({
            "path": path, "ranges": ranges, "budget": 10000
        }));
        fs::remove_dir_all(dir).unwrap();
        assert!(second.contains("30|line29"), "续读应读到末行: {second}");
        assert!(!second.contains("18446744073709551615"), "{second}");
        let mut numbers = Vec::new();
        for output in [&first, &second] {
            numbers.extend(output.lines().filter_map(|line| {
                let (number, _) = line.split_once('|')?;
                number.parse::<usize>().ok()
            }));
        }
        assert_eq!(
            numbers,
            (1..=30).collect::<Vec<_>>(),
            "跨页重复或遗漏: {first}\n---\n{second}"
        );
    }

    #[test]
    fn fs_read_paths声明数量上限() {
        let defs = tool_defs();
        let read = defs
            .iter()
            .find(|def| def["name"] == "fs_read")
            .expect("缺少 fs_read");
        assert_eq!(
            read["inputSchema"]["properties"]["paths"]["maxItems"],
            json!(20),
            "fs_read.paths 必须在 schema 中声明最多 20 个"
        );
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
    fn grep_max_file_size跳过超限文件并给可调提示() {
        let dir = 临时目录("grep-max-size");
        fs::write(dir.join("a.txt"), "needle\n").unwrap(); // 7 字节
        fs::write(dir.join("b.txt"), "needle needle\n").unwrap(); // 14 字节
        let path = dir.to_string_lossy().replace('\\', "/");

        let out = grep_tool(&json!({
            "pattern": "needle",
            "path": path,
            "output": "files",
            "max_file_size": 7
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(out.contains("a.txt"), "{out}");
        assert!(!out.contains("b.txt"), "超过上限的文件必须跳过: {out}");
        assert!(out.contains("跳过 1"), "跳过必须回报: {out}");
        assert!(out.contains("max_file_size"), "必须给出可调参数: {out}");
        assert!(out.contains("默认 8.0M"), "必须写清默认值: {out}");
        assert!(out.contains("0 表示不限制"), "必须写清 0 的含义: {out}");
    }

    #[test]
    fn grep_max_file_size_0表示不限制() {
        let dir = 临时目录("grep-max-size-zero");
        fs::write(dir.join("a.txt"), "needle\n").unwrap();
        fs::write(dir.join("b.txt"), "needle needle\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");

        let out = grep_tool(&json!({
            "pattern": "needle",
            "path": path,
            "output": "files",
            "max_file_size": 0
        }));
        fs::remove_dir_all(dir).unwrap();

        assert!(out.contains("a.txt"), "{out}");
        assert!(out.contains("b.txt"), "0 表示不限制，必须命中大文件: {out}");
        assert!(!out.contains("跳过"), "不应再报跳过: {out}");
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
            "E_BAD_ARGS 缺少 edits；需要 [{\"old\":\"原文\",\"new\":\"替换\"}] 或 [{\"pattern\":\"正则\",\"replacement\":\"替换\"}]".to_string()
        } else if let Some(list) = value.as_array() {
            let bad = list.iter().position(|item| {
                let literal = item.get("old").and_then(Value::as_str).is_some()
                    && item.get("new").and_then(Value::as_str).is_some();
                let regex_mode = item.get("pattern").and_then(Value::as_str).is_some()
                    && item.get("replacement").and_then(Value::as_str).is_some();
                !literal && !regex_mode
            });
            match bad {
                Some(i) => format!(
                    "E_BAD_ARGS edits[{}] 需要 old+new 或 pattern+replacement；当前={}",
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
    // 写入成功后清掉读取缓存：即使 hash 校验已能挡住旧内容，这里再兜一层。
    for item in &prepared {
        crate::cache::invalidate(&item.fp.display().to_string());
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
    crate::cache::invalidate(&fp.display().to_string());
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
    // repo map：一次给「哪些文件值得先看」，而不是路径列表。
    if arg_bool(args, "map", false) {
        if arg_bool(args, "dirs", false) || arg_bool(args, "detail", false) {
            return "E_BAD_ARGS map=true 不能与 dirs/detail 同时使用；repo map 自带符号与排序"
                .into();
        }
        return crate::map::build(
            &root,
            pattern,
            arg_str(args, "focus").unwrap_or(""),
            budget,
            arg_usize(args, "max_files", 2000),
            arg_usize(args, "time_budget_ms", 5000) as u128,
        );
    }
    let limit = arg_usize(args, "limit", 200);
    let offset = arg_usize(args, "offset", 0);
    if limit == 0 {
        return "E_BAD_ARGS limit 必须大于 0；省略表示默认 200".into();
    }
    let opts = WalkOpts {
        include_dirs: arg_bool(args, "dirs", false),
        limit,
        follow_links: false,
        offset,
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
                } else if report.offset_skipped > 0 {
                    // offset 越界/单文件根跳页：确实有匹配条目，只是被本页跳过。
                    format!(
                        "ok 本页无结果：offset={} 已跳过 {} 个匹配条目",
                        offset, report.offset_skipped
                    )
                } else {
                    format!("ok 无匹配（{} pattern={pattern}）", root.display())
                }
            } else {
                let detail = arg_bool(args, "detail", false);
                let lines: Vec<String> = entries
                    .iter()
                    .map(|entry| walk::render_entries(std::slice::from_ref(entry), detail))
                    .collect();
                let mut shown = 0usize;
                render::converge_budget(budget, |body_budget| {
                    let mut text = String::new();
                    shown = 0;
                    for line in &lines {
                        if text.len() + line.len() > body_budget {
                            break;
                        }
                        text.push_str(line);
                        shown += 1;
                    }
                    if shown == 0 {
                        return "E_BUDGET 匹配条目存在但预算不足以输出完整路径；请增大 budget"
                            .into();
                    }
                    let budget_cut = shown < entries.len();
                    if budget_cut {
                        text.push_str(&format!("… 输出截断：已给 {shown} 条"));
                        if !report.stopped {
                            text.push_str(&format!("（共 {} 条匹配）", entries.len()));
                        }
                        text.push('\n');
                    }
                    if budget_cut || (report.stopped && !report.lookahead_limited) {
                        text.push_str(&format!("… next_offset={}\n", offset + shown));
                    }
                    if report.lookahead_limited {
                        // 只探测了有限个后续项，无法确认是否还有下一页。
                        text.push_str("… 达到 limit，后续未探测完；增大 limit 或缩小 path\n");
                    }
                    if timed_out {
                        text.push_str(&format!(
                            "… 时间预算用尽（{}ms，已访问 {} 项），结果可能不完整\n",
                            opts.time_budget_ms, report.entries_visited
                        ));
                    }
                    text.trim_end().to_string()
                })
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
    let mut opts = GrepOpts {
        pattern,
        output,
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
    // 暴露既有的大文件搜索上限：0 表示不限制，其余按字节数比较。
    if let Some(value) = args.get("max_file_size").and_then(Value::as_u64) {
        opts.max_file_size = if value == 0 { u64::MAX } else { value };
    }
    match search::grep(&root, &opts) {
        Ok(r) => {
            // 大文件被跳过时必须回报，否则模型会把“没搜到”当成“不存在”。
            let skip_note = if r.files_skipped_large > 0 {
                format!(
                    "\n… 跳过 {} 个文件：超过 max_file_size={}（默认 {}，0 表示不限制），最大 {}；调大 max_file_size 可纳入搜索",
                    r.files_skipped_large,
                    io::human_size(opts.max_file_size),
                    io::human_size(8 * 1024 * 1024),
                    io::human_size(r.largest_skipped)
                )
            } else {
                String::new()
            };
            if r.hits.is_empty() && r.file_counts.is_empty() {
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
                    let text = if output == GrepOutput::Files {
                        format!(
                            "ok 本页无结果：offset={} 已跳过 {} 个匹配文件",
                            opts.offset, r.offset_skipped
                        )
                    } else {
                        format!(
                            "ok 本页无结果：offset={} 已跳过 {} 条命中",
                            opts.offset, r.offset_skipped
                        )
                    };
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
    if targets.len() > 50 {
        return format!(
            "E_BAD_ARGS fs_stat 一次最多处理 50 个路径，当前 {} 个；请分批查询",
            targets.len()
        );
    }
    let mut out = String::new();
    for t in &targets {
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
            let bytes = match io::read_bytes(&fp.wide()) {
                Ok(bytes) => bytes,
                Err(error) => {
                    out.push_str(&format!("{}\n", io::with_path(error, &fp)));
                    continue;
                }
            };
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
            match io::snapshot_from_bytes(&fp, bytes) {
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

#[cfg(test)]
mod summary_tests {
    use super::*;
    use std::fs;

    fn 临时目录(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fsops-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn summary只输出声明与行号且跳过测试模块() {
        let dir = 临时目录("summary");
        let path = dir.join("sample.rs");
        fs::write(
            &path,
            "use std::x;\n\npub struct A {\n    v: u8,\n}\n\nimpl A {\n    pub fn new() -> Self { Self { v: 0 } }\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t1() {}\n}\n\npub fn tail() {}\n",
        )
        .unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call("fs_read", &json!({"path": posix, "summary": true}));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("结构摘要"), "{out}");
        assert!(out.contains("3|pub struct A"), "{out}");
        assert!(out.contains("8|    pub fn new"), "{out}");
        assert!(out.contains("pub fn tail"), "{out}");
        assert!(!out.contains("fn t1"), "测试模块里的函数不应出现: {out}");
        assert!(out.contains("block=true"), "必须给出继续读的入口: {out}");
    }

    #[test]
    fn summary与block或ranges互斥() {
        let path = 临时目录("summary-conflict").join("a.rs");
        fs::write(&path, "fn a() {}\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_read",
            &json!({"path": posix, "summary": true, "block": true}),
        );
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
        let out = call(
            "fs_read",
            &json!({"path": posix, "summary": true, "ranges": [[1, 1]]}),
        );
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
    }

    #[test]
    fn summary拒绝数据文件() {
        let dir = 临时目录("summary-data");
        let path = dir.join("a.json");
        fs::write(&path, "{\"a\": 1}\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call("fs_read", &json!({"path": posix, "summary": true}));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
        assert!(out.contains("数据文件"), "{out}");
    }
}

#[cfg(test)]
mod roundtrip_tests {
    use super::*;
    use std::fs;

    fn 目录(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fsops-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads一次读多文件的不同区间() {
        let dir = 目录("reads-basic");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        fs::write(&a, "a1\na2\na3\na4\n").unwrap();
        fs::write(&b, "b1\nb2\nb3\n").unwrap();
        let out = call(
            "fs_read",
            &json!({"reads": [
                {"path": a.to_string_lossy().replace('\\', "/"), "start": 2, "end": 3},
                {"path": b.to_string_lossy().replace('\\', "/"), "start": 1, "end": 1}
            ]}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("2|a2"), "{out}");
        assert!(out.contains("3|a3"), "{out}");
        assert!(!out.contains("1|a1"), "{out}");
        assert!(out.contains("1|b1"), "{out}");
    }

    #[test]
    fn reads预算按项均分且两项都有输出() {
        let dir = 目录("reads-budget");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        let body = |prefix: &str| {
            (1..=50)
                .map(|i| format!("{prefix}{i}\n"))
                .collect::<String>()
        };
        fs::write(&a, body("a")).unwrap();
        fs::write(&b, body("b")).unwrap();
        let out = call(
            "fs_read",
            &json!({"reads": [
                {"path": a.to_string_lossy().replace('\\', "/")},
                {"path": b.to_string_lossy().replace('\\', "/")}
            ], "budget": 800}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("a.txt") && out.contains("b.txt"), "{out}");
        assert!(out.contains("1|a1"), "{out}");
        assert!(out.contains("1|b1"), "{out}");
    }

    #[test]
    fn reads某文件不存在只影响该项() {
        let dir = 目录("reads-missing");
        let missing = dir.join("nope.txt");
        let ok = dir.join("ok.txt");
        fs::write(&ok, "ok1\nok2\n").unwrap();
        let out = call(
            "fs_read",
            &json!({"reads": [
                {"path": missing.to_string_lossy().replace('\\', "/")},
                {"path": ok.to_string_lossy().replace('\\', "/")}
            ]}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("E_"), "缺失文件应报错: {out}");
        assert!(out.contains("1|ok1"), "另一项仍应输出: {out}");
    }

    #[test]
    fn focus返回命中所在的完整代码块() {
        let dir = 目录("focus-basic");
        let path = dir.join("a.rs");
        fs::write(
            &path,
            "fn alpha() {\n    let a = 1;\n}\n\nfn beta() {\n    let target = 2;\n    let b = 3;\n}\n",
        )
        .unwrap();
        let out = call(
            "fs_read",
            &json!({"path": path.to_string_lossy().replace('\\', "/"), "focus": "target"}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("5|fn beta()"), "应包含函数签名: {out}");
        assert!(out.contains("6|    let target = 2;"), "{out}");
        assert!(out.contains("8|}"), "应包含函数结束括号: {out}");
        assert!(!out.contains("let a = 1"), "不应包含未命中的函数体: {out}");
    }

    #[test]
    fn focus无命中给出建议() {
        let dir = 目录("focus-none");
        let path = dir.join("a.rs");
        fs::write(&path, "fn alpha() {}\n").unwrap();
        let out = call(
            "fs_read",
            &json!({"path": path.to_string_lossy().replace('\\', "/"), "focus": "不存在的东西"}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("E_NO_MATCH"), "{out}");
        assert!(out.contains("fs_grep"), "{out}");
    }

    #[test]
    fn focus与summary互斥() {
        let dir = 目录("focus-conflict");
        let path = dir.join("a.rs");
        fs::write(&path, "fn alpha() {}\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_read",
            &json!({"path": posix, "focus": "a", "summary": true}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
    }

    #[test]
    fn focus最多返回12个块并标注截断() {
        let dir = 目录("focus-truncate");
        let path = dir.join("many.rs");
        let mut body = String::new();
        for i in 0..20 {
            body.push_str(&format!("fn f{i}() {{\n    let hit = {i};\n}}\n\n"));
        }
        fs::write(&path, body).unwrap();
        let out = call(
            "fs_read",
            &json!({"path": path.to_string_lossy().replace('\\', "/"), "focus": "hit", "budget": 60000}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("仅显示前 12 块"), "{out}");
        assert!(out.contains("命中 20 个代码块"), "{out}");
    }
    #[test]
    fn 第二次同区间读返回未变更摘要() {
        let dir = 目录("cache-hit");
        let path = dir.join("a.txt");
        fs::write(
            &path,
            (1..=30).map(|i| format!("line{i}\n")).collect::<String>(),
        )
        .unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let first = call("fs_read", &json!({"path": posix, "start": 1, "end": 30}));
        assert!(first.contains("1|line1"), "{first}");
        let second = call("fs_read", &json!({"path": posix, "start": 1, "end": 30}));
        let _ = fs::remove_dir_all(&dir);
        assert!(second.contains("未变更"), "{second}");
        assert!(!second.contains("1|line1"), "{second}");
        assert!(second.contains("force=true"), "{second}");
    }

    #[test]
    fn 不同区间不命中缓存() {
        let dir = 目录("cache-range");
        let path = dir.join("a.txt");
        fs::write(
            &path,
            (1..=30).map(|i| format!("line{i}\n")).collect::<String>(),
        )
        .unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let _ = call("fs_read", &json!({"path": posix, "start": 1, "end": 10}));
        let out = call("fs_read", &json!({"path": posix, "start": 20, "end": 25}));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("20|line20"), "未读过的区间必须返回正文: {out}");
        assert!(!out.contains("未变更"), "{out}");
    }

    #[test]
    fn 文件改动后不命中缓存() {
        let dir = 目录("cache-change");
        let path = dir.join("a.txt");
        fs::write(&path, "old1\nold2\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let _ = call("fs_read", &json!({"path": posix, "start": 1, "end": 2}));
        fs::write(&path, "new1\nnew2\n").unwrap();
        let out = call("fs_read", &json!({"path": posix, "start": 1, "end": 2}));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("1|new1"), "文件改动后必须返回新内容: {out}");
        assert!(!out.contains("未变更"), "{out}");
    }

    #[test]
    fn glob_map输出排序后的文件与符号() {
        let dir = 目录("glob-map");
        fs::write(
            dir.join("core.rs"),
            "pub struct Token {}\npub fn verify() {}\n",
        )
        .unwrap();
        fs::write(
            dir.join("user.rs"),
            "fn use_it() { let t = Token; verify(); }\n",
        )
        .unwrap();
        fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        let posix = dir.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_glob",
            &json!({"path": posix, "pattern": "**/*", "map": true}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("core.rs"), "{out}");
        assert!(out.contains("符号"), "{out}");
        assert!(out.contains("pub fn verify"), "{out}");
        let core = out.find("core.rs").expect("core.rs");
        let user = out.find("user.rs").expect("user.rs");
        assert!(core < user, "被引用的 core.rs 应排在 user.rs 前: {out}");
    }

    #[test]
    fn glob_map与dirs互斥() {
        let out = call(
            "fs_glob",
            &json!({"path": ".", "pattern": "**/*", "map": true, "dirs": true}),
        );
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
    }

    #[test]
    fn 小文件自适应预算一次读完() {
        let dir = 目录("adaptive-budget");
        let path = dir.join("big.txt");
        let body: String = (1..=400)
            .map(|i| format!("{i:04}-{}\n", "x".repeat(30)))
            .collect();
        fs::write(&path, body).unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call("fs_read", &json!({"path": posix}));
        let _ = fs::remove_dir_all(&dir);
        assert!(
            out.contains("400|"),
            "14KB 文件应一次读完: {}",
            &out[out.len().saturating_sub(300)..]
        );
        assert!(!out.contains("续读"), "不应要求续读: {out}");
    }
    #[test]
    fn 编辑后同区间再读返回新内容() {
        let dir = 目录("cache-edit");
        let path = dir.join("a.txt");
        fs::write(&path, "aaa\nbbb\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let _ = call("fs_read", &json!({"path": posix, "start": 1, "end": 2}));
        let edited = call(
            "fs_edit",
            &json!({"path": posix, "edits": [{"old": "aaa", "new": "zzz"}]}),
        );
        assert!(edited.starts_with("ok"), "{edited}");
        let out = call("fs_read", &json!({"path": posix, "start": 1, "end": 2}));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("1|zzz"), "编辑后必须返回新内容: {out}");
        assert!(!out.contains("未变更"), "{out}");
    }

    #[test]
    fn force绕过缓存() {
        let dir = 目录("cache-force");
        let path = dir.join("a.txt");
        fs::write(&path, "x1\nx2\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let _ = call("fs_read", &json!({"path": posix, "start": 1, "end": 2}));
        let out = call(
            "fs_read",
            &json!({"path": posix, "start": 1, "end": 2, "force": true}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("1|x1"), "force=true 必须返回正文: {out}");
    }
    #[test]
    fn reads与paths互斥() {
        let dir = 目录("reads-conflict");
        let a = dir.join("a.txt");
        fs::write(&a, "x\n").unwrap();
        let posix = a.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_read",
            &json!({"reads": [{"path": posix.clone()}], "paths": [posix]}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
    }
}
