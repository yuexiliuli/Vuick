# Benchmarks/gif — kimg_gif vs C# Sdcb.SimdCodecs.Gif（RGBA8 单核性能对账）

自研 GIF 解码器（kimg_gif，`GifDecoder`：87a/89a、全局/局部表、隔行、透明、dispose 0–3、
LZW min 1–11、动画 `into_frames`）vs C# `Sdcb.SimdCodecs.Gif`（`GifImageDecoder.TryOpenFrames`
→ 按序 `TryDecodeFrameInto(..., Rgba32)`）的**单核** RGBA8 对账套件。
目标主口径：**geomean(ours/C#)，RGBA8，单核，全帧解码总墙钟**（静态=1 帧），并给出
**按单/动态类别的 geomean 表**（类别 = 文件名 `__<static_*|anim_*>` 后缀首段）。

## 布局

```
Benchmarks/gif/
  README.md  .gitignore（logs/probe target/gen target/csharp-ref 不入库；corpus 由 Benchmarks/.gitignore 统一忽略）
  csharp_runner/  GifRef.csproj（SDCB_ROOT 缺省 <repo>\..\SimdCodecs = 当前项目父目录；引用 Gif + Bmp）+ Program.cs
      `gen <srcDir> <outDir>`：源 = <srcDir>/001.bmp、002.bmp（Sdcb.Bmp 解 RGBA，静态缩 ≤1024 边、
          动画缩 320×240）+ 内建合成 screen（1920×1080 低色数 UI 风）→ GifEncoder/EncodeAnimation
          产 静态(每源×opaque/transparent×exact/wq) + 动画(001/screen×opaque/transparent×loop{0,3})，
          **先清空 <outDir> 防混**；打印生成清单（名/尺寸/帧数/字节数）
      `ref <corpusRoot> <refOutDir>`：逐 <corpusRoot>/GIF/*.gif TryReadInfo + TryOpenFrames →
          按序 TryDecodeFrameInto(..., Rgba32) **全帧总耗时**计时（全集暖机 1 遍 + 每文件 warmup2 +
          GIF_REF_ROUNDS=9 中位，对齐 tiff 2026-11-08 公平化姿势）+ 全帧 RGBA8 拼接
          （帧数×w*h*4，静态=1 帧）dump 到 target/csharp-ref/gif/<stem>.<w>x<h>.rgba
          + `| name | ms |` 表 + `# <name> ... frames=N` 行
  probe/  独立 workspace crate（显式 opt-level=3）：逐文件 kimg_gif GifDecoder —— 静态(1 帧)
      read_image 首帧合成（复用缓冲，对齐 C# 复用 dest）/ 动画 into_frames 逐帧全画布合成总墙钟；
      N（KIMG_GIF_ROUNDS，缺省 9）轮中位；
      输出 `KIMG_GIF_VS_CSHARP perfile ... frames=N ... ours=<ms>` + AGG（geomean、总 ms）
  compare_refs.py  解析 perfile × C# 表 → 总体 geomean ours/C# + Sigma + worst 5
      + **按单/动态类别 geomean 表**（类别 = `__` 后缀首段）
  scripts/run_csharp_ref.ps1   恒跑 gen（先清空防混，-SkipGen 跳过）→ 重生成 gif_csharp.log
      + target/csharp-ref/gif
  scripts/run_acceptance.ps1   前置检查 → oracle 闸(cargo test -p kimg_gif) → release probe
      构建 → 静置 60s → 探针 → compare_refs → 主口径尾行
  logs/（gitignore）
```

对照测试（oracle 硬闸）：`crates/kimg_gif/tests/gif_csharp_parity.rs` — 对语料（corpus/GIF）
kimg_gif 全帧 RGBA（静态 read_image / 动画 into_frames 逐帧拼接；另断言静态 read_image==首帧）
与 C# 全帧 RGBA8 拼接 dump 逐字节对比；缺 dump → SKIP（说明先跑 run_csharp_ref）。任何语料文件
解不开/字节不等 → fail（提示 gen 漏格或真实自研 bug）。

## 语料矩阵（生成式，C# 编码器）

- 源：`001`、`002`（BMP 解 RGBA，静态缩 ≤1024 边）、`screen`（合成 1920×1080 ±255 色 UI 风）。
- 静态 `<src>__static_{opaque,transparent}_{exact,wq}.gif`：3 源 × 2 透明度 × 2 量化 = **12 文件**；
  `exact`=Dither=false（screen ≤255 色 → 调色板精确无损；photo 超额自动走 Wu 不抖动），
  `wq`=Dither=true（Floyd–Steinberg 抖动）；transparent 为确定性打孔 ~1/7 像素 alpha=0。
- 动画 `<src>__anim_{opaque,transparent}_loop{0,3}.gif`：2 源(001/screen, 320×240) × 2 透明度
  × 2 循环 = **8 文件**，每文件 8 帧、每帧 140ms、逐帧水平滚动（transparent 加移动透明圆孔）；
  loop0=无限、loop3=播 3 次。
- 合计 **20 文件**。覆盖面缺口（C# 编码器产不出，README 注明不强补）：隔行、GIF87a、局部调色板。

## 一键跑法

```powershell
# 1) 语料生成 + C# 参照表（全帧 RGBA8 dump + gif_csharp.log）
pwsh -File Benchmarks\gif\scripts\run_csharp_ref.ps1

# 2) oracle 闸（含 gif_csharp_parity，需 dump 已在；另含 gif_oracle）
cargo test -p kimg_gif -- --test-threads=1

# 3) acceptance：前置(缺语料/ref 自动补) → oracle 闸 → 静置 → probe → compare_refs → 主口径
pwsh -File Benchmarks\gif\scripts\run_acceptance.ps1    # -NoWait 跳过静置 60s
```

## 全新机器装配（git clone 后）

- **C# 参照**需 .NET（10.x，`%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe`）+ Sdcb.SimdCodecs 源
  （`SDCB_ROOT`，缺省 `<repo>\..\SimdCodecs`（当前项目父目录），或 clone
  `https://github.com/sdcb/Sdcb.SimdCodecs`）：
  ```powershell
  pwsh -File Benchmarks\gif\scripts\run_csharp_ref.ps1   # 生成语料 + gif_csharp.log + target/csharp-ref/gif
  ```
- **语料 `Benchmarks/corpus/BMP`**（gitignore，不入库）：跨机需从原机拷贝；缺 001/002 → gen 报错，
  语料缺时探针/parity SKIP 而非回归。GIF 语料本身由 gen 每次重建（先清空防混）。

## 已知口径与纪律

- 探针/probe 一律 release（opt-level=3）；GUI 不打开；长命令前台阻塞到结束（K86）。
- 时序口径（2026-11-08 记录，同 tiff）：GIF 恒 RGBA8、双端 4B/px 紧密，无 png/tiff 那种补 alpha
  带宽差；唯 kimg 动画路径经 `into_frames()` 每帧新建 w*h*4 缓冲 + `RgbaImage`（真实 API 行为），
  C# 侧把帧写进每文件复用的一次性 dest —— kimg 动画路径每帧多一次分配，**轻微偏向 C#**；
  静态路径两腿都复用缓冲。动画语料 ≤320×240×8 帧，分配开销小，不翻转同源实现对比结论。
- 对账契约（勿改前缀）：`KIMG_GIF_VS_CSHARP`、C# 表 `| `name` | ms |`、日志 UTF-8。
- 覆盖面缺口（C#/自研支持之外，不进对账）：隔行 / GIF87a / 局部调色板 / dispose 之外的极端坏文件
  —— 范围墙（后置战役；gif_oracle 用 gif crate 夹具另盖 dispose/截断）。
- 已知语义分歧：无（kimg 为 SimdCodecs 同源移植；若 parity 出现 dispose/透明分歧，需停下精读两侧
  实现对齐，不与代码硬凑——见 gif_csharp_parity.rs 顶部注释）。
