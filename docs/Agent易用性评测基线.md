# fsops Agent 易用性评测基线（P0）

> 编写日期：2026-11-10。对应计划：`docs/Agent易用性优化计划.md` 的 P0。
> 本文只记录**已实测**的现状与缺口，不含产品改动。所有数字都是字符/字节口径，
> 不是模型 token；没有指定 tokenizer 的地方不换算 token。

## 1. 基线冻结

| 项 | 值 |
|---|---|
| Git 提交 | `cf5eee2` `chore(release): 0.10.0`（2026-11-10 09:28 +0800） |
| 工作区 | 无已跟踪文件改动；2 个未跟踪文档：`docs/Agent易用性优化计划.md`、本文 |
| 版本 | 0.10.0；`python scripts/release.py check` 通过（Cargo.toml / Cargo.lock / package.json / CHANGELOG / 工具参考 / 使用指南 / 发布正文一致） |
| 工具链 | rustc 1.99.1（2026-10-13）、cargo 1.99.1、Python 3.13.15、Node v24.22.0 |
| 平台 | Windows 10.0.19045 x64 |
| 被测二进制（debug） | `target/debug/fsops.exe` 89,358,425 B，sha256 `bbb3e1d21ee12cd6767e000be9c50ac8cac2aebf9f50ccc999e63fef4dafdac5`，构建于 2026-11-10 09:26:11 |
| 被测二进制（release） | `target/release/fsops.exe` 3,495,936 B，sha256 `6ffa2dffd37fd023342b3ac4bcacdee7037e6715b0b28d072c09a085f706f55c` |

二进制与源码的对应关系有两条证据，不是「看着像」：

1. 两个二进制 `fsops schema` 输出逐字节相同（25,855 B，sha256 `be248b0652018fe2…`），说明 debug/release 工具定义一致；
2. 冻结后再跑 `cargo build` 输出 `Finished dev profile … in 0.13s`（无重编译），说明 debug 二进制就是当前 HEAD 源码构建的产物。

> 本机 `%LOCALAPPDATA%\fsops\fsops.exe`（宿主实际加载的副本）未纳入本次基线；
> P1b 真实评测前必须重新核对它的版本与 hash，禁止用陈旧副本当候选。

## 2. 复跑命令与基线结果

| 命令 | 结果 |
|---|---|
| `cargo test` | 383 通过 / 0 失败，7.85 s |
| `python tests/windows_semantics.py` | 186 / 186 通过，9.4 s |
| `python tests/task_bench.py --exe target/debug/fsops.exe` | 9 场景全 PASS；66 次调用，入参 6,694 字符，输出 44,010 字符，2.34 s |
| `node tests/dsh_plugin_smoke.mjs --exe target/debug/fsops.exe` | 89 项断言全通过，2.7 s |
| `node scripts/gen_dsh_specs.mjs --check` / `gen_docs_tools.mjs --check` | 均无漂移 |
| `python tests/bench.py` | 内置 fixture：3610 → 879（按该脚本的折算口径）；6 个子项 |
| `python tests/release_tooling.py` | 21 / 21 通过 |
| `powershell -File scripts/check.ps1 full` | 11 步全绿，36 s（含上述 e2e / 基准 / 生成物 / 冒烟） |

### 任务基准逐场景基线（4 次复跑）

calls 与 out 字符在 4 次复跑中完全一致；只有「后台任务轮询」的 calls 在 22–23 之间浮动（轮询节奏），因此合计 calls 为 65–66。

| 场景 | calls | out 字符 | ms |
|---|---:|---:|---:|
| files 高密度分页 | 20 | 1,711 | 146 |
| grep→block→续读 | 4 | 915 | 5 |
| glob 预算分页 | 7 | 4,407 | 41 |
| 正则编辑 dry_run+diff | 2 | 2,064 | 12 |
| count 高密度 | 1 | 31 | 3 |
| 多文件异构 reads | 6 | 1,372 | 4 |
| 重复读不短路 | 2 | 31,224 | 3 |
| repo map 排序 | 1 | 412 | 3 |
| 后台任务轮询 | 22–23 | 1,874 | ~2,100 |
| 合计 | 65–66 | 44,010 | ~2,340 |

## 2.1 P1a 回归补缺后的数字（2026-11-10）

P0 的基线数字保留在上一节，便于阶段对照；以下只列变化。

| 命令 | P0 | P1a 后 | 变化 |
|---|---|---|---|
| `cargo test` | 383 | **386** | +3（T05 1、T10 2） |
| `python tests/windows_semantics.py` | 186 | **210** | +24（T06 4、T07 3、T08 3、T09 4、T13 8、T16 2） |
| `python tests/task_bench.py` | 9 场景 / 66 调用 / 44,010 字符 | **10 场景 / 69 调用 / 约 53.8k 字符** | +T15；+3 调用来自新场景，原 9 场景的 calls 与输出逐字不变；新场景输出随临时目录路径长度浮动（实测 53,795 / 53,817） |
| `check.ps1 full` | 11 步 | **11 步全绿**，47 s | e2e 增加约 5 s（T10 单测另占 3 s） |

