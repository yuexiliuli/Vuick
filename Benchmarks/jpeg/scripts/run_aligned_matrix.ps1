# turbo 端对齐测量矩阵（2026-11-07；2026-11-08 语料切 446 主线）：probe(tj3-rgba / tj2-rgb) × C#(rgba / rgb) × 语料(446 = Sdcb 官方矩阵)。
# 目标：对齐到 Sdcb 官方基准口径（tj3 + RGBA32 输出 + 广谱）后，C#/turbo 从历史 1.076 往哪走、离 Sdcb 0.82 差多少。
# 注：旧双语料（A=19 baseline / B=B2 广谱 114，jpeg_broad 已废）角色由 446 全量替代，矩阵改为
# 单语料（-CorpusA 指向新集合即可复用）两 turbo 口径直接跑；对齐口径（tj3-rgba vs C#-rgba、
# tj2-rgb vs C#-rgb）不变。
#
# 用法：pwsh -File Benchmarks\jpeg\scripts\run_aligned_matrix.ps1 [-NoSettle] [-CorpusA <dir>]
# - 长命令一律前台阻塞到结束（K86，禁后台化）。
# - 产物：logs/aligned_matrix_<ts>.log（UTF-8 无 BOM）＋每 cell 独立日志（logs/aligned_*_<ts>_*.log）。
# - 探针 env：KIMG_JPEG_TURBO_ROUNDS=9；C# env: JPEG_REF_ROUNDS=9。
param(
    [switch]$NoSettle,        # 跳过静置（CI/已有热机态）
    [string]$CorpusA          # 缺省 Benchmarks\corpus\JPEG（446 = Sdcb 官方矩阵；env KIMG_JPEG_CORPUS 可覆盖）
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Resolve-Path (Join-Path $here '..\..\..')
$suite = Resolve-Path (Join-Path $here '..')
$logDir = Join-Path $suite 'logs'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
if (-not $CorpusA) { $CorpusA = Join-Path $root 'Benchmarks\corpus\JPEG' }

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$log = Join-Path $logDir "aligned_matrix_$stamp.log"
$utf8 = New-Object System.Text.UTF8Encoding($false)
$probeManifest = Join-Path $suite 'probe\Cargo.toml'
$runnerProj = Join-Path $suite 'csharp_runner\JpegRef.csproj'
$dotnet = Join-Path $env:LOCALAPPDATA 'Microsoft\dotnet\dotnet.exe'
$py = Get-Command python -ErrorAction SilentlyContinue

function Write-Log { param([string]$m) $m | Tee-Object -FilePath $log -Append }

Write-Log "== turbo 端对齐测量 $stamp =="
Write-Log "root=$root corpusA=$CorpusA settle=$(-not $NoSettle)"

if (-not (Test-Path $CorpusA)) { Write-Error "CorpusA 不存在: $CorpusA" }

# 0) release 探针构建（一次，两 cell 复用）
Write-Log '== release probe build =='
cargo build --release --manifest-path $probeManifest 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append
if ($LASTEXITCODE -ne 0) { Write-Error '探针 release 构建失败'; exit 1 }

$env:KIMG_JPEG_TURBO_ROUNDS = '9'
$env:JPEG_REF_ROUNDS = '9'
$corpora = @(@{Label='A'; Dir=$CorpusA})

# 1) C# 参照（语料 rgb + rgba 两轮；rgba 只计时不 dump；过滤 # 注释行与 | name | ms | 表）
$csvcLogs = @{}
foreach ($c in $corpora) {
    foreach ($m in @('rgb','rgba')) {
        $dumpDir = Join-Path $root "target\csharp-ref-aligned-$($c.Label)"
        $probeArgs = @('run','-c','Release','--project',$runnerProj,'--','ref',$c.Dir,$dumpDir)
        if ($m -eq 'rgba') { $probeArgs += '--rgba' }
        $clog = Join-Path $logDir "aligned_csharp_$($c.Label)_$m`_$stamp.log"
        Write-Log "== C# ref $($c.Label)/$m =="
        $table = & $dotnet @probeArgs 2>&1 | ForEach-Object { "$_" } | Where-Object { $_ -match '^\s*#|^\s*\|' }
        [System.IO.File]::WriteAllLines($clog, $table, $utf8)
        $csvcLogs["$($c.Label):$m"] = $clog
        Write-Log "  log=$clog rows=$($table.Count)"
    }
}

# 2) probe（每语料 × 每 turbo 口径，同窗交错；静置后开跑）
$probeLogs = @{}
$first = $true
foreach ($c in $corpora) {
    foreach ($m in @('tj2-rgb','tj3-rgba')) {
        if (-not $NoSettle) {
            $sec = if ($first) { 60 } else { 20 }
            Write-Log "== 静置 ${sec}s =="
            if ($sec -gt 0) { Start-Sleep -Seconds $sec }
        }
        $first = $false
        $plog = Join-Path $logDir "aligned_probe_$($c.Label)_$($m.Replace('-','_'))_$stamp.log"
        Write-Log "== probe $($c.Label)/$m =="
        $env:KIMG_JPEG_TURBO_MODE = $m
        $out = cargo run --release --manifest-path $probeManifest -- --corpus $c.Dir 2>&1 |
            ForEach-Object { "$_" } | Where-Object { $_ -match 'KIMG_JPEG_VS_TURBO' }
        Remove-Item Env:KIMG_JPEG_TURBO_MODE -ErrorAction SilentlyContinue
        [System.IO.File]::WriteAllLines($plog, $out, $utf8)
        $probeLogs["$($c.Label):$m"] = $plog
        Write-Log "  log=$plog rows=$($out.Count) last=$($out[-1])"
    }
}

# 3) 合成 cell 表：probe(<turbo 口径>) × C#(<对应输出格式>)
Write-Log ''
Write-Log '== 矩阵合成 =='
$env:PYTHONUTF8 = '1'
$matrix = @(
    '| 语料 | turbo_mode | C#/turbo | ours/turbo | ours/C# |',
    '|---|---|---|---|---|'
)
$cells = @(
    @{Label='A';  Name='A 446矩阵';    Tm='tj2-rgb';   Cm='rgb'},
    @{Label='A';  Name='A 446矩阵';    Tm='tj3-rgba';  Cm='rgba'}
)
foreach ($cell in $cells) {
    $plog = $probeLogs["$($cell.Label):$($cell.Tm)"]
    $clog = $csvcLogs["$($cell.Label):$($cell.Cm)"]
    Write-Log "== cell $($cell.Name) / $($cell.Tm)  (probe=$plog csharp=$clog) =="
    $out = python (Join-Path $suite 'compare_refs.py') $plog $clog 2>&1 | ForEach-Object { "$_" }
    foreach ($l in $out) { Write-Log $l }
    $line = $out | Where-Object { $_ -match '^\s*geomean ours/turbo' } | Select-Object -First 1
    $m = [regex]::Match($line, 'geomean ours/turbo = ([\d.]+)\s+geomean ours/C# = ([\d.]+)\s+geomean C#/turbo = ([\d.]+)')
    if ($m.Success) {
        $matrix += "| $($cell.Name) | $($cell.Tm) | $($m.Groups[3].Value) | $($m.Groups[1].Value) | $($m.Groups[2].Value) |"
    } else {
        $matrix += "| $($cell.Name) | $($cell.Tm) | (解析失败) | | |"
    }
}
Remove-Item Env:PYTHONUTF8 -ErrorAction SilentlyContinue
foreach ($row in $matrix) { Write-Log $row }

# 4) UTF-8 无 BOM 归一
$text = Get-Content -Path $log -Raw -Encoding utf8
[System.IO.File]::WriteAllText($log, $text, $utf8)
Write-Log "DONE log=$log"
Write-Host ''
Write-Host '== 矩阵结果 =='
$matrix | ForEach-Object { Write-Host $_ }
