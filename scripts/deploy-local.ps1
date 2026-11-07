[CmdletBinding()]
param(
    [string]$Version = 'v0.9.0',
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