T13 的故障注入不是 sleep 猜时序：用 `open(path, 'rb')` 持有句柄，Windows 默认共享模式不允许 delete，原子改名必然失败，失败点确定。两次独立执行的“已写入 1/2”与磁盘完全一致。

## 2.2 P2a 引导收敛后的数字（2026-11-10）

本阶段只改文案与排版，不改任何工具行为。

| 项 | P0 | P2a 后 | 变化 |
|---|---:|---:|---:|
| `initialize.instructions` | 1,262 B | **997 B** | **−21.0%** |
| `integrate --rules` 规则块（RULE_BODY） | 2,850 B | **1,978 B** | **−30.6%** |
| instructions ↔ RULE_BODY 字符重合 | 47.1% | **19.6%** | −27.5 pp |
| 工具 description 合计 | 3,164 B | **3,195 B** | +31 B |
| inputSchema 合计 | 14,225 B | 14,225 B | 0 |
| 描述+引导常驻合计 | 17,389 B | **17,420 B** | +31 B |
| `cargo test` | 383 | **391** | +8（含描述契约 3 项） |
| `check.ps1 full` | 11 步 | 11 步全绿，34 s | — |

如实说明：**计划 P2 验收写的「公共说明字节量减少约 25%」只对 `instructions` 单份成立（−21%，接近但未达 25%）；两份引导合计从 4,112 B 降到 2,975 B（−27.7%）。而把参数事实搬进工具描述后，描述从 3,164 涨到 3,195 B，所以“引导+描述”总常驻只减了 241 B。真正的收益是重复率下降，不是总量。**

P2 的分工边界（已写成测试）：

- 工具描述：参数名、互斥、默认预算、统计口径这类“不说就猜不到”的事实；
- `instructions` / `RULE_BODY`：只用 fsops、何时回退、跨工具策略，并指向工具描述。

两条防回退断言：`引导不复制工具描述里的参数枚举`（instructions 不得再出现 `reads=[`/`focus=`/`map=true`/`block=true`）、`规则正文不重复参数枚举`（RULE_BODY 不得出现上述枚举与 `默认完整统计`）。

## 2.3 P4 后的数字（2026-11-10）

| 命令 | P2a 后 | P4 后 | 变化 |
|---|---|---|---|
| `cargo test` | 391 | **401** | +10（F-6 2、F-7 3、F-9 1、P3 遗留 4） |
| e2e | 213 | 213 | — |
| 10 个基准场景 | 全 PASS | 全 PASS | 无回归 |
| `fs_inspect` 声明参数 | 5 | **7** | +`time_budget_ms`、`no_ignore` |
| 全部 inputSchema | 14,225 B | **14,527 B** | +302 B（inspect 声明了两个真实存在的参数） |
| 描述 + schema | 17,420 B | **17,722 B** | +302 B |

## 3. 体积口径

### 3.1 MCP 常驻成本

| 项 | 字节（P0） | 说明 |
|---|---:|---|
| `initialize.instructions` | 1,262 | 694 字符，14 个分句 |
| `tools/list` 整条响应 | 17,897 | 9 个工具的 name/description/inputSchema |
| └ description 合计 | 3,164 | 平均 352 B |
| └ inputSchema 合计 | 14,225 | 含类型、枚举、描述与 `additionalProperties:false` |

> 本节数字是 P0 口径；最新值见 2.2、2.3 节。

逐工具（description / inputSchema / 参数个数）：

| 工具 | desc | schema | 参数数 |
|---|---:|---:|---:|
| fs_read | 660 | 3,252 | 14 |
| fs_grep | 191 | 2,611 | 16 |
| fs_edit | 518 | 2,370 | 7 |
| fs_exec | 370 | 1,726 | 14 |
| fs_glob | 279 | 1,456 | 12 |
| fs_file | 338 | 904 | 10 |
| fs_write | 275 | 911 | 6 |
| fs_inspect | 268 | 591 | 5 |
| fs_stat | 265 | 404 | 3 |

这两个数字解释了两件事：

- 常驻体积的 **80% 在 inputSchema**（14,225 / 17,389），不在单工具 description；只精简 description 的收益上限约 3.1 KB。
- 描述里可省的部分很小：7 个工具的「优先(用本工具)替代 …」句合计 229 B（fs_read 66、fs_glob 36、fs_edit 35、fs_grep 34、fs_write 29、fs_stat 29）。

### 3.2 其它投影

| 产物 | 字节 | 备注 |
|---|---:|---|
| `initialize.instructions` | 1,262 | 服务端系统级引导 |
| `integrate --rules` 写入的规则块（`RULE_BODY`） | 2,850 | 1,666 字符，34 条 |
| 本仓库自己的 `AGENTS.md` | 3,567 | 2,041 字符，人工维护 |
| `dsh/tool-specs.generated.js` | 23,169 | desc 3,318 + params 13,161；fs_exec 描述被 patch 到 524 B |
| `docs/工具参考.md` | 36,931 | 人读文档 |

