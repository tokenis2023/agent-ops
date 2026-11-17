/**
 * 本文件由 scripts/gen_dsh_specs.mjs 从 `fsops schema` 生成，请勿手工编辑。
 * 改了 src/tools.rs 的工具定义后，跑一次该脚本重新生成。
 */
export const TOOL_SPECS = [
  {
    "name": "fs_read",
    "readOnly": true,
    "description": "读取文件，优先替代宿主 read_file/cat/head/tail/Get-Content。常用：单文件 `path`；多文件异构 `reads=[{path,start,end}]`（一次读完，勿一文件一轮）；按正则取整块 `focus=<正则>`（省 grep→block 两步）；日志尾 `tail=N`。模式表：`path` 与 `paths` 是并集（共用 start/end）；`focus`/`summary` 仅单文件，`reads`/`focus`/`tail`/`summary` 各有互斥范围（读对应参数说明，区间写在 reads 项内）；默认 32KB 并给续读参数；支持 DOCX/PDF、GBK/UTF-16/BOM/CRLF；>32MB 用 scan=window（默认）/full。",
    "parameters": {
      "block": {
        "description": "展开包含 start 行的整个代码块（配合 fs_grep 行号）",
        "type": "boolean"
      },
      "budget": {
        "description": "最多返回字节，默认 32768；≤32KB 且未指定时读全文",
        "type": "integer"
      },
      "end": {
        "description": "结束行（含）；用 block=true 时可省略",
        "type": "integer"
      },
      "focus": {
        "description": "只返回命中所在的完整代码块；数据文件只给命中行 ±2 行；JSON 里反斜杠双写",
        "type": "string"
      },
      "force": {
        "description": "读取缓存默认关闭；开启（FSOPS_MCP_CACHE=1）时用本参数强制返回正文",
        "type": "boolean"
      },
      "max_line": {
        "description": "每行最多字符，默认 240；0 不截断",
        "type": "integer"
      },
      "path": {
        "description": "文件路径，如 /d/repo/app/src/main.rs",
        "type": "string"
      },
      "paths": {
        "description": "多文件、共用 start/end，最多 20 个",
        "type": "array",
        "items": {
          "type": "string"
        }
      },
      "ranges": {
        "description": "单文件多段区间，如 [[1,2],[8,9]]；优先于 start/end",
        "type": "array",
        "items": {
          "type": "array",
          "items": {
            "type": "integer"
          }
        }
      },
      "reads": {
        "description": "多文件异构批量：项内支持 path/start/end/ranges/block/max_line/scan/budget/focus（语义同顶层）；与顶层 path/paths/ranges/start/end/block/summary/focus 互斥",
        "type": "array",
        "items": {
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "block": {
              "type": "boolean"
            },
            "budget": {
              "type": "integer"
            },
            "end": {
              "type": "integer"
            },
            "focus": {
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
        "description": ">32MB 专用：window 默认只读区间并探测是否还有内容（不给全文 hash/行数）；full 全扫给精确值",
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
        "description": "结构摘要：声明行+行号，不展开函数体；数据文件与超大文件不支持",
        "type": "boolean"
      },
      "tail": {
        "description": "只读最后 N 行（日志场景）；不足 N 行读全部",
        "type": "integer"
      }
    }
  },
  {
    "name": "fs_edit",
    "description": "精确替换（old→new）或正则替换（pattern→replacement）：不知道原文时用 pattern，省一次 fs_read。两套二选一，同时给会报 E_BAD_EDIT。替代 patch/Edit/apply_patch。同文件多处用 edits，多文件用 files；预检通过才写。最小形状 {path, edits:[{old, new}]}；结构改动用 diff=true 自查。自动处理 CRLF/LF 并保留原编码；DOCX 不支持精确替换，改用 fs_write。",
    "parameters": {
      "diff": {
        "description": "附带改动片段（@@ 行号 -原文 +新文）；省一次回读",
        "type": "boolean"
      },
      "dry_run": {
        "description": "只校验不落盘",
        "type": "boolean"
      },
      "edits": {
        "description": "单文件修改列表",
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
              "description": "结束行（含）；省略到末尾",
              "type": "integer"
            },
            "new": {
              "description": "替换内容，空串删除；与 replacement 二选一",
              "type": "string"
            },
            "old": {
              "description": "原文（需唯一，除非 all=true）；与 pattern 二选一",
              "type": "string"
            },
            "pattern": {
              "description": "正则：替换所有匹配，$1 引用捕获组；不知道原文时用它",
              "type": "string"
            },
            "replacement": {
              "description": "正则替换内容",
              "type": "string"
            },
            "start": {
              "description": "只改第 start..=end 行（1 基）；配合 all 限定函数内",
              "type": "integer"
            }
          }
        }
      },
      "expect_hash": {
        "description": "并发保护：文件已变则拒绝",
        "type": "string"
      },
      "files": {
        "description": "多文件批量；预检通过才写，最多 50",
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
        "description": "混合换行时显式归一为主风格（平局取 LF）",
        "type": "boolean"
      },
      "path": {
        "description": "单文件路径；批量用 files",
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
        "description": "多文件批量；预检通过才写，最多 50；与 path/content 互斥",
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
        "description": "显式归一换行风格；已有稳定文件始终沿用原风格",
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
        "description": "响应最多字节，默认 16384",
        "type": "integer"
      },
      "detail": {
        "description": "附大小与修改时间",
        "type": "boolean"
      },
      "dirs": {
        "description": "是否包含目录",
        "type": "boolean"
      },
      "focus": {
        "description": "map 的关注词，空格分隔",
        "type": "string"
      },
      "limit": {
        "description": "最多返回条数，默认 200",
        "type": "integer"
      },
      "map": {
        "description": "输出 repo map（按符号引用重要性排序）",
        "type": "boolean"
      },
      "max_files": {
        "description": "map 最多扫描的文件数，默认 2000",
        "type": "integer"
      },
      "no_ignore": {
        "description": "忽略 .gitignore 与 node_modules/target 等默认跳过项；只在明确要查依赖源码或构建产物时用",
        "type": "boolean"
      },
      "offset": {
        "description": "跳过前 N 条，用于续页",
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
        "description": "遍历时间预算，默认 5000 毫秒",
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
        "description": "可选关注词（空格分隔）：影响最相关文件排序",
        "type": "string"
      },
      "limit": {
        "description": "列出的文件数，默认 15",
        "type": "integer"
      },
      "no_ignore": {
        "description": "忽略 .gitignore 与 node_modules/target 等默认跳过项；只在明确要统计依赖源码时用",
        "type": "boolean"
      },
      "path": {
        "description": "根目录，默认当前目录",
        "type": "string"
      },
      "pattern": {
        "description": "可选正则：附带命中分布与样例",
        "type": "string"
      },
      "time_budget_ms": {
        "description": "遍历时间预算，默认 3000 毫秒；超时返回已扫到的部分",
        "type": "integer"
      }
    }
  },
  {
    "name": "fs_grep",
    "readOnly": true,
    "description": "正则流式搜索，优先替代 grep/rg/Select-String。按文件分组，支持 DOCX/PDF，output=files/count/matches/snippets；截断返回 next_offset。count 默认完整统计；显式给 limit 时按命中截断并标“本页可见”，不是总数。最小形状：{pattern, path}；分页参数是 output/limit/offset。",
    "parameters": {
      "budget": {
        "description": "响应最多字节，默认 16384",
        "type": "integer"
      },
      "context": {
        "description": "上下文行数（最多 5）；matches 默认 0，snippets 默认 2",
        "type": "integer"
      },
      "expand": {
        "description": "expand=block：把命中展开成完整代码块（配平花括号），省掉再 fs_read 一轮",
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
        "description": "本页上限（files 按文件，其他按命中），默认 200；count 省略时完整统计",
        "type": "integer"
      },
      "max_file_size": {
        "description": "只搜不超过该字节数的文件，默认 8 MiB；0 不限制",
        "type": "integer"
      },
      "max_line": {
        "description": "命中行最多字符，默认 240；0 不截断",
        "type": "integer"
      },
      "max_per_file": {
        "description": "每文件命中上限，默认不限；省略部分在尾注说明",
        "type": "integer"
      },
      "no_ignore": {
        "description": "忽略 .gitignore 与 node_modules/target 等默认跳过项；只在明确要搜依赖源码时用",
        "type": "boolean"
      },
      "offset": {
        "description": "跳过前 N 项（files 按文件，其他按命中），用于续页",
        "type": "integer"
      },
      "output": {
        "description": "files 只给路径；count 给每文件命中数；matches 给命中行；snippets 带 2 行上下文",
        "type": "string",
        "enum": [
          "files",
          "count",
          "matches",
          "snippets"
        ]
      },
      "parallel": {
        "description": "并行扫描；省略时按仓库规模自动选择",
        "type": "boolean"
      },
      "path": {
        "description": "搜索根目录，默认当前目录",
        "type": "string"
      },
      "pattern": {
        "description": "Rust regex（不支持 lookaround/反向引用）；JSON 里反斜杠双写",
        "type": "string",
        "required": true
      },
      "time_budget_ms": {
        "description": "遍历总时间预算，默认 5000 毫秒",
        "type": "integer"
      }
    }
  },
  {
    "name": "fs_stat",
    "readOnly": true,
    "description": "查看路径元信息。优先替代 stat/ls/Get-Item。返回类型、大小、修改时间、换行风格、编码、内容指纹。path 或 paths 至少给一个；detail=server 是不带路径的自检：服务端版本、工具数与当前目录。",
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
    "description": "执行程序（program + args 数组，不经过 shell；参数名是 args）。替代 PowerShell 跑 cargo/git/npm/python：无引号/插值/2>&1 问题。非零退出不算错误（要当失败用 check=true）；超时结束进程树。启动任务必须给 program。DSH 插件形态支持后台任务（background/job/list/wait_ms/kill）：长命令用 background=true 立即拿 job，再用 job=<id> wait_ms=<毫秒> 一次等到退出，期间可以继续做别的调用。",
    "parameters": {
      "args": {
        "description": "参数数组，每个参数独立，不拼字符串",
        "type": "array",
        "items": {
          "type": "string"
        }
      },
      "background": {
        "description": "后台任务：返回 job id，用 job 轮询；随会话结束",
        "type": "boolean"
      },
      "check": {
        "description": "非零退出按 E_EXIT 报错；默认 false",
        "type": "boolean"
      },
      "cwd": {
        "description": "工作目录，默认当前目录",
        "type": "string"
      },
      "env": {
        "description": "追加/覆盖环境变量",
        "type": "json"
      },
      "job": {
        "description": "轮询/终止 job：传 job id",
        "type": "integer"
      },
      "kill": {
        "description": "结束 job 的整个进程树",
        "type": "boolean"
      },
      "list": {
        "description": "列出会话内全部后台任务",
        "type": "boolean"
      },
      "max_output": {
        "description": "每条流最多保留字节，默认 32768",
        "type": "integer"
      },
      "program": {
        "description": "可执行文件（PATH 名或绝对路径）；启动任务时必填",
        "type": "string"
      },
      "shell": {
        "description": "走 cmd /c（管道/重定向时用）",
        "type": "boolean"
      },
      "stdin": {
        "description": "传给进程的标准输入",
        "type": "string"
      },
      "timeout_ms": {
        "description": "超时毫秒，默认 120000，上限 600000",
        "type": "integer"
      },
      "wait_ms": {
        "description": "配合 job：最多等 N 毫秒（上限 60000）",
        "type": "integer"
      }
    }
  },
  {
    "name": "fs_file",
    "description": "文件管理：copy/move/remove/mkdir/convert/archive/extract。替代 PowerShell 的 Copy-Item/Move-Item/Remove-Item 与压缩解压。copy/move/archive/extract 要 src+dst；remove/mkdir/convert 要 path；remove 必须 confirm=true。最小形状 {op:\"copy\", src, dst}；参数名是 op 不是 operation。convert 需 encoding（只改换行也要给 encoding）。",
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
        "description": "copy/move 目标，可为目录",
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
