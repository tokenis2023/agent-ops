# 参与开发

## 环境

- Windows 10/11
- Rust 1.75 或更新版本
- Python 3.10+，仅用于真实 MCP 端到端测试
- Windows GNU 工具链需要配置 MinGW；MSVC 工具链使用对应链接器

## 本地验证

按改动范围选档位，不要每次编辑都跑全套：

```powershell
.\scripts\check.ps1 fast     # 格式 + clippy + 改动模块单测        约 1–7 秒
.\scripts\check.ps1 commit   # fast + 完整 cargo test              约 8 秒
.\scripts\check.ps1 full     # commit + 构建 + 真实 MCP e2e + 任务级基准 + DSH 插件冒烟  约 16 秒
```

- 编辑过程中用 `fast`（改动模块自动推断，也可用 `-Module tools` 指定）。
- 提交前跑 `commit`。
- push 前，或改动 `io`/`text`/`edit`/`render` 等 I/O 内核时跑 `full`。

等价的底层命令：

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
python tests/windows_semantics.py
python tests/task_bench.py --exe target/debug/fsops.exe
node tests/dsh_plugin_smoke.mjs --exe target/debug/fsops.exe
```

也可以运行：

```bash
bash scripts/build.sh all
```

## 约束

- 不增加不必要的工具数量。新能力优先并入现有六个工具。
- 修改编码、换行、原子写入、并发或分页逻辑时，必须先写失败测试。
- 文件语义测试比较真实字节，不只比较解码后的字符串。
- 输出必须受预算约束，并保证 UTF-8 字符不被截断。
- 不提交本机绝对路径、用户名、工具链安装位置或特定仓库路径。
- 不提交构建产物、测试夹具、崩溃转储和凭据。
- 注释、文档和提交信息使用中文。
- PowerShell 脚本必须保存为 UTF-8 BOM；Windows PowerShell 5.1 会把无 BOM 的 UTF-8 当成本地代码页读取，导致中文解析失败。

## 提交前检查

1. `.\scripts\check.ps1 commit` 通过（等价于 fmt + clippy + 完整 cargo test）。
2. 准备 push 或改动 I/O 内核时，`.\scripts\check.ps1 full` 通过。
3. `git diff --check` 无空白错误。
4. 没有新增未跟踪的临时文件。

## 重新构建 release

Windows 上如果 Hermes、Claude Code 等宿主仍持有 `fsops.exe`，替换会失败：

```powershell
Get-Process fsops -ErrorAction SilentlyContinue | Stop-Process -Force
cargo build --release
```

MCP 宿主会在下一次调用时重新启动服务。

## 发布

1. 更新 `Cargo.toml` 版本和 `CHANGELOG.md`。
2. 运行完整验证。
3. 提交 `chore(release): X.Y.Z`。
4. 创建注解标签 `vX.Y.Z` 并推送标签。
5. 在 GitHub Release 上传 Windows 二进制和 SHA-256。
