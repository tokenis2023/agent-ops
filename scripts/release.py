"""fsops 发布工具链：版本一致性检查、升版本、发布正文回填与上传。

用法:
    python scripts/release.py check                # 校验版本一致性（CI / 门禁用）
    python scripts/release.py prepare v0.6.0       # 升版本 + 文档引用 + CHANGELOG/发布正文骨架
    python scripts/release.py fill v0.6.0          # 用构建产物回填正文的 SHA/大小
    python scripts/release.py v0.6.0               # 创建 GitHub Release 并上传资产
    python scripts/release.py v0.6.0 --exe X.exe   # 指定构建产物（例如标签 worktree 里的）

`check` 校验：Cargo.toml / Cargo.lock / package.json / CHANGELOG 首段 /
工具参考自检示例 / 使用指南示例 / docs/releases/v<版本>.md 全部指向同一版本。
凭据运行时通过 `git credential fill` 取得，不落盘、不打印。
"""
import argparse
import datetime
import hashlib
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

for stream in (sys.stdout, sys.stderr):
    if hasattr(stream, "reconfigure"):
        stream.reconfigure(encoding="utf-8")

ROOT = Path(__file__).resolve().parents[1]
OWNER, REPO = "tokenis2023", "agent-ops"
API = f"https://api.github.com/repos/{OWNER}/{REPO}"
UPLOAD = f"https://uploads.github.com/repos/{OWNER}/{REPO}"


def root_dir() -> Path:
    return Path(os.environ.get("FSOPS_RELEASE_ROOT") or ROOT).resolve()


def parse_tag(tag: str) -> str:
    text = tag.strip()
    if text.startswith("v"):
        text = text[1:]
    if not re.fullmatch(r"\d+\.\d+\.\d+", text):
        raise ValueError(f"版本格式应为 vX.Y.Z，当前 {tag!r}")
    return text


def version_key(version: str) -> tuple[int, int, int]:
    major, minor, patch = version.split(".")
    return int(major), int(minor), int(patch)


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def current_version(root: Path) -> str:
    text = read(root / "Cargo.toml")
    match = re.search(r'(?m)^version = "([^"]+)"', text)
    if not match:
        raise ValueError("Cargo.toml 缺少 [package] version")
    return match.group(1)


def check(root: Path) -> tuple[str, list[str]]:
    """返回 (当前版本, 问题列表)；问题为空表示一致。"""
    version = current_version(root)
    problems: list[str] = []

    lock = read(root / "Cargo.lock")
    match = re.search(r'\[\[package\]\]\nname = "fsops"\nversion = "([^"]+)"', lock)
    if not match:
        problems.append("Cargo.lock 缺少 fsops 包条目")
    elif match.group(1) != version:
        problems.append(f"Cargo.lock 的 fsops 版本 {match.group(1)} ≠ {version}")

    package = json.loads(read(root / "package.json"))
    if package.get("version") != version:
        problems.append(f"package.json 版本 {package.get('version')} ≠ {version}")

    changelog = read(root / "CHANGELOG.md")
    match = re.search(r"(?m)^## \[([0-9.]+)\]", changelog)
    if not match:
        problems.append("CHANGELOG.md 缺少 ## [x.y.z] 首段")
    elif match.group(1) != version:
        problems.append(f"CHANGELOG 首段版本 {match.group(1)} ≠ {version}")

    reference = read(root / "docs" / "工具参考.md")
    if f"fsops {version} tools=" not in reference:
        problems.append(f"docs/工具参考.md 的自检示例未指向 fsops {version} tools=")

    guide = read(root / "docs" / "使用指南.md")
    if f"-Version v{version}" not in guide:
        problems.append(f"docs/使用指南.md 的安装示例未指向 -Version v{version}")

    if not (root / "docs" / "releases" / f"v{version}.md").exists():
        problems.append(f"缺少发布正文 docs/releases/v{version}.md")

    return version, problems


