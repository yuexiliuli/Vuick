<#
一键打包可分发的 vuick（release 二进制 + media_ffmpeg.dll + FFmpeg 运行库 + pdfium）。

背景：视频解码内核已拆成独立 cdylib（工作区成员 media_ffmpeg，产物 media_ffmpeg.dll），
主 exe 只经 libloading 在打开视频时才「运行时」加载它（PE 导入表里没有 FFmpeg dll）。
因此：
  - exe 缺 FFmpeg 也能正常启动，只在用视频时于窗口内报「缺少视频运行库」；
  - 要让视频功能可用，分发时 exe 必须能在 bin\ffmpeg\ 子目录找到 media_ffmpeg.dll 及其 FFmpeg 运行库
    （运行时候选顺序 = exe 旁 → exe\bin\ffmpeg\，见 crates/media_io/src/bridge.rs）。

分发布局（2026-11-22 起，成品在 dist 根，不再 vuick-win64 子目录）：
  dist\
  ├─ vuick.exe                        （release，非 debug：debug 还依赖 vcruntime140d/ucrtbased 调试 CRT）
  ├─ bin\ffmpeg\                      （视频/音乐解码内核 + 它实际导入的 FFmpeg 运行库）
  │   ├─ media_ffmpeg.dll             （cdylib 构建产物）
  │   └─ avcodec-63.dll / avformat-63.dll / avutil-61.dll / swresample-7.dll / swscale-10.dll（视形态）
  ├─ bin\pdf\pdfium-154.0.8035.0.dll  （pdfium 运行时库，**不再内嵌 exe**，见 crates/pdf_core/build.rs）
  └─ third_party\THIRD_PARTY_NOTICES.md + FFmpeg LGPL 许可原文等（合规声明统一收进该子目录，随分发义务）
  └─ 不含任何 *.lib（导入库仅 build 期用；本脚本装配后断言，防把 FFMPEG_DIR\lib\*.lib 混进分发）

media_ffmpeg.dll 的 FFmpeg 依赖按**实际导入表**自动随带（三态判定，不拷 avdevice/avfilter——
media_ffmpeg Cargo.toml 已剪 device/filter feature → 只带 avcodec/avformat/avutil/swresample/swscale；
与编译期同一 FFmpeg 构建、版本一致不会 ABI 错位）。

媒体分发有两种形态，由 FFMPEG_DIR 指向的构建与 media_ffmpeg.dll 的导入表共同决定（三态自动判定）：
  - B1 shared（默认）：FFMPEG_DIR 指 ffmpeg-slim（bin\ 有 av*.dll），media_ffmpeg.dll
    导入表含 avcodec/avformat/avutil/swresample/swscale 五件 → bin\ffmpeg\ 随带这 5 件；
  - B2 静态（opt-in）：FFMPEG_DIR 指 ffmpeg-slim-static（无 bin\，lib\ 是静态 COFF 归档），
    media_ffmpeg.dll 已静态链入 FFmpeg、导入表无 av/sw → bin\ffmpeg\ 只放 media_ffmpeg.dll 一件。

自动供给（默认开，可 -SkipFfmpegProvision / -SkipPdfiumProvision 关闭）：
  - FFMPEG_DIR 未设置或结构不完整（缺 lib\pkgconfig 或 bin\avcodec-63.dll）→ 调
    scripts/build-slim-ffmpeg.sh（需 MSYS2 UCRT64 bash 环境）自建 B1 瘦包（LGPL，~6MB），
    成功后其 dll 进 bin\ffmpeg\（B2 静态目录有 lib\* 静态归档、无 bin\ 属常态，不触发供给）；
  - bin\pdf\ 缺 pdfium-154.0.8035.0.dll → 先拷 resources\pdfium\ 同名 dll（开发树常有），
    仍缺则从 bblanchon/pdfium-binaries 下载（GitHub API 匹配 x64 release，-PdfiumUrl 可覆盖），
    解压取 pdfium.dll 改名后放 bin\pdf\。

其余依赖（VCRUNTIME140.dll / api-ms-win-crt-* / d3dcompiler_47.dll / icuuc.dll 等）都是
Windows 10/11 系统自带，无需打包；若目标机是精简版 / 旧版 Win10，建议先在本机安装
「Microsoft Visual C++ 2015-2022 Redistributable」再分发。

