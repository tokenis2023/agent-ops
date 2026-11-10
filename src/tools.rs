//! 工具层：9 个工具的定义与执行。
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

/// 工具定义（schema）的缓存。
///
/// 为什么必须缓存：`call()` 每次都要做参数校验，而校验需要 schema；
/// 直接构建一次 `tool_defs()` 要分配约 10KB 的 JSON（9 个工具、上百个字段），
/// 单个工具调用光这一项就花掉约 200µs——比实际读写文件还贵。
/// schema 是编译期常量，只读，进程内复用一份即可。
static TOOL_DEFS: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();

/// 取工具定义（进程内只构建一次）。
pub fn tool_defs() -> &'static [Value] {
    TOOL_DEFS.get_or_init(build_tool_defs)
}

fn build_tool_defs() -> Vec<Value> {
    let mut defs = vec![
        json!({
            "name": "fs_read",
            "description": "读取文件，优先替代宿主 read_file/cat/head/tail/Get-Content。常用：单文件 `path`；多文件异构 `reads=[{path,start,end}]`（一次读完，勿一文件一轮）；按正则取整块 `focus=<正则>`（省 grep→block 两步）；日志尾 `tail=N`。模式表：`path` 与 `paths` 是并集（共用 start/end）；`focus`/`summary` 仅单文件，`reads`/`focus`/`tail`/`summary` 各有互斥范围（读对应参数说明，区间写在 reads 项内）；默认 32KB 并给续读参数；支持 DOCX/PDF、GBK/UTF-16/BOM/CRLF；>32MB 用 scan=window（默认）/full。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "文件路径，如 /d/repo/app/src/main.rs"},
                    "paths": {"type": "array", "items": {"type": "string"}, "maxItems": 20, "description": "一次读多个文件（最多 20 个），所有文件共用 start/end"},
                    "reads": {
                        "type": "array",
                        "maxItems": 20,
                        "description": "一次读多个文件的不同区间，每项 {path,start?,end?,ranges?,block?,max_line?,scan?,budget?,focus?}；与 path/paths/ranges/start/end/block/summary/focus 互斥，预算按项均分",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "start": {"type": "integer"},
                                "end": {"type": "integer"},
                                "ranges": {"type": "array", "items": {"type": "array", "items": {"type": "integer", "minimum": 1}, "minItems": 2, "maxItems": 2}},
                                "block": {"type": "boolean"},
                                "max_line": {"type": "integer"},
                                "scan": {"type": "string", "enum": ["window", "full"]},
                                "budget": {"type": "integer", "description": "该项独立预算；未给时与其他项均分顶层 budget"},
                                "focus": {"type": "string", "description": "该项只返回命中所在的完整代码块"}
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
                    "budget": {"type": "integer", "description": "最多返回字节数，默认 32768（约 8K token）；文件 ≤32KB 且未显式指定时自动读全文"},
                    "max_line": {"type": "integer", "description": "每行最多字符数，默认 240；0 表示不截断长行"},
                    "scan": {
                        "type": "string",
                        "enum": ["window", "full"],
                        "description": "仅对 >32MB 文件生效：window（默认）只读到请求区间末尾并探测是否还有内容，毫秒级返回，但不给全文 hash/总行数；full 全文件扫描，给精确 hash 与总行数"
                    },
                    "summary": {"type": "boolean", "description": "只输出结构摘要：声明行 + 行号，不展开函数体；行号可直接接 block=true 读实现。与 ranges/block/end 互斥，数据文件与超大文件不支持"},
                    "tail": {"type": "integer", "minimum": 1, "description": "只读最后 N 行（日志场景）：与 start/end/ranges/block/summary/focus/paths 互斥，文件不足 N 行时读全部；免去先 fs_stat 拿总行数那一步"},
                    "focus": {"type": "string", "description": "只返回匹配该正则的完整代码块（含上下文），替代「先 fs_grep 再 block=true」两步。数据文件（JSON/YAML/Markdown 等）没有代码块，只给命中行 ±2 行。仅单文件，与 paths/reads/ranges/start/end/block/summary 互斥。JSON 字符串里反斜杠需双写"},
                    "force": {"type": "boolean", "description": "读取缓存默认关闭；仅当 FSOPS_MCP_CACHE=1 开启后，用本参数绕过未变更摘要、强制返回正文。默认部署重复读直接返回全文，无需传"}
                },
                "required": []
            }
        }),
        json!({
            "name": "fs_edit",
            "description": "精确替换文本（old→new）或正则替换（pattern→replacement）：不知道确切原文时用 pattern，省掉为抄原文而做的一次 fs_read。两套参数必须二选一：old+new 或 pattern+replacement；同时给会报 E_BAD_EDIT。优先替代 patch/Edit/apply_patch。同文件多处用 edits，多文件用 files；全部预检通过才写入。结构调整后用 diff=true 自查改动片段，省一次全文回读。自动处理 CRLF/LF 并保留原编码；DOCX 不支持精确替换，请用 fs_write 整体重建。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "单文件路径；批量时改用 files"},
                    "edits": {
                        "type": "array",
                        "description": "单文件的修改列表",
                        "items": edit_item_schema(false)
                    },
                    "files": {
                        "type": "array",
                        "description": "多文件批量编辑；全部文件预检成功后逐文件原子写入，最多 50 个",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "edits": {"type": "array", "items": edit_item_schema(true)},
                                "expect_hash": {"type": "string", "description": "该文件的并发保护指纹"},
                                "normalize_eol": {"type": "boolean", "description": "该文件含混合换行时，显式允许归一为主换行风格（多数行使用的风格，平局取 LF）"}
                            },
                            "required": ["path", "edits"]
                        }
                    },
                    "dry_run": {"type": "boolean", "description": "只校验不落盘"},
                    "diff": {"type": "boolean", "description": "附带被替换的原文/新文片段（`@@ 行号` + `-原文` / `+新文`）；默认关闭，改结构后想自查时打开，可省一次 fs_read"},
                    "normalize_eol": {"type": "boolean", "description": "混合换行时显式归一为主换行风格（多数行使用的风格，平局取 LF）"},
                    "expect_hash": {"type": "string", "description": "单文件并发保护：文件已变则拒绝写入"}
                },
                "required": []
            }
        }),
        json!({
            "name": "fs_write",
            "description": "写入文件。优先替代 write_file/Write。原子替换、自动建目录、保留原有编码与换行风格；文本外也支持生成 DOCX（纯文本段落，整体重建）。多文件用 files 一次提交（全部预检通过才写），不要一个文件一轮。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "files": {
                        "type": "array",
                        "description": "多文件批量写入（一次建 10 个文件不再 10 轮）；全部预检通过才写入，最多 50 个；与 path/content 互斥",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "content": {"type": "string"},
                                "expect_hash": {"type": "string"},
                                "normalize_eol": {"type": "boolean"}
                            },
                            "required": ["path", "content"]
                        }
                    },
                    "normalize_eol": {"type": "boolean", "description": "新文件或原文件为混合换行时，显式归一为主换行风格（多数行使用的风格，平局取 LF）；已有稳定文件始终沿用原换行"},
                    "expect_hash": {"type": "string", "description": "并发保护：文件已变则拒绝覆盖"},
                    "dry_run": {"type": "boolean", "description": "只做路径/编码/换行/hash 预检，不创建也不覆盖文件"}
                },
                "required": []
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
                    "budget": {"type": "integer", "description": "最终响应最多字节数，默认 16384"},
                    "map": {"type": "boolean", "description": "输出 repo map：按符号被引用程度、入口点和 focus 排序的文件与符号，一次调用回答先看哪些文件"},
                    "focus": {"type": "string", "description": "map=true 时的关注词，空格分隔；文件名或符号命中会加权"},
                    "max_files": {"type": "integer", "description": "map=true 时最多扫描的文件数，默认 2000"}
                },
                "required": []
            }
        }),
        json!({
            "name": "fs_inspect",
            "description": "一次看清目录：文件规模分布 + 可选的正则命中分布与样例，替代「先 fs_glob 看结构、再 fs_stat 查大小、再 fs_grep 找目标」的多轮探索。适合接手陌生仓库或任务开头；已知具体目标后改用 fs_grep/fs_read。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "根目录，默认当前目录"},
                    "pattern": {"type": "string", "description": "可选 Rust 正则：给了就附带命中分布与前几条命中样例"},
                    "focus": {"type": "string", "description": "可选关键词（空格分隔）：影响「最相关文件」的排序，命中文件名或路径的优先"},
                    "limit": {"type": "integer", "description": "列出的文件数，默认 15"},
                    "budget": {"type": "integer", "description": "响应字节上限，默认 16384"},
                    "time_budget_ms": {"type": "integer", "description": "目录遍历时间预算，默认 3000 毫秒；超时返回已扫到的部分并标注"},
                    "no_ignore": {"type": "boolean", "description": "忽略 .gitignore 与 node_modules/target 等默认跳过项；只在明确要统计依赖源码时用"}
                },
                "required": []
            }
        }),
        json!({
            "name": "fs_grep",
            "description": "正则流式搜索，优先替代 grep/rg/Select-String。按文件分组，支持 DOCX/PDF，output=files/count/matches/snippets；截断返回 next_offset。count 默认完整统计；显式给 limit 时按命中截断并标“本页可见”，不是总数。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Rust regex 语法（不支持 lookaround 与反向引用，与 PCRE 不同）；JSON 字符串里反斜杠需双写"},
                    "path": {"type": "string", "description": "搜索根目录，默认当前目录"},
                    "glob": {"type": "string", "description": "只看匹配的文件，如 **/*.ts"},
                    "ignore_case": {"type": "boolean"},
                    "output": {
                        "type": "string",
                        "enum": ["files", "count", "matches", "snippets"],
                        "description": "files 每个匹配文件只给路径；count 给路径和真实行命中数（默认完整统计）；matches 给命中行；snippets 默认带 2 行上下文"
                    },
                    "context": {"type": "integer", "description": "上下文行数（最多 5）；matches 默认 0，snippets 默认 2"},
                    "expand": {
                        "type": "string",
                        "enum": ["block"],
                        "description": "expand=block：把每条命中展开成它所在的完整代码块（花括号配平）。省掉「grep 拿行号 → fs_read block=true 再读一遍」那一轮；只在 matches/snippets 生效，预算不足时降级为普通命中行"
                    },
                    "limit": {"type": "integer", "description": "本页上限：files 按文件，matches/snippets 按命中，count 显式设置时按命中；默认 200（count 省略时完整统计），必须大于 0；预算才是真正的输出上限"},
                    "max_per_file": {"type": "integer", "description": "每个文件最多返回的命中数，默认不限制；省略部分会在尾注说明"},
                    "max_line": {"type": "integer", "description": "命中行最多显示字符数，默认 240（与 fs_read 一致）；0 表示不截断，一次拿到完整长行，省掉再 fs_read 一轮"},
                    "offset": {"type": "integer", "description": "files 跳过前 N 个匹配文件，其他模式跳过前 N 个命中；用于 next_offset 续取"},
                    "parallel": {"type": "boolean", "description": "并行扫描；省略时自动按仓库规模选择：小仓库串行，大仓库并行；true 强制并行，false 强制串行；输出与串行一致"},
                    "budget": {"type": "integer", "description": "最终响应最多字节数，默认 16384"},
                    "time_budget_ms": {"type": "integer", "description": "文件与目录遍历总时间预算，默认 5000 毫秒"},
                    "max_file_size": {"type": "integer", "description": "只搜索不超过该字节数的文件，默认 8388608（8 MiB）；0 表示不限制。被跳过的文件会在尾注报告"},
                    "no_ignore": {"type": "boolean", "description": "忽略 .gitignore 与 node_modules/target 等默认跳过项；只在明确要搜依赖源码时用"}
                },
                "required": ["pattern"]
            }
        }),
        json!({
            "name": "fs_stat",
            "description": "查看路径元信息。优先替代 stat/ls/Get-Item。返回类型、大小、修改时间、换行风格、编码、内容指纹。path 或 paths 至少给一个；detail=server 是不带路径的自检，用来确认宿主连上的服务端版本与工具数。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "paths": {"type": "array", "items": {"type": "string"}, "maxItems": 50},
                    "detail": {
                        "type": "string",
                        "enum": ["meta", "hash", "full", "server"],
                        "description": "meta 只查元信息不读内容；hash 读一次给指纹；full 额外统计行数，默认 full；server 返回服务端版本与工具数，用于自检"
                    }
                },
                "required": []
            }
        }),
        json!({
            "name": "fs_exec",
            "description": "执行程序（argv 数组，不经过 shell）。替代 PowerShell 跑 cargo/git/npm/python：没有引号/插值、2>&1、ErrorRecord、$LASTEXITCODE 问题。非零退出不是错误（前缀写 exit=N 非零，要当失败使用 check=true）；超时会结束整个进程树。启动任务必须给 program；只轮询/终止用 job=<id>，列任务用 list=true。",
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
                    "check": {"type": "boolean", "description": "非零退出按 E_EXIT 错误返回；默认 false，非零仍是 ok"},
                    "background": {"type": "boolean", "description": "会话内后台任务：立即返回 job id，用 job=<id> 轮询输出，list=true 列出全部；进程随 MCP 会话结束"},
                    "job": {"type": "integer", "description": "轮询/终止后台任务：传启动时返回的 job id"},
                    "list": {"type": "boolean", "description": "列出当前会话的全部后台任务（id/pid/状态/时长）"},
                    "wait_ms": {"type": "integer", "description": "配合 job：最多等待 N 毫秒直到任务退出再返回（上限 60000）；超时仍运行则返回增量输出"},
                    "kill": {"type": "boolean", "description": "配合 job 使用：结束该任务的整个进程树"}
                },
                "required": []
            }
        }),
        json!({
            "name": "fs_file",
            "description": "文件管理：copy/move/remove/mkdir/convert/archive/extract。替代 PowerShell 的 Copy-Item/Move-Item/Remove-Item/New-Item 与压缩解压。必填映射：copy/move/archive/extract 要 src+dst；remove/mkdir/convert 要 path；remove 必须再给 confirm=true。convert 需 encoding；只改换行也要给 encoding（如 utf-8）。",
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
                if spec["type"] == "integer" && spec.get("minimum").is_none() {
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
///
/// 必须先全局排序：模型常从多段 grep 结果里拼区间，顺序不可控；
/// 只与「已输出的最后一个区间」比较会把后到的 [1,6] 吞进先到的 [10,14]，
/// 静默丢掉一整段内容——这比多输出几行危险得多。
fn merge_ranges(ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut sorted = ranges;
    sorted.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(sorted.len());
    for (from, to) in sorted {
        if let Some(last) = out.last_mut() {
            if from <= last.1.saturating_add(1) {
                last.1 = last.1.max(to);
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

/// 把 grep 命中行展开成它所在的完整代码块（供 `fs_grep expand=block`）。
///
/// 只对花括号语言有意义：`enclosing_block` 找不到边界时，该命中保持原样（只给行）。
/// 同一文件的多条命中若落在同一个块里，只输出一次，避免重复内容撑爆上下文。
fn expand_hits_to_blocks(r: &search::GrepResult, root: &FsPath) -> Option<String> {
    if r.hits.is_empty() {
        return None;
    }
    // 每个文件只读一次。按首次出现顺序处理，输出顺序对模型更可预测。
    let mut order: Vec<&str> = Vec::new();
    let mut by_file: std::collections::HashMap<&str, Vec<usize>> = std::collections::HashMap::new();
    for hit in &r.hits {
        if hit.is_context {
            continue;
        }
        let entry = by_file.entry(hit.path.as_str()).or_default();
        if entry.is_empty() {
            order.push(hit.path.as_str());
        }
        if !entry.contains(&hit.line) {
            entry.push(hit.line);
        }
    }
    let mut out = String::new();
    for path in order {
        // grep 返回的 path 相对搜索根；直接 parse 会按进程 cwd 解析而读不到。
        let joined = std::path::Path::new(&root.native()).join(path);
        let fp = match FsPath::parse(&joined.to_string_lossy()) {
            Ok(fp) => fp,
            Err(_) => continue,
        };
        let snapshot = match io::read_snapshot(&fp) {
            Ok(snapshot) => snapshot,
            Err(_) => continue,
        };
        let lines: Vec<&str> = snapshot.doc.text.split('\n').collect();
        let mut printed: Vec<(usize, usize)> = Vec::new();
        for line in by_file.get(path).into_iter().flatten() {
            let (from, to) = match enclosing_block(&lines, *line) {
                Some(range) => range,
                None => (*line, *line),
            };
            // 同一个块里的多条命中只打印一次（grep 常在一个函数里命中好几行）。
            if printed.iter().any(|(a, b)| from >= *a && to <= *b) {
                continue;
            }
            if out.is_empty() {
                out.push_str(path);
            } else {
                out.push('\n');
                out.push_str(path);
            }
            out.push('\n');
            for n in from..=to.min(lines.len()) {
                let text = lines.get(n - 1).copied().unwrap_or("");
                let sep = if n == *line { ":" } else { "-" };
                out.push_str(&format!("{n}{sep}{text}\n"));
            }
            printed.push((from, to));
        }
    }
    if out.is_empty() {
        return None;
    }
    out.push_str("… expand=block：已把命中展开为所在代码块；只需了解命中行本身时去掉该参数");
    Some(out)
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
/// fs_edit 的单条修改项 schema。
///
/// `compact` 用于 `files[].edits`：字段名与类型必须与顶层完全一致（校验靠它），
/// 但 7 条说明在嵌套里重复一遍要多花约 700 字符——工具 schema 每轮都进上下文，
/// 这是纯浪费。嵌套处只留字段名，模型看顶层说明即可。
fn edit_item_schema(compact: bool) -> serde_json::Value {
    if compact {
        return json!({
            "type": "object",
            "properties": {
                "old": {"type": "string"},
                "new": {"type": "string"},
                "pattern": {"type": "string"},
                "replacement": {"type": "string"},
                "all": {"type": "boolean"},
                "start": {"type": "integer", "minimum": 1},
                "end": {"type": "integer", "minimum": 1}
            },
            "required": []
        });
    }
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

/// 顶层简写模式可接受的字段：直接把单条修改写在顶层时，按这个顺序折进 edits[0]。
const EDIT_ITEM_FIELDS_FOR_SHORTHAND: [&str; 7] = [
    "old",
    "new",
    "pattern",
    "replacement",
    "all",
    "start",
    "end",
];

/// fs_edit 单条修改项自己的字段：容易被模型误提到顶层的关键字。
const EDIT_ITEM_FIELDS: [&str; 7] = [
    "old",
    "new",
    "pattern",
    "replacement",
    "all",
    "start",
    "end",
];

/// 按 schema 校验参数类型。
///
/// 为什么必须有：类型写错时旧代码会用 `unwrap_or(default)` 静默退回默认值，
/// 模型以为参数生效了却得到不同行为——这比直接报错危险得多。
fn validate_args(name: &str, args: &Value) -> Option<String> {
    let def = tool_defs().iter().find(|d| d["name"] == name)?;
    let props = def["inputSchema"]["properties"].as_object()?;
    let obj = args.as_object()?;
    // required：宿主把 schema 当提示文本时，漏必填不该等深层逻辑才发现。
    if let Some(required) = def["inputSchema"]["required"].as_array() {
        for key in required.iter().filter_map(|v| v.as_str()) {
            if !obj.contains_key(key) {
                return Some(format!(
                    "E_BAD_ARGS {name} 缺少必填参数 {key}；补上后原样重试"
                ));
            }
        }
    }
    for (key, value) in obj {
        let Some(spec) = props.get(key) else {
            // fs_edit 的字段级参数提到顶层是模型的自然写法（实测 3/3 次独立任务都这么写）。
            // 单条修改场景直接接受（edit_tool 会折进 edits[0]）；只有在已经给了 edits/files、
            // 或与批量模式混用时才报错——那时顶层字段确实无处安放。
            if name == "fs_edit" && EDIT_ITEM_FIELDS.contains(&key.as_str()) {
                let shorthand_ok = obj.get("edits").is_none() && obj.get("files").is_none();
                if !shorthand_ok {
                    return Some(format!(
                        "E_BAD_ARGS fs_edit.{key} 是单条修改自己的字段：给了 edits/files 时它必须在每一项里，不能放顶层"
                    ));
                }
                continue;
            }
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
        // enum / minimum / maxItems / 数组项必填：这些约束写在 schema 里，
        // 宿主未做严格校验时入口必须自己执行，否则模型要多付一轮试错。
        if let Some(allowed) = spec["enum"].as_array() {
            if !allowed.iter().any(|v| v == value) {
                let options = allowed
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join("/");
                return Some(format!(
                    "E_BAD_ARGS {name}.{key}={value} 不在允许值内；可用 {options}"
                ));
            }
        }
        if let (Some(min), Some(n)) = (spec["minimum"].as_u64(), value.as_u64()) {
            if n < min {
                return Some(format!(
                    "E_BAD_ARGS {name}.{key} 最小为 {min}，收到 {n}；改为 {min} 或更大值重试"
                ));
            }
        }
        if wanted == "array" {
            if let (Some(max), Some(items)) = (spec["maxItems"].as_u64(), value.as_array()) {
                if items.len() as u64 > max {
                    return Some(format!(
                        "E_BAD_ARGS {name}.{key} 最多 {max} 项，收到 {}；分批或改用 fs_grep 缩小范围",
                        items.len()
                    ));
                }
            }
            if let Some(item_required) = spec["items"]["required"].as_array() {
                for (index, item) in value.as_array().into_iter().flatten().enumerate() {
                    for field in item_required.iter().filter_map(|v| v.as_str()) {
                        if item.get(field).is_none() {
                            return Some(format!(
                                "E_BAD_ARGS {name}.{key}[{index}] 缺少必填字段 {field}；补上后原样重试"
                            ));
                        }
                    }
                }
            }
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
        "fs_inspect" => inspect_tool(args),
        "fs_grep" => grep_tool(args),
        "fs_stat" => stat(args),
        "fs_exec" => crate::exec::run_with_cancel(args, crate::mcp::cancel_flag()),
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
    // 尾注必须完整：预算再紧也不能给出「或调大 bud」这种半句。
    // 先按「一个符号都不显示」估算尾部，连它都放不下时直接报预算错误。
    let partial_tail = |shown: usize| {
        format!(
            "… 已显示 {}/{} 个符号（共 {} 行）| 用 fs_grep 精确定位，或调大 budget\n",
            shown,
            symbols.len(),
            total_lines
        )
    };
    let full_tail = format!(
        "… {} 个符号（共 {} 行）| 用 fs_read block=true start=<行号> 读实现\n",
        symbols.len(),
        total_lines
    );
    if symbols.is_empty() {
        let tail = "… 未识别到声明（可能是数据文件或不支持的语言）；请直接 fs_read 或 fs_grep\n";
        if header.len() + tail.len() > budget {
            return format!(
                "E_BUDGET 结构摘要至少需要 {}B（文件头 {}B），当前 budget={budget}；请调大 budget",
                header.len() + tail.len(),
                header.len()
            );
        }
        return format!("{header}{tail}");
    }
    let minimal_tail = partial_tail(0);
    if header.len() + minimal_tail.len() > budget {
        return format!(
            "E_BUDGET 结构摘要至少需要 {}B（文件头 {}B + 尾注 {}B），当前 budget={budget}；请调大 budget",
            header.len() + minimal_tail.len(),
            header.len(),
            minimal_tail.len()
        );
    }
    let mut out = String::with_capacity(budget.min(8 * 1024));
    out.push_str(header);
    let mut shown = 0usize;
    for symbol in symbols {
        let line = format!("{}|{}\n", symbol.line, shorten_line(&symbol.text, max_line));
        let tail_len = if shown + 1 == symbols.len() {
            full_tail.len()
        } else {
            partial_tail(shown + 1).len()
        };
        if out.len() + line.len() + tail_len > budget {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if shown == symbols.len() {
        out.push_str(&full_tail);
    } else {
        out.push_str(&partial_tail(shown));
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
    // tail 没有「每项一个尾部」的合理语义；静默忽略会让模型以为读到了末尾。
    // max_line / scan / block 则对每一项都成立，像 budget 一样传播。
    if args.get("tail").is_some() {
        return "E_BAD_ARGS reads 不能与 tail 同时使用；要读末尾请对单个文件用 fs_read tail=N"
            .into();
    }
    for key in [
        "path", "paths", "ranges", "start", "end", "block", "summary", "focus",
    ] {
        if args.get(key).is_some() {
            return format!(
                "E_BAD_ARGS reads 不能与 {key} 同时使用；每项的区间写在 reads 项内（path/start/end/ranges/block/max_line/budget/focus）"
            );
        }
    }
    if items.is_empty() {
        return "E_BAD_ARGS reads 不能为空".into();
    }
    if items.len() > 20 {
        return format!("E_BAD_ARGS reads 最多 20 项，当前 {} 项", items.len());
    }
    const ALLOWED: [&str; 9] = [
        "path", "start", "end", "ranges", "block", "max_line", "scan", "budget", "focus",
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
    let mut failed = 0usize;
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
                "E_BAD_ARGS reads[{index}] 未知字段 {}；可用 path/start/end/ranges/block/max_line/scan/budget/focus\n",
                unknown.join(",")
            ));
            continue;
        }
        let mut single = obj.clone();
        // 项内 budget 优先：大文件可以多要一点，小文件把额度让出来。
        if obj.get("budget").is_none() {
            single.insert("budget".to_string(), json!(per_item));
        }
        // 顶层 max_line / scan 对每一项都成立，传播而不是静默丢弃；
        // 项内显式值优先（允许单个超长行文件单独设 max_line=0）。
        for key in ["max_line", "scan"] {
            if single.get(key).is_none() {
                if let Some(value) = args.get(key) {
                    single.insert(key.to_string(), value.clone());
                }
            }
        }
        if force {
            single.insert("force".to_string(), json!(true));
        }
        let text = read(&Value::Object(single));
        // reads 顶层没有 path，单项递归读时尾注会写成「同 path」；
        // 多个文件同时截断时多条「同 path」无法对应，补上本项路径。
        let text = if text.contains("续读: fs_read 同 path") {
            let item_path = obj.get("path").and_then(Value::as_str).unwrap_or("");
            text.replacen(
                "续读: fs_read 同 path",
                &format!("续读: fs_read path={item_path}"),
                1,
            )
        } else {
            text
        };
        // 项级成败：单项输出的首行是 E_ 即失败；MCP 的 isError 对整体保守，
        // 头部这行让调用方能一眼看出还有多少内容可用。
        if text.starts_with("E_") {
            failed += 1;
        }
        if index > 0 && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&text);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    let total = items.len();
    let ok = total - failed;
    let mut summary = format!("ok reads {total} 项：{ok} 成功");
    if failed > 0 {
        summary.push_str(&format!("，{failed} 失败"));
    }
    summary.push('\n');
    summary.push_str(out.trim_end());
    summary
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
///
/// `data_file`（JSON/YAML/Markdown 等）没有函数边界：花括号是数据结构，
/// 按它扩展会把 focus 变成「读整个文件」，因此只给命中行 ±2 行。
fn focus_blocks(
    index: &render::LineIndex<'_>,
    pattern: &str,
    data_file: bool,
) -> Result<FocusBlocks, String> {
    let regex = regex::Regex::new(pattern).map_err(|e| {
        format!(
            "E_BAD_REGEX 正则非法: {e}；检查转义，或先用 fs_grep 确认形状；{}",
            crate::search::REGEX_DIALECT_HINT
        )
    })?;
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for (offset, line) in index.lines().iter().enumerate() {
        if regex.is_match(line) {
            let line_no = offset + 1;
            let block = if data_file {
                (line_no.saturating_sub(2).max(1), line_no + 2)
            } else {
                enclosing_block(index.lines(), line_no).unwrap_or((line_no, line_no))
            };
            blocks.push(block);
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
    // block 按 start 行探测块边界，ranges 直接给区间：对“读哪些行”回答不同。
    // 旧行为静默按 ranges 处理，模型会以为自己拿到了整个函数。
    if arg_bool(args, "block", false) && args.get("ranges").is_some() {
        return "E_BAD_ARGS block 与 ranges 互斥：block 会按 start 行向外展开成完整代码块，ranges 已显式指出区间；按需求二选一（续读 block 截断结果时，移除 block，按返回的 ranges 续读；重新传 block 会重新定位代码块）".into();
    }
    // tail：只读最后 N 行。区间由总行数决定，因此与任何“指定区间”的参数互斥。
    let tail = arg_usize(args, "tail", 0);
    if tail > 0
        && (args.get("start").is_some()
            || args.get("end").is_some()
            || args.get("ranges").is_some()
            || arg_bool(args, "block", false)
            || arg_bool(args, "summary", false)
            || args.get("focus").is_some()
            || args.get("paths").is_some())
    {
        return "E_BAD_ARGS tail 与 start/end/ranges/block/summary/focus/paths 互斥：tail 自己确定区间，只用于单文件".into();
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
    // 多文件按大小分配预算：小文件优先读满（再跑一轮更贵），
    // 剩余按大小给大文件。固定均分会让 5 行的小文件浪费一半额度，
    // 200 行的大文件只拿到 40 行。
    let (per_file_need, small_total, big_total) = if targets.len() > 1 {
        let need: Vec<usize> = targets
            .iter()
            .map(|t| {
                FsPath::parse(t)
                    .ok()
                    .and_then(|fp| io::meta(&fp).ok())
                    .map(|m| m.size as usize + 256)
                    .unwrap_or(256)
            })
            .collect();
        let small: usize = need.iter().filter(|n| **n <= 1024).sum();
        let big: usize = need.iter().filter(|n| **n > 1024).sum();
        (need, small, big)
    } else {
        (Vec::new(), 0, 0)
    };
    // 多文件时给公共前缀声明、文件间分隔和续读尾注留出余量；
    // 否则各项预算之和正好等于 budget，尾部提示会被外层硬切。
    let alloc_budget = if targets.len() > 1 {
        budget.saturating_sub(64 + 40 * targets.len())
    } else {
        budget
    };

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
                if tail > 0 {
                    out.push_str(&format!(
                        "E_TOO_LARGE {} {}，超大文件不支持 tail（定位末尾需要全扫）；请用 fs_grep 定位，或 scan=full 拿总行数后用 start/end\n",
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
                if window.encoding == "utf-8-lossy" {
                    out.push_str(
                        "… 警告：原文含无法解码的字节，正文里的 U+FFFD 是替换字符、不是原文；切勿据此写回，请先用 fs_file op=convert 转码\n",
                    );
                }
                // 已写入 out 的全部内容（含公共前缀声明、路径头、元信息）都要扣掉。
                let header_len = out.len();
                let want = if targets.len() > 1 {
                    let need = per_file_need.get(i).copied().unwrap_or(256);
                    let share = if small_total * 2 <= alloc_budget {
                        if need <= 1024 {
                            need
                        } else {
                            alloc_budget
                                .saturating_sub(small_total)
                                .saturating_mul(need)
                                / big_total.max(1)
                        }
                    } else {
                        // 小文件总量已吃掉一半以上预算：全部按需要比例分，保证总和 ≤ budget。
                        alloc_budget.saturating_mul(need) / (small_total + big_total).max(1)
                    };
                    share.max(256).saturating_sub(usize::from(i > 0))
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
                            "E_LINE_TOO_LONG 第 {} 行约 {} 字节，渲染剩余预算 {}B 放不下；请设 max_line=0 并把 budget 提到至少 {}",
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
                        let (first_cut, last_cut) = r
                            .cut_lines
                            .first()
                            .copied()
                            .map(|f| (f, r.cut_lines.last().copied().unwrap_or(f)))
                            .unwrap_or((window.first_line, rendered_last));
                        text.push_str(&format!(
                            "… 有长行被截断（行 {first_cut}-{last_cut}）；重读该行需 max_line=0 且 budget≈{}\n",
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
        // 自适应预算：小文件一次读完，省掉一次「预算截断 → 续读」的往返。
        // 多送几 KB 换掉一整轮上下文重发，划算；显式 budget 永远优先。
        if !budget_explicit
            && targets.len() == 1
            && budget == render::DEFAULT_BUDGET
            && doc.size <= render::DEFAULT_BUDGET as u64
        {
            // 行号前缀每行 4-6 字节，大文件能轻松吃掉 4096 的余量；
            // 按行数补足，否则「一次读完」仍会被截断成续读。
            let lines = doc.text.lines().count();
            budget = doc.size as usize + 4096 + lines * 6;
        }
        // 行索引只建一次：总行数、block 探测和 converge 多轮渲染共用，
        // 避免每次缩预算都把整个文件重新 split 一遍。
        let index = render::LineIndex::new(&doc.text);
        let total_lines = index.total_lines();
        // focus：命中行扩成完整代码块；与 block 类似，但范围由正则决定。
        let focus_info = match arg_str(args, "focus") {
            Some(pattern) => match focus_blocks(
                &index,
                pattern,
                crate::summary::is_data(crate::summary::language_of(&fp.display())),
            ) {
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
        // block=true 没找到花括号块时（Python 等缩进语言、或起始行不是块开头）
        // 必须显式说明已退回普通窗口读取，否则模型会以为拿到的是完整函数体。
        let mut block_fallback = false;
        let per_file_ranges = if tail > 0 {
            // tail：区间由总行数决定，读完才能算。
            let from = total_lines.saturating_sub(tail).saturating_add(1).max(1);
            vec![(from, total_lines.max(1))]
        } else if let Some((focus, _)) = &focus_info {
            focus.blocks.clone()
        } else if arg_bool(args, "block", false) && args.get("ranges").is_none() {
            match enclosing_block(index.lines(), start) {
                Some((from, to)) => vec![(from, to)],
                None => {
                    block_fallback = true;
                    ranges.clone()
                }
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
        if doc.encoding == "utf-8-lossy" {
            header.push_str(
                "… 警告：原文含无法解码的字节，正文里的 U+FFFD 是替换字符、不是原文；切勿据此 fs_write/fs_edit 写回，请先用 fs_file op=convert 转码或改按二进制处理\n",
            );
        }
        if let Some((focus, pattern)) = &focus_info {
            if focus.truncated {
                header.push_str(&format!(
                    "… focus={pattern} 命中 {} 处，仅显示前 {MAX_FOCUS_BLOCKS} 处；用 fs_grep 缩小范围\n",
                    focus.total
                ));
            } else {
                header.push_str(&format!("… focus={pattern} 命中 {} 处\n", focus.total));
            }
        }
        if block_fallback {
            let (from, to) = per_file_ranges.first().copied().unwrap_or((start, start));
            header.push_str(&format!(
                "… block=true 未定位到花括号块（缩进语言不支持）；已按 {from}-{to} 行读取，需要精确边界请用 fs_grep 定位后传 ranges\n"
            ));
        }

        let want = if targets.len() > 1 {
            let need = per_file_need.get(i).copied().unwrap_or(256);
            let share = if small_total * 2 <= alloc_budget {
                if need <= 1024 {
                    need
                } else {
                    alloc_budget
                        .saturating_sub(small_total)
                        .saturating_mul(need)
                        / big_total.max(1)
                }
            } else {
                // 小文件总量已吃掉一半以上预算：全部按需要比例分，保证总和 ≤ budget。
                alloc_budget.saturating_mul(need) / (small_total + big_total).max(1)
            };
            share.max(256).saturating_sub(usize::from(i > 0))
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
                // body_budget 被收敛到 0：真正吃掉预算的是 header（路径/编码/hash）
                // 加续读尾注，不是“第 N 行太长”。报行太长会把模型引向错误的调参方向。
                if body_budget == 0 {
                    return format!(
                        "E_BUDGET budget={want} 连文件头（约 {}B）和续读尾注都放不下；请把 budget 提到至少 {}",
                        header.len(),
                        header.len() + 240
                    );
                }
                let line_no = r.continuation[0].0;
                let need = line_need_bytes(&doc.text, line_no);
                return format!(
                    "E_LINE_TOO_LONG 第 {} 行约 {} 字节，渲染剩余预算 {}B 放不下；请设 max_line=0 并把 budget 提到至少 {}",
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
                // 给可重放的完整命令：模型不必再自己定位是哪一行、也不必猜参数。
                let need = r.max_cut_line_bytes;
                let (first_cut, last_cut) = r
                    .cut_lines
                    .first()
                    .copied()
                    .map(|f| (f, r.cut_lines.last().copied().unwrap_or(f)))
                    .unwrap_or((r.first_line.max(1), r.last_line));
                target.push_str(&format!(
                    "… 有长行被截断（行 {first_cut}-{last_cut}）；最长截断行约 {} 字节（单行成本）；重读该行: fs_read path={} start={first_cut} end={last_cut} max_line=0 budget≈{}\n",
                    need,
                    fp.display(),
                    need + 200
                ));
            }
            if !r.continuation.is_empty() {
                // 多文件时必须带路径：一串「同 path」无法对应到具体文件。
                if targets.len() > 1 {
                    target.push_str(&format!(
                        "… {}-{}/{} 行 | 续读: fs_read path={} ",
                        r.first_line.max(1),
                        r.last_line,
                        r.total_lines,
                        fp.display()
                    ));
                } else {
                    target.push_str(&format!(
                        "… {}-{}/{} 行 | 续读: fs_read 同 path ",
                        r.first_line.max(1),
                        r.last_line,
                        r.total_lines,
                    ));
                }
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
    fn reads续读尾注带各自路径() {
        // reads 模式顶层没有 path，每项递归读时若沿用「同 path」，
        // 多条一模一样的尾注会让模型无法把 start 对应回文件。
        let dir = 临时目录("hint-reads");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        let body: String = (1..=100).map(|n| format!("line{n}\n")).collect();
        fs::write(&a, &body).unwrap();
        fs::write(&b, &body).unwrap();
        let pa = a.to_string_lossy().replace('\\', "/");
        let pb = b.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_read",
            &json!({
                "reads": [
                    {"path": pa, "start": 1, "end": 100},
                    {"path": pb, "start": 1, "end": 100}
                ],
                "budget": 900,
                "force": true
            }),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(
            !out.contains("续读: fs_read 同 path"),
            "reads 模式不该出现无法对应的「同 path」: {out}"
        );
        assert!(
            out.matches("续读: fs_read path=").count() >= 2,
            "每项续读都应带自己的路径: {out}"
        );
        assert!(out.contains(&pa), "应含 a 的路径: {out}");
        assert!(out.contains(&pb), "应含 b 的路径: {out}");
    }

    #[test]
    fn 长行截断尾注给可重放的重读命令() {
        // 只说「用 max_line=0」仍要模型自己找被截的是哪一行，等于多一轮。
        // 尾注应直接给可复制的完整命令。
        let content = format!("head\n{}\ntail\n", "y".repeat(600));
        let path = 临时文本文件(&content);
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = read(&json!({"path": posix, "max_line": 80}));
        let _ = fs::remove_file(path);
        assert!(
            out.contains("重读该行: fs_read path="),
            "应给可重放命令: {out}"
        );
        assert!(out.contains("start=2"), "应指出被截行号: {out}");
        assert!(out.contains("max_line=0"), "应给出关闭截断的参数: {out}");
    }

    #[test]
    fn stale错误先要求核对内容再给重试参数() {
        // P3：外部修改可能已改掉同一段文本，先劝「改传新 hash 重试」会把外部改动覆盖。
        // 恢复顺序必须是：fs_read 核对 → 内容仍适用才带新 hash 重试。
        let dir = 临时目录("stale-order");
        let path = dir.join("a.txt");
        fs::write(&path, "alpha\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_write",
            &json!({"path": posix, "content": "beta\n", "expect_hash": "0000000000000000"}),
        );
        let body = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert!(out.starts_with("E_STALE"), "{out}");
        let verify = out.find("fs_read").expect("应要求先核对正文");
        let retry = out.find("expect_hash").expect("应给重试参数");
        assert!(
            verify < retry,
            "必须先核对内容再给重试参数（当前顺序颠倒）: {out}"
        );
        assert!(out.contains("未写入"), "应声明本次未写入: {out}");
        assert_eq!(body, "alpha\n", "拒绝时磁盘不应变化");
    }

    #[test]
    fn stale错误给出可直接复制的重试指令() {
        // 并发冲突时若只报「文件已变化」，模型会整轮 fs_read 重来；
        // 其实 old 仍匹配时带新 hash 原样重试即可，错误应把这条路写清楚。
        let dir = 临时目录("stale-hint");
        let path = dir.join("a.txt");
        fs::write(&path, "alpha\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_write",
            &json!({"path": posix, "content": "beta\n", "expect_hash": "0000000000000000"}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("E_STALE"), "{out}");
        assert!(
            out.contains("expect_hash="),
            "应给可直接复制的重试参数: {out}"
        );
        assert!(out.contains("fs_read"), "应给出重读分支: {out}");
    }

    #[test]
    fn 非生成器维护的文案不报错工具数() {
        // P0 F-3：生成器只同步 README/工具参考/使用指南/package.json，
        // 注释与 CLI 帮助里的「8 个工具」无人负责，v0.9.0 加 fs_inspect 后就漂了。
        // 这里把其余手写位置钉住：写数量就必须等于实际工具数。
        let expected = tool_defs().len().to_string();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = ["src/cli.rs", "dsh/index.js", "cordis.patch.yml"];
        let pattern = regex::Regex::new(r"(\d+) 个(?: fs_\*)?工具").unwrap();
        let mut found = 0usize;
        for file in files {
            let path = root.join(file);
            let text = fs::read_to_string(&path).unwrap();
            for caps in pattern.captures_iter(&text) {
                let number = &caps[1];
                found += 1;
                assert_eq!(
                    number, expected,
                    "{file} 里的工具数已漂移（实际 {}）: {}",
                    expected, &caps[0]
                );
            }
        }
        // 一个都不写也合法（本轮已改为不提数量）；写了就必须对。
        let _ = found;
    }

    #[test]
    fn fs_read描述把常用路径放在前面() {
        // P2：模型没时间读完描述，常用四种用法必须在开头就出现，
        // 且顺序就是建议的选择顺序（单文件 → 批量 → 按模式 → 日志末尾）。
        // 常用路径前移不能删掉互斥与限制：它们是防止盲试的保护性约束。
        let defs = tool_defs();
        let read = defs.iter().find(|d| d["name"] == "fs_read").unwrap();
        let desc = read["description"].as_str().unwrap();
        let head = &desc[..desc.find("模式表").expect("缺少模式表")];
        let at = |needle: &str| {
            head.find(needle)
                .unwrap_or_else(|| panic!("开头缺 {needle}: {head}"))
        };
        let (path, reads, focus, tail) = (at("`path`"), at("`reads="), at("focus="), at("tail=N"));
        assert!(
            path < reads && reads < focus && focus < tail,
            "常用路径顺序不对: {head}"
        );
        for needle in ["互斥", "仅单文件", "32KB", "scan=window"] {
            assert!(desc.contains(needle), "描述漏了 {needle}: {desc}");
        }
    }

    #[test]
    fn 描述带上无法从参数名推断的约束() {
        // 这三条都是“不说就猜不到”的：diff 自查能省一轮回读；
        // count 的精确/本页可见口径直接决定模型敢不敢拿它当总数。
        let defs = tool_defs();
        let desc = |name: &str| {
            defs.iter()
                .find(|d| d["name"] == name)
                .and_then(|d| d["description"].as_str())
                .unwrap_or("")
        };
        assert!(desc("fs_edit").contains("diff=true"), "{}", desc("fs_edit"));
        assert!(
            desc("fs_grep").contains("本页可见"),
            "count 口径缺失: {}",
            desc("fs_grep")
        );
        assert!(
            desc("fs_grep").contains("完整统计"),
            "count 默认口径缺失: {}",
            desc("fs_grep")
        );
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
        // 必填映射要出现在描述里，而不是只活在文档表格里。
        assert!(desc("fs_file").contains("src") && desc("fs_file").contains("confirm"));
        assert!(desc("fs_exec").contains("program") && desc("fs_exec").contains("job"));
        assert!(desc("fs_stat").contains("path") && desc("fs_stat").contains("paths"));
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
    fn 乱序区间合并后不丢行() {
        // 模型常从 grep 结果里拼区间，顺序不保证；只与已输出区间比较
        // 会把 [1,6] 吞进先到的 [10,14]，静默少读一整段。
        assert_eq!(
            merge_ranges(vec![(10, 14), (1, 6), (12, 20)]),
            vec![(1, 6), (10, 20)]
        );
    }

    #[test]
    fn stat多文件部分失败时首行给成败计数() {
        let path = 临时文本文件("x\n");
        let posix = path.to_string_lossy().replace('\\', "/");
        let missing = format!("{posix}.missing");
        let out = stat(&json!({"paths": [posix, missing]}));
        let _ = fs::remove_file(path);
        assert!(out.starts_with("ok stat 2 项：1 成功，1 失败"), "{out}");
        assert!(out.contains("E_NOT_FOUND"), "{out}");
    }

    #[test]
    fn 预算小于文件头时报告头开销() {
        let path = 临时文本文件("line1\nline2\n");
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let out = read(&json!({"path": posix_path, "budget": 100, "force": true}));
        let _ = fs::remove_file(path);
        assert!(out.starts_with("E_BUDGET"), "应报告预算而非行太长: {out}");
        assert!(out.contains("文件头"), "{out}");
    }

    #[test]
    fn summary预算紧张时尾注不被截成半句() {
        let path = std::env::current_dir().unwrap().join(format!(
            "fsops-summary-budget-{}-{}.rs",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // 函数体要足够长，否则会触发“ summary 不比全文更省”的退化分支。
        let body: String = (0..12)
            .map(|i| format!("fn f{i}() {{\n    let a = 1;\n    let b = 2;\n}}\n"))
            .collect();
        fs::write(&path, body).unwrap();
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let out = read(&json!({
            "path": posix_path,
            "summary": true,
            "budget": 260,
            "force": true
        }));
        let _ = fs::remove_file(path);
        assert!(out.contains("个符号"), "应走摘要路径: {out}");
        assert!(
            out.contains("或调大 budget") || out.contains("E_BUDGET"),
            "尾注被硬切: {out:?}"
        );
        assert!(
            out.contains("调大 budget") || out.contains("E_BUDGET"),
            "预算不足必须给出可执行提示: {out}"
        );
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
        assert!(
            out.contains("Set-ItemProperty"),
            "应给可直接照做的 PowerShell 命令: {out}"
        );
        assert!(out.contains("IsReadOnly"), "{out}");
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
    fn 行范围限定只改同函数内的两处相同字面量() {
        // T05：同一函数里两处相同字面量要一起改，且不能波及同文件的其他函数。
        let dir = 临时目录("edit-dup-literal");
        let path = dir.join("a.rs");
        fs::write(
            &path,
            "fn a() {\n    let x = limit;\n    let y = limit;\n}\n\nfn b() {\n    let z = limit;\n}\n",
        )
        .unwrap();
        let output = edit_tool(&json!({
            "path": path.to_string_lossy(),
            "edits": [{"old": "limit", "new": "cap", "all": true, "start": 1, "end": 4}]
        }));
        let body = fs::read_to_string(&path).unwrap();
        fs::remove_dir_all(dir).unwrap();

        assert!(output.starts_with("ok 2 处修改"), "{output}");
        assert_eq!(
            body,
            "fn a() {\n    let x = cap;\n    let y = cap;\n}\n\nfn b() {\n    let z = limit;\n}\n"
        );
    }

    #[test]
    fn 锁短暂占用时等待后写入成功() {
        // T10 前半：另一线程短时间持有写锁，工具层有限等待后继续写入。
        // 锁必须由另一条线程获取并在同一线程释放（Windows 命名互斥量按线程归属）。
        let dir = 临时目录("lock-brief");
        let path = dir.join("a.txt");
        fs::write(&path, "alpha\n").unwrap();
        let held_fp = FsPath::parse(path.to_str().unwrap()).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _lock = io::lock_path(&held_fp, 1000).expect("持有写锁");
            ready_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(250));
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("持锁线程未就绪");

        let output = edit_tool(&json!({
            "path": path.to_string_lossy(),
            "edits": [{"old": "alpha", "new": "beta"}]
        }));
        holder.join().unwrap();
        let body = fs::read_to_string(&path).unwrap();
        fs::remove_dir_all(dir).unwrap();

        assert!(
            output.starts_with("ok 1 处修改"),
            "等待后应写入成功: {output}"
        );
        assert_eq!(body, "beta\n");
    }

    #[test]
    fn 锁持续占用时报出等待时长且不写入() {
        // T10 后半：锁一直被占时，工具层等满 3000ms 后如实报告，磁盘不变；
        // 锁释放后原参数直接重试能成功（有限等待 + 可重试，不是盲重试）。
        let dir = 临时目录("lock-held");
        let path = dir.join("a.txt");
        fs::write(&path, "alpha\n").unwrap();
        let held_fp = FsPath::parse(path.to_str().unwrap()).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _lock = io::lock_path(&held_fp, 1000).expect("持有写锁");
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(60));
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("持锁线程未就绪");

        let started = std::time::Instant::now();
        let output = edit_tool(&json!({
            "path": path.to_string_lossy(),
            "edits": [{"old": "alpha", "new": "beta"}]
        }));
        let waited = started.elapsed();
        let body_while_locked = fs::read_to_string(&path).unwrap();

        assert!(output.starts_with("E_LOCKED"), "{output}");
        assert!(output.contains("3000ms"), "应说明已等多久: {output}");
        assert!(
            output.contains("本次未写入任何内容"),
            "应声明未写入: {output}"
        );
        assert!(
            waited >= std::time::Duration::from_millis(2900),
            "应真的等满锁超时，实际 {waited:?}"
        );
        assert_eq!(body_while_locked, "alpha\n", "锁冲突时磁盘被改了");

        drop(release_tx);
        holder.join().unwrap();
        let retry = edit_tool(&json!({
            "path": path.to_string_lossy(),
            "edits": [{"old": "alpha", "new": "beta"}]
        }));
        let body = fs::read_to_string(&path).unwrap();
        fs::remove_dir_all(dir).unwrap();

        assert!(retry.starts_with("ok 1 处修改"), "释放后重试失败: {retry}");
        assert_eq!(body, "beta\n");
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
    fn edit字段级参数与edits混用时报错且不猜最接近字段() {
        // 单条修改时顶层写法已被接受（见「顶层单条修改简写被接受」）；
        // 但一旦给了 edits，顶层字段就无处安放，必须报错而不是静默忽略。
        // 通用「最接近合法字段」会把它猜成 path，反而把模型引到更错的方向。
        for key in [
            "all",
            "start",
            "end",
            "old",
            "new",
            "pattern",
            "replacement",
        ] {
            let mut args = json!({
                "path": "a.txt",
                "edits": [{"old": "a", "new": "b"}]
            });
            args[key] = json!(true);
            let output = call("fs_edit", &args);
            assert!(output.starts_with("E_BAD_ARGS"), "{key}: {output}");
            assert!(
                output.contains("edits"),
                "{key} 应指引放进 edits[]: {output}"
            );
            assert!(
                !output.contains("最接近的合法字段"),
                "{key} 不该猜最接近字段: {output}"
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
    fn 顶层单条修改简写被接受() {
        // 实测三次独立任务 3/3 都把 old/new 或 pattern/replacement 放顶层，
        // 每次都因 E_BAD_ARGS 白费一轮。单条修改时应直接接受这种自然写法。
        let dir = 临时目录("edit-shorthand");
        let f = dir.join("a.ts");
        fs::write(&f, "export function log() {\n  console.log(1)\n}\n").unwrap();
        let path = f.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_edit",
            &json!({"path": path, "pattern": "console\\.log", "replacement": "console.error"}),
        );
        assert!(out.starts_with("ok"), "顶层 pattern 应被接受: {out}");
        assert!(
            fs::read_to_string(&f).unwrap().contains("console.error"),
            "简写应真的落盘"
        );

        let out = call(
            "fs_edit",
            &json!({"path": path, "old": "console.error(1)", "new": "console.warn(1)"}),
        );
        assert!(out.starts_with("ok"), "顶层 old/new 应被接受: {out}");

        // 给了 edits 时顶层字段仍必须嵌进去，不能被静默忽略
        let mixed = call(
            "fs_edit",
            &json!({"path": path, "old": "x", "new": "y", "edits": [{"old": "warn", "new": "error"}]}),
        );
        assert!(mixed.starts_with("E_BAD_ARGS"), "混用应报错: {mixed}");
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn 半行old截断在词中间时给警告() {
        // 实测踩坑：文件是 `# mini-task-service`，从片段抄了 `# mini-task-servic`，
        // 替换后留下 `# 新标题e`——工具返回 ok，内容却坏了，要等下一轮 fs_read 才发现。
        let dir = 临时目录("edit-half-word");
        let f = dir.join("README.md");
        fs::write(&f, "# mini-task-service\n").unwrap();
        let out = call(
            "fs_edit",
            &json!({"path": f.to_string_lossy().replace('\\', "/"),
                    "edits": [{"old": "# mini-task-servic", "new": "# 新标题"}]}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("ok"), "仍应成功（只是提示）: {out}");
        assert!(out.contains("截断在词中间"), "应提示半行 old: {out}");
        assert!(out.contains("mini-task-service"), "应给出原文整行: {out}");
    }

    #[test]
    fn 整词与行内片段替换不报警告() {
        // 正常编辑不该被提示干扰，否则警告会被模型当噪声忽略。
        let dir = 临时目录("edit-no-half-word");
        let f = dir.join("a.txt");
        fs::write(&f, "let limit_0 = 100;\nconst x = 1 + 2;\n").unwrap();
        let path = f.to_string_lossy().replace('\\', "/");
        let whole = call(
            "fs_edit",
            &json!({"path": path, "edits": [{"old": "limit_0", "new": "cap_0"}]}),
        );
        assert!(!whole.contains("截断在词中间"), "整词替换不该警告: {whole}");
        let inline = call(
            "fs_edit",
            &json!({"path": path, "edits": [{"old": "1 + 2", "new": "3"}]}),
        );
        assert!(
            !inline.contains("截断在词中间"),
            "行内片段不该警告: {inline}"
        );
        let _ = fs::remove_dir_all(&dir);
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
        assert!(out.contains("@ 1,1:11,1:19"), "同行多处给列号: {out}");

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
    fn 长行截断时提示单行读法() {
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
        assert!(
            out.contains("重读该行"),
            "成本是单行字节数，必须说清单行而不是全文: {out}"
        );
        assert!(!out.contains("读全文"), "不能把单行成本说成读全文: {out}");
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
        for def in defs.iter() {
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
    fn block未定位到花括号块时提示已回退() {
        // Python 等缩进语言没有花括号：block=true 必须明说“没定位到”，
        // 否则模型会以为拿到的是函数体，实际是 start 到文件末尾。
        let path = std::env::temp_dir().join(format!(
            "fsops-block-python-{}-{}.py",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, "def alpha():\n    x = 1\n    return x\n").unwrap();
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let out = read(&json!({
            "path": posix_path,
            "start": 1,
            "block": true,
            "budget": 4096,
            "force": true
        }));
        let _ = fs::remove_file(path);
        assert!(out.contains("未定位到花括号块"), "缺少回退提示: {out}");
        assert!(out.contains("1-3"), "应说明实际读取区间: {out}");
    }

    #[test]
    fn focus在数据文件上只给命中行附近() {
        // JSON/YAML 的花括号是数据结构，不是函数边界；
        // 按花括号扩展会把 focus 变成「读整个文件」。
        let path = 临时文本文件(
            "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3,\n  \"d\": 4,\n  \"target\": 5,\n  \"e\": 6,\n  \"f\": 7,\n  \"g\": 8\n}\n",
        );
        let posix_path = path.to_string_lossy().replace('\\', "/");
        let out = read(&json!({
            "path": posix_path,
            "focus": "target",
            "budget": 2000,
            "force": true
        }));
        let _ = fs::remove_file(path);
        assert!(out.contains("6|"), "应包含命中行: {out}");
        assert!(!out.contains("1|{"), "数据文件不应扩展成整个文件: {out}");
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
        // matches 模式默认 limit=200：150 条能一次装完（旧默认 100 要多翻一页，
        // 那一页要把整个上下文重发一次）。超过默认值仍应给 next_offset。
        let matches = grep_tool(&json!({
            "path": root.display(),
            "pattern": "needle",
            "output": "matches"
        }));
        let overflow = grep_tool(&json!({
            "path": root.display(),
            "pattern": "needle",
            "output": "matches",
            "limit": 20
        }));
        let _ = fs::remove_dir_all(&dir);

        assert!(
            out.contains("many.txt:150"),
            "默认 count 应给完整数字: {out}"
        );
        assert!(!out.contains("本页可见"), "未截断不应标注: {out}");
        assert!(capped.contains("本页可见"), "显式 limit 仍应标注: {capped}");
        assert!(
            !matches.contains("next_offset") && matches.contains("150 处命中"),
            "matches 默认 200 应一次装完 150 条: {matches}"
        );
        assert!(
            overflow.contains("next_offset"),
            "超过 limit 时仍给 next_offset: {overflow}"
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
    fn 批量写入一次建多个文件() {
        // 建 10 个文件不该 10 轮：fs_edit 早就有 files，fs_write 没有是明显不对称。
        let dir = 临时目录("write-batch");
        let files: Vec<Value> = (0..10)
            .map(|i| {
                json!({
                    "path": dir.join(format!("m{i}.js")).to_string_lossy().replace('\\', "/"),
                    "content": format!("// module {i}\n"),
                })
            })
            .collect();
        let out = call("fs_write", &json!({ "files": files }));
        assert!(out.starts_with("ok 10 个文件已写入"), "{out}");
        for i in 0..10 {
            assert!(dir.join(format!("m{i}.js")).exists(), "m{i}.js 未写入");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 批量写入预检失败时一个都不写() {
        // 与 fs_edit files 一致：全部预检通过才写入，不留“写了一半”。
        let dir = 临时目录("write-batch-preflight");
        let out = call(
            "fs_write",
            &json!({"files": [
                {"path": dir.join("a.txt").to_string_lossy().replace('\\', "/"), "content": "a\n"},
                {"path": dir.join("b.txt").to_string_lossy().replace('\\', "/"), "content": 42}
            ]}),
        );
        assert!(out.starts_with("E_BATCH"), "{out}");
        assert!(
            !dir.join("a.txt").exists(),
            "预检失败不该写出任何文件: {out}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 批量写入拒绝与单文件混用() {
        let dir = 临时目录("write-batch-mixed");
        let out = call(
            "fs_write",
            &json!({"path": dir.join("x.txt").to_string_lossy().replace('\\', "/"), "content": "a", "files": []}),
        );
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 批量写入dry_run不落盘() {
        let dir = 临时目录("write-batch-dry");
        let out = call(
            "fs_write",
            &json!({"dry_run": true, "files": [
                {"path": dir.join("a.txt").to_string_lossy().replace('\\', "/"), "content": "a\n"}
            ]}),
        );
        assert!(out.starts_with("ok dry_run"), "{out}");
        assert!(!dir.join("a.txt").exists(), "dry_run 不该落盘: {out}");
        let _ = fs::remove_dir_all(&dir);
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
        assert!(
            out.contains("fs_glob"),
            "写入目录也给列出内容的下一步: {out}"
        );
        assert!(out.contains("改为文件路径"), "{out}");
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
    fn 工具参考覆盖全部工具与错误码() {
        // 三处契约（MCP schema / 源码错误码 / 文档）手工维护必然漂移；
        // 把文档钉住：源码里出现的错误码必须能在工具参考里找到。
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let doc = fs::read_to_string(root.join("docs/工具参考.md")).unwrap();
        for def in tool_defs() {
            let name = def["name"].as_str().unwrap();
            assert!(
                doc.contains(&format!("## {name}")),
                "工具参考缺少 {name} 章节"
            );
        }
        let pattern = regex::Regex::new(r"E_[A-Z][A-Z0-9_]{2,}").unwrap();
        let mut codes: Vec<String> = Vec::new();
        for entry in fs::read_dir(root.join("src")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            for m in pattern.find_iter(&text) {
                // 排除 RULE_BODY / MAX_ARCHIVE_ENTRIES 这类标识符里嵌的 "E_XXX"：
                // 匹配前一个字符仍是标识符字符，说明是更长名字的一部分。
                let before = text[..m.start()].chars().next_back();
                if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                    continue;
                }
                let code = m.as_str().to_string();
                // 注释与文案里的占位示例，不是真实错误码。
                if code == "E_XXX" {
                    continue;
                }
                if !codes.contains(&code) {
                    codes.push(code);
                }
            }
        }
        let missing: Vec<String> = codes
            .into_iter()
            .filter(|code| !doc.contains(code.as_str()))
            .collect();
        assert!(missing.is_empty(), "源码错误码未入工具参考: {missing:?}");
    }

    #[test]
    fn 工具描述与实际能力一致() {
        let defs = tool_defs();
        let get = |name: &str| defs.iter().find(|d| d["name"] == name).unwrap();
        // 正则替换就是为省掉「抄原文」那一轮，description 里必须看得到。
        let edit_desc = get("fs_edit")["description"].as_str().unwrap();
        assert!(
            edit_desc.contains("正则替换"),
            "模型看不到正则替换会白多一轮 fs_read: {edit_desc}"
        );
        // 读取缓存默认关闭，别让模型以为重复读会被短路。
        let force = get("fs_read")["inputSchema"]["properties"]["force"]["description"]
            .as_str()
            .unwrap();
        assert!(
            !force.contains("默认情况下"),
            "force 描述停留在旧的默认行为: {force}"
        );
        assert!(force.contains("默认关闭"), "{force}");
        // reads 项支持 budget/focus，描述里列全才能少一轮试错。
        let reads_desc = get("fs_read")["inputSchema"]["properties"]["reads"]["description"]
            .as_str()
            .unwrap();
        assert!(
            reads_desc.contains("budget") && reads_desc.contains("focus"),
            "reads 项描述漏了可选字段: {reads_desc}"
        );
    }

    #[test]
    fn 连续空行的正文可直接当old编辑() {
        // 回归：默认折叠空行时，模型按 fs_read 输出拼 old 会缺行 → E_NO_MATCH。
        let dir = 临时目录("read-edit-blank");
        let path = dir.join("a.rs");
        let content = "fn a() {\n    one();\n}\n\n\npub fn b() {\n    two();\n}\n";
        fs::write(&path, content).unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = read(&json!({"path": posix}));
        assert!(
            out.contains("4|\n") && out.contains("5|\n") && out.contains("6|pub fn b"),
            "连续空行必须逐行带行号，否则模型拼出的 old 会缺行: {out}"
        );
        let edited = call(
            "fs_edit",
            &json!({
                "path": posix,
                "edits": [{"old": "}\n\n\npub fn b()", "new": "}\n\npub fn b()"}]
            }),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(
            !edited.starts_with("E_"),
            "按原文构造 old 应能直接命中: {edited}"
        );
    }

    #[test]
    fn 截断与空结果区分及续读参数可直接重放() {
        // P4 F-9：四种“不完整”不能混为一谈。这里钉住两个最容易混的：
        // ① count 未截断时不能标“本页可见”，截断时必须标；
        // ② 续读提示里的 offset 必须等于真实返回的下一个偏移，而不是猜一个值。
        let dir = 临时目录("p4-paging-contract");
        for i in 0..7 {
            fs::write(dir.join(format!("f{i}.txt")), "needle\n").unwrap();
        }
        let path = dir.to_string_lossy().replace('\\', "/");
        let exact = grep_tool(&json!({
            "pattern": "needle", "path": path, "output": "count"
        }));
        let limited = grep_tool(&json!({
            "pattern": "needle", "path": path, "output": "count", "limit": 2
        }));
        let paged = grep_tool(&json!({
            "pattern": "needle", "path": path, "output": "files", "limit": 2
        }));
        let _ = fs::remove_dir_all(&dir);

        assert!(exact.contains(":1"), "精确统计应给每文件数字: {exact}");
        assert!(
            !exact.contains("本页可见"),
            "未截断的 count 不应标本页可见: {exact}"
        );
        assert!(
            limited.contains("本页可见"),
            "显式 limit 截断的 count 必须标本页可见: {limited}"
        );
        let m = regex::Regex::new(r"next_offset=(\d+)")
            .unwrap()
            .captures(&paged)
            .expect("分页应给 next_offset");
        let next: usize = m[1].parse().unwrap();
        let shown = paged
            .lines()
            .filter(|l| l.ends_with(".txt") && !l.starts_with('…'))
            .count();
        assert_eq!(
            next, shown,
            "next_offset 必须等于本页实际条目数（续读直接重放）: {paged}"
        );
    }

    #[test]
    fn next_offset尾注给出可重放命令() {
        // 只给一个裸数字，模型还得回查 schema 才知道把它塞进哪个参数。
        let dir = 临时目录("hint-next");
        for i in 0..12 {
            fs::write(dir.join(format!("f{i:02}.txt")), "needle\n").unwrap();
        }
        let path = dir.to_string_lossy().replace('\\', "/");
        let glob_out = glob_tool(&json!({
            "path": path, "pattern": "*.txt", "limit": 2
        }));
        let grep_out = grep_tool(&json!({
            "pattern": "needle", "path": path, "limit": 1
        }));
        let _ = fs::remove_dir_all(&dir);
        assert!(
            glob_out.contains("续读: fs_glob offset="),
            "glob 尾注应给可重放命令: {glob_out}"
        );
        assert!(
            grep_out.contains("续读: fs_grep offset="),
            "grep 尾注应给可重放命令: {grep_out}"
        );
        assert!(
            grep_out.contains("output="),
            "grep 尾注应固定 output 单位: {grep_out}"
        );
    }

    #[test]
    fn 批量部分失败错误列出已写路径() {
        let msg = batch_partial_error(
            &["a.txt".to_string(), "b.txt".to_string()],
            "c.txt",
            "E_OPEN",
            3,
        );
        assert!(msg.starts_with("E_BATCH_PARTIAL"), "{msg}");
        assert!(
            msg.contains("a.txt") && msg.contains("b.txt"),
            "应列已写路径: {msg}"
        );
        assert!(msg.contains("仅对剩余文件重发"), "应给下一步: {msg}");
    }

    #[test]
    fn docx错误警告格式丢失并给重建命令() {
        let dir = 临时目录("docx-hint");
        let path = dir.join("a.docx");
        fs::write(&path, "x").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_edit",
            &json!({"files": [{"path": posix, "edits": [{"old": "x", "new": "y"}]}]}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("E_DOCX"), "{out}");
        assert!(
            out.contains("格式") && out.contains("图片"),
            "必须警告会丢光格式: {out}"
        );
        assert!(
            out.contains("fs_write") && out.contains("content="),
            "应给重建命令形状: {out}"
        );
    }

    #[test]
    fn schema声明的约束在入口就被执行() {
        // 宿主把 schema 当提示文本时，违反 required/enum/minimum/maxItems 的调用
        // 会漏到深层才报错，甚至静默退回默认值；入口一次给可复制的修法。
        let out = call("fs_grep", &json!({"pattern": "x", "output": "snippet"}));
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
        assert!(
            out.contains("output") && out.contains("snippets"),
            "应列出合法枚举值: {out}"
        );

        let out = call("fs_read", &json!({"path": "a", "start": 0}));
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
        assert!(
            out.contains("start") && out.contains("1"),
            "应给出下限: {out}"
        );

        let out = call("fs_grep", &json!({"limit": 1}));
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
        assert!(out.contains("pattern"), "应指出必填项: {out}");

        let many: Vec<String> = (0..21).map(|i| format!("f{i}.txt")).collect();
        let out = call("fs_read", &json!({"paths": many}));
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
        assert!(
            out.contains("paths") && out.contains("20"),
            "应给出上限: {out}"
        );
    }

    #[test]
    fn lossy解码时警告不可据此写回() {
        // GBK/GB18030 都解不出时回落 utf-8-lossy，正文里的 U+FFFD 不是原文；
        // 模型若据此 fs_write 写回，磁盘字节会被永久替换。
        let dir = 临时目录("lossy-hint");
        let path = dir.join("a.bin");
        fs::write(&path, b"abc\xff\xfe\x80def\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = read(&json!({"path": posix}));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("utf-8-lossy"), "{out}");
        assert!(
            out.contains("警告") && out.contains("U+FFFD"),
            "必须警告正文不可信: {out}"
        );
        assert!(out.contains("切勿"), "应给出写回禁令: {out}");
    }

    #[test]
    fn grep_max_line_0一次拿到完整长行() {
        // 硬裁 200 字符时模型看到半行，还得再 fs_read 一轮确认。
        let dir = 临时目录("grep-max-line");
        let long = format!("prefix{}suffix\n", "x".repeat(500));
        fs::write(dir.join("a.txt"), &long).unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");
        let out = grep_tool(&json!({
            "pattern": "prefix", "path": path, "max_line": 0
        }));
        let _ = fs::remove_dir_all(&dir);
        assert!(
            out.contains("suffix") && out.contains(&"x".repeat(500)),
            "max_line=0 应返回完整命中行，省掉再 fs_read 一轮: {out}"
        );
    }

    #[test]
    fn fs_grep_schema暴露max_line() {
        let defs = tool_defs();
        let grep = defs.iter().find(|d| d["name"] == "fs_grep").unwrap();
        let props = &grep["inputSchema"]["properties"];
        assert!(props["max_line"].is_object(), "grep 应暴露 max_line");
        assert!(
            props["max_line"]["description"]
                .as_str()
                .unwrap()
                .contains("0"),
            "应说明 0 表示不截断"
        );
    }

    #[test]
    fn stat路径含空格时用引号可切分() {
        // 空格既是路径又是字段分隔符，模型无法切出完整路径；引号给出稳定边界。
        let dir = 临时目录("stat-space");
        let path = dir.join("my file.txt");
        fs::write(&path, "x").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call("fs_stat", &json!({"path": posix, "detail": "meta"}));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with('\"'), "路径应被引号包裹: {out}");
        let close = out[1..].find('\"').map(|i| i + 1).unwrap_or(0);
        assert!(close > 1, "应有闭合引号: {out}");
        assert!(
            out[1..close].contains("my file.txt"),
            "含空格路径应完整落在引号内: {out}"
        );
        assert!(
            out[close + 1..].trim_start().starts_with("file "),
            "引号后应紧跟字段，模型才能稳定切分: {out}"
        );
    }

    #[test]
    fn 混合换行归一时沿用原文件主风格() {
        // 一律归一为 LF 会把 CRLF 仓库整文件改成 LF，产生巨大 diff。
        let dir = 临时目录("eol-dominant");
        let path = dir.join("a.txt");
        fs::write(&path, b"alpha\r\nbeta\ngamma\r\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_edit",
            &json!({
                "path": posix,
                "edits": [{"old": "beta", "new": "changed"}],
                "normalize_eol": true
            }),
        );
        let raw = fs::read(&path).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("ok"), "{out}");
        assert_eq!(
            raw, b"alpha\r\nchanged\r\ngamma\r\n",
            "CRLF 为主的文件不应被改成 LF: {out}"
        );
    }

    #[test]
    fn 陈旧与混合换行同时成立时先报陈旧并一次说清() {
        // 外部追加是最常见的场景：它同时造成「hash 变了」和「换行混合了」。
        // 旧实现先报 E_MIXED_EOL，模型归一完再来一轮才被告知 hash 也变了。
        let dir = 临时目录("eol-and-stale");
        let path = dir.join("a.txt");
        fs::write(&path, b"alpha\nbeta\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let read = call("fs_read", &json!({"path": posix}));
        assert!(read.contains("hash="), "{read}");

        fs::write(&path, b"alpha\r\nbeta\r\nadded\n").unwrap();
        let out = call(
            "fs_edit",
            &json!({
                "path": posix,
                "expect_hash": "0000000000000000",
                "edits": [{"old": "beta", "new": "BETA"}]
            }),
        );
        let raw = fs::read(&path).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("E_STALE"), "陈旧应优先: {out}");
        assert!(out.contains("混合换行"), "应同时告知换行事实: {out}");
        assert!(raw.starts_with(b"alpha\r\n"), "被拒绝时磁盘不应变化: {out}");
    }

    #[test]
    fn 全量覆盖不受源文件混合换行影响() {
        // 覆盖写是全量替换，源文件的混合换行已无意义；只要待写内容风格稳定，
        // 就不该再要求 normalize_eol（旧行为会多拦一轮）。
        let dir = 临时目录("write-over-mixed");
        let path = dir.join("a.txt");
        fs::write(&path, b"old\r\nmixed\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_write",
            &json!({"path": posix, "content": "new\nstable\n"}),
        );
        let raw = fs::read(&path).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("ok"), "稳定的新内容不应被源文件拦截: {out}");
        assert_eq!(raw, b"new\nstable\n", "应按内容自身的稳定风格写: {out}");
    }

    #[test]
    fn fs_read描述包含模式表() {
        // 模式互斥只能靠运行时错误教学的话，模型要付一轮试错；
        // description 里给一张组合表，让它在发出调用前就能拼出合法参数。
        // 注意：path 与 paths 实为并集（可同时给），写成「四选一」会误导模型多一轮。
        let defs = tool_defs();
        let read = defs.iter().find(|d| d["name"] == "fs_read").unwrap();
        let desc = read["description"].as_str().unwrap();
        assert!(desc.contains("模式表"), "缺少模式表: {desc}");
        assert!(desc.contains("并集"), "应说明 path+paths 是并集: {desc}");
        assert!(desc.contains("summary"), "缺少 summary 的限制说明: {desc}");
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
        // `edits` 与 `files[].edits` 分开手写会漂移；嵌套处若只剩 {"type":"object"}，
        // 模型只能猜字段名。但嵌套处复制一份完整描述要多花约 700 字符 schema，
        // 而 schema 每轮都进上下文——所以只要求「字段名 + 类型 + enum」一致，
        // 描述可以在嵌套处省略（顶层已经写清了）。
        let defs = tool_defs();
        let edit = defs
            .iter()
            .find(|def| def["name"] == "fs_edit")
            .expect("缺少 fs_edit");
        let props = &edit["inputSchema"]["properties"];
        let single = &props["edits"]["items"];
        let nested = &props["files"]["items"]["properties"]["edits"]["items"];
        let shape = |node: &Value| -> Value {
            let mut out = serde_json::Map::new();
            for (key, spec) in node["properties"].as_object().expect("必须有 properties") {
                let mut item = serde_json::Map::new();
                for field in ["type", "enum", "minimum"] {
                    if let Some(value) = spec.get(field) {
                        item.insert(field.to_string(), value.clone());
                    }
                }
                out.insert(key.clone(), Value::Object(item));
            }
            Value::Object(out)
        };
        assert_eq!(
            shape(single),
            shape(nested),
            "两处 items 的字段与类型必须一致"
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
        assert!(!defs.is_empty(), "工具表不能为空");
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
    fn expand_同一块内多命中只输出一次() {
        // grep 常在一个函数里命中好几行；展开时重复输出同一个块会白白撑爆上下文。
        let dir = 临时目录("grep-expand-dedup");
        fs::write(
            dir.join("a.rs"),
            "fn alpha() {\n    // TODO one\n    // TODO two\n}\n",
        )
        .unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");
        let out = grep_tool(
            &json!({"pattern": "TODO", "path": path, "output": "snippets", "context": 0, "expand": "block"}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            out.matches("fn alpha()").count(),
            1,
            "同一块只应输出一次: {out}"
        );
        assert!(
            out.contains("TODO one") && out.contains("TODO two"),
            "{out}"
        );
    }

    #[test]
    fn expand_非花括号语言退回命中行() {
        // Python 等缩进语言没有花括号：找不到块边界时只给命中行，不能假装展开。
        let dir = 临时目录("grep-expand-fallback");
        fs::write(dir.join("a.py"), "def alpha():\n    x = 1  # TODO\n").unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");
        let out = grep_tool(
            &json!({"pattern": "TODO", "path": path, "output": "snippets", "context": 0, "expand": "block"}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("TODO"), "仍要给出命中: {out}");
    }
    #[test]
    fn inspect_一次给出规模与命中分布() {
        // 接手陌生目录时模型固定走「glob 看结构 → stat 查大小 → grep 找目标」三步；
        // inspect 一次给全，每省一轮就省一次整个上下文的重发。
        let dir = 临时目录("inspect-basic");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), "fn alpha() {}\n// TODO one\n").unwrap();
        fs::write(dir.join("src/b.rs"), "fn beta() {}\n// TODO two\n").unwrap();
        // 造一个明显更大的文件：只有大小确有差异时才值得按体积列清单；
        // 一堆等大的文件列出来是纯 token 成本。
        fs::write(dir.join("src/big.rs"), "z".repeat(4000)).unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");
        let out = call("fs_inspect", &json!({"path": path, "pattern": "TODO"}));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("ok"), "{out}");
        assert!(out.contains("个文件"), "应给文件规模: {out}");
        assert!(
            out.contains("最大："),
            "大小有差异时应按体积给出先看哪些: {out}"
        );
        assert!(out.contains("big.rs"), "最大的文件应排在前面: {out}");
        assert!(out.contains("2 文件 2 处"), "应给命中分布: {out}");
        assert!(out.contains("样例"), "应给命中样例: {out}");
    }

    #[test]
    fn inspect_focus影响排序() {
        let dir = 临时目录("inspect-focus");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/big.rs"), "x".repeat(4000)).unwrap();
        fs::write(dir.join("src/target.rs"), "y".repeat(10)).unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");
        let plain = call("fs_inspect", &json!({"path": path, "limit": 2}));
        let focused = call(
            "fs_inspect",
            &json!({"path": path, "limit": 2, "focus": "target"}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(
            plain.find("big.rs").unwrap() < plain.find("target.rs").unwrap(),
            "默认按大小: {plain}"
        );
        assert!(
            focused.find("target.rs").unwrap() < focused.find("big.rs").unwrap(),
            "focus 应把关键词命中的文件提前: {focused}"
        );
    }

    #[test]
    fn inspect_规模同质时不堆体积清单() {
        // 12 个等大的源文件：列「最大：a 149B, b 149B, …」没有信息量，只是浪费 token。
        let dir = 临时目录("inspect-uniform");
        fs::create_dir_all(dir.join("src")).unwrap();
        for index in 0..12 {
            fs::write(dir.join(format!("src/m{index}.rs")), "fn x() {}\n").unwrap();
        }
        let path = dir.to_string_lossy().replace('\\', "/");
        let out = call("fs_inspect", &json!({"path": path}));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("规模接近"), "同质时应只说规模: {out}");
        assert!(!out.contains("最大："), "同质时不该堆体积清单: {out}");
    }

    #[test]
    fn inspect_空目录与非正则给出可读结果() {
        let dir = 临时目录("inspect-edge");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");
        let empty = call("fs_inspect", &json!({"path": path}));
        fs::write(dir.join("a.txt"), "content\n").unwrap();
        let bad = call("fs_inspect", &json!({"path": path, "pattern": "(?=x)"}));
        let missing = call("fs_inspect", &json!({"path": format!("{path}/nope")}));
        let _ = fs::remove_dir_all(&dir);
        assert!(empty.contains("空目录"), "{empty}");
        assert!(bad.contains("lookaround"), "非法正则要给方言提示: {bad}");
        assert!(missing.starts_with("E_NOT_FOUND"), "{missing}");
    }
    #[test]
    fn expand_把命中展开成整块() {
        let dir = 临时目录("grep-expand-block");
        fs::write(
            dir.join("a.rs"),
            "fn alpha() {\n    let x = 1;\n    let todo = \"TODO\";\n    x + 1\n}\n",
        )
        .unwrap();
        let path = dir.to_string_lossy().replace('\\', "/");
        let plain = grep_tool(
            &json!({"pattern": "TODO", "path": path, "output": "snippets", "context": 0}),
        );
        let expanded = grep_tool(
            &json!({"pattern": "TODO", "path": path, "output": "snippets", "context": 0, "expand": "block"}),
        );
        fs::remove_dir_all(dir).unwrap();
        assert!(plain.contains("3:    let todo"), "{plain}");
        assert!(!plain.contains("1-fn alpha"), "普通模式不该展开: {plain}");
        assert!(
            expanded.contains("1-fn alpha()"),
            "应展开块首行: {expanded}"
        );
        assert!(expanded.contains("5-}"), "应展开到块尾: {expanded}");
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
            assert!(
                first.starts_with("a.txt\n") && first.contains("next_offset=1"),
                "parallel={parallel}: {first}"
            );
            assert!(
                second.starts_with("b.txt\n") && second.contains("next_offset=2"),
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

        // 摘要头是新加的：一次调用就给出「几个文件、分布在哪」，省掉先 files 再 count 两轮。
        assert_eq!(
            output,
            "ok 共 1 个文件 1 处命中；最多：a.txt:1\na.txt\n1-top1\n2-top2\n3:needle\n4-after1\n5-after2",
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

        assert_eq!(
            output, "ok 共 1 个文件 1 处命中；最多：a.txt:1\na.txt\n1-before\n2:needle\n3-after",
            "{output}"
        );
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

/// 批量写入部分失败时把「哪些已写」列全，避免模型整批重发造成二次替换。
fn batch_partial_error(written: &[String], failed: &str, err: &str, total: usize) -> String {
    let done = if written.is_empty() {
        "无".to_string()
    } else {
        written.join(",")
    };
    format!(
        "E_BATCH_PARTIAL 已写入 {}/{}；已写 {done}；失败 {failed}: {err}；仅对剩余文件重发，已写的跳过或用其 hash 作 expect_hash",
        written.len(),
        total
    )
}

/// DOCX/PDF 不支持精确替换；把「会丢什么」和重建命令一次说清。
fn doc_rebuild_hint(code: &str, path: &str) -> String {
    format!(
        "{code} 不支持精确替换 {path}：整体重建只生成纯文本段落，会丢弃全部格式/图片/表格；如确需重建，先 fs_read 取正文，改好后 fs_write path={path} content=<改后全文>；需保留格式请勿用 fsops 编辑"
    )
}

/// 并发冲突：把「先核对内容」写在前面，再给两个可执行的下一步。
///
/// 旧文案先劝「原样重试并改传新 hash」，等于告诉模型跳过内容核对直接放行；
/// 而外部修改可能已经改掉了同一段文本，重试会把外部改动覆盖掉。
fn stale_changed(hash: &str) -> String {
    format!(
        "E_STALE 文件已被其他进程改过（当前 hash={hash}），本次未写入。先 fs_read 核对目标文本是否仍适用且未被外部改动：仍适用则带 expect_hash={hash} 重试，内容已变则按新内容修正 old 后再改"
    )
}

/// 目标不存在时告诉模型怎么放行，而不是只陈述问题。
fn stale_missing() -> &'static str {
    "E_STALE 目标文件不存在，无法校验 hash；确认要新建则去掉 expect_hash 重试，或先 fs_read 确认路径"
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
            "E_READONLY {} 是只读文件；先去掉只读属性（PowerShell: \
             Set-ItemProperty -Path '{}' -Name IsReadOnly -Value $false）再编辑",
            fp.display(),
            fp.display()
        ));
    }
    let io::Snapshot { doc, hash, .. } = io::read_snapshot(fp)?;
    let mixed = doc.eol == crate::text::Eol::Mixed;
    let dominant = mixed.then(|| crate::text::dominant_eol(&doc.text));
    // 陈旧优先于换行：文件变了是更根本的事实。两者同时成立时一次说清，
    // 否则模型要跑两轮（先被 E_MIXED_EOL 拦、再被 E_STALE 拦）——
    // 而「外部追加」恰好是同时制造陈旧与混合换行的常见路径。
    if let Some(expect) = expect_hash {
        if hash != expect {
            let extra = match dominant {
                Some(target) => format!(
                    "；另：该文件当前含混合换行，解决陈旧后可能还需 normalize_eol=true（主风格 {}）",
                    target.as_str()
                ),
                None => String::new(),
            };
            return Err(format!("{}{extra}", stale_changed(&hash)));
        }
    }
    let eol = match dominant {
        Some(target) if normalize_eol => target,
        Some(target) => {
            return Err(format!(
                "E_MIXED_EOL 文件包含混合换行；请显式设置 normalize_eol=true 归一为主风格 {}（将把整文件统一为该风格）",
                target.as_str()
            ));
        }
        None => doc.eol,
    };
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
                return doc_rebuild_hint(doc_format_error(format), &fp.display().to_string());
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
        // 单条修改的简写：模型很自然会把 old/new 或 pattern/replacement 直接放顶层，
        // 而不是嵌进 edits:[{...}]——实测三次独立任务全部这么写（3/3），每次白费一轮。
        // 与其报错纠正，不如接受这种写法：把顶层关键字折进一个 edits 项。
        let has_top_level_edit = ["old", "new", "pattern", "replacement", "all"]
            .iter()
            .any(|key| args.get(*key).is_some());
        let shorthand = if args.get("edits").is_none() && has_top_level_edit {
            let mut item = serde_json::Map::new();
            for key in EDIT_ITEM_FIELDS_FOR_SHORTHAND {
                if let Some(value) = args.get(key) {
                    item.insert(key.to_string(), value.clone());
                }
            }
            Some(Value::Array(vec![Value::Object(item)]))
        } else {
            None
        };
        let edits = match parse_edits(
            shorthand
                .as_ref()
                .unwrap_or_else(|| args.get("edits").unwrap_or(&Value::Null)),
        ) {
            Ok(edits) => edits,
            Err(e) => return e,
        };
        let fp = match FsPath::parse(path) {
            Ok(fp) => fp,
            Err(e) => return format!("E_BAD_PATH {e}"),
        };
        if let Some(format) = crate::documents::format_of(fp.native()) {
            return doc_rebuild_hint(doc_format_error(format), &fp.display().to_string());
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
    // dry_run 只是预演、不会写入，不需要独占锁：否则会和真实写入互斥、白等 3 秒。
    let mut _locks = Vec::with_capacity(requests.len());
    if !dry_run {
        let mut lock_order: Vec<usize> = (0..requests.len()).collect();
        lock_order.sort_by_key(|&index| requests[index].fp.display());
        for index in lock_order {
            match io::lock_path(&requests[index].fp, 3000) {
                Ok(lock) => _locks.push(lock),
                Err(e) if batch_mode => {
                    return format!("E_BATCH {}: {e}", requests[index].path);
                }
                Err(e) => return e,
            }
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

    let mut written: Vec<String> = Vec::new();
    for item in prepared.iter() {
        if let Err(e) = io::atomic_write(&item.fp, &item.bytes) {
            return batch_partial_error(&written, &item.fp.display().to_string(), &e, file_count);
        }
        written.push(item.fp.display().to_string());
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
        for warning in &item.result.warnings {
            out.push_str("\n  ⚠ ");
            out.push_str(warning);
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
        for warning in &item.result.warnings {
            out.push_str("\n  ⚠ ");
            out.push_str(warning);
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

/// fs_write 批量：一次建/覆盖多个文件。
///
/// 为什么要有：`fs_edit` 早就有 `files` 批量，`fs_write` 没有——改 10 个文件 1 轮，
/// 建 10 个文件 10 轮。生成类任务（脚手架、测试夹具、多文件代码生成）里每一轮都要
/// 重发整个上下文，这是最贵的浪费。语义对齐 `fs_edit files`：全部预检通过才写入。
fn write(args: &Value) -> String {
    let batch = match args.get("files").and_then(Value::as_array) {
        Some(files) => files,
        None => return write_one(args),
    };
    if args.get("path").is_some() || args.get("content").is_some() {
        return "E_BAD_ARGS path/content 与 files 不能混用".into();
    }
    if batch.is_empty() {
        return "E_BAD_ARGS files 不能为空".into();
    }
    if batch.len() > 50 {
        return "E_BAD_ARGS 一次最多写入 50 个文件".into();
    }
    let global_dry_run = arg_bool(args, "dry_run", false);
    let global_normalize = arg_bool(args, "normalize_eol", false);

    // 每个文件单独成 args，复用单文件逻辑（路径/编码/换行/hash 校验完全相同）。
    let mut items: Vec<(String, serde_json::Map<String, Value>)> = Vec::with_capacity(batch.len());
    let mut seen: Vec<String> = Vec::new();
    for (index, item) in batch.iter().enumerate() {
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
            return format!("E_BATCH 重复路径 {shown}；同一文件请合并为一项");
        }
        seen.push(shown);
        let mut single = serde_json::Map::new();
        single.insert("path".into(), Value::String(path.to_string()));
        match item.get("content") {
            Some(content @ Value::String(_)) => {
                single.insert("content".into(), content.clone());
            }
            Some(other) => {
                return format!(
                    "E_BATCH {path}: content 必须是字符串；当前是 {}",
                    type_name(other)
                )
            }
            None => return format!("E_BATCH {path}: 缺少 content"),
        }
        if let Some(expect) = item.get("expect_hash").and_then(Value::as_str) {
            single.insert("expect_hash".into(), Value::String(expect.to_string()));
        }
        if global_normalize || arg_bool(item, "normalize_eol", false) {
            single.insert("normalize_eol".into(), Value::Bool(true));
        }
        items.push((path.to_string(), single));
    }

    if global_dry_run {
        // 预演：逐项跑完整校验（不落盘），任何一项失败都不写。
        let mut lines = Vec::with_capacity(items.len());
        for (path, single) in &items {
            let mut args = single.clone();
            args.insert("dry_run".into(), Value::Bool(true));
            let out = write_one(&Value::Object(args));
            if !out.starts_with("ok") {
                return format!("E_BATCH {path}: {out}");
            }
            lines.push(out);
        }
        return format!(
            "ok dry_run: {} 个文件将写入（未落盘）\n{}",
            items.len(),
            lines.join("\n")
        );
    }

    // 两阶段：先全部预检，再统一写入。预检失败时一个文件都不动，
    // 避免"写了一半"这种最难排查的中间态。
    for (path, single) in &items {
        let mut args = single.clone();
        args.insert("dry_run".into(), Value::Bool(true));
        let out = write_one(&Value::Object(args));
        if !out.starts_with("ok") {
            return format!("E_BATCH {path}: {out}");
        }
    }
    let mut lines = Vec::with_capacity(items.len());
    let mut written_paths: Vec<String> = Vec::new();
    for (path, single) in &items {
        let out = write_one(&Value::Object(single.clone()));
        if !out.starts_with("ok") {
            // 预检已通过，这里失败通常是并发改动/IO 故障。
            // 报告形状与 fs_edit files 统一（E_BATCH_PARTIAL + 已写清单），
            // 否则同一个“提交中途失败”在两个工具里要说两遍才能看懂。
            return batch_partial_error(&written_paths, path, &out, items.len());
        }
        written_paths.push(path.clone());
        lines.push(out);
    }
    format!(
        "ok {} 个文件已写入\n{}",
        written_paths.len(),
        lines.join("\n")
    )
}

fn write_one(args: &Value) -> String {
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
    // dry_run 不落盘，不该抢占独占写锁。
    let _write_lock = if arg_bool(args, "dry_run", false) {
        None
    } else {
        match io::lock_path(&fp, 3000) {
            Ok(lock) => Some(lock),
            Err(e) => return e,
        }
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
                return stale_missing().into();
            }
            let bytes = match io::read_bytes(&fp.wide()) {
                Ok(bytes) => bytes,
                Err(e) => return e,
            };
            if io::hash_hex(&bytes) != expect {
                return stale_changed(&io::hash_hex(&bytes));
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
            // 全量覆盖时源文件的混合换行已无意义：只要待写内容本身风格稳定，
            // 直接按内容风格写，不再要求 normalize_eol（旧行为会多拦一轮）。
            if content_eol != crate::text::Eol::Mixed {
                content_eol
            } else {
                let target = crate::text::dominant_eol(content);
                if !arg_bool(args, "normalize_eol", false) {
                    return format!(
                        "E_MIXED_EOL 待写内容包含混合换行；请显式设置 normalize_eol=true 归一为主风格 {}（将把整文件统一为该风格）",
                        target.as_str()
                    );
                }
                target
            }
        }
        Some(snapshot) => snapshot.doc.eol,
        None if content_eol == crate::text::Eol::Mixed => {
            let target = crate::text::dominant_eol(content);
            if !arg_bool(args, "normalize_eol", false) {
                return format!(
                    "E_MIXED_EOL 待写内容包含混合换行；请显式设置 normalize_eol=true 归一为主风格 {}（将把整文件统一为该风格）",
                    target.as_str()
                );
            }
            target
        }
        None => content_eol,
    };
    if let Some(expect) = arg_str(args, "expect_hash") {
        match &existing {
            Some(snapshot) => {
                if snapshot.hash != expect {
                    return stale_changed(&snapshot.hash);
                }
            }
            None => return stale_missing().into(),
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

/// `fs_inspect`：一次给出「目录规模 + 命中分布 + 样例」。
///
/// 为什么要合成一个工具：接手陌生仓库时，模型固定要走
/// `fs_glob`（有什么）→ `fs_stat`（多大/什么编码）→ `fs_grep`（目标在哪）三步，
/// 每一轮都要把整个上下文重发一次。这里把它们压成一次调用。
/// 不追求把每个文件细节都给出——只给「先看哪些」和「目标在哪」，细节交给后续 fs_read。
fn inspect_tool(args: &Value) -> String {
    let budget = arg_usize(args, "budget", 16 * 1024);
    let limit = arg_usize(args, "limit", 15).max(1);
    let root_s = arg_str(args, "path").unwrap_or(".");
    let root = match FsPath::parse(root_s) {
        Ok(root) => root,
        Err(e) => return format!("E_BAD_PATH {e}"),
    };
    if !root.exists() {
        return format!("E_NOT_FOUND {} 不存在；用 fs_glob 确认路径", root.display());
    }

    // 目录摘要：沿用 walk 的 .gitignore 感知与忽略目录，保证与 fs_glob 口径一致。
    let opts = WalkOpts {
        include_dirs: false,
        limit: 100_000,
        follow_links: false,
        offset: 0,
        time_budget_ms: arg_usize(args, "time_budget_ms", 3000) as u128,
        no_ignore: arg_bool(args, "no_ignore", false),
    };
    let entries = match walk::glob_with_report(&root, "", &opts) {
        Ok((entries, _)) => entries,
        Err(e) => return e,
    };
    if entries.is_empty() {
        return format!("ok 空目录（{}）：没有可统计的文件", root.display());
    }

    let total_files = entries.len();
    let total_bytes: u64 = entries.iter().map(|e| e.size).sum();
    // 按大小排序给「先看哪些文件」；focus 命中的优先，因为模型通常已经知道关键词。
    let focus_words: Vec<String> = arg_str(args, "focus")
        .unwrap_or("")
        .split_whitespace()
        .map(|word| word.to_ascii_lowercase())
        .filter(|word| !word.is_empty())
        .collect();
    let focus_hit = |path: &str| -> bool {
        if focus_words.is_empty() {
            return false;
        }
        let lower = path.to_ascii_lowercase();
        focus_words.iter().any(|word| lower.contains(word))
    };
    let mut ranked: Vec<&walk::Entry> = entries.iter().collect();
    ranked.sort_by(|a, b| {
        focus_hit(&b.path)
            .cmp(&focus_hit(&a.path))
            .then_with(|| b.size.cmp(&a.size))
            .then_with(|| a.path.cmp(&b.path))
    });
    // 体积分布按「真实大小顺序」统计，不能用 ranked：focus 会把小文件提到前面，
    // 拿它算 largest/median 会把「有大小差异」误判成「规模接近」。
    let mut by_size: Vec<u64> = entries.iter().map(|entry| entry.size).collect();
    by_size.sort_unstable_by(|a, b| b.cmp(a));

    let mut out = format!("ok {} 个文件 {}", total_files, io::human_size(total_bytes));
    // 文件大小高度同质时（一堆等大的源文件），按大小列十几个没有任何信息量，
    // 只剩 token 成本。判据用「最大值 vs 中位数」而不是「最大 vs 最小」——
    // 后者会被一个特别小的文件（README）骗过，误判成"差异很大"。
    let largest = by_size.first().copied().unwrap_or(0);
    let median = by_size.get(by_size.len() / 2).copied().unwrap_or(0);
    if largest > median.saturating_mul(3) && largest > 0 {
        let listed: Vec<String> = ranked
            .iter()
            .take(limit)
            .map(|entry| format!("{} {}", entry.path, io::human_size(entry.size)))
            .collect();
        out.push_str("\n最大：");
        out.push_str(&listed.join(", "));
        if total_files > listed.len() {
            out.push_str(&format!(" …（共 {} 个）", total_files));
        }
    } else if let Some(top) = ranked.first() {
        out.push_str(&format!(
            "\n规模接近：约 {}（随机取 {} 为例）",
            io::human_size(top.size),
            top.path
        ));
    }

    // 命中分布：模型最常见的下一步就是「目标在哪」，顺手给出。
    if let Some(pattern) = arg_str(args, "pattern") {
        // 用 Matches 而不是 Count：count 模式不物化逐行命中，拿不到样例行，
        // 而样例正是让模型「不用再 grep 一次」的关键。
        let grep_opts = search::GrepOpts {
            pattern: pattern.to_string(),
            output: search::GrepOutput::Matches,
            limit: 400,
            ..Default::default()
        };
        match search::grep(&root, &grep_opts) {
            Ok(result) if !result.file_counts.is_empty() => {
                let mut dist: Vec<&(String, usize)> = result.file_counts.iter().collect();
                dist.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                // 分布条数跟着 limit 走：写死 8 时，模型想拿全 12 个文件名只能再跑一次
                // fs_grep（实测 A/B 两侧都因此多花一轮）。上限 40 防止超大结果撑爆。
                let dist_cap = limit.clamp(8, 40);
                let shown: Vec<String> = dist
                    .iter()
                    .take(dist_cap)
                    .map(|(path, count)| format!("{path}:{count}"))
                    .collect();
                out.push_str(&format!(
                    "\n命中 `{pattern}`：{} 文件 {} 处 → {}{}",
                    result.file_counts.len(),
                    result.match_count,
                    shown.join(", "),
                    if dist.len() > shown.len() { " …" } else { "" }
                ));
                // 样例：只给每个文件第一条命中的行号+内容，够模型决定读哪里。
                let samples: Vec<String> = result
                    .hits
                    .iter()
                    .filter(|hit| !hit.is_context)
                    .take(6)
                    .map(|hit| {
                        let text = hit.text.trim();
                        let shown = if text.chars().count() > 120 {
                            format!("{}…", text.chars().take(120).collect::<String>())
                        } else {
                            text.to_string()
                        };
                        format!("\n  {}:{} {}", hit.path, hit.line, shown)
                    })
                    .collect();
                if !samples.is_empty() {
                    out.push_str("\n样例：");
                    for sample in &samples {
                        out.push_str(sample);
                    }
                }
            }
            Ok(_) => out.push_str(&format!("\n命中 `{pattern}`：无")),
            Err(e) => out.push_str(&format!("\n命中 `{pattern}`：{e}")),
        }
    }
    out.push_str("\n如需细节：fs_read 目标文件，或 fs_grep expand=block 直接取整块");
    render::truncate_utf8(&out, budget).to_string()
}

fn glob_tool(args: &Value) -> String {
    let budget = arg_usize(args, "budget", 16 * 1024);
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
                        let next = offset + shown;
                        text.push_str(&format!(
                            "… next_offset={next} | 续读: fs_glob offset={next}（单位：匹配条目）\n"
                        ));
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
    let budget = arg_usize(args, "budget", 16 * 1024);
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
    // 默认 200（原 100）：预算才是真正的上限（默认 32KB，渲染层按 budget 收敛），
    // limit 只是"最多取多少条"的保护。100 条在小仓库里经常卡在翻页边界上，
    // 模型要多跑一轮 offset=100 才拿到剩下的命中——而那一轮要把整个上下文重发一次。
    // 给到 200 让常见仓库一次装完，超出预算时仍会截断并给 next_offset。
    let mut limit = arg_usize(args, "limit", 200);
    if limit == 0 {
        return render::truncate_utf8("E_BAD_ARGS limit 必须大于 0；省略表示默认 200", budget)
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
        max_line: arg_usize(args, "max_line", render::MAX_LINE_CHARS),
        budget,
        parallel: match args.get("parallel").and_then(Value::as_bool) {
            None => search::ParallelMode::Auto,
            Some(true) => search::ParallelMode::On,
            Some(false) => search::ParallelMode::Off,
        },
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
            // expand=block：把每条命中展开成它所在的完整代码块。
            // 真实编码循环是「grep 拿行号 → fs_read block=true 读实现」，两轮；
            // 这里把第二步吞掉。只对 matches/snippets 生效，读不到块时退回原命中行。
            let expanded = if arg_str(args, "expand") == Some("block")
                && matches!(output, GrepOutput::Matches | GrepOutput::Snippets)
            {
                expand_hits_to_blocks(&r, &root)
            } else {
                None
            };
            let mut rendered_matches = 0usize;
            let mut render_cut = false;
            let output = render::converge_budget(budget, |body_budget| {
                let rendered = match &expanded {
                    Some(text) => {
                        // 展开结果自己就是最终正文（已按预算收敛），不再走行渲染。
                        let cut = text.len() > body_budget;
                        let body = render::truncate_utf8(text, body_budget).to_string();
                        rendered_matches = r.match_count;
                        render_cut = cut;
                        search::GrepRendered {
                            text: body,
                            matches_rendered: r.match_count,
                            files_rendered: r.file_counts.len(),
                            truncated: cut,
                        }
                    }
                    None => search::render_mode(&r, output, body_budget),
                };
                rendered_matches = rendered.matches_rendered;
                render_cut = rendered.truncated;
                let mut text = rendered.text.trim_end().to_string();
                if !skip_note.is_empty() {
                    text.push_str(&skip_note);
                }
                // 摘要头：一次调用就告诉模型「总共多少文件/命中、分布在哪」。
                // 没有这一行时，模型必须先 fs_grep output=files 看范围、再 output=count
                // 看分布，最后才回 snippets 看内容——同一个 pattern 白花两轮上下文。
                if matches!(output, GrepOutput::Matches | GrepOutput::Snippets)
                    && !r.file_counts.is_empty()
                {
                    let total_files = r.file_counts.len();
                    let mut dist: Vec<(&str, usize)> = r
                        .file_counts
                        .iter()
                        .map(|(path, count)| (path.as_str(), *count))
                        .collect();
                    dist.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
                    let shown_dist: Vec<String> = dist
                        .iter()
                        .take(5)
                        .map(|(path, count)| format!("{path}:{count}"))
                        .collect();
                    let more = if total_files > shown_dist.len() {
                        "…".to_string()
                    } else {
                        String::new()
                    };
                    let scope = if r.truncated { "本页覆盖 " } else { "共 " };
                    text = format!(
                        "ok {}{} 个文件 {} 处命中；最多：{}{}\n{}",
                        scope,
                        total_files,
                        r.match_count,
                        shown_dist.join(", "),
                        more,
                        text
                    );
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
                    let mode = format!("{output:?}").to_ascii_lowercase();
                    let unit = if output == GrepOutput::Files {
                        "匹配文件"
                    } else {
                        "命中"
                    };
                    text.push_str(&format!(
                        "\n… next_offset={next} | 续读: fs_grep offset={next} output={mode}（单位：{unit}）"
                    ));
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

/// stat 输出里路径可能含空格，用引号包裹让模型能稳定切出完整路径。
fn quoted_stat_path(fp: &FsPath) -> String {
    format!("\"{}\"", fp.display())
}

fn stat(args: &Value) -> String {
    let detail = arg_str(args, "detail").unwrap_or("full");
    if detail == "server" {
        // 版本自检：宿主重载后模型一条调用就能确认连的是哪版、几个工具。
        return format!("fsops {} tools={}", crate::VERSION, tool_defs().len());
    }
    if !matches!(detail, "meta" | "hash" | "full") {
        return "E_BAD_ARGS detail 仅支持 meta/hash/full/server".into();
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
                    quoted_stat_path(&fp),
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
                        quoted_stat_path(&fp),
                        io::human_size(m.size),
                        io::fmt_time(m.mtime),
                        format_label(format),
                        if m.readonly { "ro" } else { "rw" },
                        doc.hash
                    ));
                } else {
                    out.push_str(&format!(
                        "{} file {} {} {} LF {} {}行 hash={}\n",
                        quoted_stat_path(&fp),
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
                    quoted_stat_path(&fp),
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
                            quoted_stat_path(&fp),
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
                            quoted_stat_path(&fp),
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
                    quoted_stat_path(&fp),
                    io::human_size(m.size)
                )),
            }
        } else {
            // 符号链接自身 size 是 0，只报 0B 会让模型以为目标文件是空的。
            match &m.link {
                Some((target, target_size)) => out.push_str(&format!(
                    "{} {} {} {} -> {} {}\n",
                    quoted_stat_path(&fp),
                    m.kind,
                    io::human_size(m.size),
                    io::fmt_time(m.mtime),
                    target,
                    io::human_size(*target_size)
                )),
                None => out.push_str(&format!(
                    "{} {} {} {}\n",
                    quoted_stat_path(&fp),
                    m.kind,
                    io::human_size(m.size),
                    io::fmt_time(m.mtime)
                )),
            }
        }
    }
    let failed = out
        .lines()
        .filter(|line| line.trim_start().starts_with("E_"))
        .count();
    if targets.len() > 1 && failed > 0 {
        // 一个路径失败不该淹没其余结果：首行先给成败计数，
        // 调用方即使只看到 isError=true，也知道还有多少内容可用。
        let ok = targets.len() - failed.min(targets.len());
        return format!(
            "ok stat {} 项：{} 成功，{} 失败\n{}",
            targets.len(),
            ok,
            failed,
            out.trim_end()
        );
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
    fn reads顶层_max_line_传给每项() {
        // P4 F-7：reads 与 path/paths 的互斥已报错，但 max_line/scan 被静默忽略。
        // 写了 max_line=0 却拿到截断行，模型会以为文件本来如此。
        // 它们与 budget 一样对每一项都适用，应该传播而不是丢弃。
        let dir = 临时目录("reads-top-maxline");
        let path = dir.join("a.txt");
        let long = "x".repeat(300);
        fs::write(&path, format!("{long}\nshort\n")).unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_read",
            &json!({"reads": [{"path": posix}], "max_line": 0}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(
            !out.contains("(+60字符)") && !out.contains("…(+"),
            "顶层 max_line=0 应传到子项，不应出现行内截断标记: {out}"
        );
        assert!(!out.contains("E_BAD_ARGS"), "不应把合法参数当错误: {out}");
    }

    #[test]
    fn reads与tail同时给时报错而不是静默忽略() {
        // tail 没有「每项一个尾部」的合理语义；静默忽略等于让模型以为读到了末尾。
        let dir = 临时目录("reads-tail");
        let path = dir.join("a.txt");
        fs::write(&path, "1\n2\n3\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call("fs_read", &json!({"reads": [{"path": posix}], "tail": 1}));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
        assert!(out.contains("tail"), "应点名冲突参数: {out}");
    }

    #[test]
    fn block与ranges同时给时报错() {
        // block 按 start 行探测块边界，ranges 直接给区间：两者对“读哪些行”
        // 回答不同。旧行为静默按 ranges 处理，模型会以为自己拿到了整个函数。
        let dir = 临时目录("block-ranges");
        let path = dir.join("a.rs");
        fs::write(&path, "fn a() {\n    one();\n    two();\n}\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_read",
            &json!({"path": posix, "start": 2, "block": true, "ranges": [[2, 3]]}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
        assert!(out.contains("block"), "应点名冲突参数: {out}");
    }

    #[test]
    fn 嵌套块截断后按范围续读不遗漏() {
        let dir = 临时目录("nested-block-resume");
        let path = dir.join("a.rs");
        let mut body = String::from("fn outer() {\n    if ready {\n");
        for n in 0..60 {
            body.push_str(&format!("        step_{n:02}();\n"));
        }
        body.push_str("    }\n    after_inner();\n}\n");
        fs::write(&path, &body).unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let mut args = json!({"path": posix, "start": 1, "block": true, "budget": 800});
        let mut seen = Vec::new();
        let continuation = regex::Regex::new(r"ranges=(\[\[.*\]\])").unwrap();
        let mut recovery = String::new();
        for _ in 0..20 {
            let out = call("fs_read", &args);
            assert!(!out.starts_with("E_"), "{out}");
            for line in out.lines() {
                if let Some((number, _)) = line.split_once('|') {
                    if let Ok(number) = number.parse::<usize>() {
                        seen.push(number);
                    }
                }
            }
            let Some(capture) = continuation.captures(&out) else {
                break;
            };
            let ranges: Value = serde_json::from_str(&capture[1]).unwrap();
            if recovery.is_empty() {
                recovery = call(
                    "fs_read",
                    &json!({
                        "path": posix, "block": true, "ranges": ranges
                    }),
                );
            }
            // 保留原块的剩余范围，不能用 block 重新定位内层块。
            args = json!({"path": posix, "ranges": ranges, "budget": 800});
        }
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(seen, (1..=body.lines().count()).collect::<Vec<_>>());
        assert!(recovery.starts_with("E_BAD_ARGS"), "{recovery}");
        assert!(
            recovery.contains("移除 block") && recovery.contains("ranges"),
            "{recovery}"
        );
        assert!(!recovery.contains("传 block=true 与 start"), "{recovery}");
    }

    #[test]
    fn inspect声明的预算参数与实现一致() {
        // P0 F-6：实现早就读 time_budget_ms/no_ignore，schema 却没声明，
        // 传了直接撞 additionalProperties:false，等于这两个能力不可达。
        let defs = tool_defs();
        let inspect = defs
            .iter()
            .find(|d| d["name"] == "fs_inspect")
            .expect("缺少 fs_inspect");
        let props = &inspect["inputSchema"]["properties"];
        for key in ["time_budget_ms", "no_ignore", "budget", "limit"] {
            assert!(
                props.get(key).is_some(),
                "fs_inspect 缺少声明参数 {key}（实现读了它）"
            );
        }
        assert!(props["no_ignore"]["type"] == "boolean");
        assert!(props["time_budget_ms"]["type"] == "integer");
    }

    #[test]
    fn inspect接受声明的预算参数() {
        let dir = 临时目录("inspect-budget-args");
        fs::write(dir.join("a.txt"), "needle\n").unwrap();
        let posix = dir.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_inspect",
            &json!({"path": posix, "time_budget_ms": 2000, "no_ignore": true}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(!out.starts_with("E_BAD_ARGS"), "声明的参数不应被拒: {out}");
        assert!(out.contains("个文件"), "应给出目录规模: {out}");
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
    fn reads项支持budget与focus() {
        let dir = 目录("reads-item-opts");
        let a = dir.join("a.rs");
        let b = dir.join("b.txt");
        fs::write(
            &a,
            "fn one() {\n    let t = 1;\n}\n\nfn two() {\n    let t = 2;\n}\n",
        )
        .unwrap();
        fs::write(&b, "alpha\nbeta\n").unwrap();
        let out = call(
            "fs_read",
            &json!({"reads": [
                {"path": a.to_string_lossy().replace('\\', "/"), "focus": "let t", "budget": 2048},
                {"path": b.to_string_lossy().replace('\\', "/"), "budget": 400}
            ]}),
        );
        let _ = fs::remove_dir_all(&dir);
        // a 项：focus 只给命中所在的完整函数块
        assert!(out.contains("fn one()"), "{out}");
        assert!(out.contains("fn two()"), "{out}");
        // b 项：单独的小预算也放得下两行
        assert!(out.contains("alpha"), "{out}");
        assert!(out.contains("beta"), "{out}");
    }

    #[test]
    fn stat_server模式返回版本与工具数() {
        let out = call("fs_stat", &json!({"detail": "server"}));
        assert!(out.contains(crate::VERSION), "{out}");
        // 工具数不要写死：与 tool_defs() 对齐，增删工具时这里自动跟上，
        // 否则每加一个工具都要改一遍测试（改漏就把「工具数」这个事实固化成错的）。
        let expected = format!("tools={}", tool_defs().len());
        assert!(out.contains(&expected), "应为 {expected}: {out}");
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
        assert!(out.starts_with("ok reads"), "应带部分成功头: {out}");
        assert!(out.contains("1 成功"), "应报成功项数: {out}");
        assert!(out.contains("1 失败"), "应报失败项数: {out}");
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
    fn focus正则方言错误点明lookaround与反向引用() {
        // focus 与 fs_grep/fs_edit 同用 Rust regex：PCRE 写法要直接点破方言差异。
        let dir = 目录("focus-dialect");
        let path = dir.join("a.rs");
        fs::write(&path, "fn alpha() {}\n").unwrap();
        let out = call(
            "fs_read",
            &json!({"path": path.to_string_lossy().replace('\\', "/"), "focus": "(?=alpha)"}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("E_BAD_REGEX"), "{out}");
        assert!(out.contains("lookaround"), "{out}");
        assert!(out.contains("反向引用"), "{out}");
        assert!(out.contains("${1}"), "{out}");
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
        assert!(out.contains("仅显示前 12 处"), "{out}");
        assert!(out.contains("命中 20 处"), "{out}");
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
    fn 三十kb文件默认一次读完() {
        // 默认预算 32KB：多一轮续读就是一次完整上下文重发，比多给几 KB 贵。
        let dir = 目录("default-budget");
        let path = dir.join("big.txt");
        let content: String = (1..=3000).map(|i| format!("line{i:04}\n")).collect();
        fs::write(&path, &content).unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call("fs_read", &json!({"path": posix, "force": true}));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.len() > 24_000, "应一次读完 27KB: {} 字节", out.len());
        assert!(!out.contains("续读"), "不应需要续读: {out}");
    }

    #[test]
    fn 多文件按大小分配预算() {
        let dir = 目录("paths-share");
        let mut paths = Vec::new();
        for i in 0..10 {
            let small = dir.join(format!("s{i}.txt"));
            fs::write(&small, "a\nb\nc\nd\ne\n").unwrap();
            paths.push(small.to_string_lossy().replace('\\', "/"));
        }
        for i in 0..10 {
            let big = dir.join(format!("b{i}.txt"));
            let body: String = (1..=200)
                .map(|n| format!("line{n} padding padding\n"))
                .collect();
            fs::write(&big, body).unwrap();
            paths.push(big.to_string_lossy().replace('\\', "/"));
        }
        let out = call("fs_read", &json!({"paths": paths}));
        let _ = fs::remove_dir_all(&dir);
        // 小文件必须读满（再跑一轮更贵）
        assert!(out.contains("5|e"), "小文件应读满: {out}");
        // 大文件拿到的行数应明显多于均分（32KB/20 ≈ 40 行）
        let big_block = out.split("b0.txt").nth(1).expect("应包含 b0.txt");
        let lines = big_block
            .lines()
            .filter(|l| {
                l.split_once('|')
                    .map(|(n, _)| n.parse::<usize>().is_ok())
                    .unwrap_or(false)
            })
            .count();
        assert!(lines > 50, "大文件应拿到更多行，实际 {lines}");
    }

    #[test]
    fn tail读最后几行() {
        let dir = 目录("tail");
        let path = dir.join("log.txt");
        fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
        let posix = path.to_string_lossy().replace('\\', "/");
        let out = call("fs_read", &json!({"path": posix, "tail": 2, "force": true}));
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("4|d"), "{out}");
        assert!(out.contains("5|e"), "{out}");
        assert!(!out.contains("1|a"), "不应从头读: {out}");
    }

    #[test]
    fn tail与start互斥() {
        let out = call("fs_read", &json!({"path": "x.txt", "tail": 5, "start": 1}));
        assert!(out.starts_with("E_BAD_ARGS"), "{out}");
    }

    #[test]
    fn 多文件续读提示带各自路径() {
        let dir = 目录("hint-multi");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        let body: String = (1..=100).map(|n| format!("line{n}\n")).collect();
        fs::write(&a, &body).unwrap();
        fs::write(&b, &body).unwrap();
        let pa = a.to_string_lossy().replace('\\', "/");
        let pb = b.to_string_lossy().replace('\\', "/");
        let out = call(
            "fs_read",
            &json!({"paths": [pa, pb], "budget": 900, "force": true}),
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("续读: fs_read path="), "续读应带路径: {out}");
        assert!(out.contains("a.txt"), "{out}");
        assert!(out.contains("b.txt"), "{out}");
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
