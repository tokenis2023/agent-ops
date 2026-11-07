"""任务级确定性基准：一个持久 MCP 会话里跑完整调用链。

与 tests/bench.py 的区别：
- bench.py 每次工具调用起一个新进程，只比较单次输出体积；
- 这里只 initialize 一次，之后顺序 tools/call、逐条读响应，统计真实任务链的
  调用次数、工具参数字符、工具输出字符与耗时，并校验每个场景的最终结果。

口径说明：只统计字符数，不装 tiktoken、不换算、也不宣称模型 token 消耗。
CI 用它做确定性回归（结果正确 + 调用数不膨胀），不按共享 runner 的延迟卡阈值。

用法：
    python tests/task_bench.py                 # 优先 release，其次 debug
    python tests/task_bench.py --exe target/debug/fsops.exe
"""
import argparse
import json
import os
import queue
import re
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import unicodedata
from pathlib import Path

# GitHub Windows runner 的控制台可能是 cp1252，中文输出会编码失败。
for stream in (sys.stdout, sys.stderr):
    if hasattr(stream, "reconfigure"):
        stream.reconfigure(encoding="utf-8")

PROJECT_ROOT = Path(__file__).resolve().parents[1]
TIMEOUT_S = 180


def posix(p) -> str:
    drive, tail = os.path.splitdrive(os.path.abspath(str(p)))
    tail = tail.replace("\\", "/")
    return f"/{drive[0].lower()}{tail}" if drive else tail


def display_width(s: str) -> int:
    return sum(2 if unicodedata.east_asian_width(ch) in "WF" else 1 for ch in s)


def pad(s: str, width: int) -> str:
    return s + " " * max(0, width - display_width(s))


def pick_exe(explicit: str | None) -> Path:
    if explicit:
        p = Path(explicit).resolve()
        if not p.exists():
            raise SystemExit(f"指定的二进制不存在: {p}")
        return p
    env = os.environ.get("FSOPS_BIN")
    if env:
        p = Path(env).resolve()
        if not p.exists():
            raise SystemExit(f"FSOPS_BIN 指向的二进制不存在: {p}")
        return p
    for rel in ("target/release/fsops.exe", "target/debug/fsops.exe"):
        p = PROJECT_ROOT / rel
        if p.exists():
            return p
    print("未找到 fsops.exe，先执行 cargo build ...")
    subprocess.run(["cargo", "build"], cwd=PROJECT_ROOT, check=True)
    p = PROJECT_ROOT / "target" / "debug" / "fsops.exe"
    if not p.exists():
        raise SystemExit("cargo build 后仍找不到 target/debug/fsops.exe")
    return p


class Mcp:
    """持久 MCP 客户端：initialize 一次，之后顺序调用，逐条读响应。"""

    def __init__(self, exe: Path):
        self.proc = subprocess.Popen(
            [str(exe), "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self._out: queue.Queue = queue.Queue()
        self._err: list[str] = []
        threading.Thread(target=self._pump_out, daemon=True).start()
        threading.Thread(target=self._pump_err, daemon=True).start()
        self._next_id = 1
        self.reset_stats()
        rid = self._send({
            "jsonrpc": "2.0",
            "id": self._take_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "task-bench", "version": "1"},
            },
        })
        self.server = self._await(rid)
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        # initialize 不算任务开销。
        self.reset_stats()

    def _pump_out(self) -> None:
        for line in self.proc.stdout:
            self._out.put(line.decode("utf-8", "replace"))
        self._out.put(None)

    def _pump_err(self) -> None:
        for line in self.proc.stderr:
            self._err.append(line.decode("utf-8", "replace").rstrip())

    def _take_id(self) -> int:
        n = self._next_id
        self._next_id += 1
        return n

    def _send(self, msg: dict) -> int:
        rid = msg.get("id")
        data = (json.dumps(msg, ensure_ascii=False) + "\n").encode("utf-8")
        try:
            self.proc.stdin.write(data)
            self.proc.stdin.flush()
        except OSError as e:
            raise RuntimeError(f"写入 MCP 进程失败: {e}；stderr={self.stderr()}") from e
        return rid

    def _await(self, rid: int) -> dict:
        while True:
            try:
                line = self._out.get(timeout=TIMEOUT_S)
            except queue.Empty:
                raise RuntimeError(
                    f"等待响应 id={rid} 超时（{TIMEOUT_S}s）；stderr={self.stderr()}"
                ) from None
            if line is None:
                raise RuntimeError(f"MCP 进程提前退出；stderr={self.stderr()}")
            if not line.strip():
                continue
            msg = json.loads(line)
            if msg.get("id") != rid:
                continue
            if "error" in msg:
                raise RuntimeError(f"MCP 错误 id={rid}: {msg['error']}")
            return msg["result"]

    def call(self, name: str, **arguments) -> tuple[str, bool]:
        rid = self._send({
            "jsonrpc": "2.0",
            "id": self._take_id(),
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        })
        result = self._await(rid)
        text = result["content"][0]["text"]
        self.calls += 1
        self.tool_args_chars += len(json.dumps(arguments, ensure_ascii=False))
        self.tool_out_chars += len(text)
        return text, bool(result.get("isError"))

    def stderr(self) -> str:
        return "\n".join(self._err[-10:])

    def reset_stats(self) -> None:
        self.calls = 0
        self.tool_args_chars = 0
        self.tool_out_chars = 0

    def close(self) -> None:
        try:
            if self.proc.stdin:
                self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.terminate()
            self.proc.wait(timeout=10)
        except Exception:
            try:
                self.proc.kill()
            except Exception:
                pass


