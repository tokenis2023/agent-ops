# 变更记录

本项目使用语义化版本。日期为发布日。

## [Unreleased]

### 减少 Agent 轮次（token = 钱：每轮都要重发整个上下文）

- **`fs_write` 支持 `files` 批量**：一次建/覆盖多个文件，全部预检通过才写入，失败返回
  `E_BATCH`。此前 `fs_edit` 早就有 `files`，`fs_write` 没有——改 10 个文件 1 轮，建 10 个文件
  10 轮。真实评测的任务流里，建 12 个文件占了 12 次调用，现在 1 次。
- **`fs_grep` 的 `matches`/`snippets` 首行带摘要**：`ok 共 N 个文件 M 处命中；最多：a:3, b:2…`。
  原来模型必须先 `output=files` 看范围、再 `output=count` 看分布，才敢回来看内容——同一个
  pattern 白花两轮。现在一行给出分布，一次调用足够。为此让串行/并行两条扫描路径都收集
  每文件命中数（只多一个 `String+usize`）。
- 代价：schema 增加 298 字符（约 187 token/轮），换掉的是 1–14 轮不等的上下文重发。
- **`fs_edit` 检测「半行 old」并给警告**：`old` 恰好截断在词中间时（实测踩过
  `# mini-task-servic` 少一个字母，把标题改成 `# 新标题e`），成功回执追加
  `⚠ 提示：…（原文 `# mini-task-service`）` 并给出原文整行。只在一侧相邻字符与 `old` 边界
  字符同类（字母/数字/下划线）时触发，整词与行内片段替换零误报。消掉的是「改完不自知、
  等下一轮 fs_read 才发现、再改一次」的三轮返工。
- **`fs_grep` 新增 `expand=block`**：把每条命中展开成所在完整代码块（花括号配平，命中行 `:`、
  其余 `-`），省掉「grep 拿行号 → `fs_read block=true` 再读一遍」那一轮——这是编码任务里
  最高频的两步循环。同一块内多命中只输出一次；非花括号语言（Python 等）自动退回命中行，
  不假装展开。复用 `fs_read block=true` 已有的 `enclosing_block` 边界推算。
- **`fs_grep` 默认 `limit` 100 → 200**：真正的输出上限是 `budget`（默认 32KB，渲染层按预算收敛），
  `limit` 只防一次取太多。100 条常卡在翻页边界，模型要多跑一轮 `offset=100` 才拿到剩下的，
  而那一轮要把整个上下文重发一次。
- **顺手压缩 schema**：`files[].edits` 原先复制一份完整的 7 字段说明（约 700 字符），
  schema 每轮都进上下文，这是纯浪费。嵌套处只留字段名与类型，顶层仍是完整描述；
  配套把「两处嵌套结构一致」的守卫从「逐字节相同」放宽为「字段名+类型+enum 一致」，
  漂移仍会红灯。省下的 341 字符正好抵掉 `fs_write.files` 新增的 297 字符——
  **批量能力是免费的**（总 schema 反而从 9887 降到 9844）。

### 修复（真实 Agent 调用评测）

- **DSH 插件：`fs_exec.env` 在插件形态不可用**。生成器把 MCP 的自由对象（`{"type":"object"}`）
  投影成「空对象 + `additionalProperties:false`」，宿主会拒绝任何环境变量键。现在自由映射投影为
  `json`，插件形态可正常注入 env。
- **DSH 插件：`fs_exec` 描述与 schema 漂移**。描述仍教模型用 `job`/`list`，但 schema 已剔除；
  现在生成器同步覆写描述，说明插件形态不提供后台任务参数，传输层也会显式拒绝这五个参数并提示
  改用 MCP 形态（不再出现「文档说没有、运行时却穿透」）。
- **DSH 插件：取消语义**。取消（abort）单个进行中的写/执行请求时会结束长驻会话，避免取消后
  fsops 继续写盘/跑命令；有并发在途调用时不杀会话，避免失败重放导致重复写。
- **DSH 插件：长驻会话按 cwd 增加 LRU 上限（8 个）**，避免长命 Host 切换目录时 `fsops mcp`
  进程无界累积。
- **文档：修正安全表述**。README/SECURITY/使用指南不再声称「隔离依赖 Harness 沙箱」——DSH 插件
  是 Host 代码，Harness 的 fs 沙箱与审批不覆盖 `fs_*`；补 MCP 形态 + 宿主沙箱的隔离建议。
- **文档：安装与接线**。安装建议固定 tag/commit；补安装后自检（`fs_stat detail=server`）、
  错误分层（宿主英文 `invalid arguments:` 与内核中文 `E_*`）、`shell=true` 用法
  （完整命令行放进 `program`，`args` 会被引号化）。
- **`package.json` 补 `engines.dsh` 兼容声明**。
- **Rust 错误提示**：`E_BAD_REGEX` 附 Rust regex 方言说明（不支持 lookaround 与反向引用）；
  `E_IS_DIR`/`E_BINARY` 补下一步；`fs_edit` 的 `E_READONLY` 与 `fs_write` 对齐给出
  PowerShell 命令。

## [0.7.0] - 2026-11-07

### 修复（Agent 友好度评审的 P0）

- `fs_edit` 同时给 `old` 与 `pattern`（或 `new` 与 `pattern`、`replacement` 不带 `pattern`）现在直接报 `E_BAD_EDIT`。
  旧实现只要给了 `pattern` 就 `continue`，`old`/`new` 被静默忽略：模型以为字面量生效，
  写盘后内容错且没有任何信号——这是评审里唯一的「静默写错」。
- `E_AMBIGUOUS` 的建议改成可照做的说法：`all`/`start`/`end` 属于 `edits[]` 内的单条修改项，
  并显式说明传到顶层会被 schema 拒绝。原文案直接写「设 all=true」，模型照做会撞
  `E_BAD_ARGS ... 是未知字段`，把一个错误变成两个。
- `E_BAD_ARGS` 对 `fs_edit` 的字段级关键字（`old`/`new`/`pattern`/`replacement`/`all`/`start`/`end`）
  提到顶层时给定向指引，不再机械地猜「最接近的合法字段是 path」（而 `all` 根本不是拼错）。