## 4. 描述与引导流向（计划假设的修正）

真源链是单向的，已逐层核对：

```text
src/tools.rs::build_tool_defs  ──fsops schema──►  scripts/gen_dsh_specs.mjs ──► dsh/tool-specs.generated.js
        │                                                    ▲
        │                                     dsh/excluded.js（EXCLUDED / DESCRIPTION_PATCHES）
        └──fsops schema──► scripts/gen_docs_tools.mjs ──► README / 工具参考 / 使用指南 / package.json

src/mcp.rs::instructions()      ──initialize──► 宿主
src/adopt.rs::RULE_BODY         ──integrate──►  AGENTS.md / CLAUDE.md / .cursor 等
```

由此得到三点**修正**：

1. **重复不在工具描述内部。** 逐工具 description 与 instructions 的分句重合数是 0/14；每个工具描述只讲自己的参数与局部约束。
2. **真正的重复是 instructions ↔ RULE_BODY。** 用 difflib 做字符级比对，instructions 有 **47.1% 的字符**能在 RULE_BODY 里找到连续匹配（≥6 字符块）；最大块是「或 shell 的 cat/head/tail/grep/find/sed 处理文本」。两者都是 1–3 KB 级、每轮都会进上下文的文字，这才是 P2 该收敛的对象。
3. **宿主侧还有第三份。** 仓库 `AGENTS.md` 由人维护，与上面两份**语义重叠但文案不同**（例如「2 分钟」「output=count」「工具发现」只在 AGENTS.md 里有）。P2 应先定义「哪份是权威、哪份是投影」，再谈删减，否则会把唯一入口删掉。

### 事实分布矩阵（Y=有，.=无）

| 事实 | instructions | RULE_BODY | 仓库 AGENTS.md | 工具描述 |
|---|---|---|---|---|
| 禁用 cat/head/tail/grep | Y | Y | Y | Y（fs_read） |
| DOCX/PDF 走 fs_read | Y | Y | . | Y |
| pattern/replacement | Y | Y | . | Y |
| diff=true | Y | . | . | . |
| reads=[…] | Y | Y | . | . |
| focus= | Y | Y | . | . |
| block=true | Y | Y | . | . |
| map=true | Y | Y | . | Y |
| 并行只读 | Y | Y | Y | . |
| fs_exec argv | Y | Y | Y | . |
| fs_file | Y | . | . | . |
| output=count 统计口径 | . | Y | Y | . |
| 回退边界 | Y | Y | . | . |
| 前台 2 分钟 | Y | . | Y | . |

缺口很明确：`diff=true` 只出现在 instructions；`fs_file` 在 RULE_BODY 与 AGENTS.md 里都没有；`output=count` 的「默认完整统计 / 本页可见」只在不进服务端的 RULE_BODY 与 AGENTS.md 里——宿主若忽略 instructions 也读不到 AGENTS.md，这条就没入口。

## 5. fs_read 契约盘点

### 5.1 模式与互斥（实测行为，不是读注释）

| 组合 | 实际行为 |
|---|---|
| `path` + `paths` | 并集，一次读多个，共用 start/end（文档已说明，非冲突） |
| `reads` + `path`/`paths`/`ranges`/`start`/`end`/`block`/`summary`/`focus` | `E_BAD_ARGS reads 不能与 {key} 同时使用` |
| `tail` + `start`/`end`/`ranges`/`block`/`summary`/`focus`/`paths` | `E_BAD_ARGS tail 与 … 互斥` |
| `summary` + `ranges`/`block`/`end` | `E_BAD_ARGS … summary=true 不能与 … 同时使用` |
| `focus` + `paths`/`ranges`/`start`/`end`/`block`/`summary` | `E_BAD_ARGS focus 只能用于单文件 path…` |
| `reads` + `tail` / `max_line` / `scan` | P0 时：静默忽略；**P4 后**：`max_line`/`scan` 传播到每项，`tail` 报 `E_BAD_ARGS` |
| `block` + `ranges` | P0 时：静默按 ranges；**P4 后**：`E_BAD_ARGS block 与 ranges 互斥` |

后两行是本次盘点的**新发现**：它们是「无效调用但不报错」，已在 P4 消除（见第 7 节 F-7）。

### 5.2 预算与截断

| 项 | 默认 / 规则 |
|---|---|
| fs_read budget | 32,768 B；单文件 ≤32 KB 且未显式给 budget 时自动读全文 |
| 显式 start/end 或 ranges 且未给 budget | 按请求行数估算放宽，上限 64 KB |
| paths 多文件 | 按文件大小分配；某文件分到的预算 <120 B 时 `E_BUDGET … 放不下文件头` |
| reads | 顶层预算按项均分；每项 <120 B 时 `E_BUDGET … N 项只有 … 字节预算` |
| max_line | 240 字符；`0` 不截断 |
| >32 MB 文件 | `scan=window`（默认）只读到区间末尾，**不给全文 hash/总行数**；`scan=full` 全扫 |
| fs_grep / fs_glob / fs_inspect budget | 均为 16,384 B |
| 遍历时间预算 | grep 5,000 ms；glob 5,000 ms；inspect 3,000 ms（后两者实现里有 `time_budget_ms`/`no_ignore`，但 schema 未声明，见第 7 节） |

