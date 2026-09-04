//! PDF 读取与展示模块。
//!
//! 用 `pdfium-render`（Google PDFium）把 PDF 逐页**栅格化**成内存位图（PNG），
//! 由 [`PdfViewer`] 按**原样**逐页显示：页面即原图，不做任何文本抽取/布局改写，
//! 全程不落盘。这样即使是表格/矢量绘图为主的 PDF（纯文本提取几乎为空）
//! 也能完整、保真地呈现每一页内容。
//!
//! 模块结构：
//! - [`parser`]（`parser.rs`）：PDFium 绑定、DLL 定位、逐页栅格化与 PNG 编码，产出 [`PdfDocument`]
//! - [`view`]（`view.rs`）：[`PdfViewer`]——行级虚拟化、整页原样显示的阅读器
//!
//! 运行时需要一个 `pdfium.dll`：优先在可执行文件同目录查找，其次资源目录
//! `resources/pdfium/pdfium.dll`。Windows 上 libloading 会按 EXE 所在目录搜索。

mod parser;
mod view;

pub use parser::parse_pdf_file;
pub use view::PdfViewer;

use std::sync::Arc;

use gpui::Image;

/// 解析后的 PDF 文档：元数据 + 栅格化分页位图（原样显示用）。
#[derive(Debug, Clone, Default)]
pub struct PdfDocument {
    /// 文件名（不含扩展名，作为标题显示）
    pub title: String,
    /// 页数
    pub page_count: usize,
    /// 逐页栅格化位图（按阅读顺序）。
    pub pages: Vec<PdfPage>,
}

/// 一页 PDF：栅格化后的内存 PNG 位图 + 位图像素尺寸。
/// 仅承载“原样”页面图像，不包含任何文本改写。
#[derive(Debug, Clone)]
pub struct PdfPage {
    /// 页面 PNG 位图（内存，不落盘）。
    pub image: Arc<Image>,
    /// 位图像素宽。
    pub width: u32,
    /// 位图像素高。
    pub height: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::ImageFormat;

    fn test_pdf_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/N107内外饰配置表NEW.pdf")
    }

    #[test]
    fn renders_pdf_pages_unchanged() {
        let doc = parse_pdf_file(&test_pdf_path()).expect("解析 PDF 应成功");
        assert!(doc.page_count >= 1, "应有至少 1 页，实际={}", doc.page_count);
        assert_eq!(
            doc.pages.len(),
            doc.page_count,
            "页数应为每页一张位图: pages={} count={}",
            doc.pages.len(),
            doc.page_count
        );
        for (i, page) in doc.pages.iter().enumerate() {
            assert_eq!(page.image.format, ImageFormat::Png, "第 {i} 页位图应为 PNG");
            assert!(
                page.image.bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                "第 {i} 页 PNG 魔数正确"
            );
            assert!(
                page.width > 0 && page.height > 0,
                "第 {i} 页应有正尺寸: {}x{}",
                page.width,
                page.height
            );
        }
    }
}
