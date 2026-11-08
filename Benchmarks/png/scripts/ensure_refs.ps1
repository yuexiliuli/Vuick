# ensure_refs.ps1 — PNG 套件参照 DLL 自动装配（跨机器/全新 clone 用）。
# 缺失 bin/libpng16.dll 或 bin/zlib1.dll 时，从 MSYS2 官方打包下载固定版本（SHA-256 钉死），
# 解压 .zst → 抽取 DLL → 校验 → 就位。幂等：bin 已就位 → 直接 no-op 返回（不重下、不覆盖本机已验证 DLL）。
#
# 固定版本表（MSYS2 mingw-w64 官方打包，勿改 URL/SHA；版本升级须重新对账）：
#   mingw-w64-x86_64-libpng-1.6.59-2   -> mingw64/bin/libpng16-16.dll -> bin/libpng16.dll （探针 --libpng 指向）
#   mingw-w64-x86_64-zlib-1.4-2        -> mingw64/bin/zlib1.dll       -> bin/zlib1.dll    （libpng16 依赖，ALTERED 路径旁置解析）
# 依赖关系（PE 导入表实证）：libpng16.dll 仅依赖 KERNEL32/msvcrt + zlib1.dll；zlib1.dll 仅依赖 KERNEL32/msvcrt。
# 探针（Benchmarks/png/libpng_probe）用 LOAD_WITH_ALTERED_SEARCH_PATH 从 DLL 自身目录解析 zlib → 两 DLL 同放 bin/ 即可，无需 PATH。
#
# 用法：
#   pwsh -File Benchmarks\png\scripts\ensure_refs.ps1            # 默认装进 Benchmarks\png\bin
#   pwsh -File ...\ensure_refs.ps1 -BinDir <临时目录>            # 装到临时目录（验证用，绝不碰本机 bin）
#   pwsh -File ...\ensure_refs.ps1 -Force                        # 强制重装（会覆盖既有 DLL，慎用）
param(
    [string]$BinDir,      # 缺省 = <suite>/bin
    [string]$CacheDir,    # 缺省 = %TEMP%\kilo\ensure-refs-cache（下载 + 解压中间态）
    [switch]$Force
)
$ErrorActionPreference = 'Stop'
$here   = Split-Path -Parent $MyInvocation.MyCommand.Path
$suite  = Split-Path -Parent $here
if (-not $BinDir)   { $BinDir   = Join-Path $suite 'bin' }
if (-not $CacheDir) { $CacheDir = Join-Path $env:TEMP 'kilo\ensure-refs-cache' }

# ① 固定版本表：URL(SHA-256) → 包内路径 → bin 名
$Pkgs = @(
    @{
        Pkg   = 'libpng-1.6.59-2'
        Url   = 'https://mirror.msys2.org/mingw/mingw64/mingw-w64-x86_64-libpng-1.6.59-2-any.pkg.tar.zst'
        Sha   = '8cf4e17ed9f592d23a5d1f1f8875c6a8da42eaf7772a997908565e94131cfc20'
        Files = @(
            @{ In = 'mingw64/bin/libpng16-16.dll'; Out = 'libpng16.dll' }
        )
    },
    @{
        Pkg   = 'zlib-1.4-2'
        Url   = 'https://mirror.msys2.org/mingw/mingw64/mingw-w64-x86_64-zlib-1.4-2-any.pkg.tar.zst'
        Sha   = '2a065fb3311d6c08bd2c7431a092b912ade08a83954474e06ea4eb0479d908cd'
        Files = @(
            @{ In = 'mingw64/bin/zlib1.dll'; Out = 'zlib1.dll' }
        )
    }
)

