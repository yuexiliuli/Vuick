# kimg

纯 Rust 图像解码/编码 crate，`image 0.25.10` 的 API 兼容镜像，供 vuick 全仓泛用解码路径（`ImageReader→decode→to_rgba8`）直接替换 crates.io `image`。许可 Apache-2.0 自持，不引入 AGPL 等传染性依赖。

与 `image` 的关系：**PNG、WebP、GIF 为仓内自研核心**（PNG 全链路 pngsimd + png_encoder；WebP container + VP8L + VP8 + ALPH；GIF 容器 + LZW 解/编 + 画布帧合成）；JPEG/TIFF 复用既有第三方纯 Rust 后端（JPEG 为 `zune-jpeg`，与 image 同源保证字节级一致性）；BMP/ICO/PNM 为仓内手写实现，补齐 image 自带的解析能力。JPEG 曾自研 8-bit baseline（与 zune 逐字节一致）但 release 慢 2.13×，按「无效即回滚」未并入，维持 zune（见 docs 交接）。

## 支持格式

| 方向 | 格式 | 后端 | 备注 |
|---|---|---|---|
| decode | PNG | 自研 pngsimd | 16bit / Adam7 / 调色板 / 1–4bit |
| decode | WebP | 自研 container + VP8L + VP8 + ALPH | 有损 / 无损 / alpha / lossy-alpha；动画走 image-webp 兜底 |
| decode | GIF | 自研 容器+LZW+画布 | 动画合成（disposal 链 / 交织 / NETSCAPE loop） |
| decode | JPEG | zune-jpeg | baseline + progressive + CMYK |
| decode | BMP | kimg 手写 | 含 RLE / bitfields |
| decode | ICO | kimg 手写（容器） | PNG + BMP 条目 |
| decode | PNM | kimg 手写 | P1–P7 / PAM |
| decode | TIFF | tiff crate | — |
| encode | PNG | 自研 png_encoder | fdeflate + adler32 自产，零 png crate 运行时依赖 |
| encode | GIF | 自研 LZW | 精确调色板 / Wu 量化 + NETSCAPE loop |
| encode | WebP / JPEG | 无 | JPEG 测试夹具走 image_viewer 自包含最小 baseline 编码器（原 libjpeg-turbo-rs 参照已移除） |

## 性能对比（关键交付）

口径：release / opt-level=3、本机、多轮**中位**。语料 `target/imgbench-corpus`（PNG 34 真图；WebP 116+ 含 2400×3600 大图）。

**PNG**（imgbench corpus，34 真图）：

| 参照 | corpus 相对时间 | 说明 |
|---|---|---|
| vs `png` crate | ≈0.85~0.92（反超） | 灰度 / 调色板 / 16bit 快，rgba8 ≈1.0 |
| vs libpng 1.6.0（vendored，SSE2） | ≈0.70（≈快 1.4×） | 全场景快 |

关键点：双核管道（inflate∥unfilter+expand）、Paeth FPNGE 全 8 位 lane SSE4.1/AVX2、IDAT CRC 跳过、`unfilter_to` 零额外拷贝。

**WebP**（webp_release_bench，语料 116+）：

| 模式 | vs image-webp | vs libwebp 1.6.0 SIMD | 说明 |
|---|---|---|---|
| lossy | ≈0.74×（快 26%） | ≈1.51×（起点 2.4×） | 自研接管静态族 |
| lossless | ≈1.02×（当前语料，小中图） | ≈1.18×（当前语料） | 熵层未 bundle，头号优化点；2400×3600 大图生成入 corpus 后待复测 |

SIMD：YUV→RGB SSE4.1（2.5×）、帧内预测 SSSE3（端到端 −9%）、IDCT SSE2（−4%）、VP8L subtract-green / argb→rgba SSE；环路滤波三轮实证无益→维持标量。多线程：VP8 双线程重建∥滤波管道（libwebp `-mt` 式，宽图 1.62–1.72×）。

**GIF**（gif_jpeg_bmp_release_bench，7 个动画 7–8MB）：

| 后端 | 动画中位 | 说明 |
|---|---|---|
| 旧：gif crate 0.14 | 1475ms | — |
| 新：自研（本会话交付） | **1130ms（-23.5%）** | 解码与 gif crate 逐字节一致（corpus + 夹具双路 oracle、编码双向回环）；gif 降 dev-dep 仅作对照 |

