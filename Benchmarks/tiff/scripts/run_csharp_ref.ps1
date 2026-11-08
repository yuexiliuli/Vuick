# TIFF C# 参照再生成：csharp_runner/TiffRef `ref <corpus> <refOut>` 计时 + RGBA8 字节参照 dump。
# 前置：corpus\TIFF 缺失或为空 → 先调 run_gen.ps1（Rust gen crate 生成 8bit 矩阵）。
# 产物：Benchmarks\corpus\csharp\tiff_csharp.log（UTF-8 无 BOM，md 表，compare_refs.py 消费）
#      + target\csharp-ref\tiff\<stem>.<w>x<h>.rgba（tiff_csharp_parity 的字节恒等参照）。
# 依赖：.NET 10（%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe）+ Sdcb.SimdCodecs.Tiff 源（SDCB_ROOT）。
# 参数/env：-RunnerProj ($env:TIFF_REF_PROJ)、-CorpusDir ($env:KIMG_TIFF_CORPUS)、-RefOut。
# 缺省：Benchmarks\tiff\csharp_runner\TiffRef.csproj + Benchmarks\corpus + <root>\target\csharp-ref\tiff。
param(
    [string]$RunnerProj,
    [string]$CorpusDir,
    [string]$RefOut,
    [switch]$SkipGen
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Resolve-Path (Join-Path $here '..\..\..')
$suite = Resolve-Path (Join-Path $here '..')

if (-not $RunnerProj) { $RunnerProj = $env:TIFF_REF_PROJ }
if (-not $CorpusDir)  { $CorpusDir  = $env:KIMG_TIFF_CORPUS }
if (-not $RunnerProj) { $RunnerProj = Join-Path $suite 'csharp_runner\TiffRef.csproj' }
if (-not $CorpusDir)  { $CorpusDir  = Join-Path $root 'Benchmarks\corpus' }
if (-not $RefOut)     { $RefOut     = Join-Path $root 'target\csharp-ref\tiff' }

$tiffDir = Join-Path $CorpusDir 'TIFF'
$bmpDir = Join-Path $CorpusDir 'BMP'

$sdcb = $env:SDCB_ROOT
if (-not $sdcb) { $sdcb = Join-Path $root '..\SimdCodecs' }
if (-not (Test-Path $sdcb)) {
    Write-Error "❓ 未找到 Sdcb.SimdCodecs 源（缺省 <repo>\..\SimdCodecs）。请将其克隆/移动到该位置，或设 env SDCB_ROOT 指向真实路径"
    exit 1
}
if (-not (Test-Path $RunnerProj)) { Write-Error "RunnerProj 不存在: $RunnerProj" }
if (-not (Test-Path $bmpDir)) { Write-Error "CorpusDir/BMP 不存在（语料需先装配）: $bmpDir" }

$dotnet = Join-Path $env:LOCALAPPDATA 'Microsoft\dotnet\dotnet.exe'
if (-not (Test-Path $dotnet)) { Write-Error '未找到 dotnet（%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe）' }

# 1) 生成语料（TIFF 缺失或为空时调 run_gen.ps1）。
$tiffEmpty = $true
if (Test-Path $tiffDir) {
    $n = @(Get-ChildItem $tiffDir -Filter *.tif -File -ErrorAction SilentlyContinue).Count
    if ($n -gt 0) { $tiffEmpty = $false }
}
if ($SkipGen -or -not $tiffEmpty) {
    if ($SkipGen) {
        Write-Host '[tiff-ref] -SkipGen，跳过语料生成'
    } else {
        Write-Host "[tiff-ref] TIFF 语料已就位（$n 文件），跳过生成（-SkipGen 可强制跳过；先删目录可强制重生成）"
    }
} else {
    Write-Host '[tiff-ref] 生成 TIFF 语料（run_gen.ps1）...'
    & (Join-Path $here 'run_gen.ps1') -CorpusDir $CorpusDir 2>&1 | ForEach-Object { "$_" }
    if ($LASTEXITCODE -ne 0) { Write-Error 'run_gen.ps1 失败' }
    $n = @(Get-ChildItem $tiffDir -Filter *.tif -File -ErrorAction SilentlyContinue).Count
    if ($n -eq 0) { Write-Error "TIFF 语料仍为空: $tiffDir" }
}

# 2) ref 计时 + RGBA8 dump。
$log = Join-Path $root 'Benchmarks\corpus\csharp\tiff_csharp.log'
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $log) | Out-Null
New-Item -ItemType Directory -Force -Path $RefOut | Out-Null
# 先清空 RefOut（防止旧语料残留累积——对齐 bmp/png 版防混入姿势）。
Get-ChildItem $RefOut -File -ErrorAction SilentlyContinue | Remove-Item -Force

$utf8 = New-Object System.Text.UTF8Encoding($false)
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$header = "# TIFF C# 单核参照（再生成 $stamp；全集暖机 1 遍 + 每文件 warmup2 + 9 轮中位；Sdcb.SimdCodecs.Tiff TiffImageDecoder TryDecodeIntoRgba8）`n# 列 = ms；文件 = ``name`` 反引号包裹；均为 RGBA8 紧密 4B/px（w*h*4）`n"
[System.IO.File]::WriteAllText($log, $header, $utf8)

# stdout 过滤：只留 # 注释行与 | name | ms | 表行（丢弃 dotnet 构建噪声）；UTF-8 无 BOM 追加。
$env:SDCB_ROOT = $sdcb
$env:TIFF_REF_ROUNDS = '9'
$table = & $dotnet run -c Release --project $RunnerProj -- ref $CorpusDir $RefOut 2>&1 |
    ForEach-Object { "$_" } |
    Where-Object { $_ -match '^\s*#|^\s*\|' }
if ($LASTEXITCODE -ne 0) { Write-Error 'TiffRef ref 失败' }
[System.IO.File]::AppendAllText($log, ($table -join "`n") + "`n", $utf8)
Remove-Item Env:SDCB_ROOT -ErrorAction SilentlyContinue
Remove-Item Env:TIFF_REF_ROUNDS -ErrorAction SilentlyContinue

Write-Host "DONE log=$log refOut=$RefOut"
