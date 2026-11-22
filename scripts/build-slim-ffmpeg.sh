#!/bin/bash
# ============================================================
# FFmpeg B1 瘦共享（shared）构建脚本
#
# ⚠️ B2 静态单 dll（一次性链进 media_ffmpeg.dll，分发=exe+1 dll，opt-in）见
#    build-slim-ffmpeg-static-msvc.sh + media_ffmpeg `static-link` feature。
#
# 用途：
#   构建只解码本地 file 协议的 FFmpeg shared 构建包（约 10-13MB），
#   无 encode / mux / 网络 / 滤镜 / 设备 / bsf / 字幕解码器 / 图片解码器。
#   硬解 d3d11va / dxva2 必须保留。
#   版本 pin n9.0.2（bindgen 版本门控）。
#
# 白名单定稿来源（主 agent 会话「FFmpeg 精简方案头脑风暴」，2026-10-27）：
#   - 白名单契约映射核对：字幕解码器可全删、attached_pic 不需解码器、
#     bsf 全不 enable、parser 必须显式枚举（mp3 的 parser 名是 mpegaudio、
#     pcm_* 在 configure 无通配、必须枚举）。
#   - 构建链研究：--disable-autodetect 会连带掐掉 d3d11va/dxva2 硬解
#     autodetect，故显式 --enable-d3d11va --enable-dxva2 找回（configure
#     后必须确认二者 enabled）。
#   - 本脚本不改白名单，如需重新定稿白名单请回主 agent 会话。
#   - 注：定稿清单中的 --disable-postproc 已在 n9.0.2 删除——libpostproc 自
#     FFmpeg 8.0 起整体移除，configure 中无任何 postproc 引用（本仓 grep
#     configure 零命中），该选项为失效空操作；不删会让 configure 直接
#     "Unknown option" 退出。对本构建无任何组件影响。
#   - 注：定稿清单中的 --enable-linklib 已从 n9 configure 移除（grep 零命中）。
#     linklib 是早期 MSVC 专用开关；MinGW/UCRT64 shared 构建会自动产出
#     av*.lib 导入库（MSVC 可消费），无需该选项。
#   - 注：定稿清单中的 --enable-hwaccel=d3d11va,dxva2 不匹配 configure 任何项
#     （FFmpeg 的 hwaccel 名是 codec 级，如 h264_d3d11va；库级 --enable-d3d11va/
#     --enable-dxva2 ≠ 解码器侧注册）。首次 configure 时 "Enabled hwaccels:" 为空，
#     即硬解运行期不会生效。已按 n9.0.2 合法名单改为白名单内 codec 的精确 hwaccel：
#     h264/hevc/mpeg2/vp9（+wmv3 自动依赖拉入的 vc1）的 d3d11va/d3d11va2/dxva2。
#     vp8/mpeg4/wmv 上游无 d3d11va/dxva2 hwaccel 实现，无需也不能 enable。
#   - 注：定稿清单只有 --disable-devices / --disable-filters——它们只清空
#     libavdevice / libavfilter 的组件，仍会编译两个空壳共享库（Libraries:
#     列表含 avdevice/avfilter），交付时 bin\ 就有 7 件 dll，违反「恰 5 件、
#     无 avdevice/avfilter」的验收项。故补库级 --disable-avdevice
#     --disable-avfilter，让这两个空壳库完全不构建。
#   - 注：定稿为 --target-os=win64，但 win64 分支的 SHFLAGS 是 MSVC link.exe
#     风格（-dll -def: -implib:），mingw gcc/ld 直接报
#     "unrecognized command-line option '-implib:...'"，且 SLIB_CREATE_DEF_CMD
#     不注入 AR/NM（默认找 MSVC lib.exe+dumpbin）。改为 --target-os=mingw64：
#     同一组件集、同一 dll 命名（avcodec-63.dll 等）、同一 include/.def/.lib
#     布局；该分支自带 AR="$(AR_CMD)" NM="$(NM_CMD)" 注入，def 由 GNU ar/nm
#     生成，dll 由 gcc/ld 链接，avcodec.lib 等导入库由 dlltool 以 .def 为源
#     生成（MSVC 可消费，含 __imp_ 符号；如 Phase B 遇 MSVC 兼容问题可改用
#     llvm-lib /def:avcodec-63.def /out:avcodec.lib 重生成）。
#   注：--enable-hwaccel 已按 n9.0.2 合法名单修正；configure 后对表
#     "Enabled hwaccels:" 应含 15 项（h264/hevc/mpeg2/vp9/vc1 × d3d11va/d3d11va2/dxva2）。
#
# 注（B1 Phase B，2026-10-28）：MinGW 链出的共享 dll 默认依赖两个 MSYS2 运行时
#   libwinpthread-1.dll（avutil，取 clock_gettime64/nanosleep64）与 libgcc_s_seh-1.dll
#   （swscale）——分发须随包部署，极不干净。故 configure 把两个运行时静态内嵌进各 dll：
#   - libgcc_s_seh：--extra-ldflags="-static-libgcc"（驱动选项，实测生效，swscale 干净）。
#   - libwinpthread：不能用 -static-libwinpthread（UCRT64 GCC 16.2 不认该驱动选项，
#     实测报 unrecognized option）；`-Wl,-Bstatic -lwinpthread` 放 LDFLAGS 也无效——
#     FFmpeg 链接序是 LDFLAGS 在 objects **之前**，静态库先于依赖它的 o 被扫、落空，
#     末尾驱动隐式 -lwinpthread 仍拉 DLL。正确姿势：--extra-libs 把 `-l:libwinpthread.a`
#     追加到每库链接命令**末尾**（objects 之后），配合 LDFLAGS 自带 --as-needed，
#     winpthread 全部符号静态解析后，驱动末尾的动态 winpthread 因无未决引用被丢弃
#     （llvm-objdump -p 复核：avutil 不再依赖 libwinpthread-1.dll）。
#   若任一 dll 重建后仍依赖 libwinpthread-1.dll/libgcc_s_seh-1.dll（objdump 复核）或
#   体积膨胀显著（合计 +>1.5MB），回退本方案：删掉 --extra-ldflags/--extra-libs 两行后
#   重跑本脚本，两种方案都记录在 "${VUICK_DEV_DIR:-$HOME/.vuick-dev}/b1"。
#
# 如何运行（MSYS2 UCRT64 环境下）：
#   bash scripts/build-slim-ffmpeg.sh
#   （UCRT64 环境 = MSYSTEM=UCRT64，或 msys2_shell.cmd -ucrt64 -c "bash scripts/build-slim-ffmpeg.sh"）
#   可传两参覆盖：bash scripts/build-slim-ffmpeg.sh [SRC] [PREFIX]
#   默认 SRC=${VUICK_DEV_DIR:-$HOME/.vuick-dev}/ffmpeg、PREFIX=${VUICK_DEV_DIR:-$HOME/.vuick-dev}/ffmpeg-slim
# ============================================================
set -e

