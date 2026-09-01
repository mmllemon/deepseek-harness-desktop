<#
.SYNOPSIS
  Hard acceptance Gate from doc section 12.6 (real Windows environment, item by item).
.DESCRIPTION
  Exit 0 only if all REQUIRED items pass; required (a)(b)(b2)(c)(e) failure => exit 1.
  (a) harness entry (dsh-dist/lib/bin.js) exists;
  (b) start `node <entry> web --port <free port>`, capture ready line http://127.0.0.1:<port> within timeout;
  (b2) native modules functional: node-pty can actually spawn a pty AND koffi loads in the deployed
       dsh-dist. This is the core reason SEA (Tier 1) was abandoned in favour of Tier 2 sidecar.
  (c) curl http://127.0.0.1:<port> returns HTML;
  (d) if DEEPSEEK_API_KEY set, probe API (INFORMATIONAL only, never counts as pass/fail);
  (e) clean exit (terminate process);
  (f) print PASS/FAIL summary.
.PARAMETER EntryPath
  Path to the deployed harness entry (default ./dsh-dist/lib/bin.js) run via `node`.
.PARAMETER TimeoutSec
  Max seconds to wait for ready line (default 60).
#>
param(
    [string]$EntryPath = "./dsh-dist/lib/bin.js",
    [int]$TimeoutSec = 60
)

$ErrorActionPreference = "Stop"

$results = @()

# alpha.3+ 上游 harness 强制浏览器会话鉴权：launch URL 形如
# `http://127.0.0.1:<port>/?token=<43-char>`，GET 它换取签名 Set-Cookie。
# 若 harness 未注入 token（旧版本），则为 $null，gate (c) 退化为直接 curl。
$launchToken = $null

function Record($name, $ok, $detail, $gate = $true) {
    if ($ok) { Write-Host "PASS  $name : $detail" }
    else     { Write-Host "FAIL  $name : $detail" }
    $script:results += [PSCustomObject]@{ Name = $name; Ok = $ok; Detail = $detail; Gate = $gate }
}

$entry = Resolve-Path $EntryPath -ErrorAction SilentlyContinue
if (-not $entry) { Write-Error "harness entry not found: $EntryPath"; exit 1 }
$entry = $entry.Path
$nodeExe = (node -e "process.stdout.write(process.execPath)")