**BMP**（gif_jpeg_bmp_release_bench，6 张 10–26MB）：

| 版本 | 总中位 | 说明 |
|---|---|---|
| 前 | 220ms | 逐像素 3 字节 read + reverse |
| 后（本会话交付） | **189ms（-14%）** | 24/32bit 通道重排 SSSE3/AVX2、16bit/bitfields 整行读 + 256 LUT、`KIMG_BMP_NO_SIMD` 标量回退；SIMD≡标量≡旧语义（31 金指） |

**JPEG**：维持 `zune-jpeg`（300KB~570KB 语料 315–328ms；turbo SIMD 参照 235ms，参照通道已移除）。自研 8-bit baseline 曾达与 zune 逐字节一致（19/19，含 SIMD 色转），但 release 仍慢 2.13×（360.8 vs 169.4ms，熵+IDCT 标量吞吐不足），已按「无效即回滚」撤销，作为后续工作记录在 docs 交接。

结论如实：lossy 静态 WebP、PNG 主路径、GIF 动画、BMP 均反超/持平既有参照；lossless WebP 与 JPEG 保持既有后端不强改。

## 引擎开关

| 变量 | 取值 | 说明 |
|---|---|---|
| `KIMG_WEBP_BACKEND` | `ours` / `imagewebp`（已忽略） / `auto` | 默认与 `auto` = 自研（自包含，无 image-webp 运行时）；`imagewebp` 值接受但忽略并提示一次，等同 ours |
| `KIMG_WEBP_NO_SIMD` | `1` | 关闭 WebP SIMD 路径 |
| `KIMG_WEBP_THREADS` | `0` / `1` / `2` / `auto` | VP8 双线程重建∥滤波 |
| `KIMG_PNG_LEVEL` | `A` / `B` / `auto` | unfilter 引擎档位 |
| `KIMG_PNG_PIPELINE` | `1` | 双核 inflate∥unfilter+expand 管道 |
| `KIMG_PNG_PAETH_LEGACY` | `1` | 回退标量 Paeth 实现 |
| `KIMG_BMP_NO_SIMD` | `1` | 关闭 BMP 通道重排/16bit LUT 的 SIMD 路径（量化 SIMD 收益） |

## 方法与复现

```powershell
cargo bench -p imgbench --bench png_release_bench [--features libpng-ref]
cargo bench -p imgbench --bench webp_release_bench --features libwebp-ref  # libwebp-sys vendored 1.6.0 SIMD
cargo bench -p imgbench --bench gif_jpeg_bmp_release_bench  # GIF/BMP/JPEG 三场景（JPEG turbo 参照列已移除，恒 N/A）
```

- 语料：`target/imgbench-corpus`（gitignore，不入仓库）+ 外部 `E:\Rust\SimdCodecs\测试图片`；kimg 测试经 `tests/fixtures/webp/mod.rs` 解析外部语料，缺则 SKIP。
- 方法学：release / opt-level=3、本机、多轮中位；debug ≈ release 的 ~10×，只看相对变化。

## 已知缺口 / 边界

- WebP 动画未自研（image-webp 兜底）。
- 已知 10 个 webp 小/大文件 vs image-webp/libwebp 有记录未判失败的 diff（详见 docs 交接 §3.2/§4 与 `VP8_EXTRA_DIFF`/`VP8L_EXTRA_DIFF` 打印），不作决断、只如实记录。
- 16bit PNG / BMP RLE / TIFF 真机样例缺（parity 靠合成/语料）。
- WebP / JPEG 编码无（JPEG 测试夹具走 image_viewer 自包含最小 baseline 编码器）。
- APNG 不支持（与 `image 0.25` 默认一致）。

## 许可 / 归属

kimg 本体 Apache-2.0 自持；运行时后端 `image-webp`（WebP 动画兜底）/`tiff`/`zune-jpeg`/`zune-core` 各依其自身许可证；`gif`/`png`/`zlib-rs` 等仅 dev-dependencies（oracle/夹具对照）不入运行时；fixtures golden 源 sdcb/SimdCodecs 为 MIT。