class Ctx:
    def __init__(self, mcp: Mcp, root: Path):
        self.mcp = mcp
        self.root = root
        self.failures: list[str] = []

    def call(self, name: str, **arguments):
        return self.mcp.call(name, **arguments)

    def check(self, ok: bool, label: str, detail: str = "") -> bool:
        if not ok:
            self.failures.append(label if not detail else f"{label} -> {detail}")
        return ok

    def p(self, *parts) -> str:
        return posix(self.root.joinpath(*parts))


def body_lines(text: str) -> dict[int, str]:
    """解析 fs_read 的 `行号|正文` 行。"""
    out: dict[int, str] = {}
    for line in text.splitlines():
        m = re.match(r"^(\d+)\|(.*)$", line)
        if m:
            out[int(m.group(1))] = m.group(2)
    return out


def notes(lines: list[str]) -> list[str]:
    """去掉内容行后剩下的尾注（以 … 开头）。"""
    return [l for l in lines if l.startswith("…")]


def build_fixture(root: Path) -> dict:
    src = root / "src"
    files = root / "files"
    glob_dir = root / "glob"
    count_dir = root / "count"
    edit_dir = root / "edit"
    reads_dir = root / "reads"
    repeat_dir = root / "repeat"
    map_dir = root / "map"
    for d in (src, files, glob_dir, count_dir, edit_dir, reads_dir, repeat_dir, map_dir):
        d.mkdir(parents=True, exist_ok=True)

    # 场景 2：grep 命中函数内部 → block 展开 → 预算截断续读
    body = [f"    let v{i} = compute({i});" for i in range(1, 13)]
    app = ["fn handle_request() {", "    let total = 0;"] + body[:5] + [
        "    MARKER_HIT();",
    ] + body[5:] + ["    total + 1", "}", ""]
    (src / "app.rs").write_text("\n".join(app) + "\n", encoding="utf-8", newline="\n")
    other = ["fn other() {", "    let x = 1;", "}", ""]
    with (src / "app.rs").open("a", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(other))

    # 场景 1：60 个文件都有命中，强制 files 分页
    for i in range(60):
        (files / f"f{i:03d}.txt").write_text(
            "needle\nfiller\nneedle\n", encoding="utf-8", newline="\n")

    # 场景 3：120 个长文件名，预算截断后要靠 next_offset 续页
    for i in range(120):
        (glob_dir / f"glob-file-{i:03d}-with-long-name.txt").write_text(
            "x\n", encoding="utf-8", newline="\n")

    # 场景 5：高密度命中，count 必须精确
    (count_dir / "count-a.txt").write_text("hit\n" * 300, encoding="utf-8", newline="\n")
    (count_dir / "count-b.txt").write_text("hit\n" * 250, encoding="utf-8", newline="\n")

    # 场景 4：20 行可正则替换
    cfg = ["fn cfg() {"] + [f"    let limit_{i} = {100 + i};" for i in range(20)] + ["}", ""]
    (edit_dir / "edit.rs").write_text("\n".join(cfg), encoding="utf-8", newline="\n")

    # 场景 7：重复读同一区间，验证未变更短路
    (repeat_dir / "loop.txt").write_text(
        "".join(f"loop-{i:04}-{'x' * 24}\n" for i in range(1, 401)),
        encoding="utf-8", newline="\n")

    # 场景 8：repo map 排序
    (map_dir / "core.rs").write_text(
        "pub struct Token {}\npub fn verify() {}\n", encoding="utf-8", newline="\n")
    (map_dir / "user.rs").write_text(
        "fn use_it() { let t = Token; verify(); }\n", encoding="utf-8", newline="\n")
    (map_dir / "main.rs").write_text("fn main() {}\n", encoding="utf-8", newline="\n")
    (map_dir / "notes.md").write_text("# notes\n", encoding="utf-8", newline="\n")

    # 场景 6：5 个文件各读不同区间，一次 reads 调用
    for i in range(5):
        lines = [f"r{i}-line{j}" for j in range(1, 7)]
        (reads_dir / f"r{i}.txt").write_text(
            "\n".join(lines) + "\n", encoding="utf-8", newline="\n")

    return {
        "src_file": src / "app.rs",
        "files_dir": files,
        "glob_dir": glob_dir,
        "count_dir": count_dir,
        "edit_file": edit_dir / "edit.rs",
        "reads_dir": reads_dir,
        "repeat_dir": repeat_dir,
        "map_dir": map_dir,
    }


