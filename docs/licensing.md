# 许可证与第三方合规

> 本文内容原属 README.md 说明书部分，重写后拆分至此。

## 本体代码

- **GPL-3.0-or-later**（`LICENSE-GPL`）：主程序与全部 crates 以 GPL-3.0-or-later 授权。原因：仓库 `markdown` 的识别层（`recognition.rs` / `cells.rs` / `inline_style.rs`）与 `markdown_wysiwyg_math` 的数学排版链衍生自 **[zorite]**（GPL-3.0-or-later），按 GPL 传染要求整个程序以 GPL-3.0-or-later 发布。
- 历史声明：早期为 **Apache-2.0**（`LICENSE-APACHE` 仍保留作参考）；部分自研模块保留了 Apache 风格的实现说明，但项目整体许可证现为 GPL-3.0-or-later。

## 第三方组件

分发包由 `scripts/package.ps1` 随包提供 `THIRD_PARTY_NOTICES.md` 与 `third_party/` 许可原文：

- **zorite**（Markdown 识别/数学排版衍生来源）：**GPL-3.0-or-later**。本仓库 `markdown/src/parse/{recognition,cells,inline_style}.rs` 与 `markdown_wysiwyg_math`（`math_typeset.rs` / `math.rs` / `content_sync.rs::sync_math` 等）的识别与数学排版实现衍生自该仓库（GitHub: `packetThrower/zorite`），保留其版权与 `LICENSE-GPL`（全文同源）。将整个程序转为 GPL-3.0-or-later 以满足其许可条件。
- **FFmpeg**（视频/音频解码）：**LGPL v2.1+**；以未修改的 FFmpeg 上游主线 **n9.0.2** 做**瘦化 decode-only** 构建（configure 白名单仅启用原生解码器 + 硬解，无 `--enable-gpl` / `--enable-nonfree`、无外部 GPL 库）；主 exe 经 `libloading` **运行时动态加载** `media_ffmpeg.dll`（B1 独立 av\*.dll / B2 静态归档可重链），本体代码不受 LGPL 传染。构建脚本 `scripts/build-slim-ffmpeg*.sh`，文本见 `third_party/LGPL-2.1.txt`、`third_party/FFmpeg-LICENSE.md`。
- **zstd**（PDF 载荷压缩）：BSD-3-Clause / MIT。
- **pdfium**（`pdfium-render` —— MIT OR Apache-2.0；内嵌 PDFium 预编译库 —— **BSD-3-Clause + Apache-2.0**，含 FreeType/ICU 等内嵌第三方各自许可）：PDF 栅格化。许可原文见 `third_party/pdfium/`。
- **gpui 及 zed 子集**（UI 框架，`vendor/zed`）：Apache-2.0（zed 仓库 gpui 等 crates 子系统；仅 vendor Windows 子集闭包，rev 93d7456）。Apache-2.0 宽松许可，可并入 GPL-3.0 程序。
- **SimdCodecs**（`crates/kimg*` 图像编解码内核的重写来源）：**MIT**（C# `sdcb/SimdCodecs`，GitHub: `sdcb/SimdCodecs`）。`kimg` 为其纯 Rust 重写（PNG/JPEG/WebP/GIF/BMP/ICO/PNM/TIFF 解码）；夹具 golden 数据许可记录见 `crates/kimg_webp/tests/fixtures/webp/ATTRIBUTION.md`。

## 专利口径（非法律意见）

HEVC/H.265、AAC 等标准必要专利与代码许可无关；本应用为桌面软件、纯 CPU 解码，属 HEVC Advance「royalty-free 应用层软件」政策覆盖面；商业分发 / 收费 / 云服务 / 硬件形态，发货前请另行评估专利池并与法务确认。
