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
    param([string]$Base, [string]$Name, [hashtable]$Index = $null)
    # 1) top-level hoisted node_modules/<name> (a symlink in a pnpm workspace)
    $top = Join-Path $Base "node_modules/$Name"
    if (Test-Path -LiteralPath $top) { return (Resolve-Path -LiteralPath $top).Path }
    # 2) content index over .pnpm virtual store: pnpm renames over-long store dirs to
    #    '<truncated-name>_<hash>' where the truncation can cut INSIDE the package name
    #    (e.g. '@opentelemetry+exporter-log_8841f7...' for exporter-logs-otlp-http), so
    #    name-pattern matching is unreliable -- match on each package.json's real "name" instead.
    if ($Index -and $Index.ContainsKey($Name)) { return $Index[$Name] }
    # 3) fallback: pattern match the store dir (handles un-truncated dirs when no index passed)
    $store = Join-Path $Base "node_modules/.pnpm"
    if (Test-Path -LiteralPath $store) {
        $match = $Name -replace '/', '+'
        $dirs = Get-ChildItem -LiteralPath $store -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -like "*+$match@*" -or $_.Name -like "$match@*" }
        foreach ($d in ($dirs | Sort-Object Name -Descending)) {
            $cand = Join-Path $d.FullName "node_modules/$Name"
            if (Test-Path -LiteralPath $cand) { return (Resolve-Path -LiteralPath $cand).Path }
        }
    }
    return $null
}

# Honour a package's npm "os"/"cpu" fields against the CURRENT runner, so Linux-only workspace
# sub-packages (e.g. the landlock-run prebuilt binaries) are never pulled into a Windows bundle.
# Supports both allow-lists ("linux") and deny-lists ("!win32").
function Test-PlatformCompatible {
    param($PkgJson)
    $osNow = 'win32'
    if ($env:OS -ne 'Windows_NT') {
        $osNow = if ([System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::OSX)) { 'darwin' } else { 'linux' }
    }
    $cpuNow = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLower()
    foreach ($pair in @(@('os', $osNow), @('cpu', $cpuNow))) {
        $field = $pair[0]; $now = $pair[1]
        $prop = $PkgJson.PSObject.Properties[$field]
        if (-not $prop -or -not $prop.Value) { continue }
        $allow = @(); $deny = @()
        foreach ($v in @($prop.Value)) {
            if ("$v".StartsWith('!')) { $deny += "$v".Substring(1) } else { $allow += "$v" }
        }
        if ($deny -contains $now) { return $false }
        if ($allow.Count -gt 0 -and ($allow -notcontains $now)) { return $false }
    }
    return $true
}

