"""用真实文件验证 Windows 语义：GBK/UTF-16/CRLF/长路径/中文文件名/并发保护。

直接驱动 MCP 服务（和 agent 实际调用同一条路径），断言写回后的字节。
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

# GitHub Windows runner 的默认控制台可能是 cp1252，测试输出含中文会编码失败。
for stream in (sys.stdout, sys.stderr):
    if hasattr(stream, "reconfigure"):
        stream.reconfigure(encoding="utf-8")

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

# 16. fsops integrate / doctor：规则块幂等、检查、移除
def cli(*args):
    p = subprocess.run([str(EXE), *args], capture_output=True, timeout=30)
    return p.returncode, p.stdout.decode("utf-8"), p.stderr.decode("utf-8")


adopt_dir = os.path.join(root, "adopt")
os.makedirs(adopt_dir)
agents = os.path.join(adopt_dir, "AGENTS.md")
open(agents, "w", encoding="utf-8").write("# 项目\n\n用户内容\n")
code, out, err = cli("integrate", "--rules", "--target", "agents", "--path", posix(adopt_dir))
check("integrate 写入规则块",
      code == 0 and "已追加" in out and "fsops:begin v1" in open(agents, encoding="utf-8").read(),
      out + err)
code, out, err = cli("integrate", "--rules", "--target", "agents", "--path", posix(adopt_dir))
check("integrate 重复执行无变化", code == 0 and "无变化" in out, out + err)
code, out, err = cli("integrate", "--rules", "--target", "agents", "--check", "--path", posix(adopt_dir))
check("integrate --check 通过", code == 0, out + err)
code, out, err = cli("doctor", "--json", "--path", posix(adopt_dir))
doctor = json.loads(out)
agents_status = next(r["status"] for r in doctor["rules"] if r["file"] == "AGENTS.md")
check("doctor 报告规则与软引导",
      code == 0 and agents_status == "ok" and doctor["enforcement"] == "soft", out)
code, out, err = cli("integrate", "--rules", "--target", "agents", "--remove", "--path", posix(adopt_dir))
check("integrate 移除规则块保留用户内容",
      code == 0 and "fsops:begin" not in open(agents, encoding="utf-8").read()
      and "用户内容" in open(agents, encoding="utf-8").read(),
      out + err)

# 17. DOCX 正文读取，且禁止把 docx 当文本编辑/覆盖
docx_path = os.path.join(root, "sample.docx")
with zipfile.ZipFile(docx_path, "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("[Content_Types].xml", "<?xml version='1.0'?><Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'/>")
    z.writestr(
        "word/document.xml",
        "<?xml version='1.0'?><w:document xmlns:w='http://schemas.openxmlformats.org/wordprocessingml/2006/main'><w:body>"
        "<w:p><w:r><w:t>DOCX 第一段</w:t></w:r></w:p>"
        "<w:p><w:r><w:t>DOCX 第二段</w:t></w:r></w:p></w:body></w:document>",
    )
text, _ = call("fs_read", path=posix(docx_path))
check("fs_read 抽取 DOCX 正文",
      "docx" in text and "1|DOCX 第一段" in text and "2|DOCX 第二段" in text, text)
text, _ = call("fs_stat", path=posix(docx_path), detail="full")
check("fs_stat full 识别 DOCX",
      " docx LF " in text and "行 hash=" in text, text)
text, _ = call("fs_grep", pattern="DOCX 第二段", path=posix(root), glob="*.docx")
check("fs_grep 搜索 DOCX 正文",
      "sample.docx" in text and "\n2:" in text, text)
text, _ = call("fs_grep", pattern="DOCX 第二段", path=posix(root), glob="*.docx", parallel=True)
check("fs_grep 并行搜索 DOCX 正文",
      "sample.docx" in text and "\n2:" in text, text)

# PDF：读正文、stat 识别、grep 命中、拒绝写入
pdf_path = os.path.join(root, "sample.pdf")
pdf_content = b"BT /F1 14 Tf 72 720 Td (PDF first line) Tj 0 -18 Td (PDF second line) Tj ET"
pdf_objs = [
    b"<< /Type /Catalog /Pages 2 0 R >>",
    b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
    b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
    b"<< /Length %d >>\nstream\n" % len(pdf_content) + pdf_content + b"\nendstream",
    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
]
pdf_out = bytearray(b"%PDF-1.4\n")
pdf_offsets = []
for i, body in enumerate(pdf_objs, start=1):
    pdf_offsets.append(len(pdf_out))
    pdf_out += b"%d 0 obj\n" % i + body + b"\nendobj\n"
pdf_xref = len(pdf_out)
pdf_out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(pdf_objs) + 1)
for off in pdf_offsets:
    pdf_out += b"%010d 00000 n \n" % off
pdf_out += (b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n"
            % (len(pdf_objs) + 1, pdf_xref))
open(pdf_path, "wb").write(bytes(pdf_out))
text, _ = call("fs_read", path=posix(pdf_path))
check("fs_read 抽取 PDF 正文",
      "pdf LF" in text and "PDF first line" in text, text)
text, _ = call("fs_stat", path=posix(pdf_path), detail="full")
check("fs_stat full 识别 PDF", " pdf LF " in text and "行 hash=" in text, text)
text, _ = call("fs_grep", pattern="PDF second line", path=posix(root), glob="*.pdf")
check("fs_grep 搜索 PDF 正文", "sample.pdf" in text, text)
text, _ = call("fs_write", path=posix(pdf_path), content="覆盖")
check("fs_write 拒绝覆盖 PDF", text.startswith("E_PDF"), text)
text, _ = call("fs_edit", path=posix(pdf_path), edits=[{"old": "PDF", "new": "X"}])
check("fs_edit 拒绝编辑 PDF", text.startswith("E_PDF"), text)

# 二进制：读、编辑、覆盖都必须拒绝，且字节不变
bin_path = os.path.join(root, "blob.bin")
bin_original = b"ABC\x00\x01\x02DEF\x00GHI"
open(bin_path, "wb").write(bin_original)
text, err = call("fs_read", path=posix(bin_path))
check("fs_read 拒绝二进制",
      text.startswith("E_BINARY") and err is True, text)
text, _ = call("fs_edit", path=posix(bin_path), edits=[{"old": "DEF", "new": "XYZ"}])
check("fs_edit 拒绝二进制", text.startswith("E_BINARY"), text)
text, _ = call("fs_write", path=posix(bin_path), content="hello")
check("fs_write 拒绝覆盖二进制", text.startswith("E_BINARY"), text)
check("二进制文件字节未被改动",
      open(bin_path, "rb").read() == bin_original, "文件被改写")
text, _ = call("fs_stat", path=posix(bin_path), detail="full")
check("fs_stat 标记二进制", " binary " in text and "hash=" in text, text)
text, err = call("fs_read", paths=[posix(os.path.join(root, "src.js")), posix(bin_path)])
check("多文件含二进制时标记 isError",
      "==" in text and "E_BINARY" in text and err is True, text)
text, err = call("fs_read", paths=[posix(os.path.join(root, "src.js")),
                                   posix(os.path.join(root, "missing.txt"))])
check("多文件含缺失时标记 isError",
      "E_READ" in text and err is True, text)
text, err = call("fs_read", paths=[posix(os.path.join(root, "src.js")),
                                   posix(os.path.join(root, "mixed.txt"))])
check("多文件全部成功时不误报 isError", err is False, text)

# 空文件与重复路径
empty_path = os.path.join(root, "empty.txt")
open(empty_path, "w", encoding="utf-8").close()
text, err = call("fs_read", path=posix(empty_path))
check("空文件报 0 行且无正文",
      " 0B 0行 " in text and err is False and not text.rstrip().endswith("|"), text)
text, _ = call("fs_stat", path=posix(empty_path), detail="full")
check("空文件 stat 报 0 行", " 0行 " in text, text)
text, _ = call("fs_read", paths=[posix(os.path.join(root, "src.js")),
                                posix(os.path.join(root, "src.js"))])
check("重复 paths 只读一次", text.count("needle") == 1 and "==" not in text, text)

# ranges：重叠不重复输出；越界明确提示；行数前后一致
lines_path = os.path.join(root, "lines.txt")
open(lines_path, "w", encoding="utf-8").write("".join(f"L{i}\n" for i in range(1, 21)))
text, _ = call("fs_read", path=posix(lines_path), ranges=[[1, 5], [3, 8]])
check("重叠 ranges 不重复输出",
      all(text.count(f"{i}|L{i}") == 1 for i in range(1, 9)), text)
text, _ = call("fs_read", path=posix(lines_path), ranges=[[1, 2], [100, 200]])
check("越界区间明确提示且行数一致",
      "超出文件范围" in text and "共20行" in text and " 20行 " in text, text)
text, _ = call("fs_read", path=posix(lines_path))
check("末尾换行不算额外一行", " 20行 " in text and "21|" not in text, text)

# UTF-16 带 BOM（纯中文，几乎无零字节）不能被误判成二进制，且编辑后保持编码
u16_path = os.path.join(root, "u16.txt")
open(u16_path, "wb").write("中文内容\n第二行\n".encode("utf-16"))
text, _ = call("fs_read", path=posix(u16_path))
check("纯中文 UTF-16 可正常读取",
      "utf-16" in text and "中文内容" in text and "E_BINARY" not in text, text)
text, err = call("fs_edit", path=posix(u16_path), edits=[{"old": "第二行", "new": "改过了"}])
raw = open(u16_path, "rb").read()
check("UTF-16 编辑后保持 BOM 与编码",
      raw[:2] == b"\xff\xfe" and "改过了" in raw.decode("utf-16"), repr(raw[:8]))
check("UTF-16 编辑成功", err is False and "ok" in text, text)

# glob 写法误用为正则时要给出可操作提示
text, err = call("fs_grep", path=posix(root), pattern="*.txt")
check("glob 误用为正则时给出提示",
      text.startswith("E_BAD_REGEX") and "是正则不是 glob" in text
      and ".txt" in text and err is True, text)

# 单行文件 + context：不应输出末尾幽灵空上下文行
one_path = os.path.join(root, "one.txt")
open(one_path, "w", encoding="utf-8").write("NEEDLE here\n")
text, _ = call("fs_grep", path=posix(one_path), pattern="NEEDLE",
               output="snippets", context=1)
check("上下文不输出幽灵空行", "2-" not in text and "NEEDLE here" in text, text)

# 默认跳过 node_modules；no_ignore=true 时必须能看到依赖源码
nm_root = os.path.join(root, "nm")
os.makedirs(os.path.join(nm_root, "node_modules", "pkg"), exist_ok=True)
os.makedirs(os.path.join(nm_root, "src"), exist_ok=True)
open(os.path.join(nm_root, "node_modules", "pkg", "c.js"), "w", encoding="utf-8").write("x\n")
open(os.path.join(nm_root, "src", "a.rs"), "w", encoding="utf-8").write("x\n")
text, _ = call("fs_glob", path=posix(nm_root), pattern="**/*.js")
check("默认跳过 node_modules", "无匹配" in text and "c.js" not in text, text)
text, _ = call("fs_glob", path=posix(nm_root), pattern="**/*.js", no_ignore=True)
check("no_ignore 可查依赖源码", "c.js" in text, text)
text, _ = call("fs_grep", path=posix(nm_root), pattern="x",
               output="files", no_ignore=True)
check("grep 也支持 no_ignore", "c.js" in text, text)

# 截断尾注的行号范围必须与实际输出一致（曾把 1-21 写成 12-21）
big_path = os.path.join(root, "big.txt")
open(big_path, "w", encoding="utf-8").write(
    "".join(f"line{i} " + "x" * 60 + "\n" for i in range(1, 201)))
text, _ = call("fs_read", path=posix(big_path), budget=600)
note = [l for l in text.splitlines() if "续读" in l]
body = [l for l in text.splitlines() if "|" in l and not l.startswith("…")]
check("截断尾注行号与正文一致",
      note and note[0].startswith("… 1-")
      and f"1-{body[-1].split('|')[0]}/" in note[0], f"{note} / {body[-1]}")

# fs_write 的 dry_run 必须真的不落盘（曾直接覆盖文件）
keep_path = os.path.join(root, "keep.txt")
open(keep_path, "w", encoding="utf-8").write("IMPORTANT DATA\n")
text, err = call("fs_write", path=posix(keep_path), content="CLOBBERED\n", dry_run=True)
check("fs_write dry_run 不改动已有文件",
      text.startswith("ok dry_run") and
      open(keep_path, encoding="utf-8").read() == "IMPORTANT DATA\n", text)
fresh_path = os.path.join(root, "brand-new.txt")
text, _ = call("fs_write", path=posix(fresh_path), content="hi\n", dry_run=True)
check("fs_write dry_run 不创建新文件",
      text.startswith("ok dry_run") and not os.path.exists(fresh_path), text)

# 模型可见的 schema 必须与实现一致：dry_run 在 fs_write/fs_edit 里都要可见
def tool_schemas():
    reqs = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize",
         "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "t", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
    ]
    payload = "\n".join(json.dumps(r) for r in reqs) + "\n"
    p = subprocess.run([str(EXE), "mcp"], input=payload.encode("utf-8"),
                       capture_output=True, timeout=30)
    for line in p.stdout.decode("utf-8").splitlines():
        d = json.loads(line)
        if d.get("id") == 2:
            return {t["name"]: t["inputSchema"]["properties"] for t in d["result"]["tools"]}
    return {}


schemas = tool_schemas()
check("fs_write 的 dry_run 对模型可见",
      "dry_run" in schemas.get("fs_write", {}), str(schemas.get("fs_write")))
check("fs_edit 的 dry_run 对模型可见",
      "dry_run" in schemas.get("fs_edit", {}), str(schemas.get("fs_edit")))
check("六个工具都在 tools/list 中",
      len(schemas) == 6, str(sorted(schemas)))

# 长行截断时要提示一次全文读法，且不能每行重复
minjs_path = os.path.join(root, "min.js")
open(minjs_path, "w", encoding="utf-8").write("var a=" + "Q" * 5000 + ";\n")
text, _ = call("fs_read", path=posix(minjs_path), max_line=80)
check("长行截断提示全文读法",
      text.count("max_line=0") == 1 and "有长行被截断" in text, text)
text, _ = call("fs_read", path=posix(minjs_path), max_line=0)
check("未截断时不提示", "有长行被截断" not in text, text[:200])

# 读取失败时必须指明是哪个文件（多文件场景尤其重要）
missing_path = os.path.join(root, "missing.txt")
text, err = call("fs_read", path=posix(missing_path))
check("单文件读取失败带出路径",
      text.startswith("E_READ") and posix(missing_path) in text and err is True, text)
text, err = call("fs_read", paths=[posix(one_path), posix(missing_path)])
check("多文件读取失败指明具体文件",
      posix(missing_path) in text and err is True, text)

# 带空格/中文/方括号的路径必须能正常读写
weird_dir = os.path.join(root, "sub dir")
os.makedirs(weird_dir, exist_ok=True)
weird_path = os.path.join(weird_dir, "a b [中文].txt")
open(weird_path, "w", encoding="utf-8").write("weird\n")
text, _ = call("fs_read", path=posix(weird_path))
check("特殊字符路径可读取", "weird" in text and "E_READ" not in text, text)
text, _ = call("fs_edit", path=posix(weird_path),
               edits=[{"old": "weird", "new": "edited"}])
check("特殊字符路径可编辑",
      open(weird_path, encoding="utf-8").read() == "edited\n", text)

# 深目录多文件读取要省略公共前缀，且不丢内容
deep_dir = os.path.join(root, *(["deep-padding-segment"] * 12))
os.makedirs(deep_dir, exist_ok=True)
deep_files = []
for n in ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]:
    fp = os.path.join(deep_dir, n)
    open(fp, "w", encoding="utf-8").write("content\n")
    deep_files.append(posix(fp))
text, _ = call("fs_read", paths=deep_files)
check("深路径省略公共前缀",
      "省略公共前缀" in text and text.count("content") == 5, text[:200])
text2, _ = call("fs_read", path=deep_files[0])
check("单文件不省略前缀", "省略公共前缀" not in text2, text2[:120])

# 差异统计必须反映真实行数变化（曾恒为 +0 -0）
diff_path = os.path.join(root, "diff.txt")
open(diff_path, "w", encoding="utf-8").write("a\nb\nc\nd\n")
text, _ = call("fs_edit", path=posix(diff_path), edits=[{"old": "b", "new": "B"}])
check("单行替换统计 +1 -1", "(+1 -1)" in text and "@ 2 " in text, text)
open(diff_path, "w", encoding="utf-8").write("a\nb\nc\nd\n")
text, _ = call("fs_edit", path=posix(diff_path), edits=[{"old": "b\nc", "new": "BC"}])
check("跨行合并统计 +1 -2", "(+1 -2)" in text, text)
open(diff_path, "w", encoding="utf-8").write("a\nb\n")
text, _ = call("fs_edit", path=posix(diff_path), edits=[{"old": "a", "new": "X\nY\nZ"}])
check("一行拆三行统计 +3 -1", "(+3 -1)" in text and "@ 1-3 " in text, text)

# 双重盘符路径要指出格式错误，否则模型会以为文件不存在
real_p = posix(os.path.join(root, "real.txt"))
open(os.path.join(root, "real.txt"), "w", encoding="utf-8").write("hello\n")
text, err = call("fs_read", path="/d" + real_p)
check("双重盘符给出纠正建议",
      err is True and "多了一层盘符" in text and real_p in text, text)

# E_ENCODING_LOSS 必须报出具体字符与行号，否则模型只能整段重写反复试
gbk_path = os.path.join(root, "gbk-loss.txt")
open(gbk_path, "wb").write("中文测试\n".encode("gbk"))
text, err = call("fs_edit", path=posix(gbk_path),
                 edits=[{"old": "中文测试", "new": "中文😀"}])
check("编码损失报出码位与行号",
      err is True and "U+1F600" in text and "第 1 行" in text and "UTF-8" in text, text)

# 长行截断提示必须给出可执行的 budget，否则"用 max_line=0"是死路
minjs = os.path.join(root, "min.js")
open(minjs, "w", encoding="utf-8").write("var a=1;" * 14000)
text, _ = call("fs_read", path=posix(minjs))
check("长行尾注给出所需 budget", "budget≈" in text, text[-120:])
text, err = call("fs_read", path=posix(minjs), max_line=0)
check("E_LINE_TOO_LONG 给出可执行 budget",
      err is True and "E_LINE_TOO_LONG" in text and "至少" in text, text)

# block=true 的命中行在函数内部时，应向外扩展为整个块
blk2 = os.path.join(root, "block-inside.rs")
open(blk2, "w", encoding="utf-8").write(
    "fn outer() {\n    let x = 1;\n    if x > 0 {\n        call();\n    }\n}\n")
text, err = call("fs_read", path=posix(blk2), start=2, block=True, budget=100000)
body = [l for l in text.splitlines() if "|" in l]
check("block=true 从函数内部展开到整个块",
      err is False and len(body) == 6 and body[0].startswith("1|fn outer"), text)
text, err = call("fs_read", path=posix(blk2), start=4, block=True, budget=100000)
body = [l for l in text.splitlines() if "|" in l]
check("block=true 嵌套块内展开到最近的 if",
      err is False and len(body) == 3 and body[0].startswith("3|"), text)

# fs_edit 行范围限定：同一字面量多处出现时只改指定区间
range_path = os.path.join(root, "range-edit.rs")
open(range_path, "w", encoding="utf-8").write(
    "fn a() {\n    let x = 42;\n    let y = 42;\n}\nfn b() {\n    let z = 42;\n}\n")
text, err = call("fs_edit", path=posix(range_path),
                 edits=[{"old": "42", "new": "MAX", "all": True, "start": 1, "end": 4}])
after = open(range_path, encoding="utf-8").read()
check("行范围限定只改区间内",
      err is False and "MAX" in after and "let z = 42;" in after, text + after)
text, err = call("fs_edit", path=posix(range_path),
                 edits=[{"old": "MAX", "new": "42", "all": True, "start": 1, "end": 1}])
check("范围内无匹配时报错并给出其他位置数",
      err is True and "E_NO_MATCH" in text and "处匹配" in text, text)

# block=true：从 start 行按括号配平定位代码块，免去猜 end
blk_path = os.path.join(root, "block-probe.rs")
open(blk_path, "w", encoding="utf-8").write(
    "fn outer() {\n    let x = 1;\n    if x > 0 {\n        println!(\"hi\");\n    }\n}\nfn other() {\n    let y = 2;\n}\n")
text, err = call("fs_read", path=posix(blk_path), start=1, block=True, budget=100000)
body = [l for l in text.splitlines() if "|" in l]
check("block=true 精确定位代码块",
      err is False and len(body) == 6 and body[-1].startswith("6|")
      and "fn other" not in text, text)

# count 模式默认不受 limit 截断（统计必须准确）
big_cnt = os.path.join(root, "big-count.txt")
open(big_cnt, "w", encoding="utf-8").write("hit\n" * 150)
text, _ = call("fs_grep", path=posix(big_cnt), pattern="hit", output="count")
check("count 默认给完整数字", "big-count.txt:150" in text and "本页可见" not in text, text)
text, _ = call("fs_grep", path=posix(big_cnt), pattern="hit", output="count", limit=100)
check("count 显式 limit 仍标注", "本页可见" in text, text)

# fs_glob 预算截断时要说明"已给多少 / 共多少"，否则无法判断要不要继续
trunc_dir = os.path.join(root, "glob-trunc")
os.makedirs(trunc_dir, exist_ok=True)
for i in range(120):
    open(os.path.join(trunc_dir, f"trunc-file-{i:03d}.txt"), "w").write("x")
text, _ = call("fs_glob", path=posix(trunc_dir), pattern="*.txt", limit=500, budget=900)
check("glob 截断说明已给条数与总数",
      "输出截断" in text and "已给" in text and "120" in text, text[-120:])

# CRLF 文件：new 里带 \r\n 不应产生 \r\r\n
crlf2 = os.path.join(root, "crlf-edit.txt")
open(crlf2, "wb").write(b"a\r\nb\r\n")
text, err = call("fs_edit", path=posix(crlf2), edits=[{"old": "b", "new": "B\r\nC"}])
raw = open(crlf2, "rb").read()
check("new 带 CRLF 不产生 \\r\\r\\n", b"\r\r\n" not in raw and b"B\r\nC" in raw, repr(raw))

# 多文件编辑遇到混合换行时，其他文件不能被改（全量预检语义）
atom_crlf = os.path.join(root, "atom-crlf.txt")
atom_mix = os.path.join(root, "atom-mix.txt")
open(atom_crlf, "wb").write(b"x\r\ny\r\n")
open(atom_mix, "wb").write(b"a\r\nb\nc\r\n")
before_crlf = open(atom_crlf, "rb").read()
text, err = call("fs_edit", files=[
    {"path": posix(atom_crlf), "edits": [{"old": "x", "new": "X"}]},
    {"path": posix(atom_mix), "edits": [{"old": "b", "new": "B"}]},
])
check("混合换行导致整体失败时其他文件未被改动",
      err is True and "E_MIXED_EOL" in text
      and open(atom_crlf, "rb").read() == before_crlf, text)

# count 模式受 limit 截断时必须标注"本页可见"，否则会被读成文件总命中数
cnt_path = os.path.join(root, "count-many.txt")
open(cnt_path, "w", encoding="utf-8").write("needle\n" * 50)
text, _ = call("fs_grep", path=posix(cnt_path), pattern="needle", output="count", limit=10)
check("count 截断时标注本页可见",
      "本页可见" in text and "next_offset" in text, text)
text, _ = call("fs_grep", path=posix(cnt_path), pattern="needle", output="count")
check("count 未截断时保持简洁",
      "count-many.txt:50" in text and "本页可见" not in text, text)

# fs_write 到目录要报 E_IS_DIR，不能泄漏底层 rename 错误
tgt_dir = os.path.join(root, "write-target-dir")
os.makedirs(tgt_dir, exist_ok=True)
text, err = call("fs_write", path=posix(tgt_dir), content="x\n")
check("fs_write 到目录报 E_IS_DIR",
      text.startswith("E_IS_DIR") and "是目录" in text and err is True
      and "E_RENAME" not in text, text)

# 符号链接 stat 要报出目标与目标大小（链接自身是 0B）
sreal = os.path.join(root, "sl-real.txt")
slink = os.path.join(root, "sl-link.txt")
open(sreal, "w", encoding="utf-8").write("content\n")
subprocess.run(["powershell", "-NoProfile", "-Command",
                f"$null = New-Item -ItemType SymbolicLink -Path '{slink}' -Target '{sreal}' -ErrorAction Stop"],
               capture_output=True, timeout=30)
text, err = call("fs_stat", path=posix(slink), detail="full")
after = text.split("->")[-1] if "->" in text else ""
check("符号链接 stat 报出目标与大小",
      " link " in text and "->" in text and "B" in after and "//?/" not in text, text)

# 零时间预算必须立刻返回，不白扫 256 项
zb_dir = os.path.join(root, "zero-budget")
os.makedirs(zb_dir, exist_ok=True)
for i in range(300):
    open(os.path.join(zb_dir, f"z{i}.txt"), "w", encoding="utf-8").write("x\n")
text, err = call("fs_glob", path=posix(zb_dir), pattern="**/*.zzz", time_budget_ms=0)
check("零时间预算立刻返回",
      text.startswith("E_TIMEOUT") and "已访问 256 项" not in text and err is True, text)

# 同一行多处修改：替换次数照实报，行数变化只算一次
same_path = os.path.join(root, "same-line.txt")
open(same_path, "w", encoding="utf-8").write("fn foo(a: i32) -> i32 { a }\n")
text, _ = call("fs_edit", path=posix(same_path), edits=[
    {"old": "foo", "new": "bar"},
    {"old": "i32", "new": "u64", "all": True},
])
check("同一行 3 处修改报 +1 -1",
      "3 处修改" in text and "(+1 -1)" in text, text)

# glob 模式：花括号内的逗号不是分隔符；竖线要报错而不是静默无匹配
glob_dir = os.path.join(root, "globprobe")
os.makedirs(glob_dir, exist_ok=True)
for n in ["a.rs", "b.rs", "c.ts"]:
    open(os.path.join(glob_dir, n), "w", encoding="utf-8").write("x\n")
text, err = call("fs_glob", path=posix(glob_dir), pattern="{a,b}.rs")
check("花括号分组不被逗号切碎",
      err is False and "a.rs" in text and "b.rs" in text and "c.ts" not in text, text)
text, err = call("fs_glob", path=posix(glob_dir), pattern="*.rs|*.ts")
check("竖线写法报可执行错误",
      text.startswith("E_BAD_GLOB") and "不支持 `|`" in text and err is True, text)
text, err = call("fs_glob", path=posix(glob_dir), pattern="*.rs,*.ts")
check("逗号分隔多模式仍可用",
      err is False and "a.rs" in text and "c.ts" in text, text)

# 能匹配空位置的正则会命中每一行，必须拒绝（否则返回 100 行零信息量结果）
for empty_pat in ["^", "$", "a*", ".*", r"\b"]:
    text, err = call("fs_grep", path=posix(root), pattern=empty_pat)
    check(f"空匹配模式 {empty_pat} 被拒绝",
          text.startswith("E_BAD_REGEX") and "空位置" in text and err is True, text)
text, err = call("fs_grep", path=posix(root), pattern="^line1")
check("合法锚点模式仍可用", err is False and "line1" in text, text[:120])

# 错误消息不能被极小 budget 截成不可解析的碎片
text, err = call("fs_read", budget=10)
check("极小 budget 下错误仍可解析",
      text.startswith("E_BAD_ARGS") and len(text) > 20 and err is True, text)
text, err = call("fs_read", path=posix(os.path.join(root, "nope.txt")), budget=10)
check("读取失败错误带完整路径与原因",
      text.startswith("E_READ") and posix(os.path.join(root, "nope.txt")) in text
      and "读取失败" in text and err is True, text)

# 超上限与预算不足都不能静默降级
many_files = []
for i in range(25):
    fp = os.path.join(root, f"many{i}.txt")
    open(fp, "w", encoding="utf-8").write(f"c{i}\n")
    many_files.append(posix(fp))
text, err = call("fs_read", paths=many_files)
check("读 25 个文件时显式拒绝",
      text.startswith("E_BAD_ARGS") and "20 个文件" in text and err is True, text)
text, err = call("fs_grep", path=posix(root), pattern="c1", context=9)
check("context 超上限显式拒绝",
      text.startswith("E_BAD_ARGS") and "最大 5" in text and err is True, text)
text, err = call("fs_read", paths=many_files[:20], budget=400)
check("多文件预算不足只给一条提示",
      text.startswith("E_BUDGET") and "E_LINE_TOO_LONG" not in text and err is True, text)

# 长行命中在中间时必须围绕命中开窗，否则模型看到的全是填充字符
long_line = os.path.join(root, "bundle.min.js")
open(long_line, "w", encoding="utf-8").write("var a=" + "Q" * 3000 + ";FINDME;" + "R" * 3000 + ";\n")
text, _ = call("fs_grep", path=posix(root), pattern="FINDME", output="snippets")
check("长行命中可见（围绕命中开窗）",
      "FINDME" in text and text.count("Q") < 200, text[:200])
text, _ = call("fs_grep", path=posix(root), pattern="FINDME", output="matches")
check("matches 模式同样可见命中", "FINDME" in text, text[:200])

# 参数类型错误必须明确报错，不能静默退回默认值
text, err = call("fs_read", path=posix(one_path), budget="big")
check("budget 类型错误报 E_BAD_ARGS",
      text.startswith("E_BAD_ARGS") and "需要数字" in text and err is True, text)
text, err = call("fs_read", path=42)
check("path 类型错误报 E_BAD_ARGS",
      text.startswith("E_BAD_ARGS") and "需要字符串" in text and err is True, text)
text, err = call("fs_edit", path=posix(one_path), edits=[{"old": "NEEDLE here"}])
check("edits 漏 new 时拒绝而不是删除",
      text.startswith("E_BAD_ARGS") and "old 与 new" in text and err is True, text)
check("漏 new 时文件未改动",
      open(one_path, encoding="utf-8").read() == "NEEDLE here\n", "文件被改动")

# 只读文件：明确报 E_READONLY 而不是部分写入，且字节不变
ro_path = os.path.join(root, "readonly.txt")
ro_original = "line one\nline two\n"
open(ro_path, "w", encoding="utf-8").write(ro_original)
subprocess.run(["powershell", "-NoProfile", "-Command",
                f"Set-ItemProperty -Path '{ro_path}' -Name IsReadOnly -Value $true"],
               capture_output=True, timeout=30)
text, err = call("fs_edit", path=posix(ro_path), edits=[{"old": "line two", "new": "changed"}])
check("只读文件编辑报 E_READONLY",
      text.startswith("E_READONLY") and err is True, text)
check("只读文件未被改动",
      open(ro_path, encoding="utf-8").read() == ro_original, "内容被改写")
subprocess.run(["powershell", "-NoProfile", "-Command",
                f"Set-ItemProperty -Path '{ro_path}' -Name IsReadOnly -Value $false"],
               capture_output=True, timeout=30)

# 大文件被 grep 跳过时必须回报，不能静默假阴性
big_path = os.path.join(root, "big-skip.log")
open(big_path, "wb").write(("x" * 80 + "\n").encode() * 120000)  # ~9.6MB
skip_dir = os.path.join(root, "skipdir")
os.makedirs(skip_dir)
open(os.path.join(skip_dir, "small.txt"), "w", encoding="utf-8").write("NEEDLE_SMALL\n")
os.replace(big_path, os.path.join(skip_dir, "big-skip.log"))
text, _ = call("fs_grep", pattern="NEEDLE", path=posix(skip_dir))
check("fs_grep 回报跳过的大文件",
      "NEEDLE_SMALL" in text and "跳过 1 个" in text, text)
hook_pdf = json.dumps({"tool_name": "Read", "tool_input": {"file_path": "/repo/a.pdf"}})
p = subprocess.run([str(EXE), "hook", "claude"], input=hook_pdf.encode("utf-8"),
                   capture_output=True, timeout=30)
hook_decision = json.loads(p.stdout.decode("utf-8"))
check("hook 要求 PDF 走 fs_read",
      hook_decision["hookSpecificOutput"]["permissionDecision"] == "deny",
      p.stdout.decode("utf-8"))
docx_write = os.path.join(root, "written.docx")
text, _ = call("fs_write", path=posix(docx_write), content="写入第一段\n写入 & <第二段>\n")
check("fs_write 生成 DOCX", text.startswith("ok 写入") and "docx" in text, text)
text, _ = call("fs_read", path=posix(docx_write))
check("fs_write 后 DOCX 可读回",
      "1|写入第一段" in text and "2|写入 & <第二段>" in text, text)
text, _ = call("fs_edit", path=posix(docx_path), edits=[{"old": "DOCX 第一段", "new": "改"}])
check("fs_edit 拒绝 DOCX 精确替换",
      text.startswith("E_DOCX") and "fs_write" in text, text)

# 18. Claude Code 硬接管与 hook 决策
harden_dir = os.path.join(root, "harden")
os.makedirs(harden_dir)
code, out, err = cli("harden", "claude", "--path", posix(harden_dir))
hard_settings = os.path.join(harden_dir, ".claude", "settings.json")
check("harden 安装 Claude Code hook",
      code == 0 and os.path.exists(hard_settings) and "已安装" in out, out + err)
code, out, err = cli("harden", "claude", "--check", "--path", posix(harden_dir))
check("harden --check 通过", code == 0, out + err)
code, out, err = cli("doctor", "--json", "--path", posix(harden_dir))
doctor = json.loads(out)
check("doctor 报告 Claude Code 硬接管",
      doctor["claude_hook"] is True and doctor["enforcement"] == "hard-claude", out)
hook_input = json.dumps({"tool_name": "Read", "tool_input": {"file_path": "/repo/a.txt"}})
p = subprocess.run([str(EXE), "hook", "claude"], input=hook_input.encode("utf-8"),
                   capture_output=True, timeout=30)
decision = json.loads(p.stdout.decode("utf-8"))
check("hook 拒绝文本 Read",
      p.returncode == 0
      and decision["hookSpecificOutput"]["permissionDecision"] == "deny", p.stdout.decode("utf-8"))
hook_image = json.dumps({"tool_name": "Read", "tool_input": {"file_path": "/repo/a.png"}})
p = subprocess.run([str(EXE), "hook", "claude"], input=hook_image.encode("utf-8"),
                   capture_output=True, timeout=30)
check("hook 放行图片 Read", p.returncode == 0 and not p.stdout.strip(), p.stdout.decode("utf-8"))
code, out, err = cli("harden", "claude", "--remove", "--path", posix(harden_dir))
check("harden 移除后不再接管",
      code == 0 and "hook claude" not in open(hard_settings, encoding="utf-8").read(),
      out + err)

passed = sum(1 for _, c, _ in results if c)
print(f"\n{passed}/{len(results)} 通过   目录={root}")
# 用完即清：临时夹具不留在磁盘上
if passed == len(results):
    shutil.rmtree(root, ignore_errors=True)
sys.exit(0 if passed == len(results) else 1)
