# Benchmarks/jpeg — JPEG 三路对照复现套件

复现「自研 kimg_jpeg vs C# Sdcb.SimdCodecs.Jpeg vs libjpeg-turbo 3.2.0」三路解码性能与字节恒等对照。

## 结构

```
Benchmarks/jpeg/
  bin/turbojpeg.dll        # libjpeg-turbo 3.2.0 官方 vc-x64（黑盒，只用 turbojpeg.dll API）
  probe/                   # 独立 workspace：自研 vs turbo 同窗交错中位探针（kimg_jpeg_vs_turbo）
  csharp_runner/           # C# 参照工程（JpegRef，net10.0；ProjectReference 注入 SDCB_ROOT）
  scripts/run_csharp_ref.ps1   # 生成 C# 参照表 + target/csharp-ref 字节参照
  scripts/run_acceptance.ps1   # oracle 闸 + release 探针 + compare_refs 一键对账
  compare_refs.py          # 三列 geomean + Sigma + worst 前 5
  logs/                    # 测量日志（gitignored）
```

## 用法

```powershell
# 1) C# 参照（首次或换语料必跑）：-> Benchmarks/corpus/csharp/jpeg_csharp.log + target/csharp-ref 19 张
pwsh -File Benchmarks\jpeg\scripts\run_csharp_ref.ps1

# 2) 一键对账：oracle 闸（debug，byte 恒等）+ release 探针 + 三路表
pwsh -File Benchmarks\jpeg\scripts\run_acceptance.ps1
# 结尾一行 `ours÷turbo geomean=… / ours÷C# geomean=…` 即主口径，完整表在 logs/acceptance_<ts>.log
```

## 铁律（与 webp/heif 套件一致）

- **同窗交错中位**：每文件 ours 与 turbo 各暖机 1 + N 轮（N=9，`KIMG_JPEG_TURBO_ROUNDS` 可改）取中位，
  同一进程同一轮窗口内对拍；结论只下同一窗口、同二进制。
- **静置 60s**：跑探针前静置 60s（散热/变频稳定），`-NoWait` 可跳过（CI/已热机态）。
- **字节恒等闸**：`run_acceptance` 先跑 `cargo test -p kimg_jpeg -- --test-threads=1`（debug），
  出现 `result: FAILED` 即停；`jpeg_csharp_parity` 全 PASS 才放行性能对账。
- **黑盒 DLL**：`bin/turbojpeg.dll` 只经 LoadLibrary + tjDecompressHeader/tjDecompress2 API 调用，禁读其源码。
- **长命令前台阻塞**：本套件所有 runner/探针均为前台阻塞式运行，禁后台化 + 提前返回（K86）。

## 口径

- 灰度判定：自研 `color_type()==L8` → 灰度（turbo `TJPF_GRAY` 1B/px、C# `Gray8`；其余彩色
  turbo `TJPF_RGB` 3B/px、C# `Rgb24`）——三路同输出布局，逐字节可对。
- 计时姿态：双方均复用调用方已分配缓冲（暖态零分配 direct 姿态）。
- 语料：`Benchmarks/corpus/JPEG` 19 张（env `KIMG_JPEG_CORPUS` 覆盖）。
- C# 参照：`Sdcb.SimdCodecs.Jpeg`（纯托管解码器）`DecodeTo` 计时；其输出同时 dump 到
  `target/csharp-ref/<stem>.<w>x<h>.{rgb|gray}` 作字节恒等参照（oracle 闸消费）。