# Build a package-name -> real-dir map used by Resolve-RealPackage as a content index.
# Two sources, because a pnpm workspace keeps packages in two very different places:
#   1) the .pnpm virtual store (third-party deps; dirs may be hash-truncated, so the only
#      reliable key is each package.json's real "name");
#   2) the workspace SOURCE tree (apps/ packages/ vendor/ native/). Workspace deps are
#      symlinked and never stored in .pnpm, so the virtual-store pass alone cannot see them.
#      Missing (2) is why @deepseek-ai/node-addon-landlock-run (native/landlock-run/packages/
#      entry) was silently skipped and the harness loader aborted with ERR_MODULE_NOT_FOUND.
# Platform-mismatched packages are excluded so Linux-only siblings stay out of a Windows bundle.
function Build-PackageIndex {
    param([string]$Base)
    $map = @{}
    # (1) .pnpm virtual store
    $store = Join-Path $Base 'node_modules/.pnpm'
    if (Test-Path -LiteralPath $store) {
        foreach ($d in (Get-ChildItem -LiteralPath $store -Directory -ErrorAction SilentlyContinue)) {
            $inner = Join-Path $d.FullName 'node_modules'
            if (-not (Test-Path -LiteralPath $inner)) { continue }
            foreach ($nm in (Get-ChildItem -LiteralPath $inner -Directory -ErrorAction SilentlyContinue)) {
                $cands = if ($nm.Name -like '@*') { Get-ChildItem -LiteralPath $nm.FullName -Directory -ErrorAction SilentlyContinue } else { @($nm) }
                foreach ($c in $cands) {
                    $pj = Join-Path $c.FullName 'package.json'
                    if (-not (Test-Path -LiteralPath $pj)) { continue }
                    try { $p = Get-Content -LiteralPath $pj -Raw | ConvertFrom-Json } catch { continue }
                    if ($p.PSObject.Properties['name'] -and $p.name) { $map[$p.name] = $c.FullName }
                }
            }
        }
    }
    # (2) workspace source tree -- includes native/, which the deploy-promotion scan also misses
    foreach ($section in @('apps', 'packages', 'vendor', 'native')) {
        $root = Join-Path $Base $section
        if (-not (Test-Path -LiteralPath $root)) { continue }
        Get-ChildItem -LiteralPath $root -Recurse -Filter package.json -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -notmatch '[\\/]node_modules[\\/]' } | ForEach-Object {
                try { $p = Get-Content -LiteralPath $_.FullName -Raw | ConvertFrom-Json } catch { return }
                if (-not ($p.PSObject.Properties['name'] -and $p.name)) { return }
                if (-not (Test-PlatformCompatible -PkgJson $p)) { return }
                $map[$p.name] = $_.Directory.FullName
            }
    }
    return $map
}

# Build the source-side package index ONCE (virtual store + workspace tree); every
# Resolve-RealPackage on the harness dir passes it, because pnpm hashes over-long store dir
# names (truncation cuts inside the package name) and workspace deps never land in .pnpm --
# only each package.json's real "name" field is a reliable lookup key.
Write-Host "==> building source package index (virtual store + workspace tree)"
$pkgIndex = Build-PackageIndex -Base $hDir
Write-Host "   source package index entries: $($pkgIndex.Count)"

Write-Host "==> native-build sync into dsh-dist"
foreach ($m in @('koffi', 'node-pty')) {
    $srcPkg = Resolve-RealPackage -Base $hDir -Name $m -Index $pkgIndex
    $dstPkg = Resolve-RealPackage -Base $outAbs -Name $m
    if (-not $dstPkg) { Write-Error "$m missing from dsh-dist after deploy"; exit 1 }
    if (-not $srcPkg) { Write-Warning "  $m build artifacts not found in source store -- skip sync"; continue }
    # Mirror the whole source package (JS + built binaries) over the deployed one to guarantee
    # the .node/.dll land beside their loader. Same version/lockfile => safe to overwrite.
    Copy-Item -LiteralPath $srcPkg -Destination $dstPkg -Recurse -Force -ErrorAction Stop
    $nativeOk = (Get-ChildItem -LiteralPath $dstPkg -Recurse -Include '*.node', '*.dll' -File -ErrorAction SilentlyContinue | Measure-Object).Count
    Write-Host "   native-synced $m <- $srcPkg -> $dstPkg ($nativeOk native file(s))"
}

