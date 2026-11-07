<#
.SYNOPSIS
  本地部署：把 target/release 的 fsops.exe 换到 %LOCALAPPDATA%\fsops。

.DESCRIPTION
  为什么单独一步：%LOCALAPPDATA%\fsops\fsops.exe 被 DSH 的长驻 `fsops mcp` 会话
  占用，运行中替换会 EBUSY。必须在 DeepSeek Harness 完全退出后执行。

  重要：插件的 tool-specs 与二进制必须同版本。只同步插件 JS 不换 exe 时，
  新参数（fs_write.files / fs_grep.expand）会被旧内核拒绝成 E_BAD_ARGS。

.PARAMETER Force
  允许在检测到 fsops 进程时仍然尝试（一般会失败，仅排障用）。
#>
[CmdletBinding()]
param([switch]$Force)

$ErrorActionPreference = 'Stop'
$src = Join-Path $PSScriptRoot '..\target\release\fsops.exe'
$dst = Join-Path $env:LOCALAPPDATA 'fsops\fsops.exe'
$bak = "$dst.bak-before-rounds"

if (-not (Test-Path $src)) { throw "找不到 release 二进制: $src（先 cargo build --release）" }
if (-not (Test-Path $dst)) { throw "找不到已安装的 fsops.exe: $dst" }
if (-not (Test-Path $bak)) {
    Copy-Item $dst $bak
    Write-Host "已备份原二进制 -> $bak"
}

$procs = @(Get-Process fsops -ErrorAction SilentlyContinue)
if ($procs.Count -gt 0 -and -not $Force) {
    Write-Host "检测到 $($procs.Count) 个 fsops 进程仍占用二进制：" -ForegroundColor Yellow
    $procs | Select-Object Id, Path | Format-Table -AutoSize
    Write-Host "请先完全退出 DeepSeek Harness，再重跑本脚本。" -ForegroundColor Yellow
    exit 2
}

Copy-Item $src $dst -Force
Write-Host "已替换二进制: $dst" -ForegroundColor Green
$version = & $dst version
Write-Host "当前版本: $version" -ForegroundColor Green
Write-Host "完成，现在可以启动 DeepSeek Harness。" -ForegroundColor Green
