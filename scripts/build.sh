#!/usr/bin/env bash
# 构建入口；Rust、链接器和 Python 由使用者的标准环境提供。
set -euo pipefail
cd "$(dirname "$0")/.."

PYTHON="${PYTHON:-python}"

case "${1:-build}" in
  build)   cargo build --release ;;
  test)    cargo test ;;
  e2e)     cargo build && "$PYTHON" tests/windows_semantics.py ;;
  bench)   "$PYTHON" tests/bench.py "${2:-}" ;;
  dsh)     cargo build && node tests/dsh_plugin_smoke.mjs --exe target/debug/fsops.exe ;;
  all)
    cargo test
    cargo build
    "$PYTHON" tests/windows_semantics.py
    cargo build --release
    ;;
  *) echo "用法: scripts/build.sh [build|test|e2e|bench|dsh|all]"; exit 1 ;;
esac