### 5.3 续读提示（逐字实测）

```text
read 截断      … 1-41/400 行 | 续读: fs_read 同 path start=42
read 多文件    … a-b/N 行 | 续读: fs_read path=<p> start=…
read ranges    … 续读: fs_read 同 path ranges=[[a,b],…]
glob 截断      … next_offset=2 | 续读: fs_glob offset=2（单位：匹配条目）
grep 截断      … next_offset=3 | 续读: fs_grep offset=3 output=matches（单位：命中）
```

长行另有可重放命令：`… 有长行被截断（行 a-b）；最长截断行约 N 字节；重读该行: fs_read path=… start=a end=b max_line=0 budget≈N+200`。

结论：**「续读提示可直接构造成下一次合法调用」这一条现在已基本满足**，P4 剩下的是覆盖完整性与「截断 vs 空结果」的区分，不是重造提示。尚未写成契约的有两点：

- offset 不是快照（并发变化的目录不承诺无遗漏/无重复）——`docs/` 里找不到这句；
- count 的精确数 vs 本页可见数，只有 `本页可见` 标记，没有集中说明。

## 6. 错误恢复现状审计

均用 `fsops call` 实测取真实返回。下表为 **P0 时点**的快照，P3 后最新文案见 6.1。

| 错误 | P0 文案（要点） | P0 时的缺口 |
|---|---|---|
| `E_AMBIGUOUS` | 命中数 + 行/列 + 每处上下文片段；建议顺序为「加更多上下文 → `all: true` → `start`/`end`」 | 把 `all=true` 摆在限定范围之前 |
| `E_NO_MATCH` | 最接近的行（≤5）或全文首 3 行 + 总行数 | 只列 5 条不说总数 |
| `E_STALE` | `当前 hash=<h>；若改动仍适用，原样重试并改传 expect_hash=<h>` | 「改传 hash」写在最前，诱导跳过内容核对 |
| `E_LOCKED` | `已自动等待 3000ms，本次未写入，参数不变直接重试即可` | 没有「停止重试并报告」的升级路径 |
| `E_MIXED_EOL` / `E_ENCODING_LOSS` | 事实 + 显式参数即可放行 + 「文件未修改」 | 无 |
| `E_BATCH` / `E_BATCH_PARTIAL` | 预检失败全不写；中途失败给已写清单 | 只有格式化函数单测；`fs_write files` 用另一套文案 |

### 6.1 P3 后的实际文案（实测）

```text
E_AMBIGUOUS 第1处匹配到 2 个位置（行 1,2 列 1,1）；先加更多上下文让 old 唯一，
或用 "start"/"end" 限定行范围（…也可放进 edits[] 项里）；确认要改全部时才用 "all": true
E_STALE 文件已被其他进程改过（当前 hash=<h>），本次未写入。先 fs_read 核对目标文本
是否仍适用且未被外部改动：仍适用则带 expect_hash=<h> 重试，内容已变则按新内容修正 old 后再改
E_LOCKED <path> 正被其他进程占用；已自动等待 3000ms，本次未写入任何内容。短时占用可直接重试；
若连续两次仍被占（释放无望）就不要再试，报告哪个进程在占用该文件
E_NO_MATCH 第1处未找到目标文本；最接近的行（共 N 条相似，仅列前 5 条）：…
```

对应改动与断言：

| 项 | 修改 | 钉住它的测试 |
|---|---|---|
| `E_STALE` 顺序 | 先把「fs_read 核对」写在前面，再给「仍适用才带新 hash」 | `stale错误先要求核对内容再给重试参数`（校验 `fs_read` 位置 < `expect_hash` 位置） |
| `E_AMBIGUOUS` 顺序 | 加上下文 → 限定范围 → `all=true`（写成「确认要改全部时才用」） | `歧义提示先限定范围再考虑全部替换` |
| `E_AMBIGUOUS` 预算 | 候选片段从固定 3 处改为按 1200 B 截断，未展开的写明条数与行号 | `歧义候选展开受预算限制`（12 处长行，错误 < 2000 B 且含「未展开」） |
| `E_NO_MATCH` 总数 | 扫描全部相似行后再取前 5，文案写「共 N 条相似，仅列前 5 条」 | `未命中候选超过五条时标注总数` |
| `E_LOCKED` 升级 | 加「连续两次仍被占就不要再试，报告占用进程」 | `锁错误给重试与停止条件` |
| `E_BATCH_PARTIAL` 统一 | `fs_write files` 中途失败改用同一函数与同一错误码，带已写清单 | e2e「fs_write 中途失败报告已写入数量」（断言 `E_BATCH_PARTIAL`） |
| 三类状态 | 内存中间态 / 单文件落盘 / 多文件提交各自措辞不变并补断言 | `多编辑中途失败时说明片段是内存中间态`、e2e「单文件内存中间态声明未写入」 |

