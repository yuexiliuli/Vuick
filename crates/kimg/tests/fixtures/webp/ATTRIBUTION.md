# WebP 测试语料来源注记（外部语料，不入仓）

本目录**不再保存 `*.webp` 图片**。测试按名从外部语料目录定位图片（见 `mod.rs`
`webp_fixture_path`），本目录只保留：
- `webp-oracle-golden.tsv`（52 数据行，静态非动画）
- `webp-animation-oracle-golden.tsv`（25 数据行，逐帧动画）
- `mod.rs`（外部语料定位助手）
- 本注记

## golden 来源

两个 golden TSV 均摘自 **sdcb/SimdCodecs**（https://github.com/sdcb/SimdCodecs）：
- 上游路径：`tests/Sdcb.SimdCodecs.Tests/Fixtures/webp/`
- 上游许可：**MIT License**（SimdCodecs 仓整体 MIT）。
- 口径与 SimdCodecs `WebpOracleTests.cs` 一致：
  - `webp-oracle-golden.tsv`：逐 fixture `sha256(YUV)` / `sha256(nofancy RGBA)` /
    `sha256(fancy RGBA)`，RGBA = dwebp 1.6.0 `-pam` 输出剥离头部后的 `width*height*4`
    字节（无 alpha 图 alpha 通道=255）。
  - `webp-animation-oracle-golden.tsv`：`anim_dump 1.6.0` 逐帧 `sha256(RGBA)`，
    列 = fixture / frame / canvas 宽高 / timestamp-ms / duration-ms / hash。

golden 对应图片文件位于 SimdCodecs 上游夹具目录（与 golden 同名），由
`webp_fixture_path` 按名查找。

## 外部图片语料候选（按序，取存在的）

1. `KIMG_WEBP_FIXTURES` 环境变量（`;` 分隔多目录，最高优先）
2. `target/imgbench-corpus/WEBP`（gitignored 快照）
3. `E:\Rust\SimdCodecs\测试图片\WEBP`（用户主语料，001-036.webp）
4. `E:\Rust\SimdCodecs\tests\Sdcb.SimdCodecs.Tests\Fixtures\webp`（SimdCodecs 上游夹具）

WebP 数据本身由 libwebp 1.6.0 工具（cwebp/gif2webp/img2webp/anim_dump/dwebp）生成，libwebp
采用 **BSD-3-Clause** 许可。本目录仅含「解码后的哈希值（TSV）」，不包含 libwebp /
SimdCodecs 源码，故不触 clean-room 禁读约束（数据非源码）。
