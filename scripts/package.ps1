<#
一键打包可分发的 vuick（release 二进制 + media_ffmpeg.dll + FFmpeg 运行库）。

背景：视频解码内核已拆成独立 cdylib（工作区成员 media_ffmpeg，产物 media_ffmpeg.dll），
主 exe 只经 libloading 在打开视频时才「运行时」加载它（PE 导入表里没有 FFmpeg dll）。
因此：
  - exe 缺 FFmpeg 也能正常启动，只在用视频时于窗口内报「缺少视频运行库」；
  - 要让视频功能可用，分发时 exe 必须与 media_ffmpeg.dll 及 FFmpeg 运行库同目录。

本脚本产出 dist\vuick-win64\：
  - vuick.exe（release，非 debug：debug 还依赖 vcruntime140d/ucrtbased 调试 CRT）
  - media_ffmpeg.dll（视频解码内核，本仓库 cdylib 构建产物）
  - media_ffmpeg.dll **实际导入**的 FFmpeg dll（导入表驱动自动随必需集；media_ffmpeg
    Cargo.toml 已剪 device/filter feature → 只带 avcodec/avformat/avutil/swresample/swscale，
    不再拷 avdevice/avfilter。与编译期同一 FFmpeg 构建、版本一致不会 ABI 错位）
  - THIRD_PARTY_NOTICES.md + third_party/（第三方合规声明与 FFmpeg LGPL 许可原文，LGPL 随分发义务）

其余依赖（VCRUNTIME140.dll / api-ms-win-crt-* / d3dcompiler_47.dll / icuuc.dll 等）都是
Windows 10/11 系统自带，无需打包；若目标机是精简版 / 旧版 Win10，建议先在本机安装
「Microsoft Visual C++ 2015-2022 Redistributable」再分发。

媒体分发有两种形态，由 FFMPEG_DIR 指向的构建与 media_ffmpeg.dll 的导入表共同决定（三态自动判定）：
  - B1 shared（默认）：FFMPEG_DIR 指 ffmpeg-slim（bin\ 有 av*.dll），media_ffmpeg.dll
    导入表含 avcodec/avformat/avutil/swresample/swscale 五件 → 打包随带这 5 件；
  - B2 静态（opt-in）：FFMPEG_DIR 指 ffmpeg-slim-static（无 bin\，lib\ 是静态 COFF 归档），
    media_ffmpeg.dll 已静态链入 FFmpeg、导入表无 av/sw → 打包不拷 FFmpeg dll（dist = exe + media_ffmpeg.dll 两件）。

用法：pwsh scripts/package.ps1 [可选输出目录]
诊断：pwsh scripts/package.ps1 -Simulate [-KernelDll <media_ffmpeg.dll 路径>]  # 只判三态，不构建不复制
#>
param(
    [switch]$Simulate,            # 只做「三态判定 + 按需 bin/ 校验」，不构建不复制（打包形态断言用）
    [Parameter(Position = 0)]
    [string]$OutDir,              # 可选输出目录（兼容旧用法第一个位置参数）
    [Parameter(Position = 1)]
    [string]$KernelDll            # -Simulate 被测 media_ffmpeg.dll（缺省 target\release\media_ffmpeg.dll）
)

$ErrorActionPreference = 'Stop'

