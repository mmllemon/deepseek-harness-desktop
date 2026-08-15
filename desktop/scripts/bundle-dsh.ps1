<#
.SYNOPSIS
  Tier 2 sidecar bundler: deploy the built deepseek-harness CLI into a self-contained dsh-dist folder.
.DESCRIPTION
  Uses `pnpm deploy` to materialize @deepseek-ai/dsh (and its full dependency closure, including
  all workspace packages and node_modules) into $OutDir as REAL files (no symlinks), so it can be
  shipped as a Tauri resource and run with `node <OutDir>/lib/bin.js web --port <port>`.

  Why Tier 2 instead of Node SEA (Tier 1):
  - deepseek-harness is an ESM pnpm monorepo; SEA cannot bundle its entire dependency graph.
  - SEA's V8 code-cache generation chokes on ESM entry points ("Cannot use import statement").
  - Native modules (node-pty / koffi) are problematic under SEA; with Tier 2 they load normally.
.PARAMETER HarnessDir
  Path to the cloned + built deepseek-harness checkout (default ./deepseek-harness).
.PARAMETER OutDir
  Target deployment directory (default ./dsh-dist).
#>
param(
    [string]$HarnessDir = "./deepseek-harness",
    [string]$OutDir = "./dsh-dist"
)

$ErrorActionPreference = "Stop"

$hDir = Resolve-Path $HarnessDir -ErrorAction SilentlyContinue
if (-not $hDir) { Write-Error "harness not found: $HarnessDir"; exit 1 }
$hDir = $hDir.Path

# clean target (pnpm deploy requires an empty/non-existent target outside the workspace)
if (Test-Path $OutDir) { Remove-Item -Recurse -Force $OutDir }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$outAbs = (Resolve-Path $OutDir).Path

# ---------------------------------------------------------------------------
# Fix: `pnpm deploy --prod` drops peerDependencies, but several harness packages
# (notably @deepseek-ai/dsh-app-boot) import @deepseek-ai/* workspace packages at
# runtime. Those are only declared as peers (host-provided by design), so they
# never enter the deployed closure and the sidecar crashes with
# ERR_MODULE_NOT_FOUND at smoke time. Promote every @deepseek-ai/* workspace peer
# dep found anywhere in the harness to a regular dependency of the deployed root
# (@deepseek-ai/dsh = apps/cli), then re-install so pnpm links them, so
# `pnpm deploy` materializes them together with their full transitive closure.
# ---------------------------------------------------------------------------
Write-Host "==> promoting @deepseek-ai workspace peer deps to apps/cli dependencies"
$rootPkgPath = Join-Path $hDir "apps/cli/package.json"
$rootPkg = Get-Content $rootPkgPath -Raw | ConvertFrom-Json
if (-not $rootPkg.PSObject.Properties['dependencies']) {
    $rootPkg | Add-Member -NotePropertyName dependencies -NotePropertyValue ([PSCustomObject]@{})
}
$peerSet = @{}
$scanDirs = @()
foreach ($d in @('apps', 'packages')) {
    $p = Join-Path $hDir $d
    if (Test-Path $p) { $scanDirs += $p }
}
Get-ChildItem -Path $scanDirs -Recurse -Filter package.json -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -notmatch '[\\/]node_modules[\\/]' } | ForEach-Object {
        try { $sp = Get-Content $_.FullName -Raw | ConvertFrom-Json } catch { return }
        if ($sp.PSObject.Properties['peerDependencies']) {
            $sp.peerDependencies.PSObject.Properties | ForEach-Object {
                if ($_.Name -like '@deepseek-ai/*' -and ($_.Value -eq 'workspace:^' -or $_.Value -like 'workspace:*')) {
                    $peerSet[$_.Name] = $_.Value
                }
            }
        }
    }
foreach ($k in $peerSet.Keys) {
    if (-not $rootPkg.dependencies.PSObject.Properties[$k]) {
        $rootPkg.dependencies | Add-Member -NotePropertyName $k -NotePropertyValue $peerSet[$k] -Force
        Write-Host "   + $k -> $($peerSet[$k])"
    }
}
$rootPkg | ConvertTo-Json -Depth 50 | Set-Content -Encoding UTF8 $rootPkgPath

Write-Host "==> pnpm install (relink promoted peers)"
Push-Location $hDir
try {
    # CI sets frozen-lockfile=true by default; we just edited apps/cli/package.json,
    # so allow pnpm to refresh pnpm-lock.yaml in the (throwaway) clone.
    pnpm install --no-frozen-lockfile
    if ($LASTEXITCODE -ne 0) { throw "pnpm install failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

Write-Host "==> pnpm deploy @deepseek-ai/dsh -> $outAbs"
Push-Location $hDir
try {
    # --legacy: pnpm v10+ only deploys from workspaces with inject-workspace-packages=true
    # by default; upstream deepseek-harness has no such config, so force legacy deploy.
    pnpm deploy --legacy --filter @deepseek-ai/dsh --prod $outAbs
    if ($LASTEXITCODE -ne 0) { throw "pnpm deploy failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

# Prune non-runtime artifacts from the deploy so the NSIS bundler (which
# recursively packs the dsh-dist resource) doesn't choke on the very deep
# type-declaration paths that pnpm's virtual store produces
# (e.g. .../node_modules/.pnpm/@deepseek-ai+dsh-client-run_.../.../lib/types/.../*.d.ts).
# .d.ts / .map / .tsbuildinfo / .flow are NEVER loaded by Node at runtime.
# NOTE: use native `cmd /c del /s` -- PowerShell's Get-ChildItem -Recurse pipeline
# over a materialized pnpm store (millions of files) is catastrophically slow and
# would hang the bundling step for 20+ minutes.
Write-Host "==> pruning non-runtime files from dsh-dist"
$pat = '*.d.ts *.d.mts *.d.cts *.map *.tsbuildinfo *.flow'
cmd /c "cd /d `"$outAbs`" && del /s /q $pat" 2>$null
Write-Host "==> dsh-dist pruned"

$entry = Join-Path $outAbs "lib/bin.js"
if (-not (Test-Path $entry)) { Write-Error "deploy produced no entry: $entry"; exit 1 }
Write-Host "OK dsh-dist ready: $entry"
