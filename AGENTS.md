# Agent 使用规则

本文件既用于开发 fsops 仓库，也可以复制到任何希望固定使用 fsops 的项目根目录。

## 文件操作优先级

当 fsops MCP 工具可用时：

1. 读取文件：`fs_read`
2. 修改已有文件：`fs_edit`
3. 新建或整体覆盖文件：`fs_write`
4. 按文件名查找：`fs_glob`
5. 按内容搜索：`fs_grep`
6. 查看大小、编码、换行、hash：`fs_stat`

不要在 fsops 可用时优先使用以下替代品：

- `cat`、`head`、`tail`、PowerShell `Get-Content`
- `grep -rn`、`findstr`、PowerShell `Select-String`
- `find`、PowerShell `Get-ChildItem`
- 整文件读取后整文件写回
- 无锁的临时脚本批量替换

只有 fsops 不可用、超时，或任务不是文本文件时才退回宿主内置工具。

## 推荐工作流

### 读取

1. 已知文件但不知道位置：先 `fs_grep output=files`。
2. 需要上下文：`fs_grep output=snippets`。
3. 已知行号：`fs_read start=... end=...`。
4. 需要多个相关片段：`fs_read ranges=[[a,b],[c,d]]`。
5. 不要把长文件连续多次从头读到尾。

### 修改

1. 先 `fs_read` 获取目标片段和 `hash`。
2. 同文件多处修改一次传给 `fs_edit.edits`。
3. 多文件修改一次传给 `fs_edit.files`，每个文件带自己的 `expect_hash`。
4. 高风险或大范围改动先用 `dry_run=true`。
5. 修改后只在需要时复查；`fs_edit` 已返回行号和新 hash。

### 搜索与分页

1. 大范围初筛用 `output=files` 或 `output=count`。
2. 定位用 `output=snippets`。
3. 截断时使用返回的 `next_offset`，不要自己猜测偏移。
4. 搜索默认自动并行（小仓库串行），一般无需设置 `parallel`；调优时才显式 `true`/`false`。

### 执行命令

1. 前台 `fs_exec` 单次控制在 2 分钟内；更长的构建/测试用 `background=true` 拿 job id，再 `job=<id> wait_ms=60000` 轮询，别反复短轮询。
2. `program` 用 PATH 名或原生绝对路径；相对路径不随 `cwd` 解析。

### 大文件

超过 32 MB 的文件使用连续区间：

```json
{"path": "/d/repo/data.log", "start": 1000, "end": 1050}
```

不要对超大文件省略 `end`。

## 安全边界

- fsops 没有路径沙箱，权限等于当前用户权限。
- 不要编辑项目目录之外的文件，除非用户明确要求。
- 不要用 `fs_write` 做局部修改；局部修改必须用 `fs_edit`。
- 不要删除、移动或覆盖未确认的用户数据。
- `E_STALE`、`E_LOCKED`、`E_AMBIGUOUS`、`E_NO_MATCH` 出现时先按返回提示重新读取，不要原样重试。
- `E_MIXED_EOL` 和 `E_ENCODING_LOSS` 是保护性错误，不要绕过。

## 本仓库开发规则

- Rust 1.88+，Windows 原生。
- 修改行为前先写失败测试。
- 编辑过程中只跑改动模块的单测（`cargo test --lib <模块>::`），不要每改一行跑全套。
- 提交前运行 `.\scripts\check.ps1 commit`（fmt + clippy + 完整 cargo test）；
  push 前、或改动 `io`/`text`/`edit`/`render` 时运行 `.\scripts\check.ps1 full`。
- 不提交本机绝对路径、用户名、工具链安装位置、构建产物或凭据。
- 中文注释、中文文档、中文提交信息。