用法：pwsh scripts/package.ps1 [可选输出目录]
      pwsh scripts/package.ps1 -SkipFfmpegProvision -SkipPdfiumProvision   # 只用现成 FFMPEG_DIR / 不自动供给
诊断：pwsh scripts/package.ps1 -Simulate [-KernelDll <media_ffmpeg.dll 路径>]  # 只判三态，不构建不复制
#>
param(
    [switch]$Simulate,            # 只做「三态判定 + 按需 bin/ 校验」，不构建不复制（打包形态断言用）
    [Parameter(Position = 0)]
    [string]$OutDir,              # 可选输出目录（缺省仓库根 dist；兼容旧用法第一个位置参数）
    [Parameter(Position = 1)]
    [string]$KernelDll,           # -Simulate 被测 media_ffmpeg.dll（缺省 target\release\media_ffmpeg.dll）
    [switch]$SkipFfmpegProvision, # 关闭 FFmpeg 自动供给（FFMPEG_DIR 缺失/不完整时直接报错）
    [switch]$SkipPdfiumProvision, # 关闭 pdfium 自动供给（bin\pdf\ 缺失时仅警告不下载）
    [string]$PdfiumUrl,           # 覆盖 pdfium 下载地址（bblanchon 资产直链或任意 .tgz 直链）
    [string]$FfmpegPrefix,        # 覆盖 build-slim-ffmpeg.sh 的 PREFIX（缺省 $VUICK_DEV_DIR/ffmpeg-slim；未设时 %USERPROFILE%\.vuick-dev\ffmpeg-slim）
    [string]$FfmpegSrc            # 覆盖 build-slim-ffmpeg.sh 的 SRC（FFmpeg 源码树；缺省 $VUICK_DEV_DIR/ffmpeg；未设时 %USERPROFILE%\.vuick-dev\ffmpeg）
)

$ErrorActionPreference = 'Stop'

function ConvertTo-MsysPath {
    # Windows 绝对路径 → MSYS 风格（E:\a\b → /e/a/b），供 msys2 bash 传参
    param([string]$Path)
    if ($Path -match '^([A-Za-z]):\\(.*)$') {
        return ('/' + $Matches[1].ToLower() + '/' + ($Matches[2] -replace '\\', '/'))
    }
    return $Path
}