# koffi 3.x ships its native binary in the per-arch optionalDependency sub-package
# `@koromix/koffi-<platform>-<arch>`, which koffi's index.cjs loadDynamic() resolves as
# `node_modules/@koromix/koffi-<platform>-<arch>` (a validation failing with
# "Cannot find the native Koffi module; did you bundle it correctly?" when it is absent).
#
# Why `pnpm deploy --prod` drops it:
#   - the sub-package is an optionalDependency of koffi, filtered out by --prod;
#   - the fallback build output `<koffi>/build/koffi/<triplet>/koffi.node` is stripped by deploy;
#   - re-running koffi's cnoke install in-place needs the node-api headers that ship with
#     `pnpm install` (not present after deploy), so it silently produces 0 .node files.
#
# Fix (authoritative & deterministic): fetch the SAME-version prebuilt sub-package for the
#   current OS/arch straight from the npm registry and materialize it at top-level
#   `node_modules/@koromix/koffi-<platform>-<arch>`. Its tiny index.js just require()s
#   `<triplet>/koffi.node` (e.g. win32_x64/koffi.node). The sub-package version MUST equal the
#   koffi main package version or koffi throws "Mismatched native Koffi modules" — derive it from
#   the deployed koffi package.json rather than hard-coding.
# Verification: smoke gate (b2) require()s koffi; success is only possible if this .node loads.
function Install-KoffiSubPackage {
    param([string]$OutDir, [string]$SubName, [string]$Version)
    $dst = Join-Path $OutDir "node_modules/$SubName"
    # idempotent fast path: a native binary already present (source-store sync / prior run)
    $alreadyNative = (Get-ChildItem -LiteralPath $dst -Recurse -Include '*.node', '*.dll', '*.dylib', '*.so' -File -ErrorAction SilentlyContinue | Measure-Object).Count -gt 0
    if ($alreadyNative) {
        Write-Host "   $SubName already present with a native binary -- skip registry fetch"
        return
    }
    $short = $SubName.Substring($SubName.IndexOf('/') + 1)
    $tgz = Join-Path $env:TEMP ("$short-$Version.tgz")
    $extract = Join-Path $env:TEMP ("$short-$Version-extract")
    if (Test-Path $tgz)     { Remove-Item -Force $tgz }
    if (Test-Path $extract) { Remove-Item -Recurse -Force $extract }
    New-Item -ItemType Directory -Force -Path $extract | Out-Null
    try {
        Write-Host "==> downloading $SubName@$Version from npm registry"
        Invoke-WebRequest -Uri "https://registry.npmjs.org/$SubName/-/$short-$Version.tgz" -OutFile $tgz -UseBasicParsing
        tar -xzf $tgz -C $extract
        $pkgDir = Join-Path $extract 'package'
        if (-not (Test-Path $pkgDir)) { throw "registry tarball for $SubName@$Version has no package/ root" }
        New-Item -ItemType Directory -Force -Path (Split-Path $dst) | Out-Null
        if (Test-Path $dst) { Remove-Item -Recurse -Force $dst }
        Copy-Item -Recurse -Force $pkgDir $dst
    } finally {
        Remove-Item -Force $tgz -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force $extract -ErrorAction SilentlyContinue
    }
    $n = (Get-ChildItem -LiteralPath $dst -Recurse -Include '*.node', '*.dll', '*.dylib', '*.so' -File -ErrorAction SilentlyContinue | Measure-Object).Count
    if ($n -eq 0) { Write-Error "$SubName@$Version downloaded but contains no native binary"; exit 1 }
    Write-Host "   materialized $SubName@$Version -> $dst ($n native file(s))"
}

# platform triplet matching koffi's @koromix naming (<platform>-<arch>, e.g. win32-x64)
$platform = [System.Runtime.InteropServices.RuntimeInformation]::OSDescription
$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLower()
$osName = if ($platform -match 'Windows') { 'win32' }
          elseif ($platform -match 'Darwin') { 'darwin' }
          elseif ($platform -match 'Linux') { 'linux' }
          else { 'unknown' }
$koffiArchSub = "@koromix/koffi-$osName-$arch"

# sub-package version MUST equal the koffi main package version
$koffiDst = Resolve-RealPackage -Base $outAbs -Name koffi
$koffiVer = (Get-Content (Join-Path $koffiDst 'package.json') -Raw | ConvertFrom-Json).version
Write-Host "==> materializing koffi native sub-package $koffiArchSub@$koffiVer into dsh-dist"
Install-KoffiSubPackage -OutDir $outAbs -SubName $koffiArchSub -Version $koffiVer

