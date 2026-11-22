#!/usr/bin/env bash
# ============================================================
# FFmpeg Linux 瘦共享（shared）构建脚本 —— Windows B1 的 Linux 移植
#
# 用途：
#   构建只解码本地 file 协议的 FFmpeg shared 构建包（**LGPL**，无 --enable-gpl），
#   无 encode / mux / 网络 / 滤镜 / 设备 / bsf / 字幕解码器 / 图片解码器。
#   Linux 无 d3d11va/dxva2 硬解（vaapi 后续可选），纯软解 + swscale/swresample。
#   版本 pin n9.0.2（bindgen 版本门控；换版 SOVERSION/名字变 + 门控失效）。
#
# 白名单移植（与 Windows scripts/build-slim-ffmpeg.sh 逐项一致，勿改）：
#   - demuxer 12：ogg flac mp3 mov matroska avi mpegts asf flv aac wav ape
#   - decoder 29（pcm 系须逐项枚举，configure 无 pcm_* 通配）：
#     vorbis opus flac mp3 aac ape pcm_s16le pcm_s24le pcm_s32le pcm_u8 pcm_s8
#     pcm_f32le pcm_f64le pcm_alaw pcm_mulaw adpcm_ms wmav1 wmav2 wmapro h264 hevc
#     vp8 vp9 mpeg4 mpeg2video ac3 wmv1 wmv2 wmv3
#   - parser 12（无 mp3，真名 mpegaudio，且不自动带出须全显式）：
#     aac ac3 h264 hevc vp8 vp9 mpeg4video mpegvideo mpegaudio vorbis opus flac
#   - protocol 仅 file；bsf 全禁（--disable-bsfs，decode-only 不触发）；
#     字幕解码器全删（subtitle.rs Rust 自解析）；attached_pic 不需图片解码器。
# 与 Windows 的差异（仅平台层，白名单不变）：
#   - 去掉 --target-os=mingw64（Linux 原生 configure 自动），
#     去掉 MinGW runtime 静态内嵌（--extra-ldflags=-static-libgcc /
#     --extra-libs=-l:libwinpthread.a，Windows 自包含专用）。
#   - 去掉 --enable-d3d11va --enable-dxva2 与 codec 级 --enable-hwaccel=... 15 项。
#   - 保留 --disable-avdevice --disable-avfilter（防两个空壳库 → 恰 5 件 .so）。
#   - 保留 --disable-programs --disable-doc（只出库+头+pc）、
#     --enable-small --disable-debug。
#
# 如何运行（Linux bash）：
#   bash scripts/build-slim-ffmpeg-linux.sh [PREFIX] [SRC]
#   默认 PREFIX=$HOME/code/rust/temp/ffmpeg-linux-slim
#   默认 SRC  =$HOME/code/rust/temp/ffmpeg-src
#   幂等：$PREFIX/lib/libavcodec.so 与 lib/pkgconfig/libavcodec.pc 已存在
#        → 跳过构建直接打印三句 env。
#   源码获取：优先 git clone -b n9.0.2；失败（github 被墙）退化下载
#            ffmpeg-9.0.2.tar.xz 解包。版本必须 pin n9.0.2。
# ============================================================
set -euo pipefail

PREFIX="${1:-$HOME/code/rust/temp/ffmpeg-linux-slim}"
SRC="${2:-$HOME/code/rust/temp/ffmpeg-src}"

log() { printf '[build-slim-ffmpeg-linux] %s\n' "$*"; }
die() { printf '[build-slim-ffmpeg-linux][ERROR] %s\n' "$*" >&2; exit 1; }

finish() {
  echo
  echo "export FFMPEG_DIR=${PREFIX}"
  echo "export PKG_CONFIG_PATH=${PREFIX}/lib/pkgconfig"
  echo "export LD_LIBRARY_PATH=${PREFIX}/lib"
  echo
  local total=0 seen=0
  for pat in libavcodec libavformat libavutil libswresample libswscale; do
    for f in "$PREFIX"/lib/${pat}.so*; do
      [ -e "$f" ] || continue
      if [ -L "$f" ]; then
        printf '  %-40s -> %s\n' "$(basename "$f")" "$(readlink "$f")"
      elif [ -f "$f" ]; then
        local sz
        sz=$(stat -c %s "$f")
        total=$((total + sz)); seen=$((seen + 1))
        printf '  %-40s %10d B  (%s MB)\n' "$(basename "$f")" "$sz" \
          "$(awk -v n="$sz" 'BEGIN{printf "%.2f", n/1048576}')"
      fi
    done
  done
  printf '  合计: %d 件真实库, %d B (%s MB)\n' "$seen" "$total" \
    "$(awk -v n="$total" 'BEGIN{printf "%.2f", n/1048576}')"
  log "完成: $PREFIX"
}