磁盘保护实测（P3 后复验）：外部改动后 `E_STALE` 且磁盘保持外部版本；单文件多 edit 中途失败磁盘未变；批量预检失败一个都不写。

## 7. 已确认的产品问题（P2/P3/P4 的输入）

| 编号 | 问题 | 证据 | 归属 |
|---|---|---|---|
| F-1 | ~~`instructions` 与 `RULE_BODY` 47% 字符重复~~ **P2a 已处理**：现 19.6%，两份合计 4,112→2,975 B | 2.2 节 | 完成 |
| F-2 | ~~`diff=true`/`fs_file` 只在 instructions；`output=count` 口径不在服务端引导~~ **P2a 已处理**：`diff=true` 与 count 口径已写进 `fs_edit`/`fs_grep` 描述 | `描述带上无法从参数名推断的约束` | 完成 |
| F-3 | ~~工具数注释写「8 个」而实际 9 个~~ **P2a 已处理**：6 处文案已去数量，新增 `非生成器维护的文案不报错工具数` 钉住残留 | 单测 | 完成 |
| F-4 | ~~`E_STALE` 先建议「改传新 hash 重试」，`E_AMBIGUOUS` 先建议 `all=true`~~ **P3 已处理**：两者恢复顺序已调，并有位置断言 | 6.1 节 | 完成 |
| F-5 | ~~锁持续占用无升级路径~~ **P3 已处理**：`E_LOCKED` 增加「连续两次仍被占就停止并报告」；等待仍为 3000 ms | 6.1 节 | 完成 |
| F-6 | ~~`fs_inspect` 实现读取 `time_budget_ms`/`no_ignore`，schema 未声明~~ **P4 已处理**：两者已进 schema 与工具参考，传了不再报未知字段 | `inspect声明的预算参数与实现一致` | 完成 |
| F-7 | ~~`reads` + `tail`/`max_line`/`scan`、`block` + `ranges` 静默忽略~~ **P4 已处理**：`max_line`/`scan` 传播到子项；`tail` 与 `block`+`ranges` 改为 `E_BAD_ARGS` | 第 5.1 节与新增单测 | 完成 |
| F-8 | ~~`docs/工具参考.md` 的 `E_LOCKED` 与实现不一致~~ **P3 已处理**：错误表已同步，并补上恢复顺序 | 工具参考错误表 | 完成 |
| F-9 | ~~offset 不是快照、count 精确/部分口径没有成文契约~~ **P4 已处理**：工具参考新增「四种截断」表与 offset 非快照声明，并有 `截断与空结果区分及续读参数可直接重放` 验证 | 工具参考「输出预算与截断」 | 完成 |

## 8. 覆盖矩阵（计划第 5 节 T01–T16）

判定口径：有确定性断言且校验磁盘/输出内容 = 覆盖；有断言但缺关键环节 = 部分；无断言 = 缺失。

| 编号 | 任务 | 判定 | 证据 / 缺口 |
|---|---|---|---|
| T01 | 陌生小仓库定位并回答 | 部分 | `tests/bench.py` 内置 fixture；缺真实 Agent 侧 |
| T02 | 大仓库按文件和符号缩范围 | 覆盖（确定性） | `map::tests::*`、`glob_map*`、`search::tests::默认跳过依赖目录…`、`walk::tests::glob_limit…`；大仓库规模本身未覆盖 |
| T03 | 多文件不同范围读取 | 覆盖 | `tools::roundtrip_tests::reads*`、e2e「reads 一次读三文件」、task_bench「多文件异构 reads」 |
| T04 | focus / block 定位上下文 | 覆盖 | `focus*` 6 项、`block*` 6 项、e2e「block=true 从函数内部展开」「focus 一次取回两个命中代码块」 |
| T05 | 函数内两处重复文本 | 覆盖 | 新增 `tools::tests::行范围限定只改同函数内的两处相同字面量`：两处同改、同文件其他函数不动 |
| T06 | 三文件批量修改 | 覆盖 | 新增 e2e「三文件批量编辑一次写入」：GBK+CRLF / UTF-16LE+BOM / UTF-8+LF 逐字节比对 |
| T07 | 歧义后恢复 | 覆盖 | 新增 e2e 三项：首次 `E_AMBIGUOUS` → 磁盘未变 → 限定范围后一次改对且不波及同文件其他函数 |
| T08 | 目标不匹配后恢复 | 覆盖 | 新增 e2e 三项：候选行可解析 → 直接用作 old 重试成功 |
| T09 | 外部修改造成 hash 陈旧 | 覆盖 | 新增 e2e 四项：陈旧拒绝 → 外部修改仍在 → 重读拿新 hash → 改对且保留外部行 |
| T10 | 锁短暂 / 持续占用 | 覆盖 | 新增 `锁短暂占用时等待后写入成功`（250ms 后成功）与 `锁持续占用时报出等待时长且不写入`（≥2900ms、E_LOCKED 含 3000ms、磁盘不变、释放后原参数重试成功） |
| T11 | GBK / UTF-16 / BOM / CRLF | 覆盖 | e2e 6 项字节级断言 + `text::tests::*` |
| T12 | 不可编码字符 / 混合换行拒绝 | 覆盖 | e2e 8 项 + `text::tests::编码损失…` |
| T13 | 批量预检失败 / 提交中途失败 | 覆盖 | 预检：单测 + e2e（一个都不变）；中途失败：新增故障注入（占用句柄使 rename 失败）→ `E_BATCH_PARTIAL 已写入 1/2`、已写清单与磁盘一致、只重发剩余文件成功；`fs_write files` 同路径也验证 |
| T14 | grep / glob 多页遍历 | 覆盖 | task_bench 2 场景、`grep_*`/`glob_*` 系列、e2e next_offset 重放 |
| T15 | 长行 / >32 MB 窗口续读 | 覆盖 | 单测 `超大文件*` 8 项；新增 task_bench「>32MB 窗口续读」：窗口不伪造 hash/总行数、续读区间连续、`scan=full` 给精确值 |
| T16 | 后台构建 + 并发只读 | 覆盖 | e2e background、`jobs::tests::*`、DSH 冒烟；新增 e2e「长任务运行中只读调用不被阻塞」：持久会话内起 8 次 ping，同连接 fs_read <2s 返回且 `list` 报 running |

