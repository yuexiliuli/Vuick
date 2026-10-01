//! Markdown 渲染器 / 预览相关的配置常量（行虚拟化、图片几何、EPUB 图片协议）。

/// 行级虚拟化在视口上下各多构建的像素量（保证快速滚动时内容连续）。
pub const ROW_OVERDRAW_PX: f32 = 600.0;
/// 估算内容宽额外加的缓冲（px）。
pub const WIDTH_BUFFER_PX: f32 = 24.0;
/// 未知尺寸图片行的默认高（px）。
pub const IMAGE_ROW_DEFAULT_H: f32 = 280.0;
/// 图片行最大宽（px）。
pub const IMAGE_MAX_W: f32 = 600.0;
/// 图片行最小宽（px，拖拽调整条时钳制下限，避免缩没）。
pub const IMAGE_MIN_W: f32 = 32.0;
/// 图片行题注（可编辑 alt 文本行）与图片盒之间的垂直间隙（px）。
pub const IMAGE_CAPTION_GAP_PX: f32 = 6.0;
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

// ---------------------------------------------------------------------------
// 标题排版（预览 view / WYSIWYG 共用，见 `markdown::view::style::heading_metrics_for_size`）
// ---------------------------------------------------------------------------

/// 各级标题的**设计字号**（px）：在 [`HEADING_DESIGN_BASE`](=14) 这一**参考正文字号**下的
/// 取值，实际渲染时按正文字号等比缩放（`body / HEADING_DESIGN_BASE × 设计值`），使编辑区/
/// 预览/WYSIWYG 调字号时 #/##/### 成比例一起变大变小。索引 1..=6 对应 # ~ ######（0 位不用）。
///
/// 注意：设计基准固定为 14，与运行期默认字号 `layout::DEFAULT_FONT_SIZE` **解耦**——
/// 否则调大默认字号（如 24）会把标题基准一起带偏，标题反而小于正文。
pub const HEADING_DESIGN_SIZES: [f32; 7] = [0.0, 20.0, 18.0, 16.0, 16.0, 15.0, 15.0];
/// 标题设计字号所对应的**参考正文字号**（px）：`HEADING_DESIGN_SIZES` 在该字号下成立，
/// 其余正文按 `body / 此值` 等比缩放标题。
pub const HEADING_DESIGN_BASE: f32 = 14.0;
/// 标题行高系数（× 标题字号）。
pub const HEADING_LINE_RATIO: f32 = 1.45;