- `fs_exec` 非零退出不再用 `ok` 前缀：`ok exit=3` → `exit=3 非零`。`check` 默认 false 时它确实是
  成功返回，但 `ok` 会让模型把编译/测试失败当成功。
- 多编辑中途失败时，错误里的文件片段标注为「内存中间态，本次未写入任何文件」。
  原来预览显示的是「已应用前几处」的内存态，与磁盘不符，模型会误判已部分写入。

### 性能

- DSH 插件改为复用**一条长驻 `fsops mcp` 会话**（换行分隔 JSON-RPC、按 id 匹配响应、
  `fsops mcp` 自带 4 个工作线程所以并发只读调用是真并行），不再每次调用新起进程：
  实测 `fs_read` **33.5ms → 0.54ms**、`fs_stat` **32.9ms → 0.34ms**、`fs_grep` 1.8ms、
  4 路并发一轮 0.59ms（首次调用付一次 spawn，约 35ms）。
  一次性 spawn 的 33ms 里工具本身 <0.5ms，其余是 Windows 进程创建（~9ms）、
  隐藏 console 分配（~17ms）和镜像加载（~6ms）——这部分只能靠复用进程摊销。
- 会话级故障（spawn 失败、进程退出、初始化失败）自动退回一次性 `fsops call`，功能不降级；
  可用 `mcp: false` 强制一次性调用。插件释放时关闭会话，并让挂起请求立即失败而不是干等。
- 可执行文件解析结果缓存，不再每次调用重走候选路径探测。
- 新增 `node tests/bench_dsh_latency.mjs`：对比一次性 spawn 与长驻会话，输出 min/med/p95/max。
- 冒烟测试新增两条回归守卫：全部工具调用只 spawn 一次 `fsops mcp`、正常路径不退回一次性 spawn。

### 插件接入

- **修掉版本静默降级**：`resolveFsops` 以前只要找到 `%LOCALAPPDATA%\fsops\fsops.exe` 就直接用，
  不核对版本——插件升到 0.6.0 而那里躺着 0.5.0 时，schema 是新的、跑的是旧内核，
  参数差异直接变成 `E_BAD_ARGS` 循环，而且没有任何提示。现在对 `%LOCALAPPDATA%\fsops` 与
  PATH 上的候选跑一次 `fsops version` 比对：不一致就用版本匹配的缓存/下载，都不行则报
  `E_VERSION` 并点名实际版本与三种修法；包内 `bin/` 与按版本命名的缓存仍不探测（零额外开销）。
- 显式来源（配置 `binary` / `FSOPS_BIN`）保持权威，不做事后推翻；确需旧版本可用
  `allowVersionMismatch: true` 或 `FSOPS_ALLOW_VERSION_MISMATCH=1`。
- 冒烟测试新增 5 项：版本解析 4 项（含 `v` 前缀、空输出、乱文本）与「显式 binary 不做版本探测」（65 项全绿）。

### 参数表述（让模型在调用前就能拼对）

- `fs_read` 的「模式四选一」改为「模式表」：`path` 与 `paths` 实为**并集**（实测同给会读两个文件），旧文案会让模型以为非法；`focus`/`tail`/`summary` 的互斥对象逐条列出。
- `fs_file` 补 op→必填映射（copy/move/archive/extract 要 `src`+`dst`；remove/mkdir/convert 要 `path`；convert 需 `encoding`）——原来只活在文档表格里，schema 看不到。
- `fs_exec` 补「启动必须给 `program`」；`fs_stat` 补「`path`/`paths` 至少一个，`detail=server` 可免路径」。
- `fs_edit` 写明 `old`+`new` 与 `pattern`+`replacement` 必须二选一，同时给报 `E_BAD_EDIT`。
- `fs_grep` 的「Rust/PCRE 风格正则」更正为「Rust regex 语法（不支持 lookaround 与反向引用）」：引擎是 `regex` crate，按 PCRE 写 lookbehind 必然失败且原错误无引导。
- DSH 插件 `fs_exec` 描述里「每次调用都是新进程，不支持长驻后台任务」已过期（现复用长驻会话），改为说明为何仍不暴露后台参数；`check` 参数也不再写「非零仍是 ok」。
- 三处手写 schema（Rust / DSH / 工具参考）同步；单测与冒烟断言从钉住「四选一」改为钉住「模式表/并集」，避免测试继续维护一个错误事实。

### 工具定义单一真源

- 新增 `scripts/gen_dsh_specs.mjs`：把 `fsops schema`（Rust 端 `tool_defs()`）投影成 DSH 的
  value-schema DSL，生成 `dsh/tool-specs.generated.js`。`dsh/index.js` 不再手写 `TOOL_SPECS`，
  描述与参数从 Rust 直接流出——手写三份时漂移过 11 处、评审时仍有 5/8 个工具描述不一致，
  模型靠 description 拼参数，这等于直接制造拼错。
- 投影规则覆盖两套 schema 的差异：丢弃 DSL 不支持的 `minimum`/`maxItems`/`minItems`，
  把 MCP 的对象级 `required: [...]` 下推成属性级 `required: true`，插件刻意不暴露的
  `fs_exec` 后台任务参数集中声明在 `EXCLUDED` 里（不再靠「手写时少写几个」）。
- 冒烟测试新增两项漂移守卫：生成物必须与 `fsops schema` 逐字一致、渲染结果必须稳定可提交
  （已用「合法篡改生成物 → 红灯」反向验证守卫真的会响）。冒烟 65 → 67 项。

### 核心行为（所有客户端共用）

- **陈旧优先于换行**：`fs_edit` 同时命中「文件已变」与「混合换行」时，现在先报 `E_STALE`
  并在同一条里补一句换行事实；旧实现先报 `E_MIXED_EOL`，模型要先归一、再重试才被告知
  hash 也变了（两轮）。「外部追加」恰好是同时制造这两种状态的常见路径。
- **全量覆盖不再被源文件混合换行拦住**：`fs_write` 是整文件替换，源文件的混合换行已无意义；
  只要待写内容本身风格稳定，直接按内容风格写，不再要求 `normalize_eol`。
  待写内容自身混合、且目标不存在时仍需显式归一（不变）。