$proc = $null
try {
    # (a) harness entry exists
    $exists = Test-Path -Path $entry -PathType Leaf
    Record "(a) harness entry exists" $exists "path=$entry"
    if (-not $exists) { throw "Gate (a) failed" }

    # (b2) native modules functional inside the deployed dsh-dist (run from its dir)
    Push-Location (Split-Path $entry)
    try {
        $probe = @'
const pty = require('node-pty');
const koffi = require('koffi');
const shell = process.platform === 'win32' ? 'cmd.exe' : 'sh';
const term = pty.spawn(shell, [], { cols: 80, rows: 30 });
let out = '';
term.on('data', d => { out += d; });
term.write('echo NATIVE_PTY_OK\n');
setTimeout(() => {
  const koffiOk = koffi && typeof koffi.register === 'function';
  const ptyOk = out.includes('NATIVE_PTY_OK');
  console.log('NATIVE_MODULES_OK pty=' + ptyOk + ' koffi=' + koffiOk);
  process.exit(ptyOk && koffiOk ? 0 : 3);
}, 2500);
'@
        $nativeOut = $probe | & $nodeExe - 2>&1
        $nativeOk = ($LASTEXITCODE -eq 0) -and ($nativeOut -match 'NATIVE_MODULES_OK')
        Record "(b2) native modules work in dsh-dist (node-pty spawn + koffi load)" $nativeOk "exit=$LASTEXITCODE; $nativeOut"
        if (-not $nativeOk) { throw "Gate (b2) failed: native modules not functional in dsh-dist" }
    } finally {
        Pop-Location
    }

    # pick a free port
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = ([string]$listener.LocalEndpoint).Split(':')[-1]
    $listener.Stop()

    # (b) start and capture ready line
    $outFile = Join-Path $env:TEMP ("dsh-smoke-" + [guid]::NewGuid().ToString("N") + ".out.log")
    $errFile = Join-Path $env:TEMP ("dsh-smoke-" + [guid]::NewGuid().ToString("N") + ".err.log")
    Write-Host "==> start node $entry web --port $port (stdout -> $outFile)"
    $proc = Start-Process -FilePath $nodeExe -ArgumentList $entry, "web", "--port", $port `
        -RedirectStandardOutput $outFile -RedirectStandardError $errFile -PassThru -NoNewWindow

    $ready    = $false
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ($proc.HasExited) {
            $err = if (Test-Path $errFile) { Get-Content $errFile -Raw -ErrorAction SilentlyContinue } else { "" }
            throw "harness exited before ready. stderr: $err"
        }
        if (Test-Path $outFile) {
            $log = Get-Content $outFile -Raw -ErrorAction SilentlyContinue
            # 捕获 alpha.3+ 的 launch token（?token= 之后的 base64 串），供 gate (c) 换取 cookie
            if ($log -match ("http://127\.0\.0\.1:" + [regex]::Escape($port) + "/\?token=([A-Za-z0-9_\-]+)")) {
                $ready = $true
                $script:launchToken = $matches[1]
                break
            }
            # 旧版本（无 token 注入）：仅匹配 host:port 即算就绪
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
    # alpha.3+ 强制浏览器会话鉴权：裸端口 GET / 直接 401，必须先 GET launch URL 换取签名 cookie。
    # 流程：① curl -c jar GET /?token=<launchToken> → 303 + Set-Cookie；② curl -b jar GET / → 200 + HTML。
    $bodyFile = Join-Path $env:TEMP ("dsh-smoke-" + [guid]::NewGuid().ToString("N") + ".html")
    $cookieJar = Join-Path $env:TEMP ("dsh-smoke-" + [guid]::NewGuid().ToString("N") + ".ck.txt")
    if ($launchToken) {
        $launchCode = & curl.exe -s -o nul -c $cookieJar -w "%{http_code}" "http://127.0.0.1:$port/?token=$launchToken"
        $code = & curl.exe -s -b $cookieJar -o $bodyFile -w "%{http_code}" "http://127.0.0.1:$port"
        $detail = "http_code=$code; launch_code=$launchCode"
    } else {
        # 旧版本（无 launch token 注入）：直接访问。
        $code = & curl.exe -s -o $bodyFile -w "%{http_code}" "http://127.0.0.1:$port"
        $detail = "http_code=$code"
    }
    $body = if (Test-Path $bodyFile) { Get-Content $bodyFile -Raw -ErrorAction SilentlyContinue } else { "" }
    $isHtml = ($code -eq "200") -and ($body -match "(?i)<!doctype|<html|<body")
    Record "(c) curl returns HTML" $isHtml $detail
    if (-not $isHtml) { throw "Gate (c) failed. $detail" }

    # (d) optional API reachability probe — INFORMATIONAL ONLY, never gates the result
    if ($env:DEEPSEEK_API_KEY) {
        Write-Host "==> (d) DEEPSEEK_API_KEY set, running reachability probe (informational)"
        try {
            $probe = & curl.exe -s -o $null -w "%{http_code}" -X POST "http://127.0.0.1:$port/api/chat" `
                -H "Content-Type: application/json" `
                -d '{"message":"ping"}' --max-time 15
            Write-Host "    probe http_code=$probe (informational)"
            Record "(d) API reachability probe (optional)" $true "http_code=$probe" $false
        } catch {
            Write-Host "    probe failed (skippable): $_"
            Record "(d) API reachability probe (optional)" $true "skipped: $_" $false
        }
    } else {
        Write-Host "==> (d) DEEPSEEK_API_KEY not set, skipping API reachability probe (informational)"
        Record "(d) API reachability probe (optional)" $true "skipped (no key)" $false
    }
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

# (f) summary — only GATED items decide PASS/FAIL
Write-Host "`n==== SMOKE GATE SUMMARY ===="
$allOk = $true
foreach ($r in $results) {
    $mark = if ($r.Ok) { "PASS" } else { if ($r.Gate) { $allOk = $false }; "FAIL" }
    $tag = if (-not $r.Gate) { " (informational)" } else { "" }
    Write-Host ("  " + $r.Name + " -> " + $mark + $tag)
}
if ($allOk) { Write-Host "RESULT: PASS"; exit 0 } else { Write-Host "RESULT: FAIL"; exit 1 }
