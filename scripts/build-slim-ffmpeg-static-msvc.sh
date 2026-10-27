#!/bin/bash
# ============================================================
# FFmpeg B2 瘦静态（static / MSVC）构建脚本
#
# 用途：
#   在 B1 瘦共享包之上另建一套 **MSVC 静态** 瘦包（同一白名单）：
#   - B1（scripts/build-slim-ffmpeg.sh）：shared（.dll），默认链路，
#     FFMPEG_DIR = E:\Rust\temp\ffmpeg-slim，分发 5 件 dll（6.2MB）。
#   - B2（本脚本）：--toolchain=msvc --enable-static，产物是 5 个 COFF
#     静态归档 avcodec.lib / avformat.lib / avutil.lib / swresample.lib /
#     swscale.lib（无任何 dll，bin\ 为空）。
#     配合 crates/media-ffmpeg 的 `static-link` cargo feature，ffmpeg-sys
#     以 `static=` 把这些 .lib 直接链进 media_ffmpeg.dll → 分发只需
#     exe + 1 件 media_ffmpeg.dll（导入表只剩系统 dll，无 av*.dll）。
#     B2 是 opt-in：默认（无 feature）仍是 B1 shared，FFMPEG_DIR 不动。
#   版本 pin n9.0.2（与 B1 同一源树 E:\Rust\temp\ffmpeg）。
#
# 白名单与 B1 完全一致（不另定稿），只换静态/MSVC 相关：
#   - --toolchain=msvc --enable-static --disable-shared
#     （B2 产物要是 MSVC 可消费的 COFF 归档；mingw 的 .a 不能混 MSVC 目标）
#   - --target-os=win64：静态无 dll 链接，故无需 B1 那套 mingw64 的
#     SHFLAGS/AR-NM 注入；MSVC 分支（cl/link wrapper/lib.exe）直接可用。
#   - --x86asmexe=/ucrt64/bin/nasm.exe：x86 汇编（h264/hevc 等需要）。
#   - --extra-cflags=-MD ⚠️：MSVC cl 默认 /MT（静态 CRT = LIBCMT），而
#     rustc 的 MSVC cdylib 用 /MD（动态 CRT = VCRUNTIME140，见现有
#     media_ffmpeg.dll 导入表）。cl 不开 /MD 直接编出的 .obj 会在 Rust
#     链接期报 LNK2038 RuntimeLibrary 不匹配。此项强制动态 CRT 对齐。
#     注：预案初稿写「cl 默认 /MD」经实测有误（默认 /MT），故补此项
#     （configure 也会自动给 cl 加 -MD，这里显式写死双保险）。
#   - cl.exe 英文 banner shim ⚠️：本机 MSVC（14.40.33813）只装 zh-CN，
#     其编译器 logo/版权横幅固定中文（实测 VSLANG/LANG/LC_ALL 全部无效，
#     logo 随 OS MUI 编译进 cl.exe）。FFmpeg configure 识别 MSVC 的探针
#     是 `VSLANG=1033 cl -nologo- | grep '^Microsoft'` 且 msvc 工具链块
#     还 `cl_major_ver=$(cl.exe | sed 's/.*Version <n>\./...')`——中文横幅
#     二者都不匹配 → 识别不到 msvc → 配置期拿 GCC 默认 -o 当输出参数，
#     cl 忽略 -o 只写 test.obj 到 CWD → mslink 链到陈旧/缺失 obj 报
#     LNK1136/LNK1181（实测第一轮 configure 即死在此）。故构建脚本自建
#     一个只在本脚本 PATH 里生效的 cl.exe shim（放临时目录）：
#     * 仅当被当作「纯横幅探测」调用（无实参或仅 -nologo-）→ 输出英文
#       Microsoft logo（含 Version 19.x 满足 cl_major_ver 门），exit 0；
#     * 其余一切编译调用 → exec 透传真实 cl.exe（本机 14.40.33807）。
#     本 shim 只修探测横幅，产物仍由真实 cl 编译（见下条 ⚠️2 后用 clang-cl，
#     该 shim 仅剩 mslink/工具链门禁的 cl.exe banner 用途）。
#   - ⚠️2 编译器实际用 clang-cl（--cc=…/clang-cl.exe，非 cl）：
#     MSVC cl 对 FFmpeg 瘦配置有一处硬伤（实测 -O1/-O2 均报）：
#     libavcodec/ac3dec_float.c 在 CONFIG_EAC3_DECODER=0 时
#     `#include "eac3dec.c"` 被 #if 跳过，而 ac3dec.c 里 `else if
#     (CONFIG_EAC3_DECODER)` 的死分支仍引用静态函数 ff_eac3_parse_header
#     （定义只在 eac3dec.c）→ cl 报 C2129「已声明但未定义」；GCC/clang
#     会把常量条件整折掉所以无错（B1 mingw 同白名单通过）。开 eac3 解码器
#     能绕开但会改白名单（预案明令与 B1 一致，不改）。clang-cl 目标
#     x86_64-pc-windows-msvc（MSVC ABI、动态 CRT），折叠死分支无 C2129，
#     产 COFF .obj → lib.exe 归档成 avcodec.lib，Rust `static=` 可链，
#     ABI 与 cl 完全一致。故 msvc 工具链 + clang-cl + 上方 shim + 下方
#     MSYS2_ARG_CONV_EXCL='*'（防 MSYS 把 -FoC:/… 之类改坏）。
#   - d3d11/dxgi/dxva2 不链：avutil hwcontext_d3d11va/dxva2 走运行时
#     dlopen("d3d11.dll"等)+GetProcAddress，静态链零依赖；
#     静态只需 SDK 的 bcrypt.lib(avutil random_seed)/user32.lib(avutil)/
#     ole32.lib(avcodec)，由 media-ffmpeg/build.rs 在 Rust 侧补链。
#
# 运行方式（必须在 MSYS2 bash 下，本机 = C:\msys64\usr\bin\bash.exe）：
#   bash scripts/build-slim-ffmpeg-static-msvc.sh
#   可传两参覆盖：bash scripts/build-slim-ffmpeg-static-msvc.sh [SRC] [PREFIX]
#   默认 SRC=/e/Rust/temp/ffmpeg、PREFIX=/e/Rust/temp/ffmpeg-slim-static
#   脚本自行 make distclean（清掉 B1 mingw shared 配置残留），无需手动。
#   MSVC 环境经 .bat shim 导入 vcvars64（规避 MSYS 引号/路径转换）。
#
# 验证（static-link 模式的回归与打包门禁）：
#   $env:FFMPEG_DIR='E:\Rust\temp\ffmpeg-slim-static'
#   cargo test -p media-ffmpeg --features static-link -- --test-threads=1
#     # 期望 38 passed / 4 ignored（与 B1 shared 同套解码回归，静态链下跑）
#   cargo build -p media-ffmpeg --features static-link
#   llvm-objdump -p target\debug\media_ffmpeg.dll | findstr "DLL Name"
#     # 期望无 avcodec/avformat/... dll 导入，仅系统 dll（分发=exe+1 件 dll）
# # 注意：package.ps1 尚未适配静态包（无 bin\ 目录会前置报错），B2 打包属后续项。
# ============================================================
set -e