# ---------- 幂等：产物已就位则直接给 env ----------
if [ -f "$PREFIX/lib/libavcodec.so" ] && [ -f "$PREFIX/lib/pkgconfig/libavcodec.pc" ]; then
  log "产物已就位，跳过构建: $PREFIX"
  finish
  exit 0
fi

# ---------- 构建前置：make/nasm（只装缺的；sudo 免密或 root） ----------
if ! command -v make >/dev/null 2>&1 || ! command -v nasm >/dev/null 2>&1; then
  need=""
  command -v make  >/dev/null 2>&1 || need="$need make"
  command -v nasm  >/dev/null 2>&1 || need="$need nasm"
  log "缺失工具:$need，尝试安装（sudo -n apt-get）..."
  apt="apt-get"
  [ "$(id -u)" = "0" ] || apt="sudo -n apt-get"
  if ! $apt install -y make nasm >/tmp/build-slim-ffmpeg-linux.apt.log 2>&1; then
    log "直接安装失败（apt 缓存可能陈旧），先 update 再装..."
    $apt update -y >/tmp/build-slim-ffmpeg-linux.apt-update.log 2>&1 || true
    $apt install -y make nasm >/tmp/build-slim-ffmpeg-linux.apt.log 2>&1 \
      || die "安装 make/nasm 失败——请手动安装或确保 sudo 免密（见 /tmp/build-slim-ffmpeg-linux.apt.log）"
  fi
fi
command -v make >/dev/null 2>&1 || die "仍缺 make"
command -v nasm >/dev/null 2>&1 || die "仍缺 nasm"

# ---------- 源码获取（版本 pin n9.0.2） ----------
PARENT="$(dirname "$SRC")"
mkdir -p "$PARENT"
if [ ! -f "$SRC/configure" ]; then
  if [ -d "$SRC" ]; then log "清理不完整源码目录: $SRC"; rm -rf "$SRC"; fi
  if command -v git >/dev/null 2>&1; then
    log "git clone -b n9.0.2 -> $SRC"
    if git clone --depth 1 -b n9.0.2 https://github.com/FFmpeg/FFmpeg.git "$SRC" \
        >/tmp/build-slim-ffmpeg-linux.git.log 2>&1; then
      log "git clone 就绪"
    else
      log "git clone 失败（github 不可达？），退化下载 ffmpeg-9.0.2.tar.xz"
      rm -rf "$SRC"
    fi
  fi
fi
if [ ! -f "$SRC/configure" ]; then
  command -v curl >/dev/null 2>&1 || die "缺少 curl"
  command -v tar  >/dev/null 2>&1 || die "缺少 tar"
  command -v xz   >/dev/null 2>&1 || die "缺少 xz"
  TB="$PARENT/ffmpeg-9.0.2.tar.xz"
  if [ ! -f "$TB" ] || ! tar -tJf "$TB" >/dev/null 2>&1; then
    rm -f "$TB"
    log "下载 https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz"
    curl -fL --retry 5 --retry-all-errors --retry-delay 3 -C - --max-time 3600 \
      -o "$TB" https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz \
      || die "下载失败(重试后仍失败): ffmpeg-9.0.2.tar.xz"
    tar -tJf "$TB" >/dev/null 2>&1 || die "tar 校验失败（下载可能不完整）: $TB"
  fi
  log "解包 $TB"
  tar -xJf "$TB" -C "$PARENT"
  [ -d "$PARENT/ffmpeg-9.0.2" ] || die "解包后未找到 ffmpeg-9.0.2 目录"
  mv "$PARENT/ffmpeg-9.0.2" "$SRC"
fi
[ -f "$SRC/configure" ] || die "源码不可用: $SRC"

# ---------- configure + make + install（前台阻塞到结束） ----------
log "configure + make + install: $SRC -> $PREFIX"
cd "$SRC"
./configure --prefix="$PREFIX" --arch=x86_64 \
  --enable-shared --disable-static --disable-programs --disable-doc \
  --disable-network --disable-devices --disable-filters \
  --disable-avdevice --disable-avfilter \
  --disable-muxers --disable-encoders --disable-bsfs --disable-autodetect \
  --disable-everything --enable-small --disable-debug \
  --enable-demuxer=ogg,flac,mp3,mov,matroska,avi,mpegts,asf,flv,aac,wav,ape \
  --enable-decoder=vorbis,opus,flac,mp3,aac,ape,pcm_s16le,pcm_s24le,pcm_s32le,pcm_u8,pcm_s8,pcm_f32le,pcm_f64le,pcm_alaw,pcm_mulaw,adpcm_ms,wmav1,wmav2,wmapro,h264,hevc,vp8,vp9,mpeg4,mpeg2video,ac3,wmv1,wmv2,wmv3 \
  --enable-parser=aac,ac3,h264,hevc,vp8,vp9,mpeg4video,mpegvideo,mpegaudio,vorbis,opus,flac \
  --enable-protocol=file

make -j"$(nproc)"
make install

finish
