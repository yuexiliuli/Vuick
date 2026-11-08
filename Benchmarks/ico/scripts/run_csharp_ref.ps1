# ICO C# 参照再生成：csharp_runner/IcoRef `gen <bmp> <ico>`（源 = BMP 001/002 + 合成 screen →
# ICO 语料，先清空防混）+ `ref <corpus> <refOut>` 计时 + Best 条目 RGBA8 dump。
# 产物：Benchmarks\corpus\csharp\ico_csharp.log（UTF-8 无 BOM，md 表，compare_refs.py 消费）
#      + target\csharp-ref\ico\<stem>.<w>x<h>.rgba（ico_csharp_parity 的字节恒等参照）。
# 依赖：.NET 10（%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe）+ Sdcb.SimdCodecs 源（SDCB_ROOT，Ico+Bmp）。
# 参数/env：-RunnerProj ($env:ICO_REF_PROJ)、-CorpusDir ($env:KIMG_ICO_CORPUS)、-RefOut、-SkipGen。
# 缺省：Benchmarks\ico\csharp_runner\IcoRef.csproj + Benchmarks\corpus + <root>\target\csharp-ref\ico。
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

if (-not $RunnerProj) { $RunnerProj = $env:ICO_REF_PROJ }
if (-not $CorpusDir)  { $CorpusDir  = $env:KIMG_ICO_CORPUS }
if (-not $RunnerProj) { $RunnerProj = Join-Path $suite 'csharp_runner\IcoRef.csproj' }
if (-not $CorpusDir)  { $CorpusDir  = Join-Path $root 'Benchmarks\corpus' }
if (-not $RefOut)     { $RefOut     = Join-Path $root 'target\csharp-ref\ico' }

$icoDir = Join-Path $CorpusDir 'ICO'
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

# 1) 生成语料（gen 恒跑以「先清空防混」、防外部大 ICO 残留；-SkipGen 强制跳过）。
if ($SkipGen) {
    Write-Host '[ico-ref] -SkipGen，跳过语料生成'
} else {
    Write-Host "[ico-ref] 生成 ICO 语料（先清空 $icoDir 防混）..."
    & $dotnet run -c Release --project $RunnerProj -- gen $bmpDir $icoDir 2>&1 | ForEach-Object { "$_" }
    if ($LASTEXITCODE -ne 0) { Write-Error 'IcoRef gen 失败' }
    $n = @(Get-ChildItem $icoDir -File -ErrorAction SilentlyContinue | Where-Object { $_.Extension -in '.ico', '.cur' }).Count
    if ($n -eq 0) { Write-Error "ICO 语料仍为空: $icoDir" }
    Write-Host "[ico-ref] 语料就位（$n 文件）"
}

# 2) ref 计时 + Best 条目 RGBA8 dump。
$log = Join-Path $root 'Benchmarks\corpus\csharp\ico_csharp.log'
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $log) | Out-Null
New-Item -ItemType Directory -Force -Path $RefOut | Out-Null
# 先清空 RefOut（防止旧语料残留累积——对齐 bmp/png/gif/tiff 版防混入姿势）。
Get-ChildItem $RefOut -File -ErrorAction SilentlyContinue | Remove-Item -Force

$utf8 = New-Object System.Text.UTF8Encoding($false)
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$header = "# ICO C# 单核参照（再生成 $stamp；全集暖机 1 遍 + 每文件 warmup2 + 9 轮中位；" +
    "Sdcb.SimdCodecs.Ico IcoDecoder.Decode → Best 条目 → RGBA8）`n" +
    "# 口径 = Best 条目解码总墙钟（读目录 + 选 Best + 解码）；列 = ms；文件 = ``name`` 反引号包裹；均为 RGBA8 紧密 4B/px`n"
[System.IO.File]::WriteAllText($log, $header, $utf8)

# stdout 过滤：只留 # 注释行与 | name | ms | 表行（丢弃 dotnet 构建噪声）；UTF-8 无 BOM 追加。
$env:SDCB_ROOT = $sdcb
$env:ICO_REF_ROUNDS = '9'
$table = & $dotnet run -c Release --project $RunnerProj -- ref $CorpusDir $RefOut 2>&1 |
    ForEach-Object { "$_" } |
    Where-Object { $_ -match '^\s*#|^\s*\|' }
if ($LASTEXITCODE -ne 0) { Write-Error 'IcoRef ref 失败' }
[System.IO.File]::AppendAllText($log, ($table -join "`n") + "`n", $utf8)
Remove-Item Env:SDCB_ROOT -ErrorAction SilentlyContinue
Remove-Item Env:ICO_REF_ROUNDS -ErrorAction SilentlyContinue

Write-Host "DONE log=$log refOut=$RefOut"
