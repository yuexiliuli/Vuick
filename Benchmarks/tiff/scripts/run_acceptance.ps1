# TIFF acceptance 一键对账：前置检查（corpus/BMP、corpus/TIFF、refOut 就位，缺则调 run_csharp_ref.ps1）
# → oracle 闸（debug 字节恒等 → 含 tiff_csharp_parity）→ release gen+probe 构建 → 静置 60s →
# release 探针 → compare_refs 双路 geomean（总 + 按布局类别表）。TIFF 全文件 RGBA8 可比。
# 用法：pwsh -File Benchmarks\tiff\scripts\run_acceptance.ps1   （-NoWait 跳过静置 60s）
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
$tiffDir = Join-Path $corpus 'TIFF'
$bmpDir = Join-Path $corpus 'BMP'
$genManifest = Join-Path $suite 'gen\Cargo.toml'
$probeManifest = Join-Path $suite 'probe\Cargo.toml'
$csharpLog = Join-Path $corpus 'csharp\tiff_csharp.log'
$refOut = Join-Path $root 'target\csharp-ref\tiff'

Write-Host "root=$root"; Write-Host "log=$log"
'=' * 20 | Tee-Object -FilePath $log

# 0) 前置检查：corpus/BMP、corpus/TIFF、refOut 就位；缺 → 调 run_csharp_ref.ps1 补全。
$needRef = $false
if (-not (Test-Path (Join-Path $bmpDir '001.bmp'))) { Write-Error "corpus/BMP/001.bmp 不存在: $bmpDir" }
if (-not (Test-Path $tiffDir) -or @(Get-ChildItem $tiffDir -Filter *.tif -File -ErrorAction SilentlyContinue).Count -eq 0) {
    Write-Host '== 前置：corpus\TIFF 缺失/为空 → 调 run_csharp_ref.ps1 生成 =='
    $needRef = $true
}
if (-not (Test-Path $refOut) -or @(Get-ChildItem $refOut -File -ErrorAction SilentlyContinue).Count -eq 0) {
    Write-Host '== 前置：target\csharp-ref\tiff 为空 → 调 run_csharp_ref.ps1 =='
    $needRef = $true
}
if ($needRef) {
    & (Join-Path $here 'run_csharp_ref.ps1') -CorpusDir $corpus | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
    if (-not (Test-Path $tiffDir) -or @(Get-ChildItem $tiffDir -Filter *.tif -File -ErrorAction SilentlyContinue).Count -eq 0) {
        Write-Error "corpus\TIFF 仍为空 — 生成失败，请检查 corpus/BMP/001.bmp + 002.bmp 与 dotnet/SimdCodecs 装配"
    }
    if (-not (Test-Path $refOut) -or @(Get-ChildItem $refOut -File -ErrorAction SilentlyContinue).Count -eq 0) {
        Write-Error "target\csharp-ref\tiff 仍为空 — 先跑 pwsh -File Benchmarks\tiff\scripts\run_csharp_ref.ps1"
    }
}

# 1) oracle 闸（debug，字节恒等全量 + tiff_csharp_parity；FAILED 即停）
Write-Host '== oracle gate (cargo test -p kimg_tiff -- --test-threads=1) =='
cargo test -p kimg_tiff -- --test-threads=1 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
if ($LASTEXITCODE -ne 0) { Write-Error 'oracle 闸不过（cargo test 非零），先修字节恒等再对账'; exit 1 }
if (Select-String -Path $log -Pattern 'result: FAILED' -Quiet) {
    Write-Error 'oracle 闸不过（result: FAILED），先修字节恒等再对账'; exit 1
}

# 2) release 构建：gen + probe（精确测量：默认无插桩）
Write-Host '== release build (gen + probe) =='
cargo build --release --manifest-path $genManifest 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
if ($LASTEXITCODE -ne 0) { Write-Error 'gen release 构建失败'; exit 1 }
cargo build --release --manifest-path $probeManifest 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
if ($LASTEXITCODE -ne 0) { Write-Error 'probe release 构建失败'; exit 1 }

if (-not $NoWait) { Write-Host '== 静置 60s（散热/变频稳定）=='; Start-Sleep -Seconds 60 }

# 3) release 探针（ours 单核）
Write-Host '== release probe (ours) =='
$env:KIMG_TIFF_CORPUS = $tiffDir
cargo run --release --manifest-path $probeManifest 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
Remove-Item Env:KIMG_TIFF_CORPUS -ErrorAction SilentlyContinue

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