def brace_block(lines: list[str], header: str) -> tuple[int, int]:
    start = next(i for i, l in enumerate(lines, 1) if l.startswith(header))
    depth = 0
    seen = False
    for i in range(start - 1, len(lines)):
        for ch in lines[i]:
            if ch == "{":
                depth += 1
                seen = True
            elif ch == "}":
                depth -= 1
        if seen and depth == 0:
            return start, i + 1
    raise AssertionError(f"找不到 {header} 的闭合块")


def scenario_files_paging(c: Ctx, fx: dict) -> None:
    """files 高密度分页：60 个命中文件，每页 3 个，靠 next_offset 走完。"""
    expected = {f"f{i:03d}.txt" for i in range(60)}
    seen: list[str] = []
    offset = 0
    pages = 0
    first_note = ""
    while True:
        text, err = c.call("fs_grep", path=c.p("files"), pattern="needle",
                           output="files", limit=3, offset=offset)
        pages += 1
        c.check(not err, "files: grep 不应报错", text[:200])
        lines = [l for l in text.splitlines() if l and not l.startswith("…")]
        seen.extend(lines)
        if pages == 1:
            first_note = "\n".join(notes(text.splitlines()))
        m = re.search(r"next_offset=(\d+)", text)
        if not m:
            break
        offset = int(m.group(1))
        c.check(pages < 100, "files: 分页未收敛（超过 100 页）", text[:200])
        if pages >= 100:
            return
    c.check(pages > 1 and "next_offset=3" in first_note,
            "files: 第一页应截断并给出 next_offset=3", first_note or "无尾注")
    c.check(len(seen) == len(set(seen)), "files: 分页出现重复路径",
            f"共 {len(seen)} 条，去重 {len(set(seen))}")
    c.check(set(seen) == expected, "files: 路径集合与 fixture 不一致",
            f"缺 {sorted(expected - set(seen))[:5]}，多 {sorted(set(seen) - expected)[:5]}")
    c.check(pages == 20, "files: 60 个文件按每页 3 个应为 20 页", f"实际 {pages} 页")


