# 一键生成 Sdcb 官方 446 JPEG 基准语料（守望jpeg 战役用）→ 复制进本仓 Benchmarks/corpus/JPEG/。
# 幂等：源图已存在且 SHA 匹配即跳过；矩阵已存在即跳过 prepare；平铺命名已在则跳过。
# 用法：pwsh -File Benchmarks\jpeg\scripts\setup_jpeg_corpus.ps1 [-SimdCodecsRoot <dir>] [-ForceReDownload] [-ForcePrepare] [-ForceFlatten]
# 依赖：dotnet（10.x，%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe）、System32 curl、python、网络（Wikimedia ~440MB，慢/不稳时用代理或 VPN 重试）。
param(
    [string]$SimdCodecsRoot = $(if ($env:SDCB_ROOT) { $env:SDCB_ROOT } else { 'E:\Rust\SimdCodecs' }),
    [switch]$ForceReDownload,
    [switch]$ForcePrepare,
    [switch]$ForceFlatten
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Resolve-Path (Join-Path $here '..\..\..')
$suite = Resolve-Path (Join-Path $here '..')

$benchDir = Join-Path $SimdCodecsRoot 'benchmarks\Sdcb.SimdCodecs.Jpeg.Benchmarks'
$manifest = Join-Path $benchDir 'corpus\manifest.tsv'
$originals = Join-Path $benchDir '.cache\corpus\originals'
$matrix = Join-Path $benchDir '.cache\corpus\matrix'
$turboDll = Join-Path $benchDir '.cache\libjpeg-turbo\win-x64\turbojpeg.dll'
$dst = Join-Path $root 'Benchmarks\corpus\JPEG'
$dotnet = Join-Path $env:LOCALAPPDATA 'Microsoft\dotnet\dotnet.exe'

if (-not (Test-Path $manifest)) { Write-Error "manifest 不存在: $manifest（SimdCodecsRoot 对吗？）"; exit 1 }
Write-Host "SimdCodecsRoot=$SimdCodecsRoot"
Write-Host "manifest ok（条目=$((Get-Content $manifest | Where-Object { $_ -and -not $_.StartsWith('#') -and -not $_.StartsWith('name`t') }).Count)）"

# ---------- 1) turbojpeg.dll：SimdCodecs .cache 缺 → 从本仓已就位 DLL 复制 ----------
if (-not (Test-Path $turboDll)) {
    $localDll = Join-Path $suite 'bin\turbojpeg.dll'
    if (Test-Path $localDll) {
        New-Item -ItemType Directory -Force -Path (Split-Path $turboDll) | Out-Null
        Copy-Item $localDll $turboDll -Force
        Write-Host "[tj] 已从本仓复制 → $turboDll"
    } else {
        Write-Error "turbojpeg.dll 本仓也没有: $localDll — 先安置 Benchmarks\jpeg\bin\turbojpeg.dll（官方 vc-x64 3.2.0）"; exit 1
    }
} else { Write-Host "[tj] turbojpeg.dll 已就位" }

# ---------- 2) 源图下载（curl + SHA256 校验，幂等） ----------
New-Item -ItemType Directory -Force -Path $originals | Out-Null
$ua = 'Sdcb.SimdCodecs-bench-corpus/0.1 (https://github.com/sdcb/SimdCodecs)'
$fail = 0
foreach ($line in Get-Content $manifest | Where-Object { $_ -and -not $_.StartsWith('#') -and -not $_.StartsWith('name`t') }) {
    $f = $line -split "`t"
    if ($f[0] -eq 'name') { continue }
    $name = $f[0]; $sha = $f[5]; $url = $f[6]
    $out = Join-Path $originals "$name.jpg"
    if (Test-Path $out) {
        $h = (Get-FileHash -LiteralPath $out -Algorithm SHA256).Hash
        if ($h -eq $sha) { Write-Host "[dl] skip $name"; continue }
        Write-Host "[dl] sha mismatch $name → 重下"; Remove-Item $out -Force
    }
    Write-Host "[dl] downloading $name"
    & curl.exe -fsSL --retry 12 --retry-delay 5 --retry-all-errors -C - -A $ua $url -o $out
    if ($LASTEXITCODE -ne 0) { Write-Host "[dl] FAIL $name rc=$LASTEXITCODE"; $fail++; continue }
    $h = (Get-FileHash -LiteralPath $out -Algorithm SHA256).Hash
    if ($h -ne $sha) { Write-Host "[dl] SHA MISMATCH $name"; $fail++ } else { Write-Host "[dl] ok $name ($([math]::Round((Get-Item $out).Length/1MB,1))MB)" }
}
if ($fail -ne 0) { Write-Error "源图下载失败 $fail 个（网络不稳可重跑，幂等续下）"; exit 1 }
Write-Host "[dl] 源图 $((Get-ChildItem $originals -Filter *.jpg).Count)/16 ✓"

# ---------- 3) dotnet prepare 生成 446 矩阵（幂等：index.tsv 存在则跳过） ----------
if ((-not (Test-Path (Join-Path $matrix 'index.tsv'))) -or $ForcePrepare) {
    Write-Host "[prepare] 运行 dotnet prepare（生成矩阵，首次几分钟）..."
    & $dotnet run -c Release --project (Join-Path $benchDir 'Sdcb.SimdCodecs.Jpeg.Benchmarks.csproj') -- prepare | Select-Object -Last 5
    if ($LASTEXITCODE -ne 0) { Write-Error "dotnet prepare 失败"; exit 1 }
} else { Write-Host "[prepare] 矩阵已存在" }
$idxRows = (Get-Content (Join-Path $matrix 'index.tsv') | Select-Object -Skip 1).Count
if ($idxRows -ne 446) { Write-Error "矩阵 index 行数异常: $idxRows（期望 446）"; exit 1 }
Write-Host "[prepare] 矩阵 $idxRows 文件 ✓"

# ---------- 4) 平铺复制 → 本仓 Benchmarks\corpus\JPEG ----------
Write-Host "[flatten] 平铺复制 group__image.jpg → $dst"
$count = & python -c "import csv,os,shutil; src=r'$matrix'; dst=r'$dst'; os.makedirs(dst,exist_ok=True); rows=list(csv.DictReader(open(os.path.join(src,'index.tsv'),encoding='utf-8'),delimiter='\t')); n=sum(1 for r in rows if (lambda id_: shutil.copy2(os.path.join(src,*id_.split('/'))+'.jpg', os.path.join(dst,id_.split('/')[0]+'__'+id_.split('/')[1]+'.jpg')) or True)(r['id'])); print('copied',n)"
if ($LASTEXITCODE -ne 0) { Write-Error "flatten 失败"; exit 1 }
$flatCount = (Get-ChildItem $dst -Filter *.jpg).Count
Write-Host "[flatten] $flatCount/446 ✓ dst=$dst"

Write-Host "DONE — 语料就位。下一会话如需 C# 参照：pwsh -File $here\run_csharp_ref.ps1"
