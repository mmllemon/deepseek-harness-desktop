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
Write-Host "==> pruning non-runtime files from dsh-dist"
$pruneExt = '*.d.ts', '*.d.mts', '*.d.cts', '*.map', '*.tsbuildinfo', '*.flow'
Get-ChildItem -Path $outAbs -Recurse -File -Include $pruneExt -ErrorAction SilentlyContinue |
    Remove-Item -Force -ErrorAction SilentlyContinue
# Type-only trees (lib/types) are also never needed at runtime; drop them to
# remove the deepest path segments that trip up the NSIS File command.
Get-ChildItem -Path $outAbs -Recurse -Directory -Filter types -ErrorAction SilentlyContinue |
    Where-Object { ($_.FullName -replace '\\', '/') -match 'node_modules' } |
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
Write-Host "==> dsh-dist pruned"

$entry = Join-Path $outAbs "lib/bin.js"
if (-not (Test-Path $entry)) { Write-Error "deploy produced no entry: $entry"; exit 1 }
Write-Host "OK dsh-dist ready: $entry"
