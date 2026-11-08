# PNG C# 单核参照再生成：csharp_runner/PngRef `ref <corpus> <refOut>` 计时 + RGBA8 字节参照 dump。
# 产物：Benchmarks\corpus\csharp\png_csharp.log（UTF-8 无 BOM，md 表，compare_refs.py 消费）
#      + target\csharp-ref\png\<stem>.<w>x<h>.rgba（png_csharp_parity 的字节恒等参照；png 专用子目录）。
# 依赖：.NET 10（%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe）+ Sdcb.SimdCodecs.Png 源（SDCB_ROOT）。
# 参数/env：-RunnerProj ($env:PNG_REF_PROJ)、-CorpusDir ($env:KIMG_PNG_CORPUS)、-RefOut。
# 缺省：Benchmarks\png\csharp_runner\PngRef.csproj + Benchmarks\corpus\PNG + <root>\target\csharp-ref。
param(
    [string]$RunnerProj,
    [string]$CorpusDir,
    [string]$RefOut
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Resolve-Path (Join-Path $here '..\..\..')
$suite = Resolve-Path (Join-Path $here '..')

if (-not $RunnerProj) { $RunnerProj = $env:PNG_REF_PROJ }
if (-not $CorpusDir)  { $CorpusDir  = $env:KIMG_PNG_CORPUS }
if (-not $RunnerProj) { $RunnerProj = Join-Path $suite 'csharp_runner\PngRef.csproj' }
if (-not $CorpusDir)  { $CorpusDir  = Join-Path $root 'Benchmarks\corpus\PNG' }
if (-not $RefOut)     { $RefOut     = Join-Path $root 'target\csharp-ref\png' }

$sdcb = $env:SDCB_ROOT
if (-not $sdcb) { $sdcb = Join-Path $root '..\SimdCodecs' }
if (-not (Test-Path $sdcb)) {
    Write-Error "❓ 未找到 Sdcb.SimdCodecs 源（缺省 <repo>\..\SimdCodecs）。请将其克隆/移动到该位置，或设 env SDCB_ROOT 指向真实路径"
    exit 1
}
if (-not (Test-Path $RunnerProj)) { Write-Error "RunnerProj 不存在: $RunnerProj" }
if (-not (Test-Path $CorpusDir))  { Write-Error "CorpusDir 不存在: $CorpusDir" }

$dotnet = Join-Path $env:LOCALAPPDATA 'Microsoft\dotnet\dotnet.exe'
if (-not (Test-Path $dotnet)) { Write-Error '未找到 dotnet（%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe）' }

$log = Join-Path $root 'Benchmarks\corpus\csharp\png_csharp.log'
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $log) | Out-Null
New-Item -ItemType Directory -Force -Path $RefOut | Out-Null
# 先清空 RefOut（防止旧语料残留累积——对齐 jpeg 版防混入姿势）
Get-ChildItem $RefOut -File -ErrorAction SilentlyContinue | Remove-Item -Force

$utf8 = New-Object System.Text.UTF8Encoding($false)
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$header = "# PNG C# 单核参照（再生成 $stamp；全集暖机 1 遍 + 每文件 warmup2 + 9 轮中位；Sdcb.SimdCodecs.Png TryDecodeIntoRgba8）`n# 列 = ms；文件 = ``name`` 反引号包裹；均为 RGBA8 紧密 4B/px（w*h*4）`n"
[System.IO.File]::WriteAllText($log, $header, $utf8)

# stdout 过滤：只留 # 注释行与 | name | ms | 表行（丢弃 dotnet 构建噪声）；UTF-8 无 BOM 追加。
$env:SDCB_ROOT = $sdcb
$env:PNG_REF_ROUNDS = '9'
$table = & $dotnet run -c Release --project $RunnerProj -- ref $CorpusDir $RefOut 2>&1 |
    ForEach-Object { "$_" } |
    Where-Object { $_ -match '^\s*#|^\s*\|' }
[System.IO.File]::AppendAllText($log, ($table -join "`n") + "`n", $utf8)
Remove-Item Env:SDCB_ROOT -ErrorAction SilentlyContinue
Remove-Item Env:PNG_REF_ROUNDS -ErrorAction SilentlyContinue

Write-Host "DONE log=$log refOut=$RefOut"