- **`max_output` 钳制不再静默**：请求值低于最小值 1024B 时，输出首行回显
  `max_output=1024B（请求 200B，已按最小值执行）`；未钳制时不加噪音。

### 路径错误可恢复性

- 缺文件/目录时给可直接执行的下一步：`用 fs_glob pattern="*文件名*" 定位，或 fs_stat 确认路径`，
  不再只回裸 OS 错误（旧行为只有「同目录有相近名字」时才给提示，其余情况无任何引导）。
- 修掉一个**从未生效的判定**：比较串写成「找不到指定路径」，而 Windows 实际报
  「找不到指定**的**路径」，少一个「的」让 os error 3 整条提示分支永远不命中。现在按
  `os error 2/3` 与多语言片段共同判定。
- 路径里的 `$env:NAME` / `%NAME%` 会直接点破「不会被展开（原样当字符）」并给出做法，
  不再报 `os error 123` 这种与真实原因无关的错误（模型写 `$env:TEMP` 是高频行为）。
- 新增 4 个 Rust 单测：目录不存在、无相近候选、两种环境变量形态、普通文件名里的 `%` 不误判。

### 测试

- 新增 5 个 Rust 单测：`old`/`pattern` 互斥 3 个、多编辑中间态提示 1 个、
  `fs_edit` 顶层字段指引 1 个；并给 `fs_exec` 非零退出的前缀加了反向断言。
- `cargo test --lib` 348 → 353 项全绿；`cargo clippy -D warnings` 干净；DSH 冒烟 57 → 65 项全绿。

## [0.6.0] - 2026-11-07

- 新增 `fsops upgrade [--check|--force]` 自更新：跟随 `releases/latest` 取最新 tag，下载二进制与 `.sha256` 并校验，SHA-256 不匹配时中止且不改动任何文件；替换采用「先改名再落新版」，失败自动回滚，旧文件保留为 `fsops.exe.old-*` 并在下次升级自动清理。网络瞬时失败重试 3 次。
- `fs_exec` 的 `program` 用相对路径或 POSIX 形式且启动失败时，错误直接说明「相对路径不随 `cwd` 解析」，不再让模型按 cwd 猜路径。
- `initialize.instructions`、`AGENTS.md`、使用指南补充：前台 `fs_exec` 控制在 2 分钟内（宿主超时会丢结果）；更长的构建/测试用 `background=true` + `job wait_ms=60000`。
- 发布工具链：`scripts/release.py` 新增 `check`（版本一致性，接入 CI 与 `check.ps1 full`）、`prepare`（升版本 + 同步工具参考/使用指南引用 + CHANGELOG/发布正文骨架）、`fill`（用构建产物回填正文 SHA-256/大小）。
- 新增 `tests/release_tooling.py`（临时仓库夹具，21 项断言）；CI 与 `check.ps1 full` 从 7 项门禁扩到 9 项。
- 版本 0.6.0，新增 8 个 Rust 单测（自更新 7 + 相对 program 1）。

## [0.5.0] - 2026-11-07

- `fs_grep` 的 `parallel` 改为三态：省略即自动（小仓库串行，满一波候选后并行），`true`/`false` 强制。Auto 在满一波前逐个串行扫描，保留「满页即停」，不因并行多访问后续目录项。
- 实测（release，5 次中位）：40 文件与串行持平（5.9ms vs 6.3ms），500 文件 52.6ms → 31.9ms（1.65×），3000 文件 330.3ms → 178.7ms（1.85×）；自动/强制并行/串行三种输出逐字节一致。
- 修复 `fs_exec` 的 `cwd` 不经路径层：`/d/repo` 原样交给 `current_dir` 报 `os error 267`，还把目录问题误导成 PATH 问题。现与文件工具同一套解析（POSIX / 原生 / `~/` / 相对路径），不存在时返回 `E_BAD_ARGS` 并给出解析结果。
- 修复一行安装命令在 Windows PowerShell 5.1 下因 BOM 解析失败：`irm` 把 BOM 解成 `ï»¿` 导致 `[CmdletBinding()]` 不再是首语句，改用 `Net.WebClient` 取原始字节 + 去 BOM + `scriptblock::Create`。
- schema、DSH 插件描述、`工具参考`、`AGENTS.md` 同步自动并行语义与 cwd 路径形式。
- 版本 0.5.0，新增 8 个单测（自动并行 6 + cwd 2），`check.ps1 full` 七项门禁全绿。

## [0.4.1] - 2026-11-07