## 9. 评测资源口径与未确定项

### 9.1 已确定

- 真源与投影链（第 4 节），候选版本改动必须从 Rust 真源生成 DSH 与文档清单；
- 确定性回归入口：`tests/task_bench.py`、`tests/windows_semantics.py`、`tests/dsh_plugin_smoke.mjs`，统一由 `scripts/check.ps1 full` 调用；
- 现有 DSH subagent 流程能承载真实 Agent 对照（`docs/轮次实测.md` 用过）；
- 夹具用 `TemporaryDirectory`/随机前缀，默认不留在仓库；README 已声明凭据不入库。

### 9.2 未确定（P1b 前必须定）

| 项 | 现状 | 需要的决定 |
|---|---|---|
| 模型与推理配置 | 未定 | 两侧同模型同配置；记录模型名与版本字符串 |
| 宿主与工具发现 | 候选为 DSH subagent；MCP 直连也可复现 | 选一个主宿主，并记录两侧实际可见的描述 |
| usage 获取 | DSH 调用日志可得调用次数/时间；token usage 是否可得未验证 | 若不可得，报告标「不可获取」，不猜 |
| 预算 | 未授权外部付费调用 | 先 6 任务 × 3 次排查评测；正式 16 任务 × 5 次需另行授权 |
| 版本矩阵 | 1.88+ / Windows 原生 | 只承诺 Windows；不扩 Linux |
| 原生工具组 | 需禁用 fsops 并去掉「必须用 fsops」引导 | 两侧任务书与内容安全约束保持一致 |

### 9.3 默认建议与成本估计

建议顺序：**P1a（确定性补缺）→ 6×3 小规模真实对照 → P1b 正式规模**。理由：T10/T13/T16 的缺失可在零模型成本下补齐，先把「评测本身有 bug」的概率压掉，再花钱。

成本用公式表达，不假装已测得：

```text
输入 token ≈ 运行数 × 平均调用轮次 × (常驻 schema/引导 + 累积上下文)
运行数    = 任务数 × 每任务次数 × 2 侧
```

按 `docs/轮次实测.md` 的「约 6.5k 常驻 token、单任务 40 轮量级」做**假设**（不是新测量）：6×3×2=36 次运行、每次约 40 轮 × 平均 30k 输入 token ≈ 43M 输入 token；16×5×2=160 次运行 ≈ 190M。二者相差 4 倍以上，所以先用 6×3 校准「每次运行的实际 token 与轮次」，再决定正式规模。缓存命中率未知，账单不能由上下文长度直接推算。

## 10. 结论

### P0 验收

- ✅ 每条优化输入都有实测证据或明确标为待验证（第 7 节 F-1…F-9）；
- ✅ 基线可复跑，原始命令与数字在第 2 节；
- ✅ 实际二进制与源码对应（第 1 节两条证据）；
- ✅ 既有失败：0 项。`check.ps1 full` 11 步全绿，没有需要区分的环境问题。

### P1a 验收

计划 P1a 的验收是「确定性用例稳定通过」。现状：

- ✅ 计划点名的 T05、T06、T07、T08、T09、T10、T13、T15、T16 全部由「部分/缺失」转为**有断言且校验磁盘的覆盖**（第 8 节矩阵已更新）；
- ✅ 新增断言均为确定性，无 `sleep` 猜时序：锁用持锁线程 + 通道同步，部分写入用占用句柄制造确定的 rename 失败，长任务用持久会话 + 同连接回读；
- ✅ `check.ps1 full` 仍 11 步全绿；原 9 个基准场景的 calls/输出与 P0 逐字一致，无回归；
- ✅ **未修改任何产品代码**：本轮只动 `src/tools.rs` 的 `#[cfg(test)]` 模块与两个测试脚本。P1a 是测量能力补缺，不是行为变更。