$koffiNative = Get-ChildItem (Join-Path $outAbs "node_modules/$koffiArchSub") -Recurse -Include '*.node', '*.dll', '*.dylib', '*.so' -File -ErrorAction SilentlyContinue
Write-Host "   $koffiArchSub resident native file(s) in dsh-dist: $($koffiNative.Count)"
if ($koffiNative.Count -eq 0) { Write-Error "koffi native binary still missing after install -- smoke gate (b2) will fail"; exit 1 }

# ---------------------------------------------------------------------------
# STEP 6.6: Rebuild the runtime dependency closure dropped by `pnpm deploy --prod`
# ---------------------------------------------------------------------------
# Why: `pnpm deploy --prod` materializes only the deploy ENTRY's direct closure. The harness
#   `web` profile dynamically imports a broad set of workspace plugins (@deepseek-ai/dsh-*),
#   and each plugin's OWN deps are not part of the CLI closure -- they resolve via the source
#   workspace's top-level hoisting / virtual store, so the deploy silently drops them and the
#   harness ESM loader aborts with ERR_MODULE_NOT_FOUND when loading each loader entry
#   (observed in smoke gate (b): chokidar, sharp, compression, ipaddr.js, fflate, open,
#   eventsource-parser, @earendil-works/pi-ai, @opentelemetry/sdk-logs, ...). A hard-coded
#   package list is whack-a-mole; the fix must walk the closure.
# Fix: BFS over every package ALREADY present in dsh-dist (workspace plugins + their deps).
#   For each dependency (dependencies + peerDependencies + optionalDependencies) that is
#   missing from dsh-dist, resolve its REAL dir in the harness source store and mirror it into
#   dsh-dist's TOP-LEVEL node_modules, then recurse into the newly copied package. Optional
#   deps that the source store never materialized for this platform (e.g. other-OS native
#   binaries) are skipped -- Node only loads the platform-matching ones.
# Verification: smoke gate (b) starts the harness web server; any dropped dependency aborts
#   with ERR_MODULE_NOT_FOUND, so the gate passes only when the closure is complete.
function Sync-PluginDependencyClosure {
    param([string]$HarnessDir, [string]$OutDir, [hashtable]$Index = $null)
    $outNM = Join-Path $OutDir 'node_modules'
    if (-not (Test-Path -LiteralPath $outNM)) { return }
    $queue = New-Object System.Collections.Generic.Queue[string]
    $seen = @{}
    foreach ($d in (Get-ChildItem -LiteralPath $outNM -Directory -ErrorAction SilentlyContinue)) {
        if ($d.Name -like '@*') {
            foreach ($sub in (Get-ChildItem -LiteralPath $d.FullName -Directory -ErrorAction SilentlyContinue)) {
                $queue.Enqueue(($d.Name + '/' + $sub.Name))
            }
        } else {
            $queue.Enqueue($d.Name)
        }
    }
    while ($queue.Count -gt 0) {
        $pkg = $queue.Dequeue()
        if ($seen.ContainsKey($pkg)) { continue }
        $seen[$pkg] = $true
        $pkgJson = Join-Path $outNM ($pkg + '/package.json')
        if (-not (Test-Path -LiteralPath $pkgJson)) { continue }
        $pj = $null
        try { $pj = Get-Content -LiteralPath $pkgJson -Raw | ConvertFrom-Json } catch { continue }
        $depMap = @{}
        foreach ($section in @('dependencies', 'peerDependencies', 'optionalDependencies')) {
            $sec = $pj.PSObject.Properties[$section]
            if ($sec -and $sec.Value) {
                foreach ($prop in $sec.Value.PSObject.Properties) { $depMap[$prop.Name] = $prop.Value }
            }
        }
        foreach ($depName in $depMap.Keys) {
            $depTop = Join-Path $outNM $depName
            if (-not (Test-Path -LiteralPath (Join-Path $depTop 'package.json'))) {
                $srcReal = Resolve-RealPackage -Base $HarnessDir -Name $depName -Index $Index
                if ($srcReal) {
                    New-Item -ItemType Directory -Force -Path (Split-Path $depTop) | Out-Null
                    Copy-Item -LiteralPath $srcReal -Destination $depTop -Recurse -Force -ErrorAction Stop
                    Write-Host "   closure-synced $depName <- $srcReal"
                } else {
                    Write-Warning "   closure dep $depName not resolvable in source store (platform-specific optional?) -- skip"
                }
            }
            $queue.Enqueue($depName)
        }
    }
    Write-Host "   plugin dependency closure sync done ($($seen.Count) packages scanned)"
}
Write-Host "==> syncing plugin runtime dependency closure into dsh-dist"
Sync-PluginDependencyClosure -HarnessDir $hDir -OutDir $outAbs -Index $pkgIndex

