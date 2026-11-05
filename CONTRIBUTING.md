# 参与开发

## 环境

- Windows 10/11
- Rust 1.75 或更新版本
- Python 3.10+，仅用于真实 MCP 端到端测试
- Windows GNU 工具链需要配置 MinGW；MSVC 工具链使用对应链接器

## 本地验证

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
python tests/windows_semantics.py
cargo build --release
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

1. `cargo fmt --check` 通过。
2. `cargo clippy --all-targets -- -D warnings` 通过。
3. `cargo test` 全部通过。
4. `python tests/windows_semantics.py` 全部通过。
5. `git diff --check` 无空白错误。
6. 没有新增未跟踪的临时文件。

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