- 修复 `fs_read block=true` 在缩进语言（Python 等）上找不到花括号块时静默读到文件末尾：现在文件头明确提示已按 `start..end` 普通读取，并给出 `fs_grep` 定位后传 `ranges` 的替代路径。
- 修复长行截断尾注把单行成本说成全文成本：改为「最长截断行约 N 字节，重读该行需 max_line=0 且 budget≈M」。
- 修复 `fs_read ranges` 乱序输入丢行：先全局排序再合并，`[[10,14],[1,6],[12,20]]` 不再把 `[1,6]` 吞进 `[10,14]`。
- 修复 `fs_read summary` 在预算紧张时尾注被硬切成「或调大 bud」：按实际尾注长度预留，连文件头都放不下时返 `E_BUDGET` 并给出所需字节数。
- 修复极小 `budget` 误报 `E_LINE_TOO_LONG`：那是文件头加续读尾注的成本，现在报 `E_BUDGET`。
- `summary` 的 Rust 测试模块识别改为字符串感知的括号计数，并覆盖 `mod *_tests` 命名；`tools.rs` 符号数 133 → 39（22 个正式声明 + 17 个测试后 item）。
- `fs_glob map=true` 把 `tests/`、`benches/`、`*_test` 文件排到实现文件之后；「还有 N 个符号」处提示用 `fs_read summary=true` 看完整结构。
- `fs_read focus` 在数据文件（JSON/YAML/Markdown 等）上只给命中行 ±2 行，不再按花括号扩成整个文件；提示文案从「命中 N 个代码块」改为「命中 N 处」。
- `fs_stat` 多文件部分失败时首行给出 `ok stat N 项：X 成功，Y 失败`，不再让一项失败淹没其余结果。
- `fs_read` 默认预算从 12 KB 提到 32 KB：多一轮续读就是一次完整上下文重发，比多给几 KB 贵；小文件自适应同步提到 ≤32 KB，并按行数补足行号开销（原来 4096 的余量对多行文件不够，仍会被截成续读）。
- 读取缓存（未变更 stub）默认关闭，`FSOPS_MCP_CACHE=1` 显式开启：stub 省的是单次响应，代价是模型多花一轮 `force=true`。
- `E_NO_MATCH` 不再建议「取新 hash 用 expect_hash 重试」：绝大多数是 old 抄错，并发冲突由 `E_STALE` 承担；文案改为直接用候选行作为 old。前缀匹配全部落空时（拼错一两个字母）按编辑距离找最相似行，`alfa` 也能拿到 `alpha`。
- `fs_glob detail=true` 的文件行去掉冗余 `f ` 前缀（只在目录行保留 `d `）。
- `fs_grep` / `fs_glob` 默认预算 8192 → 16384：snippets 30 条以上不再频繁翻页，少一轮续取。
- 文件不存在时（`E_READ`）列出同目录最相似的 3 个条目，文件名拼错不用再跑一轮 `fs_glob`。
- `fs_read paths` 多文件按大小分配预算：小文件优先读满，剩余给大文件（实测 20 文件场景大文件从 40 行提到 72 行，同样的 32KB 预算）。
- `fs_read` 新增 `tail=N`：只读最后 N 行，免去「先 `fs_stat` 拿总行数再算 start」那一轮；超大文件不支持 tail。
- `E_AMBIGUOUS` 的列标记从正文内移到行尾（`← 列 N`）：原实现把 `^` 插在命中列，会直接吞掉该列字符（`x();` 显示成 `^();`）。
- 多文件读取的续读尾注带各自路径（`续读: fs_read path=… start=N`），不再是一串无法对应的「同 path」。
- 版本 0.4.1，新增 14 个单测。
- 输出与错误全面自带「可重放下一步」：`reads` 续读带各自路径、长行尾注给被截行号与重读命令、分页 `next_offset` 给完整命令并标注单位、`E_STALE`/`E_BATCH_PARTIAL`/`E_ENCODING_LOSS`/`E_DOCX` 都给出可直接照做的下一步，repo map 截断尾注给调法。
- `fs_read` 正文逐行原样输出（不再折叠连续空行），可直接当 `fs_edit` 的 `old`；主路径与超大文件分支的 `utf-8-lossy` 都显式警告 U+FFFD 不是原文、切勿据此写回。
- `validate_args` 在入口执行 schema 声明的 `required`/`enum`/`minimum`/`maxItems` 与数组项必填；schema 生成不再覆盖显式 `minimum`。
- `fs_grep` 新增 `max_line`（默认 240 与 `fs_read` 一致，`0` 不截断），命中长行一次拿全；移除硬编码 200 字符窗口。
- `dry_run` 不再抢占独占写锁；`E_LOCKED` 说明已等待时长、本次未写入、参数不变直接重试。
- schema 可见性：`fs_edit` 描述补正则替换，`force`/`reads`/`detail` 描述与实际行为对齐；`install.ps1` 升级后提示 `/reload-mcp` 与清 schema 缓存。
- DSH 插件不再暴露 5 个不可用后台参数；冒烟测试改为参数超集比对并新增 description 能力词比对；DSH 包补齐 `docs/` 与根目录文档。
- `fs_stat` 路径统一用双引号包裹，含空格路径可稳定切分；`E_MIXED_EOL` 归一到原文件主换行风格（多数行风格，平局取 LF），不再一律改成 LF。
- 本批 8 轮修复全程 TDD，`check.ps1 full` 七项门禁（含 185 项真实 MCP e2e、任务基准、DSH 冒烟）保持全绿。

## [0.4.0] - 2026-11-07

- 修复 `fs_exec` 的 schema 把 `program` 标为必填：只轮询/终止后台任务的 `{"job":1}` 会在宿主 schema 校验处就被拒，而运行时本就允许省略。DSH 插件与工具参考同步。
- `fs_exec` 新增 `check=true`：非零退出按 `E_EXIT code=N` 返回，默认仍是 `ok`；跑必须成功的命令时不必再人工判断退出码。
- `fs_edit` 的 `E_AMBIGUOUS` 附前 3 处命中位置的 ±2 行片段：构造唯一的 `old` 不再需要额外一次 `fs_read`。
- `fs_read reads[]` 每项支持 `budget` 与 `focus`：大文件可单项多要预算，多文件按模式取块一次完成。
- `fs_read` 的 description 增加模式互斥表（path/paths/reads/focus 四选一），模型在发出调用前就能拼出合法参数。
- `fs_stat detail=server` 返回 `fsops 0.3.0 tools=8`：宿主重载后一条调用即可自检连接的服务端版本与工具数。
- `fs_exec list=true`：列出当前会话的后台任务（id/pid/状态/时长），丢了 job id 也能自救；启动文案明确标注「会话内」任务，进程随 MCP 会话结束。
- `fs_read reads` 输出首行改为 `ok reads N 项：X 成功，Y 失败`——单项失败不再淹没在正文里；MCP 的 `isError` 保守置 true 时，调用方可从首行判断还有多少内容可用。
- 新增 `fsops schema` 子命令：输出 8 个工具的 MCP schema JSON；DSH 冒烟测试用它逐工具比对插件参数集合，三处手写 schema 的漂移从此变红灯。
- 文档防漂移测试：扫描源码全部 `E_*` 错误码，断言工具参考覆盖每个码与每个工具章节。
- `fs_exec job` 支持 `wait_ms`：最多等待 N 毫秒（上限 60000）直到任务退出再返回——跑短命令从「反复轮询」变一次调用；超时仍运行则返回增量输出并标注。
- `focus` 与 `fs_grep` 的正则描述标注「JSON 字符串里反斜杠需双写」，减少一层转义错误。
- `E_LINE_TOO_LONG` 的「当前 budget=N」改为「渲染剩余预算 NB」：那是收敛后的剩余额度而非请求值，原措辞会把调参引向错误方向。
- `fsops mcp --jobs 0/1` 不再走读线程串行 dispatch：升为单工作线程，避免 `background`/`wait_ms` 把读线程占住、后续请求（包括 kill 自救）全部排队；控制类请求仍在读线程就地应答。
- 使用指南新增「升级后工具面仍是旧版」排障章节：宿主按连接配置指纹缓存 schema，先用 `fs_stat detail=server` 确认连的是哪一版，再按需需要 `/reload-mcp` 或删除 `mcp_schema_cache.json`。
- `wait_ms` 测试改为断言「退出码 + 输出完整」，不再依赖 `ping -n 2` 的墙钟时间。
- `fs_edit` 的 loci 在与前一处同一线路号时追加列号（`1,1:11,1:19`），解决同行多处修改无法区分的问题；单处修改仍为简洁的行号。
- 工具参考补一句：读取缓存对请求类型敏感，`summary`/`focus` 不会命中普通读取的缓存。
- `fs_edit` 的 `E_AMBIGUOUS` 错误附带列号：同一行多处匹配时，错误标题写明「行 1 列 1,9」，片段里的命中行也用 `^` 指针标出命中列。

