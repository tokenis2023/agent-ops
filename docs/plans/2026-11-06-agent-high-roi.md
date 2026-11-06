# Agent 高 ROI 优化实施计划

> **For implementer:** 全程使用 TDD。每个行为先写失败测试、运行并确认预期失败，再写最小实现使其通过。不要提交、推送或发版。

**目标：** 修复第一批会造成重复扫描、错误续读、无效重试和重复 I/O 的高 ROI 问题。

**架构：** 保持现有 6 个工具。搜索输出模式进入执行层；续读使用可重放区间/偏移；参数在进入执行前严格校验；stat 复用 bytes 快照；walk 自己执行 deadline。

**技术栈：** Rust 2021、现有 serde/serde_json/regex/ignore/globset/blake3，Python 端到端测试，不新增依赖。

---

## Task 1：files/count 搜索专用执行

**文件：** `src/search.rs`、`src/tools.rs`、`docs/工具参考.md`

1. 写失败测试：首文件含 100 条命中、后续两个文件各一条时，`output=files` 默认返回三个文件；`limit=1` 时按文件分页，下一页从第二个文件开始。
2. 写失败测试：count 对每文件返回真实行命中数，结果结构不保存每条命中的正文；串行与并行输出一致。
3. 运行相关 `cargo test`，确认旧实现因首文件挤占 limit 或缺少聚合结构而失败。
4. 把 `GrepOutput` 传入 `GrepOpts`；为 files 使用每文件首命中与文件级 offset/limit，为 count 使用独立的每文件计数集合。只有观察到额外文件才设置截断/下一页。
5. 调整 `grep_tool` 的空结果、渲染与 `next_offset`，使 count 不再依赖 `hits`，files 的偏移按文件推进。
6. 更新 schema 和工具参考，明确 files 与 matches/snippets 的分页单位。
7. 重跑相关测试，确认通过。

## Task 2：block 跨页固定原代码块

**文件：** `src/tools.rs`

1. 写失败测试：从嵌套代码块内部 `block=true` 读取并给小预算，使响应截断；按尾注的 ranges 续读，拼接后不得重复，且必须覆盖最初块的所有行。
2. 运行测试，确认旧实现因重新探测内部块而重复/漏行。
3. 首次块展开后保留固定边界；block 截断尾注输出 `ranges=[[下一行,原块结束]]`，不得只给 start。
4. 重跑测试并确认普通 start/end、显式 ranges 行为不变。

## Task 3：glob 完整条目分页与长行可执行预算

**文件：** `src/walk.rs`、`src/tools.rs`、`src/render.rs`、`docs/工具参考.md`

1. 写失败测试：glob `budget=100` 不得出现半路径；按 `next_offset` 重放，页间无重复、无遗漏；结果恰好等于 limit 时不得伪造下一页。
2. 为 glob 增加按匹配条目计的 `offset`，遍历时做真实 lookahead；去掉伪 Entry 形式的“可能还有更多”。
3. 按完整行和尾注预算渲染；预算截断时 next_offset 只推进实际完整输出的条数，空页返回 `E_BUDGET`。
4. 写失败测试：响应中后出现的截断行比首行更长时，提示预算必须能照抄读取成功。
5. 在渲染结果中记录所有截断行的最大原始字节数，普通与超大文件分支都使用该值生成建议。
6. 更新 schema/文档并运行相关测试。

## Task 4：严格参数与 stat 数量上限

**文件：** `src/tools.rs`

1. 分别写失败测试：未知字段、负数 integer、浮点 integer、paths 非字符串元素都返回 `E_BAD_ARGS`，不得按默认值继续执行。
2. 未知字段错误给最接近的合法字段；schema 增加 `additionalProperties:false`、适当的 `minimum` 和数组 `maxItems`。
3. 写失败测试：fs_stat 传 51 个路径必须显式报错，不得静默只返回 50 个。
4. 运行测试确认 RED，再实现最小校验并确认 GREEN；保留现有合法调用兼容性。

## Task 5：stat 单次读取与 visit 内部 deadline

**文件：** `src/io.rs`、`src/tools.rs`、`src/walk.rs`

1. 写失败测试：用仅在测试编译启用的线程本地读取计数器，证明普通文本 `fs_stat detail=hash/full` 只调用一次底层 read；旧实现应观察到两次。
2. 抽出从已读 bytes 构造 Snapshot 的入口，`read_snapshot` 与 stat 共用；stat 的普通文本路径不得再次读文件。
3. 写失败测试：`walk::visit` 使用 `include_dirs=false` 且不匹配任何文件、visitor 从未执行时，仍能按时间预算停止并报告 timed_out。
4. 在 visit 原始目录项循环每 256 项及结束时检查 deadline；保持现有返回结构与串行搜索结果不变。
5. 重跑相关测试并清理全部测试临时文件。

## Task 6：集成验证与文档核对

**文件：** 上述源码、`README.md`、`docs/工具参考.md`

1. 检查 schema、实现和文档对 files/count/offset/续读的描述一致。
2. 运行 `cargo fmt --all -- --check`。
3. 运行 `cargo clippy --all-targets -- -D warnings`。
4. 运行 `cargo test`。
5. 先 `cargo build`，再运行 `python tests/windows_semantics.py`。
6. 运行 `python tests/bench.py`，记录真实输出；若本批改变数字，只更新受影响的实测表，不虚构 token 收益。
7. 检查 `git diff` 和 `git status`，不提交；确认没有临时文件或构建垃圾进入工作区。
