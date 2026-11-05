# 重度 Agent 文件操作优化实施计划

> **For implementer:** 全程使用 TDD。先写失败测试并确认失败，再写最小实现使其通过。未经用户明确要求，不提交、不推送。

**目标：** 减少重复 I/O、全目录扫描、无效工具往返和输出 token，同时堵住混合换行、编码转换造成的文件损坏。

**架构：** 保持现有 6 个工具，不增加工具数量。I/O 层提供单次快照；读取工具支持多段区间；搜索改为流式遍历并支持精简结果模式；编辑工具兼容原参数并增加多文件预检写入。

**技术栈：** Rust 2021、现有 serde/regex/ignore/globset/encoding_rs/blake3，Python 端到端测试。

---

## Task 1：单次快照与写入正确性

**文件：** `src/io.rs`、`src/text.rs`、`src/edit.rs`、`src/tools.rs`、`tests/windows_semantics.py`

1. 先增加失败测试：同一快照可直接提供原始字节、Doc 和 hash；GBK 插入不可表示字符返回 `E_ENCODING_LOSS` 且不改文件；Mixed EOL 默认返回 `E_MIXED_EOL` 且不改文件。
2. 分别运行相关 Rust/Python 测试，确认因缺少行为而失败。
3. 增加 `Snapshot { bytes, doc, hash }` 和 `read_snapshot`，让 read/stat/edit/write 复用一次读取结果。
4. 编码时检测不可表示字符，禁止静默改成 UTF-8；混合换行默认拒绝，仅显式 `normalize_eol=true` 时归一为 LF。
5. 重跑相关测试并确认通过。

## Task 2：多区间读取与全局输出预算

**文件：** `src/render.rs`、`src/tools.rs`

1. 先写失败测试：单文件 `ranges=[[1,2],[8,9]]` 一次返回两段；`max_line=0` 返回完整长行；包含上下文/元信息的最终响应不超过 budget。
2. 实现 ranges 解析和分段渲染，保留 start/end 兼容性。
3. 提供统一的 UTF-8 安全预算收敛函数，覆盖头部、正文和尾部提示；截断后给出可继续调用的参数。
4. 保持默认输出格式，避免无谓 token 增长。

## Task 3：流式搜索、结果模式和预算

**文件：** `src/walk.rs`、`src/search.rs`、`src/tools.rs`

1. 先写失败测试：达到 limit 后停止遍历；目录遍历也计入超时；`output=files/count/matches/snippets` 输出正确；上下文计入预算；命中计数不再重复扫描 hits。
2. 抽出 walk 的流式访问接口，让 grep 不再先构造 100,000 项列表。
3. 增加 `output`、`max_per_file`、`offset`，以紧凑 `next_offset` 支持继续取结果。
4. 保持按文件分组和稳定输出；所有模式使用统一最终预算。

## Task 4：多文件批量编辑

**文件：** `src/tools.rs`、`tests/windows_semantics.py`

1. 先写失败 MCP 测试：`files=[{path,edits,expect_hash},...]` 一次修改两个文件；任一文件预检失败时所有文件均不变。
2. 抽出单文件预检结构，先完成全部路径解析、hash 校验、匹配和编码，再逐文件原子写入。
3. 明确只保证单文件原子替换；写入阶段失败返回已完成和未完成的文件，不宣称跨文件全原子。
4. 修改位置列表做条数上限，防止 `all=true` 产生超大响应。

## Task 5：验证、基准和文档

**文件：** `README.md`、`docs/设计.md`、`tests/bench.py`、`tests/windows_semantics.py`

1. 更新工具 schema、参数示例和已知限制，不增加新工具。
2. 增加任务级场景：多段读取、多文件编辑、files 初筛、长行读取、混合换行与编码失败保护。
3. 运行 `bash scripts/build.sh all`，要求全部 Rust 测试和真实 MCP 测试通过，release 构建成功。
4. 运行格式化和 clippy；检查 git diff、临时目录和工作区，清理所有测试残留。
5. 报告实际测试数字和仍未实现的并行搜索、持久缓存、真正跨文件事务，不虚构收益百分比。

## Task 6：仓库与使用说明可移植性

**文件：** `README.md`、`docs/设计.md`、`scripts/build.sh`、`tests/bench.py`、`tests/windows_semantics.py`、涉及示例路径的源码注释与测试

1. 禁止写入任何本机绝对路径、用户名、工具链安装位置或特定本地仓库路径。
2. MCP 接入示例使用 `PATH` 中的 `fsops`，需要绝对路径时由使用者自行替换。
3. Python 测试通过 `__file__` 定位项目内二进制；基准仓库路径必须由参数传入。
4. 构建脚本只调用标准 `cargo` 和可配置的 `PYTHON`，不修改使用者的 `PATH`。
5. 全仓扫描本机环境标识必须为零，并验证脚本语法和完整测试。
