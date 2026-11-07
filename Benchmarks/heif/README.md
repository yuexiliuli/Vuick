# HEIF「kimg_heif vs C# 单核」复现套件

自包含复现 HEIF 战役的**两条硬验收指标**（相对 Sdcb.SimdCodecs.Heif 单核）：`geomean(Rust 并行/C# 单核)` 与
`geomean(Rust 单核/C# 单核)`。Rust 侧测量用仓内已提交的 `#[ignore]` 探针（`crates/kimg_heif/tests/oracle_perf.rs` 的
`perf_acceptance_geo` / `perf_tile_parallel_speedup` / `stage_profile`）；C# 侧参照表静态落 `Benchmarks\corpus\csharp\`，
可任一台机器再生成（见下）。本目录不动仓库其它代码，只提供「一键对账」外壳。

## 快速开始

```powershell
# 一键：Rust release 探针（并行 acceptance + KIMG_HEIF_PARALLEL=0 单核 + C# 表对账 + geomean）
pwsh -File Benchmarks\heif\scripts\run_acceptance.ps1
#   → logs\acceptance_<时间戳>.log；结尾打印两行主口径 geomean：
#     GEO parallel   17f=<x> 14phone=<x> 3single=<x>
#     GEO singlecore 17f=<x> 14phone=<x> 3single=<x>
```

前置：`cargo`（Rust toolchain，release 可编译 kimg_heif）+ HEIF 语料在 `Benchmarks\corpus\heif\`
（27 文件：14 手机 + 3 grid + 6 synth + 3 profile-4 拒绝）。语料缺失时先跑一次
`cargo test -p kimg_heif -- --test-threads=1`（oracle/语料 SKIP 即缺）或用
`run_csharp_ref.ps1`/oracle dump 重新装配（见 `crates/kimg_heif/tests/` 顶注）。

## C# 参照表

- **静态**：`Benchmarks\corpus\csharp\heif_csharp_quick.log`（14 手机 + 3 单 tile 的 stage/RGBA ms，reps=9 中位，本战役机器）
  与 `heif_csharp_ablate.log`（5 夹具逐阶段消融，可选）。
- **再生成（各机自备 .NET 10 + Sdcb.SimdCodecs.Heif 临时工具）**：
  ```powershell
  pwsh -File scripts\run_csharp_ref.ps1 -OracleProj 'C:\path\to\heif_oracle\HeifOracle.csproj' -CorpusDir 'Benchmarks\corpus\heif'
  # 等价 env：$env:HEIF_ORACLE_PROJ / $env:HEIF_CORPUS_DIR
  # 缺参时若 %TEMP%\kilo\heif_oracle\HeifOracle.csproj 存在则用缺省，否则报错提示注入
  ```
  生成的表写回 `Benchmarks\corpus\csharp\`。`compare_refs.py` 优先读刚生成的表，缺失则回退静态表。

## 口径铁律（违反则对账无意义）

- **同窗/中位**：Rust 探针每文件 warmup + 9 轮中位；结论只认同窗交错，不谈单跑绝对值（跨窗 ±3-30%）。
- **单核口径 = `KIMG_HEIF_PARALLEL=0`**（强制 tile decode compose/color/transform 全串行）；并行口径不设该 env。
- **字节恒等闸**：探针前必先 `cargo test -p kimg_heif -- --test-threads=1` 全绿（oracle 0 失配）；性能数字只是附加。
- 测前静置 60–90s（`run_acceptance.ps1` 内建 `Start-Sleep 60`）。

## 对应战役

`docs/HEIF_FINAL_REPORT_2026-11-07.md`（本役终报，含阶段分账与 commit 明细）。基线参照：
并行 17f geomean 0.286 / 单核 17f 0.944 / 14 手机并行 0.221（master d3c064b）。
