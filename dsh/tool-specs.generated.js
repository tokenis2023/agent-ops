/**
 * 本文件由 scripts/gen_dsh_specs.mjs 从 `fsops schema` 生成，请勿手工编辑。
 * 改了 src/tools.rs 的工具定义后，跑一次该脚本重新生成。
 */
export const TOOL_SPECS = [
  {
    "name": "fs_read",
    "readOnly": true,
    "description": "读取文件。优先用本工具替代宿主 read_file/cat/head/tail/Get-Content。默认最多返回 32KB 并给出续读方式；支持行范围、多文件、DOCX/PDF 正文、自动识别 GBK/UTF-16/BOM 与 CRLF。模式表：path 与 paths 可同时给（并集，一次读多个文件）；reads 是异构区间数组（与 path/paths 互斥）；focus 按正则取整块（可与 path 同用，与 reads/paths/ranges/start/end/block/summary 互斥）；tail=N 读末尾（与 start/end/ranges/block/summary/focus/paths 互斥）；summary 只给声明行号（仅单文件，不能与 ranges/block/end 同用）。reads 每项可带 focus 与 budget。",
    "parameters": {
      "block": {
        "description": "返回包含 start 行的整个代码块（自动向前找块起始、向后配平结束）；配合 fs_grep 的行号用，命中行在函数内部也能展开",
        "type": "boolean"
      },
      "budget": {
        "description": "最多返回字节数，默认 32768（约 8K token）；文件 ≤32KB 且未显式指定时自动读全文",
        "type": "integer"
      },
      "end": {
        "description": "结束行（含）；用 block=true 时可省略",
        "type": "integer"
      },
      "focus": {
        "description": "只返回匹配该正则的完整代码块（含上下文），替代「先 fs_grep 再 block=true」两步。数据文件（JSON/YAML/Markdown 等）没有代码块，只给命中行 ±2 行。仅单文件，与 paths/reads/ranges/start/end/block/summary 互斥。JSON 字符串里反斜杠需双写",
        "type": "string"
      },
      "force": {
        "description": "读取缓存默认关闭；仅当 FSOPS_MCP_CACHE=1 开启后，用本参数绕过未变更摘要、强制返回正文。默认部署重复读直接返回全文，无需传",
        "type": "boolean"
      },
      "max_line": {
        "description": "每行最多字符数，默认 240；0 表示不截断长行",
        "type": "integer"
      },
      "path": {
        "description": "文件路径，如 /d/repo/app/src/main.rs",
        "type": "string"
      },
      "paths": {
        "description": "一次读多个文件（最多 20 个），所有文件共用 start/end",
        "type": "array",
        "items": {
          "type": "string"
        }
      },
      "ranges": {
        "description": "单文件多段行区间，如 [[1,2],[8,9]]；优先于 start/end",
        "type": "array",
        "items": {
          "type": "array",
          "items": {
            "type": "integer"
          }
        }
      },
      "reads": {
        "description": "一次读多个文件的不同区间，每项 {path,start?,end?,ranges?,block?,max_line?,scan?,budget?,focus?}；与 path/paths/ranges/start/end/block/summary/focus 互斥，预算按项均分",
        "type": "array",
        "items": {
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "block": {
              "type": "boolean"
            },
            "budget": {
              "description": "该项独立预算；未给时与其他项均分顶层 budget",
              "type": "integer"
            },
            "end": {
              "type": "integer"
            },
            "focus": {
              "description": "该项只返回命中所在的完整代码块",
              "type": "string"
            },
            "max_line": {
              "type": "integer"
            },
            "path": {
              "type": "string"
            },
            "ranges": {
              "type": "array",
              "items": {
                "type": "array",
                "items": {
                  "type": "integer"
                }
              }
            },
            "scan": {
              "type": "string",
              "enum": [
                "window",
                "full"
              ]
            },
            "start": {
              "type": "integer"
            }
          }
        }
      },
      "scan": {
        "description": "仅对 >32MB 文件生效：window（默认）只读到请求区间末尾并探测是否还有内容，毫秒级返回，但不给全文 hash/总行数；full 全文件扫描，给精确 hash 与总行数",
        "type": "string",
        "enum": [
          "window",
          "full"
        ]
      },
      "start": {
        "description": "起始行（1 基）",
        "type": "integer"
      },
      "summary": {
        "description": "只输出结构摘要：声明行 + 行号，不展开函数体；行号可直接接 block=true 读实现。与 ranges/block/end 互斥，数据文件与超大文件不支持",
        "type": "boolean"
      },
      "tail": {
        "description": "只读最后 N 行（日志场景）：与 start/end/ranges/block/summary/focus/paths 互斥，文件不足 N 行时读全部；免去先 fs_stat 拿总行数那一步",
        "type": "integer"
      }
    }
  },
  {
    "name": "fs_edit",
    "description": "精确替换文本（old→new）或正则替换（pattern→replacement）：不知道确切原文时用 pattern，省掉为抄原文而做的一次 fs_read。两套参数必须二选一：old+new 或 pattern+replacement；同时给会报 E_BAD_EDIT（旧版本会静默只执行 pattern）。优先替代 patch/Edit/apply_patch。同文件多处用 edits，多文件用 files；全部预检通过才写入。自动处理 CRLF/LF 差异并保留原编码；DOCX 不支持精确替换，请用 fs_write 整体重建。",
    "parameters": {
      "diff": {
        "description": "附带被替换的原文/新文片段（`@@ 行号` + `-原文` / `+新文`）；默认关闭，改结构后想自查时打开，可省一次 fs_read",
        "type": "boolean"
      },
      "dry_run": {
        "description": "只校验不落盘",
        "type": "boolean"
      },
      "edits": {
        "description": "单文件的修改列表",
        "type": "array",
        "items": {
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "all": {
              "description": "替换全部出现位置",
              "type": "boolean"
            },
            "end": {
              "description": "行范围结束行（含）；省略表示到文件末尾",
              "type": "integer"
            },
            "new": {
              "description": "替换内容，空串表示删除；与 replacement 二选一",
              "type": "string"
            },
            "old": {
              "description": "原文片段（需唯一，除非 all=true）；与 pattern 二选一",
              "type": "string"
            },
            "pattern": {
              "description": "正则模式：替换范围内所有匹配，可用 $1 引用捕获组；不知道确切原文时用它，省掉为抄原文而做的一次 fs_read",
              "type": "string"
            },
            "replacement": {
              "description": "正则替换内容，与 pattern 配套",
              "type": "string"
            },
            "start": {
              "description": "限定替换只发生在第 start..=end 行内（1 基）；配合 all=true 只改某个函数内的重复字面量",
              "type": "integer"
            }
          }
        }
      },
      "expect_hash": {
        "description": "单文件并发保护：文件已变则拒绝写入",
        "type": "string"
      },
      "files": {
        "description": "多文件批量编辑；全部文件预检成功后逐文件原子写入，最多 50 个",
        "type": "array",
        "items": {
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "edits": {
              "type": "array",
              "items": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                  "all": {
                    "type": "boolean"
                  },
                  "end": {
                    "type": "integer"
                  },
                  "new": {
                    "type": "string"
                  },
                  "old": {
                    "type": "string"
                  },
                  "pattern": {
                    "type": "string"
                  },
                  "replacement": {
                    "type": "string"
                  },
                  "start": {
                    "type": "integer"
                  }
                }
              }
            },
            "expect_hash": {
              "description": "该文件的并发保护指纹",
              "type": "string"
            },
            "normalize_eol": {
              "description": "该文件含混合换行时，显式允许归一为主换行风格（多数行使用的风格，平局取 LF）",
              "type": "boolean"
            },
            "path": {
              "type": "string"
            }
          }
        }
      },
      "normalize_eol": {
        "description": "混合换行时显式归一为主换行风格（多数行使用的风格，平局取 LF）",
        "type": "boolean"
      },
      "path": {
        "description": "单文件路径；批量时改用 files",
        "type": "string"
      }
    }
  },
  {
    "name": "fs_write",
    "description": "写入文件。优先替代 write_file/Write。原子替换、自动建目录、保留原有编码与换行风格；文本外也支持生成 DOCX（纯文本段落，整体重建）。多文件用 files 一次提交（全部预检通过才写），不要一个文件一轮。",
    "parameters": {
      "content": {
        "type": "string"
      },
      "dry_run": {
        "description": "只做路径/编码/换行/hash 预检，不创建也不覆盖文件",
        "type": "boolean"
      },
      "expect_hash": {
        "description": "并发保护：文件已变则拒绝覆盖",
        "type": "string"
      },
      "files": {
        "description": "多文件批量写入（一次建 10 个文件不再 10 轮）；全部预检通过才写入，最多 50 个；与 path/content 互斥",
        "type": "array",
        "items": {
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "content": {
              "type": "string"
            },
            "expect_hash": {
              "type": "string"
            },
            "normalize_eol": {
              "type": "boolean"
            },
            "path": {
              "type": "string"
            }
          }
        }
      },
      "normalize_eol": {
        "description": "新文件或原文件为混合换行时，显式归一为主换行风格（多数行使用的风格，平局取 LF）；已有稳定文件始终沿用原换行",
        "type": "boolean"
      },
      "path": {
        "type": "string"
      }
    }
  },
  {
    "name": "fs_glob",
    "readOnly": true,
    "description": "按 glob 找文件。优先替代 find/Glob/Get-ChildItem。尊重 .gitignore，自动跳过 node_modules/target 等。默认只给路径，detail=true 附大小与时间；map=true 输出按引用重要性排序的 repo map（文件+符号），一次回答先看哪些文件。",
    "parameters": {
      "budget": {
        "description": "最终响应最多字节数，默认 16384",
        "type": "integer"
      },
      "detail": {
        "description": "是否附大小与修改时间（默认只给路径，省 token）",
        "type": "boolean"
      },
      "dirs": {
        "description": "是否包含目录",
        "type": "boolean"
      },
      "focus": {
        "description": "map=true 时的关注词，空格分隔；文件名或符号命中会加权",
        "type": "string"
      },
      "limit": {
        "description": "最多返回条数，默认 200；达到后提前停止",
        "type": "integer"
      },
      "map": {
        "description": "输出 repo map：按符号被引用程度、入口点和 focus 排序的文件与符号，一次调用回答先看哪些文件",
        "type": "boolean"
      },
      "max_files": {
        "description": "map=true 时最多扫描的文件数，默认 2000",
        "type": "integer"
      },
      "no_ignore": {
        "description": "忽略 .gitignore 与 node_modules/target 等默认跳过项；只在明确要查依赖源码或构建产物时用",
        "type": "boolean"
      },
      "offset": {
        "description": "跳过前 N 个匹配条目，用于 next_offset 续页",
        "type": "integer"
      },
      "path": {
        "description": "搜索根目录，默认当前目录",
        "type": "string"
      },
      "pattern": {
        "description": "如 **/*.rs，多个用逗号分隔；省略表示全部",
        "type": "string"
      },
      "time_budget_ms": {
        "description": "目录遍历时间预算，默认 5000 毫秒；超时返回已找到结果并标注",
        "type": "integer"
      }
    }
  },
  {
    "name": "fs_inspect",
    "description": "一次看清目录：文件规模分布 + 可选的正则命中分布与样例，替代「先 fs_glob 看结构、再 fs_stat 查大小、再 fs_grep 找目标」的多轮探索。适合接手陌生仓库或任务开头；已知具体目标后改用 fs_grep/fs_read。",
    "parameters": {
      "budget": {
        "description": "响应字节上限，默认 16384",
        "type": "integer"
      },
      "focus": {
        "description": "可选关键词（空格分隔）：影响「最相关文件」的排序，命中文件名或路径的优先",
        "type": "string"
      },
      "limit": {
        "description": "列出的文件数，默认 15",
        "type": "integer"
      },
      "path": {
        "description": "根目录，默认当前目录",
        "type": "string"
      },
      "pattern": {
        "description": "可选 Rust 正则：给了就附带命中分布与前几条命中样例",
        "type": "string"
      }
    }
  },
  {
    "name": "fs_grep",
    "readOnly": true,
    "description": "正则流式搜索。优先替代 grep/rg/Select-String。按文件稳定分组，支持文本和 DOCX/PDF 正文，output 可选 files/count/matches/snippets；截断时返回 next_offset。",
    "parameters": {
      "budget": {
        "description": "最终响应最多字节数，默认 16384",
        "type": "integer"
      },
      "context": {
        "description": "上下文行数（最多 5）；matches 默认 0，snippets 默认 2",
        "type": "integer"
      },
      "expand": {
        "description": "expand=block：把每条命中展开成它所在的完整代码块（花括号配平）。省掉「grep 拿行号 → fs_read block=true 再读一遍」那一轮；只在 matches/snippets 生效，预算不足时降级为普通命中行",
        "type": "string",
        "enum": [
          "block"
        ]
      },
      "glob": {
        "description": "只看匹配的文件，如 **/*.ts",
        "type": "string"
      },
      "ignore_case": {
        "type": "boolean"
      },
      "limit": {
        "description": "本页上限：files 按文件，matches/snippets 按命中，count 显式设置时按命中；默认 200（count 省略时完整统计），必须大于 0；预算才是真正的输出上限",
        "type": "integer"
      },
      "max_file_size": {
        "description": "只搜索不超过该字节数的文件，默认 8388608（8 MiB）；0 表示不限制。被跳过的文件会在尾注报告",
        "type": "integer"
      },
      "max_line": {
        "description": "命中行最多显示字符数，默认 240（与 fs_read 一致）；0 表示不截断，一次拿到完整长行，省掉再 fs_read 一轮",
        "type": "integer"
      },
      "max_per_file": {
        "description": "每个文件最多返回的命中数，默认不限制；省略部分会在尾注说明",
        "type": "integer"
      },
      "no_ignore": {
        "description": "忽略 .gitignore 与 node_modules/target 等默认跳过项；只在明确要搜依赖源码时用",
        "type": "boolean"
      },
      "offset": {
        "description": "files 跳过前 N 个匹配文件，其他模式跳过前 N 个命中；用于 next_offset 续取",
        "type": "integer"
      },
      "output": {
        "description": "files 每个匹配文件只给路径；count 给路径和真实行命中数（默认完整统计）；matches 给命中行；snippets 默认带 2 行上下文",
        "type": "string",
        "enum": [
          "files",
          "count",
          "matches",
          "snippets"
        ]
      },
      "parallel": {
        "description": "并行扫描；省略时自动按仓库规模选择：小仓库串行，大仓库并行；true 强制并行，false 强制串行；输出与串行一致",
        "type": "boolean"
      },
      "path": {
        "description": "搜索根目录，默认当前目录",
        "type": "string"
      },
      "pattern": {
        "description": "Rust regex 语法（不支持 lookaround 与反向引用，与 PCRE 不同）；JSON 字符串里反斜杠需双写",
        "type": "string",
        "required": true
      },
      "time_budget_ms": {
        "description": "文件与目录遍历总时间预算，默认 5000 毫秒",
        "type": "integer"
      }
    }
  },
  {
    "name": "fs_stat",
    "readOnly": true,
    "description": "查看路径元信息。优先替代 stat/ls/Get-Item。返回类型、大小、修改时间、换行风格、编码、内容指纹。path 或 paths 至少给一个；detail=server 是不带路径的自检，用来确认宿主连上的服务端版本与工具数。",
    "parameters": {
      "detail": {
        "description": "meta 只查元信息不读内容；hash 读一次给指纹；full 额外统计行数，默认 full；server 返回服务端版本与工具数，用于自检",
        "type": "string",
        "enum": [
          "meta",
          "hash",
          "full",
          "server"
        ]
      },
      "path": {
        "type": "string"
      },
      "paths": {
        "type": "array",
        "items": {
          "type": "string"
        }
      }
    }
  },
  {
    "name": "fs_exec",
    "description": "执行程序（argv 数组，不经过 shell）。替代 PowerShell 跑 cargo/git/npm/python：没有引号/插值、2>&1、ErrorRecord、$LASTEXITCODE 问题。非零退出不是错误（前缀写 exit=N 非零，要当失败使用 check=true）；超时会结束整个进程树。启动任务必须给 program。DSH 插件形态支持后台任务（background/job/list/wait_ms/kill）：长命令用 background=true 立即拿 job，再用 job=<id> wait_ms=<毫秒> 一次等到退出，期间可以继续做别的调用。",
    "parameters": {
      "args": {
        "description": "参数数组，每个参数独立，不拼字符串",
        "type": "array",
        "items": {
          "type": "string"
        }
      },
      "background": {
        "description": "会话内后台任务：立即返回 job id，用 job=<id> 轮询输出，list=true 列出全部；进程随 MCP 会话结束",
        "type": "boolean"
      },
      "check": {
        "description": "非零退出按 E_EXIT 错误返回；默认 false，非零仍是 ok",
        "type": "boolean"
      },
      "cwd": {
        "description": "工作目录，默认当前目录",
        "type": "string"
      },
      "env": {
        "description": "追加/覆盖环境变量，如 {\"RUST_BACKTRACE\":\"1\"}",
        "type": "json"
      },
      "job": {
        "description": "轮询/终止后台任务：传启动时返回的 job id",
        "type": "integer"
      },
      "kill": {
        "description": "配合 job 使用：结束该任务的整个进程树",
        "type": "boolean"
      },
      "list": {
        "description": "列出当前会话的全部后台任务（id/pid/状态/时长）",
        "type": "boolean"
      },
      "max_output": {
        "description": "每条流最多保留字节，默认 32768",
        "type": "integer"
      },
      "program": {
        "description": "可执行文件，如 cargo、git、python；走 PATH。启动新任务时必填，只轮询/终止 job 时可省略",
        "type": "string"
      },
      "shell": {
        "description": "显式走 cmd /c（需要管道/重定向时才用，会回到文本解析）",
        "type": "boolean"
      },
      "stdin": {
        "description": "传给进程的标准输入",
        "type": "string"
      },
      "timeout_ms": {
        "description": "超时毫秒，默认 120000，最大 600000；超时结束进程树",
        "type": "integer"
      },
      "wait_ms": {
        "description": "配合 job：最多等待 N 毫秒直到任务退出再返回（上限 60000）；超时仍运行则返回增量输出",
        "type": "integer"
      }
    }
  },
  {
    "name": "fs_file",
    "description": "文件管理：copy/move/remove/mkdir/convert/archive/extract。替代 PowerShell 的 Copy-Item/Move-Item/Remove-Item/New-Item 与压缩解压。必填映射：copy/move/archive/extract 要 src+dst；remove/mkdir/convert 要 path；remove 必须再给 confirm=true。convert 需 encoding；只改换行也要给 encoding（如 utf-8）。",
    "parameters": {
      "confirm": {
        "description": "remove 必须显式传 true",
        "type": "boolean"
      },
      "dry_run": {
        "description": "只报告不落盘",
        "type": "boolean"
      },
      "dst": {
        "description": "copy/move 的目标；可以是目录",
        "type": "string"
      },
      "encoding": {
        "description": "convert 目标编码：utf-8/utf-8-bom/gbk/gb18030/utf-16le/utf-16be",
        "type": "string"
      },
      "eol": {
        "description": "convert 目标换行",
        "type": "string",
        "enum": [
          "lf",
          "crlf"
        ]
      },
      "op": {
        "description": "操作类型",
        "type": "string",
        "enum": [
          "copy",
          "move",
          "remove",
          "mkdir",
          "convert",
          "archive",
          "extract"
        ],
        "required": true
      },
      "overwrite": {
        "description": "copy/move 覆盖已存在目标",
        "type": "boolean"
      },
      "path": {
        "description": "remove/mkdir/convert 的目标",
        "type": "string"
      },
      "recursive": {
        "description": "remove 删除目录时必须为 true",
        "type": "boolean"
      },
      "src": {
        "description": "copy/move 的源",
        "type": "string"
      }
    }
  }
]