# ---------- 0) 前置：FFMPEG_DIR ----------
# 只校验「已设置 + 目录存在」；bin/ 是否必需由 2b 三态判定后**按需**校验
# （B1 shared / objdump 回退白名单需要 bin\；B2 静态无 bin\ 属常态，不在此强求）。
$ffDir = $env:FFMPEG_DIR
if (-not $ffDir) {
    Write-Host "[错误] 未设置 FFMPEG_DIR。先跑 scripts/setup-ffmpeg.ps1 装环境（B1 指 ffmpeg-slim；B2 静态指 ffmpeg-slim-static）。"
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
        Write-Host '[静态单 dll 形态] media_ffmpeg.dll 已自含 FFmpeg，无需额外 dll（dist = exe + media_ffmpeg.dll 两件）。'
    } elseif ($d.Needed.Count -gt 0) {
        Write-Host "               待拷名单：$($d.Needed -join ', ')"
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
$outDir = if ($OutDir) { $OutDir } else { Join-Path (Get-Location) 'dist\vuick-win64' }
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

Write-Host "[2/3] 复制 exe + media_ffmpeg.dll + FFmpeg 运行库 -> $outDir"
Copy-Item $exe (Join-Path $outDir 'vuick.exe') -Force
Copy-Item $kernel (Join-Path $outDir 'media_ffmpeg.dll') -Force

# 2b) FFmpeg 运行库：三态判定（Get-FfmpegDecision）按 media_ffmpeg.dll 的实际导入表驱动，只带必需件。
# media_ffmpeg/Cargo.toml 已剪掉 device/filter feature → avdevice/avfilter 不在导入表，
# 自动不再复制（本机 FFMPEG_DIR\bin 全量约 230MB → B1 仅 5 件约 121MB）；今后再改 feature
# 导入表变，重新打包即自适应，无需改本清单。B2 静态态导入表无 av/sw → 什么都不拷。
$decision = Get-FfmpegDecision -KernelDll $kernel
$ffNeeded = $decision.Needed

# 按需 bin/ 校验：shared/回退态必须能从 FFMPEG_DIR\bin 取件；静态态不要求 bin（B2 无 bin/ 属常态）
if ($decision.NeedBin -and -not (Test-Path $ffBin)) {
    Write-Host "[错误] 判定为 $($decision.State) 态，需要 FFMPEG_DIR\bin 存在：$ffBin"
    Write-Host "       （B2 静态打包时 media_ffmpeg.dll 应无 av/sw 导入且 FFMPEG_DIR 指向静态目录。）"
    exit 1
}

if ($decision.State -eq 'static') {
    Write-Host '[静态单 dll 形态] media_ffmpeg.dll 已自含 FFmpeg，无需额外 dll（dist = exe + media_ffmpeg.dll 两件）。'
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
        Copy-Item $src (Join-Path $outDir $n) -Force
    }
    if ($missingSrc.Count -gt 0) {
        Write-Host "[警告] FFMPEG_DIR\bin 缺以下 dll（ABI 版本与编译期不一致？）：$($missingSrc -join ', ')"
    }
    Write-Host "     已复制 media_ffmpeg.dll + $($ffNeeded.Count) 个 FFmpeg dll（$($ffNeeded -join ', ')）"
}

# 2c) 许可证合规文件：随包复制第三方声明 + FFmpeg LGPL 原文。
#     ffmpeg-slim 的 make install 不装 license 文件（构建树全树无 LICENSE/COPYING）→ 文本由
#     仓库 third_party/ vendor 兜底（scripts/build-slim-ffmpeg*.sh 自建 LGPL 构建，见 THIRD_PARTY_NOTICES.md）。
$repoRoot = Split-Path $PSScriptRoot -Parent
$notices = Join-Path $repoRoot 'THIRD_PARTY_NOTICES.md'
if (Test-Path $notices) {
    Copy-Item $notices (Join-Path $outDir 'THIRD_PARTY_NOTICES.md') -Force
} else {
    Write-Host '[警告] 缺 THIRD_PARTY_NOTICES.md，未复制第三方声明'
}
$tpDir = Join-Path $repoRoot 'third_party'
if (Test-Path $tpDir) {
    New-Item -ItemType Directory -Force -Path (Join-Path $outDir 'third_party') | Out-Null
    Copy-Item (Join-Path $tpDir '*') (Join-Path $outDir 'third_party') -Recurse -Force
} else {
    Write-Host '[警告] 缺 third_party/，未复制 FFmpeg LGPL 许可原文'
}

# ---------- 3) 校验 + 收尾 ----------
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
    # 3b) 内核导入的 FFmpeg dll 应全部在包内（与 2b 同一 $ffNeeded 清单）
    $missingPkg = @($ffNeeded | Where-Object { -not (Test-Path (Join-Path $outDir $_)) })
    if ($missingPkg.Count -gt 0) {
        Write-Host "[警告] media_ffmpeg.dll 引用的 dll 未在包内：$($missingPkg -join ', ')"
    } else {
        Write-Host "     校验通过：media_ffmpeg.dll 所需 FFmpeg dll（$($ffNeeded.Count) 件）已全部随包带上。"
    }
}

$totalMB = [math]::Round(((Get-ChildItem $outDir | Measure-Object Length -Sum).Sum / 1MB), 1)
Write-Host @"

完成。可分发目录：$outDir（共 $totalMB MB）
发给人时整个目录/打成 zip 一起给（exe 必须和这些 dll 同目录）。
缺 FFmpeg 时 exe 仍能启动：不开视频照常使用，开视频会窗口内提示缺库。
"@