# ---------- 0) 前置：FFMPEG_DIR（缺/结构不完整时自动供给） ----------
# 结构完整判定：lib\pkgconfig（编译期 pkg-config 必需）+（bin\avcodec-63.dll 为 shared 运行库；
# 或 lib\avcodec.lib / libavcodec.a 为 B2 静态目录）。B2 静态目录无 bin\ 属常态，视为完整、不触发供给。
# -Simulate 只判三态不构建，也不触发供给。
$ffDir = $env:FFMPEG_DIR
$ffDirOk = $false
if ($ffDir -and (Test-Path $ffDir)) {
    $hasPkgConfig = Test-Path (Join-Path $ffDir 'lib\pkgconfig')
    $hasSharedBin = Test-Path (Join-Path $ffDir 'bin\avcodec-63.dll')
    $hasStaticLib = (Test-Path (Join-Path $ffDir 'lib\avcodec.lib')) -or (Test-Path (Join-Path $ffDir 'lib\libavcodec.a'))
    $ffDirOk = $hasPkgConfig -and ($hasSharedBin -or $hasStaticLib)
}
$needFfmpegProvision = (-not $ffDirOk) -and (-not $Simulate) -and (-not $SkipFfmpegProvision)
if ($needFfmpegProvision) {
    Write-Host '[FFmpeg 自动供给] FFMPEG_DIR 缺或结构不完整，调 scripts/build-slim-ffmpeg.sh 自建 B1 瘦包（需 MSYS2 UCRT64 bash）...'
    $scriptPath = Join-Path $PSScriptRoot 'build-slim-ffmpeg.sh'
    $devDir  = if ($env:VUICK_DEV_DIR) { $env:VUICK_DEV_DIR } else { Join-Path $env:USERPROFILE '.vuick-dev' }
    $prefix  = if ($FfmpegPrefix) { $FfmpegPrefix } else { Join-Path $devDir 'ffmpeg-slim' }
    $srcDir  = if ($FfmpegSrc) { $FfmpegSrc } else { Join-Path $devDir 'ffmpeg' }   # build-slim-ffmpeg.sh 默认 SRC（FFmpeg 源码树）
    $msys2 = $null
    # 注意：不要在数组字面量里写 (Join-Path $env:MSYS2_ROOT ...) —— MSYS2_ROOT 未设时
    # Join-Path 抛「Path 为 null」；先拼候选清单再判断，保持空值安全（CI runner 常无此变量）。
    $cands = @('C:\msys64\msys2_shell.cmd', 'C:\Program Files\MSYS2\msys2_shell.cmd')
    if ($env:MSYS2_ROOT) { $cands = @((Join-Path $env:MSYS2_ROOT 'msys2_shell.cmd')) + $cands }
    foreach ($cand in $cands) {
        if ($cand -and (Test-Path $cand)) { $msys2 = $cand; break }
    }
    $msys2BashExists = Test-Path 'C:\msys64\usr\bin\bash.exe'
    if (-not ($msys2 -or $msys2BashExists)) {
        Write-Host @"

[错误] 找不到 MSYS2 bash，无法自动构建 FFmpeg 瘦包。安装指引：
  1. 从 https://www.msys2.org 安装 MSYS2（默认装到 C:\msys64）；
  2. 在 UCRT64 环境装工具链：  pacman -S --needed base-devel mingw-w64-ucrt-x86_64-toolchain
  3. 准备 FFmpeg 源码树到 $srcDir（build-slim-ffmpeg.sh 默认 SRC），
     或直接设好 FFMPEG_DIR 指向已有共享/静态构建后重跑本脚本。
  也可用 -SkipFfmpegProvision 跳过自动供给（此时 FFMPEG_DIR 必须已正确设置）。
"@
        exit 1
    }
    $msysScript = ConvertTo-MsysPath $scriptPath
    $msysPrefix = ConvertTo-MsysPath $prefix
    $msysSrc    = ConvertTo-MsysPath $srcDir
    if ($msys2) {
        Write-Host "   运行 msys2_shell.cmd -ucrt64 -defterm -no-start ... bash $msysScript（SRC=$msysSrc PREFIX=$msysPrefix，耗时较长）"
        & $msys2 -ucrt64 -defterm -no-start -c "bash `"$msysScript`" `"$msysSrc`" `"$msysPrefix`"" 2>&1 | ForEach-Object { Write-Host $_ }
    } else {
        # 仅 bash.exe 兜底（调用方需已处在 UCRT64 环境，正常不走到这里）
        & 'C:\msys64\usr\bin\bash.exe' -c "bash `"$msysScript`" `"$msysSrc`" `"$msysPrefix`"" 2>&1 | ForEach-Object { Write-Host $_ }
    }
    $provisionOk = (Test-Path (Join-Path $prefix 'bin\avcodec-63.dll')) -and (Test-Path (Join-Path $prefix 'lib\pkgconfig'))
    if (-not $provisionOk) {
        Write-Host "[错误] FFmpeg 瘦包自建失败/未产出（缺 bin\avcodec-63.dll 或 lib\pkgconfig）：$prefix"
        Write-Host "       请检查 MSYS2 UCRT64 工具链与 FFmpeg 源码（默认 SRC=$srcDir），或设好 FFMPEG_DIR 后用 -SkipFfmpegProvision 重跑。"
        exit 1
    }
    $env:FFMPEG_DIR = $prefix
    $ffDir = $prefix
    Write-Host "   FFmpeg 供给完成：$prefix"
}

if (-not $ffDir) {
    Write-Host "[错误] 未设置 FFMPEG_DIR（且未走自动供给）。先跑 scripts/setup-ffmpeg.ps1 装环境（B1 指 ffmpeg-slim；B2 静态指 ffmpeg-slim-static），或去掉 -SkipFfmpegProvision 让脚本自建瘦包。"
    exit 1
}
if (-not (Test-Path $ffDir)) {
    Write-Host "[错误] 未找到 FFMPEG_DIR 目录：$ffDir。先跑 scripts/setup-ffmpeg.ps1 装环境。"
    exit 1
}
$ffBin = Join-Path $ffDir 'bin'

# ---------- 0.5) 定位 llvm-objdump（导入表驱动判定依赖它；不可用则回退前缀白名单） ----------
$objdump = $null
foreach ($cand in @(
    'C:\Program Files\LLVM\bin\llvm-objdump.exe',
    (Join-Path $env:LIBCLANG_PATH 'llvm-objdump.exe')
)) {
    if ($cand -and (Test-Path $cand)) { $objdump = $cand; break }
}

