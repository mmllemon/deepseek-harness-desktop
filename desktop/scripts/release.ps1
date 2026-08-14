<#
.SYNOPSIS
  Local release: build harness, SEA pack, drop into externalBin, build frontend + Tauri installer.
.PARAMETER HarnessDir
  Harness root directory (default ./deepseek-harness).
#>
param(
    [string]$HarnessDir = "./deepseek-harness"
)

$ErrorActionPreference = "Stop"

$root    = Resolve-Path (Join-Path $PSScriptRoot ".." "..")
$dshSrc  = Join-Path $root "dist/dsh.exe"
$binDir  = Join-Path $root "src-tauri/externalBin"
$dshDst  = Join-Path $binDir "dsh-x86_64-pc-windows-msvc.exe"

# 1) build harness
& "$PSScriptRoot/build-harness.ps1" -HarnessDir $HarnessDir
if ($LASTEXITCODE -ne 0) { Write-Error "build-harness failed"; exit 1 }

# 2) SEA pack
& node (Join-Path $root "scripts/seapack.cjs")
if ($LASTEXITCODE -ne 0) { Write-Error "seapack failed"; exit 1 }

# 3) copy as Tauri externalBin sidecar
if (-not (Test-Path $dshSrc)) { Write-Error "dsh.exe not found: $dshSrc"; exit 1 }
New-Item -ItemType Directory -Force -Path $binDir | Out-Null
Copy-Item -Path $dshSrc -Destination $dshDst -Force
Write-Host "Copied as sidecar: $dshDst"

# 4) + 5) frontend build and Tauri package
Push-Location $root
try {
    Write-Host "==> npm ci"
    & npm ci
    if ($LASTEXITCODE -ne 0) { throw "npm ci failed" }

    Write-Host "==> npm run build (frontend)"
    & npm run build
    if ($LASTEXITCODE -ne 0) { throw "npm run build failed" }

    Write-Host "==> npm run tauri build"
    & npm run tauri build
    if ($LASTEXITCODE -ne 0) {
        Write-Host "npm run tauri build failed; falling back to npx @tauri-apps/cli build ..."
        & npx --yes @tauri-apps/cli build
        if ($LASTEXITCODE -ne 0) { throw "tauri build failed" }
    }
} finally {
    Pop-Location
}

Write-Host "release complete. Installer at src-tauri/target/x86_64-pc-windows-msvc/release/bundle/"