def scenario_grep_block_continue(c: Ctx, fx: dict) -> None:
    """grep 拿行号 → fs_read block 展开 → 预算截断按 ranges 续读到块结束。"""
    path = c.p("src", "app.rs")
    text, err = c.call("fs_grep", path=c.p("src"), pattern=r"MARKER_HIT\(\)",
                       output="matches")
    c.check(not err, "block: grep 不应报错", text[:200])
    m = re.search(r"^(\d+):", text, re.M)
    if not c.check(m is not None, "block: grep 未给出行号", text[:300]):
        return
    hit = int(m.group(1))

    source = (fx["src_file"]).read_text(encoding="utf-8").splitlines()
    want_from, want_to = brace_block(source, "fn handle_request() {")

    collected: dict[int, str] = {}
    text, err = c.call("fs_read", path=path, start=hit, block=True, budget=320)
    first = body_lines(text)
    c.check(not err, "block: 首次 fs_read 不应报错", text[:200])
    c.check(bool(first), "block: 首次 fs_read 没有正文", text[:300])
    collected.update(first)
    c.check("续读" in text, "block: 首次读取应因预算截断给出续读", text[-300:])

    pages = 1
    while "续读" in text:
        note = next((l for l in text.splitlines() if "续读:" in l), "")
        m = re.search(r"ranges=(\[.*\])\s*$", note)
        if not c.check(m is not None, "block: 续读尾注缺少 ranges", text[-300:]):
            return
        ranges = json.loads(m.group(1))
        text, err = c.call("fs_read", path=path, ranges=ranges, budget=320)
        pages += 1
        c.check(not err, "block: 续读不应报错", text[:200])
        collected.update(body_lines(text))
        c.check(pages <= len(source), "block: 续读未收敛", f"{pages} 页")
        if pages > len(source):
            return

    c.check(pages > 1, "block: 单页就读完，未覆盖续读路径")
    c.check(set(collected) == set(range(want_from, want_to + 1)),
            "block: 续读行号集合与代码块不一致",
            f"期望 {want_from}-{want_to}，实际 {min(collected, default=0)}-{max(collected, default=0)}（{len(collected)} 行）")
    bad = [(n, collected.get(n), source[n - 1]) for n in range(want_from, want_to + 1)
           if collected.get(n) != source[n - 1]]
    c.check(not bad, "block: 续读内容与源文件不一致", str(bad[:2]))
    c.check(any("MARKER_HIT" in v for v in collected.values()),
            "block: 展开结果未包含命中行")
    c.check(not any("fn other" in v for v in collected.values()),
            "block: block 展开越过了函数边界")


def scenario_glob_budget(c: Ctx, fx: dict) -> None:
    """glob 预算分页：120 个条目超出单页预算，跟随 next_offset 走完。"""
    expected = {f"glob-file-{i:03d}-with-long-name.txt" for i in range(120)}
    seen: list[str] = []
    offset = 0
    pages = 0
    first_note = ""
    while True:
        text, err = c.call("fs_glob", path=c.p("glob"), pattern="*.txt",
                           limit=200, budget=700, offset=offset)
        pages += 1
        c.check(not err, "glob: 不应报错", text[:200])
        lines = [l for l in text.splitlines() if l and not l.startswith("…")]
        seen.extend(lines)
        if pages == 1:
            first_note = "\n".join(notes(text.splitlines()))
        m = re.search(r"next_offset=(\d+)", text)
        if not m:
            break
        offset = int(m.group(1))
        c.check(pages < 200, "glob: 分页未收敛", text[:200])
        if pages >= 200:
            return
    c.check("输出截断" in first_note and "共 120 条匹配" in first_note,
            "glob: 首页应说明截断与总数", first_note or "无尾注")
    c.check(len(seen) == len(set(seen)), "glob: 分页出现重复条目",
            f"共 {len(seen)} 条，去重 {len(set(seen))}")
    c.check(set(seen) == expected, "glob: 条目集合与 fixture 不一致",
            f"缺 {sorted(expected - set(seen))[:5]}，多 {sorted(set(seen) - expected)[:5]}")
    c.check(pages > 1, "glob: 单页就取完，未覆盖预算分页")


def scenario_regex_edit(c: Ctx, fx: dict) -> None:
    """正则编辑 dry_run + diff：先看不落盘，再真改并校验内容。"""
    path = c.p("edit", "edit.rs")
    original = fx["edit_file"].read_bytes()
    edit = {
        "pattern": r"let limit_(\d+) = \d+;",
        "replacement": "let cap_$1 = 500;",
    }
    text, err = c.call("fs_edit", path=path, edits=[edit], dry_run=True, diff=True)
    c.check(not err, "edit: dry_run 不应报错", text[:300])
    c.check(text.startswith("ok dry_run"), "edit: dry_run 输出前缀不对", text[:200])
    c.check("20 处将修改" in text, "edit: dry_run 未报告 20 处", text[:200])
    c.check("-let limit_0 = 100;" in text and "+let cap_0 = 500;" in text,
            "edit: dry_run diff 未同时给出旧文与新文", text[:400])
    c.check("+let cap_19 = 500;" in text, "edit: diff 未覆盖最后一行", text[-300:])
    c.check(fx["edit_file"].read_bytes() == original,
            "edit: dry_run 后文件被改写")

    text, err = c.call("fs_edit", path=path, edits=[edit], diff=True)
    c.check(not err, "edit: 实际写入不应报错", text[:300])
    c.check(text.startswith("ok 20 处修改"), "edit: 未报告 20 处修改", text[:200])
    after = fx["edit_file"].read_text(encoding="utf-8").splitlines()
    want = ["fn cfg() {"] + [f"    let cap_{i} = 500;" for i in range(20)] + ["}"]
    diff = [(i + 1, a, b) for i, (a, b) in enumerate(zip(after, want)) if a != b]
    c.check(after == want, "edit: 落盘内容与预期不一致",
            str(diff[:2]) if diff else f"行数 {len(after)} != {len(want)}")


