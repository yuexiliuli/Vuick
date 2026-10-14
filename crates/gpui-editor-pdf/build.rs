//! 构建脚本：探测内置 pdfium dll 是否存在，若存在则设置 `embed_pdfium` cfg，
//! 让 `src/parser.rs` 能把该 dll 编译期嵌入可执行文件。
//!
//! 这样 exe 可以**单文件分发**（无需旁边放 pdfium.dll）：运行时把内嵌字节解出到
//! 临时目录再动态加载。若本机没有该 dll（如克隆后 resources/ 未就位），则不嵌入，
//! 运行时按磁盘 candidate 查找或给出明确报错——避免因缺文件导致编译直接失败。
//!
//! FFmpeg 与主 exe 无关：解码内核在独立 cdylib（工作区成员 `video-ffmpeg`）里，
//! 主程序运行时经 libloading 加载它；FFmpeg 的链接由该成员内的 `ffmpeg-sys-next`
//! 自行处理（需 `FFMPEG_DIR` / `LIBCLANG_PATH` 环境变量，见 scripts/setup-ffmpeg.ps1）。
//!
//! P4 拆 crate 后本脚本随 `gpui-editor-pdf` crate 迁移：pdfium dll 仍存放在工作区
//! 根 `resources/pdfium/`（crate 不在仓库根，用 `../../resources/...` 上溯两级定位）。

use std::path::Path;

/// 与 `src/parser.rs` 的 `PDFIUM_VERSIONED_NAME` 保持一致。
const PDFIUM_VERSIONED_NAME: &str = "pdfium-154.0.8035.0.dll";

fn main() {
    // 声明自定义 cfg，避免 rustc 报 `unexpected_cfgs` 警告
    println!("cargo:rustc-check-cfg=cfg(embed_pdfium)");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 应有值");
    // P4：crate 位于 crates/gpui-editor-pdf，pdfium dll 在工作区根 resources/pdfium/（上溯两级）。
    let dll = Path::new(&manifest)
        .join("../../resources")
        .join("pdfium")
        .join(PDFIUM_VERSIONED_NAME);
    if dll.is_file() {
        // 通知 cargo：dll 变化时重新运行 build.rs（从而重编译嵌入字节）
        println!("cargo:rerun-if-changed={}", dll.display());
        // 把 dll 绝对路径注入编译环境，源码用 `include_bytes!(env!("PDFIUM_DLL_PATH"))` 嵌入
        println!("cargo:rustc-env=PDFIUM_DLL_PATH={}", dll.display());
        println!("cargo:rustc-cfg=embed_pdfium");
    }
}
