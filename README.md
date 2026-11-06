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

它不替代 shell，也不提供删除/移动/复制；它专门替代 Agent 的文件读写、搜索和精确定位层。

> **定位边界：** 文件操作交给 fsops；构建、测试、Git、进程和包管理交给 shell。
>
> **安全提示：** fsops 没有路径沙箱，权限等于启动它的用户权限。接入前请阅读 [安全声明](SECURITY.md)。

## 快速开始

### 一行安装（推荐）

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://github.com/tokenis2023/agent-ops/releases/latest/download/install.ps1 | iex"
```

安装脚本会：

- 下载最新 Windows x64 Release
- 校验 SHA-256
- 安装到 `%LOCALAPPDATA%\fsops`
- 加入用户 `PATH`
- 自动注册检测到的 Claude Code、Hermes 和 Codex

装完后在项目根目录执行 `fsops integrate --rules`，让 Agent 优先选择 fsops。

跳过宿主注册或 PATH 修改：

```powershell
& ([scriptblock]::Create((irm https://github.com/tokenis2023/agent-ops/releases/latest/download/install.ps1))) -Client none -NoPath
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
粘贴仓库地址即可：

```text
https://github.com/tokenis2023/agent-ops
```

安装后默认启用，提供 6 个 `fs_*` 工具——与 MCP 形态同名、同契约、同错误码。

- **二进制来源**：优先用 `%LOCALAPPDATA%\fsops` 或 PATH 上已有的 `fsops.exe`；都没有时按本仓库版本
  从 GitHub Release 下载并校验 SHA-256（该版本还没发 Release 时回退到 latest），缓存在
  `%LOCALAPPDATA%\fsops-dsh\<版本>\`。
- **离线或自定义**：在 profile 的 `cordis.patch.yml` 里按 `fsops-tools` 覆盖配置
  `binary: D:\tools\fsops.exe`，或设环境变量 `FSOPS_BIN`，或用 `autoDownload: false` 关闭下载。
- **平台**：`dsh.bundle.platforms` 声明为 `win32`，非 Windows 宿主不会展示也不允许启用。
- **安全边界**：与 MCP 接入一致——fsops 没有路径沙箱，权限等于 Harness 进程用户；需要隔离时
  依赖 Harness 自己的沙箱策略或限定工作区。

本地验证插件（桩替代 Harness 服务，真跑 fsops.exe）：

```bash
cargo build
node tests/dsh_plugin_smoke.mjs --exe target/debug/fsops.exe
```

## 能力声明

| 能力 | 说明 |
|---|---|
| 双形态 | MCP stdio 服务 + Unix 风格 CLI |
| DSH 插件 | 仓库根即标准 DSH bundle；插件管理里粘贴仓库地址即可安装 |
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
| 构建 | Rust 1.75+；GNU 工具链需 MinGW，MSVC 使用系统链接器 |
| 测试 | Python 3.10+（仅端到端测试） |

## 工具面

8 个工具，故意做窄。

| 工具 | 用途 | 关键参数 |
|---|---|---|
| `fs_read` | 读文件、行区间、多文件、多文件异构区间、按模式取块、大文件窗口、结构摘要、DOCX/PDF 正文 | `path`/`paths`/`reads`、`start`、`end`、`ranges`、`focus`、`scan`、`summary`、`max_line`、`budget` |
| `fs_edit` | 精确替换、正则替换、diff 自查、多文件批量、并发保护 | `edits`（`old/new` 或 `pattern/replacement`）、`files`、`diff`、`expect_hash`、`normalize_eol` |
| `fs_write` | 新建或整体覆盖，原子写入，可生成 DOCX | `content`、`expect_hash`、`normalize_eol` |
| `fs_glob` | 按 glob 找文件，尊重 `.gitignore`，带遍历时间预算，可输出 repo map | `pattern`、`path`、`dirs`、`detail`、`limit`、`offset`、`time_budget_ms`、`budget`、`map`、`focus` |
| `fs_grep` | 正则搜索、四种输出、分页、并行，支持 DOCX/PDF 正文 | `pattern`、`output`、`context`、`max_per_file`、`max_file_size`、`offset`、`parallel` |
| `fs_stat` | 元信息、编码、换行、hash | `path`/`paths`、`detail=meta/hash/full` |
| `fs_exec` | 执行程序（argv 数组，不经过 shell），支持后台任务；替代 PowerShell 跑 cargo/git/npm | `program`、`args`、`cwd`、`timeout_ms`、`stdin`、`env`、`shell` |
| `fs_file` | 文件管理：copy/move/remove/mkdir/convert/archive/extract | `op`、`src`、`dst`、`path`、`confirm`、`overwrite`、`encoding`、`eol` |

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
fs_grep output=files
  → fs_grep output=snippets
  → fs_read ranges（DOCX/PDF 正文同样走 fs_read）
  → fs_edit edits/files + expect_hash
     └ 只知道文本形状：pattern/replacement 正则替换，不必先抄原文
  → fs_edit diff=true 自查改动（省一次全文回读）
```

关键规则：

- 局部修改只用 `fs_edit`，不要读全文后 `fs_write`。
- 只有图片和不支持的二进制回退宿主；DOCX/PDF 正文由 `fs_read` 读取。
- 多文件修改一次批量提交。
- 截断时使用返回的 `next_offset` 或 `ranges`。
- `E_STALE`、`E_LOCKED`、`E_AMBIGUOUS` 不要原样重试。
- 混合换行和编码保护错误不要绕过。

## 为什么需要它

| 摩擦 | 典型表现 | fsops 的处理 |
|---|---|---|
| 路径 | Agent 写 `/d/repo/x`、`~/x`，PowerShell 报“找不到路径” | 四种写法都认，超长路径自动处理 |
| 换行 | 仓库是 CRLF，模型按 LF 匹配，`old_string` 永远对不上 | 匹配前归一，写回时还原 |
| 编码 | 中文 Windows 上 GBK、UTF-16、BOM 混用 | 自动识别并按原编码写回 |
| 调用成本 | 整文件读、整文件写回，token 翻倍 | 只给窗口和行号，编辑只返回变更位置 |

### 与 PowerShell 的边界

fsops 可以替代 Agent 对 `Get-Content`、`Set-Content`、`Select-String`、`Get-ChildItem`、`Test-Path` 的依赖，但**不替代**：

- 进程、服务、端口、注册表、ACL
- Git、构建、测试、包管理
- 删除、移动、复制、压缩包
- 图片和二进制内容

推荐分工：**fsops 管文件，shell 管进程和工具链，PowerShell 只做 Windows 原生兜底。**

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

结论：**读得少、改得省**是主要收益。纯定位单行时 fsops 可能持平甚至略亏（工具输出带路径头），这类查询直接给 `pattern` 已经足够，不必期待零成本。

复跑：

```bash
python tests/bench.py                  # 自带 fixture，无需外部仓库、无需 tiktoken
python tests/bench.py /path/to/repo    # 额外在真实代码上跑一轮
```

## 安全模型

- 无路径沙箱
- 无网络请求
- 无遥测
- 不跟随目录符号链接
- 不提供删除/移动/复制
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
- 多文件编辑不是跨文件事务，写入阶段失败会返回 `E_BATCH_PARTIAL`。
- `fs_edit` 的摘要只列前 64 处落点，其余用总数表示。
- 编码识别是启发式的；极短 GBK 文件可能被判为 UTF-8-lossy。
- 并行搜索为保证稳定顺序，达到 limit 后可能多扫描当前波次。

## 文档

- [使用指南](docs/使用指南.md)：安装、宿主接入、工作流、错误码、升级卸载
- [工具参考](docs/工具参考.md)：全部参数、输出和协议
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
