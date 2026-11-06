# 变更记录

本项目使用语义化版本。日期为发布日。

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
