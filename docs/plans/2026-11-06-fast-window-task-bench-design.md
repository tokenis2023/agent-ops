# 大文件快速窗口与任务级基设计

## 目标

让 Agent 读取超大文件局部区间时不必等待全文件扫描，同时提供真实任务链的确定性回归指标。

## 范围

1. `fs_read` 为 >32 MB 文本文件增加 `scan=window|full`，默认 `window`：
   - `window` 读到请求区间后即停，不计算全量 hash/总行数。
   - `full` 保持现有全文件扫描、精确 hash 与总行数。
2. `fs_grep` 暴露现有 `max_file_size`，允许 Agent 调整大文件搜索上限。
3. 新增 `tests/task_bench.py`：持久 MCP 会话下执行真实调用链，统计调用数、工具输入/输出字符、耗时并校验结果。

## 非目标

- 不改普通文件读取、编辑和写盘语义。
- 不新增工具，不做多文件异构 ranges。
- 不把确定性任务基准冒充真实模型 token 消耗。

## 设计

### 快速窗口

`LargeWindow` 的 `hash` 与 `total_lines` 改为可选，新增 `more`：

- `scan=window`：只扫描到 `end` 后再读一行判断 `more`；EOL/编码取首 64KB 样本，hash/总行数省略。
- `scan=full`：保持逐行全扫、hash 和精确总行数。
- 输出头必须明确“窗口扫描（未全扫）”，避免模型把局部信息当全文件信息。
- 续读尾注：
  - 请求区间内被预算截断：给下一行与固定 end。
  - 请求区间已全部输出但文件还有内容：给 `start=区间末尾+1` 与原 end。
  - 区间已到文件末尾：不给续读。
- 需要全量 hash 时提示 `fs_stat detail=hash` 或 `scan=full`。

### 大文件搜索上限

`fs_grep.max_file_size`：

- 默认 8 MiB。
- `0` 表示不限制，其余为字节数。
- 跳过文件尾注必须给出当前上限和可调整参数。

### 任务级基准

脚本使用一个持久 MCP 进程，覆盖：

1. `files` 高密度文件分页；
2. grep 命中 → block 展开 → 页面续读；
3. glob 预算分页；
4. 正则编辑 dry-run + diff；
5. count 高密度文件。

每项校验最终结果，输出 `calls / in chars / out chars / ms`，失败退出非零。CI 只把它当确定性回归，不按共享 runner 延迟卡阈值。

## 验证

- 每项先写失败测试，确认 RED。
- 最终 fmt、clippy、完整 Rust 测试、真实 MCP e2e、现有 bench、新 task_bench。
- 不提交、不推送、不发版。
