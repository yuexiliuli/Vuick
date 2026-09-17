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
/// 引述块左侧「深色指示条」的宽度（px）：在引述灰底左侧压一条深色竖条，
/// 标识这里是引用块（宽度可调）。
pub const QUOTE_BAR_WIDTH_PX: f32 = 3.0;
/// 引述块左侧指示条的颜色（RGB 十六进制，不含 alpha）：
/// 比引述灰底略深即可标识引用，无需与正文文字同深。
pub const QUOTE_BAR_COLOR: u32 = 0xa8b1bd;
/// EPUB 内存图片的虚拟协议前缀（注册表键，预览解析相对路径图片的依据）。
pub const EPUB_IMG_SCHEME: &str = "epubimg://";
