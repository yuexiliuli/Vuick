<#
一键打包可分发的 gpui-editor（release 二进制 + media_ffmpeg.dll + FFmpeg 运行库）。

背景：视频解码内核已拆成独立 cdylib（工作区成员 media-ffmpeg，产物 media_ffmpeg.dll），
主 exe 只经 libloading 在打开视频时才「运行时」加载它（PE 导入表里没有 FFmpeg dll）。
因此：
  - exe 缺 FFmpeg 也能正常启动，只在用视频时于窗口内报「缺少视频运行库」；
  - 要让视频功能可用，分发时 exe 必须与 media_ffmpeg.dll 及 FFmpeg 运行库同目录。

本脚本产出 dist\gpui-editor-win64\：
  - gpui-editor.exe（release，非 debug：debug 还依赖 vcruntime140d/ucrtbased 调试 CRT）
  - media_ffmpeg.dll（视频解码内核，本仓库 cdylib 构建产物）
  - media_ffmpeg.dll **实际导入**的 FFmpeg dll（导入表驱动自动随必需集；media-ffmpeg
    Cargo.toml 已剪 device/filter feature → 只带 avcodec/avformat/avutil/swresample/swscale，
    不再拷 avdevice/avfilter。与编译期同一 FFmpeg 构建、版本一致不会 ABI 错位）

其余依赖（VCRUNTIME140.dll / api-ms-win-crt-* / d3dcompiler_47.dll / icuuc.dll 等）都是
Windows 10/11 系统自带，无需打包；若目标机是精简版 / 旧版 Win10，建议先在本机安装
「Microsoft Visual C++ 2015-2022 Redistributable」再分发。

用法：pwsh scripts/package.ps1 [可选输出目录]
#>

$ErrorActionPreference = 'Stop'

# ---------- 0) 前置：FFMPEG_DIR ----------
$ffDir = $env:FFMPEG_DIR
if (-not $ffDir -or -not (Test-Path (Join-Path $ffDir 'bin'))) {
    Write-Host "[错误] 未找到 FFMPEG_DIR（或缺 bin/）。先跑 scripts/setup-ffmpeg.ps1 装环境。"
    exit 1
}
$ffBin = Join-Path $ffDir 'bin'

# ---------- 1) release 构建（exe 与 media-ffmpeg 分开构建，确保 cdylib 一定产出） ----------
Write-Host "[1/3] cargo build --release（exe + media-ffmpeg cdylib）..."
cargo build --release -p gpui-editor
if ($LASTEXITCODE -ne 0) {
    Write-Host "[错误] release exe 构建失败。"
    exit 1
}
cargo build --release -p media-ffmpeg
if ($LASTEXITCODE -ne 0) {
    Write-Host "[错误] media-ffmpeg（cdylib）构建失败。"
    exit 1
}
$exe = Join-Path (Get-Location) 'target\release\gpui-editor.exe'
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
$outDir = if ($args.Count -gt 0) { $args[0] } else { Join-Path (Get-Location) 'dist\gpui-editor-win64' }
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

# 定位 llvm-objdump（默认 LLVM 安装位 + LIBCLANG_PATH 兜底；导入表驱动的必需件清单依赖它）
$objdump = $null
foreach ($cand in @(
    'C:\Program Files\LLVM\bin\llvm-objdump.exe',
    (Join-Path $env:LIBCLANG_PATH 'llvm-objdump.exe')
)) {
    if ($cand -and (Test-Path $cand)) { $objdump = $cand; break }
}

Write-Host "[2/3] 复制 exe + media_ffmpeg.dll + FFmpeg 运行库 -> $outDir"
Copy-Item $exe (Join-Path $outDir 'gpui-editor.exe') -Force
Copy-Item $kernel (Join-Path $outDir 'media_ffmpeg.dll') -Force

# 2b) FFmpeg 运行库：按 media_ffmpeg.dll 的**实际导入表**驱动，只带必需件。
# media-ffmpeg/Cargo.toml 已剪掉 device/filter feature → avdevice/avfilter 不在导入表，
# 自动不再复制（本机 FFMPEG_DIR\bin 全量约 230MB → 仅 5 件约 121MB）；今后再改 feature
# 导入表变，重新打包即自适应，无需改本清单。
$ffImportsPattern = 'DLL Name: avcodec-.*\.dll|DLL Name: avformat-.*\.dll|DLL Name: avutil-.*\.dll|DLL Name: swscale-.*\.dll|DLL Name: swresample-.*\.dll|DLL Name: avdevice-.*\.dll|DLL Name: avfilter-.*\.dll'
$ffNeeded = @()
if ($objdump) {
    $ffNeeded = @(& $objdump -p $kernel 2>$null |
        Select-String -Pattern $ffImportsPattern |
        ForEach-Object { ($_.ToString().Trim() -replace '^DLL Name:\s*', '') })
}
if ($ffNeeded.Count -eq 0) {
    # objdump 不可用 → 前缀白名单回退（与 Cargo.toml 剪枝后的必需集一致）
    $ffNeeded = @(Get-ChildItem $ffBin -Filter '*.dll' | Where-Object {
        $_.Name -match '^(avcodec|avformat|avutil|swresample|swscale)-' } | ForEach-Object { $_.Name })
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

# ---------- 3) 校验 + 收尾 ----------
if (-not $objdump) {
    Write-Host "[警告] 未找到 llvm-objdump，跳过 exe/内核导入校验。"
} else {
    # 3a) exe 应【不再】导入任何 FFmpeg dll —— 保证缺 FFmpeg 也能启动
    $exeImports = @(& $objdump -p (Join-Path $outDir 'gpui-editor.exe') 2>$null |
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
