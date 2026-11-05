# 变更记录

本项目使用语义化版本。日期为发布日。

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
