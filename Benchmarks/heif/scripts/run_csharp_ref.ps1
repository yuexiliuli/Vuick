# C# 单核参照再生成：quick(reps9, main8|IMG_ 过滤 17 文件) → Benchmarks\corpus\csharp\heif_csharp_quick.log；
# ablate(reps9, 5 夹具) → Benchmarks\corpus\csharp\heif_csharp_ablate.log。
# 依赖：.NET 10 + heif_oracle 临时工程（HeifOracle.csproj，见 crates/kimg_heif tests 顶注装配法；仓外自备）。
# 参数/env：-OracleProj ($env:HEIF_ORACLE_PROJ)、-CorpusDir ($env:HEIF_CORPUS_DIR)。
# 缺省：%TEMP%\kilo\heif_oracle\HeifOracle.csproj + Benchmarks\corpus\heif。
param(
    [string]$OracleProj,
    [string]$CorpusDir
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$suite = Resolve-Path (Join-Path $here '..')
$root = Resolve-Path (Join-Path $here '..\..\..')

if (-not $OracleProj) { $OracleProj = $env:HEIF_ORACLE_PROJ }
if (-not $CorpusDir)  { $CorpusDir  = $env:HEIF_CORPUS_DIR }
if (-not $OracleProj) {
    $def = Join-Path $env:TEMP 'kilo\heif_oracle\HeifOracle.csproj'
    if (Test-Path $def) { $OracleProj = $def } else { Write-Error '缺 -OracleProj（heif_oracle 工程）' }
}
if (-not $CorpusDir)  { $CorpusDir  = Join-Path $root 'Benchmarks\corpus\heif' }
if (-not (Test-Path $OracleProj)) { Write-Error "OracleProj 不存在: $OracleProj" }
if (-not (Test-Path $CorpusDir))  { Write-Error "CorpusDir 不存在: $CorpusDir" }

$dotnet = Join-Path $env:LOCALAPPDATA 'Microsoft\dotnet\dotnet.exe'
if (-not (Test-Path $dotnet)) { Write-Error '未找到 dotnet' }

$outQuick = Join-Path $root 'Benchmarks\corpus\csharp\heif_csharp_quick.log'
$outAblate = Join-Path $root 'Benchmarks\corpus\csharp\heif_csharp_ablate.log'

Write-Host "dotnet quick -> $outQuick"
"# HEIF C# 单核 quick 参照（再生成 $stamp；reps=9 中位；列 = stage_ms / RGBA_ms）" |
    Set-Content -Path $outQuick -Encoding utf8
& $dotnet run -c Release --project $OracleProj -- quick $CorpusDir 9 'main8|IMG_' 2>&1 |
    Select-String -NotMatch '^#' | Select-Object -Skip 1 | Add-Content -Path $outQuick -Encoding utf8

Write-Host "dotnet ablate -> $outAblate"
"# HEIF C# 单核 ablate（再生成 $stamp；reps=9；7 case 消融）" |
    Set-Content -Path $outAblate -Encoding utf8
& $dotnet run -c Release --project $OracleProj -- ablate $CorpusDir 9 2>&1 |
    Select-String -Pattern 'testsrc2|fuji|IMG_0083|main-3072|main10-3072' |
    Add-Content -Path $outAblate -Encoding utf8

Write-Host 'DONE'