# ---------------------------------------------------------------------------
# STEP 6.6.5: Inject a Windows stub for @deepseek-ai/node-addon-landlock-run
# ---------------------------------------------------------------------------
# WHY: upstream `dsh-sandbox-local` hard-imports the Linux-only landlock-run
#   native module at TOP LEVEL (src/index.ts `import { ... launcherPath, probe }
#   from "@deepseek-ai/node-addon-landlock-run"`). ESM resolves static imports
#   at module load regardless of platform, so on Windows the harness crashes
#   with ERR_MODULE_NOT_FOUND before its win32 chain (windows-acl) is ever
#   consulted. The package itself is Linux-gated (its native prebuilds are
#   linux-<arch> optionalDependencies), so pnpm install never materializes it on
#   a Windows runner -- hence it is absent from dsh-dist.
#   See https://github.com/deepseek-ai/deepseek-harness (packages/sandbox/
#   sandbox-local/src/index.ts).
#
# Fix: ship a self-contained stub that exports the SAME names sandbox-local
#   imports (LAUNCHER_BIN, LAUNCHER_FAILURE_EXIT, grantArgs, launcherPath,
#   probe). On Windows only LAUNCHER_BIN / LAUNCHER_FAILURE_EXIT are read at
#   module-eval time (building the fatal-diagnostics table); the other three are
#   only reached from the Linux "landlock" chain, which the win32 chain never
#   selects, so a fail-closed implementation is both correct and unreachable.
#   Injecting a stub (rather than editing upstream compiled code) keeps the
#   artifact self-contained and survives upstream rebuilds.
# Verification: this gate fails if the stub's exports do not cover every name
#   sandbox-local imports, and if the stub is not materialized as a real file
#   (checked again after the STEP 6.7 dereference).
# ---------------------------------------------------------------------------
function Invoke-WindowsLandlockStub {
    param([string]$OutDir)
    # UTF-8 without BOM for the generated .js/.json/.d.ts (BOM would break ESM 'import' parsing).
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    Write-Host "==> [PATCH-05] injecting Windows stub for @deepseek-ai/node-addon-landlock-run"
    $pkgDir = Join-Path $OutDir "node_modules/@deepseek-ai/node-addon-landlock-run"
    New-Item -ItemType Directory -Force -Path (Join-Path $pkgDir 'lib') | Out-Null

    # identifiers that sandbox-local persists (must stay in sync with the import above;
    # the gate below asserts every one of them exists in the compiled stub).
    $stubSrc = @'
export const LAUNCHER_BIN = "landlock-run";
export const LAUNCHER_FAILURE_EXIT = 125;
// Landlock (Linux ) is the Linux security-module sandbox; these are only reachable from the
// Linux "landlock" runner chain, which the Windows (win32: ["windows-acl"]) chain never selects.
// Fail closed rather than pretending the sandbox ran.
export function grantArgs() { return []; }
export function launcherPath() { throw new Error("node-addon-landlock-run is Linux-only and unavailable on Windows"); }
export function probe() { return "unusable"; }
'@
    $pkgManifest = @'
{
  "name": "@deepseek-ai/node-addon-landlock-run",
  "version": "0.0.0-windows-stub",
  "type": "module",
  "main": "lib/index.js",
  "exports": { ".": { "types": "./lib/index.d.ts", "default": "./lib/index.js" } }
}
'@
    $stubDts = @'
export declare const LAUNCHER_BIN: string;
export declare const LAUNCHER_FAILURE_EXIT: number;
export declare function grantArgs(opts?: { readOnly?: string[]; readWrite?: string[] }): string[];
export declare function launcherPath(): string;
export declare function probe(launcher?: string, opts?: { timeoutMs?: number }): "full" | "partial" | "unusable";
'@
    [System.IO.File]::WriteAllText((Join-Path $pkgDir 'package.json'), ($pkgManifest -join ''), $utf8NoBom)
    [System.IO.File]::WriteAllText((Join-Path $pkgDir 'lib/index.d.ts'), ($stubDts -join ''), $utf8NoBom)
    [System.IO.File]::WriteAllText((Join-Path $pkgDir 'lib/index.js'), $stubSrc, $utf8NoBom)

    # gate: every name sandbox-local statically imports must be exported by the stub
    $required = @('LAUNCHER_BIN', 'LAUNCHER_FAILURE_EXIT', 'grantArgs', 'launcherPath', 'probe')
    $missing = @($required | Where-Object { $stubSrc -notmatch ("export (const|function) $([regex]::Escape($_))") })
    if ($missing.Count -gt 0) {
        Write-Error "landlock stub missing exports: $($missing -join ', ')"
        exit 1
    }
    Write-Host "   stub exports verified: $($required -join ', ')"
}
Invoke-WindowsLandlockStub -OutDir $outAbs