function Find-PythonWithZstd {
    # 找一个装了 python zstandard 的解释器（本机 anaconda 有；顺带 probe 常用位置）。
    $cands = @()
    $g = Get-Command python -ErrorAction SilentlyContinue
    if ($g) { $cands += $g.Source }
    $g = Get-Command py -ErrorAction SilentlyContinue
    if ($g) { $cands += $g.Source }
    foreach ($p in @(
        "$env:USERPROFILE\anaconda3\python.exe",
        "$env:USERPROFILE\miniconda3\python.exe",
        "$env:LOCALAPPDATA\Programs\Python\Python312\python.exe",
        "$env:LOCALAPPDATA\Programs\Python\Python311\python.exe"
    )) { if (Test-Path $p) { $cands += $p } }
    foreach ($c in $cands) {
        try { & $c -c 'import zstandard' 2>$null; if ($LASTEXITCODE -eq 0) { return $c } } catch { }
    }
    return $null
}

function Get-ZstdExe {
    # 下载官方 zstd 命令行 win64 zip（GitHub facebook/zstd releases/latest），解出 zstd.exe，缓存复用。
    $zdir = Join-Path $CacheDir 'zstd-win64'
    $zexe = Join-Path $zdir 'zstd.exe'
    if (Test-Path $zexe) { return $zexe }
    try {
        $rel = Invoke-RestMethod -Uri 'https://api.github.com/repos/facebook/zstd/releases/latest' -Headers @{ 'User-Agent' = 'kilo-ensure-refs/1.0' } -TimeoutSec 30
        $asset = $rel.assets | Where-Object { $_.name -match 'win64\.zip$' } | Select-Object -First 1
        if (-not $asset) { return $null }
        $zip = Join-Path $CacheDir $asset.name
        if (-not (Test-Path $zip)) {
            Write-Host "[extract] 下载官方 zstd: $($asset.name)"
            Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $zip -UseBasicParsing -TimeoutSec 60
        }
        Expand-Archive -LiteralPath $zip -DestinationPath $zdir -Force
        if (Test-Path $zexe) { return $zexe }
    } catch { return $null }
    return $null
}

function Expand-ZstTar {
    # 把 MSYS2 的 .pkg.tar.zst 解到 OutDir（顶部是 mingw64/）。策略链逐级降级：
    # ① bsdtar 直解（tar 自带 zstd 支持则一步成）→ ② 7z → ③ python zstandard → ④ 官方 zstd.exe。
    # 本机 bsdtar 不能直接解 .zst（"unable to run zstd"）；无 7z；anaconda python zstandard 可走 ③。
    param([string]$Zst, [string]$OutDir)
    $probe = Join-Path $OutDir 'mingw64'
    # ①
    try {
        tar -xf $Zst -C $OutDir 2>$null
        if ($LASTEXITCODE -eq 0 -and (Test-Path $probe)) { Write-Host '[extract] ① bsdtar(自带 zstd) 成功'; return }
    } catch { }
    # ② 7z（PATH 或常见安装目录）
    $seven = Get-Command 7z -ErrorAction SilentlyContinue
    if (-not $seven) {
        foreach ($c in @('C:\Program Files\7-Zip\7z.exe', 'C:\Program Files (x86)\7-Zip\7z.exe')) { if (Test-Path $c) { $seven = Get-Item $c; break } }
    }
    if ($seven) {
        & $seven.Source x -y "-o$OutDir" $Zst 2>&1 | Out-Null
        if (Test-Path $probe) { Write-Host '[extract] ② 7z 成功'; return }
    }
    # ③ python zstandard：zst → tar → bsdtar -xf
    $py = Find-PythonWithZstd
    if ($py) {
        $tar = Join-Path $OutDir ((Split-Path -Leaf $Zst) -replace '\.zst$', '.tar')
        & $py -c "import zstandard,sys; d=zstandard.ZstdDecompressor(); d.copy_stream(open(sys.argv[1],'rb'),open(sys.argv[2],'wb'))" $Zst $tar
        if ($LASTEXITCODE -eq 0) {
            tar -xf $tar -C $OutDir 2>$null
            if ($LASTEXITCODE -eq 0 -and (Test-Path $probe)) { Write-Host "[extract] ③ python zstandard 成功（$py）"; return }
        }
    }
    # ④ 官方 zstd.exe：zst → tar → bsdtar -xf
    $zstd = Get-ZstdExe
    if ($zstd) {
        $tar = Join-Path $OutDir ((Split-Path -Leaf $Zst) -replace '\.zst$', '.tar')
        & $zstd -q -d $Zst -o $tar
        if ($LASTEXITCODE -eq 0) {
            tar -xf $tar -C $OutDir 2>$null
            if ($LASTEXITCODE -eq 0 -and (Test-Path $probe)) { Write-Host '[extract] ④ 官方 zstd.exe 成功'; return }
        }
    }
    throw "解压 .zst 失败（所有策略均不可用）：$Zst"
}

