# Agent 运行时优化第二批实施计划

> **For implementer:** 全程 TDD。先写失败测试并运行确认 RED，再写最小实现。不要提交、推送或发版；不要修改计划/设计文档。

**目标：** 消除正则替换与搜索中的重复扫描/复制，复用读取行索引，并同步 Agent 引导。

**架构：** 保持 6 个工具；正则编辑单趟输出；搜索借用行切片；并行 grep 在遍历回调内按波次处理；读取构建一次 LineIndex；initialize/rules 对齐 PDF 与正则编辑用法。

**技术栈：** Rust 2021、现有依赖，不新增 crate。

---

## Task A1：正则替换单趟线性化

**文件：** `src/edit.rs`

1. 写失败性能测试：大量行均有正则命中（规模需让旧实现实际超过 5 秒，且新实现明显低于阈值），记录真实 RED。
2. 写正确性测试：同一正则的匹配跨行时，后续命中的行号仍正确；`MAX_HUNKS` 截断不影响 `applied`。
3. 运行相关测试确认 RED。
4. 重写 `apply_regex`：不先收集 `hits`，维护 `line_cursor/line_no` 单趟构建输出；只保留前 `MAX_HUNKS` 个 hunk。
5. 重跑相关测试和完整 edit 测试到 GREEN。

## Task A2：搜索借用行切片

**文件：** `src/search.rs`

1. 加一个包含数万短行的大文件搜索特征测试，锁定命中行、上下文和输出不变。
2. 将 `load_search_lines -> Option<Vec<String>>` 改为返回拥有的正文；扫描作用域内构造 `Vec<&str>`，串行/并行共用同一末尾换行规则。
3. 只有最终选中的命中/上下文才生成 `String`。
4. 跑 search 串行、并行、snippets 与 offset 测试，确认输出逐字节不变。

## Task A3：并行 grep 有界流水线

**文件：** `src/search.rs`

1. 写失败测试：500 个文件、第一页足够命中、`limit` 很小；并行扫描的 `entries_visited` 必须显著小于 500，旧的全量预收集会访问全部。
2. 写一致性测试：同一 fixture 下串行/并行在 files/count/matches/snippets、offset、max_per_file 上输出一致。
3. 把候选收集与扫描合并：遍历回调累积一个波次（现有 chunk/wave 规模），达到后立即并行扫描、按序合并；截断或超时返回 `Stop`。
4. 处理收尾不足一波的条目；保持 deadline、单文件截断标记和现有 GrepResult 字段。
5. 重跑完整 search 测试到 GREEN。

## Task B1：读取行索引复用

**文件：** `src/render.rs`、`src/tools.rs`

1. 增加测试编译期 RED 的 `LineIndex` API 测试：总行数、末尾换行、ranges 渲染与现有函数一致。
2. 增加测试专用线程本地 `LineIndex::new` 计数器：一个 `fs_read` 请求即使预算收敛多轮，也只构建一次索引。
3. 实现 `LineIndex<'a>`，提供总行数、行切片和 indexed ranges 渲染；`render_ranges(text, ...)` 改为包装。
4. `fs_read` 普通文件与超大文件窗口都在进入 converge 前构建一次索引；行数统计、block 探测、ranges 渲染复用。
5. 跑 render/tools 相关测试与完整回归。

## Task B2：Agent 引导一致性

**文件：** `src/mcp.rs`、`src/adopt.rs`、`docs/工具参考.md`、`README.md`

1. 先加失败测试：initialize instructions 必须包含“PDF 走 fs_read”而非回退；规则正文必须包含 `pattern/replacement` 与 `diff=true` 用法。
2. 修改 initialize instructions 和 `RULE_BODY`，保持紧凑，只增加必要语义，不复制整份文档。
3. 统一 PDF 路由：DOCX/PDF 正文走 fs_read；图片和不支持二进制才回退。
4. 更新文档中对应路由和正则编辑链路。
5. 运行 adopt/mcp 测试和 integrate 检查。

## Task B3：集成验证

**文件：** 上述源码与文档

1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test`
4. `cargo build`
5. `python tests/windows_semantics.py`
6. `python tests/bench.py D:/code/fsops`
7. 检查 `git diff`、`git status` 和临时文件；不提交。
