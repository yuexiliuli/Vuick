<#
一键装填视频功能（ffmpeg-next 依赖，位于工作区成员 `video-ffmpeg`）的开发机环境。

需要三样：
- FFMPEG_DIR    → 指向含 include/ + lib/ + bin/ 的 FFmpeg win64 shared 构建（gyan/BtbN）
- LIBCLANG_PATH → 指向含 libclang.dll 的目录（bindgen 必需，装 LLVM 即得）
- PATH 加 ffmpeg bin → 开发期运行/测试动态加载 dll（分发打包见 scripts/package.ps1，随包捆绑）

用法：pwsh scripts/setup-ffmpeg.ps1
缺什么装什么 / 提示；已就位则跳过。设置对【新开的终端】生效。

说明：ffmpeg-sys-next 只在编译期读 FFMPEG_DIR/LIBCLANG_PATH（生成 bindings + 链接），
最终用户跑 release 二进制只需要 dll 随包带上（见 scripts/package.ps1）；源码构建方
无 FFmpeg 环境时可用 `cargo build --workspace --no-default-features --exclude video-ffmpeg`
关闭视频功能绕过（本工作区根包+成员同为默认成员，跳过视频须同时排除 cdylib 成员）。
#>

$ErrorActionPreference = 'Stop'

function Set-UserEnv([string]$Name, [string]$Value) {
    [Environment]::SetEnvironmentVariable($Name, $Value, 'User')
    Write-Host "[ok] 已设用户环境变量 $Name = $Value"
}

# ---------- 1) FFMPEG_DIR（构建期：头文件 + 导入库） ----------
$ffDir = [Environment]::GetEnvironmentVariable('FFMPEG_DIR', 'User')
$ffOk = $ffDir -and (Test-Path (Join-Path $ffDir 'include')) -and (Test-Path (Join-Path $ffDir 'lib'))
if ($ffOk) {
    Write-Host "[skip] FFMPEG_DIR 已就位: $ffDir"
} else {
    Write-Host @"

[需要] FFMPEG_DIR 未配置或其 include/ lib/ 缺失。
请下载 FFmpeg 的 win64 shared（带 dev 文件）构建：
  1) https://www.gyan.dev/ffmpeg/builds/            → ffmpeg-release-full-shared（推荐）
  2) https://github.com/BtbN/FFmpeg-Builds/releases → win64-gpl-shared（9.x 版本）
解压到固定目录（须含 include/ lib/ bin/ 三层），然后：
  [Environment]::SetEnvironmentVariable('FFMPEG_DIR', '<解压路径>', 'User')
"@
    exit 1
}

# ---------- 2) 运行期：ffmpeg bin 进用户 PATH ----------
$ffBin = Join-Path $ffDir 'bin'
$userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
if ($userPath -and (@($userPath -split ';') -contains $ffBin)) {
    Write-Host "[skip] PATH 已含 $ffBin"
} else {
    [Environment]::SetEnvironmentVariable('PATH', ($userPath + ';' + $ffBin), 'User')
    Write-Host "[ok] 已把 $ffBin 加入用户 PATH"
}

# ---------- 3) LIBCLANG_PATH（bindgen 必需） ----------
$lc = [Environment]::GetEnvironmentVariable('LIBCLANG_PATH', 'User')
if ($lc -and (Test-Path (Join-Path $lc 'libclang.dll'))) {
    Write-Host "[skip] LIBCLANG_PATH 已就位: $lc"
} else {
    $hit = @(
        'C:\Program Files\LLVM\bin\libclang.dll',
        'C:\Program Files (x86)\LLVM\bin\libclang.dll'
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($hit) {
        Set-UserEnv 'LIBCLANG_PATH' (Split-Path $hit)
    } else {
        Write-Host '[..] 未找到 libclang.dll，安装 LLVM（bindgen 必需，约 1GB）...'
        winget install -e --id LLVM.LLVM
        Set-UserEnv 'LIBCLANG_PATH' 'C:\Program Files\LLVM\bin'
    }
}

# ---------- 4) 结论 ----------
$newLc = [Environment]::GetEnvironmentVariable('LIBCLANG_PATH', 'User')
Write-Host @"

完成。已写入用户级环境变量（新终端生效）。
当前终端继续工作请先手填：
  `$env:FFMPEG_DIR    = '$ffDir'
  `$env:LIBCLANG_PATH = '$newLc'
"@