def scenario_count_density(c: Ctx, fx: dict) -> None:
    """count 高密度：默认 limit 不截断，必须给出每文件的真实命中数。"""
    text, err = c.call("fs_grep", path=c.p("count"), pattern="^hit$", output="count")
    c.check(not err, "count: 不应报错", text[:200])
    got: dict[str, int] = {}
    for line in text.splitlines():
        m = re.match(r"^([^:]+):(\d+)(?:\(本页可见[^)]*\))?$", line)
        if m:
            got[m.group(1)] = int(m.group(2))
    c.check(got == {"count-a.txt": 300, "count-b.txt": 250},
            "count: 命中数不精确", str(got))
    c.check("本页可见" not in text, "count: 未截断时不应标注本页可见", text[:200])


def scenario_multi_file_reads(c: Ctx, fx: dict) -> None:
    """多文件异构区间：5 次单读 vs 1 次 reads，内容必须等价。"""
    files = [c.p("reads", f"r{i}.txt") for i in range(5)]
    separate_chars = 0
    for i, path in enumerate(files):
        text, err = c.call("fs_read", path=path, start=i + 1, end=i + 2)
        c.check(not err, f"reads: 单读 r{i} 不应报错", text[:200])
        separate_chars += len(text)

    before = c.mcp.calls
    batched, err = c.call(
        "fs_read",
        reads=[{"path": path, "start": i + 1, "end": i + 2} for i, path in enumerate(files)],
        force=True,
    )
    used = c.mcp.calls - before
    c.check(not err, "reads: 批量调用不应报错", batched[:300])
    c.check(used == 1, "reads: 多文件异构读取应只占 1 次调用", f"实际 {used} 次")
    for i in range(5):
        c.check(f"r{i}.txt" in batched, f"reads: 缺少 r{i} 的文件头", batched[:300])
        c.check(f"{i + 1}|r{i}-line{i + 1}" in batched,
                f"reads: r{i} 缺起始行", batched[:400])
        c.check(f"{i + 2}|r{i}-line{i + 2}" in batched,
                f"reads: r{i} 缺结束行", batched[:400])
        c.check(f"1|r{i}-line1" not in batched or i == 0,
                f"reads: r{i} 读到了区间外的第 1 行", batched[:400])
    c.check(len(batched) <= separate_chars * 1.10 + 50,
            "reads: 单次批量输出不应明显大于 5 次单读",
            f"批量 {len(batched)} vs 单读 {separate_chars}")


def scenario_repeat_read(c: Ctx, fx: dict) -> None:
    """重复读同一区间：默认返回正文。

    stub 只省单次响应，却让模型多花一轮 force=true；多一轮等于整个上下文
    重发，按总成本算返回正文更便宜。短路逻辑本身由 Rust 单测覆盖。
    """
    path = c.p("repeat", "loop.txt")
    first, err = c.call("fs_read", path=path, start=1, end=400)
    c.check(not err, "repeat: 首次读不应报错", first[:200])
    second, err = c.call("fs_read", path=path, start=1, end=400)
    c.check(not err, "repeat: 二次读不应报错", second[:200])
    c.check("未变更" not in second, "repeat: 默认不应短路成摘要", second[:200])
    c.check(len(second) > len(first) * 0.9, "repeat: 默认应返回正文",
            f"{len(second)} vs {len(first)}")

def scenario_repo_map(c: Ctx, fx: dict) -> None:
    """repo map：一次调用给出「先看哪些文件」。"""
    before = c.mcp.calls
    text, err = c.call("fs_glob", path=c.p("map"), pattern="**/*", map=True)
    used = c.mcp.calls - before
    c.check(not err, "map: 不应报错", text[:300])
    c.check(used == 1, "map: 应只占 1 次调用", f"实际 {used} 次")
    c.check("符号" in text and "core.rs" in text, "map: 缺少文件或符号", text[:400])
    core = text.find("core.rs")
    user = text.find("user.rs")
    c.check(core != -1 and user != -1 and core < user,
            "map: 被引用的 core.rs 应排在 user.rs 前", text[:500])
    c.check("notes.md" not in text, "map: 数据文件应跳过", text[:400])