# ---------- no-op 判定：所有目标 DLL 已就位且未 -Force → 直接返回 ----------
$allPresent = $true
foreach ($p in $Pkgs) { foreach ($f in $p.Files) { if (-not (Test-Path (Join-Path $BinDir $f.Out))) { $allPresent = $false } } }
if ($allPresent -and -not $Force) {
    Write-Host '[ensure-refs] bin/ 下参照 DLL 已全部就位 → no-op（不重下、不覆盖）'
    exit 0
}

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
New-Item -ItemType Directory -Force -Path $CacheDir | Out-Null

$need = @()
foreach ($p in $Pkgs) {
    $missing = @($p.Files | Where-Object { -not (Test-Path (Join-Path $BinDir $_.Out)) })
    if ($Force -or $missing.Count -gt 0) { $need += $p }
}

foreach ($p in $need) {
    $leaf  = Split-Path -Leaf $p.Url
    $zst   = Join-Path $CacheDir $leaf
    # 1) 下载（缓存命中且 SHA 对 → 跳过下载）
    if (Test-Path $zst) {
        $h = (Get-FileHash -LiteralPath $zst -Algorithm SHA256).Hash.ToLower()
        if ($h -ne $p.Sha) { Write-Warning "[ensure-refs] 缓存 $leaf SHA 不符 → 重下"; Remove-Item $zst -Force }
    }
    if (-not (Test-Path $zst)) {
        Write-Host "[ensure-refs] 下载 $($p.Pkg): $($p.Url)"
        & curl.exe -fsSL --retry 8 --retry-delay 3 --retry-all-errors -o $zst $p.Url
        if ($LASTEXITCODE -ne 0) {
            throw "下载失败（rc=$LASTEXITCODE）：$($p.Url)`n可手动下载后放进 $CacheDir 再重跑，或检查网络/代理。"
        }
    }
    $h = (Get-FileHash -LiteralPath $zst -Algorithm SHA256).Hash.ToLower()
    if ($h -ne $p.Sha) {
        throw "SHA-256 校验失败：$($p.Url)`n  实得 $h`n  期望 $($p.Sha)`n镜像可能被污染或改版，停止装配（勿盲用）。"
    }
    # 2) 解压（包名隔离目录，抽取后即弃）
    $extractRoot = Join-Path $CacheDir ("extract-" + $p.Pkg)
    if (Test-Path $extractRoot) { Remove-Item $extractRoot -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $extractRoot | Out-Null
    Expand-ZstTar -Zst $zst -OutDir $extractRoot
    # 3) 抽取 DLL → bin
    foreach ($f in $p.Files) {
        $src = Join-Path $extractRoot $f.In
        if (-not (Test-Path $src)) { throw "$($p.Pkg) 包内缺少 $($f.In)（包结构变了？新版请重新对账版本表）" }
        $dst = Join-Path $BinDir $f.Out
        Copy-Item -LiteralPath $src -Destination $dst -Force
        $fi = Get-Item $dst
        if ($fi.Length -le 0) { throw "抽取产物为空：$($f.Out)" }
        Write-Host "[ensure-refs] 就位 $($f.Out) ($($fi.Length) B) <- $($p.Pkg)/$($f.In)"
    }
    Remove-Item $extractRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "[ensure-refs] 完成 → $BinDir"
Write-Host ("             " + ((Get-ChildItem $BinDir -Filter *.dll -ErrorAction SilentlyContinue | ForEach-Object { $_.Name }) -join ', '))