# 三态判定：按 media_ffmpeg.dll 的**实际导入表**决定「要拷哪几个 FFmpeg dll」。
#   (a) objdump 可用 且 导入表含 av*/sw* → shared 态：待拷 = 导入表命中名（须 FFMPEG_DIR\bin 可取件）
#   (b) objdump 可用 且 导入表无 av*/sw* → static 态：待拷 = 空（B2 静态，media_ffmpeg.dll 已自含 FFmpeg）
#   (c) objdump 不可用 → fallback 态：按前缀白名单从 FFMPEG_DIR\bin 取（保留既有回退，不并入 (b)）
function Get-FfmpegDecision {
    param([Parameter(Mandatory = $true)][string]$KernelDll)
    $pattern = 'DLL Name: avcodec-.*\.dll|DLL Name: avformat-.*\.dll|DLL Name: avutil-.*\.dll|DLL Name: swscale-.*\.dll|DLL Name: swresample-.*\.dll|DLL Name: avdevice-.*\.dll|DLL Name: avfilter-.*\.dll'
    $needed = @()
    if ($objdump) {
        $needed = @(& $objdump -p $KernelDll 2>$null |
            Select-String -Pattern $pattern |
            ForEach-Object { ($_.ToString().Trim() -replace '^DLL Name:\s*', '') })
        $needBin = ($needed.Count -gt 0)
        $state = if ($needBin) { 'shared' } else { 'static' }
        return [pscustomobject]@{ State = $state; Needed = $needed; NeedBin = $needBin }
    }
    # objdump 不可用 → 前缀白名单回退（与 Cargo.toml 剪枝后的必需集一致）
    if (-not (Test-Path $ffBin)) {
        return [pscustomobject]@{ State = 'fallback'; Needed = @(); NeedBin = $true }
    }
    $needed = @(Get-ChildItem $ffBin -Filter '*.dll' | Where-Object {
        $_.Name -match '^(avcodec|avformat|avutil|swresample|swscale)-' } | ForEach-Object { $_.Name })
    return [pscustomobject]@{ State = 'fallback'; Needed = $needed; NeedBin = $true }
}

# -Simulate：只判定不构建不复制（打包形态断言用；产物用现成 debug/release dll）
if ($Simulate) {
    $kernel = if ($KernelDll) { $KernelDll } else { Join-Path (Get-Location) 'target\release\media_ffmpeg.dll' }
    if (-not (Test-Path $kernel)) {
        Write-Host "[错误] -Simulate 被测 dll 不存在：$kernel"
        exit 1
    }
    $d = Get-FfmpegDecision -KernelDll $kernel
    if ($d.NeedBin -and -not (Test-Path $ffBin)) {
        Write-Host "[错误] 判定为 $($d.State) 态，但 FFMPEG_DIR\bin 不存在：$ffBin"
        exit 1
    }
    Write-Host "Simulate 判定：state=$($d.State)  待拷 $($d.Needed.Count) 件  需 bin=$($d.NeedBin)"
    if ($d.State -eq 'static') {
        Write-Host '[静态单 dll 形态] media_ffmpeg.dll 已自含 FFmpeg，无需额外 dll（bin\ffmpeg\ 仅 media_ffmpeg.dll 一件）。'
    } elseif ($d.Needed.Count -gt 0) {
        Write-Host "               待拷名单（→ bin\ffmpeg\）：$($d.Needed -join ', ')"
    } else {
        Write-Host '[提示] 未找到 llvm-objdump，FFmpeg dll 按前缀白名单复制（avcodec/avformat/avutil/swresample/swscale）。'
    }
    exit 0
}

# ---------- 1) release 构建（exe 与 media_ffmpeg 分开构建，确保 cdylib 一定产出） ----------
Write-Host "[1/3] cargo build --release（exe + media_ffmpeg cdylib）..."
cargo build --release -p vuick
if ($LASTEXITCODE -ne 0) {
    Write-Host "[错误] release exe 构建失败。"
    exit 1
}
cargo build --release -p media_ffmpeg
if ($LASTEXITCODE -ne 0) {
    Write-Host "[错误] media_ffmpeg（cdylib）构建失败。"
    exit 1
}
$exe = Join-Path (Get-Location) 'target\release\vuick.exe'
$kernel = Join-Path (Get-Location) 'target\release\media_ffmpeg.dll'
if (-not (Test-Path $exe)) {
    Write-Host "[错误] 产物不存在：$exe"
    exit 1
}
if (-not (Test-Path $kernel)) {
    Write-Host "[错误] 产物不存在：$kernel（视频解码内核 cdylib 未产出）"
    exit 1
}

