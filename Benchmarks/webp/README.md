# WebP 单线程/多线程「kimg vs C#」复现套件

独立复现「单线程（和多线程）kimg（OURS WebP 后端，纯 Rust）vs C#（Sdcb.SimdCodecs.Webp.Benchmarks）vs
libwebp 1.6.0 SIMD DLL」对账。语料快照与 C# 表在共享的 `Benchmarks\corpus\`（各格式共用），libwebp DLL、
对账脚本、探针 crate 在本目录（`bin/ compare/ scripts/ logs/`）。一键 bootstrap + 一键 run。不改仓库其它
任何代码（kimg 本体只读按 `../../../crates/kimg` 路径依赖，见 `compare/Cargo.toml`）。

## 复现铁律（违反则对账无意义）

- **`KIMG_WEBP_THREADS=0` 强制单线程**（kimg 的 vp8 滤波 MT 门控与动画帧并行门控均识别 `0|1|off|serial`=串行）；
  **不设 = 默认 MT**（auto：宽图宏块数 ≥2304 才开重建∥滤波流水线）。`run_single_thread.ps1` 显式设 `0`，
  `run_mt.ps1` 不设（并 `Remove-Item` 清掉残留）。
- **`KIMG_WEBP_BACKEND` 默认 ours**：探针 `main()` 内部已 `set_var("KIMG_WEBP_BACKEND","ours")` 写死，
  无需（也不应）外部再设。
- **同窗 ≥2 轮取最暖**：探针每文件/每场景「暖机 1 轮 + 9 轮取中位」；`compare_refs.py` 同名去重按 **kimg 最小
  （最暖态）** 合并——多源同位文件只留最暖一档。对账结论只认同窗交错中位，不谈单跑绝对值。
- **测前静置 60–90s**：散热/变频稳定后再跑（`run_*.ps1` 已内建 `Start-Sleep 60`）。
- **libwebp 参照必须 `WebPDecodeRGBAInto` 复用缓冲**：禁 `WebPDecode*` malloc 版（慢 ~20% 失真参照）。
  探针输出每文件/场景 `libwebp=` 全走复用缓冲；动画通道 libwebp=N/A（本版仅 WebPDecodeRGBA，动画面不走 DLL）。

## 快速开始

### 全新机器装配（git clone 后）

参照 DLL（gitignore，不入库）：`bin/libwebp.dll` 缺失时自动从 MSYS2 固定版本装配。

```powershell
pwsh -File Benchmarks\webp\scripts\ensure_refs.ps1     # 缺失 → 下载+解压+SHA 校验+就位；已就位 → no-op
pwsh -File Benchmarks\webp\scripts\run_single_thread.ps1  # run_*.ps1 前置会自动 ensure + 把 bin/ 注入 PATH
pwsh -File Benchmarks\webp\scripts\run_mt.ps1
```

- **DLL 来源 = MSYS2 mingw-w64-x86_64-libwebp 1.6.0-1（SHA-256 钉版）**，URL/SHA 见
  `scripts/ensure_refs.ps1` 顶部固定版本表：`mingw64/bin/libwebp-7.dll` → `bin/libwebp.dll`（soname =
  `libwebp-7.dll`），并连带抽取依赖 `mingw64/bin/libsharpyuv-0.dll` → `bin/libsharpyuv-0.dll`
  （libwebp-7.dll 导入表实证依赖它）。探针 plain `LoadLibraryW` 不搜 DLL 自身目录 → `run_*.ps1` 前置已把
  `bin/` 注入 `PATH` 供旁置解析；本机既有的官方 vc 自包含 libwebp.dll 无需旁置，运行不受影响。
  *数值注意*：mingw 构建与本机既有官方 vc 构建数值可能有轻微差异，跨机 `kimg/libwebp` 参照一致即可。
- **C# 参照**需 .NET 10 + Sdcb.SimdCodecs.Webp.Benchmarks 工程 + .cache/corpus：本机复制
  `Benchmarks\corpus\csharp\csharp_table.log`，或按上文「C# 表再生成」跑 `run_csharp_ref.ps1` 再生成。
- **语料** `Benchmarks/corpus/webp`（gitignore，不入库）：仓内 20 张来自
  `target/imgbench-corpus/WEBP`（任何机 clone 后都有），外部 52 张跨机需 `bootstrap.ps1 -CorpusDirs …`
  从源快照（见下方「环境自检」）；缺失时对账相应降列，不回归。

