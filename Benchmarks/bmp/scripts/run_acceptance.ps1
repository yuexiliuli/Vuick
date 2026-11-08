# BMP acceptance 一键对账：前置检查（corpus/BMP、BMP-MATRIX、refOut 就位，缺则调 run_csharp_ref.ps1）
# → oracle 闸（debug 字节恒等 → 含 bmp_csharp_parity）→ 静置 60s → release 探针 → compare_refs 双路
# geomean（总 + 按布局类别表）。BMP 全文件 RGBA8 可比。
# 用法：pwsh -File Benchmarks\bmp\scripts\run_acceptance.ps1   （-NoWait 跳过静置 60s）
# 输出：logs\acceptance_<时间戳>.log（stdout 双落，UTF-8 无 BOM）；结尾主口径。
param(
    [switch]$NoWait # 跳过静置 60s（CI/已有热机态）
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Resolve-Path (Join-Path $here '..\..\..')
$suite = Resolve-Path (Join-Path $here '..')
$logDir = Join-Path $suite 'logs'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$log = Join-Path $logDir "acceptance_$stamp.log"
$utf8 = New-Object System.Text.UTF8Encoding($false)

$corpus = Join-Path $root 'Benchmarks\corpus'
$matrix = Join-Path $corpus 'BMP-MATRIX'
$probeManifest = Join-Path $suite 'probe\Cargo.toml'
$csharpLog = Join-Path $corpus 'csharp\bmp_csharp.log'
$refOut = Join-Path $root 'target\csharp-ref\bmp'

Write-Host "root=$root"; Write-Host "log=$log"
'=' * 20 | Tee-Object -FilePath $log

# 0) 前置检查：corpus/BMP、BMP-MATRIX、refOut 就位；缺 → 调 run_csharp_ref.ps1 补全。
$needRef = $false
if (-not (Test-Path (Join-Path $corpus 'BMP'))) { Write-Error "corpus/BMP 不存在: $corpus" }
if (-not (Test-Path $matrix) -or @(Get-ChildItem $matrix -Filter *.bmp -File -ErrorAction SilentlyContinue).Count -eq 0) {
    Write-Host '== 前置：BMP-MATRIX 缺失/为空 → 调 run_csharp_ref.ps1 生成 =='
    $needRef = $true
}
if (-not (Test-Path $refOut) -or @(Get-ChildItem $refOut -File -ErrorAction SilentlyContinue).Count -eq 0) {
    Write-Host '== 前置：target\csharp-ref\bmp 为空 → 调 run_csharp_ref.ps1 =='
    $needRef = $true
}
if ($needRef) {
    & (Join-Path $here 'run_csharp_ref.ps1') | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
    if (-not (Test-Path $matrix) -or @(Get-ChildItem $matrix -Filter *.bmp -File -ErrorAction SilentlyContinue).Count -eq 0) {
        Write-Error "BMP-MATRIX 仍为空 — 生成失败，请检查 corpus/BMP/001.bmp + 002.bmp 与 dotnet/SimdCodecs 装配"
    }
    if (-not (Test-Path $refOut) -or @(Get-ChildItem $refOut -File -ErrorAction SilentlyContinue).Count -eq 0) {
        Write-Error "target\csharp-ref\bmp 仍为空 — 先跑 pwsh -File Benchmarks\bmp\scripts\run_csharp_ref.ps1"
    }
}

# 1) oracle 闸（debug，字节恒等全量 + bmp_csharp_parity；FAILED 即停）
Write-Host '== oracle gate (cargo test -p kimg_bmp -- --test-threads=1) =='
$env:KIMG_BMP_CORPUS = $corpus
cargo test -p kimg_bmp -- --test-threads=1 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
if ($LASTEXITCODE -ne 0) { Write-Error 'oracle 闸不过（cargo test 非零），先修字节恒等再对账'; exit 1 }
if (Select-String -Path $log -Pattern 'result: FAILED' -Quiet) {
    Write-Error 'oracle 闸不过（result: FAILED），先修字节恒等再对账'; exit 1
}

# 2) release 探针构建（精确测量：默认无插桩）
Write-Host '== release probe build =='
cargo build --release --manifest-path $probeManifest 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
if ($LASTEXITCODE -ne 0) { Write-Error '探针 release 构建失败'; exit 1 }
Remove-Item Env:KIMG_BMP_CORPUS -ErrorAction SilentlyContinue

if (-not $NoWait) { Write-Host '== 静置 60s（散热/变频稳定）=='; Start-Sleep -Seconds 60 }

# 3) release 探针（ours 单核）
Write-Host '== release probe (ours) =='
$env:KIMG_BMP_CORPUS = $corpus
cargo run --release --manifest-path $probeManifest 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
Remove-Item Env:KIMG_BMP_CORPUS -ErrorAction SilentlyContinue

# 4) C# 表合并 + 双路 geomean（总 + 按布局类别表）
Write-Host '== compare vs C# =='
$py = Get-Command python -ErrorAction SilentlyContinue
if ($py) {
    $env:PYTHONUTF8 = '1'
    python (Join-Path $suite 'compare_refs.py') $log $csharpLog 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
    Remove-Item Env:PYTHONUTF8 -ErrorAction SilentlyContinue
} else {
    Write-Host '(python 缺失，跳过合并表)'
}

# 5) UTF-8 无 BOM 归一（防旧 Tee 编码坑）
$text = Get-Content -Path $log -Raw -Encoding utf8
[System.IO.File]::WriteAllText($log, $text, $utf8)

Write-Host "DONE log=$log"
