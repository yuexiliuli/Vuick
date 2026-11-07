# HEIF acceptance 一键对账：Rust release 探针（并行 + KIMG_HEIF_PARALLEL=0 单核）+ C# 表合并 + geomean。
# 用法：pwsh -File Benchmarks\heif\scripts\run_acceptance.ps1
# 输出：logs\acceptance_<时间戳>.log（stdout 双落）；结尾两行主口径。
param(
    [switch]$NoWait # 跳过静置 60s（CI/已有热机态）
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Resolve-Path (Join-Path $here '..\..\..')
$suite = Resolve-Path (Join-Path $here '..')
$logDir = Join-Path $suite 'logs'; New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$log = Join-Path $logDir "acceptance_$stamp.log"

Write-Host "root=$root"; Write-Host "log=$log"
'=' * 20 | Tee-Object -FilePath $log

# 0) 字节恒等闸：全量 oracle（debug）—— 0 失配才继续。
Write-Host "== oracle gate (cargo test -p kimg_heif -- --test-threads=1) =="
cargo test -p kimg_heif -- --test-threads=1 2>&1 | Tee-Object -FilePath $log -Append
$fail = (Select-String -Path $log -Pattern 'result: FAILED' -Quiet)
if ($fail) { Write-Error 'oracle 闸不过，先修字节恒等再对账'; exit 1 }

# 1) release 探针构建
Write-Host "== release build =="
cargo build --release -p kimg_heif --tests 2>&1 | Tee-Object -FilePath $log -Append

if (-not $NoWait) { Write-Host '== 静置 60s（散热/变频稳定）=='; Start-Sleep -Seconds 60 }

$exe = Get-ChildItem (Join-Path $root 'target\release\deps') |
    Where-Object { $_.Name -match '^oracle_perf-.*\.exe$' } | Select-Object -First 1
if (-not $exe) { Write-Error '未找到 oracle_perf 探针 exe' }

# 2) 并行 acceptance
Write-Host "== parallel acceptance =="
& $exe.FullName --exact perf_acceptance_geo --ignored --nocapture --test-threads=1 2>&1 |
    Tee-Object -FilePath $log -Append
# 3) 单核 acceptance（KIMG_HEIF_PARALLEL=0）
Write-Host "== single-core acceptance (KIMG_HEIF_PARALLEL=0) =="
$env:KIMG_HEIF_PARALLEL = '0'
& $exe.FullName --exact perf_acceptance_geo --ignored --nocapture --test-threads=1 2>&1 |
    Tee-Object -FilePath $log -Append
Remove-Item Env:KIMG_HEIF_PARALLEL -ErrorAction SilentlyContinue

# 4) C# 表合并 + geomean 摘要
Write-Host "== compare vs C# =="
$py = Get-Command python -ErrorAction SilentlyContinue
if ($py) {
  python (Join-Path $suite 'compare_refs.py') $log 2>&1 | Tee-Object -FilePath $log -Append
} else {
  Write-Host '(python 缺失，跳过合并表；GEO-SUM 行已在上面)'
}

Write-Host "DONE log=$log"
