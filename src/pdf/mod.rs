//! PDF 读取与展示模块。
//!
//! 基于纯 Rust 的 [`hayro`] 栅格化器（无原生 DLL，Windows/macOS/Linux 通用），
//! 架构参照 gpui-pdf（zorite `crates/gpui-pdf`）：
//! - 文件只**解析一次**（结构 + 页尺寸，很快），解析出的 [`Document`]（`Send + Sync`）
//!   在后台任务间用 `Arc` 共享；
//! - [`PdfViewer`] **页级虚拟化**：先按页尺寸铺设正确的页槽（滚动条即时正确），
//!   只对**靠近视口的页**后台按需栅格化，滚远即释放位图与 GPU 纹理——长文档
//!   内存受限、打开即显、窗口不卡。
//!
//! 模块结构：
//! - [`parser`]（`parser.rs`）：底层原语——解析、页尺寸、单页栅格化为
//!   [`gpui::RenderImage`]
//! - [`view`]（`view.rs`）：[`PdfViewer`]——自包含阅读器（加载/滚动/按需渲染/回收）

mod parser;
mod view;

pub use parser::{parse_bytes, render_page, page_dims};
pub use view::PdfViewer;

use hayro::hayro_syntax::Pdf;

/// 已解析的 PDF 文档：结构 + 页尺寸 + 页缓存均在其内部（`Send + Sync`），
/// 可跨后台任务 `Arc` 共享，多次按页渲染无需重解析。
pub type Document = Pdf;

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pdf_paths() -> Vec<std::path::PathBuf> {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        vec![
            manifest.join("N107内外饰配置表NEW.pdf"),
            manifest.join("17. 道路机动车辆产品准入审查要求1.0.pdf.pdf"),
        ]
    }

    #[test]
    fn parses_all_test_pdfs_quickly_and_reads_dims() {
        for p in test_pdf_paths() {
            assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
            let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
            let t0 = std::time::Instant::now();
            let doc = parse_bytes(bytes).expect("解析 PDF 应成功");
            let dims = page_dims(&doc);
            let elapsed = t0.elapsed();
            // 结构解析（不栅格化）应很快：大文件也应在数秒内完成，否则无法“打开即显”
            assert!(
                elapsed < std::time::Duration::from_secs(15),
                "{} 结构解析过慢：{elapsed:?}（{} 页）",
                p.display(),
                dims.len()
            );
            assert!(!dims.is_empty(), "{} 应有页", p.display());
            for (w, h) in &dims {
                assert!(*w > 0.0 && *h > 0.0, "{} 页尺寸应为正: {w}x{h}", p.display());
            }
        }
    }

    #[test]
    fn renders_single_page_from_shared_document() {
        let p = test_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let doc = parse_bytes(bytes).expect("解析 PDF 应成功");
        let img = render_page(&doc, 0, 1.0).expect("栅格化第 1 页应成功");
        assert!(img.frame_count() >= 1, "应生成至少 1 帧");
    }

    #[test]
    fn renders_many_pages_individually() {
        // 259 页的 PDF：逐页按需渲染都应成功（证明「按需渲染」可行，不再一次性全画）
        let p = test_pdf_paths()[1].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let doc = parse_bytes(bytes).expect("解析 PDF 应成功");
        let dims = page_dims(&doc);
        assert!(dims.len() >= 200, "长文档应有大量页，实际={}", dims.len());
        // 只抽查首/中/末各一页，模拟「滚动到哪渲染哪」
        for idx in [0usize, dims.len() / 2, dims.len() - 1] {
            let img = render_page(&doc, idx, 0.5).expect("按需栅格化应成功");
            assert!(img.frame_count() >= 1, "第 {idx} 页应渲染出帧");
        }
    }
}
