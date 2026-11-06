"""fsops 的对照实验：同一批真实任务，比较"宿主原生做法"和 fsops 的工具输出。

口径说明：
- 只统计**工具输出**（模型必须读进上下文的字符）。装了 tiktoken 就按
  cl100k_base 计 token，否则退回字符数（两者结论一致）。
- baseline 取宿主自带能力：整文件读、整文件写回、grep -rn、find。
- fsops 走真实 MCP 调用（每次一个进程，含启动开销）。

用法：
    python tests/bench.py                  # 自带 fixture，无需外部仓库
    python tests/bench.py /path/to/repo    # 额外在真实代码上跑一轮
"""
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
EXE = PROJECT_ROOT / "target" / "release" / "fsops.exe"
if not EXE.exists():
    EXE = PROJECT_ROOT / "target" / "debug" / "fsops.exe"
if not EXE.exists():
    raise SystemExit("先构建：cargo build --release")

try:
    import tiktoken

    ENC = tiktoken.get_encoding("cl100k_base")
    UNIT = "token"

    def size(s: str) -> int:
        return len(ENC.encode(s))

except ImportError:  # 没有 tiktoken 也能跑：字符数是等价口径
    UNIT = "字符"

    def size(s: str) -> int:
        return len(s)


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
    p = subprocess.run([str(EXE), "mcp"], input=payload.encode(), capture_output=True, timeout=120)
    dt = (time.perf_counter() - t0) * 1000
    out = [l for l in p.stdout.decode("utf-8", "replace").splitlines() if l.strip()]
    return json.loads(out[-1])["result"]["content"][0]["text"], dt


def posix(p: str) -> str:
    drive, tail = os.path.splitdrive(os.path.abspath(p))
    tail = tail.replace("\\", "/")
    return f"/{drive[0].lower()}{tail}" if drive else tail


def read_text(p: str) -> str:
    try:
        return Path(p).read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return Path(p).read_text(encoding="gbk", errors="replace")


rows: list[tuple[str, int, int, float]] = []


def row(name: str, base: int, fast: int, note: str = "") -> None:
    save = (1 - fast / base) * 100 if base else 0
    rows.append((name, base, fast, save))
    print(f"{name:<30} {base:>7} {fast:>7} {save:>7.1f}%  {note}")


def build_fixture(root: str) -> list[str]:
    """造一个仿真实项目：40 个文件，每个一个 36 行的函数。"""
    os.makedirs(os.path.join(root, "src"), exist_ok=True)
    files = []
    for i in range(40):
        body = "\n".join(f"    let v{j} = compute({j});" for j in range(35))
        p = os.path.join(root, "src", f"mod{i}.rs")
        Path(p).write_text(f"fn handler_{i}() {{\n{body}\n}}\n", encoding="utf-8")
        files.append(p)
    return files


def run(root: str, files: list[str], label: str) -> None:
    print(f"\n=== {label}（单位：{UNIT}）===")
    root_p = posix(root)

    # A：列出含目标的文件（baseline 等价 grep -rl）
    hits = [f for f in files if "fn handler_" in read_text(f)]
    fast_a, _ = call("fs_grep", path=root_p, pattern=r"fn handler_\d+", output="files")
    row("列出命中文件", size("\n".join(hits)), size(fast_a), f"{len(hits)} 个")

    # B：只定位位置（grep -rn 一行）
    base_b = ""
    for f in files:
        for i, line in enumerate(read_text(f).splitlines(), 1):
            if "let v17 = compute" in line:
                base_b = f"{f}:{i}:{line}"
                break
        if base_b:
            break
    fast_b, _ = call("fs_grep", path=root_p, pattern=r"let v17 = compute",
                     output="matches", limit=1)
    row("定位单行（只要位置）", size(base_b), size(fast_b))

    # C：定位 + 前后各 2 行上下文
    base_c = ""
    for f in files:
        txt = read_text(f).splitlines()
        for i, line in enumerate(txt, 1):
            if "let v17 = compute" in line:
                lo, hi = max(1, i - 2), min(len(txt), i + 2)
                base_c = "\n".join(f"{f}:{n}:{txt[n - 1]}" for n in range(lo, hi + 1))
                break
        if base_c:
            break
    fast_c, _ = call("fs_grep", path=root_p, pattern=r"let v17 = compute",
                     output="snippets", limit=1)
    row("定位 + 4 行上下文", size(base_c), size(fast_c))

    # D：读文件前 10 行（baseline 整文件）
    target = files[0]
    whole = read_text(target)
    fast_d, t_d = call("fs_read", path=posix(target), end=10)
    row("读前 10 行", size(whole), size(fast_d), f"read {t_d:.0f}ms")

    # E：改 3 处（baseline 读全文 + 模型再输出全文）
    edits = []
    for ln in whole.split("\n"):
        if ln.startswith("    let v") and len(edits) < 3:
            edits.append({"old": ln, "new": ln.replace("compute", "calc")})
    if not edits:
        first = whole.split("\n")[0]
        edits = [{"old": first, "new": first + " // touched"}]
    fast_e, t_e = call("fs_edit", path=posix(target), edits=edits, dry_run=True)
    row("改 3 处", size(whole) * 2,
        size(fast_e) + size(json.dumps(edits, ensure_ascii=False)), f"edit {t_e:.0f}ms")

    # F：列文件
    fast_f, t_f = call("fs_glob", path=root_p, pattern="**/*.rs", limit=200)
    row("列出全部文件", size("\n".join(sorted(files))), size(fast_f), f"glob {t_f:.0f}ms")


def summary() -> None:
    base_total = sum(r[1] for r in rows)
    fast_total = sum(r[2] for r in rows)
    if not base_total:
        return
    print(f"\n合计: baseline {base_total} -> fsops {fast_total} "
          f"（省 {(1 - fast_total / base_total) * 100:.1f}%）")


def main() -> None:
    print(f"fsops 对照实验（单位：{UNIT}）")
    with tempfile.TemporaryDirectory(prefix="fsops-bench-") as tmp:
        run(tmp, build_fixture(tmp), "内置 fixture（40 文件 / 每文件 36 行）")
        summary()

        if len(sys.argv) > 1:
            ext = os.path.abspath(sys.argv[1])
            if not os.path.isdir(ext):
                raise SystemExit(f"不是目录: {ext}")
            ext_files = []
            for d, _, fs in os.walk(ext):
                if any(s in d for s in (".git", "node_modules", "target", "vendor")):
                    continue
                ext_files += [os.path.join(d, f) for f in fs
                              if f.endswith((".rs", ".go", ".py", ".ts", ".js"))]
            if ext_files:
                rows.clear()
                run(ext, ext_files[:400], f"真实仓库 {ext}")
                summary()


if __name__ == "__main__":
    main()