仍未覆盖（属 P1b 或后续阶段，不在 P1a 范围）：

- T01 的真实 Agent 侧（命令行工具的确定性部分已覆盖）；
- T02 的「大仓库规模」（目前 fixture 为小仓库，`map`/`walk` 有符号级断言但无限量级压测）；
- T14 的「offset 在并发变化目录上的边界」——这属于 P4 的契约问题（F-9），不是断言缺口。

对计划的两处修正应在 P2/P3 执行前生效：

1. 「接入的工具描述出现相同公共说明」不成立——重复在 `instructions` ↔ `RULE_BODY`（47% 字符），工具描述本身是局部的；P2 的收益应主要从这两份 1–3 KB 文本与 14 KB inputSchema 里找。
2. 「续读提示需重新构造」的前提不成立——现有提示已可直接重放；P4 的真实缺口是完整性口径（offset 非快照、count 精确/部分）与静默忽略参数（F-7）。

### P2a 验收（描述收敛，未做真实 Agent 对照）

计划 P2 的验收包含「公共说明字节量减少约 25%」与「参数误用不倒退」。本地能验的部分：

- ✅ `instructions` 1,262 → 997 B（−21.0%）；两份引导合计 4,112 → 2,975 B（−27.7%）；重复率 47.1% → 19.6%；
- ✅ **未达 −25% 单份目标**：如实记录，不为凑数字删减回退边界与 `fs_exec` 超时策略；
- ✅ 互斥、预算、统计口径、`scan`、大文件约束全部保留在描述或参数说明里，并有测试钉住（`fs_read描述把常用路径放在前面`、`描述带上无法从参数名推断的约束`）；
- ✅ 既有合法调用兼容：tool name、参数名、默认值、错误码均未变；391 单测 / 210 e2e / 10 基准 / 89 DSH 断言全绿；
- ✅ DSH 定义与文档清单已重新生成，`--check` 无漂移；
- ⚠️ **未做**「仅精简描述」与「补示例」的真实 Agent 对照（P2b），也无参数误用率数据。描述类改动对模型的实际影响未测量，不声称提升。

另修正了一个既有测试缺陷：`search::tests::并行count单文件超时置位truncated` 原本断言 30ms 预算内「至少扫到一个文件」，高负载下会假失败（P2 门禁实际抖过一次）。现改为断言与时序无关的契约（必报超时+截断；若已开扫，计数不得等于完整值），连续 5 次通过。

### P3 验收（错误恢复语义）

计划 P3 的硬要求是「保护性错误不修改磁盘；模型不因提示自动放弃保护」。本地能验的部分：

- ✅ `E_STALE` 恢复顺序改为「先核对内容、再给新 hash」，并有位置断言；指纹校验本身未削弱；
- ✅ `E_AMBIGUOUS` 不再把 `all=true` 排在限定范围之前；候选片段从固定 3 处改为 1200 B 预算，超出时写明条数与行号；
- ✅ `E_NO_MATCH` 扫描全部相似行后给出总数，不再让模型以为只有 5 处；
- ✅ `E_LOCKED` 加停止条件（连续两次仍被占就停），等待时长与「未写入」事实不变；
- ✅ `E_BATCH_PARTIAL` 两条路径（`fs_edit files` / `fs_write files`）统一到同一函数与错误码；
- ✅ 三类状态各自措辞保留并补断言：内存中间态 ≠ 单文件落盘 ≠ 多文件提交；
- ✅ 门禁 `check.ps1 full` 11 步全绿（395 单测 / 213 e2e / 10 基准场景）；
- ⚠️ **未验证模型行为**：「模型不因提示自动放弃保护」「恢复调用中位数减 1」需要 P1b 的真实模型数据，本轮只有文案与确定性断言。

本轮未做的事（有意保留）：将结构化 MCP 结果代替文本契约——计划 P3-6 把它列为单独决策，当前无宿主消费方证据，未实施。

### P4 验收（预算、截断与续读）

计划 P4 的验收是「续读拼接后与预期全集一致；预算边界、UTF-8 中文边界与超大文件窗口有针对性验证」。

- ✅ F-6：`fs_inspect` 的 `time_budget_ms`/`no_ignore` 已进 schema 与工具参考，传了不再报未知字段（`inspect声明的预算参数与实现一致`、`inspect接受声明的预算参数`）；
- ✅ F-7：`reads` 顶层 `max_line`/`scan` 传播到每项（项内值优先），`reads`+`tail` 与 `block`+`ranges` 改为显式 `E_BAD_ARGS`，不再静默丢弃；
- ✅ F-9：工具参考新增「输出预算与截断」四种信号的区分表与「offset 不是快照」声明；
- ✅ 新增断言 `截断与空结果区分及续读参数可直接重放`：未见顶的 count 不标「本页可见」、显式 limit 必标；`next_offset` 必须等于本页实际条目数；
- ✅ 既有完整性断言继续全绿：`files 高密度分页`（20 页拼出 60 个文件）、`glob 预算分页`（120 条目）、`grep→block→续读`（按 ranges 拼出完整代码块）、`>32MB 窗口续读`；
- ✅ 门禁 11 步全绿（401 单测 / 213 e2e / 10 基准场景）。