def release_template(version: str) -> str:
    tag = f"v{version}"
    return f"""# fsops {tag}

面向 AI Agent 的 Windows 文件操作层，MCP + CLI 双形态。（一句话概述本版重点。）

## 一行安装

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "& ([scriptblock]::Create([Text.Encoding]::UTF8.GetString((New-Object Net.WebClient).DownloadData('https://github.com/tokenis2023/agent-ops/releases/latest/download/install.ps1')).TrimStart([char]0xFEFF)))"
```

卸载：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "& ([scriptblock]::Create([Text.Encoding]::UTF8.GetString((New-Object Net.WebClient).DownloadData('https://github.com/tokenis2023/agent-ops/releases/latest/download/uninstall.ps1')).TrimStart([char]0xFEFF)))"
```

## 本版改动

- （待补充）

## 手动下载

- `fsops-{tag}-windows-x86_64.exe`
- `fsops-{tag}-windows-x86_64.exe.sha256`
- `install.ps1`
- `uninstall.ps1`

文件大小：（构建后填入） 字节。

SHA-256：

```text
（构建后填入）
```
"""


def prepare(root: Path, tag: str, date: str | None = None) -> str:
    version = parse_tag(tag)
    current = current_version(root)
    if version_key(version) <= version_key(current):
        raise ValueError(f"新版本 {version} 必须大于当前 {current}")
    day = date or datetime.date.today().isoformat()

    cargo = read(root / "Cargo.toml")
    cargo = re.sub(
        r'(?m)^version = "[0-9.]+"', f'version = "{version}"', cargo, count=1
    )
    (root / "Cargo.toml").write_text(cargo, encoding="utf-8", newline="\n")

    package_path = root / "package.json"
    package = re.sub(
        r'("version": ")[0-9.]+(")', rf"\g<1>{version}\g<2>", read(package_path), count=1
    )
    package_path.write_text(package, encoding="utf-8", newline="\n")

    lock_path = root / "Cargo.lock"
    lock = re.sub(
        r'(\[\[package\]\]\nname = "fsops"\nversion = ")[^"]+(")',
        rf"\g<1>{version}\g<2>",
        read(lock_path),
    )
    lock_path.write_text(lock, encoding="utf-8", newline="\n")

    reference_path = root / "docs" / "工具参考.md"
    reference = re.sub(
        r"(fsops )\d+\.\d+\.\d+( tools=)", rf"\g<1>{version}\g<2>", read(reference_path)
    )
    reference_path.write_text(reference, encoding="utf-8", newline="\n")

    guide_path = root / "docs" / "使用指南.md"
    guide = re.sub(
        r"(-Version v)\d+\.\d+\.\d+", rf"\g<1>{version}", read(guide_path)
    )
    guide_path.write_text(guide, encoding="utf-8", newline="\n")

    changelog_path = root / "CHANGELOG.md"
    changelog = read(changelog_path)
    if f"## [{version}]" not in changelog:
        section = f"## [{version}] - {day}\n\n- （待补充：本批改动。）\n\n"
        anchor = changelog.find("\n## [")
        if anchor == -1:
            changelog = changelog.rstrip() + "\n\n" + section
        else:
            changelog = changelog[: anchor + 1] + section + changelog[anchor + 1 :]
        changelog_path.write_text(changelog, encoding="utf-8", newline="\n")

    notes = root / "docs" / "releases" / f"v{version}.md"
    if not notes.exists():
        notes.write_text(release_template(version), encoding="utf-8", newline="\n")

    checked, problems = check(root)
    if problems:
        raise ValueError("prepare 后仍不一致：\n  - " + "\n  - ".join(problems))
    return (
        f"ok prepare v{checked}（CHANGELOG 段落与发布正文骨架需人工补内容）\n"
        f"下一步: cargo build --release && python scripts/release.py fill {tag}"
    )


