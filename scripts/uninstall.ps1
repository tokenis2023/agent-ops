<#
.SYNOPSIS
卸载 fsops 用户级安装。

.DESCRIPTION
停止 fsops 子进程，删除默认安装目录并移除用户 PATH。不会自动修改 MCP 宿主配置，
脚本会打印需要执行的移除命令。

.EXAMPLE
irm https://github.com/tokenis2023/agent-ops/releases/latest/download/uninstall.ps1 | iex

.EXAMPLE
.\uninstall.ps1 -DryRun
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA "fsops"),
    [switch]$DryRun,
    [switch]$Force
)

$ErrorActionPreference = "Stop"
try {
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [Console]::OutputEncoding = $utf8
    $OutputEncoding = $utf8
} catch {
    # 非交互控制台忽略。
}
$InstallDir = [Environment]::ExpandEnvironmentVariables($InstallDir)
$leaf = Split-Path $InstallDir -Leaf
if ($leaf -ne "fsops" -and -not $Force) {
    throw "拒绝删除非 fsops 目录：$InstallDir。自定义目录请显式加 -Force。"
}
$exe = Join-Path $InstallDir "fsops.exe"

Write-Host "fsops 卸载" -ForegroundColor Cyan
Write-Host "  安装目录: $InstallDir"
Write-Host "  可执行文件: $exe"

if ($DryRun) {
    Write-Host "DryRun：不停止进程、不删除文件、不修改 PATH。" -ForegroundColor Yellow
} else {
    Get-Process fsops -ErrorAction SilentlyContinue | Stop-Process -Force
    if (Test-Path $InstallDir) {
        Remove-Item $InstallDir -Recurse -Force
    }

    $current = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not [string]::IsNullOrWhiteSpace($current)) {
        $parts = ($current -split ";") | Where-Object { $_ -and $_ -ne $InstallDir }
        [Environment]::SetEnvironmentVariable("Path", ($parts -join ";"), "User")
    }
    Write-Host "已删除安装目录并清理用户 PATH。" -ForegroundColor Green
}

Write-Host ""
Write-Host "还需要在各宿主中移除 MCP 配置："
Write-Host "  claude mcp remove fsops"
Write-Host "  hermes mcp remove fsops"
Write-Host "  codex mcp remove fsops"
Write-Host "Cursor / Windsurf / Cline：从 mcpServers 中删除 fsops。"