SRC="${1:-/e/Rust/temp/ffmpeg}"
PREFIX="${2:-/e/Rust/temp/ffmpeg-slim-static}"
TMPD=/c/Users/yuexi/AppData/Local/Temp/kilo
VCVARS="C:\\WorkSpace\\Software\\Code\\MicrosoftVisualStudio\\2022\\Professional\\VC\\Auxiliary\\Build\\vcvars64.bat"
REAL_CL="/c/WorkSpace/Software/Code/MicrosoftVisualStudio/2022/Professional/VC/Tools/MSVC/14.40.33807/bin/HostX64/x64/cl.exe"

echo "==> [B2] SRC=$SRC PREFIX=$PREFIX"

# ---------- 0) make distclean（清掉 B1 mingw shared 的配置/对象残留） ----------
cd "$SRC"
echo "==> make distclean"
make distclean 2>/dev/null || true

# ---------- 1) 导入 vcvars64（MSVC cl/link/lib/dumpbin + SDK INCLUDE/LIB） ----------
echo "==> import vcvars64 (shim)"
mkdir -p "$TMPD"
cat > "$TMPD/vcvars_shim.bat" <<EOF
@echo off
call "$VCVARS" >nul 2>&1
set
EOF
cd "$TMPD"
while IFS='=' read -r k v; do
  case "$k" in
    ''|*[!A-Za-z0-9_]*) continue ;;
  esac
  v=${v%$'\r'}
  export "$k=$v"