# ---------- 2) 组装 dist 目录 ----------
$repoRoot = Split-Path $PSScriptRoot -Parent
$outDir = if ($OutDir) { $OutDir } else { Join-Path (Get-Location) 'dist' }
$ffmpegDir = Join-Path $outDir 'bin\ffmpeg'
$pdfDir = Join-Path $outDir 'bin\pdf'

# 清掉脚本自管的旧产物，防残留旧布局（顶层 media_ffmpeg.dll / 半搬移件 / 旧许可证副本）混入；
# docs-archive 等非脚本管理的目录不动。
Remove-Item (Join-Path $outDir 'bin') -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item (Join-Path $outDir 'vuick.exe') -Force -ErrorAction SilentlyContinue
Remove-Item (Join-Path $outDir 'media_ffmpeg.dll') -Force -ErrorAction SilentlyContinue
Remove-Item (Join-Path $outDir 'THIRD_PARTY_NOTICES.md') -Force -ErrorAction SilentlyContinue
Remove-Item (Join-Path $outDir 'third_party') -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $outDir, $ffmpegDir, $pdfDir | Out-Null

Write-Host "[2/3] 复制 exe -> $outDir\vuick.exe；media_ffmpeg.dll + FFmpeg 运行库 -> $ffmpegDir"
Copy-Item $exe (Join-Path $outDir 'vuick.exe') -Force
Copy-Item $kernel (Join-Path $ffmpegDir 'media_ffmpeg.dll') -Force

# 2b) FFmpeg 运行库：三态判定（Get-FfmpegDecision）按 media_ffmpeg.dll 的实际导入表驱动，只带必需件。
# media_ffmpeg/Cargo.toml 已剪掉 device/filter feature → avdevice/avfilter 不在导入表，
# 自动不再复制（本机 FFMPEG_DIR\bin 全量约 230MB → B1 仅 5 件约 121MB）；今后再改 feature
# 导入表变，重新打包即自适应，无需改本清单。B2 静态态导入表无 av/sw → 只复制媒体内核一件。
$decision = Get-FfmpegDecision -KernelDll $kernel
$ffNeeded = $decision.Needed

# 按需 bin/ 校验：shared/回退态必须能从 FFMPEG_DIR\bin 取件；静态态不要求 bin（B2 无 bin/ 属常态）
if ($decision.NeedBin -and -not (Test-Path $ffBin)) {
    Write-Host "[错误] 判定为 $($decision.State) 态，需要 FFMPEG_DIR\bin 存在：$ffBin"
    Write-Host "       （B2 静态打包时 media_ffmpeg.dll 应无 av/sw 导入且 FFMPEG_DIR 指向静态目录。）"
    exit 1
}

if ($decision.State -eq 'static') {
    Write-Host '[静态单 dll 形态] media_ffmpeg.dll 已自含 FFmpeg，无需额外 dll（bin\ffmpeg\ 仅 media_ffmpeg.dll 一件）。'
    if (-not (Test-Path (Join-Path $ffDir 'lib'))) {
        Write-Host "[提示] FFMPEG_DIR 下未见 lib/（$ffDir）；静态打包本来也不读它，目录仅用于编译期头/库复现，建议保留。"
    }
} else {
    if ($decision.State -eq 'fallback') {
        Write-Host '[提示] 未找到 llvm-objdump，FFmpeg dll 按前缀白名单复制（avcodec/avformat/avutil/swresample/swscale）。'
    }
    $missingSrc = @()
    foreach ($n in $ffNeeded) {
        $src = Join-Path $ffBin $n
        if (-not (Test-Path $src)) { $missingSrc += $n; continue }
        Copy-Item $src (Join-Path $ffmpegDir $n) -Force
    }
    if ($missingSrc.Count -gt 0) {
        Write-Host "[警告] FFMPEG_DIR\bin 缺以下 dll（ABI 版本与编译期不一致？）：$($missingSrc -join ', ')"
    }
    Write-Host "     已复制 media_ffmpeg.dll + $($ffNeeded.Count) 个 FFmpeg dll（$($ffNeeded -join ', ')）-> bin\ffmpeg\"
}