# ---------------------------------------------------------------------------
# STEP 6.7: Materialize dsh-dist as REAL files, then strip the residual .pnpm
#   virtual-store structure.
#
# Why: `pnpm deploy --legacy` still lays down the top-level node_modules as
#   junctions/symlinks INTO .pnpm/<pkg>@<ver>/node_modules/<pkg> (observed: 266
#   links right after deploy), so naively deleting .pnpm leaves every top-level
#   link dangling. robocopy /E dereferences each reparse point into a plain
#   file/dir copy (verified locally), so we re-materialize the whole tree through
#   a staging dir, swap it in, and only then drop .pnpm. A junction left inside
#   dsh-dist would dangle once the source checkout is gone at install time; the
#   artifact must be self-contained real files only.
# Verification: this step fails the build if any reparse point remains in dsh-dist.
# ---------------------------------------------------------------------------
Write-Host "==> materializing dsh-dist as real files (dereference junctions/symlinks)"
$staging = Join-Path (Split-Path -Parent $outAbs) ((Split-Path -Leaf $outAbs) + '-real')
if (Test-Path -LiteralPath $staging) { Remove-Item -LiteralPath $staging -Recurse -Force }
robocopy $outAbs $staging /E /COPY:DAT /NFL /NDL /NJH /NJS /NP /R:1 /W:1 | Out-Null
if ($LASTEXITCODE -ge 8) {
    Write-Error "robocopy dereference failed (exit $LASTEXITCODE) -- dsh-dist not materialized as real files"
    exit 1
}
Remove-Item -LiteralPath $outAbs -Recurse -Force
Move-Item -LiteralPath $staging -Destination $outAbs

Write-Host "==> stripping .pnpm virtual store from dsh-dist"
$pnpmStore = Join-Path $outAbs 'node_modules/.pnpm'
if (Test-Path -LiteralPath $pnpmStore) {
    Remove-Item -LiteralPath $pnpmStore -Recurse -Force
    Write-Host "   removed $pnpmStore"
} else {
    Write-Host "   no .pnpm virtual store present (clean deploy)"
}
$linkCount = (Get-ChildItem -LiteralPath (Join-Path $outAbs 'node_modules') -Recurse -Force -ErrorAction SilentlyContinue |
    Where-Object { $_.LinkType -or ($_.Attributes -band [System.IO.FileAttributes]::ReparsePoint) } | Measure-Object).Count
