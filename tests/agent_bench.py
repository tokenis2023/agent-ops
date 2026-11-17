"""P1b 真实 Agent 对照：fsops 工具集 vs 宿主原生工具集。

设计（对应 docs/Agent易用性优化计划.md 的 P1b、docs/Agent易用性评测基线.md 的 9.2）：

- 两侧使用同一模型、同一推理配置、同一任务书、逐字节相同的 fixture；
- 唯一变量是「工具集」：fsops 侧拿到 9 个 fs_* 工具（schema 直接取自 `fsops schema`），
  原生侧拿到宿主等价的内置工具（读/写/精确替换/搜索/列文件/命令）；
- 每次运行独立目录（mkdtemp），跑完删除；原始调用链、usage、墙钟逐条落盘；
- 校验器独立检查磁盘内容与答案，不采信模型自述成功。

用法：
    python tests/agent_bench.py --smoke            # 1 任务 × 1 次 × 2 侧，排查 harness
    python tests/agent_bench.py                    # 6 任务 × 3 次 × 2 侧
    python tests/agent_bench.py --tasks R01,R05 --times 5 --keep

环境变量：
    AGENT_BENCH_API    默认 http://127.0.0.1:3425/v1（OpenAI 兼容）
    AGENT_BENCH_MODEL  默认 auto/deepseek-v4.1-flash
    FSOPS_BIN          被测二进制，默认 target/release/fsops.exe
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import traceback
import urllib.error
import urllib.request
from pathlib import Path

# GitHub Windows runner 的控制台可能是 cp1252，中文输出会编码失败。
for stream in (sys.stdout, sys.stderr):
    if hasattr(stream, "reconfigure"):
        stream.reconfigure(encoding="utf-8")

PROJECT_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))
from task_bench import Mcp  # noqa: E402  复用持久 MCP 客户端

API_BASE = os.environ.get("AGENT_BENCH_API", "http://127.0.0.1:3425/v1")
MODEL = os.environ.get("AGENT_BENCH_MODEL", "auto/deepseek-v4.1-flash")
OUT_DIR = PROJECT_ROOT / "tests" / "agent_bench_out"

MAX_TURNS = 30          # 依赖批次上限（高于 30 视为未完成）
RUN_TIMEOUT_S = 300     # 单次运行墙钟上限
API_TIMEOUT_S = 180     # 单次 API 调用超时
TOOL_TIMEOUT_S = 120    # 单次工具调用超时

SYSTEM_PROMPT = (
    "你是一个编码 Agent，在给定工作目录内完成用户任务。"
    "使用提供的工具，完成后用简短文字给出最终答案（涉及修改时说明改了哪里）。"
)


# ---------------------------------------------------------------- 路径工具


def posix(p: str | Path) -> str:
    drive, tail = os.path.splitdrive(os.path.abspath(str(p)))
    tail = tail.replace("\\", "/")
    return f"/{drive[0].lower()}{tail}" if drive else tail


def resolve(p: str, root: Path) -> str:
    """把相对路径解析到本次运行目录。

    绝对路径原样返回；MSYS 风格 `/c/...`、`~/...`、`C:/...`、UNC 都算绝对，
    交给 fsops 自己解析（Windows 的 Path.is_absolute() 不认 `/c/...`）。
    """
    s = str(p)
    if s.startswith("~"):
        return str(Path(s).expanduser())
    if s.startswith(("/", "\\")) or re.match(r"^[A-Za-z]:", s) or s.startswith("//"):
        return s
    return str(root / s)


def win_path(p: str, root: Path) -> str:
    """原生侧用：把 MSYS 的 `/c/...` 映射成 `C:/...`。"""
    s = resolve(p, root)
    m = re.match(r"^/([A-Za-z])(/.*)$", s.replace("\\", "/"))
    if m:
        return f"{m.group(1).upper()}:{m.group(2)}"
    return s


def sha256_file(p: Path) -> str:
    h = hashlib.sha256()
    with p.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def snapshot(root: Path) -> dict[str, str]:
    """记录运行前全部文件的 hash，用于「未授权文件未变」断言。"""
    out: dict[str, str] = {}
    for p in sorted(root.rglob("*")):
        if p.is_file():
            out[str(p.relative_to(root)).replace("\\", "/")] = sha256_file(p)
    return out


# ---------------------------------------------------------------- fixture


def _w(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8", newline="\n")


def make_fixture(task: str, root: Path) -> None:
    """为每个任务生成确定性 fixture。内容与两侧无关，逐字节相同。"""

    if task == "R01":  # 定位：14 文件小仓库，12 个 svc 导出手柄
        _w(root / "README.md", "# svc kit\n\n12 个服务处理器，接线见 src/log.ts。\n")
        _w(root / "src" / "log.ts", "export const logger = console.log;\n")
        for i in range(1, 13):
            _w(
                root / "src" / f"svc{i:02d}.ts",
                f"export function handle_svc{i:02d}(input: string) {{\n"
                f"  return `svc{i:02d}:${{input}}`;\n"
                "}\n",
            )

    elif task == "R03":  # 多文件不同区间读取
        for name, base in (("a", 1), ("a", 2), ("a", 3), ("b", 1), ("b", 2), ("b", 3)):
            fname = f"{name}{base}.ts"
            start, end = (20, 25) if name == "a" else (60, 65)
            lines = []
            for n in range(1, 81):
                if n == start + 2:
                    lines.append(f"export const TOKEN_{name}{base}_{n} = {n};")
                else:
                    lines.append(f"// filler {fname} line {n}")
            _w(root / "src" / fname, "\n".join(lines) + "\n")

    elif task == "R05":  # 函数内两处重复文本，只改其中之一
        _w(
            root / "src" / "counter.ts",
            "let count = 0;\n"
            "\n"
            "export function tick() {\n"
            "  count += 1;\n"
            "  return count;\n"
            "}\n"
            "\n"
            "export function bump() {\n"
            "  count += 1;\n"
            "  return count;\n"
            "}\n",
        )

    elif task == "R06":  # 三文件批量改名
        for i, n in enumerate(("alpha", "beta", "gamma"), 1):
            _w(
                root / "src" / f"h{i}.ts",
                f"export function handle_{n}(x: number) {{\n  return x + {i};\n}}\n",
            )

    elif task == "R07":  # 歧义：三处相同字符串，只改指定上下文
        _w(
            root / "src" / "config.ts",
            "export const defaults = { timeout: 30 };\n"
            "\n"
            "export function loadDisk(path: string) {\n"
            "  timeout = 30;\n"
            "  return path;\n"
            "}\n"
            "\n"
            "export function loadNetwork(url: string) {\n"
            "  timeout = 30;\n"
            "  return url;\n"
            "}\n"
            "\n"
            "export function loadCache(key: string) {\n"
            "  timeout = 30;\n"
            "  return key;\n"
            "}\n",
        )

    elif task == "R08":  # 不匹配：任务书给的名字与磁盘不符，需读盘修正
        _w(
            root / "src" / "legacy.ts",
            "export function calc_total(items: number[]) {\n"
            "  return items.reduce((a, b) => a + b, 0);\n"
            "}\n"
            "\n"
            "export function calc_avg(items: number[]) {\n"
            "  return calc_total(items) / Math.max(items.length, 1);\n"
            "}\n",
            )

    else:
        raise SystemExit(f"未知任务 {task}")


# ---------------------------------------------------------------- 任务书与校验

TASKS: dict[str, dict] = {
    "R01": {
        "prompt": (
            "工作目录就是仓库根目录。回答两个问题：\n"
            "1) 仓库里一共有多少个文件（按文件计，不含目录）？\n"
            "2) 哪些文件导出了以 handle_ 开头的函数？列出文件名。\n"
            "不要修改任何文件。完成后用简短文字给出答案。"
        ),
        "writes": False,
    },
    "R03": {
        "prompt": (
            "工作目录就是仓库根目录。请读取 src/a1.ts、src/a2.ts、src/a3.ts 的第 20-25 行，"
            "以及 src/b1.ts、src/b2.ts、src/b3.ts 的第 60-65 行，"
            "把每个文件这几个区间里出现的 TOKEN_* 常量按「文件:行号:标记」列出。\n"
            "不要修改文件。"
        ),
        "writes": False,
    },
    "R05": {
        "prompt": (
            "工作目录就是仓库根目录。src/counter.ts 的 tick() 函数里有一处 `count += 1;`，"
            "bump() 函数里也有一处同样的语句。只把 tick() 里的那一处改成 `count += 2;`，"
            "bump() 里的保持不变。完成后说明改了哪里。"
        ),
        "writes": True,
    },
    "R06": {
        "prompt": (
            "工作目录就是仓库根目录。把 src/h1.ts、src/h2.ts、src/h3.ts 里的导出名"
            "`handle_alpha` / `handle_beta` / `handle_gamma` 分别改成 `on_alpha` / `on_beta` / `on_gamma`。"
            "三个文件都要改，不要动其他文件。"
        ),
        "writes": True,
    },
    "R07": {
        "prompt": (
            "工作目录就是仓库根目录。src/config.ts 里有三处 `timeout = 30;`，"
            "只把位于 loadNetwork() 函数内部的那一处改成 `timeout = 60;`，"
            "loadDisk() 和 loadCache() 里的保持不变。"
        ),
        "writes": True,
    },
    "R08": {
        "prompt": (
            "工作目录就是仓库根目录。把 src/legacy.ts 里那个求和函数的名字改成 `calcSum`（驼峰），"
            "函数定义和所有调用点都要改。注意：文件里的实际写法可能与 `calcTotal` 不完全一致"
            "（大小写、下划线），请以磁盘内容为准完成修改；改完说明实际改了什么。"
        ),
        "writes": True,
    },
}


def validate(task: str, root: Path, answer: str, before: dict[str, str]) -> tuple[bool, list[str]]:
    """独立校验：磁盘内容 + 答案。不采信模型自述。"""
    problems: list[str] = []
    after = snapshot(root)

    # 未授权文件不得变化（所有任务都断言）
    allowed = {
        "R05": {"src/counter.ts"},
        "R06": {"src/h1.ts", "src/h2.ts", "src/h3.ts"},
        "R07": {"src/config.ts"},
        "R08": {"src/legacy.ts"},
        "R01": set(),
        "R03": set(),
    }[task]
    for name, h in before.items():
        if name in allowed:
            continue
        if after.get(name) != h:
            problems.append(f"未授权文件被改: {name}")
    for name in after:
        if name not in before:
            problems.append(f"多出文件: {name}")

    def read(rel: str) -> str:
        p = root / rel
        return p.read_text(encoding="utf-8", errors="replace") if p.exists() else ""

    if task == "R01":
        files = [n for n in after]
        if len(files) != 14:
            problems.append(f"文件数应为 14，实际 {len(files)}")
        if "12" not in answer:
            problems.append("答案未体现 12 个 handle_ 文件")
        svc_hit = sum(1 for i in range(1, 13) if f"svc{i:02d}" in answer)
        # 模型常用「svc01 ~ svc12」的范围写法，两个端点都在就算列全了。
        range_form = "svc01" in answer and "svc12" in answer
        if svc_hit < 6 and not range_form:
            problems.append(f"答案只列出 {svc_hit}/12 个 svc 文件")

    elif task == "R03":
        for name in ("a", "b"):
            for i in (1, 2, 3):
                token = f"TOKEN_{name}{i}_"
                if token not in answer:
                    problems.append(f"答案缺少 {token}")

    elif task == "R05":
        want = (
            "let count = 0;\n"
            "\n"
            "export function tick() {\n"
            "  count += 2;\n"
            "  return count;\n"
            "}\n"
            "\n"
            "export function bump() {\n"
            "  count += 1;\n"
            "  return count;\n"
            "}\n"
        )
        if read("src/counter.ts") != want:
            problems.append("counter.ts 不是期望内容（可能改错处或格式变化）")

    elif task == "R06":
        for i, n in enumerate(("alpha", "beta", "gamma"), 1):
            got = read(f"src/h{i}.ts")
            if f"export function on_{n}(" not in got:
                problems.append(f"h{i}.ts 未改成 on_{n}")
            if f"handle_{n}" in got:
                problems.append(f"h{i}.ts 仍残留 handle_{n}")

    elif task == "R07":
        got = read("src/config.ts")
        net = re.search(r"loadNetwork\([^)]*\)\s*\{(.*?)\}", got, re.S)
        if not net or "timeout = 60;" not in net.group(1):
            problems.append("loadNetwork 内未改成 60")
        if got.count("timeout = 30;") != 2:
            problems.append(f"其余处应保持 2 个 timeout = 30;，实际 {got.count('timeout = 30;')}")
        if "timeout = 60;" in got.split("loadNetwork")[0]:
            problems.append("loadNetwork 之前的位置被误改")

    elif task == "R08":
        got = read("src/legacy.ts")
        if "export function calcSum(" not in got:
            problems.append("未出现 calcSum(")
        if "calc_total" in got:
            problems.append("calc_total 仍残留（应全部改为 calcSum，含调用处）")

    return (not problems), problems


# ---------------------------------------------------------------- 工具集


def build_tools(side: str, exe: Path) -> list[dict]:
    """返回 OpenAI tools 定义。fsops 侧取真实 schema，原生侧手写宿主等价工具。"""
    if side == "fsops":
        raw = json.loads(subprocess.check_output([str(exe), "schema"], timeout=60))
        tools = []
        for t in raw:
            tools.append(
                {
                    "type": "function",
                    "function": {
                        "name": t["name"],
                        "description": t.get("description", ""),
                        "parameters": t.get("inputSchema", {"type": "object"}),
                    },
                }
            )
        return tools

    def fn(name: str, desc: str, props: dict, required: list[str]) -> dict:
        return {
            "type": "function",
            "function": {
                "name": name,
                "description": desc,
                "parameters": {
                    "type": "object",
                    "properties": props,
                    "required": required,
                    "additionalProperties": False,
                },
            },
        }

    return [
        fn(
            "read_file",
            "读取文本文件，返回带行号的正文（N|内容）。",
            {
                "path": {"type": "string", "description": "文件路径"},
                "offset": {"type": "integer", "description": "起始行，1 基", "minimum": 1},
                "limit": {"type": "integer", "description": "最多返回行数，默认 2000"},
            },
            ["path"],
        ),
        fn(
            "write_file",
            "整体覆盖写入文本文件，自动建目录。",
            {
                "path": {"type": "string"},
                "content": {"type": "string"},
            },
            ["path", "content"],
        ),
        fn(
            "edit_file",
            "精确替换：old_string 必须在文件中唯一匹配，否则报错。",
            {
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean", "description": "替换全部匹配，默认 false"},
            },
            ["path", "old_string", "new_string"],
        ),
        fn(
            "grep",
            "按正则搜索文件内容，返回 path:line:content。",
            {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "搜索根目录，默认工作目录"},
                "glob": {"type": "string", "description": "只搜匹配的文件，如 *.ts"},
                "ignore_case": {"type": "boolean"},
            },
            ["pattern"],
        ),
        fn(
            "find_files",
            "按 glob 查找文件，返回相对路径。",
            {
                "pattern": {"type": "string", "description": "如 **/*.ts"},
                "path": {"type": "string", "description": "搜索根目录，默认工作目录"},
            },
            ["pattern"],
        ),
        fn(
            "run_command",
            "在工作目录执行 shell 命令（bash），返回退出码与输出。",
            {
                "command": {"type": "string"},
                "timeout_s": {"type": "integer", "description": "超时秒数，默认 60"},
            },
            ["command"],
        ),
    ]


class FsopsSession:
    """fsops 侧：持久 MCP 会话，相对路径解析到本次运行目录。"""

    side = "fsops"

    def __init__(self, exe: Path, root: Path):
        self.root = root
        # 关键：MCP 进程的工作目录必须是本次运行目录，否则 fs_exec 的缺省 cwd
        # 会落到 fsops 仓库本身，评测结果无效（实测模型因此数到了 fsops 的 32KB 文件列表）。
        self.mcp = Mcp(exe, cwd=root)

    def _abs(self, value: str) -> str:
        return resolve(value, self.root)

    def _fix(self, args: dict) -> dict:
        out = dict(args)
        for key in ("path", "src", "dst"):
            if isinstance(out.get(key), str):
                out[key] = self._abs(out[key])
        if isinstance(out.get("paths"), list):
            out["paths"] = [self._abs(p) if isinstance(p, str) else p for p in out["paths"]]
        for key in ("reads", "files"):
            if isinstance(out.get(key), list):
                fixed = []
                for item in out[key]:
                    if isinstance(item, dict) and isinstance(item.get("path"), str):
                        item = dict(item)
                        item["path"] = self._abs(item["path"])
                    fixed.append(item)
                out[key] = fixed
        if isinstance(out.get("cwd"), str):
            out["cwd"] = self._abs(out["cwd"])
        return out

    def call(self, name: str, args: dict) -> tuple[str, bool]:
        try:
            return self.mcp.call(name, **self._fix(args))
        except Exception as e:  # JSON-RPC 层错误按工具失败上报，不中断任务
            return f"ERROR: {e}", True

    def close(self) -> None:
        self.mcp.close()


class NativeSession:
    """原生侧：宿主等价工具的最小实现（读写、精确替换、搜索、命令）。"""

    side = "native"

    def __init__(self, root: Path):
        self.root = root

    def _p(self, p: str) -> Path:
        return Path(win_path(p, self.root))

    def call(self, name: str, args: dict) -> tuple[str, bool]:
        try:
            fn = getattr(self, f"_t_{name}", None)
            if fn is None:
                return f"ERROR: 未知工具 {name}", True
            return fn(**args), False
        except TypeError as e:
            return f"ERROR: 参数不匹配: {e}", True
        except Exception as e:
            return f"ERROR: {e}", True

    # -- 工具实现

    def _t_read_file(self, path: str, offset: int = 1, limit: int = 2000) -> str:
        p = self._p(path)
        if not p.is_file():
            return f"ERROR: 文件不存在: {path}"
        lines = p.read_text(encoding="utf-8", errors="replace").splitlines()
        start = max(1, int(offset))
        end = min(len(lines), start - 1 + max(1, int(limit or 2000)))
        body = "\n".join(f"{i}|{lines[i - 1]}" for i in range(start, end + 1))
        note = "" if end >= len(lines) else f"\n… 共 {len(lines)} 行，已到 {end}，用 offset={end + 1} 续读"
        return body + note

    def _t_write_file(self, path: str, content: str) -> str:
        p = self._p(path)
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content, encoding="utf-8", newline="\n")
        return f"已写入 {path}（{len(content)} 字符）"

    def _t_edit_file(
        self, path: str, old_string: str, new_string: str, replace_all: bool = False
    ) -> str:
        p = self._p(path)
        if not p.is_file():
            return f"ERROR: 文件不存在: {path}"
        text = p.read_text(encoding="utf-8")
        hits = text.count(old_string)
        if hits == 0:
            return f"ERROR: 0 处匹配，文件内容不是这样（用 read_file 查看实际内容）"
        if hits > 1 and not replace_all:
            return f"ERROR: {hits} 处匹配，不唯一；请给出更多上下文或 replace_all=true"
        new_text = text.replace(old_string, new_string) if replace_all else text.replace(old_string, new_string, 1)
        p.write_text(new_text, encoding="utf-8", newline="\n")
        return f"已替换 {hits if replace_all else 1} 处"

    def _t_grep(
        self, pattern: str, path: str = ".", glob: str = "", ignore_case: bool = False
    ) -> str:
        base = self._p(path)
        rx = re.compile(pattern, re.I if ignore_case else 0)
        out: list[str] = []
        files = base.rglob("*") if base.is_dir() else [base]
        for f in files:
            if not f.is_file():
                continue
            if glob and not f.match(glob):
                continue
            try:
                for i, line in enumerate(f.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
                    if rx.search(line):
                        out.append(f"{f.relative_to(self.root).as_posix()}:{i}:{line[:200]}")
                        if len(out) >= 50:
                            return "\n".join(out) + "\n… 已达 50 条上限"
            except OSError:
                continue
        return "\n".join(out) if out else "（无匹配）"

    def _t_find_files(self, pattern: str, path: str = ".") -> str:
        base = self._p(path)
        hits = [p for p in base.rglob(pattern) if p.is_file()][:200]
        return "\n".join(p.relative_to(self.root).as_posix() for p in hits) or "（无匹配）"

    def _t_run_command(self, command: str, timeout_s: int = 60) -> str:
        proc = subprocess.run(
            ["bash", "-lc", command],
            cwd=str(self.root),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=max(1, int(timeout_s or 60)),
        )
        out = (proc.stdout or "") + (proc.stderr or "")
        return f"exit={proc.returncode}\n{out[:8000]}"

    def close(self) -> None:
        pass


# ---------------------------------------------------------------- API


def chat(messages: list[dict], tools: list[dict]) -> tuple[dict, dict]:
    body = {
        "model": MODEL,
        "messages": messages,
        "tools": tools,
        "tool_choice": "auto",
        "stream": False,
    }
    req = urllib.request.Request(
        f"{API_BASE}/chat/completions",
        data=json.dumps(body, ensure_ascii=False).encode("utf-8"),
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=API_TIMEOUT_S) as resp:
            data = json.load(resp)
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", "replace")[:500]
        raise RuntimeError(f"API {e.code}: {detail}") from e
    return data["choices"][0]["message"], data.get("usage") or {}


# ---------------------------------------------------------------- 单次运行


def run_one(task: str, side: str, exe: Path, keep: bool) -> dict:
    root = Path(tempfile.mkdtemp(prefix=f"fsops-ab-{task}-{side}-"))
    record: dict = {
        "task": task,
        "side": side,
        "model": MODEL,
        "root": posix(root),
        "started_at": time.strftime("%Y-%m-%dT%H:%M:%S"),
        "calls": 0,
        "failed_calls": 0,
        "turns": 0,
        "usage": {"prompt": 0, "completion": 0, "cached": 0},
        "tool_chars": 0,
        "wall_ms": 0,
        "ok": False,
        "problems": [],
        "answer": "",
        "transcript": [],
        "error": None,
    }
    t0 = time.time()
    session = None
    try:
        make_fixture(task, root)
        before = snapshot(root)
        tools = build_tools(side, exe)
        record["tools_bytes"] = len(json.dumps(tools, ensure_ascii=False))
        session = FsopsSession(exe, root) if side == "fsops" else NativeSession(root)
        messages = [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": TASKS[task]["prompt"]},
        ]

        for _ in range(MAX_TURNS):
            if time.time() - t0 > RUN_TIMEOUT_S:
                record["error"] = "运行超时"
                break
            msg, usage = chat(messages, tools)
            record["turns"] += 1
            record["usage"]["prompt"] += int(usage.get("prompt_tokens") or 0)
            record["usage"]["completion"] += int(usage.get("completion_tokens") or 0)
            cached = (usage.get("prompt_tokens_details") or {}).get("cached_tokens")
            record["usage"]["cached"] += int(cached or 0)

            assistant = {"role": "assistant", "content": msg.get("content") or ""}
            if msg.get("tool_calls"):
                assistant["tool_calls"] = msg["tool_calls"]
            messages.append(assistant)

            if not msg.get("tool_calls"):
                record["answer"] = (msg.get("content") or "").strip()
                break

            for tc in msg["tool_calls"]:
                name = tc["function"]["name"]
                raw = tc["function"].get("arguments") or "{}"
                try:
                    args = json.loads(raw)
                    if not isinstance(args, dict):
                        raise ValueError("arguments 必须是 JSON 对象")
                except Exception as e:
                    out, is_err = f"ERROR: 参数不是合法 JSON: {e}", True
                    args = {}
                else:
                    out, is_err = session.call(name, args)

                record["calls"] += 1
                if is_err:
                    record["failed_calls"] += 1
                record["tool_chars"] += len(out)
                record["transcript"].append(
                    {"tool": name, "args": args, "is_error": is_err, "out_chars": len(out), "out_head": out[:300]}
                )
                messages.append(
                    {"role": "tool", "tool_call_id": tc["id"], "content": out[:20000]}
                )
        else:
            record["error"] = f"超过 {MAX_TURNS} 个依赖批次仍无最终答案"

        record["ok"], record["problems"] = validate(task, root, record["answer"], before)
    except Exception as e:
        record["error"] = f"{type(e).__name__}: {e}"
        record["problems"] = [record["error"]]
        record["trace"] = traceback.format_exc()[-1500:]
    finally:
        if session is not None:
            session.close()
        record["wall_ms"] = int((time.time() - t0) * 1000)
        if not keep:
            shutil.rmtree(root, ignore_errors=True)
            record["root"] = "(已删除)"

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    with (OUT_DIR / "runs.jsonl").open("a", encoding="utf-8") as f:
        f.write(json.dumps(record, ensure_ascii=False) + "\n")
    return record


# ---------------------------------------------------------------- 汇总


def summarize(records: list[dict]) -> str:
    lines: list[str] = []
    head = (
        "| 任务 | 侧 | 运行 | 成功 | 轮次(中位) | 调用(中位) | 失败调用 | 输入 tok(中位) | 输出 tok | 墙钟 s(中位) |"
        "\n|---|---|---:|---:|---:|---:|---:|---:|---:|---:|"
    )
    lines.append(head)
    for task in sorted({r["task"] for r in records}):
        for side in ("fsops", "native"):
            rs = [r for r in records if r["task"] == task and r["side"] == side]
            if not rs:
                continue
            ok = sum(1 for r in rs if r["ok"])
            med = lambda k: statistics.median([r[k] for r in rs])  # noqa: E731
            med_u = lambda k: statistics.median([r["usage"][k] for r in rs])  # noqa: E731
            lines.append(
                f"| {task} | {side} | {len(rs)} | {ok}/{len(rs)} | {med('turns'):.1f} | "
                f"{med('calls'):.1f} | {sum(r['failed_calls'] for r in rs)} | "
                f"{med_u('prompt'):.0f} | {sum(r['usage']['completion'] for r in rs)} | "
                f"{med('wall_ms') / 1000:.1f} |"
            )
    lines.append("")
    for side in ("fsops", "native"):
        rs = [r for r in records if r["side"] == side]
        if not rs:
            continue
        lines.append(
            f"- {side} 合计：运行 {len(rs)}，成功 {sum(1 for r in rs if r['ok'])}，"
            f"调用 {sum(r['calls'] for r in rs)}，失败调用 {sum(r['failed_calls'] for r in rs)}，"
            f"输入 {sum(r['usage']['prompt'] for r in rs)} tok，输出 {sum(r['usage']['completion'] for r in rs)} tok，"
            f"缓存命中 {sum(r['usage']['cached'] for r in rs)} tok"
        )
    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser()
    all_tasks = list(TASKS)
    ap.add_argument("--tasks", default=",".join(all_tasks), help=f"逗号分隔，默认全部：{','.join(all_tasks)}")
    ap.add_argument("--times", type=int, default=3, help="每任务每侧运行次数，默认 3")
    ap.add_argument("--sides", default="fsops,native")
    ap.add_argument("--smoke", action="store_true", help="冒烟：R01 × 1 次 × 两侧")
    ap.add_argument("--keep", action="store_true", help="保留运行目录（默认清理）")
    ap.add_argument("--exe", default=os.environ.get("FSOPS_BIN", str(PROJECT_ROOT / "target/release/fsops.exe")))
    args = ap.parse_args()

    if args.smoke:
        tasks, times = ["R01"], 1
    else:
        tasks = [t.strip() for t in args.tasks.split(",") if t.strip()]
        times = args.times
    sides = [s.strip() for s in args.sides.split(",") if s.strip()]
    for t in tasks:
        if t not in TASKS:
            raise SystemExit(f"未知任务 {t}；可选 {','.join(TASKS)}")

    exe = Path(args.exe).resolve()
    if not exe.exists():
        raise SystemExit(f"二进制不存在: {exe}")

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    if (OUT_DIR / "runs.jsonl").exists():
        (OUT_DIR / "runs.jsonl").unlink()

    print(f"模型 {MODEL}；二进制 {exe}")
    print(f"任务 {tasks} × {times} 次 × 侧 {sides}")
    records: list[dict] = []
    for task in tasks:
        for i in range(times):
            for side in sides:
                rec = run_one(task, side, exe, args.keep)
                records.append(rec)
                flag = "OK " if rec["ok"] else "FAIL"
                print(
                    f"  [{flag}] {task} {side} #{i + 1}: 轮次 {rec['turns']}、调用 {rec['calls']}、"
                    f"失败 {rec['failed_calls']}、输入 {rec['usage']['prompt']} tok、"
                    f"{rec['wall_ms'] / 1000:.1f}s"
                    + (f"；问题 {rec['problems']}" if not rec["ok"] else "")
                )

    report = summarize(records)
    print("\n" + report)
    (OUT_DIR / "summary.md").write_text(report + "\n", encoding="utf-8")
    print(f"\n原始记录：{posix(OUT_DIR / 'runs.jsonl')}")
    return 0 if all(r["ok"] for r in records) else 1


if __name__ == "__main__":
    raise SystemExit(main())
