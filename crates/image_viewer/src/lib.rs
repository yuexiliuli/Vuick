//! 图片查看器模块（crates/image_viewer/ 独立 crate）。
//!
//! 打开 / 拖入图片（png/jpg/gif/bmp/webp/ico/tiff…）时以全屏阅读器展示：
//! - **单滚轮滚轮**：在图片所在文件夹内逐张切换（向下滚 → 下一张，向上滚 → 上一张，循环）；
//! - **Ctrl+滚轮**：放大 / 缩小（缩放倍率作用于「适应视口」的基准尺寸，描影居中）；
//! - 解码在后台线程完成（`raster` crate，第三方 `image` 库别名），主线程只包一层 `RenderImage` 上屏；
//! - 与 [`crate::pdf::PdfViewer`] 同款「自包含阅读器」姿势：实体自持状态与滚轮处理，
//!   宿主（`EditorApp` 全屏预览）只负责摆一个 `size_full` 的孩子。
//!
//! 模块结构：
//! - [`viewer`]：`ImageViewer` 视图核心（后台解码 + 文件夹清单 + 切换 / 缩放 + 渲染）
//!
//! 图片扩展名识别清单（供 [`crate::app::EditorApp`] 路由与文件夹扫描共用）。

mod viewer;

pub use viewer::ImageViewer;

/// 可识别的图片扩展名（小写；`raster` crate 默认特性可解码的子集）。
/// 供 `EditorApp::is_image_path`（打开路由）与查看器自身「同文件夹扫描」共用。
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "ico", "tif", "tiff",
];

/// 某路径是否可识别的图片文件（按扩展名判断）。
pub fn is_image_extension(p: &std::path::Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
        .unwrap_or(false)
}
