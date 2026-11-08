# Benchmarks/ico — kimg_ico vs C# Sdcb.SimdCodecs.Ico（RGBA8 单核性能对账）

自研 ICO 解码器（kimg_ico，`IcoDecoder`：读目录 → 选 Best 条目 → 内嵌 PNG（kimg_png）或内嵌 DIB
（kimg_bmp `new_with_ico_format` + AND 掩码））vs C# `Sdcb.SimdCodecs.Ico`（`IcoDecoder.Decode` → Best
条目 → RGBA8）的**单核** RGBA8 对账套件。
目标主口径：**geomean(ours/C#)，RGBA8，单核，Best 条目解码总墙钟**（两侧都「读目录 + 选 Best + 解码」），
并给出**按类别（单尺寸 {size}_{png,bmp32} / multi / cur）的 geomean 表**。

## 布局

```
Benchmarks/ico/
  README.md  .gitignore（logs/probe target/gen target/csharp-ref 不入库；corpus 由 Benchmarks/.gitignore 统一忽略）
  csharp_runner/  IcoRef.csproj（SDCB_ROOT 缺省 <repo>\..\SimdCodecs = 当前项目父目录；引用 Ico + Bmp）+ Program.cs
      `gen <srcDir> <outDir>`：源 = <srcDir>/001.bmp、002.bmp（Sdcb.Bmp 解 RGBA）+ 内建合成 screen
          （1920×1080 低色数 UI 风）→ 双线性缩放到条目尺寸 → IcoEncoder.Encode/EncodeCursor 产：
          单尺寸 {16,32,48}×{Png,Bmp32}×{opaque,transparent}（每源 12）+ multi（Auto 16,32,48,256，每源 1）
          + cur（32×32 透明，001/screen 各 1）；**先清空 <outDir> 防混**；打印生成清单
      `ref <corpusRoot> <refOutDir>`：逐 <corpusRoot>/ICO/*.ico + *.cur `IcoDecoder.Decode`（Best）
          计时（全集暖机 1 遍 + 每文件 warmup2 + ICO_REF_ROUNDS=9 中位）+ Best 条目 RGBA8 dump 到
          target/csharp-ref/ico/<stem>.<w>x<h>.rgba + `| name | ms |` 表 + `# <name> ...` 行
  probe/  独立 workspace crate（显式 opt-level=3）：逐文件 kimg_ico IcoDecoder::new（读目录+选 Best）
      → read_image（RGBA8）N（KIMG_ICO_ROUNDS，缺省 9）轮中位；
      输出 `KIMG_ICO_VS_CSHARP perfile ... layout=rgba8 ours=<ms>` + AGG（geomean、总 ms）
  compare_refs.py  解析 perfile × C# 表 → 总体 geomean ours/C# + Sigma + worst 5
      + **按类别 geomean 表**（类别 = `__` 后缀：`<size>_<type>` / multi / cur）
  scripts/run_csharp_ref.ps1   恒跑 gen（先清空防混，-SkipGen 跳过）→ 重生成 ico_csharp.log
      + target/csharp-ref/ico
  scripts/run_acceptance.ps1   前置检查 → oracle 闸(cargo test -p kimg_ico) → release probe
      构建 → 静置 60s → 探针 → compare_refs → 主口径尾行
  logs/（gitignore）
```

对照测试（oracle 硬闸）：`crates/kimg_ico/tests/ico_csharp_parity.rs` — 对语料（corpus/ICO）
kimg_ico Best 条目 RGBA8 与 C# Best 条目 RGBA8 dump 逐字节对比，且断言 kimg 尺寸 == dump 文件名
`<w>x<h>`（守卫两侧同选一条）；缺 dump → SKIP（说明先跑 run_csharp_ref）。任何语料文件解不开/字节
不等/尺寸分歧 → fail（提示 gen 漏格或真实自研 bug）。

## 语料矩阵（生成式，C# 编码器）

- 源：`001`、`002`（BMP 解 RGBA）、`screen`（合成 1920×1080 ±255 色 UI 风）。
- 单尺寸 `<src>__<size>_<type>_{opq,trp}.ico`：3 源 × {16,32,48} × {png,bmp32} × {opaque,transparent}
  = **36 文件**；transparent 为确定性打孔 ~1/7 像素 alpha=0（alpha 仅 {0,255}）。
- 多尺寸 `<src>__multi.ico`：Auto（16,32,48 BMP32 + 256 PNG，全 32bpp 深度）→ 3 文件；
  **统一 32bpp 深度**使 kimg(位深→面积) 与 C#(面积→位深) 的 Best 选帧一致（选 256）。
- 光标 `<src>__cur.ico`：32×32 透明光标（热点 (5,5)，Auto → BMP32）→ 2 文件。
- 合计 **41 文件**。
- **SKIP 面（不进语料，README 注明）**：混合位深容器（Best 选帧分歧）与 32bpp 中间 alpha
  （DIB 路径 C#「有非零像素 alpha 用像素 alpha、全 0 才用 AND 掩码」vs kimg「无条件应用 AND 掩码」分歧）。

## 一键跑法

```powershell
# 1) 语料生成 + C# 参照表（Best 条目 RGBA8 dump + ico_csharp.log）
pwsh -File Benchmarks\ico\scripts\run_csharp_ref.ps1

# 2) oracle 闸（含 ico_csharp_parity，需 dump 已在；另含 ico_parity 合成/金指纹）
cargo test -p kimg_ico -- --test-threads=1

# 3) acceptance：前置(缺语料/ref 自动补) → oracle 闸 → 静置 → probe → compare_refs → 主口径
pwsh -File Benchmarks\ico\scripts\run_acceptance.ps1    # -NoWait 跳过静置 60s
```

## 全新机器装配（git clone 后）

- **C# 参照**需 .NET（10.x，`%LOCALAPPDATA%\Microsoft\dotnet\dotnet.exe`）+ Sdcb.SimdCodecs 源
  （`SDCB_ROOT`，缺省 `<repo>\..\SimdCodecs`（当前项目父目录），或 clone
  `https://github.com/sdcb/Sdcb.SimdCodecs`）：
  ```powershell
  pwsh -File Benchmarks\ico\scripts\run_csharp_ref.ps1   # 生成语料 + ico_csharp.log + target/csharp-ref/ico
  ```
- **语料 `Benchmarks/corpus/BMP`**（gitignore，不入库）：跨机需从原机拷贝；缺 001/002 → gen 报错，
  语料缺时探针/parity SKIP 而非回归。ICO 语料本身由 gen 每次重建（先清空防混）。

## 已知口径与纪律

- 探针/probe 一律 release（opt-level=3）；GUI 不打开；长命令前台阻塞到结束（K86）。
- 时序口径（2026-11-08 记录）：两侧每轮都新建输出缓冲（C# `Decode` 新建 Rgba；kimg 每轮
  `vec![0u8; n]`），分配形态对称。语料 ≤256×256。
- 选帧口径 = **统一 32bpp 语料**，使 kimg(位深→面积) 与 C#(面积→位深) 选同一条；SKIP 面见上方。
- 对账契约（勿改前缀）：`KIMG_ICO_VS_CSHARP`、C# 表 `| `name` | ms |`、日志 UTF-8。
- 覆盖面缺口（C#/自研支持之外，不进对账）：混合位深容器 / 中间 alpha；.cur 经 kimg
  `IcoDecoder::new` 解（type 字段忽略，热点进 planes/bits 字段做上限校验）——均按 SKIP 面处理。
- 已知语义分歧：语料内 BMP32+DIB 的 AND 掩码在「有非零像素 alpha」时两侧都输出像素 alpha、掩码无效
  （kimg 无条件应用掩码但与像素 alpha 一致）；PNG 条目的最小 alpha（如 alpha=1 类 PNG 滤波/取整）未覆盖，
  若 parity 出现分歧 → 停下精读两侧实现对齐，不与代码硬凑（见 ico_csharp_parity.rs 顶部注释）。
