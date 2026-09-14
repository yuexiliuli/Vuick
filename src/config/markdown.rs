//! Markdown 渲染器 / 预览相关的配置常量（行虚拟化、图片几何、EPUB 图片协议）。

/// 行级虚拟化在视口上下各多构建的像素量（保证快速滚动时内容连续）。
pub const ROW_OVERDRAW_PX: f32 = 600.0;
/// 估算内容宽额外加的缓冲（px）。
pub const WIDTH_BUFFER_PX: f32 = 24.0;
/// 未知尺寸图片行的默认高（px）。
pub const IMAGE_ROW_DEFAULT_H: f32 = 280.0;
/// 图片行最大宽（px）。
pub const IMAGE_MAX_W: f32 = 600.0;
/// 表格横向滚动条底部预留条高度（px，与滚动条轨道高一致，避免遮挡表格内容）。
pub const TABLE_SCROLLBAR_RESERVE_PX: f32 = 16.0;
/// EPUB 内存图片的虚拟协议前缀（注册表键，预览解析相对路径图片的依据）。
pub const EPUB_IMG_SCHEME: &str = "epubimg://";
