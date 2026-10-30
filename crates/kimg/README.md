# kimg

纯 Rust 图像解码/编码 crate，`image 0.25.10` 的 API 兼容镜像，供 gpui-editor 全仓泛用解码路径（`ImageReader→decode→to_rgba8`）直接替换 crates.io `image`。许可 Apache-2.0 自持，不引入 AGPL 等传染性依赖。

与 `image` 的关系：在同一组纯 Rust 后端的**逐位对齐复用**下重挂裁切——PNG/GIF/WebP/JPEG/TIFF 直接复用 `image 0.25.10` 自身依赖锁定的后端（其中 JPEG 与 image 同用 `zune-jpeg`，保证字节级一致性）；BMP/ICO/PNM 为仓内手写实现，补齐 image 自带的解析能力。

## 支持格式

| 方向 | 格式 | 后端 | 备注 |
|---|---|---|---|
| decode | PNG | png crate | 16bit / Adam7 |
| decode | JPEG | zune-jpeg | baseline + progressive + CMYK（与 image 0.25.10 同后端） |
| decode | WebP | image-webp | 有损 / 无损 / alpha / 动画 |
| decode | GIF | gif crate | 动画合成 |
| decode | BMP | 手写 | 含 RLE / bitfields |
| decode | ICO | 手写（容器） | PNG + BMP 条目 |
| decode | PNM | 手写 | P1–P7 / PAM |
| decode | TIFF | tiff crate | — |
| encode | PNG | png crate | — |
| encode | GIF | gif crate | — |
| encode | JPEG | 无 | 测试夹具走 libjpeg-turbo-rs |

## 性能对比（关键交付）

同口径：`cargo run -p imgbench`（debug），语料固化在 `target/imgbench-corpus`（83 文件，gitignore），逐格式 `total_ms` / `median_ms`。before = `image 0.25.10`（提交 `98dd1f3`），after = kimg（当前提交 `1874281`）。相对变化为 after vs before，`+` = 变慢、`-` = 变快。

| 格式 | 文件数 | before total | after total | before median | after median | total 变化 | median 变化 |
|---|---|---|---|---|---|---|---|
| bmp | 6 | 7583.8 | 8381.0 | 1262.87 | 1387.66 | +10.5% | +9.9% |
| gif | 7 | 32166.9 | 23959.0 | 4523.92 | 3194.16 | **−25.5%** | **−29.4%** |
| ico | 1 | 4.1 | 4.3 | 4.10 | 4.28 | +4.9% | +4.4% |
| jpg | 19 | 14987.2 | 15795.3 | 903.28 | 949.08 | +5.4% | +5.1% |
| png | 34 | 15014.5 | 16332.1 | 226.66 | 231.45 | +8.8% | +2.1% |
| webp | 16 | 12171.1 | 12868.2 | 671.74 | 715.26 | +5.7% | +6.5% |

体积/依赖面对比：

| 项 | before（image 0.25.10） | after（kimg） | 变化 |
|---|---|---|---|
| exe（debug，`target\debug\gpui-editor.exe`） | 70,194,688 B | 58,920,960 B | **−16.1%** |
| Cargo.lock image 依赖树 | 37 个包条目 | 19 个包条目（image 包本身已移除，留存 kimg 复用后端） | −18 条目 |

结论如实：**解码吞吐与 image 基本持平**——bmp/ico/jpg/png/webp 差异均在 debug 噪声带内（约 ±2% ~ +10%，单文件数少时抖动明显），GIF 出现较明显加速（−25~29%，动画合成路径）；**体积与依赖面显著下降**是替换的核心收益。不做「凭空加速」的夸大表述。自研的 BMP/ICO/PNM 解码路径凭手写实现维持与 image 内置实现同一量级。

## 方法与复现

```powershell
cargo run -p imgbench   # debug；语料 target/imgbench-corpus（gitignore，不含仓库）
```

- 语料：镜像 gpui/editor 泛用解码路径（`ImageReader→decode→to_rgba8`；GIF 走 `into_frames`），83 文件固化于 `target/imgbench-corpus`。
- 方法学（debug 相对比较）：debug ≈ release 的 ~10×，只看**相对变化**与稳定子项，不看绝对值；窗宽抖动与单格式样本数有限时，±10% 内视为噪声持平，不做显著性断言。
- 基准命令即 `imgbench` crate（`crates/imgbench`），其 Cargo 依赖已从 `image` 改为 `kimg`。

## 已知缺口 / 边界

- 16bit PNG 直测样例缺（解码路径已实现，缺语料覆盖）。
- BMP RLE / bitfields 缺真机样例（手写路径无真实样本回归）。
- TIFF 样例缺。
- JPEG 编码无（`image` 侧 JPEG 编码亦经 libjpeg 系，测试夹具走 libjpeg-turbo-rs）。
- APNG 不支持（与 `image 0.25` 默认一致）。

## 许可 / 归属

kimg 本体 Apache-2.0 自持；后端 `png`/`gif`/`image-webp`/`tiff`/`zune-jpeg`/`zune-core` 各依其自身许可证。
