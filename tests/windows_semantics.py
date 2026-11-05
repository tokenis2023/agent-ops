"""用真实文件验证 Windows 语义：GBK/UTF-16/CRLF/长路径/中文文件名/并发保护。

直接驱动 MCP 服务（和 agent 实际调用同一条路径），断言写回后的字节。
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
EXE = PROJECT_ROOT / "target" / "debug" / "fsops.exe"
root = tempfile.mkdtemp(prefix="fsops-e2e-")
results = []


def call(name, **arguments):
    """单次进程调用：最贴近 agent 的用法（每次工具调用一个进程）。"""
    reqs = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize",
         "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "t", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
         "params": {"name": name, "arguments": arguments}},
    ]
    payload = "\n".join(json.dumps(r, ensure_ascii=False) for r in reqs) + "\n"
    p = subprocess.run([str(EXE), "mcp"], input=payload.encode("utf-8"),
                       capture_output=True, timeout=30)
    lines = [l for l in p.stdout.decode("utf-8").splitlines() if l.strip()]
    last = json.loads(lines[-1])
    return last["result"]["content"][0]["text"], last["result"].get("isError")


def posix(p):
    drive, tail = os.path.splitdrive(os.path.abspath(p))
    tail = tail.replace("\\", "/")
    return f"/{drive[0].lower()}{tail}" if drive else tail


def check(name, cond, detail=""):
    results.append((name, cond, detail))
    print(("PASS " if cond else "FAIL ") + name + ("" if cond else f"  -> {detail}"))


# 1. GBK + CRLF：改中文，字节必须仍是 GBK + CRLF
gbk = os.path.join(root, "中文文件.txt")
open(gbk, "wb").write("第一行\r\n第二行内容\r\n".encode("gbk"))
text, _ = call("fs_edit", path=posix(gbk), edits=[{"old": "第二行内容", "new": "改过的第二行"}])
raw = open(gbk, "rb").read()
check("GBK 文件改中文后编码不变", raw == "第一行\r\n改过的第二行\r\n".encode("gbk"), raw)
check("GBK 编辑返回摘要含 ok", text.startswith("ok"), text[:80])

# GBK 无法表示的字符必须拒绝，禁止静默改成 UTF-8
lossy = os.path.join(root, "gbk-loss.txt")
lossy_original = "中文\r\n".encode("gbk")
open(lossy, "wb").write(lossy_original)
text, _ = call("fs_edit", path=posix(lossy),
               edits=[{"old": "中文", "new": "中文🙂"}])
check("GBK 不可表示字符拒绝写入",
      text.startswith("E_ENCODING_LOSS") and open(lossy, "rb").read() == lossy_original,
      text)
text, _ = call("fs_edit", path=posix(lossy), dry_run=True,
               edits=[{"old": "中文", "new": "中文🙂"}])
check("GBK dry_run 不可表示字符拒绝且不改字节",
      text.startswith("E_ENCODING_LOSS") and open(lossy, "rb").read() == lossy_original,
      text)

# 混合换行默认拒绝，避免编辑时悄悄统一换行
mixed = os.path.join(root, "mixed.txt")
mixed_original = b"alpha\r\nbeta\ngamma\r\n"
open(mixed, "wb").write(mixed_original)
text, _ = call("fs_edit", path=posix(mixed),
               edits=[{"old": "beta", "new": "changed"}])
check("混合换行默认拒绝编辑",
      text.startswith("E_MIXED_EOL") and open(mixed, "rb").read() == mixed_original,
      text)
text, _ = call("fs_edit", path=posix(mixed), normalize_eol=True,
               edits=[{"old": "beta", "new": "changed"}])
check("显式允许后混合换行归一为 LF",
      text.startswith("ok") and open(mixed, "rb").read() == b"alpha\nchanged\ngamma\n",
      text)

mixed_write = os.path.join(root, "mixed-write.txt")
mixed_write_original = b"one\r\ntwo\n"
open(mixed_write, "wb").write(mixed_write_original)
text, _ = call("fs_write", path=posix(mixed_write), content="one\r\nchanged\n")
check("混合换行默认拒绝覆盖",
      text.startswith("E_MIXED_EOL") and open(mixed_write, "rb").read() == mixed_write_original,
      text)
text, _ = call("fs_write", path=posix(mixed_write), content="one\r\nchanged\n",
               normalize_eol=True)
check("显式允许后覆盖归一为 LF",
      text.startswith("ok") and open(mixed_write, "rb").read() == b"one\nchanged\n",
      text)

# 待写内容自身含混合换行时，无论目标是否已存在都必须先校验。
new_mixed_reject = os.path.join(root, "new-mixed-reject.txt")
text, _ = call("fs_write", path=posix(new_mixed_reject), content="one\r\nchanged\n")
check("新建文件混合换行默认拒绝",
      text.startswith("E_MIXED_EOL") and not os.path.exists(new_mixed_reject),
      text)
new_mixed_normalize = os.path.join(root, "new-mixed-normalize.txt")
text, _ = call("fs_write", path=posix(new_mixed_normalize), content="one\r\nchanged\n",
               normalize_eol=True)
check("新建文件混合换行显式归一 LF",
      text.startswith("ok") and open(new_mixed_normalize, "rb").read() == b"one\nchanged\n",
      text)

lf_mixed = os.path.join(root, "lf-mixed.txt")
lf_original = b"one\ntwo\n"
open(lf_mixed, "wb").write(lf_original)
text, _ = call("fs_write", path=posix(lf_mixed), content="one\r\nchanged\n")
check("LF 文件写入混合内容时沿用 LF",
      text.startswith("ok") and open(lf_mixed, "rb").read() == b"one\nchanged\n", text)

crlf_mixed = os.path.join(root, "crlf-mixed.txt")
crlf_original = b"one\r\ntwo\r\n"
open(crlf_mixed, "wb").write(crlf_original)
text, _ = call("fs_write", path=posix(crlf_mixed), content="one\r\nchanged\n")
check("CRLF 文件写入混合内容时沿用 CRLF",
      text.startswith("ok") and
      open(crlf_mixed, "rb").read() == b"one\r\nchanged\r\n", text)

# 2. UTF-16LE 带 BOM
u16 = os.path.join(root, "utf16.txt")
open(u16, "wb").write(b"\xff\xfe" + "alpha\r\nbeta\r\n".encode("utf-16-le"))
call("fs_edit", path=posix(u16), edits=[{"old": "beta", "new": "gamma"}])
raw = open(u16, "rb").read()
check("UTF-16LE 改后仍是 UTF-16LE+BOM",
      raw == b"\xff\xfe" + "alpha\r\nGamma\r\n".replace("Gamma", "gamma").encode("utf-16-le"), raw)

# 3. 多行块替换（缩进敏感），验证匹配与行号
code = os.path.join(root, "a.rs")
open(code, "w", encoding="utf-8", newline="\n").write("fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n")
text, _ = call("fs_edit", path=posix(code),
               edits=[{"old": "    let x = 1;\n    println!(\"{x}\");", "new": "    let x = 2;\n    dbg!(x);"}])
body = open(code, encoding="utf-8").read()
check("多行替换正确", "let x = 2;\n    dbg!(x);" in body, body)
check("返回只报行号不返全文", "dbg!" not in text and "let x = 2" not in text and text.startswith("ok"), text)

# 4. 歧义匹配要报行号而不是乱改
dup = os.path.join(root, "dup.txt")
open(dup, "w", encoding="utf-8").write("x();\nx();\n")
text, is_err = call("fs_edit", path=posix(dup), edits=[{"old": "x();", "new": "y();"}])
check("歧义匹配被拒绝", text.startswith("E_AMBIGUOUS") and "1,2" in text, text)
check("拒绝时文件未被改动", open(dup, encoding="utf-8").read() == "x();\nx();\n")

# 5. 未命中时给候选行
text, _ = call("fs_edit", path=posix(dup), edits=[{"old": "z();", "new": "w();"}])
check("未命中有提示", text.startswith("E_NO_MATCH"), text)

# 6. 长路径（>260 字符）
deep = root
for i in range(8):
    deep = os.path.join(deep, "verylongdirectoryname" + str(i) * 3)
os.makedirs(deep, exist_ok=True)
lp = os.path.join(deep, "long.txt")
open(lp, "w", encoding="utf-8").write("hello\n")
text, _ = call("fs_read", path=posix(lp))
check("超长路径可读", "1|hello" in text, text[:120])
text, _ = call("fs_edit", path=posix(lp), edits=[{"old": "hello", "new": "world"}])
check("超长路径可写", open(lp, encoding="utf-8").read() == "world\n", text)

# 7. 并发保护：hash 不匹配要拒绝
h_text, _ = call("fs_stat", path=posix(lp))
old_hash = h_text.split("hash=")[-1].strip()
open(lp, "w", encoding="utf-8").write("changed by someone else\n")
text, _ = call("fs_edit", path=posix(lp), expect_hash=old_hash,
               edits=[{"old": "changed", "new": "mine"}])
check("hash 失配拒绝写入", text.startswith("E_STALE"), text)

# 8. 新建文件自动建目录
newf = os.path.join(root, "sub", "deep", "new.txt")
text, _ = call("fs_write", path=posix(newf), content="a\nb\n")
check("自动创建父目录", os.path.exists(newf) and open(newf, encoding="utf-8").read() == "a\nb\n", text)

# 9. grep 不扫 node_modules
os.makedirs(os.path.join(root, "node_modules", "pkg"), exist_ok=True)
open(os.path.join(root, "node_modules", "pkg", "i.js"), "w").write("needle\n")
open(os.path.join(root, "src.js"), "w").write("needle\n")
text, _ = call("fs_grep", pattern="needle", path=posix(root), limit=50)
check("grep 跳过 node_modules", "src.js" in text and "node_modules" not in text, text)

# 10. 二进制文件不被 grep 当文本
open(os.path.join(root, "bin.dat"), "wb").write(b"\x00\x01\x02needle")
text, _ = call("fs_grep", pattern="needle", path=posix(root), limit=50)
check("grep 跳过二进制", "bin.dat" not in text, text)

# 11. 搜索输出模式：files/count/matches/snippets
open(os.path.join(root, "grep-a.txt"), "w").write("needle\nneedle\n")
open(os.path.join(root, "grep-b.txt"), "w").write("needle\n")
text, _ = call("fs_grep", pattern="needle", path=posix(root),
               glob="grep-*.txt", output="files")
check("grep files 只给路径", text.splitlines() == ["grep-a.txt", "grep-b.txt"], text)
text, _ = call("fs_grep", pattern="needle", path=posix(root),
               glob="grep-*.txt", output="count")
check("grep count 给路径与命中数",
      text.splitlines() == ["grep-a.txt:2", "grep-b.txt:1"], text)
serial_text, _ = call("fs_grep", pattern="needle", path=posix(root),
                      glob="grep-*.txt", limit=2)
parallel_text, _ = call("fs_grep", pattern="needle", path=posix(root),
                        glob="grep-*.txt", limit=2, parallel=True)
check("grep 并行与串行输出一致", parallel_text == serial_text,
      f"serial={serial_text!r} parallel={parallel_text!r}")

open(os.path.join(root, "snippet.txt"), "w").write(
    "top1\ntop2\nneedle\nafter1\nafter2\n")
text, _ = call("fs_grep", pattern="needle", path=posix(root),
               glob="snippet.txt", output="snippets")
check("grep snippets 默认带上下文",
      "1-top1" in text and "2-top2" in text and
      "4-after1" in text and "5-after2" in text, text)
text, _ = call("fs_grep", pattern="needle", path=posix(root),
               glob="snippet.txt", context=1)
check("grep 默认 matches 保留显式上下文",
      "2-top2" in text and "3:needle" in text and "4-after1" in text, text)

# 12. 分页 offset 前进且不重复第一页
text, _ = call("fs_grep", pattern="needle", path=posix(root),
               glob="grep-*.txt", limit=1)
check("grep 截断给 next_offset", "next_offset=1" in text, text)
text, _ = call("fs_grep", pattern="needle", path=posix(root),
               glob="grep-*.txt", limit=1, offset=1)
check("grep 重放 offset 前进",
      "next_offset=2" in text and "1:needle" not in text and "2:needle" in text, text)

# 13. 多文件编辑：全部预检后一次写入
batch_a = os.path.join(root, "batch-a.txt")
batch_b = os.path.join(root, "batch-b.txt")
open(batch_a, "w", encoding="utf-8").write("alpha\n")
open(batch_b, "w", encoding="utf-8").write("beta\ngamma\n")
text, _ = call("fs_edit", files=[
    {"path": posix(batch_a), "edits": [{"old": "alpha", "new": "changed"}]},
    {"path": posix(batch_b), "edits": [
        {"old": "beta", "new": "changed"},
        {"old": "gamma", "new": "removed"},
    ]},
])
check("多文件编辑一次写入",
      text.startswith("ok 2 个文件") and
      open(batch_a, encoding="utf-8").read() == "changed\n" and
      open(batch_b, encoding="utf-8").read() == "changed\nremoved\n", text)

# 14. grep 直接搜索单文件与失败读取的错误标记
text, _ = call("fs_grep", pattern="needle", path=posix(os.path.join(root, "src.js")))
check("grep 可直接搜索单文件", "src.js" in text and "1:needle" in text, text)
missing_text, missing_err = call("fs_read", path=posix(os.path.join(root, "missing.txt")))
check("读取失败标记为 isError", missing_text.startswith("E_READ") and missing_err is True,
      missing_text)
meta_text, _ = call("fs_stat", path=posix(os.path.join(root, "src.js")), detail="meta")
check("fs_stat meta 不读内容",
      "file" in meta_text and "hash=" not in meta_text and "行" not in meta_text,
      meta_text)

# 15. 时间预算覆盖目录遍历
text, _ = call("fs_grep", pattern="needle", path=posix(root),
               glob="**/*.txt", time_budget_ms=0)
check("grep 时间预算包含目录遍历",
      "时间预算用尽" in text and "扫描 0 个文件" in text, text)

passed = sum(1 for _, c, _ in results if c)
print(f"\n{passed}/{len(results)} 通过   目录={root}")
# 用完即清：临时夹具不留在磁盘上
if passed == len(results):
    shutil.rmtree(root, ignore_errors=True)
sys.exit(0 if passed == len(results) else 1)
