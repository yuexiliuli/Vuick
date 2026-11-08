# 第三方许可声明（THIRD-PARTY NOTICES）

本文件是 gpui-editor 分发/源码的第三方组件合规说明。**本体代码一律 Apache-2.0**（见`LICENSE-APACHE`），下列为随分发捆绑或引用到的第三方组件。

`scripts/package.ps1` 会把本文件与 `third_party/`（许可原文）随包复制到发布目录，满足「随
分发提供许可证文本」义务。

## FFmpeg（视频/音频解码）—— LGPL v2.1+

- **形态**：`crates/media-ffmpeg`（Apache-2.0 Rust cdylib）经 `libloading` 运行时动态加载；
  B1（默认）导入 FFmpeg 共享库 `avcodec/avformat/avutil/swresample/swscale` 五件独立 dll，
  主 exe 不静态导入 FFmpeg；B2（opt-in `static-link`）将 FFmpeg 静态归档链入 `media_ffmpeg.dll`
  （提供 `third_party` 之外构建脚本与静态归档以满足 LGPL 可重链）。
- **来源**：未修改的 FFmpeg 上游主线，**pin n9.0.2**；configure 白名单做 **瘦化 decode-only**
  （`--disable-encoders/muxers/network/filters/devices/bsfs/avdevice/avfilter` + `--disable-everything`），
  只启用 FFmpeg **原生解码器**（h264/hevc/vp8/vp9/aac/mp3/opus/flac…）与 d3d11va/dxva2 硬解；
  **未启用** `--enable-gpl` / `--enable-nonfree`、无外部 GPL/专利库。构建脚本：
  `scripts/build-slim-ffmpeg.sh`、`scripts/build-slim-ffmpeg-static-msvc.sh`（部署/复现文档见本机
  `docs/ffmpeg/`，未入库、仅本地）。
- **许可原文**：`third_party/LGPL-2.1.txt`（GNU LGPL v2.1）、`third_party/FFmpeg-LICENSE.md`（FFmpeg
  官方 LICENSE 说明：LGPL 默认、部分组件为 LGPL v3 / GPL v2+，本构建未启用任何 GPL/外部组件）。
- **源码**：https://ffmpeg.org （FFmpeg 项目版权归其贡献者所有，署名见上游源码）
- 注意：不要把 gyan / BtbN 的 **GPL** FFmpeg 构建包分发出去（会带来 GPL 传染）；分发只应使用自建 LGPL 瘦包。

## zstd（`zstd` crate，PDF 载荷压缩）

- BSD-3-Clause / MIT（Ze 字节级数据压缩；build 编译自带 C 源，见根 `Cargo.toml`）。

## pdfium（`pdfium-render`，PDF 栅格化）

- BSD-3-Clause（Chromium PDFium；随包以 zstd 压缩内嵌 pdfium dll，见根 `Cargo.toml`）。

## 其余

- 所有未在此列出的 crates / 代码均为本仓库自研，Apache-2.0。

---

## 专利口径（非法律意见）

HEVC/H.265、AAC 等属于标准必要专利（SEP）范畴，与代码许可证无关：Access Advance 等专利池
对**桌面软件、纯 CPU 解码的应用层软件**（浏览器/播放器/查看器）执行 royalty-free 政策；
若将来做商业分发 / 收费软件 / 云服务 / 进硬件，请在发货前另行评估专利池许可并与贵方法务确认。