# 2c) 许可证合规文件：随包复制第三方声明 + FFmpeg LGPL 原文（收进 dist\third_party\）。
#     ffmpeg-slim 的 make install 不装 license 文件（构建树全树无 LICENSE/COPYING）→ 文本由
#     仓库 third_party/ vendor 兜底（scripts/build-slim-ffmpeg*.sh 自建 LGPL 构建，见 THIRD_PARTY_NOTICES.md）。
$tpDest = Join-Path $outDir 'third_party'
New-Item -ItemType Directory -Force -Path $tpDest | Out-Null
$notices = Join-Path $repoRoot 'THIRD_PARTY_NOTICES.md'
if (Test-Path $notices) {
    Copy-Item $notices (Join-Path $tpDest 'THIRD_PARTY_NOTICES.md') -Force
} else {
    Write-Host '[警告] 缺 THIRD_PARTY_NOTICES.md，未复制第三方声明'
}
$tpDir = Join-Path $repoRoot 'third_party'
if (Test-Path $tpDir) {
    Copy-Item (Join-Path $tpDir '*') $tpDest -Recurse -Force
} else {
    Write-Host '[警告] 缺 third_party/，未复制 FFmpeg LGPL 许可原文'
}

# 2d) pdfium 运行时库：磁盘分发到 bin\pdf\（不再内嵌 exe）。缺失时自动供给。
Write-Host "[2d] pdfium -> $pdfDir"
$pdfiumName = 'pdfium-154.0.8035.0.dll'
$pdfiumTarget = Join-Path $pdfDir $pdfiumName
if (Test-Path $pdfiumTarget) {
    Write-Host "   pdfium 已存在：$pdfiumTarget"
} elseif ($SkipPdfiumProvision) {
    Write-Host "[警告] 缺 $pdfiumName 且 -SkipPdfiumProvision：PDF 预览运行时不可用（exe 照常启动，打开 PDF 会提示缺库）。"
} else {
    # ① 开发树 resources\pdfium\ 同名 dll（最常见，含正确版本号）
    $resPdf = Join-Path $repoRoot "resources\pdfium\$pdfiumName"
    if (Test-Path $resPdf) {
        Copy-Item $resPdf $pdfiumTarget -Force
        Write-Host "   pdfium：从 resources\pdfium\ 拷贝（$pdfiumName）"
    } else {
        # ② bblanchon/pdfium-binaries 下载（-PdfiumUrl 可覆盖；下载的可能是更新的 rev，改名复用版本名）
        Write-Host '   pdfium：resources\pdfium\ 缺同名 dll，改从 bblanchon/pdfium-binaries 下载...'
        $tmpTgz = Join-Path $env:TEMP ('vuick-pdfium-' + [guid]::NewGuid().ToString('N') + '.tgz')
        $tmpX = Join-Path $env:TEMP ('vuick-pdfium-x-' + [guid]::NewGuid().ToString('N'))
        try {
            $assetUrl = $PdfiumUrl
            $actualRev = if ($PdfiumUrl) { '（-PdfiumUrl 覆盖）' } else { $null }
            if (-not $assetUrl) {
                $headers = @{ 'User-Agent' = 'vuick-package.ps1'; 'Accept' = 'application/vnd.github+json' }
                $rels = Invoke-RestMethod -Uri 'https://api.github.com/repos/bblanchon/pdfium-binaries/releases?per_page=20' -Headers $headers
                # 本仓 pdfium 版本（154.0.8035.0）与 bblanchon chromium/NNNN 无稳定公开映射 →
                # 保守取最新 chromium release 的 x64 资产，打印实际 rev（CDN「latest」语义）。
                $rel = $rels | Where-Object { $_.tag_name -match '^chromium/\d+$' } | Select-Object -First 1
                if (-not $rel) { throw 'GitHub API 未返回 chromium 形 release（网络/限流？）' }
                $asset = $rel.assets | Where-Object { $_.name -like 'pdfium-win-x64*.tgz' } | Select-Object -First 1
                if (-not $asset) { throw "release $($rel.tag_name) 无 pdfium-win-x64*.tgz 资产" }
                $assetUrl = $asset.browser_download_url
                $actualRev = $rel.tag_name
            }
            Write-Host "   下载 $actualRev：$assetUrl"
            Invoke-WebRequest -Uri $assetUrl -OutFile $tmpTgz
            New-Item -ItemType Directory -Force -Path $tmpX | Out-Null
            tar -xzf $tmpTgz -C $tmpX
            $dll = Get-ChildItem $tmpX -Recurse -Filter 'pdfium*.dll' -File | Select-Object -First 1
            if (-not $dll) { throw "解压后未找到 pdfium*.dll（$tmpTgz）" }
            Copy-Item $dll.FullName $pdfiumTarget -Force
            Write-Host "   pdfium：下载成功（实际 rev = $actualRev），改名 $pdfiumName 放 bin\pdf\"
        } catch {
            Write-Host "[警告] pdfium 自动供给失败：$($_.Exception.Message)"
            Write-Host '       可手动下载配套 rev 到 resources\pdfium\ 同名文件后重跑，或用 -PdfiumUrl <tgz 直链> 覆盖。'
        } finally {
            Remove-Item $tmpTgz -Force -ErrorAction SilentlyContinue
            Remove-Item $tmpX -Recurse -Force -ErrorAction SilentlyContinue
        }
        if (-not (Test-Path $pdfiumTarget)) {
            Write-Host '[警告] 最终 bin\pdf\ 缺 pdfium：exe 照常启动，打开 PDF 会提示缺库。'
        }
    }
}

