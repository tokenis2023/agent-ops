"""fsops 的对照实验：同一批真实任务，比较"宿主原生做法"和 fsops 的工具输出 token。

口径说明：
- 只统计**工具输出**（模型必须读进上下文的字符），用 cl100k_base 计 token；
  不含系统提示词、不含模型自己的思考。
- baseline 取宿主自带能力：整文件读、整文件写、grep -rn、find。
- fsops 走真实 MCP 调用（每次一个进程，含启动开销）。
"""
import json
import os
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

import tiktoken

PROJECT_ROOT = Path(__file__).resolve().parents[1]
EXE = PROJECT_ROOT / "target" / "release" / "fsops.exe"
ENC = tiktoken.get_encoding("cl100k_base")
if len(sys.argv) != 2:
    raise SystemExit(f"用法: {Path(__file__).name} <Go 仓库路径>")
ROOT = os.path.abspath(sys.argv[1])


def toks(s: str) -> int:
    return len(ENC.encode(s))


def sh(cmd: str, cwd: str) -> str:
    p = subprocess.run(["bash", "-lc", cmd], cwd=cwd, capture_output=True)
    return p.stdout.decode("utf-8", "replace")


def call(name: str, **args) -> tuple[str, float]:
    reqs = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize",
         "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "bench", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
         "params": {"name": name, "arguments": args}},
    ]
    payload = "\n".join(json.dumps(r, ensure_ascii=False) for r in reqs) + "\n"
    t0 = time.perf_counter()
    p = subprocess.run([EXE, "mcp"], input=payload.encode(), capture_output=True, timeout=120)
    dt = (time.perf_counter() - t0) * 1000
    out = [l for l in p.stdout.decode("utf-8", "replace").splitlines() if l.strip()]
    return json.loads(out[-1])["result"]["content"][0]["text"], dt


def posix(p: str) -> str:
    drive, tail = os.path.splitdrive(os.path.abspath(p))
    tail = tail.replace("\\", "/")
    return f"/{drive[0].lower()}{tail}" if drive else tail


rows = []


def row(name: str, base: int, fast: int, note: str = "") -> None:
    save = (1 - fast / base) * 100 if base else 0
    rows.append((name, base, fast, save, note))
    print(f"{name:<34} {base:>7} {fast:>7} {save:>6.1f}%  {note}")


# ---------- 场景 1：读一个文件里的一段 ----------
target = os.path.join(ROOT, "migrator", "migrator.go")
if not os.path.exists(target):
    cands = []
    for d, _, fs in os.walk(ROOT):
        if "vendor" in d:
            continue
        cands += [os.path.join(d, f) for f in fs if f.endswith(".go")]
    target = max(cands, key=os.path.getsize) if cands else None

base_read = sh(f"cat '{posix(target)}'", ROOT)
fast_read, t_read = call("fs_read", path=posix(target), start=40, end=80)
row("读 40-80 行（baseline 整文件）", toks(base_read), toks(fast_read),
    f"文件 {len(base_read)}B，fsops {len(fast_read)}B")

# ---------- 场景 2：找符号定义 ----------
sym = "func NewWatcher"
base_grep = sh(f"grep -rn --include='*.go' '{sym}' .", ROOT)
fast_grep, t_grep = call("fs_grep", pattern=re.escape(sym), path=posix(ROOT), glob="**/*.go", limit=20)
row("找符号定义", toks(base_grep), toks(fast_grep), f"grep {t_grep:.0f}ms")

# ---------- 场景 3：正则搜多处 ----------
pat = r"context\.Background\(\)"
base_grep2 = sh(f"grep -rn --include='*.go' '{pat}' .", ROOT)
fast_grep2, _ = call("fs_grep", pattern=pat, path=posix(ROOT), glob="**/*.go", limit=30)
row("正则搜索（30 条上限）", toks(base_grep2), toks(fast_grep2),
    f"baseline {len(base_grep2.splitlines())} 行 / fsops {len(fast_grep2.splitlines())} 行")

# ---------- 场景 4：列文件 ----------
base_find = sh("find . -name '*.go' -not -path './vendor/*' | sort", ROOT)
fast_glob, t_glob = call("fs_glob", path=posix(ROOT), pattern="**/*.go", limit=122)
row("列出全部 .go 文件", toks(base_find), toks(fast_glob),
    f"find {len(base_find.splitlines())} 行 / glob {len(fast_glob.splitlines())} 行")

# ---------- 场景 5：改 3 处（模型侧输入 + 输出都算） ----------
# baseline：读整文件（输入）+ 写回整文件（模型要重新输出全文）
sample = target
try:
    src = open(sample, encoding="utf-8").read()
except UnicodeDecodeError:
    src = open(sample, encoding="gbk", errors="replace").read()
lines = src.split("\n")
edits = []
for i, ln in enumerate(lines):
    if ln.startswith("func ") and len(edits) < 3:
        edits.append({"old": ln, "new": ln.replace("func ", "func ", 1) + " // touched"})
if not edits:
    edits = [{"old": lines[0], "new": lines[0] + " // touched"}]

baseline_edit_tokens = toks(src) + toks(src)  # 读全文 + 再写出全文
edit_payload = toks(json.dumps(edits, ensure_ascii=False))
fast_edit, t_edit = call("fs_edit", path=posix(sample), edits=edits, dry_run=True)
row("改 3 处（读+写 vs 单次 edit）", baseline_edit_tokens,
    toks(fast_edit) + edit_payload, f"输出 {len(fast_edit)}B，edit {t_edit:.0f}ms")

# ---------- 汇总 ----------
print()
base_total = sum(r[1] for r in rows)
fast_total = sum(r[2] for r in rows)
print(f"合计: baseline {base_total} tokens -> fsops {fast_total} tokens "
      f"（省 {(1 - fast_total / base_total) * 100:.1f}%）")
print(f"fsops 冷启动: read {t_read:.0f}ms / grep {t_grep:.0f}ms / glob {t_glob:.0f}ms / edit {t_edit:.0f}ms")
