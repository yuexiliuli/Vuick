# BMP C# 参照再生成：csharp_runner/BmpRef `ref <corpus> <refOut>` 计时 + RGBA8 字节参照 dump。
# 前置：若 corpus\BMP-MATRIX 缺失或为空 → 先 BmpRef `gen`（photo 源 001/002 + 合成 screen
#       + singles 的 p2/444 全覆盖矩阵；hand-rolled 低层写入器在 Program.cs 内）。
# 产物：Benchmarks\corpus\csharp\bmp_csharp.log（UTF-8 无 BOM，md 表，compare_refs.py 消费）
#      + target\csharp-ref\bmp\<stem>.<w>x<h>.rgba（bmp_csharp_parity 的字节恒等参照）。
# 依赖：.NET 10（%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe）+ Sdcb.SimdCodecs.Bmp 源（SDCB_ROOT）。
# 参数/env：-RunnerProj ($env:BMP_REF_PROJ)、-CorpusDir ($env:KIMG_BMP_CORPUS)、-RefOut。
# 缺省：Benchmarks\bmp\csharp_runner\BmpRef.csproj + Benchmarks\corpus + <root>\target\csharp-ref\bmp。
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

if (-not $RunnerProj) { $RunnerProj = $env:BMP_REF_PROJ }
if (-not $CorpusDir)  { $CorpusDir  = $env:KIMG_BMP_CORPUS }
if (-not $RunnerProj) { $RunnerProj = Join-Path $suite 'csharp_runner\BmpRef.csproj' }
if (-not $CorpusDir)  { $CorpusDir  = Join-Path $root 'Benchmarks\corpus' }
if (-not $RefOut)     { $RefOut     = Join-Path $root 'target\csharp-ref\bmp' }

$matrixDir = Join-Path $CorpusDir 'BMP-MATRIX'

$sdcb = $env:SDCB_ROOT
if (-not $sdcb) { $sdcb = Join-Path $root '..\SimdCodecs' }
if (-not (Test-Path $sdcb)) {
    Write-Error "❓ 未找到 Sdcb.SimdCodecs 源（缺省 <repo>\..\SimdCodecs）。请将其克隆/移动到该位置，或设 env SDCB_ROOT 指向真实路径"
    exit 1
}
if (-not (Test-Path $RunnerProj)) { Write-Error "RunnerProj 不存在: $RunnerProj" }
if (-not (Test-Path (Join-Path $CorpusDir 'BMP'))) { Write-Error "CorpusDir/BMP 不存在: $CorpusDir" }

$dotnet = Join-Path $env:LOCALAPPDATA 'Microsoft\dotnet\dotnet.exe'
if (-not (Test-Path $dotnet)) { Write-Error '未找到 dotnet（%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe）' }

# 1) 生成矩阵语料（BMP-MATRIX 缺失或为空时）。
$matrixEmpty = $true
if (Test-Path $matrixDir) {
    $n = @(Get-ChildItem $matrixDir -Filter *.bmp -File -ErrorAction SilentlyContinue).Count
    if ($n -gt 0) { $matrixEmpty = $false }
}
if ($SkipGen -or -not $matrixEmpty) {
    if ($SkipGen) {
        Write-Host "[bmp-ref] -SkipGen，跳过矩阵生成"
    } else {
        Write-Host "[bmp-ref] BMP-MATRIX 已就位（$n 文件），跳过生成（-SkipGen 可强制跳过；先删目录可强制重生成）"
    }
} else {
    Write-Host '[bmp-ref] 生成 BMP-MATRIX 矩阵语料...'
    $env:SDCB_ROOT = $sdcb
    & $dotnet run -c Release --project $RunnerProj -- gen $CorpusDir $matrixDir 2>&1 | ForEach-Object { "$_" }
    if ($LASTEXITCODE -ne 0) { Write-Error 'BmpRef gen 失败' }
    Remove-Item Env:SDCB_ROOT -ErrorAction SilentlyContinue
}

# 2) ref 计时 + RGBA8 dump。
$log = Join-Path $root 'Benchmarks\corpus\csharp\bmp_csharp.log'
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $log) | Out-Null
New-Item -ItemType Directory -Force -Path $RefOut | Out-Null
# 先清空 RefOut（防止旧语料残留累积——对齐 png 版防混入姿势）
Get-ChildItem $RefOut -File -ErrorAction SilentlyContinue | Remove-Item -Force

$utf8 = New-Object System.Text.UTF8Encoding($false)
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$header = "# BMP C# 单核参照（再生成 $stamp；全集暖机 1 遍 + 每文件 warmup2 + 9 轮中位；Sdcb.SimdCodecs.Bmp BmpImageDecoder TryDecodeIntoRgba8）`n# 列 = ms；文件 = ``name`` 反引号包裹；均为 RGBA8 紧密 4B/px（w*h*4）`n"
[System.IO.File]::WriteAllText($log, $header, $utf8)

# stdout 过滤：只留 # 注释行与 | name | ms | 表行（丢弃 dotnet 构建噪声）；UTF-8 无 BOM 追加。
$env:SDCB_ROOT = $sdcb
$env:BMP_REF_ROUNDS = '9'
$table = & $dotnet run -c Release --project $RunnerProj -- ref $CorpusDir $RefOut 2>&1 |
    ForEach-Object { "$_" } |
    Where-Object { $_ -match '^\s*#|^\s*\|' }
if ($LASTEXITCODE -ne 0) { Write-Error 'BmpRef ref 失败' }
[System.IO.File]::AppendAllText($log, ($table -join "`n") + "`n", $utf8)
Remove-Item Env:SDCB_ROOT -ErrorAction SilentlyContinue
Remove-Item Env:BMP_REF_ROUNDS -ErrorAction SilentlyContinue

Write-Host "DONE log=$log refOut=$RefOut"
