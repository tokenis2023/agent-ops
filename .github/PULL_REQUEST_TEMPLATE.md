## 改动

说明解决的问题和用户可见行为。

## 验证

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test`
- [ ] `python tests/windows_semantics.py`
- [ ] 新增/修改行为有失败测试先行

## 文档与安全

- [ ] 更新了相关文档或 CHANGELOG
- [ ] 没有提交本机绝对路径、凭据、构建产物或临时文件
- [ ] 已说明编码、换行、并发、预算或兼容性影响
