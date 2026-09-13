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

  PATCHES: This script contains hotfix patches for upstream harness behavior.
  Each patch has a comment with rationale, upstream issue link (if any), and a verification step.
  See ../PATCHES.md for the full tracking table.
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
# STEP 1: Promote @deepseek-ai/* workspace packages to apps/cli dependencies
# ---------------------------------------------------------------------------
# Why: `pnpm deploy --prod` only bundles the CLI's own dependency closure, but
# the harness `web` profile dynamically imports a broad set of @deepseek-ai/*
# workspace packages by name at runtime (e.g. @deepseek-ai/dsh-client-ui-goal,
# @deepseek-ai/dsh-typert-loader, @deepseek-ai/dsh-client-ui-plan, ...). Those
# are NOT in the CLI's transitive deps -- in a full workspace `pnpm install`
# they resolve via hoisting, but in the deploy they go missing and the harness
# crashes with ERR_MODULE_NOT_FOUND.
#
# Fix: Promote EVERY @deepseek-ai/* workspace package to a dependency of
# apps/cli so `pnpm deploy` materializes the entire workspace.
# Verification: After deploy, every @deepseek-ai/* package should exist under
#   dsh-dist/node_modules/ (checked in post-deploy section).
# ---------------------------------------------------------------------------
function Add-WorkspacePackages {
    param([string]$HarnessDir, [string]$RootPkgPath)

    Write-Host "==> [PATCH-01] adding all @deepseek-ai workspace packages as apps/cli dependencies"
    $rootPkg = Get-Content $RootPkgPath -Raw | ConvertFrom-Json
    if (-not $rootPkg.PSObject.Properties['dependencies']) {
        $rootPkg | Add-Member -NotePropertyName dependencies -NotePropertyValue ([PSCustomObject]@{})
    }

    $wsPkgs = @{}
    $scanDirs = @()
    foreach ($d in @('apps', 'packages', 'vendor')) {
        $p = Join-Path $HarnessDir $d
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
    $rootPkg | ConvertTo-Json -Depth 50 | Set-Content -Encoding UTF8 $RootPkgPath
}

# ---------------------------------------------------------------------------
# STEP 2: Force native modules into apps/cli direct dependencies
# ---------------------------------------------------------------------------
# Why: `pnpm deploy --legacy` only materializes the DIRECT deps of the filtered
# package. Third-party transitive natives (node-pty, koffi) get silently dropped,
# causing the sidecar to crash with MODULE_NOT_FOUND when terminal/FFI features
# are used.
#
# node-pty: used by terminal/bash tools (via @deepseek-ai/dsh-subprocess-local)
# koffi:    used by FFI features (via @deepseek-ai/dsh-fs-local, etc.)
#
# Fix: Promote them to DIRECT deps of apps/cli, versions read from declaring
#   packages so pnpm dedupes to the same instance already in the lockfile.
# Verification: smoke.ps1 gate (b2) runs node-pty spawn + koffi load test.
# ---------------------------------------------------------------------------
function Add-NativeModules {
    param([string]$HarnessDir, $rootPkg)

    Write-Host "==> [PATCH-02] forcing native modules (node-pty, koffi) into apps/cli dependencies"
    $nativeSpecs = @(
        @{ pkg = 'node-pty'; src = (Join-Path $HarnessDir 'packages/subprocess/subprocess-local/package.json') },
        @{ pkg = 'koffi';    src = (Join-Path $HarnessDir 'packages/fs/fs-local/package.json') }
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
}

# ---------------------------------------------------------------------------
# STEP 3: Post-deploy repair — materialize dropped @deepseek-ai/* packages
# ---------------------------------------------------------------------------
# Why: vendor packages consumed via `file:` specs (e.g. @deepseek-ai/cordis ->
# file:vendor/cordis) declare workspace:^ sub-dependencies (@deepseek-ai/cosmokit,
# @deepseek-ai/schemastery, @deepseek-ai/cordis-plugin-*) that the deploy cannot
# resolve, so that whole closure never lands in dsh-dist.
#
# Fix: Copy BUILT package dirs (lib/ produced by `pnpm run build`) from the
#   harness checkout into dsh-dist/node_modules as real directories.
#   Note: NSIS junction dereference can leave empty-shell dirs; check package.json
#   presence, not just directory existence.
# ---------------------------------------------------------------------------
function Materialize-DroppedPackages {
    param([string]$HarnessDir, [string]$OutDir)

    Write-Host "==> [PATCH-03] materializing dropped @deepseek-ai packages from harness checkout"
    $aiPkgs = @{}
    $aiScan = @()
    foreach ($d in @('apps', 'packages', 'vendor')) {
        $p = Join-Path $HarnessDir $d
        if (Test-Path $p) { $aiScan += $p }
    }
    Get-ChildItem -Path $aiScan -Recurse -Filter package.json -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -notmatch '[\\/]node_modules[\\/]' } | ForEach-Object {
            try { $sp = Get-Content $_.FullName -Raw | ConvertFrom-Json } catch { return }
            if ($sp.PSObject.Properties['name'] -and $sp.name -like '@deepseek-ai/*' -and -not $aiPkgs.ContainsKey($sp.name)) {
                $aiPkgs[$sp.name] = $_.Directory.FullName
            }
        }

    $materialized = 0
    foreach ($k in $aiPkgs.Keys) {
        $dst = Join-Path $OutDir ("node_modules/" + $k)
        $real = (Test-Path (Join-Path $dst "package.json"))
        if (-not $real) {
            $srcDir = $aiPkgs[$k]
            $libOk = Test-Path (Join-Path $srcDir 'lib')
            if (-not $libOk) {
                Write-Error "package $k missing from dsh-dist AND has no built lib/ in checkout ($srcDir) -- harness build incomplete"
                exit 1
            }
            if (Test-Path $dst) {
                Remove-Item -Recurse -Force $dst
            }
            Copy-Item -Recurse -Force $srcDir $dst
            $materialized++
            Write-Host "   materialized: $k$(if (-not $libOk) { ' (WARNING: no lib/)' })"
        }
    }
    Write-Host "==> @deepseek-ai materialization done ($materialized copied)"
}

# ---------------------------------------------------------------------------
# STEP 4: Post-deploy patch — dsh-settings array-tolerance
# ---------------------------------------------------------------------------
# WHY: an upstream migration writes the `llm-pi-ai` settings namespace back
# to settings.yaml as a bare ARRAY (e.g. `- name: openai ...`). dsh-settings'
# section() calls isPlainObject() on it, fails, and THROWS
# `settings section "llm-pi-ai" must be an object of keys`. That aborts the
# namespace registration, so the frontend's protocolChoices() yields an empty
# list and the "添加提供方 / 添加自定义提供方" buttons stay greyed out.
#
# Fix: Replace the throw with `return {}` (tolerance: non-object section -> empty object).
# This makes the fix part of the shipped artifact so it survives upgrades.
#
# If upstream patches this, the regex match will fail and we error out.
# To verify: check that "添加提供方" buttons are functional after first launch.
# ---------------------------------------------------------------------------
function Patch-DshSettings {
    param([string]$OutDir)

    Write-Host "==> [PATCH-04] patching dsh-settings section() array-tolerance"
    $settingsIdx = @()
    $store = Join-Path $OutDir "node_modules/.pnpm"
    if (Test-Path $store) {
        $settingsIdx += Get-ChildItem -Path $store -Directory -Filter "@deepseek-ai+dsh-settings@*" -ErrorAction SilentlyContinue |
            ForEach-Object { Join-Path $_.FullName "node_modules/@deepseek-ai/dsh-settings/lib/index.js" } |
            Where-Object { Test-Path $_ }
    }
    $top = Join-Path $OutDir "node_modules/@deepseek-ai/dsh-settings/lib/index.js"
    if (Test-Path $top) { $settingsIdx += $top }
    if ($settingsIdx.Count -eq 0) {
        Write-Error "dsh-settings/lib/index.js not found in dsh-dist -- section() patch cannot be applied"
        exit 1
    }
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    $patchedCount = 0
    foreach ($idx in ($settingsIdx | Select-Object -Unique)) {
        $raw = [System.IO.File]::ReadAllText($idx)
        if ($raw -match 'throw new TypeError\(`settings section "\$\{ns\}" must be an object of keys`\)') {
            $patched = $raw -replace 'if \(!isPlainObject\(section\)\) throw new TypeError\(`settings section "\$\{ns\}" must be an object of keys`\);', 'if (!isPlainObject(section)) return {};'
            [System.IO.File]::WriteAllText($idx, $patched, $utf8NoBom)
            $patchedCount++
            Write-Host "   patched: $idx"
        } else {
            Write-Host "   already patched or shape changed: $idx"
        }
    }
    if ($patchedCount -eq 0) { Write-Error "no dsh-settings index.js was patched -- verify the pattern still matches upstream"; exit 1 }
    Write-Host "==> dsh-settings section() patched ($patchedCount file(s))"
}

# ---------------------------------------------------------------------------
# Execute build steps
# ---------------------------------------------------------------------------
$rootPkgPath = Join-Path $hDir "apps/cli/package.json"

# STEP 1: Promote workspace packages
Add-WorkspacePackages -HarnessDir $hDir -RootPkgPath $rootPkgPath

# STEP 2: Force native modules (need rootPkg before rereading)
$rootPkg = Get-Content $rootPkgPath -Raw | ConvertFrom-Json
Add-NativeModules -HarnessDir $hDir -rootPkg $rootPkg
$rootPkg | ConvertTo-Json -Depth 50 | Set-Content -Encoding UTF8 $rootPkgPath

# STEP 3: Install with updated deps
Write-Host "==> pnpm install --no-frozen-lockfile (relink promoted workspace packages)"
Push-Location $hDir
try {
    pnpm install --no-frozen-lockfile
    if ($LASTEXITCODE -ne 0) { throw "pnpm install failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

# STEP 3.5: Rebuild native modules at source so deploy carries built .node binaries.
# pnpm 10+ skips unapproved dependency build scripts (node-gyp / prebuilt download) by
# default, so without this node-pty/koffi land in dsh-dist as JS-only dirs and smoke(b2) fails.
Write-Host "==> pnpm rebuild node-pty koffi"
Push-Location $hDir
try {
    pnpm rebuild node-pty koffi
    if ($LASTEXITCODE -ne 0) { throw "pnpm rebuild native failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

# STEP 4: Deploy
Write-Host "==> pnpm deploy @deepseek-ai/dsh -> $outAbs"
Push-Location $hDir
try {
    pnpm deploy --legacy --filter @deepseek-ai/dsh --prod $outAbs
    if ($LASTEXITCODE -ne 0) { throw "pnpm deploy failed (exit $LASTEXITCODE)" }
} finally {
    Pop-Location
}

# STEP 5: Prune non-runtime artifacts
# .d.ts / .map / .tsbuildinfo / .flow are NEVER loaded by Node at runtime.
# Use native cmd /c del — PowerShell's pipeline over pnpm virtual store hangs for 20+ min.
Write-Host "==> pruning non-runtime files from dsh-dist"
$pat = '*.d.ts *.d.mts *.d.cts *.map *.tsbuildinfo *.flow'
cmd /c "cd /d `"$outAbs`" && del /s /q $pat" 2>$null
Write-Host "==> dsh-dist pruned"

# STEP 6: Post-deploy repairs (PATCH-03, PATCH-04)
Materialize-DroppedPackages -HarnessDir $hDir -OutDir $outAbs
Patch-DshSettings -OutDir $outAbs

# STEP 6.5: Sync source-side native build artifacts into dsh-dist.
#
# Why BOTH the .node binary and the virtual-store lookup are required:
#   - `pnpm rebuild` writes its output into the REAL package dir that lives inside the
#     pnpm virtual store (.pnpm/<pkg>@<ver>/node_modules/<pkg>), NOT the hoisted top-level
#     node_modules/<pkg> symlink. We must resolve the real dir, otherwise the sync is skipped.
#   - `pnpm deploy --legacy` copies packages from store metadata, so the post-install outputs
#     (koffi build/Release/koffi.node, node-pty build/Release/pty.node + conpty.dll, ...) are
#     silently dropped from the deployed tree. Without re-copying them, smoke gate (b2) cannot
#     load the FFI/pty natives.
#   - A rebuild of the native addon against the shipped Node ABI is also required so the binary
#     matches `node.exe`'s ABI (see STEP 3.5).
#
# Fix: resolve each package's real directory (top-level OR .pnpm virtual store) in BOTH the
#   harness source and the deployed dsh-dist, then mirror the source package (build artifacts
#   included) over the deployed one. Verification: smoke gate (b2) loads koffi + spawns node-pty.
function Resolve-RealPackage {
    param([string]$Base, [string]$Name)
    # 1) top-level hoisted node_modules/<name> (a symlink in a pnpm workspace)
    $top = Join-Path $Base "node_modules/$Name"
    if (Test-Path -LiteralPath $top) { return (Resolve-Path -LiteralPath $top).Path }
    # 2) virtual store: .pnpm/<name>@<version>/node_modules/<name>  (scoped names use '+' => '<scope>+<name>@...')
    $store = Join-Path $Base "node_modules/.pnpm"
    if (Test-Path -LiteralPath $store) {
        $dirs = Get-ChildItem -LiteralPath $store -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -like "*+$Name@*" -or $_.Name -like "$Name@*" }
        foreach ($d in ($dirs | Sort-Object Name -Descending)) {
            $cand = Join-Path $d.FullName "node_modules/$Name"
            if (Test-Path -LiteralPath $cand) { return (Resolve-Path -LiteralPath $cand).Path }
        }
    }
    return $null
}

Write-Host "==> native-build sync into dsh-dist"
foreach ($m in @('koffi', 'node-pty')) {
    $srcPkg = Resolve-RealPackage -Base $hDir -Name $m
    $dstPkg = Resolve-RealPackage -Base $outAbs -Name $m
    if (-not $dstPkg) { Write-Error "$m missing from dsh-dist after deploy"; exit 1 }
    if (-not $srcPkg) { Write-Warning "  $m build artifacts not found in source store -- skip sync"; continue }
    # Mirror the whole source package (JS + built binaries) over the deployed one to guarantee
    # the .node/.dll land beside their loader. Same version/lockfile => safe to overwrite.
    Copy-Item -LiteralPath $srcPkg -Destination $dstPkg -Recurse -Force -ErrorAction Stop
    $nativeOk = (Get-ChildItem -LiteralPath $dstPkg -Recurse -Include '*.node', '*.dll' -File -ErrorAction SilentlyContinue | Measure-Object).Count
    Write-Host "   native-synced $m <- $srcPkg -> $dstPkg ($nativeOk native file(s))"
}

# ---------------------------------------------------------------------------
# Verification gates
# ---------------------------------------------------------------------------
Write-Host "==> running verification gates"
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
