# Benchmarks/png — kimg_png vs C# Sdcb.SimdCodecs.Png（RGBA8 单核性能战役）

自研 PNG 引擎（kimg_png 的 EXPAND 引擎，字节恒等 png crate `Transformations::EXPAND`）
vs C# `Sdcb.SimdCodecs.Png.PngImageDecoder.TryDecodeIntoRgba8` 的**单核** RGBA8 对账套件。
目标主口径：**geomean(ours/C#)，RGBA8，单核（`KIMG_PNG_PIPELINE=0`，关双核管道）**。

## 布局

```
Benchmarks/png/
  README.md  .gitignore（corpus/logs/probe target 不入库）
  csharp_runner/  PngRef.csproj（SDCB_ROOT 缺省 <repo>\..\SimdCodecs，当前项目父目录）+ Program.cs
      `ref <corpusDir> <refOutDir>`：逐文件 TryDecodeIntoRgba8 计时（全集暖机 1 遍 + 每文件
        预热 2 + PNG_REF_ROUNDS=9 中位，对齐 jpeg 2026-11-08 公平化姿势）+ RGBA8 dump 到
        target/csharp-ref/<stem>.<w>x<h>.rgba + `| name | ms |` 表（UTF-8）
      `gen <srcPng> <outDir> <tag>`：比较集扩充（PngWriter 保真变体 rgba8 / p8a）
  probe/  独立 workspace crate（显式 opt-level=3）：逐文件 kimg_png 解码 N（KIMG_PNG_ROUNDS，
      缺省 9）轮中位墙钟，进程内强制 KIMG_PNG_PIPELINE=0（单核口径）；
      输出 `KIMG_PNG_VS_CSHARP perfile ... ours=<ms>` + AGG（geomean、总 ms）
  compare_refs.py  解析 perfile × C# 表 → geomean ours/C# + Sigma + worst 5（比较集 = layout=rgba8）
  scripts/run_csharp_ref.ps1   重生成 png_csharp.log + target/csharp-ref
  scripts/run_acceptance.ps1   oracle 闸(cargo test -p kimg_png) → 静置 60s → release 探针(pipeline off)
      → compare_refs → 主口径尾行
  logs/（gitignore）
```

对照测试（oracle 硬闸）：`crates/kimg_png/tests/png_csharp_parity.rs` — 比较集 RGBA8-natural
文件 kimg 自然输出 vs C# rgba dump 逐字节一致；非 RGBA8-natural（RGB8/gray8，布局不同）SKIP。

## 语料与比较集

- 语料 `Benchmarks/corpus/PNG`：34 真图（RGB8×10 + RGBA8×24，非隔行，全 8-bit）。
- **phase-1 比较集 = RGBA8 布局双端可比**：24 张真 RGBA8-natural（ct=6）+ 生成比较集例子
  （`33__sdcb-p8a.png` 等，见下）。RGB8/gray8 布局不同，**排除**在比较集外（不许跨布局比时）。
- 生成比较集例子（`PngRef gen` 或 PIL）：
  - `<stem>__sdcb-rgba8.png`：Sdcb `PngWriter.Encode` 非隔行 RGBA8（Sdcb 自家压缩流）
  - `<stem>__sdcb-p8a.png`：`PngWriter.Encode + LosslessReduction`（≤256 色 → PLTE+tRNS；kimg
    EXPAND 后仍 RGBA8，字节可比）
  - `<stem>__adam7.png`：Adam7 隔行 RGBA8（PIL interlace=True 生成）
  生成后放进语料目录即被 ref/probe/parity 统一收进比较集；`run_csharp_ref.ps1` 重跑会清空
  `target/csharp-ref` 防止旧例混入。

## 一键跑法

```powershell
# 1) C# 参照表 + RGBA8 dump（target/csharp-ref）
pwsh -File Benchmarks\png\scripts\run_csharp_ref.ps1

# 2) acceptance：oracle 闸 → 静置 → probe（单核）→ compare_refs → 主口径
pwsh -File Benchmarks\png\scripts\run_acceptance.ps1      # -NoWait 跳过静置 60s
```

## 全新机器装配（git clone 后）

参照 DLL（gitignore，不入库）：`bin/libpng16.dll + bin/zlib1.dll` 缺失时自动从 MSYS2 固定版本装配。

```powershell
pwsh -File Benchmarks\png\scripts\ensure_refs.ps1     # 缺失 → 下载+解压+SHA 校验+就位；已就位 → no-op
pwsh -File Benchmarks\png\scripts\run_acceptance.ps1  # 前置检查会自动 ensure；仍缺才报错并给出手动命令
```

- **DLL 来源 = MSYS2 mingw-w64 官方打包（SHA-256 钉版）**：`mingw-w64-x86_64-libpng-1.6.59-2`
  （`libpng16-16.dll` → `libpng16.dll`）+ `mingw-w64-x86_64-zlib-1.4-2`（`zlib1.dll`），URL/SHA 见
  `scripts/ensure_refs.ps1` 顶部固定版本表；两 DLL 同放 `bin/`（探针 `LOAD_WITH_ALTERED_SEARCH_PATH` 从
  libpng16.dll 自身目录解析 zlib，无需 PATH）。
- **C# 参照**需 .NET（10.x，`%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe`）+ Sdcb.SimdCodecs 源
  （`SDCB_ROOT`，缺省 `<repo>\..\SimdCodecs`（当前项目父目录），或 clone `https://github.com/sdcb/Sdcb.SimdCodecs`）：
  ```powershell
  pwsh -File Benchmarks\png\scripts\run_csharp_ref.ps1   # 生成 Benchmarks/corpus/csharp/png_csharp.log + target/csharp-ref
  ```
- **语料** `Benchmarks/corpus/PNG`（gitignore，不入库）：跨机需从原机拷贝，或用 `PngRef gen` 按上文
  「语料与比较集」生成扩充例重建；缺失时探针/测试 SKIP 而非回归。

分账（ours 内部时间去哪）：`stage-profile` feature + 分账跑法见 `probe/src/main.rs` 头注：
`KIMG_PNG_STAGE_PROFILE=1`（需 `--features stage-profile` 构建）输出 inflate/unfilter/expand/copy/wall；
`KIMG_PNG_NO_SIMD=1` 量化 SIMD 收益。

## 已知口径与纪律

- 探针/probe 一律 release（opt-level=3）；GUI 不打开；长命令前台阻塞到结束（K86）。
- 比较集只收 RGBA8 布局（跨布局计时可比性不成立）；`compare_refs.py` 只按 `layout=rgba8` 计 geomean。
- 对账契约（勿改前缀）：`KIMG_PNG_VS_CSHARP`、`STAGE`、C# 表 `| `name` | ms |`、日志 UTF-8。