done < <(cmd //c vcvars_shim.bat 2>/dev/null)
# PATH 还原回 POSIX 形式（否则 bash 的 /usr/bin/* 工具全不可寻址）
export PATH="$(/usr/bin/cygpath -u -p -- "$PATH")"

echo "--- toolchain sanity (vcvars 后、shim 前) ---"
CL_PATH=$(/usr/bin/which cl  2>/dev/null || true)
LINK_PATH=$(/usr/bin/which link 2>/dev/null || true)
echo "cl  = $CL_PATH"
echo "link= $LINK_PATH"
case "$CL_PATH" in
  *MicrosoftVisualStudio*) : ;;
  *) echo "!! cl 不是 MSVC，中止"; exit 1 ;;
esac
case "$LINK_PATH" in
  *MicrosoftVisualStudio*) : ;;
  *) echo "!! link 不是 MSVC（被 MSYS 的 GNU link 遮蔽），中止"; exit 1 ;;
esac

# ---------- 1b) 生成 cl.exe 英文 banner shim 并置顶 PATH（见头部注释） ----------
SHIM_DIR="$TMPD/b2-cl"
mkdir -p "$SHIM_DIR"
cat > "$SHIM_DIR/cl.exe" <<'EOF'
#!/bin/sh
REAL_CL="/c/WorkSpace/Software/Code/MicrosoftVisualStudio/2022/Professional/VC/Tools/MSVC/14.40.33807/bin/HostX64/x64/cl.exe"
if [ "$#" -le 1 ]; then
  echo "Microsoft (R) C/C++ Optimizing Compiler Version 19.40.33813 for x64"
  echo "Copyright (C) Microsoft Corporation.  All rights reserved."
  exit 0
fi
exec "$REAL_CL" "$@"
EOF
chmod +x "$SHIM_DIR/cl.exe"
export PATH="$SHIM_DIR:$PATH"

echo "--- shim 生效检查 (cl 应解析到 b2-cl) ---"
/usr/bin/which cl

