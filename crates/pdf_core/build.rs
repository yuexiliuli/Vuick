//! 构建脚本：探测内置 pdfium dll 是否存在，若存在则用 **zstd level 19** 压缩
//! (6.9MiB→~2.9MiB) 后注入编译环境，让 `src/parser.rs` 编译期内嵌压缩字节。
//!
//! 这样 exe 可以**单文件分发**（无需旁边放 pdfium.dll）：运行时把内嵌压缩字节
//! 一次性解压到临时目录再动态加载。若本机没有该 dll（如克隆后 resources/ 未就位），
//! 则不嵌入，运行时按磁盘 candidate 查找或给出明确报错——避免因缺文件导致编译直接失败。
//!
//! 压缩在 OUT_DIR 产出 `.zst` 中间文件（不入仓、不污染 resources/），随 dll 变化
//! 自动重压；由 `cargo:rerun-if-changed=dll` 保证只在 dll 变更时重跑（增量构建零开销）。
//!
//! FFmpeg 与主 exe 无关：解码内核在独立 cdylib（工作区成员 `media-ffmpeg`）里，
//! 主程序运行时经 libloading 加载它；FFmpeg 的链接由该成员内的 `ffmpeg-sys-next`
//! 自行处理（需 `FFMPEG_DIR` / `LIBCLANG_PATH` 环境变量，见 scripts/setup-ffmpeg.ps1）。
//!
//! P4 拆 crate 后本脚本随 `pdf` crate 迁移：pdfium dll 仍存放在工作区
//! 根 `resources/pdfium/`（crate 不在仓库根，用 `../../resources/...` 上溯两级定位）。

use std::path::Path;

use sha2::{Digest, Sha256};

/// 与 `src/parser.rs` 的 `PDFIUM_VERSIONED_NAME` 保持一致。
const PDFIUM_VERSIONED_NAME: &str = "pdfium-154.0.8035.0.dll";

/// zstd 压缩档位：19 = 高压缩（实测本 dll 42%，压缩 ~2.4s，一次性构建成本）。
const ZSTD_LEVEL: i32 = 19;

fn main() {
    // 声明自定义 cfg，避免 rustc 报 `unexpected_cfgs` 警告
    println!("cargo:rustc-check-cfg=cfg(embed_pdfium)");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 应有值");
    // P4：crate 位于 crates/pdf，pdfium dll 在工作区根 resources/pdfium/（上溯两级）。
    let dll = Path::new(&manifest)
        .join("../../resources")
        .join("pdfium")
        .join(PDFIUM_VERSIONED_NAME);
    if dll.is_file() {
        // 通知 cargo：dll 变化时重新运行 build.rs（从而重压缩并重嵌入字节）
        println!("cargo:rerun-if-changed={}", dll.display());
        // 读取原 dll 并 zstd 压缩，产物放 OUT_DIR（cargo 构建产出目录，不污染仓库）。
        let raw = std::fs::read(&dll).expect("读取 pdfium dll 失败");
        // 原始字节的 sha256（hex）：注入编译环境，运行期 `extract_embedded_pdfium` 复用
        // 临时 dll 前比对磁盘 sha256，防「同长但内容被种植」的 DLL 投毒。
        let digest = Sha256::digest(&raw);
        let raw_sha256_hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        let compressed = zstd::bulk::compress(&raw, ZSTD_LEVEL)
            .expect("zstd 压缩 pdfium dll 失败");
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR 应有值");
        let zst_path = Path::new(&out_dir).join(format!("{PDFIUM_VERSIONED_NAME}.zst"));
        std::fs::write(&zst_path, &compressed).expect("写入压缩中间文件失败");
        // 把压缩字节路径、原大小与原 sha256 注入编译环境：源码用
        // `include_bytes!(env!("PDFIUM_ZST_PATH"))` 嵌入，运行期按 `PDFIUM_RAW_SIZE` 解压、
        // 按 `PDFIUM_RAW_SHA256` 校验复用。
        println!("cargo:rustc-env=PDFIUM_ZST_PATH={}", zst_path.display());
        println!("cargo:rustc-env=PDFIUM_RAW_SIZE={}", raw.len());
        println!("cargo:rustc-env=PDFIUM_RAW_SHA256={raw_sha256_hex}");
        println!("cargo:rustc-cfg=embed_pdfium");
    }
}
