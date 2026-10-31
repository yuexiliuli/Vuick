# kimg

纯 Rust 图像解码/编码 crate，`image 0.25.10` 的 API 兼容镜像，供 gpui-editor 全仓泛用解码路径（`ImageReader→decode→to_rgba8`）直接替换 crates.io `image`。许可 Apache-2.0 自持，不引入 AGPL 等传染性依赖。

与 `image` 的关系：PNG、WebP 为仓内自研核心（PNG 全链路 pngsimd + png_encoder；WebP container + VP8L + VP8 + ALPH），GIF/JPEG/TIFF 复用既有第三方纯 Rust 后端（JPEG 为 `zune-jpeg`，与 image 同源保证字节级一致性）；BMP/ICO/PNM 为仓内手写实现，补齐 image 自带的解析能力。

## 支持格式

| 方向 | 格式 | 后端 | 备注 |
|---|---|---|---|
| decode | PNG | 自研 pngsimd | 16bit / Adam7 / 调色板 / 1–4bit |
| decode | WebP | 自研 container + VP8L + VP8 + ALPH | 有损 / 无损 / alpha / lossy-alpha；动画走 image-webp 兜底 |
| decode | GIF | gif crate | 动画合成 |
| decode | JPEG | zune-jpeg | baseline + progressive + CMYK |
| decode | BMP | kimg 手写 | 含 RLE / bitfields |
| decode | ICO | kimg 手写（容器） | PNG + BMP 条目 |
| decode | PNM | kimg 手写 | P1–P7 / PAM |
| decode | TIFF | tiff crate | — |
| encode | PNG | 自研 png_encoder | fdeflate + adler32 自产，零 png crate 运行时依赖 |
| encode | GIF | gif crate | — |
| encode | WebP / JPEG | 无 | JPEG 测试夹具走 libjpeg-turbo-rs |

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
| lossless | ≈1.29×（慢） | ≈1.5× | 熵层未 bundle，头号优化点 |

SIMD：YUV→RGB SSE4.1（2.5×）、帧内预测 SSSE3（端到端 −9%）、IDCT SSE2（−4%）、VP8L subtract-green / argb→rgba SSE；环路滤波三轮实证无益→维持标量。多线程：VP8 双线程重建∥滤波管道（libwebp `-mt` 式，宽图 1.62–1.72×）。

结论如实：lossy 静态 WebP 与 PNG 主路径均反超既有参照，lossless WebP 仍慢于 image-webp；不做「凭空加速」的夸大表述。

## 引擎开关

| 变量 | 取值 | 说明 |
|---|---|---|
| `KIMG_WEBP_BACKEND` | `ours` / `imagewebp` / `auto` | AUTO 默认仍 image-webp，ours 接管静态族 |
| `KIMG_WEBP_NO_SIMD` | `1` | 关闭 WebP SIMD 路径 |
| `KIMG_WEBP_THREADS` | `0` / `1` / `2` / `auto` | VP8 双线程重建∥滤波 |
| `KIMG_PNG_LEVEL` | `A` / `B` / `auto` | unfilter 引擎档位 |
| `KIMG_PNG_PIPELINE` | `1` | 双核 inflate∥unfilter+expand 管道 |
| `KIMG_PNG_PAETH_LEGACY` | `1` | 回退标量 Paeth 实现 |

## 方法与复现

```powershell
cargo bench -p imgbench --bench png_release_bench [--features libpng-ref]
cargo bench -p imgbench --bench webp_release_bench --features libpng-ref  # libwebp-sys vendored 1.6.0 SIMD
```

- 语料：`target/imgbench-corpus`（gitignore，不入仓库）+ 外部 `E:\Rust\SimdCodecs\测试图片`；kimg 测试经 `tests/fixtures/webp/mod.rs` 解析外部语料，缺则 SKIP。
- 方法学：release / opt-level=3、本机、多轮中位；debug ≈ release 的 ~10×，只看相对变化。

## 已知缺口 / 边界

- WebP 动画未自研（image-webp 兜底）。
- 已知 10 个 webp 小/大文件 vs image-webp/libwebp 有记录未判失败的 diff（详见 docs 交接 §3.2/§4 与 `VP8_EXTRA_DIFF`/`VP8L_EXTRA_DIFF` 打印），不作决断、只如实记录。
- 16bit PNG / BMP RLE / TIFF 真机样例缺（parity 靠合成/语料）。
- WebP / JPEG 编码无（JPEG 测试夹具走 libjpeg-turbo-rs）。
- APNG 不支持（与 `image 0.25` 默认一致）。

## 许可 / 归属

kimg 本体 Apache-2.0 自持；后端 `gif`/`image-webp`（动画兜底）/`tiff`/`zune-jpeg`/`zune-core` 各依其自身许可证；fixtures golden 源 sdcb/SimdCodecs 为 MIT。
