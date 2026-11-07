<#
.SYNOPSIS
  fsops 分级检查：按改动范围选择测试档位，避免每次编辑都跑全套。

.DESCRIPTION
  fast    格式 + clippy + 仅改动模块的单测（秒级，编辑过程中用）
  commit  fast + 完整 cargo test（提交前跑）
  full    commit + 构建 + 真实 MCP e2e + 任务级基准 + DSH 插件冒烟
          （push 前，或改动 io/text/edit/render 等 I/O 内核时跑）

.PARAMETER Tier
  fast / commit / full，默认 commit。

.PARAMETER Module
  强制指定模块（如 tools、summary），默认按 git 改动自动推断。

.EXAMPLE
  .\scripts\check.ps1 fast
  .\scripts\check.ps1 commit
  .\scripts\check.ps1 full
#>
[CmdletBinding()]
param(
    [ValidateSet('fast', 'commit', 'full')]
    [string]$Tier = 'commit',
    [string]$Module = ''
)

# 原生命令（cargo/python/git）会往 stderr 写进度，Stop 会把它当异常；这里统一用 Continue。
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root

function Invoke-Step {
    param([string]$Name, [string]$Command)
    $started = Get-Date
    # cmd /c 在 OS 层合并 stderr，PowerShell 只会拿到普通文本，不会产生 NativeCommandError。
    $output = cmd /c "$Command 2>&1"
    $code = $LASTEXITCODE
    $ms = [math]::Round(((Get-Date) - $started).TotalMilliseconds)
    if ($code -ne 0) {
        Write-Host ("FAIL  {0}  ({1} ms)" -f $Name, $ms) -ForegroundColor Red
        $output | ForEach-Object { Write-Host $_ }
        throw "$Name 失败"
    }
    Write-Host ("ok    {0}  ({1} ms)" -f $Name, $ms) -ForegroundColor Green
}

# 只测试受影响的模块：cargo 的编译+链接是大头，过滤用例只能省执行时间。
function Get-ChangedModules {
    $files = @()
    $files += @(cmd /c "git diff --name-only HEAD 2>nul")
    $files += @(cmd /c "git ls-files --others --exclude-standard 2>nul")
    $modules = @()
    foreach ($file in $files) {
        $path = "$file".Trim()
        if ($path -match '^src/(.+?)\.rs$') {
            $name = $Matches[1]
            # lib.rs / main.rs 改动影响整个 crate：返回 $null 表示“跑全量”，
            # 与“没有 src 改动”的空数组区分开。
            if ($name -eq 'lib' -or $name -eq 'main') { return $null }
            if ($modules -notcontains $name) { $modules += $name }
        }
    }
    return $modules
}

$env:PYTHONUTF8 = '1'
$env:PYTHONIOENCODING = 'utf-8'
$exitCode = 0
try {
    Write-Host "=== fsops check ($Tier) ===" -ForegroundColor Cyan

    Invoke-Step 'cargo fmt --check' 'cargo fmt --all -- --check'
    Invoke-Step 'cargo clippy' 'cargo clippy --all-targets -- -D warnings'

    if ($Tier -eq 'fast') {
        $targets = $null
        if ($Module) { $targets = @($Module) } else { $targets = Get-ChangedModules }
        if ($null -eq $targets) {
            Invoke-Step 'cargo test --lib（lib/main 改动，跑全量）' 'cargo test --lib'
        }
        elseif ($targets.Count -eq 0) {
            Write-Host 'ok    （无 src 改动，跳过单测）' -ForegroundColor Green
        }
        elseif ($targets.Count -gt 2) {
            Invoke-Step 'cargo test --lib（改动模块超过 2 个，跑全量）' 'cargo test --lib'
        }
        else {
            foreach ($target in $targets) {
                Invoke-Step "cargo test --lib $target" "cargo test --lib $target::"
            }
        }
    }
    else {
        Invoke-Step 'cargo test' 'cargo test'
    }

    if ($Tier -eq 'full') {
        Invoke-Step 'cargo build' 'cargo build'
        Invoke-Step '真实 MCP e2e' 'python tests/windows_semantics.py'
        Invoke-Step '任务级基准' 'python tests/task_bench.py --exe target/debug/fsops.exe'
        Invoke-Step '版本一致性' 'python scripts/release.py check'
        Invoke-Step '发布工具自检' 'python tests/release_tooling.py'
        if (Get-Command node -ErrorAction SilentlyContinue) {
            Invoke-Step 'DSH 工具定义不漂移' 'node scripts/gen_dsh_specs.mjs --check --exe target/debug/fsops.exe'
            Invoke-Step '文档工具清单不漂移' 'node scripts/gen_docs_tools.mjs --check --exe target/debug/fsops.exe'
            Invoke-Step 'DSH 插件冒烟' 'node tests/dsh_plugin_smoke.mjs --exe target/debug/fsops.exe'
        }
        else {
            Write-Host 'skip  DSH 插件冒烟（未找到 node）' -ForegroundColor Yellow
        }
    }

    Write-Host '=== 全部通过 ===' -ForegroundColor Green
}
catch {
    Write-Host $_ -ForegroundColor Red
    $exitCode = 1
}
finally {
    Pop-Location
}
exit $exitCode
