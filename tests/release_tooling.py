"""发布工具链自检：check / prepare / fill 在临时仓库夹具上的行为。

夹具不碰真实仓库：通过 FSOPS_RELEASE_ROOT 指向临时目录。
"""
import hashlib
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

for stream in (sys.stdout, sys.stderr):
    if hasattr(stream, "reconfigure"):
        stream.reconfigure(encoding="utf-8")

REPO = Path(__file__).resolve().parents[1]
SCRIPT = REPO / "scripts" / "release.py"
FAILED = []


def check(name: str, ok: bool, detail: str = "") -> None:
    print(f"{'PASS' if ok else 'FAIL'} {name}" + (f"  {detail}" if not ok else ""))
    if not ok:
        FAILED.append(name)


def write_tree(root: Path, version: str = "0.5.0") -> None:
    (root / "docs" / "releases").mkdir(parents=True, exist_ok=True)
    (root / "Cargo.toml").write_text(
        f'[package]\nname = "fsops"\nversion = "{version}"\nedition = "2021"\n',
        encoding="utf-8",
    )
    (root / "Cargo.lock").write_text(
        f'[[package]]\nname = "fsops"\nversion = "{version}"\ndependencies = []\n',
        encoding="utf-8",
    )
    (root / "package.json").write_text(
        json.dumps({"name": "@tokenis2023/fsops", "version": version}, indent=2) + "\n",
        encoding="utf-8",
    )
    (root / "CHANGELOG.md").write_text(
        "# 变更记录\n\n本项目使用语义化版本。\n\n"
        f"## [{version}] - 2026-11-07\n\n- 示例条目。\n",
        encoding="utf-8",
    )
    (root / "docs" / "工具参考.md").write_text(
        "# 工具参考\n\n"
        f"| `server` | 不读 | `fsops {version} tools=8`——一条调用自检 |\n",
        encoding="utf-8",
    )
    (root / "docs" / "使用指南.md").write_text(
        "# 使用指南\n\n"
        f"… -Version v{version} -InstallDir \"$env:LOCALAPPDATA\\fsops\"\n",
        encoding="utf-8",
    )
    (root / "docs" / "releases" / f"v{version}.md").write_text(
        f"# fsops v{version}\n\n示例发布说明。\n", encoding="utf-8"
    )


def run(root: Path, *args: str) -> subprocess.CompletedProcess:
    env = os.environ.copy()
    env["FSOPS_RELEASE_ROOT"] = str(root)
    env["PYTHONUTF8"] = "1"
    env["PYTHONIOENCODING"] = "utf-8"
    return subprocess.run(
        [sys.executable, str(SCRIPT), *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=env,
        cwd=str(REPO),
        timeout=60,
    )


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


with tempfile.TemporaryDirectory(prefix="fsops-release-test-") as tmp:
    root = Path(tmp)
    write_tree(root)

    r = run(root, "check")
    check("check 夹具一致时通过", r.returncode == 0 and "ok 版本一致 v0.5.0" in r.stdout, r.stdout + r.stderr)

    pkg = root / "package.json"
    pkg.write_text(
        json.dumps({"name": "@tokenis2023/fsops", "version": "0.4.9"}, indent=2) + "\n",
        encoding="utf-8",
    )
    r = run(root, "check")
    check(
        "check 能报出版本不一致的文件",
        r.returncode == 1 and "package.json" in r.stdout,
        r.stdout + r.stderr,
    )
    pkg.write_text(
        json.dumps({"name": "@tokenis2023/fsops", "version": "0.5.0"}, indent=2) + "\n",
        encoding="utf-8",
    )

    r = run(root, "prepare", "v0.6.0", "--date", "2026-12-01")
    check("prepare 成功", r.returncode == 0, r.stdout + r.stderr)
    check("prepare 更新 Cargo.toml", 'version = "0.6.0"' in read(root / "Cargo.toml"))
    check("prepare 更新 Cargo.lock", 'name = "fsops"\nversion = "0.6.0"' in read(root / "Cargo.lock"))
    check("prepare 更新 package.json", '"version": "0.6.0"' in read(pkg))
    changelog = read(root / "CHANGELOG.md")
    check("prepare 插入 CHANGELOG 新段", "## [0.6.0] - 2026-12-01" in changelog)
    check("prepare 保留 CHANGELOG 旧段", "## [0.5.0] - 2026-11-07" in changelog)
    check("prepare 更新工具参考自检版本", "fsops 0.6.0 tools=8" in read(root / "docs" / "工具参考.md"))
    check("prepare 更新使用指南示例版本", "-Version v0.6.0" in read(root / "docs" / "使用指南.md"))
    notes = root / "docs" / "releases" / "v0.6.0.md"
    check("prepare 生成发布正文骨架", notes.exists())
    if notes.exists():
        body = read(notes)
        check("骨架含资产名", "fsops-v0.6.0-windows-x86_64.exe" in body)
        check("骨架含大小与 SHA 占位", "（构建后填入）" in body and body.count("（构建后填入）") >= 2)

    r = run(root, "check")
    check("prepare 后 check 通过", r.returncode == 0 and "ok 版本一致 v0.6.0" in r.stdout, r.stdout + r.stderr)

    fake = root / "fake.exe"
    fake.write_bytes(b"fake-binary")
    r = run(root, "fill", "v0.6.0", "--exe", str(fake))
    check("fill 成功", r.returncode == 0, r.stdout + r.stderr)
    check("fill 后正文仍存在", notes.exists())
    if notes.exists():
        body = read(notes)
        sha = hashlib.sha256(b"fake-binary").hexdigest()
        check("fill 写入真实 SHA", sha in body)
        check("fill 写入真实大小", "11 字节" in body)
        check("fill 清空占位", "（构建后填入）" not in body)

    r = run(root, "prepare", "v0.5.0")
    check(
        "prepare 拒绝降级",
        r.returncode != 0 and "E_RELEASE" in r.stdout + r.stderr,
        r.stdout + r.stderr,
    )
    r = run(root, "prepare", "v0.6.0")
    check(
        "prepare 拒绝同版本",
        r.returncode != 0 and "E_RELEASE" in r.stdout + r.stderr,
        r.stdout + r.stderr,
    )

if FAILED:
    print(f"\n{len(FAILED)} 项失败")
    sys.exit(1)
print("\n全部通过")
