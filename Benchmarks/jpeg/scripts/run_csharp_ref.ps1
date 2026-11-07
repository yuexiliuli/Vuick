# JPEG C# 单核参照再生成：csharp_runner/JpegRef `ref <corpus> <refOut>` 计时 + 字节参照 dump。
# 产物：Benchmarks\corpus\csharp\jpeg_csharp.log（UTF-8 无 BOM，md 表，compare_refs.py 消费）
#      + target\csharp-ref\<stem>.<w>x<h>.{rgb|gray}（jpeg_csharp_parity 的字节恒等参照）。
# 依赖：.NET 10（%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe）+ Sdcb.SimdCodecs.Jpeg 源（SDCB_ROOT）。
# 参数/env：-RunnerProj ($env:JPEG_REF_PROJ)、-CorpusDir ($env:KIMG_JPEG_CORPUS)、-RefOut。
# 缺省：Benchmarks\jpeg\csharp_runner\JpegRef.csproj + Benchmarks\corpus\JPEG + <root>\target\csharp-ref。
param(
    [string]$RunnerProj,
    [string]$CorpusDir,
    [string]$RefOut
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Resolve-Path (Join-Path $here '..\..\..')
$suite = Resolve-Path (Join-Path $here '..')

if (-not $RunnerProj) { $RunnerProj = $env:JPEG_REF_PROJ }
if (-not $CorpusDir)  { $CorpusDir  = $env:KIMG_JPEG_CORPUS }
if (-not $RunnerProj) { $RunnerProj = Join-Path $suite 'csharp_runner\JpegRef.csproj' }
if (-not $CorpusDir)  { $CorpusDir  = Join-Path $root 'Benchmarks\corpus\JPEG' }
if (-not $RefOut)     { $RefOut     = Join-Path $root 'target\csharp-ref' }

$sdcb = $env:SDCB_ROOT
if (-not $sdcb) { $sdcb = 'E:\Rust\SimdCodecs' }
if (-not (Test-Path $sdcb))       { Write-Error "SDCB_ROOT 不存在: $sdcb（env SDCB_ROOT 可覆盖）" }
if (-not (Test-Path $RunnerProj)) { Write-Error "RunnerProj 不存在: $RunnerProj" }
if (-not (Test-Path $CorpusDir))  { Write-Error "CorpusDir 不存在: $CorpusDir" }

$dotnet = Join-Path $env:LOCALAPPDATA 'Microsoft\dotnet\dotnet.exe'
if (-not (Test-Path $dotnet)) { Write-Error '未找到 dotnet（%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe）' }

$log = Join-Path $root 'Benchmarks\corpus\csharp\jpeg_csharp.log'
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $log) | Out-Null
New-Item -ItemType Directory -Force -Path $RefOut | Out-Null
# 先清空 RefOut（防止旧语料残留累积——上轮曾 133 个文件混入 446 集）
Get-ChildItem $RefOut -File -ErrorAction SilentlyContinue | Remove-Item -Force

$utf8 = New-Object System.Text.UTF8Encoding($false)
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$header = "# JPEG C# 单核参照（再生成 $stamp；warmup1+9 轮中位；Sdcb.SimdCodecs.Jpeg DecodeTo）`n# 列 = ms；文件 = ``name`` 反引号包裹`n"
[System.IO.File]::WriteAllText($log, $header, $utf8)

# stdout 过滤：只留 # 注释行与 | name | ms | 表行（丢弃 dotnet 构建噪声）；UTF-8 无 BOM 追加。
$env:SDCB_ROOT = $sdcb
$table = & $dotnet run -c Release --project $RunnerProj -- ref $CorpusDir $RefOut 2>&1 |
    ForEach-Object { "$_" } |
    Where-Object { $_ -match '^\s*#|^\s*\|' }
[System.IO.File]::AppendAllText($log, ($table -join "`n") + "`n", $utf8)
Remove-Item Env:SDCB_ROOT -ErrorAction SilentlyContinue

Write-Host "DONE log=$log refOut=$RefOut"