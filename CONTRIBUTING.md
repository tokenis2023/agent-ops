# 参与开发

## 环境

- Windows 10/11
- Rust 1.88 或更新版本
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
- **每个任务绿了就提交，不跑 e2e**；`cargo test --lib <模块>::` 足够。
- 提交前跑 `commit`。
- push 前，或改动 `io`/`text`/`edit`/`render` 等 I/O 内核时跑 `full`。
- **e2e 与基准只在功能收尾跑一次**，不要每改一版就重跑。

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

改了 `src/tools.rs` 的工具定义或描述后，重新生成 DSH 插件定义：

```bash
node scripts/gen_dsh_specs.mjs --exe target/release/fsops.exe
```

漏了会被 DSH 冒烟的漂移守卫拦下，红灯会直接给你这条命令。

也可以运行：

```bash
bash scripts/build.sh all
```

## 约束

- 不增加不必要的工具数量。新能力优先并入现有八个工具（文件 6 个 + 执行/文件管理 2 个）；新增工具必须同时更新工具参考与 DSH 插件。
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

```powershell
python scripts/release.py prepare vX.Y.Z   # 升版本 + 同步文档引用 + CHANGELOG/发布正文骨架
# 人工补写新 CHANGELOG 段与 docs/releases/vX.Y.Z.md 的正文
.\scripts\check.ps1 full                    # 完整门禁（含版本一致性与发布工具自检）
cargo build --release
python scripts/release.py fill vX.Y.Z        # 用构建产物回填正文的 SHA-256 与大小
```

1. 提交 `chore(release): X.Y.Z`，创建注解标签 `vX.Y.Z` 并推送。
2. `python scripts/release.py vX.Y.Z` 创建 GitHub Release 并上传二进制、SHA-256 与两个脚本。
3. `python scripts/release.py check` 可随时校验版本一致性；CI 与 `check.ps1 full` 都会跑。
