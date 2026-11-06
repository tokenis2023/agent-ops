# 大文件快速窗口与任务级基准实施计划

> **For implementer:** 全程 TDD。先写失败测试并确认 RED，再写最小实现。不要提交、推送或发版；不要修改计划/设计文档。

**目标：** 默认快速读取超大文件局部窗口，暴露大文件搜索上限，并加入确定性任务链基准。

**架构：** `read_large_window` 增加 window/full 模式；工具头与尾注显式区分部分扫描；grep 暴露既有上限；新 Python 基准走持久 MCP。

**技术栈：** Rust 2021 + 现有依赖；Python 3 标准库。

---

## Task 1：`read_large_window` 双模式

**文件：** `src/io.rs`

1. 写失败测试：window 模式只返回请求区间与 `more`，`hash=None`、`total_lines=None`；full 模式仍有精确 hash/总行数。
2. 签名增加 `full_scan: bool`，字段改为可选并新增 `more`。
3. window 读到 `end` 后再读一行判断 more 即停止；full 保持全扫。
4. 覆盖：空文件、末尾换行、无末尾换行、请求到末行、请求超出末尾。

## Task 2：`fs_read.scan` 契约与渲染

**文件：** `src/tools.rs`、`docs/工具参考.md`、`README.md`

1. 写失败测试：schema 有 `scan`；非法值 `E_BAD_ARGS`；大文件默认窗口头含“未全扫”且无 hash；`scan=full` 有 hash/总行数。
2. 解析 `scan=window|full`，默认 window；只对超大文件生效，小文件行为不变。
3. 快速分支头、E_RANGE、续读尾注按设计输出；window 需要 hash 时提示 `fs_stat detail=hash` 或 `scan=full`。
4. 保持预算收敛、长行提示与 block 无关路径不变。
5. 更新文档与工具表。

## Task 3：`fs_grep.max_file_size`

**文件：** `src/tools.rs`、`docs/工具参考.md`

1. 写失败测试：`max_file_size=1` 跳过 2 字节文件并给可调整提示；`0` 表示不限制并命中；schema 有该参数。
2. 参数传入 `GrepOpts`；0 映射 `u64::MAX`。
3. skip 尾注写清默认值、当前值和调法。
4. 跑 grep 相关测试。

## Task 4：任务级基准脚本

**文件：** `tests/task_bench.py`（新建）

1. 实现持久 MCP 会话：初始化一次，顺序 `tools/call`，逐条读响应；退出时终止进程。
2. fixture 与五个场景按设计；每场景校验最终结果，统计 calls、工具参数字符、工具输出字符、耗时。
3. 任一校验失败非零退出；脚本不使用 tiktoken，不宣称模型 token。
4. 直接用 `python tests/task_bench.py` 跑通，记录输出。

## Task 5：CI 与集成验证

**文件：** `.github/workflows/ci.yml`

1. 先构建 debug 二进制，再运行 `task_bench.py`。
2. 本地运行 fmt、clippy、cargo test、cargo build、windows_semantics、bench、task_bench。
3. 检查 git status 与临时文件；不提交。
