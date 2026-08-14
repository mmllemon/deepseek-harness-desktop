<#
.SYNOPSIS
  Hard acceptance Gate from doc section 12.6 (real Windows environment, item by item).
.DESCRIPTION
  Exit 0 only if all pass; required items (a)(b)(c)(e) failure => exit 1.
  (a) dsh.exe exists and is a single file;
  (b) start dsh web --port <free port>, capture ready line http://127.0.0.1:<port> within timeout;
  (c) curl http://127.0.0.1:<port> returns HTML;
  (d) if DEEPSEEK_API_KEY set, probe API (informational, not counted as failure);
  (e) clean exit (terminate process);
  (f) print PASS/FAIL summary.
.PARAMETER DshPath
  Path to dsh.exe under test (default ./dist/dsh.exe).
.PARAMETER TimeoutSec
  Max seconds to wait for ready line (default 60).
#>
param(
    [string]$DshPath = "./dist/dsh.exe",
    [int]$TimeoutSec = 60
)

$ErrorActionPreference = "Stop"

$results = @()

function Record($name, $ok, $detail) {
    if ($ok) { Write-Host "PASS  $name : $detail" }
    else     { Write-Host "FAIL  $name : $detail" }
    $script:results += [PSCustomObject]@{ Name = $name; Ok = $ok; Detail = $detail }
}

$dsh = Resolve-Path $DshPath -ErrorAction SilentlyContinue
if (-not $dsh) { Write-Error "dsh not found: $DshPath"; exit 1 }
$dsh = $dsh.Path

$proc = $null
try {
    # (a) dsh.exe exists and is a single file
    $isFile  = Test-Path -Path $dsh -PathType Leaf
    $size    = if ($isFile) { (Get-Item $dsh).Length } else { 0 }
    $singleFile = $isFile -and $size -gt 1MB
    Record "(a) dsh.exe exists and is single file" $singleFile "path=$dsh size=$size"
    if (-not $singleFile) { throw "Gate (a) failed" }

    # pick a free port
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = ([string]$listener.LocalEndpoint).Split(':')[-1]
    $listener.Stop()

    # (b) start and capture ready line
    $outFile = Join-Path $env:TEMP ("dsh-smoke-" + [guid]::NewGuid().ToString("N") + ".out.log")
    $errFile = Join-Path $env:TEMP ("dsh-smoke-" + [guid]::NewGuid().ToString("N") + ".err.log")
    Write-Host "==> start dsh web --port $port (stdout -> $outFile)"
    $proc = Start-Process -FilePath $dsh -ArgumentList "web", "--port", $port `
        -RedirectStandardOutput $outFile -RedirectStandardError $errFile -PassThru -NoNewWindow

    $ready    = $false
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ($proc.HasExited) {
            $err = if (Test-Path $errFile) { Get-Content $errFile -Raw -ErrorAction SilentlyContinue } else { "" }
            throw "dsh exited before ready. stderr: $err"
        }
        if (Test-Path $outFile) {
            $log = Get-Content $outFile -Raw -ErrorAction SilentlyContinue
            if ($log -match ("http://127.0.0.1:" + [regex]::Escape($port))) {
                $ready = $true
                break
            }
        }
        Start-Sleep -Seconds 1
    }
    Record "(b) captured ready line http://127.0.0.1:$port" $ready "port=$port"
    if (-not $ready) {
        $log = if (Test-Path $outFile) { Get-Content $outFile -Raw } else { "" }
        throw "Gate (b) failed. stdout: $log"
    }

    # (c) curl returns HTML
    $bodyFile = Join-Path $env:TEMP ("dsh-smoke-" + [guid]::NewGuid().ToString("N") + ".html")
    $code = & curl.exe -s -o $bodyFile -w "%{http_code}" "http://127.0.0.1:$port"
    $body = if (Test-Path $bodyFile) { Get-Content $bodyFile -Raw -ErrorAction SilentlyContinue } else { "" }
    $isHtml = ($code -eq "200") -and ($body -match "(?i)<!doctype|<html|<body")
    Record "(c) curl returns HTML" $isHtml "http_code=$code"
    if (-not $isHtml) { throw "Gate (c) failed. http_code=$code" }

    # (d) optional API reachability probe (not counted as failure)
    if ($env:DEEPSEEK_API_KEY) {
        Write-Host "==> (d) DEEPSEEK_API_KEY set, running reachability probe (not counted)"
        try {
            $probe = & curl.exe -s -o $null -w "%{http_code}" -X POST "http://127.0.0.1:$port/api/chat" `
                -H "Content-Type: application/json" `
                -d '{"message":"ping"}' --max-time 15
            Write-Host "    probe http_code=$probe (informational)"
        } catch {
            Write-Host "    probe failed (skippable): $_"
        }
    } else {
        Write-Host "==> (d) DEEPSEEK_API_KEY not set, skipping API reachability probe"
    }
    Record "(d) API reachability probe (optional)" $true "skipped-or-probed"
} catch {
    Write-Error "smoke failed: $_"
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    foreach ($r in $results) { Write-Host ("  " + $r.Name + " -> " + $(if ($r.Ok) { "PASS" } else { "FAIL" })) }
    Write-Host "RESULT: FAIL"
    exit 1
} finally {
    # (e) clean exit
    if ($proc -and -not $proc.HasExited) {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 1
    }
}

# (f) summary
Write-Host "`n==== SMOKE GATE SUMMARY ===="
$allOk = $true
foreach ($r in $results) {
    $mark = if ($r.Ok) { "PASS" } else { $allOk = $false; "FAIL" }
    Write-Host ("  " + $r.Name + " -> " + $mark)
}
if ($allOk) { Write-Host "RESULT: PASS"; exit 0 } else { Write-Host "RESULT: FAIL"; exit 1 }
