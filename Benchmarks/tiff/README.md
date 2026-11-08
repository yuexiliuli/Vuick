# Benchmarks/tiff — kimg_tiff vs C# Sdcb.SimdCodecs.Tiff（RGBA8 单核性能对账）

自研 TIFF 解码器（kimg_tiff，`TiffDecoder`：8bit L8/RGB8/RGBA8 + None/PackBits/LZW/Deflate/
ZlibPkzip + Horizontal predictor(2)）vs C# `Sdcb.SimdCodecs.Tiff.TiffImageDecoder.TryDecodeIntoRgba8`
的**单核** RGBA8 对账套件。
目标主口径：**geomean(ours/C#)，RGBA8，单核**，并给出**按布局类别的 geomean 表**
（类别 = 文件名 `__<layout>_<comp>[_pred]` 后缀；另有 2 条多 strip 用例 `*_rps<N>`）。

## 布局

```
Benchmarks/tiff/
  README.md  .gitignore（logs/probe target/gen target/csharp-ref 不入库；corpus 由 Benchmarks/.gitignore 统一忽略）
  csharp_runner/  TiffRef.csproj（SDCB_ROOT 缺省 <repo>\..\SimdCodecs，当前项目父目录）+ Program.cs
      `ref <corpusRoot> <refOutDir>`：逐 <corpusRoot>/TIFF/*.tif TryReadInfo + TryDecodeIntoRgba8
         计时（全集暖机 1 遍 + 每文件 warmup2 + TIFF_REF_ROUNDS=9 中位，对齐 bmp 2026-11-08
         公平化姿势）+ RGBA8 dump 到 target/csharp-ref/tiff/<stem>.<w>x<h>.rgba + `| name | ms |` 表
      **无 gen 子命令**：语料由 Rust 侧 gen crate 生成（C# TiffWriter 压缩只支持
         Uncompressed/Deflate，产不出 LZW/PackBits 矩阵）
  gen/  独立 workspace crate（显式 opt-level=3）：语料生成器 —— 源 = BMP 001/002（kimg 解 RGBA，
      缩略到最大边 1024 内）+ 合成 screen（1920×1080 桌面 UI 风）；tiff crate 编码器出
      3 布局 × 4 压缩 × 2 predictor + 2 条多 strip（RPS=64/1）→ <corpusRoot>/TIFF（覆盖式重生成）
  probe/  独立 workspace crate（显式 opt-level=3）：逐文件 kimg_tiff TiffDecoder 解码 N
      （KIMG_TIFF_ROUNDS，缺省 9）轮中位墙钟；L8/RGB8 补 alpha=FF → RGBA8 归一；
      输出 `KIMG_TIFF_VS_CSHARP perfile ... ours=<ms>` + AGG（geomean、总 ms）
  compare_refs.py  解析 perfile × C# 表 → 总体 geomean ours/C# + Sigma + worst 5
      + **按布局类别 geomean 表**（类别 = `__` 后缀）
  scripts/run_gen.ps1   编译运行 gen（release）→ corpus\TIFF
  scripts/run_csharp_ref.ps1   缺语料 → run_gen.ps1；重生成 tiff_csharp.log + target/csharp-ref/tiff
  scripts/run_acceptance.ps1   前置检查 → oracle 闸(cargo test -p kimg_tiff) → release gen+probe
      构建 → 静置 60s → 探针 → compare_refs → 主口径尾行
  logs/（gitignore）
```

对照测试（oracle 硬闸）：`crates/kimg_tiff/tests/tiff_csharp_parity.rs` — 对语料（corpus/TIFF）
自研 `TiffDecoder` 自然输出补 alpha=FF（L8→LLL FF、RGB8→RGB FF、RGBA8 直用）与 C# rgba dump
逐字节对比；缺 dump → SKIP（说明先跑 run_csharp_ref）。任何 8bit 语料文件解不开（Unspported/坏文件）
→ fail（提示 gen 漏格或真实自研 bug）。

## 语料矩阵（每个源 × 3 布局 × 4 压缩 × 2 predictor）

- 源：`001`、`002`（缩略到 1024 内）、`screen`（合成）。
- 布局 `{gray8, rgb8, rgba8}`；压缩 `{none, packbits, lzw, deflate}`；predictor `{off, on(=Horizontal 2,
  全部 4 压缩都配，含 packbits+pred)}`。
- 命名 `<src>__<layout>_<comp>[_pred].tif` → 3·源 × 12·布局·压缩·pred = 36 格类（各 2 文件）+
  多 strip `001__rgb8_none_rps64.tif`、`screen__rgba8_none_rps1.tif`，共 **74 文件**。
- 全部 8bit chunky、II 端序、classic、单页（范围墙：16bit/CMYK/tile/CCITT/JPEG/BigTIFF/多页不在语料）。

## 一键跑法

```powershell
# 1) 语料生成
pwsh -File Benchmarks\tiff\scripts\run_gen.ps1

# 2) C# 参照表 + RGBA8 dump（target/csharp-ref/tiff）+ tiff_csharp.log
pwsh -File Benchmarks\tiff\scripts\run_csharp_ref.ps1

# 3) oracle 闸（含 tiff_csharp_parity，需 dump 已在）
cargo test -p kimg_tiff -- --test-threads=1

# 4) acceptance：前置(缺语料/ref 自动补) → oracle 闸 → 静置 → probe → compare_refs → 主口径
pwsh -File Benchmarks\tiff\scripts\run_acceptance.ps1    # -NoWait 跳过静置 60s
```

## 全新机器装配（git clone 后）

- **C# 参照**需 .NET（10.x，`%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe`）+ Sdcb.SimdCodecs 源
  （`SDCB_ROOT`，缺省 `<repo>\..\SimdCodecs`（当前项目父目录），或 clone
  `https://github.com/sdcb/Sdcb.SimdCodecs`）：
  ```powershell
  pwsh -File Benchmarks\tiff\scripts\run_csharp_ref.ps1   # 生成语料 + tiff_csharp.log + target/csharp-ref/tiff
  ```
- **语料 `Benchmarks/corpus/BMP`**（gitignore，不入库）：跨机需从原机拷贝；缺 001/002 → gen 报错，
  语料缺时探针/parity SKIP 而非回归。

## 已知口径与纪律

- 探针/probe 一律 release（opt-level=3）；GUI 不打开；长命令前台阻塞到结束（K86）。
- 时序口径（2026-11-08 记录，同 bmp）：kimg 的 L8/RGB8→RGBA8 补 alpha 在**计时循环外**一次完成
  （计时只测自然输出解到 1B/3B 每像素缓冲），C# 每调用恒 4B/px 满写 —— 故 L8/RGB8 自然输出文件
  偏轻量带宽，**轻微偏向 kimg**（<1.3× 内存带宽差）。不翻转结论（产物字节与 C# 全等）。
- 对账契约（勿改前缀）：`KIMG_TIFF_VS_CSHARP`、C# 表 `| `name` | ms |`、日志 UTF-8。
- 覆盖面缺口（C#/自研支持之外，不进对账）：16bit/CMYK/tile/CCITT/JPEG/多页/BigTIFF ——
  范围墙（后置战役）。
- 已知语义分歧：无（8bit L8/RGB8/RGBA8 双端语义一致；predictor2 双端同一差分还原）。