# 2e) 断言：分发目录不得含 *.lib（导入库仅 build 期用；防把 FFMPEG_DIR\lib\*.lib 混进分发）
Write-Host '[2e] 断言 dist 无 *.lib 导入库...'
$libs = @(Get-ChildItem $outDir -Recurse -Filter '*.lib' -File -ErrorAction SilentlyContinue)
if ($libs.Count -gt 0) {
    Write-Host "[错误] 分发目录含 $($libs.Count) 个 *.lib（导入库不应分发）："
    $libs | ForEach-Object { Write-Host "       $($_.FullName)" }
    exit 1
}
Write-Host '      通过：无 *.lib。'

# ---------- 3) 校验 + 收尾 ----------
if (-not (Test-Path $pdfiumTarget)) {
    Write-Host "[警告] bin\pdf\ 缺 $pdfiumName：PDF 预览运行时不可用。"
}
if (-not $objdump) {
    Write-Host "[警告] 未找到 llvm-objdump，跳过 exe/内核导入校验。"
} else {
    # 3a) exe 应【不再】导入任何 FFmpeg dll —— 保证缺 FFmpeg 也能启动
    $exeImports = @(& $objdump -p (Join-Path $outDir 'vuick.exe') 2>$null |
        Select-String -Pattern 'DLL Name: avcodec-.*\.dll|DLL Name: avformat-.*\.dll|DLL Name: swscale-.*\.dll|DLL Name: swresample-.*\.dll|DLL Name: avutil-.*\.dll')
    if ($exeImports) {
        Write-Host "[警告] exe 仍静态导入 FFmpeg：$($exeImports.Line -join ', ')。缺库时 exe 无法启动，未达预期。"
    } else {
        Write-Host "[3/3] 校验通过：exe 无 FFmpeg 静态导入（缺库也能启动，视频缺库走窗口内报错）。"
    }
    # 3b) 内核导入的 FFmpeg dll 应全部在包内（与 2b 同一 $ffNeeded 清单，落点 bin\ffmpeg\）
    $missingPkg = @($ffNeeded | Where-Object { -not (Test-Path (Join-Path $ffmpegDir $_)) })
    if ($missingPkg.Count -gt 0) {
        Write-Host "[警告] media_ffmpeg.dll 引用的 dll 未在包内：$($missingPkg -join ', ')"
    } else {
        Write-Host "     校验通过：media_ffmpeg.dll 所需 FFmpeg dll（$($ffNeeded.Count) 件）已全部随包带上（bin\ffmpeg\）。"
    }
}

$totalMB = [math]::Round(((Get-ChildItem $outDir -Recurse -File | Measure-Object Length -Sum).Sum / 1MB), 1)
Write-Host @"

完成。可分发目录：$outDir（共 $totalMB MB）
  - vuick.exe（dist 根）
  - bin\ffmpeg\（media_ffmpeg.dll + FFmpeg 运行库）
  - bin\pdf\（pdfium-154.0.8035.0.dll）
发给人时整个目录/打成 zip 一起给。
缺 FFmpeg / pdfium 时 exe 仍能启动：不开视频/PDF 照常使用，对应功能会窗口内提示缺库。
"@
