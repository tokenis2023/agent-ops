<#
.SYNOPSIS
  把 v0.8.0 的 fsops.exe 换到 %LOCALAPPDATA%\fsops（DSH 插件默认使用的位置）。

.DESCRIPTION
  为什么单独一步：%LOCALAPPDATA%\fsops\fsops.exe 被 DSH 的长驻 `fsops mcp` 会话
  占用，运行中替换会 EBUSY。必须在 DeepSeek Harness 完全退出后执行。

  版本必须与插件一致：插件 0.8.0 的 tool-specs 声明了 fs_write.files / fs_grep.expand，
  旧内核会把这些新参数拒绝成 E_BAD_ARGS。

  默认从 GitHub Release 下载并校验 SHA-256；也可以 -FromLocal 用仓库里的构建产物。

.PARAMETER FromLocal
  用 <仓库>/target/release/fsops.exe，而不是从 Release 下载。

.PARAMETER Version
  目标版本，默认 v0.8.0。
#>
[CmdletBinding()]
param(
    [string]$Version = 'v0.8.0',
    [switch]$FromLocal
)

$ErrorActionPreference = 'Stop'
$dst = Join-Path $env:LOCALAPPDATA 'fsops\fsops.exe'
$bak = "$dst.bak-before-rounds"
$tmp = Join-Path $env:TEMP "fsops-$Version.exe"

if (-not (Test-Path (Split-Path $dst))) { New-Item -ItemType Directory -Path (Split-Path $dst) -Force | Out-Null }

$procs = @(Get-Process fsops -ErrorAction SilentlyContinue)
if ($procs.Count -gt 0) {
    Write-Host "检测到 $($procs.Count) 个 fsops 进程仍占用二进制：" -ForegroundColor Yellow
    $procs | Select-Object Id, Path | Format-Table -AutoSize
    Write-Host "请先完全退出 DeepSeek Harness，再重跑本脚本。" -ForegroundColor Yellow
    exit 2
}

if ($FromLocal) {
    $repo = Split-Path $PSScriptRoot -Parent
    $src = Join-Path $repo 'target\release\fsops.exe'
    if (-not (Test-Path $src)) { throw "找不到本地构建产物: $src（先 cargo build --release）" }
    Copy-Item $src $tmp -Force
} else {
    $asset = "fsops-$Version-windows-x86_64.exe"
    $base = "https://github.com/tokenis2023/agent-ops/releases/download/$Version"
    Write-Host "下载 $asset ..."
    Invoke-WebRequest "$base/$asset" -OutFile $tmp -UseBasicParsing
    $expect = (Invoke-WebRequest "$base/$asset.sha256" -UseBasicParsing).Content.Trim().Split(' ')[0]
    $actual = (Get-FileHash $tmp -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expect) { throw "SHA-256 校验失败：期望 $expect，实际 $actual" }
    Write-Host "SHA-256 校验通过" -ForegroundColor Green
}

if (Test-Path $dst -and -not (Test-Path $bak)) {
    Copy-Item $dst $bak
    Write-Host "已备份原二进制 -> $bak"
}
Move-Item $tmp $dst -Force
Write-Host "已安装到 $dst" -ForegroundColor Green
& $dst version
Write-Host "完成，现在可以启动 DeepSeek Harness。" -ForegroundColor Green
