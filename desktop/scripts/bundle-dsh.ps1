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
# Fix: `pnpm deploy --prod` only bundles the CLI's own dependency closure, but
# the harness `web` profile dynamically imports a broad set of @deepseek-ai/*
# workspace packages by name at runtime (e.g. @deepseek-ai/dsh-client-ui-goal,
# @deepseek-ai/dsh-typert-loader, @deepseek-ai/dsh-client-ui-plan, ...). Those
# are NOT in the CLI's transitive deps -- in a full workspace `pnpm install`
# they resolve via hoisting, but in the deploy they go missing and the harness
# crashes with ERR_MODULE_NOT_FOUND. Promote EVERY @deepseek-ai/* workspace
# package to a dependency of the deployed root (@deepseek-ai/dsh = apps/cli) so
# `pnpm deploy` materializes the entire workspace, guaranteeing any
# profile-referenced package resolves. (This also covers the peer-only packages
# like @deepseek-ai/cordis-plugin-group.)
# ---------------------------------------------------------------------------
Write-Host "==> adding all @deepseek-ai workspace packages as apps/cli dependencies"
$rootPkgPath = Join-Path $hDir "apps/cli/package.json"
$rootPkg = Get-Content $rootPkgPath -Raw | ConvertFrom-Json
if (-not $rootPkg.PSObject.Properties['dependencies']) {
    $rootPkg | Add-Member -NotePropertyName dependencies -NotePropertyValue ([PSCustomObject]@{})
}
$wsPkgs = @{}
$scanDirs = @()
foreach ($d in @('apps', 'packages', 'vendor')) {
    $p = Join-Path $hDir $d
    if (Test-Path $p) { $scanDirs += $p }
}
Get-ChildItem -Path $scanDirs -Recurse -Filter package.json -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -notmatch '[\\/]node_modules[\\/]' } | ForEach-Object {
        try { $sp = Get-Content $_.FullName -Raw | ConvertFrom-Json } catch { return }
        if ($sp.PSObject.Properties['name'] -and $sp.name -like '@deepseek-ai/*') {
            $wsPkgs[$sp.name] = 'workspace:*'
        }
    }
foreach ($k in $wsPkgs.Keys) {
    if (-not $rootPkg.dependencies.PSObject.Properties[$k]) {
        $rootPkg.dependencies | Add-Member -NotePropertyName $k -NotePropertyValue $wsPkgs[$k] -Force
        Write-Host "   + $k"
    }
}

# Ensure native modules (node-pty / koffi) are materialized in the deploy.
# They are transitive PROD dependencies of the CLI (node-pty via
# @deepseek-ai/dsh-subprocess-local -> used by terminal/bash tools; koffi via
# @deepseek-ai/dsh-fs-local / dsh-sandbox-windows-acl / dsh-session-persistence-jsonl
# -> used by FFI features). `pnpm deploy --legacy` only materializes the DIRECT
# deps of the filtered package, so these third-party transitive natives get
# dropped from dsh-dist and the sidecar crashes when a terminal tool spawns a
# pty. Promoting them to DIRECT deps of apps/cli forces pnpm deploy to include
# them (mirrors the @deepseek-ai/* promotion just above). Versions are read
# from the harness packages that declare them so pnpm dedupes to the same
# instance already in the lockfile.
$nativeSpecs = @(
    @{ pkg = 'node-pty'; src = (Join-Path $hDir 'packages/subprocess/subprocess-local/package.json') },
    @{ pkg = 'koffi';    src = (Join-Path $hDir 'packages/fs/fs-local/package.json') }
)
foreach ($n in $nativeSpecs) {
    $ver = $null
    if (Test-Path $n.src) {
        try { $sp = Get-Content $n.src -Raw | ConvertFrom-Json; $ver = $sp.dependencies.$($n.pkg) } catch {}
    }
    if (-not $ver) { $ver = 'latest' }
    if (-not $rootPkg.dependencies.PSObject.Properties[$n.pkg]) {
        $rootPkg.dependencies | Add-Member -NotePropertyName $n.pkg -NotePropertyValue $ver -Force
        Write-Host "   + $($n.pkg) (native module, forced into deploy) version=$ver"
    }
}
$rootPkg | ConvertTo-Json -Depth 50 | Set-Content -Encoding UTF8 $rootPkgPath

Write-Host "==> pnpm install --no-frozen-lockfile (relink promoted workspace packages)"
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

# Verify native modules actually landed in the deploy. `pnpm deploy --legacy`
# has been observed to silently drop third-party transitive natives; if they're
# missing here, fail fast with a clear message instead of letting the smoke
# gate discover a MODULE_NOT_FOUND deep in the deployed dsh-dist later.
foreach ($m in @('node-pty', 'koffi')) {
    $mp = Join-Path $outAbs "node_modules/$m"
    if (-not (Test-Path $mp)) {
        Write-Error "$m missing from dsh-dist after pnpm deploy -- this breaks native-module features (terminal/FFI). Check bundle-dsh native-module promotion."
        exit 1
    }
    Write-Host "   verified native module present: $m"
}

$entry = Join-Path $outAbs "lib/bin.js"
if (-not (Test-Path $entry)) { Write-Error "deploy produced no entry: $entry"; exit 1 }
Write-Host "OK dsh-dist ready: $entry"
