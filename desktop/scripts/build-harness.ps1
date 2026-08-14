<#
.SYNOPSIS
  Build deepseek-harness (pnpm install + pnpm run build).
.DESCRIPTION
  Runs `pnpm install` and `pnpm run build` in the harness root (produces apps/cli/lib and apps/web/dist).

  Windows compatibility: if native/landlock-run exists (a Linux-only runtime feature that
  breaks Windows builds / dirties the tree), move it to $env:TEMP first, then move it back
  after the build. Never use Remove-Item on a version-controlled directory.

  After build, guard with `git -C <harness> diff --exit-code` to ensure no tracked files changed.
.PARAMETER HarnessDir
  Harness root directory (default ./deepseek-harness).
#>
param(
    [string]$HarnessDir = "./deepseek-harness"
)

$ErrorActionPreference = "Stop"

$harness = Resolve-Path $HarnessDir -ErrorAction SilentlyContinue
if (-not $harness) {
    Write-Error "Harness directory not found: $HarnessDir"
    exit 1
}
$harness = $harness.Path

$landlockSrc = Join-Path $harness "native/landlock-run"
$moved       = $false
$stashDir    = $null

function Restore-Landlock {
    if ($script:moved -and $script:stashDir -and (Test-Path $script:stashDir)) {
        Move-Item -Path $script:stashDir -Destination $landlockSrc -Force
        Write-Host "Restored native/landlock-run to: $landlockSrc"
        $script:moved = $false
    }
}

try {
    $isWindows = ($env:OS -eq 'Windows_NT') -or $IsWindows
    if ($isWindows -and (Test-Path $landlockSrc)) {
        $stashName = "landlock-run.stash." + [System.Guid]::NewGuid().ToString("N")
        $stashDir  = Join-Path $env:TEMP $stashName
        Move-Item -Path $landlockSrc -Destination $stashDir -Force
        Write-Host "Windows build: temporarily moved native/landlock-run to $stashDir"
        $moved = $true
    }

    Write-Host "==> pnpm install (in $harness)"
    Push-Location $harness
    try {
        & pnpm install
        if ($LASTEXITCODE -ne 0) { throw "pnpm install failed (exit $LASTEXITCODE)" }

        Write-Host "==> pnpm run build"
        & pnpm run build
        if ($LASTEXITCODE -ne 0) { throw "pnpm run build failed (exit $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }

    if (Test-Path (Join-Path $harness ".git")) {
        Write-Host "==> git diff --exit-code guard"
        & git -C $harness diff --exit-code
        if ($LASTEXITCODE -ne 0) {
            throw "Build modified tracked files (git diff non-empty). Check harness build scripts."
        }
        Write-Host "Build guard passed: no tracked files modified."
    } else {
        Write-Host "Warning: harness is not a git repo; skipping git diff guard."
    }
} catch {
    Write-Error "build-harness failed: $_"
    Restore-Landlock
    exit 1
} finally {
    Restore-Landlock
}
