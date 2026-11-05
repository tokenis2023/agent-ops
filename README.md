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

## 能力声明

| 能力 | 说明 |
|---|---|
| 双形态 | MCP stdio 服务 + Unix 风格 CLI |
| 单文件分发 | Rust 编译的独立 Windows 可执行文件，无 Node/Python 运行时依赖 |
| 文本编码 | UTF-8 / BOM / GBK / GB18030 / UTF-16 自动识别并原编码写回 |
| 换行保持 | 匹配层归一 LF，写入层还原原文件 CRLF/LF/CR |
| 长路径 | 自动添加 Windows 长路径前缀 |
| 原子写入 | 临时文件 + 落盘同步 + 原子替换 |
| 并发保护 | 内容 hash + 跨进程命名互斥锁 |
| 搜索 | `.gitignore` 感知、流式扫描、分页、并行、时间预算 |
| Token 控制 | 行号窗口、字节预算、紧凑输出、续读参数 |
| 大文件 | >32 MB 文本按连续行区间流式读取 |
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

6 个工具，故意做窄。

| 工具 | 用途 | 关键参数 |
|---|---|---|
| `fs_read` | 读文件、行区间、多文件、大文件窗口 | `path`/`paths`、`start`、`end`、`ranges`、`max_line`、`budget` |
| `fs_edit` | 精确替换、多文件批量、dry run、并发保护 | `edits`、`files`、`expect_hash`、`normalize_eol` |
| `fs_write` | 新建或整体覆盖，原子写入 | `content`、`expect_hash`、`normalize_eol` |
| `fs_glob` | 按 glob 找文件，尊重 `.gitignore` | `pattern`、`path`、`dirs`、`detail`、`limit`、`budget` |
| `fs_grep` | 正则搜索、四种输出、分页、并行 | `pattern`、`output`、`context`、`max_per_file`、`offset`、`parallel` |
| `fs_stat` | 元信息、编码、换行、hash | `path`/`paths`、`detail=meta/hash/full` |

完整参数、输出和错误码见 **[工具参考](docs/工具参考.md)**。

## Agent 使用引导

推荐把 [AGENTS.md](AGENTS.md) 中的规则复制到项目的 `AGENTS.md` 或 `.hermes.md`。

推荐链路：

```text
fs_grep output=files
  → fs_grep output=snippets
  → fs_read ranges
  → fs_edit edits/files + expect_hash
  → 仅在需要时复查
```

关键规则：

- 局部修改只用 `fs_edit`，不要读全文后 `fs_write`。
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
- PDF/Word/二进制内容

推荐分工：**fsops 管文件，shell 管进程和工具链，PowerShell 只做 Windows 原生兜底。**

## 实测

某真实 Go 仓库，122 个 Go 文件。只统计**工具输出** token（cl100k_base），baseline 使用宿主原生整文件读取、`grep -rn` 和 `find`。

| 场景 | baseline | fsops | 省 |
|---|---:|---:|---:|
| 读某文件 40–80 行 | 14,861 | 603 | **95.9%** |
| 找符号定义 | 27 | 26 | 3.7% |
| 正则搜索（限量） | 1,198 | 541 | 54.8% |
| 列出全部 `.go` | 862 | 739 | 14.3% |
| 改 3 处（读全文+写全文 vs 单次 edit） | 29,722 | 131 | **99.6%** |

结论：**读得少、改得省**是主要收益；纯搜索收益有限，不要期待 grep 变成零成本。

复跑：

```bash
python tests/bench.py /path/to/go/repo
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

- 只处理文本，不解析 PDF/Word/图片。
- 超大 UTF-16 文件暂不支持流式读取。
- 超大文件只支持一个连续 `start`/`end` 区间，不支持多区间。
- `fs_edit` 遇到混合换行默认拒绝，必须显式 `normalize_eol=true`。
- 多文件编辑不是跨文件事务，写入阶段失败会返回 `E_BATCH_PARTIAL`。
- 编码识别是启发式的；极短 GBK 文件可能被判为 UTF-8-lossy。
- 并行搜索为保证稳定顺序，达到 limit 后可能多扫描当前波次。

## 文档

- [使用指南](docs/使用指南.md)：安装、宿主接入、工作流、错误码、升级卸载
- [工具参考](docs/工具参考.md)：全部参数、输出和协议
- [设计说明](docs/设计.md)：工具面、预算、Windows 语义取舍
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