## [0.3.0] - 2026-11-06

- 修复 `fs_file copy/move` 的输出把目标路径写成 Windows 反斜杠（`C:\...`），与「输出统一 POSIX」的约定不一致；新增 `paths::display_native`，transfer/extract/collect_files 的全部中间路径输出统一走它。
- 修复 `fs_file move` 同盘 rename 报 `0B`、目录恒报 `1 个文件`：改为统计目标真实大小与文件数。
- 修复 `fs_file convert` 到 `utf-8-bom`/`utf-16le`/`utf-16be` 不写 BOM：按目标编码写 BOM，Windows 编辑器不再把 UTF-16 当二进制。
- 修复 `fs_exec job=<id>` 的增量解码：进程把多字节字符拆在两次读取之间时，不再解出替换字符或整块丢字；只解码到完整 UTF-8 边界。
- 修复 `fs_read reads` 的预算：严格按项均分，连文件头都放不下时返回 `E_BUDGET`，不再每项强留 256B 导致总输出反超 `budget`。
- CLI 退出码改用与 MCP `isError` 相同的 `E_` 行判定，多文件响应里夹在正文中的错误也会返回 2。
- schema 补齐：`fs_edit.files[]` 新增 `normalize_eol`；`fs_file.encoding` 补 `utf-8-bom`。
- DSH 插件与 MCP 对齐：8 个工具、`reads` 项补 `ranges`/`scan`、`fs_read`/`fs_glob` 描述补齐；明确 `background/job/kill` 在 DSH 下会返回 `E_NO_JOB`。
- 工具参考补「文件管理与执行」错误码：`E_CONFIRM_REQUIRED`、`E_EXISTS`、`E_IO`、`E_ZIP`、`E_NO_JOB`、`E_JOB_LIMIT`、`E_JOB`、`E_BAD_REPLACEMENT`、`E_INTERNAL`。
- 文档对齐 0.2.7 的实际能力：README、使用指南、CONTRIBUTING 更新为 8 个工具，补齐 `fs_exec`/`fs_file`，修正 Rust 最低版本（1.88）和「不提供删除/移动/复制」等过时边界；使用指南新增「执行命令」「文件管理」两节。

## [0.2.7] - 2026-11-06

**减少轮次**

- `fs_read reads=[{path,start,end,…}]`：一次读多个文件的**不同区间**，最多 20 项，预算按项均分；单项失败只影响该项。
- `fs_read focus=<正则>`：只返回命中所在的完整代码块（自动展开并合并区间），替代「fs_grep 拿行号 → block=true」两步；最多 12 块。
- `fsops mcp --jobs N`：`tools/call` 用固定线程池并发处理（默认 4，0/1 串行），响应按 id 匹配；initialize/ping/tools/list 仍在读线程就地应答。
- 引导升级：`initialize.instructions` 与 AGENTS 规则写入「同一回复可并行多个只读调用 + 已知多文件用 reads 一次读完 + 按模式取块用 focus」。

**减少体积**

- `fs_read` 未变更短路：同一 MCP 会话内同文件同 hash 同区间再读只回一行摘要（带 hash/区间/总行数/首行锚点），实测重复读输出约为正文的 1.1%；`force=true` 可重读，`FSOPS_MCP_CACHE=0` 可关闭。
- `fs_read` 自适应预算：单文件、未显式传 `budget`、≤16 KB 时自动放开到全文，12–16 KB 文件不再被 12288 截断逼出第二次续读。
- `fs_glob map=true`：repo map lite。按「符号被引用程度 + 入口点 + focus」排序输出文件与关键符号，一次调用回答「先改哪些文件」；数据文件与 >256KB 跳过，最多 2000 文件。

**替代 PowerShell**

- `fs_exec`：用 argv 数组直接启动程序，不经过 shell——没有引号/插值、`2>&1`、ErrorRecord、`$LASTEXITCODE` 问题；非零退出不是错误，超时结束整个进程树。
- `fs_exec` 后台任务：`background=true` 立即返回 job id，`job=<id>` 轮询增量输出，`kill=true` 结束进程树；适合 dev server/watch。只在长驻 MCP 会话内有效。
- `fs_file`：copy/move/remove/mkdir/convert/archive/extract，替代 PowerShell 的 Copy-Item/Move-Item/Remove-Item/New-Item 与 Compress-Archive/Expand-Archive；`remove` 必须 `confirm=true`，`extract` 拒绝 zip-slip。

**工具面**

- 6 → 8 个工具；`remove` 强制 `confirm=true`，`extract` 拒绝 zip-slip，`fs_exec` 明确不是安全沙箱。
- 测试：单测 278、e2e 185、task_bench 9 场景。

## [0.2.6] - 2026-11-06