### 环境自检（一次）
```powershell
# 多机可用：脚本不写死任何盘符路径，源一律「参数/环境变量」注入。本机建议全量：
pwsh -File Benchmarks\webp\scripts\bootstrap.ps1 `
    -CorpusDirs '<repo>\..\SimdCodecs\测试图片\WEBP' `
    -Dll        '<repo>\..\SimdCodecs\benchmarks\Sdcb.SimdCodecs.Webp.Benchmarks\.cache\libwebp.dll' `
    -CSharpTable "$env:TEMP\kilo\kimg_webp_vs_dll\csharp_quick_refresh.log"
# 或用环境变量同效：$env:KIMG_CORPUS_EXT / $env:LIBWEBP_DLL / $env:KIMG_CSHARP_TABLE
# 最简（只仓内 20 张，外部源跳过）：直接 pwsh -File Benchmarks\webp\scripts\bootstrap.ps1
```
幂等；核对结尾计数：`webp=72（20 仓内 + 52 外部去重）`、`dll=True`、`csharp_table_lines≥75`。
源缺失只警告继续（K64 语义：外部语料目录会被外部进程重排，快照进 `Benchmarks\corpus\webp\` 才稳定）。

> **多台机器**：仓内 20 张永远跟随本仓库 target/imgbench-corpus —— 任何机器 clone 后都有基线；
> 外部 52 张 + libwebp DLL + C# 表是**各机自备**（`-CorpusDirs/-Dll/-CSharpTable` 或对应 env），
> 缺谁对账就相应降级（如缺 C# 表则无 kimg/C# 列，只剩 kimg/libwebp）。

### 单线程对账
```powershell
pwsh -File Benchmarks\webp\scripts\run_single_thread.ps1
```
= `$env:KIMG_WEBP_THREADS=0` + 静置 60s + `cargo run --release`（探针，首跑全量编译 kimg，几分钟）+
`python compare_refs.py` 对账。stdout 双落 `Benchmarks\webp\logs\dll_single_<时间戳>.log`；结尾打印
`kimg÷C# geomean=…（static-lossy）` 一行供一眼看。

### 多线程对账
```powershell
pwsh -File Benchmarks\webp\scripts\run_mt.ps1
```
同单线程，但**不设** `KIMG_WEBP_THREADS`（默认 MT auto），log 名 `Benchmarks\webp\logs\dll_mt_<时间戳>.log`。

### 解释输出（compare_refs.py 的 3 列 geomean）

| 列 | 含义 |
|---|---|
| `geomean kimg/libwebp` | 自研 kimg NEW 直写路径 ÷ libwebp DLL（>1 = kimg 比 DLL 慢） |
| `geomean C#/libwebp` | C# current ÷ 同机 libwebp DLL（参照系一致性的自检） |
| `geomean kimg/C#` | 自研 kimg ÷ C#（**对账主口径**；>1 = kimg 慢于 C#，<1 = kimg 快） |

`== static-lossy: matched=N` 为对账文件数（tiny-* 微片与 C# 表缺名者即跳过）；`worst …` 只列出对账最差若干文件。
加 `--all-scen` 可扩展到 static-lossless / alpha-lossless。

## C# 表再生成（可选，需 .NET 10 + Sdcb.SimdCodecs.Webp.Benchmarks 工程 + .cache/corpus）

现成 `Benchmarks\corpus\csharp\csharp_table.log` 由 `Benchmarks\webp\scripts\bootstrap.ps1` 的 `-CSharpTable`（或 `KIMG_CSHARP_TABLE`）
从各机已有的 C# quick 2 列 `| \`名\` | ms |` 表快照得到；本机无现成表时也可直接传 C# quick 裸跑的 stdout。
再生成命令与口径见仓库 `docs/bench/WEBP_PERF_PROTOCOL.md`（本 README 只引用，不内联、不写死路径）。

## 口径备注

- 探针输出格式（`KIMG_WEBP_VS_DLL perfile …`）与 `compare_refs.py` 解析正则**严格一致**，勿改。
- 四通道口径：`kimg_old`（ImageReader→DynamicImage→to_rgba8，含两笔整帧临时分配）、`kimg_new`
  （VP8L 静态直写复用缓冲 / 其余复用缓冲 read_image，暖态零分配，C# DecodeTo 镜像）、`imagewebp`
  （0.2.4 直连）、`dll`（libwebp 复用缓冲 FFI）。
- 探针独立 workspace（`compare/` 自身 `[workspace]` 根），默认独立 `compare/target`（不设 target-dir）。
