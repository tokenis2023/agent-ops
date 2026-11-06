"""创建 GitHub Release 并上传资产。

用法:
    python scripts/release.py v0.2.5              # 用 target/release/fsops.exe
    python scripts/release.py v0.2.5 --exe X.exe  # 指定构建产物（例如标签 worktree 里的）

前置：docs/releases/<tag>.md 已写好（含 SHA/大小占位行，本脚本会填真实值）。
凭据运行时通过 `git credential fill` 取得，不落盘、不打印。
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OWNER, REPO = "tokenis2023", "agent-ops"
API = f"https://api.github.com/repos/{OWNER}/{REPO}"
UPLOAD = f"https://uploads.github.com/repos/{OWNER}/{REPO}"


def token() -> str:
    p = subprocess.run(
        ["git", "credential", "fill"],
        input=b"protocol=https\nhost=github.com\n\n",
        capture_output=True,
        cwd=ROOT,
    )
    for line in p.stdout.decode().splitlines():
        if line.startswith("password="):
            return line.split("=", 1)[1]
    raise SystemExit("未能取得 GitHub 凭据；先 git push 一次让凭据管理器记住它")


def req(url, tok, method="GET", data=None, ctype="application/json"):
    r = urllib.request.Request(url, data=data, method=method)
    r.add_header("Authorization", f"Bearer {tok}")
    r.add_header("Accept", "application/vnd.github+json")
    r.add_header("User-Agent", "fsops-release")
    if data is not None:
        r.add_header("Content-Type", ctype)
    try:
        with urllib.request.urlopen(r, timeout=300) as x:
            body = x.read()
            return x.status, (json.loads(body) if body else {})
    except urllib.error.HTTPError as e:
        return e.code, {"error": e.read().decode("utf-8", "replace")}


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("tag", help="如 v0.2.5")
    ap.add_argument("--exe", default=str(ROOT / "target" / "release" / "fsops.exe"))
    args = ap.parse_args()
    tag = args.tag

    exe = Path(args.exe)
    if not exe.exists():
        raise SystemExit(f"找不到二进制: {exe}（先 cargo build --release）")
    notes = ROOT / "docs" / "releases" / f"{tag}.md"
    if not notes.exists():
        raise SystemExit(f"找不到 Release 正文: {notes}")

    data = exe.read_bytes()
    sha, size = hashlib.sha256(data).hexdigest(), len(data)
    print(f"tag={tag}  大小={size}   sha={sha}")

    asset = f"fsops-{tag}-windows-x86_64.exe"
    sha_path = exe.parent / (asset + ".sha256")
    sha_path.write_text(f"{sha}  {asset}\n", encoding="ascii", newline="\n")

    body = notes.read_text(encoding="utf-8")
    body = re.sub(r"```text\n（构建后填入）\n```", f"```text\n{sha}\n```", body)
    body = body.replace("文件大小：（构建后填入） 字节。", f"文件大小：{size:,} 字节。")

    tok = token()
    payload = json.dumps({
        "tag_name": tag, "name": f"fsops {tag}", "body": body,
        "draft": False, "prerelease": False,
    }).encode()
    st, resp = req(f"{API}/releases", tok, "POST", payload)
    if st == 422:
        code, exist = req(f"{API}/releases/tags/{tag}", tok)
        if code != 200:
            raise SystemExit(f"创建失败 {st}: {resp}")
        rid = exist["id"]
        print(f"Release 已存在，复用 id={rid}")
    elif st in (200, 201):
        rid = resp["id"]
        print(f"创建成功 id={rid}\n{resp['html_url']}")
    else:
        raise SystemExit(f"创建失败 {st}: {resp}")

    _, cur = req(f"{API}/releases/{rid}/assets", tok)
    have = {a["name"]: a["id"] for a in cur} if isinstance(cur, list) else {}
    files = [
        (asset, exe),
        (asset + ".sha256", sha_path),
        ("install.ps1", ROOT / "scripts" / "install.ps1"),
        ("uninstall.ps1", ROOT / "scripts" / "uninstall.ps1"),
    ]
    for name, path in files:
        if name in have:
            req(f"{API}/releases/assets/{have[name]}", tok, "DELETE")
        blob = path.read_bytes()
        st, r = req(f"{UPLOAD}/releases/{rid}/assets?name={name}", tok, "POST",
                    blob, "application/octet-stream")
        if st not in (200, 201):
            raise SystemExit(f"上传 {name} 失败 {st}: {r}")
        print(f"  上传 {name} ({len(blob)} 字节)")

    _, final = req(f"{API}/releases/tags/{tag}", tok)
    print(f"\n核对: {final.get('tag_name')} draft={final.get('draft')}")
    for a in final.get("assets", []):
        print(f"  {a['name']:<44}{a['size']:>10}")
    print(final.get("html_url"))
    print("\n下一步：把 docs/releases/%s.md 里的占位换成上面的 SHA/大小并提交。" % tag)


if __name__ == "__main__":
    main()
