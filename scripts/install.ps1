<#
.SYNOPSIS
安装 fsops 并注册已检测到的 MCP 宿主。

.DESCRIPTION
从 GitHub Release 下载 Windows x64 二进制，校验 SHA-256，安装到用户目录并加入用户 PATH。
默认自动注册检测到的 Claude Code、Hermes 和 Codex。

.EXAMPLE
irm https://github.com/tokenis2023/agent-ops/releases/latest/download/install.ps1 | iex

.EXAMPLE
.\install.ps1 -Version v0.2.2 -Client none -InstallDir "$env:LOCALAPPDATA\fsops"

.EXAMPLE
.\install.ps1 -DryRun
#>
[CmdletBinding()]
param(
    [string]$Version = "latest",
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA "fsops"),
    [ValidateSet("auto", "none", "claude", "hermes", "codex")]
    [string]$Client = "auto",
    [switch]$NoPath,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
try {
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [Console]::OutputEncoding = $utf8
    $OutputEncoding = $utf8
} catch {
    # 非交互控制台忽略。
}
$Repo = "tokenis2023/agent-ops"
$ApiBase = "https://api.github.com/repos/$Repo"
$Headers = @{
    "User-Agent" = "fsops-installer"
    "Accept"     = "application/vnd.github+json"
}

try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
} catch {
    # 旧 PowerShell 已默认启用时忽略。
}

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "fsops 当前只提供 Windows x64 版本。"
}

function Resolve-Release {
    param([string]$Requested)
    if ($Requested -eq "latest" -or [string]::IsNullOrWhiteSpace($Requested)) {
        return Invoke-RestMethod -Uri "$ApiBase/releases/latest" -Headers $Headers
    }
    $tag = if ($Requested.StartsWith("v")) { $Requested } else { "v$Requested" }
    return Invoke-RestMethod -Uri "$ApiBase/releases/tags/$tag" -Headers $Headers
}

function Add-UserPath {
    param([string]$Directory)
    $current = [Environment]::GetEnvironmentVariable("Path", "User")
    if ([string]::IsNullOrWhiteSpace($current)) {
        [Environment]::SetEnvironmentVariable("Path", $Directory, "User")
    } elseif (($current -split ";") -notcontains $Directory) {
        [Environment]::SetEnvironmentVariable("Path", "$current;$Directory", "User")
    }
    if (($env:Path -split ";") -notcontains $Directory) {
        $env:Path = "$Directory;$env:Path"
    }
}

function Register-McpClient {
    param(
        [string]$Name,
        [string]$Executable
    )
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        Write-Host "  跳过 $Name：未检测到命令" -ForegroundColor DarkGray
        return
    }
    try {
        $listed = (& $Name mcp list 2>$null | Out-String)
        if ($listed -match '(?m)^\s*fsops(\s|$)') {
            Write-Host "  $Name 已存在 fsops 配置，跳过" -ForegroundColor DarkGray
            return
        }
        switch ($Name) {
            "claude" {
                & claude mcp add fsops -- $Executable mcp
            }
            "hermes" {
                "Y`n" | & hermes mcp add fsops --command $Executable --args mcp
            }
            "codex" {
                & codex mcp add fsops -- $Executable mcp
            }
        }
        if ($LASTEXITCODE -eq 0) {
            Write-Host "  已注册到 $Name" -ForegroundColor Green
        } else {
            Write-Warning "$Name 注册返回 $LASTEXITCODE；可能已存在，请用 $Name mcp list 检查。"
        }
    } catch {
        Write-Warning "$Name 自动注册失败：$($_.Exception.Message)"
    }
}

$release = Resolve-Release $Version
$exeAsset = $release.assets | Where-Object { $_.name -like "fsops-v*-windows-x86_64.exe" } | Select-Object -First 1
$shaAsset = $release.assets | Where-Object { $_.name -like "fsops-v*-windows-x86_64.exe.sha256" } | Select-Object -First 1
if (-not $exeAsset -or -not $shaAsset) {
    throw "Release $($release.tag_name) 缺少 Windows x64 二进制或 SHA-256 文件。"
}

$InstallDir = [Environment]::ExpandEnvironmentVariables($InstallDir)
$destination = Join-Path $InstallDir "fsops.exe"
Write-Host "fsops $($release.tag_name)" -ForegroundColor Cyan
Write-Host "  来源: $($exeAsset.browser_download_url)"
Write-Host "  安装到: $destination"

if ($DryRun) {
    Write-Host "DryRun：已确认 Release 资产存在，不下载、不修改 PATH。" -ForegroundColor Yellow
    return
}

$temp = Join-Path ([IO.Path]::GetTempPath()) ("fsops-install-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $temp -Force | Out-Null
try {
    $downloadExe = Join-Path $temp $exeAsset.name
    $downloadSha = Join-Path $temp $shaAsset.name
    Invoke-WebRequest -Uri $exeAsset.browser_download_url -OutFile $downloadExe -UseBasicParsing
    Invoke-WebRequest -Uri $shaAsset.browser_download_url -OutFile $downloadSha -UseBasicParsing

    $expected = ((Get-Content $downloadSha -TotalCount 1) -split "\s+")[0].ToLowerInvariant()
    $actual = (Get-FileHash $downloadExe -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        throw "SHA-256 校验失败：期望 $expected，实际 $actual"
    }
    Write-Host "  SHA-256: $actual" -ForegroundColor DarkGray

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $staged = Join-Path $InstallDir ("fsops.exe.new-" + [Guid]::NewGuid().ToString("N"))
    Copy-Item $downloadExe $staged -Force
    try {
        Move-Item $staged $destination -Force
    } catch {
        Write-Warning "目标正在运行，停止 fsops 子进程后重试。MCP 宿主会在下次调用时重启。"
        Get-Process fsops -ErrorAction SilentlyContinue | Stop-Process -Force
        Start-Sleep -Milliseconds 400
        Move-Item $staged $destination -Force
    }
} finally {
    Remove-Item $temp -Recurse -Force -ErrorAction SilentlyContinue
}

if (-not $NoPath) {
    Add-UserPath $InstallDir
    Write-Host "  已加入用户 PATH（新终端生效）" -ForegroundColor Green
}

$installedVersion = (& $destination version) -join " "
if ($LASTEXITCODE -ne 0) {
    throw "安装后自检失败。"
}
Write-Host "  自检: $installedVersion" -ForegroundColor Green

$clients = @()
if ($Client -eq "auto") {
    $clients = @("claude", "hermes", "codex") | Where-Object { Get-Command $_ -ErrorAction SilentlyContinue }
} elseif ($Client -ne "none") {
    $clients = @($Client)
}
if ($clients.Count -gt 0) {
    foreach ($name in $clients) {
        Register-McpClient -Name $name -Executable $destination
    }
} else {
    Write-Host "未自动注册 MCP 宿主。可运行：" -ForegroundColor Yellow
    Write-Host "  claude mcp add fsops -- `"$destination`" mcp"
    Write-Host "  hermes mcp add fsops --command `"$destination`" --args mcp"
    Write-Host "  codex mcp add fsops -- `"$destination`" mcp"
}

Write-Host ""
Write-Host "安装完成。新终端执行：fsops version" -ForegroundColor Green
Write-Host "项目内建议执行：fsops integrate --rules" -ForegroundColor Green
Write-Host "接入诊断：fsops doctor" -ForegroundColor Green
Write-Host "使用指南：https://github.com/$Repo/blob/master/docs/使用指南.md"
