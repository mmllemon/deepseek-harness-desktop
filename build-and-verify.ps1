# build-and-verify.ps1 - One-click build + section 12.6 acceptance gate.
# Double-click the bundled build-and-verify.bat; no programming knowledge required.
# Prereqs on the machine: Rust (with MSVC toolchain), Node 22 LTS, pnpm 11.7.0,
# and the deepseek-harness source repo (default D:\Program Files\deepseek-harness).
param(
    [string]$HarnessDir = "D:\Program Files\deepseek-harness"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Push-Location $root

function Banner($m){ Write-Host ""; Write-Host ("==== " + $m + " ====") -ForegroundColor Cyan }
function Pass($m){ Write-Host ("[OK]   " + $m) -ForegroundColor Green }
function Fail($m){ Write-Host ("[FAIL] " + $m) -ForegroundColor Red }

# 1. prereq
Banner "1/6 检查前置工具 (cargo / node / pnpm / harness)"
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
$node  = Get-Command node  -ErrorAction SilentlyContinue
$pnpm = Get-Command pnpm  -ErrorAction SilentlyContinue
if(-not $cargo){ Fail "未找到 cargo (Rust)。请先安装 Rust 并勾选 MSVC 工具链: https://rustup.rs"; Read-Host "按回车退出"; exit 1 }
$nv = (& node -v 2>$null)
if(-not $node){ Fail "未找到 node。请安装 Node 22 LTS"; Read-Host "按回车退出"; exit 1 }
if($nv -notmatch "v22"){ Write-Host ("[提示] 检测到 node " + $nv + "，建议 Node 22 LTS") }
if(-not $pnpm){
    Write-Host "[提示] 未找到 pnpm，尝试用 corepack 启用..."
    & corepack enable 2>$null
    & corepack prepare pnpm@11.7.0 --activate 2>$null
    $pnpm = Get-Command pnpm -ErrorAction SilentlyContinue
}
if(-not $pnpm){ Fail "未找到 pnpm。请安装: npm i -g pnpm@11.7.0"; Read-Host "按回车退出"; exit 1 }

$hDir = Resolve-Path $HarnessDir -ErrorAction SilentlyContinue
if(-not $hDir){ Fail ("未找到 harness 仓库: " + $HarnessDir + " （请用 -HarnessDir 指定正确路径）"); Read-Host "按回车退出"; exit 1 }
Pass ("harness 仓库: " + $hDir.Path)

# 2. build harness
Banner "2/6 构建 deepseek-harness (pnpm install + build)"
& powershell -NoProfile -ExecutionPolicy Bypass -File "desktop/scripts/build-harness.ps1" -HarnessDir $hDir.Path
if($LASTEXITCODE -ne 0){ Fail "harness 构建失败"; Read-Host "按回车退出"; exit 1 }
$entry = Join-Path $hDir.Path "apps/cli/lib/bin.js"
if(-not (Test-Path $entry)){ Fail ("未找到构建产物: " + $entry); Read-Host "按回车退出"; exit 1 }
Pass "harness 构建完成"

# 3. Tier 2 bundle: pnpm deploy harness -> dsh-dist (self-contained node_modules)
Banner "3/6 打包 dsh-dist (Tier 2: pnpm deploy)"
& powershell -NoProfile -ExecutionPolicy Bypass -File "desktop/scripts/bundle-dsh.ps1" -HarnessDir $hDir.Path -OutDir (Join-Path $root "dsh-dist")
if($LASTEXITCODE -ne 0){ Fail "dsh-dist 打包失败"; Read-Host "按回车退出"; exit 1 }
$entry = Join-Path $root "dsh-dist/lib/bin.js"
if(-not (Test-Path $entry)){ Fail ("未找到部署入口: " + $entry); Read-Host "按回车退出"; exit 1 }
Pass "dsh-dist 打包完成"

# 4. section 12.6 gate (smoke)
Banner "4/6 运行验收 Gate (smoke.ps1 - section 12.6)"
& powershell -NoProfile -ExecutionPolicy Bypass -File "desktop/scripts/smoke.ps1" -EntryPath $entry
if($LASTEXITCODE -ne 0){ Fail "验收 Gate 未通过，请查看上方 FAIL 项"; Read-Host "按回车退出"; exit 1 }
Pass "验收 Gate 通过"

# 5. place sidecar (node.exe) into src-tauri/binaries (Tauri 期望此路径)
Banner "5/6 放置 node sidecar 到 src-tauri/binaries"
$binDir = Join-Path $root "src-tauri/binaries"
New-Item -ItemType Directory -Force -Path $binDir | Out-Null
$nodeExe = (node -e "process.stdout.write(process.execPath)")
Copy-Item -Path $nodeExe -Destination (Join-Path $binDir "node-x86_64-pc-windows-msvc.exe") -Force
Pass "sidecar 已放置"

# 6. tauri build
Banner "6/6 构建 Tauri 安装包 (cargo tauri build)"
& npm install
if($LASTEXITCODE -ne 0){ Fail "npm install 失败"; Read-Host "按回车退出"; exit 1 }
& npm run tauri build
if($LASTEXITCODE -ne 0){ Fail "cargo tauri build 失败（多半是缺少 Visual Studio 生成工具 / MSVC 链接器）"; Read-Host "按回车退出"; exit 1 }

Write-Host ""
Write-Host "==== 全部完成 ====" -ForegroundColor Green
Write-Host "安装包位于: src-tauri/target/release/bundle/ 下（.exe / .msi）"
Read-Host "按回车退出"
