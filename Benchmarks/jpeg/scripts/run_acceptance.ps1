# JPEG acceptance 一键对账：oracle 闸（debug 字节恒等）→ release 探针（ours vs turbo）→ compare_refs 三路 geomean。
# 用法：pwsh -File Benchmarks\jpeg\scripts\run_acceptance.ps1   （-NoWait 跳过静置 60s）
# 前置：target\csharp-ref 非空（先跑 run_csharp_ref.ps1）+ bin\turbojpeg.dll 就位。
# 输出：logs\acceptance_<时间戳>.log（stdout 双落，UTF-8 无 BOM）；结尾三路主口径。
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

$corpus = Join-Path $root 'Benchmarks\corpus\JPEG'
$turboDll = Join-Path $suite 'bin\turbojpeg.dll'
$probeManifest = Join-Path $suite 'probe\Cargo.toml'
$csharpLog = Join-Path $root 'Benchmarks\corpus\csharp\jpeg_csharp.log'
$refOut = Join-Path $root 'target\csharp-ref\jpeg'

Write-Host "root=$root"; Write-Host "log=$log"
'=' * 20 | Tee-Object -FilePath $log

# 0) 前置检查
if (-not (Test-Path $corpus))   { Write-Error "corpus 不存在: $corpus" }
# 0a) 参照 DLL 自动装配（turbojpeg.dll：跨机缺失时从 MSYS2 自动下载；已就位 → no-op）
if (-not (Test-Path $turboDll)) {
    & (Join-Path $here 'ensure_refs.ps1')
    if (-not (Test-Path $turboDll)) {
        Write-Error "turbojpeg.dll 缺失且自动装配失败：$turboDll — 手动跑 pwsh -File $here\ensure_refs.ps1（下载源见其固定版本表）"
    }
}
if (-not (Test-Path $refOut) -or @(Get-ChildItem $refOut -File -ErrorAction SilentlyContinue).Count -eq 0) {
    Write-Error "target\csharp-ref\jpeg 为空 — 先跑 pwsh -File Benchmarks\jpeg\scripts\run_csharp_ref.ps1"
}

# 1) oracle 闸（debug，字节恒等全量；FAILED 即停）
Write-Host '== oracle gate (cargo test -p kimg_jpeg -- --test-threads=1) =='
$env:KIMG_JPEG_CORPUS = $corpus
cargo test -p kimg_jpeg -- --test-threads=1 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
if ($LASTEXITCODE -ne 0) { Write-Error 'oracle 闸不过（cargo test 非零），先修字节恒等再对账'; exit 1 }
if (Select-String -Path $log -Pattern 'result: FAILED' -Quiet) {
    Write-Error 'oracle 闸不过（result: FAILED），先修字节恒等再对账'; exit 1
}

# 2) release 探针构建
Write-Host '== release probe build =='
cargo build --release --manifest-path $probeManifest 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
if ($LASTEXITCODE -ne 0) { Write-Error '探针 release 构建失败'; exit 1 }

if (-not $NoWait) { Write-Host '== 静置 60s（散热/变频稳定）=='; Start-Sleep -Seconds 60 }

# 3) release 探针（ours vs turbo；长命令前台阻塞到结束）
Write-Host '== release probe (ours vs turbo) =='
$env:KIMG_JPEG_CORPUS = $corpus
$env:KIMG_JPEG_TURBO_DLL = $turboDll
cargo run --release --manifest-path $probeManifest 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
Remove-Item Env:KIMG_JPEG_TURBO_DLL -ErrorAction SilentlyContinue

# 4) C# 表合并 + 三路 geomean
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