SRC="${1:-${VUICK_DEV_DIR:-$HOME/.vuick-dev}/ffmpeg}"
PREFIX="${2:-${VUICK_DEV_DIR:-$HOME/.vuick-dev}/ffmpeg-slim}"

cd "$SRC"

./configure --prefix="$PREFIX" --arch=x86_64 --target-os=mingw64 \
  --enable-shared --disable-static --disable-programs --disable-doc \
  --disable-network --disable-devices --disable-filters \
  --disable-avdevice --disable-avfilter \
  --disable-muxers --disable-encoders --disable-bsfs --disable-autodetect \
  --disable-everything --enable-small --disable-debug \
  --extra-ldflags="-static-libgcc" \
  --extra-libs="-l:libwinpthread.a" \
  --enable-d3d11va --enable-dxva2 \
  --enable-demuxer=ogg,flac,mp3,mov,matroska,avi,mpegts,asf,flv,aac,wav,ape \
  --enable-decoder=vorbis,opus,flac,mp3,aac,ape,pcm_s16le,pcm_s24le,pcm_s32le,pcm_u8,pcm_s8,pcm_f32le,pcm_f64le,pcm_alaw,pcm_mulaw,adpcm_ms,wmav1,wmav2,wmapro,h264,hevc,vp8,vp9,mpeg4,mpeg2video,ac3,wmv1,wmv2,wmv3 \
  --enable-parser=aac,ac3,h264,hevc,vp8,vp9,mpeg4video,mpegvideo,mpegaudio,vorbis,opus,flac \
  --enable-protocol=file \
  --enable-hwaccel=h264_d3d11va,h264_d3d11va2,h264_dxva2,hevc_d3d11va,hevc_d3d11va2,hevc_dxva2,mpeg2_d3d11va,mpeg2_d3d11va2,mpeg2_dxva2,vp9_d3d11va,vp9_d3d11va2,vp9_dxva2,vc1_d3d11va,vc1_d3d11va2,vc1_dxva2

make -j$(nproc)
make install
