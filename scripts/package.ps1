<#
一键打包可分发的 gpui-editor（release 二进制 + video_ffmpeg.dll + FFmpeg 运行库）。

背景：视频解码内核已拆成独立 cdylib（工作区成员 video-ffmpeg，产物 video_ffmpeg.dll），
主 exe 只经 libloading 在打开视频时才「运行时」加载它（PE 导入表里没有 FFmpeg dll）。
因此：
  - exe 缺 FFmpeg 也能正常启动，只在用视频时于窗口内报「缺少视频运行库」；
  - 要让视频功能可用，分发时 exe 必须与 video_ffmpeg.dll 及 FFmpeg 运行库同目录。

本脚本产出 dist\gpui-editor-win64\：
  - gpui-editor.exe（release，非 debug：debug 还依赖 vcruntime140d/ucrtbased 调试 CRT）
  - video_ffmpeg.dll（视频解码内核，本仓库 cdylib 构建产物）
  - FFMPEG_DIR\bin\ 下全部 *.dll（与编译期同一 FFmpeg 构建，版本号一致不会 ABI 错位）

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

# ---------- 1) release 构建（exe 与 video-ffmpeg 分开构建，确保 cdylib 一定产出） ----------
Write-Host "[1/3] cargo build --release（exe + video-ffmpeg cdylib）..."
cargo build --release -p gpui-editor
if ($LASTEXITCODE -ne 0) {
    Write-Host "[错误] release exe 构建失败。"
    exit 1
}
cargo build --release -p video-ffmpeg
if ($LASTEXITCODE -ne 0) {
    Write-Host "[错误] video-ffmpeg（cdylib）构建失败。"
    exit 1
}
$exe = Join-Path (Get-Location) 'target\release\gpui-editor.exe'
$kernel = Join-Path (Get-Location) 'target\release\video_ffmpeg.dll'
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

Write-Host "[2/3] 复制 exe + video_ffmpeg.dll + FFmpeg 运行库 -> $outDir"
Copy-Item $exe (Join-Path $outDir 'gpui-editor.exe') -Force
Copy-Item $kernel (Join-Path $outDir 'video_ffmpeg.dll') -Force
$dllCount = 0
foreach ($dll in Get-ChildItem $ffBin -Filter '*.dll') {
    Copy-Item $dll.FullName (Join-Path $outDir $dll.Name) -Force
    $dllCount++
}
Write-Host "     已复制 video_ffmpeg.dll + $dllCount 个 FFmpeg dll（avcodec/avformat/avutil/swscale/swresample/avdevice/avfilter 等）"

# ---------- 3) 校验 + 收尾 ----------
$objdump = 'C:\Program Files\LLVM\bin\llvm-objdump.exe'
if (-not (Test-Path $objdump)) {
    Write-Host "[警告] 未找到 llvm-objdump（$objdump），跳过导入校验。"
    exit 0
}

# 3a) exe 应【不再】导入任何 FFmpeg dll —— 保证缺 FFmpeg 也能启动
$exeImports = (& $objdump -p (Join-Path $outDir 'gpui-editor.exe') 2>$null |
    Select-String -Pattern 'DLL Name: avcodec-.*\.dll|DLL Name: avformat-.*\.dll|DLL Name: swscale-.*\.dll|DLL Name: swresample-.*\.dll|DLL Name: avutil-.*\.dll')
if ($exeImports) {
    Write-Host "[警告] exe 仍静态导入 FFmpeg：$($exeImports.Line -join ', ')。缺库时 exe 无法启动，未达预期。"
} else {
    Write-Host "[3/3] 校验通过：exe 无 FFmpeg 静态导入（缺库也能启动，视频缺库走窗口内报错）。"
}

# 3b) video_ffmpeg.dll 需要的 FFmpeg dll 应全部在包内
$missing = @()
foreach ($dllLine in (& $objdump -p (Join-Path $outDir 'video_ffmpeg.dll') 2>$null |
    Select-String -Pattern 'DLL Name: avcodec-.*\.dll|DLL Name: avformat-.*\.dll|DLL Name: swscale-.*\.dll|DLL Name: swresample-.*\.dll|DLL Name: avutil-.*\.dll|DLL Name: avdevice-.*\.dll|DLL Name: avfilter-.*\.dll')) {
    $name = ($dllLine.ToString().Trim() -replace '^DLL Name:\s*','')
    if (-not (Test-Path (Join-Path $outDir $name))) { $missing += $name }
}
if ($missing.Count -gt 0) {
    Write-Host "[警告] video_ffmpeg.dll 引用的以下 FFmpeg dll 未在包内：$($missing -join ', ')"
} else {
    Write-Host "     校验通过：video_ffmpeg.dll 所需 FFmpeg dll 已全部随包带上。"
}

$totalMB = [math]::Round(((Get-ChildItem $outDir | Measure-Object Length -Sum).Sum / 1MB), 1)
Write-Host @"

完成。可分发目录：$outDir（共 $totalMB MB）
发给人时整个目录/打成 zip 一起给（exe 必须和这些 dll 同目录）。
缺 FFmpeg 时 exe 仍能启动：不开视频照常使用，开视频会窗口内提示缺库。
"@