- 开发：新增 `scripts/check.ps1` 分级检查（`fast`/`commit`/`full`），并按档位更新 `CONTRIBUTING.md` 与 `AGENTS.md`：编辑过程中只跑改动模块单测，提交前跑 `commit`，push 前或改 I/O 内核跑 `full`，不再每次改动都跑全套。
- `fs_read summary=true`：输出「行号|声明」的结构摘要，不展开函数体。按扩展名识别 Rust/Python/JS/TS/Go/Java/C#/C/C++/Ruby/Shell，跳过 Rust 的 `mod tests`。配合 `block=true start=<行号>` 形成「先看地图再进门」，省掉为定位而做的一次全文读取；实测 183 KB 的 `tools.rs` 摘要 5.8 KB（省 96.8%）。
- 仓库根新增标准 DSH 插件（`package.json` + `cordis.patch.yml` + `dsh/`）：在 DeepSeek Harness 的插件管理里粘贴仓库地址即可安装，6 个 `fs_*` 工具与 MCP 形态同名、同契约。二进制优先复用已安装的 fsops，缺失时按版本从 GitHub Release 下载并校验 SHA-256。
- `fsops call <tool> -` 支持从标准输入读 JSON：Windows 命令行约 32 KB 上限，DSH 插件用它传大段 `content`/`edits`。
- `E_NO_MATCH` 补一句「若他人刚改过此文件，重新 fs_read 取新 hash 后用 expect_hash 重试」：多个 agent 并行时，失败方原先会以为自己的原文写错。
- 文档新增「多个 Agent 同时使用」：写锁是按文件的跨进程命名互斥量，读-改-写与 hash 校验都在锁内；实测 20 进程并行改不同行 20/20 成功、同改一行时失败方得 `E_NO_MATCH` 且文件不损坏、各种路径写法映射同一把锁。同时说明单宿主请求串行、锁限于同一登录会话、符号链接不归一这几个边界。

## [0.2.5] - 2026-11-06

- `fs_edit` 支持正则替换：`edits` 每项可用 `{pattern, replacement}` 代替 `{old, new}`，替换范围内所有匹配并支持 `$1` 捕获组。用于「不知道确切原文、只知道形状」的场景——值各不相同的 30 行配置一次改完，连为抄原文而做的那次 `fs_read` 也省掉。同样支持 `start`/`end` 限定行范围与 `diff=true` 自查。
- 实测：典型「读→改→再读」3 次调用 2,579 字符，改用 `fs_edit diff=true` 后 1 次调用 160 字符（省 94%）。
- 正则模式的错误提示改为「检查转义，或先用 fs_grep 确认形状」，不再复用字面量模式的「请先 fs_read」（那恰好违背用正则的初衷）。

## [0.2.4] - 2026-11-06

- `fs_edit` 新增 `diff=true`：返回被替换的原文/新文片段（`@@ 行号` + `-原文` / `+新文`），改完结构可立即自查，省掉一次 `fs_read` 回读；`dry_run` 也能配合使用，先看后写。默认关闭，单次最多 60 行，超出标「已截断」。
- 文档：`使用指南` 的 Hermes 章节按实测重写——`tools.tool_search.defer: []` 对 MCP 工具无效；真强制只能靠专用 profile 关掉 `file`/`coding` 工具集，或用 CLI 兜底；升级后需 `/reload-mcp` 才加载新 schema；用 `state.db` 里的 `mcp__fsops__%` 计数确认是否真被调用（`hermes mcp test` 只证明能握手）。

## [0.2.3] - 2026-11-06

- `fs_edit` 的 `files[].edits` 嵌套 schema 补全字段：此前只写 `{"type":"object"}`，模型在批量编辑时看不到 `old/new/all/start/end`，只能猜。两处现在共用同一个 schema 函数，避免再漂移。
- 0 字节的 DOCX/PDF 报「文件为空（0 字节）；可能下载或生成未完成」，不再抛底层解析错误把模型引向「修复格式」。
- `install.ps1` 失败时给出可执行的错误（`E_VERSION` 版本不存在、`E_INSTALL_DIR` 安装目录无效、`E_NETWORK` 无法查询 Release），不再只抛 PowerShell 堆栈。
- 规则正文与 `initialize.instructions` 补上三项高价值用法：`fs_grep` 拿行号 → `fs_read block=true` 取整个代码块、`fs_edit` 的 `start`/`end` 限定行范围、`fs_grep output=count` 统计。此前这些能力只在参数描述里，模型不会主动用。
- `integrate --check` 现在比对规则正文，不再只看标记：fsops 升级后正文变化会报「过期」，此前标记仍是 v1 就报 `ok`。
- `fsops integrate --check` / `--remove` / `--dry-run` 不再强制要求同时给出 `--rules`（意图已由参数本身表明）；裸 `fsops integrate` 仍提示需要 `--rules`。
- `fsops integrate --check` 的状态文案改为中文（`无块`/`过期`/`损坏`），与 `fsops doctor` 一致；此前同一语义一处报 `missing` 一处报「缺失」。

## [0.2.2] - 2026-11-05

### 新增

- `fs_read block=true`：返回**包含** `start` 行的整个代码块——命中行在函数内部时向前找块起始、向后配平结束，配合 `fs_grep` 的行号可一次拿到整个函数（实测 170 行的函数一次到位，此前只返回单行）。
- `fs_edit` 的每条 `edits` 支持 `start`/`end` 行范围：同一字面量在文件里多处出现时，配合 `all=true` 只改指定区间（例如只改某个函数内的那些），不必为每一处构造不同的上下文片段；区间内无匹配时报 `E_NO_MATCH` 并告知文件其他位置的匹配数。
- `fsops harden claude`：安装 Claude Code `PreToolUse` hook，拦截文本文件的 `Read/Grep/Glob` 并引导到 `fs_*`；支持 `--user`、`--check`、`--remove`、`--dry-run`。PDF、图片和二进制继续放行内置 `Read`。
- `fsops hook claude`：Claude Code hook 处理器，按扩展名决定 deny 或放行。
- 文档：`docs/工具参考.md` 补齐全部 42 个错误码的含义与下一步动作（此前只散落提到 10 个）。

### 修复

