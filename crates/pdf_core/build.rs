//! 构建脚本（2026-11-22 起：**不再内嵌 pdfium**）。
//!
//! 历史：曾探测 `resources/pdfium/<版本>.dll` 存在后用 **zstd level 19** 压缩
//! （6.9MiB→~2.9MiB）注入编译环境，再经 `embed_pdfium` cfg 让 exe 内嵌压缩字节，运行时
//! 一次性解压到临时目录再动态加载（单文件分发兜底）。
//!
//! 2026-11-22 分发布局改造：exe 移到 dist 根、pdfium dll 移到 `dist\bin\pdf\` 磁盘分发，
//! **不再把 pdfium 字节内嵌进 exe**（体积减小；运行只走磁盘候选，见 `src/parser/pdfium.rs`）。
//! 因此本脚本不再压缩/注入/emit：`cargo:rustc-cfg=embed_pdfium` 与 `PDFIUM_ZST_PATH` /
//! `PDFIUM_RAW_SIZE` / `PDFIUM_RAW_SHA256` env 均已移除，源码亦无 `cfg(embed_pdfium)` 分支，
//! 故也不再 `rustc-check-cfg`。
//!
//! THIRD_PARTY_NOTICES 影响（**不改该文件，仅备注**）：exe 不再携带 zstd 压缩的 pdfium 载荷，
//! 分发体积变化；pdfium 二进制仍随 dist 磁盘分发（`bin\pdf\`），其许可/版权（BSD-3-Clause +
//! Apache-2.0，原文 vendor 于 `third_party/pdfium/`）不变，notice 条目无需增删。

fn main() {
    // 无构建期动作：pdfium 不再内嵌，走磁盘分发。
}
