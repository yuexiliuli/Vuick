#!/usr/bin/env bash
# -----------------------------------------------------------------------------
# 幂等供给脚本：Linux FFmpeg n9 供给，三句 env 供 media_ffmpeg（ffmpeg-sys-next
# v9.0.0）构建（pkg-config 定位头文件/库）。
#
#   - 默认（无参数）：自建 **LGPL** 瘦共享包（scripts/build-slim-ffmpeg-linux.sh，
#     `--disable-everything` + 白名单，无 --enable-gpl）——可分发，Linux 默认供给。
#   - 显式 --remote-gpl：退化到 BtbN linux64-shared 全量下载（**GPL**，仅 dev 兜底，
#     禁止分发——GPL 构建会带来 GPL 传染，归档文档 §9 纪律）。
#
# 用法:
#   bash scripts/setup-ffmpeg-linux.sh [--remote-gpl] [目标前缀]
#     默认前缀 = ${VUICK_DEV_DIR:-$HOME/.vuick-dev}/ffmpeg-linux-slim （瘦自建，LGPL）
#     --remote-gpl 时默认前缀 = ${VUICK_DEV_DIR:-$HOME/.vuick-dev}/ffmpeg-linux  （BtbN GPL 兜底）
#     幂等     = 相应前缀已就位则跳过（瘦: lib/libavcodec.so + lib/pkgconfig/libavcodec.pc；
#                GPL: include/libavcodec/avcodec.h + lib/pkgconfig/libavcodec.pc）
#
# 产物: <前缀>/{include,lib(含 pkgconfig)} 层（瘦自建无 bin），配合三句 export 直接构建：
#   export FFMPEG_DIR=...
#   export PKG_CONFIG_PATH=$FFMPEG_DIR/lib/pkgconfig
#   export LD_LIBRARY_PATH=$FFMPEG_DIR/lib
# -----------------------------------------------------------------------------
set -euo pipefail

# ---------- 可配置顶部 ----------
# 目标 FFmpeg 版本（n9 → libavcodec 63 / libavutil 61）
VERSION="n9.0.2"
# GitHub API 解析失败时的退化资产 URL（从 https://github.com/FFmpeg-Builds/FFmpeg-Builds/releases
# 的 autobuild-* release 复制；BtbN 仓库已更名 FFmpeg-Builds，curl -L 跟随跳转）
FALLBACK_URL="https://github.com/FFmpeg-Builds/FFmpeg-Builds/releases/download/autobuild-2026-11-21-12-37/ffmpeg-${VERSION}-61-gd59078794d-linux64-gpl-shared-9.0.tar.xz"

# ---------- 模式与前缀 ----------
REMOTE_GPL=0
PREFIX=""
for arg in "$@"; do
  case "$arg" in
    --remote-gpl) REMOTE_GPL=1 ;;
    *) PREFIX="$arg" ;;
  esac
done
if [ -z "$PREFIX" ]; then
  if [ "$REMOTE_GPL" = 1 ]; then
    PREFIX="${VUICK_DEV_DIR:-$HOME/.vuick-dev}/ffmpeg-linux"
  else
    PREFIX="${VUICK_DEV_DIR:-$HOME/.vuick-dev}/ffmpeg-linux-slim"
  fi
fi

log() { printf '[setup-ffmpeg-linux] %s\n' "$*"; }
die() { printf '[setup-ffmpeg-linux][ERROR] %s\n' "$*" >&2; exit 1; }

print_env() {
  cat <<EOF

export FFMPEG_DIR=${PREFIX}
export PKG_CONFIG_PATH=${PREFIX}/lib/pkgconfig
export LD_LIBRARY_PATH=${PREFIX}/lib
EOF
}

# ---------- 默认路径：自建 LGPL 瘦包（build-slim-ffmpeg-linux.sh 自含幂等） ----------
if [ "$REMOTE_GPL" = 0 ]; then
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  log "默认供给: 自建 LGPL 瘦包 -> $PREFIX"
  bash "$SCRIPT_DIR/build-slim-ffmpeg-linux.sh" "$PREFIX"
  print_env
  exit 0
