# fsops

[![CI](https://github.com/tokenis2023/agent-ops/actions/workflows/ci.yml/badge.svg)](https://github.com/tokenis2023/agent-ops/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/tokenis2023/agent-ops)](https://github.com/tokenis2023/agent-ops/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Windows](https://img.shields.io/badge/platform-Windows%2010%2F11-blue.svg)](#兼容性)

**面向 AI Agent 的 Windows 文件操作插件。**

fsops 同时提供 **MCP 服务**和 **CLI**，把 Windows 上最容易让 Agent 陷入重试循环的四类问题收进内核：

- 路径：POSIX、WSL、原生盘符、家目录、超长路径
- 换行：CRLF / LF / CR / 混合换行
- 编码：UTF-8、UTF-8 BOM、GBK、GB18030、UTF-16
- 写入：原子替换、并发指纹、跨进程写锁

它不替代系统管理：进程、服务、端口、注册表和 ACL 仍交给 Windows 自带工具；除此之外，Agent 的文件读写、搜索、编辑、复制/移动/删除、压缩解压和命令启动都由 fsops 接管。

> **定位边界：** 文件操作和命令启动走 fsops；进程状态、服务、端口、注册表仍用系统工具。
>
> **安全提示：** fsops 没有路径沙箱，权限等于启动它的用户权限；`fs_file remove` 必须显式 `confirm=true`。接入前请阅读 [安全声明](SECURITY.md)。

## 快速开始

### 一行安装（推荐）

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "& ([scriptblock]::Create([Text.Encoding]::UTF8.GetString((New-Object Net.WebClient).DownloadData('https://github.com/tokenis2023/agent-ops/releases/latest/download/install.ps1')).TrimStart([char]0xFEFF)))"
```

安装脚本会：

- 下载最新 Windows x64 Release
- 校验 SHA-256
- 安装到 `%LOCALAPPDATA%\fsops`
- 加入用户 `PATH`
- 自动注册检测到的 Claude Code、Hermes 和 Codex

升级：`fsops upgrade`（查最新 Release → SHA-256 校验 → 就地替换），或重跑同一行安装脚本。

装完后在项目根目录执行 `fsops integrate --rules`，让 Agent 优先选择 fsops。

跳过宿主注册或 PATH 修改：

```powershell
& ([scriptblock]::Create([Text.Encoding]::UTF8.GetString((New-Object Net.WebClient).DownloadData('https://github.com/tokenis2023/agent-ops/releases/latest/download/install.ps1')).TrimStart([char]0xFEFF))) -Client none -NoPath
```

### 手动安装

1. 从 [最新 Release](https://github.com/tokenis2023/agent-ops/releases/latest) 下载 Windows 二进制。
2. 重命名为 `fsops.exe`，放入 `PATH` 中的固定目录。
3. 验证：

```bash
fsops version
```

4. 接入宿主：

```bash
# Claude Code
claude mcp add fsops -- fsops mcp

# Hermes
hermes mcp add fsops --command fsops --args mcp
```

Cursor / Windsurf / Cline：

```json
{
  "mcpServers": {
    "fsops": {
      "command": "fsops",
      "args": ["mcp"]
    }
  }
}
```

Codex（`~/.codex/config.toml`）：

```toml
[mcp_servers.fsops]
command = "fsops"
args = ["mcp"]
```

修改配置后新开会话，或执行宿主提供的 `/reload-mcp`。

完整安装、校验、升级和排障见 **[使用指南](docs/使用指南.md)**。

## DeepSeek Harness 插件

仓库根同时是一个标准 DSH bundle：`package.json` 的 `dsh.bundle.patch` 指向 `cordis.patch.yml`，
patch 插入 `dsh/index.js` 这个 Host 插件。在 Harness 的 **插件 → 添加插件 → 安装第三方插件** 里
粘贴仓库地址即可；**生产环境建议用 tag/commit 固定版本**，不要跟随默认分支：

```text
github:tokenis2023/agent-ops#v0.9.1
```

安装后在会话里调用 `fs_stat`（`detail=server` 可免路径）自检：应返回与插件同版本的
`fsops <版本> tools=9`；二进制版本不符时插件会报 `E_VERSION` 并点名实际版本。

安装后默认启用，提供 9 个 `fs_*` 工具，与 MCP 形态同名、同错误码。插件**复用一条长驻 `fsops mcp` 会话**（换行分隔 JSON-RPC、按 id 匹配响应），不是每次调用新起进程：release 构建实测单次工具调用 **~34ms → 0.12–0.31ms**（`fs_stat` 0.12 / `fs_read` 0.31 / `fs_grep` 1.6，4 路并发一轮 0.23ms）。会话不可用（spawn 失败、进程退出、初始化失败）时自动退回一次性 `fsops call`，功能不降级；想强制走一次性调用可配 `mcp: false`。取消（abort）单个进行中的写/执行请求会结束长驻会话以阻止后台继续执行，下一次调用自动重建。参数集合与 MCP 形态一致，**包括 `fs_exec` 的后台任务**（`background`/`job`/`list`/`wait_ms`/`kill`）：长驻会话本来就是承载后台任务的最佳位置，长命令用 `background=true` 拿 job 再 `wait_ms` 等结果，期间不阻塞其它调用。注意 job id 绑在当前工作目录的会话上，切目录或插件重载后失效。

- **二进制来源**：优先用 `%LOCALAPPDATA%\fsops` 或 PATH 上已有的 `fsops.exe`；都没有时按本仓库版本
  从 GitHub Release 下载并校验 SHA-256（该版本还没发 Release 时回退到 latest），缓存在
  `%LOCALAPPDATA%\fsops-dsh\<版本>\`。
  **会核对版本**：`%LOCALAPPDATA%\fsops` 与 PATH 上的二进制是任意旧版本，解析时跑一次
  `fsops version` 与插件版本比对；不一致则优先用版本匹配的缓存/下载，都不行时直接报
  `E_VERSION`（点名实际找到的版本），不再静默用旧内核。包内 `bin/` 与按版本命名的缓存不探测。
- **离线或自定义**：在 profile 的 `cordis.patch.yml` 里按 `fsops-tools` 覆盖配置
  `binary: D:\tools\fsops.exe`（显式指定即直接采用，不做事后版本裁决），或设环境变量 `FSOPS_BIN`，
  或用 `autoDownload: false` 关闭下载。确认就要用旧版本时设 `allowVersionMismatch: true`
  或环境变量 `FSOPS_ALLOW_VERSION_MISMATCH=1`。
- **平台**：`dsh.bundle.platforms` 声明为 `win32`，非 Windows 宿主不会展示也不允许启用。
- **安全边界**：与 MCP 接入一致——fsops 没有路径沙箱，权限等于 Harness 进程用户。注意 DSH 插件是
  Host 代码，Harness 的 fs 沙箱与审批只约束内置文件工具和受限 shell，**不覆盖本插件的 `fs_*`**；
  需要强制隔离时请改用 MCP 形态并配合宿主沙箱，或只在可信工作区启用本插件。
- **错误分层**：DSH 会先按 schema 校验参数（type/enum），失败时是英文 `invalid arguments: ...`；
  通过后才进入 fsops 内核，返回中文 `E_*`。Agent 需要识别这两种错误形态。
- **兼容声明**：`engines.dsh` 声明兼容 DSH `>=0.2.2-rc.1`（当前加载器不强制校验）。

本地验证插件（桩替代 Harness 服务，真跑 fsops.exe）：

```bash
cargo build
node tests/dsh_plugin_smoke.mjs --exe target/debug/fsops.exe
```

**工具定义只有 Rust 一份真源**：`node scripts/gen_dsh_specs.mjs` 把 `fsops schema` 投影成
`dsh/tool-specs.generated.js`（DSH 的 value-schema DSL 与 MCP schema 不是同一套关键字，
转换规则在生成器里）。改了 `src/tools.rs` 的工具定义或描述后要重新生成；冒烟测试会逐字比对，
漂移直接红灯（手写三份时漂移过 11 处，评审时仍有 5/8 个工具描述不一致）。

## 能力声明

| 能力 | 说明 |
|---|---|
| 双形态 | MCP stdio 服务 + Unix 风格 CLI |
| DSH 插件 | 仓库根即标准 DSH bundle；插件管理里粘贴仓库地址即可安装 |
| 插件延迟 | 复用长驻 `fsops mcp` 会话，单次工具调用 **0.12–0.31ms**（`fs_stat` 0.12 / `fs_read` 0.31 / `fs_grep` 1.6）；一次性 spawn 约 34ms。IPC 地板 `ping` 约 44µs |
| 单文件分发 | Rust 编译的独立 Windows 可执行文件，无 Node/Python 运行时依赖 |
| 文本编码 | UTF-8 / BOM / GBK / GB18030 / UTF-16 自动识别并原编码写回 |
| 换行保持 | 匹配层归一 LF，写入层还原原文件 CRLF/LF/CR |
| 长路径 | 自动添加 Windows 长路径前缀 |
| 原子写入 | 临时文件 + 落盘同步 + 原子替换 |
| 并发保护 | 内容 hash + 跨进程命名互斥锁 |
| 搜索 | `.gitignore` 感知、流式扫描、分页、并行、时间预算 |
| Token 控制 | 行号窗口、字节预算、紧凑输出、续读参数 |
| 大文件 | >32 MB 文本按连续行区间流式读取；默认 `scan=window` 只扫请求窗口，`full` 才全扫给 hash/总行数 |
| 可移植性 | 仓库和文档不包含开发机绝对路径或环境配置 |

## 兼容性

| 项目 | 要求 |
|---|---|
| 操作系统 | Windows 10 / 11 x64 |
| 运行时 | 无额外运行时 |
| MCP | stdio JSON-RPC 2.0 |
| 宿主 | Claude Code、Codex、Cursor、Windsurf、Cline、Hermes、自研 harness |
| 构建 | Rust 1.88+；GNU 工具链需 MinGW，MSVC 使用系统链接器 |
| 测试 | Python 3.10+（仅端到端测试） |

## 工具面

<!-- tools:begin -->

9 个工具，故意做窄。

| 工具 | 用途 | 参数 |
|---|---|---|
| `fs_read` | 读文件、行区间、多文件、多文件异构区间、按模式取块、大文件窗口、结构摘要、DOCX/PDF 正文 | `block`、`budget`、`end`、`focus`、`force`、`max_line`、`path`、`paths`、`ranges`、`reads`、`scan`、`start`、`summary`、`tail` |
| `fs_edit` | 精确替换、正则替换、diff 自查、多文件批量、并发保护 | `diff`、`dry_run`、`edits`、`expect_hash`、`files`、`normalize_eol`、`path` |
| `fs_write` | 新建或整体覆盖，原子写入，可生成 DOCX；多文件用 files 一次提交 | `content`、`dry_run`、`expect_hash`、`files`、`normalize_eol`、`path` |
| `fs_glob` | 按 glob 找文件，尊重 .gitignore，带遍历时间预算，可输出 repo map | `budget`、`detail`、`dirs`、`focus`、`limit`、`map`、`max_files`、`no_ignore`、`offset`、`path`、`pattern`、`time_budget_ms` |
| `fs_inspect` | 一次看清目录：文件规模分布 + 可选正则的命中分布与样例，替代「glob → stat → grep」多轮探索 | `budget`、`focus`、`limit`、`no_ignore`、`path`、`pattern`、`time_budget_ms` |
| `fs_grep` | 正则搜索、四种输出、分页、并行，支持 DOCX/PDF 正文；首行带命中分布摘要 | `budget`、`context`、`expand`、`glob`、`ignore_case`、`limit`、`max_file_size`、`max_line`、`max_per_file`、`no_ignore`、`offset`、`output`、`parallel`、`path`、`pattern`、`time_budget_ms` |
| `fs_stat` | 元信息、编码、换行、hash；detail=server 一条调用自检服务端版本与工具数 | `detail`、`path`、`paths` |
| `fs_exec` | 执行程序（argv 数组，不经过 shell），支持后台任务；替代 PowerShell 跑 cargo/git/npm | `args`、`background`、`check`、`cwd`、`env`、`job`、`kill`、`list`、`max_output`、`program`、`shell`、`stdin`、`timeout_ms`、`wait_ms` |
| `fs_file` | 文件管理：copy/move/remove/mkdir/convert/archive/extract | `confirm`、`dry_run`、`dst`、`encoding`、`eol`、`op`、`overwrite`、`path`、`recursive`、`src` |

<!-- tools:end -->

完整参数、输出和错误码见 **[工具参考](docs/工具参考.md)**。

## Agent 使用引导

在项目根目录写入受管规则块：

```bash
fsops integrate --rules
```

默认写入 `AGENTS.md` 和 `CLAUDE.md`；需要覆盖更多宿主时：

```bash
fsops integrate --rules --target all
```

支持 `cursor`、`windsurf`、`cline`、`gemini`、`copilot`。规则块可重复执行，不会覆盖用户内容；用 `--remove` 移除，用 `--check` 检查版本。

检查接入状态：

```bash
fsops doctor
fsops doctor --json
```

`doctor` 会报告规则文件、MCP 宿主注册状态、Claude Code hook 和当前引导级别。如果宿主把 fsops 放在 deferred 目录，先执行一次工具发现，或直接用 `fsops read/grep/glob/stat/call` CLI。注意：规则是**软引导**；Claude Code 可用 `harden` 做硬接管：

```bash
fsops harden claude
fsops harden claude --check
fsops harden claude --remove
```

`harden` 安装 `PreToolUse` hook，拦截文本文件、DOCX 和 PDF 的 `Read/Grep/Glob`；图片、旧版 `.doc`、压缩包等二进制仍放行内置 `Read`。默认写项目 `.claude/settings.json`，加 `--user` 写用户级配置。

详细规则见 [AGENTS.md](AGENTS.md)。推荐链路：

```text
fs_inspect（陌生仓库；大仓库可用 repo map）
  → fs_grep output=files / snippets
  → fs_read ranges / reads（DOCX/PDF 正文同样走 fs_read）
  → fs_edit edits/files + expect_hash + diff=true
     └ 只知道文本形状：pattern/replacement 正则替换，不必先抄原文
  → 相关测试（长构建/测试用 fs_exec 后台任务）
  → 项目规定的最终检查
```

关键规则：

- 局部修改只用 `fs_edit`，不要读全文后 `fs_write`。
- 只有图片和不支持的二进制回退宿主；DOCX/PDF 正文由 `fs_read` 读取。
- 多文件修改一次批量提交。
- 截断时使用返回的 `next_offset` 或 `ranges`；分页不是快照，并发变化的目录不承诺无遗漏。
- `E_STALE` 先核对内容再决定是否带新 hash；`E_AMBIGUOUS` 先加上下文或限定范围；`E_LOCKED` 连续两次仍被占就停止并报告。
- 混合换行和编码保护错误不要绕过。

## 为什么需要它

| 摩擦 | 典型表现 | fsops 的处理 |
|---|---|---|
| 路径 | Agent 写 `/d/repo/x`、`~/x`，PowerShell 报“找不到路径” | 四种写法都认，超长路径自动处理 |
| 换行 | 仓库是 CRLF，模型按 LF 匹配，`old_string` 永远对不上 | 匹配前归一，写回时还原 |
| 编码 | 中文 Windows 上 GBK、UTF-16、BOM 混用 | 自动识别并按原编码写回 |
| 调用成本 | 整文件读、整文件写回，token 翻倍 | 只给窗口和行号，编辑只返回变更位置 |

### 与 PowerShell 的边界

fsops 可以替代 Agent 对 `Get-Content`、`Set-Content`、`Select-String`、`Get-ChildItem`、`Test-Path`、`Copy-Item`、`Move-Item`、`Remove-Item`、`New-Item`、`Compress-Archive`、`Expand-Archive` 的依赖，也能用 `fs_exec` 直接启动 cargo/git/npm/python，不必再拼命令行字符串。**不替代**：

- 进程、服务、端口、注册表、ACL
- Git、构建、测试、包管理的语义（`fs_exec` 只是启动器）
- 图片和二进制内容解析

推荐分工：**文件和命令启动交给 fsops，进程与服务状态用系统工具，PowerShell 只做 Windows 原生兜底。**

## 实测

只统计**工具输出**（模型必须读进上下文的量），baseline 用宿主原生做法：整文件读、整文件写回、`grep -rn`、`find`。数字可用 `python tests/bench.py` 直接复跑。

**内置 fixture**（40 个文件 / 每文件 36 行，单位：字符）：

| 场景 | baseline | fsops | 省 |
|---|---:|---:|---:|
| 列出命中文件 | 2,829 | 509 | 82.0% |
| 定位单行（只要位置） | 99 | 57 | 42.4% |
| 定位 + 4 行上下文 | 499 | 177 | 64.5% |
| 读前 10 行 | 944 | 374 | 60.4% |
| 改 3 处（读全文+写回 vs 单次 edit） | 1,888 | 317 | 83.2% |
| 列出全部文件 | 2,829 | 509 | 82.0% |
| **合计** | **9,088** | **1,943** | **78.6%** |

**真实仓库**（fsops 自身源码）：

| 场景 | baseline | fsops | 省 |
|---|---:|---:|---:|
| 列出命中文件 | 28 | 17 | 39.3% |
| 定位单行（只要位置） | 76 | 78 | **−2.6%** |
| 定位 + 4 行上下文 | 357 | 243 | 31.9% |
| 读前 10 行 | 4,369 | 399 | **90.9%** |
| 改 3 处 | 8,738 | 164 | **98.1%** |
| 列出全部文件 | 501 | 188 | 62.5% |
| **合计** | **14,069** | **1,089** | **92.3%** |

### 轮次（比输出更影响上下文体积）

大部分成本来自每轮重发的上下文（含约 6.5k token 的常驻 schema）：
一次工具输出只有几百 token，一轮上下文是几万 token，所以先减轮次、再减输出。
这不等于账单：实际费用还看输入 cache 命中、输出单价与供应商计价；单次输出也会进入后续上下文。

真实 Agent 对照实测（同模型、同任务、同 fixture，唯一变量是工具是否可用）：

| | 有 `fs_inspect` | 无 `fs_inspect` |
|---|---|---|
| 调用次数 | **4** | **5** |
| 失败次数 | 0 | 0 |

`fs_edit` 顶层简写修复前后：v0.8.0 两侧各有 1 次参数失败 → v0.9.0 **0 失败**。
完整方法、数据与局限见 **[轮次实测](docs/轮次实测.md)**。

结论：**读得少、改得省**是主要收益。纯定位单行时 fsops 可能持平甚至略亏（工具输出带路径头），这类查询直接给 `pattern` 已经足够，不必期待零成本。

**插件调用延迟**（DSH 插件形态，Node 侧观测的墙钟时间）：

| 场景 | 每个调用新起进程 | 复用长驻会话 |
|---|---:|---:|
| `fs_read`（5 行） | 33.5 ms | **0.54 ms** |
| `fs_stat`（meta） | 32.9 ms | **0.34 ms** |
| `fs_grep`（count） | — | 1.8 ms |
| 4 路并发（4 次总耗时） | — | **0.59 ms** |

一次性 spawn 的 33ms 拆开看：Windows 进程创建 ~9ms、给隐藏 console 子进程新建 conhost ~17ms、镜像加载与 Rust 初始化 ~6ms，**工具本身 <0.5ms**（`fsops version` 与 `fsops call fs_stat` 几乎同耗时）。所以优化只能靠复用进程，压 Rust 代码没有收益；长驻会话把这块固定成本摊销到一次。

复跑：

```bash
python tests/bench.py                  # 自带 fixture，无需外部仓库、无需 tiktoken
python tests/bench.py /path/to/repo    # 额外在真实代码上跑一轮
node tests/bench_dsh_latency.mjs       # 插件调用延迟：一次性 spawn vs 长驻会话
```

## 安全模型

- 无路径沙箱
- 无网络请求
- 无遥测
- 不跟随目录符号链接
- `fs_file remove` 必须 `confirm=true`，拒绝盘符根目录；`extract` 拒绝 zip-slip，解压上限 512 MB
- 写入使用原子替换和跨进程锁
- Release 附带 SHA-256

详见 [SECURITY.md](SECURITY.md)。

## 已知限制

- DOCX 只读取正文；`fs_write` 可生成纯文本段落的 DOCX，但不保留复杂版式、图片、批注。
- PDF 只读取正文；扫描版（无文本层）和加密 PDF 无法抽取，`fs_write`/`fs_edit` 对 PDF 明确拒绝。
- 图片、旧版 `.doc` 和二进制文件不解析：`fs_read`/`fs_edit`/`fs_write` 返回 `E_BINARY`，`fs_stat` 标记 `binary`。UTF-16 文本不会被误判。
- 超大 UTF-16 文件暂不支持流式读取。
- 超大文件只支持一个连续 `start`/`end` 区间，不支持多区间；默认 `scan=window` 只做窗口扫描，需要全文 hash/总行数时用 `scan=full`。
- `fs_edit` 遇到混合换行默认拒绝，必须显式 `normalize_eol=true`。
- `fs_edit` 的 `old` 是子串匹配（不要求整行）：只抄半行可能留下残句，尽量给完整行或更多上下文。
- CR-only（裸 `\r`）文本的行号统计不一致：`fs_stat`/`fs_read` 按 LF 统计、`fs_edit` 按 CR 分行；处理前建议先用 `fs_file` 的 `op=convert` 转成 LF/CRLF。
- 多文件编辑不是跨文件事务，写入阶段失败会返回 `E_BATCH_PARTIAL`。
- `fs_edit` 的摘要只列前 64 处落点，其余用总数表示。
- 编码识别是启发式的；极短 GBK 文件可能被判为 UTF-8-lossy。
- 并行搜索（强制并行或 Auto 满波后）为保证稳定顺序，达到 limit 后可能多扫描当前波次。

## 文档

- [使用指南](docs/使用指南.md)：安装、宿主接入、工作流、错误码、升级卸载
- [工具参考](docs/工具参考.md)：全部参数、输出和协议
- [轮次实测](docs/轮次实测.md)：工具改造对调用次数的影响（真实对照）+ 延迟逐层分解（Rust 层压到哪一步）
- [Agent 规则](AGENTS.md)：给编码 Agent 的路由与安全规则
- [安全声明](SECURITY.md)：权限模型、写入安全、漏洞报告
- [变更记录](CHANGELOG.md)：版本历史
- [参与开发](CONTRIBUTING.md)：构建、测试、发布流程

## 构建

```bash
cargo build --release
cargo build
cargo test
python tests/windows_semantics.py
```

或：

```bash
bash scripts/build.sh all
```

Windows 上如果 MCP 宿主仍持有 `fsops.exe`，构建前停止子进程：

```powershell
Get-Process fsops -ErrorAction SilentlyContinue | Stop-Process -Force
```

## 开发

- Rust 2021
- 行为修改必须先写失败测试
- CI：格式、Clippy、单元测试、真实 MCP 端到端

见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

[MIT](LICENSE)