if ($linkCount -gt 0) {
    Write-Error "dsh-dist still contains $linkCount symlink/junction(s) -- Tier 2 bundle must be self-contained real files"
    exit 1
}
Write-Host "   verified: no symlinks/junctions remain in dsh-dist"

# ---------------------------------------------------------------------------
# Verification gates
# ---------------------------------------------------------------------------
Write-Host "==> running verification gates"
foreach ($m in @('node-pty', 'koffi', 'sharp', 'chokidar', 'resolve.exports')) {
    $mp = Join-Path $outAbs "node_modules/$m"
    if (-not (Test-Path (Join-Path $mp 'package.json'))) {
        Write-Error "$m missing/empty in dsh-dist after pnpm deploy -- this breaks native-module features (terminal/FFI/image) or the harness web server (ESM dep). Check bundle-dsh native-module promotion / runtime-dep closure sync."
        exit 1
    }
    Write-Host "   verified native/runtime module present: $m"
}

# @deepseek-ai/dsh-sandbox-local STATICALLY imports @deepseek-ai/node-addon-landlock-run at
# loader time. That package is a WORKSPACE package under native/landlock-run/packages/entry --
# neither promoted into the deploy closure nor present in .pnpm -- so the runtime dep closure
# sync must materialize it from the workspace tree (see Build-PackageIndex). Its runtime is
# Linux-only, but the JS seam must be importable on Windows (it probes and fails closed).
# Absent => the harness aborts at startup with ERR_MODULE_NOT_FOUND (smoke gate (b)).
$landlock = Join-Path $outAbs 'node_modules/@deepseek-ai/node-addon-landlock-run'
if (-not (Test-Path -LiteralPath (Join-Path $landlock 'package.json'))) {
    Write-Error "sandbox runtime dep missing: $landlock -- workspace package not synced (Build-PackageIndex must cover native/)"
    exit 1
}
if (-not (Test-Path -LiteralPath (Join-Path $landlock 'lib/index.js'))) {
    Write-Error "sandbox runtime dep has no built entry: $landlock/lib/index.js -- harness build did not compile native/landlock-run/packages/entry"
    exit 1
}
Write-Host "   verified sandbox runtime dep present: @deepseek-ai/node-addon-landlock-run"

# pi-ai ships a hidden data manifest (.manifest.json) that its loader requires() at runtime.
# upload-artifact@v4 drops dot-files by default, so CI MUST set include-hidden-files: true on
# the dsh-dist artifact; this gate fails the bundle early if the file is missing locally.
$piAiManifest = Join-Path $outAbs "node_modules/@earendil-works/pi-ai/dist/providers/data/.manifest.json"
if (-not (Test-Path -LiteralPath $piAiManifest)) {
    Write-Error "pi-ai hidden manifest missing: $piAiManifest -- ensure the package synced and CI uploads hidden files (include-hidden-files: true)"
    exit 1
}
Write-Host "   verified pi-ai hidden manifest present: $piAiManifest"

# landlock stub must survive the STEP 6.7 robocopy dereference as a real file
$landlockStub = Join-Path $outAbs "node_modules/@deepseek-ai/node-addon-landlock-run/lib/index.js"
if (-not (Test-Path -LiteralPath $landlockStub)) {
    Write-Error "Windows landlock stub missing after materialize: $landlockStub -- harness will crash on Windows (upstream dsh-sandbox-local top-level imports it)"
    exit 1
}
Write-Host "   verified landlock Windows stub present: $landlockStub"

$entry = Join-Path $outAbs "lib/bin.js"
if (-not (Test-Path $entry)) { Write-Error "deploy produced no entry: $entry"; exit 1 }

Write-Host "OK dsh-dist ready: $entry"