def substitute(body: str, sha: str, size: int) -> str:
    body = re.sub(r"```text\n（构建后填入）\n```", f"```text\n{sha}\n```", body)
    return body.replace("文件大小：（构建后填入） 字节。", f"文件大小：{size:,} 字节。")


def fill(root: Path, tag: str, exe: Path) -> str:
    version = parse_tag(tag)
    notes = root / "docs" / "releases" / f"v{version}.md"
    if not notes.exists():
        raise ValueError(f"找不到发布正文 {notes}（先运行 prepare）")
    if not exe.exists():
        raise ValueError(f"找不到二进制 {exe}（先 cargo build --release）")
    data = exe.read_bytes()
    sha, size = hashlib.sha256(data).hexdigest(), len(data)
    body = read(notes)
    updated = substitute(body, sha, size)
    if updated == body and sha not in body:
        raise ValueError("发布正文里没有可替换的占位「（构建后填入）」")
    notes.write_text(updated, encoding="utf-8", newline="\n")
    return f"ok fill v{version} 大小={size} sha={sha}"


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


def upload(root: Path, tag: str, exe: Path) -> None:
    if not exe.exists():
        raise SystemExit(f"找不到二进制: {exe}（先 cargo build --release）")
    notes = root / "docs" / "releases" / f"{tag}.md"
    if not notes.exists():
        raise SystemExit(f"找不到 Release 正文: {notes}")

    data = exe.read_bytes()
    sha, size = hashlib.sha256(data).hexdigest(), len(data)
    print(f"tag={tag}  大小={size}   sha={sha}")

    asset = f"fsops-{tag}-windows-x86_64.exe"
    sha_path = exe.parent / (asset + ".sha256")
    sha_path.write_text(f"{sha}  {asset}\n", encoding="ascii", newline="\n")

    body = substitute(read(notes), sha, size)
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
        ("install.ps1", root / "scripts" / "install.ps1"),
        ("uninstall.ps1", root / "scripts" / "uninstall.ps1"),
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
    if "（构建后填入）" in read(notes):
        print(f"\n提示：正文占位已在本页面上填好；本地 {notes.name} 仍是占位，可运行 fill 后提交。")


def main() -> None:
    argv = sys.argv[1:]
    root = root_dir()
    commands = {"check", "prepare", "fill"}
    if argv and argv[0] in commands:
        cmd, rest = argv[0], argv[1:]
        ap = argparse.ArgumentParser(prog=f"release.py {cmd}")
        if cmd == "prepare":
            ap.add_argument("tag", help="如 v0.6.0")
            ap.add_argument("--date", help="CHANGELOG 日期，默认今天")
        elif cmd == "fill":
            ap.add_argument("tag", help="如 v0.6.0")
            ap.add_argument(
                "--exe",
                default=str(root / "target" / "release" / "fsops.exe"),
                help="构建产物，默认 target/release/fsops.exe",
            )
        args = ap.parse_args(rest)
        try:
            if cmd == "check":
                version, problems = check(root)
                if problems:
                    print("E_RELEASE 版本不一致：")
                    for problem in problems:
                        print(f"  - {problem}")
                    sys.exit(1)
                print(
                    f"ok 版本一致 v{version}（Cargo.toml / Cargo.lock / package.json / "
                    "CHANGELOG / 工具参考 / 使用指南 / 发布正文）"
                )
            elif cmd == "prepare":
                print(prepare(root, args.tag, args.date))
            else:
                exe = Path(args.exe)
                if not exe.is_absolute():
                    exe = root / exe
                print(fill(root, args.tag, exe))
        except ValueError as e:
            print(f"E_RELEASE {e}")
            sys.exit(2)
        return

    ap = argparse.ArgumentParser()
    ap.add_argument("tag", help="如 v0.2.5")
    ap.add_argument("--exe", default=str(root / "target" / "release" / "fsops.exe"))
    args = ap.parse_args(argv)
    upload(root, args.tag, Path(args.exe))


if __name__ == "__main__":
    main()