- `fs_grep output=count` 默认不再受 `limit=100` 截断（统计必须准确，此前 203 处命中会报成 100）；显式传 `limit` 时仍截断并标注。
- 长行截断的尾注与 `E_LINE_TOO_LONG` 给出所需 `budget` 数值（如 `budget≈112200`），不再只建议「用 max_line=0」——单行文件常达上百 KB，照做会撞上 `E_LINE_TOO_LONG` 形成死循环。
- `E_ENCODING_LOSS` 报出具体的不可表示字符、码位与所在行（如 `U+1F600 '😀'（第 3 行）`），不再只说「包含无法表示的字符」让模型整段重写反复试。
- `E_TOO_LARGE` 超大文件提示改为「先用 fs_grep 定位行号，再用 start/end 读取该区间」；`E_BUDGET` 续读被截断时给出所需 budget 数值。
- `E_READ` 遇到「双重盘符」路径（如 `/d/c/Users/x`）时指出格式问题并给出正确形式，不再只报「系统找不到指定的路径」让模型误以为文件不存在。
- `doctor` 区分「规则文件不存在」与「文件存在但没有 fsops 块」：前者可直接新建，后者必须追加。此前两者都报「缺失」，会诱导模型用 `fs_write` 整文件覆盖用户已有的 `AGENTS.md` / `CLAUDE.md`。
- `fs_glob` 预算截断时说明「已给 N 条（共 M 条匹配）」，不再只报「输出截断」让模型无从判断。
- 修正 `docs/工具参考.md` 的续读示例与实际输出不一致（实际提示不带 `end=`）。

- `fs_write` 目标是目录时返回 `E_IS_DIR`，不再泄漏底层 `E_RENAME 拒绝访问`。
- `fs_stat` 对符号链接给出 `-> 目标路径 目标大小`，不再只报链接自身的 `0B`（会被误读成目标为空）。
- 超大文件（>32MB）读取的续读尾注不再被预算截成 `续读: start=112` 这种缺 `end=` 的碎片；同时把文件头计入预算，响应不再超出 `budget`。
- 重构：`fs_grep` 的串行与并行路径共用 `load_search_lines`，消除两份重复的文件读取/二进制判定/文档抽取/末尾换行逻辑，避免以后修复只改到一条路径导致结果分叉。
- `fs_glob` 在 `time_budget_ms=0` 时立刻返回（已访问 0 项），与 `fs_grep` 语义一致；此前要等第一个 256 项检查点才停。
- `fs_edit` 的 `+N -M` 改为"改动了几行"：同一行多处修改只算一次（此前同一行改 3 处会误报 `+3 -3`），替换次数仍照实报。
- `fs_glob` 的逗号分隔不再切碎花括号分组（`{a,b}.rs` 现在是合法的 alternate group）；`|` 写法会报 `E_BAD_GLOB` 并提示改用逗号，不再静默无匹配。
- `fs_grep` 拒绝能匹配空位置的正则（`^`、`$`、`a*`、`.*`、`\b`），它们会命中每一行却没有信息量，并提示改用具体字符或带内容的锚点。
- 错误消息保留最低可解析长度（`E_XXX` + 一句原因），极小 `budget` 下不再被截成 `E_BAD_ARGS` 这样的碎片；正常内容仍严格受 `budget` 约束。
- `fs_read` 超过 20 个文件、`fs_grep` 的 `context` 超过 5 时显式报错，不再静默截断或夹取。
- 多文件读取预算不足时给一条 `E_BUDGET` 提示并建议分批，不再为每个文件各刷一条 `E_LINE_TOO_LONG`（20 个文件从 348 字节错误垃圾降为 58 字节可执行提示）。
- `fs_grep` 的超长行改为围绕命中位置开窗，压缩后单行 JS 里命中词不再被填充字符挤出可见范围。
- 参数类型写错时明确报 `E_BAD_ARGS <工具>.<参数> 需要<类型>，收到 <实际类型>`，不再静默退回默认值。
- `fs_edit` 的 `edits` 漏写 `new` 会被拒绝，不再当成「删除内容」；删除必须显式写 `"new": ""`。
- `fs_edit` 的差异统计（`+N -M`）此前恒为 `+0 -0`（只数了换行符），现按实际行数计算，行号区间也只为真正的多行替换展开。
- `fs_read` 多文件读取时，深目录下的公共路径前缀只写一次并在开头声明，5 个深层文件可从 2.7KB 压到 0.9KB。
- `fs_read` 读取失败时错误信息带出具体文件路径，多文件部分失败时能直接定位是哪个文件。
- `fs_read` 长行被 `max_line` 截断时，尾注提示一次「读全文用 max_line=0」，不再让模型反复重试或退回 shell。
- `fs_write` 的 `dry_run` 此前无效（仍会创建或覆盖文件），现改为真正的预检不落盘，并在 schema 中对模型可见。
- 新增参数一致性测试：schema 声明的参数必须在代码里真正被读取，防止再次出现「文档承诺但未实现」。
- 截断尾注的行号范围改为按实际输出计算，不再出现「12-21/50 行」这类与实际正文不符的标注。
- `fs_glob`/`fs_grep` 新增 `no_ignore`：默认跳过 `.gitignore` 与 `node_modules`/`target` 等目录，需要查依赖源码或构建产物时可显式打开。
- `fs_grep` 的 `snippets`/`matches` 输出不再包含末尾换行产生的幽灵空上下文行。
- 带 BOM 的 UTF-16/UTF-8 文件不再被误判成二进制（纯中文 UTF-16 尤其明显）；编辑后保持原编码与 BOM。
- `fs_grep` 把 glob 写法误用为正则（如 `*.txt`）时，报错附正确的正则等价写法。
- 符号链接保护：写入前解析真实目标，不再把链接替换成普通文件。
- 修正行数口径：末尾换行不再算成额外一行，头部“N行”与越界提示的“共M行”不再自相矛盾。
- `ranges` 重叠或相邻时自动合并，同一批行不再重复输出；完全越界的区间会明确提示已跳过。
- 只读文件写入/编辑返回 `E_READONLY` 并给出 PowerShell 解法，不再误报 `E_BATCH_PARTIAL 已写入 0/1`。
- 空文件报 `0行` 且不再渲染空的 `1|` 行；行数计算不再把末尾换行算成额外一行。
- `fs_read paths=[...]` 对重复路径去重，避免同一份内容在上下文里出现两次。
- 修正二进制判定：极小文件里偶发 1 个 NUL 不再被误判成 UTF-16 文本；稀疏零字节样本按二进制拒绝。
- MCP `isError` 改为识别正文中任意位置的错误码行，多文件读取含失败项时不再报成功。
- `fs_grep` 回报被跳过的超大文件数量与最大体积，不再静默假阴性；无命中时也提示。
- `fs_glob` 增加 `time_budget_ms`（默认 5000ms）：全盘无匹配时不再阻塞，超时返回 `E_TIMEOUT` 或标注结果不完整。实测 C 盘无匹配从 58s 降到 5.1s。
- `fs_edit` 批量替换改为单趟线性构建，62MB 文件 80 万处替换从超时（>300s）降到约 0.6s；`loci` 摘要上限 64 条，避免响应膨胀。
- 只处理文本和 DOCX/PDF；二进制文件返回 `E_BINARY`，不会被当文本读或改写。
- `fs_read` 支持 DOCX/PDF 正文抽取；`fs_write` 支持生成纯文本段落的 DOCX，`fs_edit` 对 DOCX/PDF 明确拒绝以避免破坏原格式。
- `fs_grep` 支持文本和 DOCX/PDF 正文搜索；`fs_stat detail=hash/full` 对 DOCX/PDF 走正文抽取路径。
- PDF 解析包裹在 `catch_unwind` 内，损坏或加密 PDF 返回 `E_PDF` 而不是让 MCP 进程退出。
- `fsops integrate --rules`：向 `AGENTS.md`、`CLAUDE.md` 等宿主规则文件写入可重复更新的 fsops 软规则块；支持 `--target all`、`--remove`、`--check`、`--dry-run`。
- `fsops doctor`：检查规则文件、MCP 宿主注册状态、Claude Code hook 和当前引导级别，支持 `--json` 与 `--check`。

