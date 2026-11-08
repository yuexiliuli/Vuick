# TIFF 语料生成：编译并运行 Benchmarks/tiff/gen（release）→ 生成 Benchmarks\corpus\TIFF 8bit 矩阵。
# 前置：Benchmarks\corpus\BMP\001.bmp + 002.bmp（photo 源）。
# 产物：Benchmarks\corpus\TIFF\*.tif（gitignore 不入库；覆盖式重生成，幂等）。
# 参数/env：-CorpusDir ($env:KIMG_TIFF_CORPUS) —— 指向 corpus 根（其下 BMP/、TIFF/）。
# 缺省：Benchmarks\corpus。
param(
    [string]$CorpusDir
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Resolve-Path (Join-Path $here '..\..\..')
$suite = Resolve-Path (Join-Path $here '..')

if (-not $CorpusDir) { $CorpusDir = $env:KIMG_TIFF_CORPUS }
if (-not $CorpusDir) { $CorpusDir = Join-Path $root 'Benchmarks\corpus' }
$tiffDir = Join-Path $CorpusDir 'TIFF'
$bmpDir = Join-Path $CorpusDir 'BMP'

if (-not (Test-Path (Join-Path $bmpDir '001.bmp')) -or -not (Test-Path (Join-Path $bmpDir '002.bmp'))) {
    Write-Error "photo 源缺失: $bmpDir\001.bmp / 002.bmp（需先装配 Benchmarks\corpus\BMP）"
}

$genManifest = Join-Path $suite 'gen\Cargo.toml'
Write-Host '[tiff-gen] release build gen...'
cargo build --release --manifest-path $genManifest 2>&1 | ForEach-Object { "$_" }
if ($LASTEXITCODE -ne 0) { Write-Error 'gen release 构建失败' }

Write-Host "[tiff-gen] 生成语料 -> $tiffDir ..."
cargo run --release --manifest-path $genManifest -- --out $tiffDir 2>&1 | ForEach-Object { "$_" }
if ($LASTEXITCODE -ne 0) { Write-Error 'gen 运行失败' }

Write-Host "DONE tiffDir=$tiffDir"