未改的部分（有意保留）：offset 在并发变化目录上仍不承诺无遗漏——这是如实声明的边界，不是缺陷；本轮只把它写成契约，不引入快照机制（那会改变性能特性）。

## 11. P5 综合验收

基线：P0 提交 `cf5eee2`；候选：P2–P4 全部合入后的工作区（HEAD `1c3680d` + 本文档改动）。
两边跑同一套确定性用例，**不改任何测试夹具**，只在 P1a 补过的用例范围内增加断言。

### 11.1 逐任务对照（确定性口径）

| 项 | P0 | 现在 | 判定 | 原因 |
|---|---:|---:|---|---|
| 单测 | 383 | **401** | 改善（覆盖） | 新增 P1a 3、P2 3、P3 4、P4 6 项；无任一旧断言被删或放宽 |
| e2e | 186 | **213** | 改善（覆盖） | T06/T07/T08/T09/T13/T16 + P3 三类状态 |
| 基准场景 | 9 | **10** | 改善（覆盖） | +`>32MB 窗口续读` |
| 基准 calls（9 场景） | 65–66 | 66 | 持平 | 仅后台轮询节奏浮动 |
| 基准 out 字符（9 场景） | 44,010 | 44,010 | 持平 | 九个原场景逐字不变 |
| `instructions` 体积 | 1,262 B | **997 B** | 改善 | P2a |
| 规则块体积 | 2,850 B | **1,978 B** | 改善 | P2a；重叠率 47.1%→19.6% |
| 工具 desc+schema | 17,389 B | **17,722 B** | 退步 +333 B | P2a 描述 +31 B，P4 为 `fs_inspect` 声明了 2 个真实参数 +302 B（能力从不可达变为可达）；两处均非纯膨胀 |
| dsh 生成物 | 23,169 B | **23,560 B** | 退步 +391 B | 随 schema 同步 |
| `docs/工具参考.md` | 36,931 B | **38,534 B** | 预期内 +1,603 B | 新增截断契约表与恢复说明 |

结论：**没有任何一项在正确性上退步**；体积上工具面涨了 333 B，换取 `fs_inspect` 两个参数可用 + `E_AMBIGUOUS` 候选有预算。引导方面净减 1,137 B（instructions −265、规则块 −872）。

### 11.2 与基线相比的功能变化（用户可见）

| 变化 | 类型 | 是否需要适配 |
|---|---|---|
| `reads` + `tail` 报错 | 行为变严格 | 若旧调用依赖静默忽略，需改参数（无真实用户依赖证据） |
| `block` + `ranges` 报错 | 行为变严格 | 同上；续读只传 `block` + `start` |
| `reads` 顶层 `max_line`/`scan` 生效 | 行为变宽松 | 旧调用会得到新效果（之前被丢弃） |
| `fs_inspect` 接受 `time_budget_ms`/`no_ignore` | 能力新增 | 无 |
| `E_STALE`/`E_AMBIGUOUS`/`E_LOCKED` 文案 | 文案变 | 有解析错误文案的下游需同步（仓库内无） |
| `fs_write files` 中途失败改报 `E_BATCH_PARTIAL` | 错误码变 | 依赖旧 `E_BATCH` 的下游需同步 |

### 11.3 逐阶段回滚方式

每个阶段都是独立提交，可单独 revert：

| 阶段 | 提交 | 回滚方式 | 回滚后需重跑 |
|---|---|---|---|
| P0 + P1a | `0457083` | `git revert 0457083` | 仅测试与文档，无产品影响 |
| P2a | `5b6eb76` | `git revert 5b6eb76` | 引导回到旧文案；工具行为不变 |
| P3 | `84d12be` | `git revert 84d12be` | 恢复提示回到旧顺序；保护未变 |
| P4 | `1c3680d` | `git revert 1c3680d` | `fs_inspect` 参数重新不可用，静默忽略回归 |
| P5 | 本文提交 | `git revert <P5 提交>` | 仅文档 |

回滚后必须重跑 `scripts/check.ps1 full`，并重新生成 DSH 定义与文档清单（`node scripts/gen_dsh_specs.mjs` / `gen_docs_tools.mjs`）——生成物不在 revert 范围时可能指向不存在的参数。

### 11.4 P5 未完成的部分（如实列出）

- 真实 Agent 侧三项验收（任务成功率、无效调用、恢复调用中位数）**未测**：需要模型与付费授权（P1b）。
- `docs/轮次实测.md` 的旧数据仍是 n=1 小样本，本轮只修正了口径表述（区分上下文体积 / usage / 缓存 / 账单），**没有重跑**那些对照。
- 发布准备（升版本、`release.py prepare`、push、发布）**未做**，本阶段只做验收与文档。