### 测试与工具

- `tests/bench.py` 改为自带 fixture、无需外部仓库和 tiktoken 即可运行（`python tests/bench.py`），并接入 CI 做自检；README 的基准表换成可复跑数据，含一项实际的负收益（纯定位单行 −2.6%）。
- 新增 e2e 覆盖：CRLF 文件 `new` 带 `\r\n` 不产生 `\r\r\n`；多文件编辑遇混合换行时其他文件不被改动；`fs_read` 的 hash 可直接用作 `expect_hash`；`block=true` 的块内展开；`fs_edit` 行范围；编码损失报出码位；双重盘符纠正。

### 改进
- `fs_read` 在显式指定 `end` 或 `ranges` 且未显式给 `budget` 时，按请求行数自动放宽预算，最多 64KB；显式 `budget` 仍优先。
- `fs_edit` 对 DOCX 明确拒绝精确替换，并提示使用 `fs_write` 整体重建，避免破坏复杂格式。
- MCP `initialize.instructions` 明确要求文件操作优先走 fsops，并给出内置工具回退边界。
- 六个工具的描述补充“优先替代”的宿主工具名，便于模型在选工具时直接匹配。

## [0.2.1] - 2026-11-05

### 修复

- 修复 Windows GitHub Runner 使用 cp1252 控制台时，端到端测试输出中文导致的 `UnicodeEncodeError`。
- CI 设置 `PYTHONUTF8=1`，测试脚本主动将 stdout/stderr 配置为 UTF-8。
- 新增一行安装、更新和卸载脚本：自动下载 Release、校验 SHA-256、加入 PATH，并注册检测到的 MCP 宿主。

## [0.2.0] - 2026-11-05

### 新增

- `fs_read` 支持单文件多区间 `ranges`、`max_line=0` 完整长行。
- 大于 32 MB 的文本文件支持流式读取单个连续 `start`/`end` 区间。
- `fs_grep` 支持 `files`、`count`、`matches`、`snippets` 四种输出模式。
- `fs_grep` 支持 `max_per_file`、`offset`/`next_offset` 分页和 `parallel=true` 并行扫描。
- `fs_edit` 支持多文件 `files` 批量编辑，全部预检通过后逐文件原子写入。
- `fs_stat` 支持 `detail=meta|hash|full`，元信息模式不读取文件内容。
- 跨进程写锁：多个 fsops 进程修改同一文件时串行化，超时返回 `E_LOCKED`。
- GitHub Actions：格式、Clippy、单元测试和真实 MCP 端到端测试。

### 改进

- 读取改为单次快照，避免同一调用重复读文件。
- 搜索改为流式遍历，达到 limit 或时间预算后立即停止。
- 输出预算覆盖头部、正文、上下文和续读提示，并保证 UTF-8 安全截断。
- `fs_glob` 达到 limit 提前停止，新增响应字节预算和单文件根支持。
- `fs_write` 对已有稳定换行文件自动沿用原换行；混合内容不再无谓拒绝。
- 多文件编辑的修改位置列表有长度上限，避免超大响应。
- 构建脚本的 `e2e` 会先重建 debug 二进制，避免测试旧产物。

### 修复

- 修复分页空页仍推进 `offset`、导致命中永久丢失的问题。
- 修复末页返回不存在 `next_offset` 的问题。
- 修复 `fs_grep` 直接传单文件路径时返回假阴性的问题。
- 修复读取失败未标记 MCP `isError` 的问题。
- 修复极小预算下续读参数被截断的问题。
- 修复 `fs_read` 区间越界只返回无正文文件头的问题。

### 安全与正确性

- GBK 等目标编码无法表示新字符时返回 `E_ENCODING_LOSS`，不再静默转成 UTF-8。
- 混合换行文件默认拒绝编辑，必须显式 `normalize_eol=true`。
- 所有写入使用临时文件、落盘同步和原子替换。
- 仓库与文档不包含开发者本机路径、用户名或工具链安装位置。

## [0.1.0] - 2026-11-05

### 新增

- MCP stdio 服务与 CLI 双形态。
- `fs_read`、`fs_edit`、`fs_write`、`fs_glob`、`fs_grep`、`fs_stat` 六个工具。
- Windows 路径、长路径、GBK/UTF-16/BOM、CRLF/LF 语义处理。
- 原子写入、共享冲突重试、内容指纹并发保护。
- Token 预算、续读提示、错误候选行。
- 真实文件语义测试和 token 对照基准。
