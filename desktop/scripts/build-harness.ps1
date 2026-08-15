<#
.SYNOPSIS
  Build deepseek-harness (pnpm install + pnpm run build).
.DESCRIPTION
  Runs `pnpm install` and `pnpm run build` in the harness root (produces apps/cli/lib and apps/web/dist).

  NOTE: native/landlock-run is a normal tracked directory in upstream. Its TypeScript
  sources are referenced unconditionally by tsconfig.host.json (project references), so it
  MUST remain present on disk for `tsc -b` to resolve. Do NOT move or delete it under any
  platform -- only its runtime is Linux-only; the source tree is required on Windows too.

  After build, guard with `git -C <harness> diff --exit-code` to ensure no tracked files changed.
.PARAMETER HarnessDir
  Harness root directory (default ./deepseek-harness).
#>
param(
    [string]$HarnessDir = "./deepseek-harness"
)

$ErrorActionPreference = "Stop"

# Ensure pnpm (and any other tooling) runs non-interactively. In CI this is normally
# auto-detected, but being explicit prevents an interactive build-script approval
# prompt from blocking the step forever on a headless runner.
$env:CI = 'true'

$harness = Resolve-Path $HarnessDir -ErrorAction SilentlyContinue
if (-not $harness) {
    Write-Error "Harness directory not found: $HarnessDir"
    exit 1
}
$harness = $harness.Path

try {
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
    exit 1
}