# ---------- 2) configure（MSVC 静态 + clang-cl；白名单与 B1 一致） ----------
cd "$SRC"
echo "==> configure (msvc static, cc=clang-cl)"
# 防 MSYS 把 -FoC:/… /I 等参数改写成 C:/msys64/…（clang-cl no ABI 转换）。
export MSYS2_ARG_CONV_EXCL='*'
./configure --prefix="$PREFIX" --arch=x86_64 --target-os=win64 --toolchain=msvc \
  --cc="/c/WorkSpace/Software/Code/LLVM/bin/clang-cl.exe" \
  --enable-static --disable-shared --disable-programs --disable-doc \
  --disable-network --disable-devices --disable-filters \
  --disable-avdevice --disable-avfilter \
  --disable-muxers --disable-encoders --disable-bsfs --disable-autodetect \
  --disable-everything --enable-small --disable-debug \
  --extra-cflags=-MD \
  --enable-d3d11va --enable-dxva2 --x86asmexe=/ucrt64/bin/nasm.exe \
  --enable-demuxer=ogg,flac,mp3,mov,matroska,avi,mpegts,asf,flv,aac,wav,ape \
  --enable-decoder=vorbis,opus,flac,mp3,aac,ape,pcm_s16le,pcm_s24le,pcm_s32le,pcm_u8,pcm_s8,pcm_f32le,pcm_f64le,pcm_alaw,pcm_mulaw,adpcm_ms,wmav1,wmav2,wmapro,h264,hevc,vp8,vp9,mpeg4,mpeg2video,ac3,wmv1,wmv2,wmv3 \
  --enable-parser=aac,ac3,h264,hevc,vp8,vp9,mpeg4video,mpegvideo,mpegaudio,vorbis,opus,flac \
  --enable-protocol=file \
  --enable-hwaccel=h264_d3d11va,h264_d3d11va2,h264_dxva2,hevc_d3d11va,hevc_d3d11va2,hevc_dxva2,mpeg2_d3d11va,mpeg2_d3d11va2,mpeg2_dxva2,vp9_d3d11va,vp9_d3d11va2,vp9_dxva2,vc1_d3d11va,vc1_d3d11va2,vc1_dxva2

echo "==> configure 后核对 (config.mak)"
echo "CONFIG_D3D11VA=$(grep -E '^CONFIG_D3D11VA=' ffbuild/config.mak)"
echo "CONFIG_DXVA2=$(grep -E '^CONFIG_DXVA2=' ffbuild/config.mak)"
echo "Enabled hwaccels 计数: $(grep -cE '^CONFIG_(H264|HEVC|MPEG2|VP9|VC1)_(D3D11VA|D3D11VA2|DXVA2)_HWACCEL=yes' ffbuild/config.mak || true)"
echo "demuxers 计数: $(grep -cE '^CONFIG_.*_DEMUXER=yes$' ffbuild/config.mak || true)"
echo "decoders 计数: $(grep -cE '^CONFIG_.*_DECODER=yes$' ffbuild/config.mak || true)"
echo "parsers 计数: $(grep -cE '^CONFIG_.*_PARSER=yes$' ffbuild/config.mak || true)"

# ---------- 3) make + install ----------
echo "==> make -j\$(nproc)"
make -j$(nproc)
echo "==> make install"
make install

# ---------- 4) 产物判定：真静态 COFF 归档（非导入库） ----------
cd "$PREFIX/lib"
echo "==> 产物判定（$PREFIX/lib）"
ls -l avcodec.lib avformat.lib avutil.lib swresample.lib swscale.lib
for f in avcodec.lib avformat.lib avutil.lib swresample.lib swscale.lib; do
  /c/WorkSpace/Software/Code/LLVM/bin/llvm-objdump.exe -f "$f" 2>/dev/null | head -1
done
echo "--- avcodec.lib 真实代码符号抽样 (llvm-nm T 计数) ---"
/c/WorkSpace/Software/Code/LLVM/bin/llvm-nm.exe avcodec.lib 2>/dev/null | grep -c ' [Tt] ' || true
echo "--- avcodec.lib 源码对象抽样 (llvm-ar t) ---"
/c/WorkSpace/Software/Code/LLVM/bin/llvm-ar.exe t avcodec.lib 2>/dev/null | grep -E 'h264dec|hevcdec|vp9dec|aacdec' | head -8 || true
echo "--- bin\\ 应为空（静态无 dll） ---"
if [ -d "$PREFIX/bin" ] && [ "$(ls -A "$PREFIX/bin" 2>/dev/null)" ]; then
  ls -la "$PREFIX/bin"
else
  echo "(bin 不存在或为空，符合预期)"
fi
echo "==> [B2] 构建完成，安装于 $PREFIX"
