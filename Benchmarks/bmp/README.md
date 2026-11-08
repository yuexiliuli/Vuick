# Benchmarks/bmp — kimg_bmp vs C# Sdcb.SimdCodecs.Bmp（RGBA8 单核性能对账）

自研 BMP 解码器（kimg_bmp，纯 std，SSSE3/AVX2 行内核，`KIMG_BMP_NO_SIMD` 可关）
vs C# `Sdcb.SimdCodecs.Bmp.BmpImageDecoder.TryDecodeIntoRgba8` 的**单核** RGBA8 对账套件。
目标主口径：**geomean(ours/C#)，RGBA8，单核**，并给出**按布局类别的 geomean 表**
（布局 = 文件名 `__<layout>` 后缀：rgb24 / rgb24-topdown / bgra32 / argb32-v4 / rgb555 /
rgb565 / p8 / p4 / p1 / rle8 / rle4 / p2 / rgb565-444；无后缀的原生 26 张 = native）。

## 布局

```
Benchmarks/bmp/
  README.md  .gitignore（logs/probe target/csharp-ref 不入库；corpus 由 Benchmarks/.gitignore 统一忽略）
  csharp_runner/  BmpRef.csproj（SDCB_ROOT 缺省 <repo>\..\SimdCodecs，当前项目父目录）+ Program.cs
      `ref <corpusRoot> <refOutDir>`：逐 BMP（<corpusRoot>/BMP + /BMP-MATRIX）TryDecodeIntoRgba8
        计时（全集暖机 1 遍 + 每文件 warmup2 + BMP_REF_ROUNDS=9 中位，对齐 jpeg/png 2026-11-08
        公平化姿势）+ RGBA8 dump 到 target/csharp-ref/bmp/<stem>.<w>x<h>.rgba + `| name | ms |` 表
      `gen <corpusRoot> <outDir>`：photo 源 A/B（BMP/001/002.bmp）与合成 screen 源各出 11 布局
        变体 + hand-rolled p2/444（singles 源）→ <outDir>/BMP-MATRIX（覆盖式重生成，输出 gen 清单）
  probe/  独立 workspace crate（显式 opt-level=3）：逐文件 kimg_bmp 解码 N（KIMG_BMP_ROUNDS，
      缺省 9）轮中位墙钟；Rgb8 → 补尾巴 RGBA8 归一；输出 `KIMG_BMP_VS_CSHARP perfile ... ours=<ms>`
      + AGG（geomean、总 ms）
  compare_refs.py  解析 perfile × C# 表 → 总体 geomean ours/C# + Sigma + worst 5
      + **按布局类别 geomean 表**（类别 = `__<layout>` 后缀）
  scripts/run_csharp_ref.ps1   缺矩阵 → gen；重生成 bmp_csharp.log + target/csharp-ref/bmp
  scripts/run_acceptance.ps1   前置检查 → oracle 闸(cargo test -p kimg_bmp) → 静置 60s → release
      探针 → compare_refs → 主口径尾行
  logs/（gitignore）
```

对照测试（oracle 硬闸）：`crates/kimg_bmp/tests/bmp_csharp_parity.rs` — 对语料（BMP + BMP-MATRIX）
kimg 自然输出（Rgb8 补 alpha=FF / Rgba8 直用）与 C# rgba dump 逐字节对比；缺 dump → SKIP
（说明先跑 run_csharp_ref）。**2026-11-08 对账发现的已知分歧（均只分类不修实现，修复决策归主 agent）**：
- **kimg 32bpp BI_RGB 通道旋转 bug**（compression=0，`ImageType::RGB32`，如 `__bgra32`）：kimg 按
  `[X,B,G,R]` 读标准 `[B,G,R,A]`，输出 `R=保留/alpha 字节、G=源R、B=源G` 并丢源 B。`__argb32-v4`
  （BI_BITFIELDS 带 alpha 掩码）走 `[B,G,R,A]→[R,G,B,A]` 正确路径、与 C# 全字节相等，证明仅
  BI_RGB 32bit 路径异常。
- **kimg 16bit 通道展宽语义**（`__rgb555`/`__rgb565`）：kimg 按 image-0.25 查表 round(v*255/max)，
  C# 按位重复（browsers/WIC）→ 部分 5/6-bit 取值不同（如 5-bit 3 → 25 vs 24）。`__rgb565-444`
  （4-bit BITFIELDS）两者一致（i*17）→ 全等。
- **kimg RLE4/RLE8 bug**（`__rle4`/`__rle8`）：absolute 模式奇数补齐（pad 非像素）被当像素 → 行尾
  溢出崩溃（001/002 大图 `Corrupt RLE data`）或尾像素错色；且 4bit 相对 run 查表
  `palette[value]` 未取高 nibble → 大面积错色（screen 小图可解码但颜色错误）。该路径整体与 C#
  不等 → 记 divergence；kimg RLE 目前**无有效解码**，探针侧相应 SKIP、不计入 geomean。
parity 其余布局 RGB 不等 → fail（真实未知 bug）。

## 语料与比较集

- 语料 `Benchmarks/corpus/BMP`：26 原生真 BMP（001–020 用户大图 10–26MB，021–026 既有 24bit 大图）。
- `Benchmarks/corpus/BMP-MATRIX`：`BmpRef gen` 生成（覆盖率镜像 Sdcb 官方 Bmp 基准矩阵）：
  - photo 源 A/B（001/002 的 RGBA 母本）+ 合成 screen 源（1920×1080 桌面 UI 风，带半透明阴影 →
    真实 alpha，覆盖 bgra32/argb32-v4 的 alpha 语义）× 11 布局（rgb24、rgb24-topdown、bgra32、
    argb32-v4、rgb555、rgb565、p8、p4、p1、rle8、rle4）。
  - `singles` 源（固定 4 色）出 hand-rolled `p2`（2-bit 调色板 BI_RGB）与 `rgb565-444`
    （16bit BITFIELDS 4-4-4 掩码）——Sdcb 编码器产不出、手写 BMP 文件（Program.cs 内低层写入器）。
- 全部生成文件命名 `<source>__<layout>.bmp` → compare_refs 自动分类；矩阵文件解码回读与源
  逐字节一致（lossless 布局）或按量化后调色板一致（palette/RLE 布局）；不被既有 `bmp_parity.rs`
  金指纹测试触碰（它只扫 target/imgbench-corpus/BMP，未知文件会 panic，故矩阵独立目录）。

## 一键跑法

```powershell
# 1) 矩阵生成 + C# 参照表 + RGBA8 dump（target/csharp-ref/bmp）
pwsh -File Benchmarks\bmp\scripts\run_csharp_ref.ps1

# 2) acceptance：前置(缺矩阵/ref 自动补) → oracle 闸 → 静置 → probe → compare_refs → 主口径
pwsh -File Benchmarks\bmp\scripts\run_acceptance.ps1      # -NoWait 跳过静置 60s
```

## 全新机器装配（git clone 后）

- **C# 参照**需 .NET（10.x，`%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe`）+ Sdcb.SimdCodecs 源
  （`SDCB_ROOT`，缺省 `<repo>\..\SimdCodecs`（当前项目父目录），或 clone
  `https://github.com/sdcb/Sdcb.SimdCodecs`）：
  ```powershell
  pwsh -File Benchmarks\bmp\scripts\run_csharp_ref.ps1   # 生成矩阵 + bmp_csharp.log + target/csharp-ref/bmp
  ```
- **语料** `Benchmarks/corpus/BMP`（gitignore，不入库）：跨机需从原机拷贝；缺 001/002 → gen 报错，
  矩阵缺失时探针/parity SKIP 而非回归。原生 26 张不在时矩阵仍可由合成 screen/singles 生成并可跑，
  但 native 类别缺失。

## 已知口径与纪律

- 探针/probe 一律 release（opt-level=3）；GUI 不打开；长命令前台阻塞到结束（K86）。
- BMP 全部文件 RGBA8 布局双端可比（无 png 的 layout 排除）；比较集 = 双路都有且 layout=rgba8。
- 时序口径（2026-11-08 记录）：kimg 的 RGB8→RGBA8 补 alpha 在**计时循环外**一次完成（计时只测
  自然输出解到 3B/px 缓冲），C# 每调用恒 4B/px 满写 —— 故 Rgb8 自然输出文件（24bit/16bit/调色板/
  RLE 等）偏轻量带宽，**轻微偏向 kimg**（<1.3× 内存带宽差）。该偏差不翻转结论：产物字节与 C#
  全等，2–4× 级真实差距可明确区分。
- 对账契约（勿改前缀）：`KIMG_BMP_VS_CSHARP`、C# 表 `| `name` | ms |`、日志 UTF-8。
- 覆盖面缺口（C# 独有，不进对账）：ICO/CUR 容器（kimg 在 kimg_ico）、OS/2 RLE24、Huffman1D、
  BI_PNG 内嵌 PNG；kimg 不支持这些 → 不生成对应语料。
- 已知语义分歧：见上文 parity 段落（32bpp-BI_RGB 通道旋转、16bit 展宽取整、RLE4/RLE8 解析 bug，
  均只分类不修实现；修复决策归主 agent）。RLE 布局结果是「kimg 无有效解码」：001/002 大图直接
  解码失败被探针 SKIP、不计入比较集；screen 小图可解码但颜色错误 → 仍在比较集（计时有效、字节
  不可信），rle4/rle8 类别当前各只有 1 文件。
