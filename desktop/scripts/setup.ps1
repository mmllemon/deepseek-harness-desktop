<#
.SYNOPSIS
  Bootstrap: clone fork, configure upstream, build harness, produce dist/dsh.exe.
.PARAMETER ForkUrl
  Fork repository URL (required).
.PARAMETER UpstreamUrl
  Upstream repository URL (optional; configured as upstream remote).
.PARAMETER HarnessDir
  Local harness directory (default ./deepseek-harness).
#>
param(
    [Parameter(Mandatory = $true)][string]$ForkUrl,
    [string]$UpstreamUrl,
    [string]$HarnessDir = "./deepseek-harness"
)

$ErrorActionPreference = "Stop"

if (Test-Path $HarnessDir) {
    Write-Error "Target directory already exists: $HarnessDir (remove it or use another path)"
    exit 1
}

Write-Host "==> clone fork: $ForkUrl -> $HarnessDir"
& git clone $ForkUrl $HarnessDir
if ($LASTEXITCODE -ne 0) { Write-Error "git clone failed"; exit 1 }

if ($UpstreamUrl) {
    Write-Host "==> add upstream remote: $UpstreamUrl"
    & git -C $HarnessDir remote add upstream $UpstreamUrl
    if ($LASTEXITCODE -ne 0) { Write-Error "git remote add upstream failed"; exit 1 }
}

& "$PSScriptRoot/build-harness.ps1" -HarnessDir $HarnessDir
if ($LASTEXITCODE -ne 0) { Write-Error "build-harness failed"; exit 1 }

Write-Host "==> node scripts/seapack.cjs"
& node scripts/seapack.cjs
if ($LASTEXITCODE -ne 0) { Write-Error "seapack failed"; exit 1 }

Write-Host "setup complete. dist/dsh.exe generated."