fi

# ===========================================================================
# --remote-gpl：BtbN linux64-shared 下载兜底（⚠️ **GPL**，仅 dev，禁止分发）
# ===========================================================================
log "⚠️ --remote-gpl：BtbN **GPL** 构建，仅 dev 兜底，禁止分发"

# ---------- 幂等检查：前缀已就位则直接给 env 退出 ----------
if [ -f "$PREFIX/include/libavcodec/avcodec.h" ] && [ -f "$PREFIX/lib/pkgconfig/libavcodec.pc" ]; then
  log "前缀已就位，跳过下载: $PREFIX"
  print_env
  exit 0
fi

command -v curl >/dev/null 2>&1 || die "缺少 curl"
command -v tar  >/dev/null 2>&1 || die "缺少 tar"
command -v xz   >/dev/null 2>&1 || die "缺少 xz"

PARENT="$(dirname "$PREFIX")"
mkdir -p "$PARENT"
TARBALL="$PARENT/.ffmpeg-${VERSION}.tar.xz"
STAGE="$PARENT/.ffmpeg-stage.$$"
trap 'rm -rf "$STAGE"' EXIT

# ---------- 1) 解析最新 n9 shared 资产 URL（GitHub API，失败退化到固定 URL） ----------
log "解析 BtbN FFmpeg ${VERSION} linux64 shared 资产 URL (GitHub API) ..."
ASSET_URL="$(
  curl -sfL --retry 3 --max-time 60 'https://api.github.com/repos/BtbN/FFmpeg-Builds/releases?per_page=5' \
    | grep -oE '"browser_download_url"[[:space:]]*:[[:space:]]*"[^"]*"' \
    | sed -E 's/^"browser_download_url"[[:space:]]*:[[:space:]]*"//; s/"$//' \
    | grep -E "ffmpeg-${VERSION}[^/]*-linux64-gpl-[^/]*shared[^/]*\.tar\.xz" \
    | head -n1 \
    || true
)"
if [ -n "$ASSET_URL" ]; then
  log "命中: $ASSET_URL"
else
  ASSET_URL="$FALLBACK_URL"
  log "API 未命中，退化到固定 URL: $FALLBACK_URL"
fi

# ---------- 2) 下载（复用完整缓存 / 断点续传） ----------
if [ -f "$TARBALL" ] && tar -tJf "$TARBALL" >/dev/null 2>&1; then
  log "已存在完整 tar 缓存，跳过下载: $TARBALL"
else
  rm -f "$TARBALL"
  log "下载: $ASSET_URL"
  # --retry-all-errors 连 Connection refused 也重试（本机曾见 github.com 瞬时拒绝）；
  # -C - 断点续传，--max-time 防无限挂起
  curl -fL --retry 5 --retry-all-errors --retry-delay 3 -C - --max-time 1800 \
    -o "$TARBALL" "$ASSET_URL" || die "下载失败(重试后仍失败): $ASSET_URL"
  tar -tJf "$TARBALL" >/dev/null 2>&1 || die "tar 校验失败（下载可能不完整）: $TARBALL"
fi

# ---------- 3) 解压 + 校验三层 + 原子落地 ----------
log "解压到临时目录并校验三层 ..."
mkdir -p "$STAGE"
tar -xJf "$TARBALL" -C "$STAGE"
for d in "$STAGE"/*/; do INNER="$d"; break; done
INNER="${INNER%/}"
[ -n "$INNER" ] && [ -d "$INNER" ] || die "解压后未找到内容目录"
if [ ! -f "$INNER/include/libavcodec/avcodec.h" ] || [ ! -f "$INNER/lib/pkgconfig/libavcodec.pc" ]; then
  die "解压产物缺少 include/lib/pkgconfig 三层: $INNER"
fi
rm -rf "$PREFIX"
mv "$INNER" "$PREFIX"
rm -rf "$STAGE"
trap - EXIT
rm -f "$TARBALL"

log "就绪: $PREFIX（GPL，仅 dev，禁止分发）"
print_env