def scenario_background_job(c: Ctx, fx: dict) -> None:
    """后台任务：启动后轮询到退出，输出按新增返回。"""
    text, err = c.call("fs_exec", program="cmd", args=["/c", "ping", "-n", "3", "127.0.0.1"],
                       background=True)
    c.check(not err and text.startswith("ok job="), "job: 启动应返回 job id", text[:200])
    m = re.search(r"job=(\d+)", text)
    job_id = int(m.group(1)) if m else 0
    poll = ""
    done = False
    for _ in range(50):
        poll, err = c.call("fs_exec", job=job_id)
        if "exit=" in poll:
            done = True
            break
        time.sleep(0.1)
    c.check(done and "exit=0" in poll, "job: 轮询到退出", poll[:300])
    c.check("stdout" in poll, "job: 应返回输出", poll[:300])

SCENARIOS = [
    ("files 高密度分页", scenario_files_paging),
    ("grep→block→续读", scenario_grep_block_continue),
    ("glob 预算分页", scenario_glob_budget),
    ("正则编辑 dry_run+diff", scenario_regex_edit),
    ("count 高密度", scenario_count_density),
    ("多文件异构 reads", scenario_multi_file_reads),
    ("重复读不短路", scenario_repeat_read),
    ("repo map 排序", scenario_repo_map),
    ("后台任务轮询", scenario_background_job),
]


def main() -> int:
    parser = argparse.ArgumentParser(description="fsops 任务级确定性基准")
    parser.add_argument("--exe", help="fsops 可执行文件路径；默认 release，其次 debug")
    args = parser.parse_args()
    exe = pick_exe(args.exe)

    print("fsops 任务级基准（确定性；字符口径，非模型 token）")
    print(f"二进制: {exe}")

    rows: list[tuple[str, int, int, int, float, bool]] = []
    all_ok = True
    failures: list[tuple[str, list[str]]] = []

    with tempfile.TemporaryDirectory(prefix="fsops-task-bench-") as tmp:
        root = Path(tmp)
        fx = build_fixture(root)
        mcp = Mcp(exe)
        try:
            print(f"服务端: {mcp.server.get('serverInfo', {}).get('name')} "
                  f"{mcp.server.get('serverInfo', {}).get('version')}")
            print(f"fixture: {root}\n")
            for name, fn in SCENARIOS:
                c = Ctx(mcp, root)
                mcp.reset_stats()
                t0 = time.perf_counter()
                failed = False
                try:
                    fn(c, fx)
                except Exception:
                    failed = True
                    c.failures.append("异常:\n" + traceback.format_exc())
                dt = (time.perf_counter() - t0) * 1000
                ok = not failed and not c.failures
                all_ok = all_ok and ok
                rows.append((name, mcp.calls, mcp.tool_args_chars,
                             mcp.tool_out_chars, dt, ok))
                if c.failures:
                    failures.append((name, c.failures))
                print(("PASS " if ok else "FAIL ") + name)
                for f in c.failures:
                    print("  - " + f.replace("\n", "\n    "))
        finally:
            mcp.close()

    widths = [max(display_width(r[0]) for r in rows + [("场景", 0, 0, 0, 0, True)]),
              5, 8, 8, 8, 4]
    header = ("场景", "calls", "in字符", "out字符", "ms", "结果")
    print("\n" + "  ".join(pad(str(h), w) for h, w in zip(header, widths)))
    for r in rows:
        cells = (r[0], str(r[1]), str(r[2]), str(r[3]), f"{r[4]:.0f}",
                 "PASS" if r[5] else "FAIL")
        print("  ".join(pad(c, w) for c, w in zip(cells, widths)))
    total = (sum(r[1] for r in rows), sum(r[2] for r in rows),
             sum(r[3] for r in rows), sum(r[4] for r in rows))
    print("  ".join(pad(c, w) for c, w in zip(
        ("合计", str(total[0]), str(total[1]), str(total[2]), f"{total[3]:.0f}",
         "PASS" if all_ok else "FAIL"), widths)))

    if not all_ok:
        print("\n失败场景:")
        for name, _ in failures:
            print(f"- {name}")
        return 1
    print("\n全部场景通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
