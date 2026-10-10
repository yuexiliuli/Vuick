//! Markdown 渲染器 / 预览相关的配置常量（行虚拟化、图片几何、EPUB 图片协议）。

/// 行级虚拟化在视口上下各多构建的像素量（保证快速滚动时内容连续）。
pub const ROW_OVERDRAW_PX: f32 = 600.0;
/// 预览内容列的**左右对称留白**（px）：内容列最右字到「卡片/面板右缘」的最小距离。
/// 放大到**肉眼明显可感**（正文 14px 下 ≈2.5 个汉字宽），宽裕覆盖右侧自绘滚动条带
/// （thumb 6px + 内缩 ≈10px）+ 边框/小间隙，保证窄窗退化（列宽 = 可用宽）时最右字
/// 都不贴边被卡片/滚动条裁切、标题行不再「靠右缘被截断」。与左侧行内边距对称；
/// 是 MarkdownPreview（`md-list-wrap`）与 WYSIWYG 预览宿主（`#md-wy-body`）内容区
/// 间距的**单一来源**（页面恒换行，文首/文尾共用）。
pub const PREVIEW_SIDE_GAP_PX: f32 = 44.0;
/// 内容盒**内衬**（px）：内容文字/代码面板距「内容区左右缘」的最小内边距——预览
/// （`markdown::view`）行内 `px_3` 等价留白与 WYSIWYG 帧内衬（`wysiwyg::frame` 的
/// `padding`，命中/光标/划线/拖拽几何同源）的**单一来源**（旧实现各自硬编码 12）。
///
/// 层级关系：内容列**总留白** = [`PREVIEW_SIDE_GAP_PX`]（宿主外侧 gap，`md-wy-body` /
/// `md-list-wrap` 同源，面板边缘 → 内容区边缘）+ 本值（内容区边缘 → 文字/代码面板）。
/// 两者落在同一模块统一管理、刻意**独立取值**：本值是行内排版基准（预览与 WYSIWYG
/// 两侧各 12px 内衬、代码/引述/表格在内容区上再内缩 12px 的派生基础），不随宿主 gap
/// 放大而放大；宿主 gap 调大只撑开「面板 → 内容区」这一层，预览/WYSIWYG 内侧视觉一致。
pub const CONTENT_EDGE_PAD_X_PX: f32 = 12.0;
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
///
/// 数值贴近 Win11 / 微软官网大头层级（正文 14px 参考下 h1=24 h2=20 h3=18 h4=16 h5/6=15），
/// 层级差更清晰：h1 比正文大 ~1.7×，相邻两级差 ≥1px（旧值 20/18/16 层级偏挤）。
pub const HEADING_DESIGN_SIZES: [f32; 7] = [0.0, 24.0, 20.0, 18.0, 16.0, 15.0, 15.0];
/// 标题设计字号所对应的**参考正文字号**（px）：`HEADING_DESIGN_SIZES` 在该字号下成立，
/// 其余正文按 `body / 此值` 等比缩放标题。
pub const HEADING_DESIGN_BASE: f32 = 14.0;
/// 各级标题行高系数（× 该级标题字号）。大标题收紧（微软官网大头 lot ~1.2-1.3 更利落）、
/// 小级标题透气（~1.4-1.5），形成「越大越紧、越小越松」的现代排版节奏。索引 1..=6 对应
/// # ~ ######（0 位不用）。
pub const HEADING_LINE_RATIOS: [f32; 7] = [0.0, 1.3, 1.35, 1.4, 1.45, 1.5, 1.5];

// ---------------------------------------------------------------------------
// 居中阅读栏（预览恒换行 + WYSIWYG md 预览共用单一来源）
// ---------------------------------------------------------------------------

/// 居中阅读栏开关：`true` 时正文内容宽度被钳到 [`READING_COL_CHARS`] 字符列宽并在面板内
/// **水平居中**（预览 `markdown::view` 与 WYSIWYG `WyEditor` 同步生效；页面恒换行后
/// EPUB 阅读等全部路径都套用）。代码/表格等宽块不裁剪——仍在列内各自横向滚动。
pub const READING_COL_ENABLED: bool = true;
/// 居中阅读栏目标**字符列数**（ch 单位）：正文排版宽度 ≈ `READING_COL_CHARS × 平均字宽`。
/// 参考中英文阅读均适用的一栏 ~66-80ch（微软官网正文 ~70-75ch）。
pub const READING_COL_CHARS: f32 = 72.;
/// 平均字宽占正文字号的估算系数（em）：拉丁小写平均 ~0.5em、中文全角 ~1em，混合文档取
/// 0.8 作贴近主文档的保守中间值——正文 14px 时 72ch ≈ 806px（典型阅读栏宽，窗口窄于此
/// 时自动退化为可用宽，不会横向超界）。
pub const READING_COL_AVG_CHAR_EM: f32 = 0.8;

/// 阅读栏目标宽度（px）：`READING_COL_CHARS × body_fs × READING_COL_AVG_CHAR_EM`。
/// 由**正文字号**驱动——预览/WYSIWYG 的 Ctrl+滚轮缩放、EPUB 字号覆盖变化时阅读栏
/// 同步缩放；预览与 WYSIWYG 共用本函数，不再两处各写一套。
#[inline]
pub fn reading_col_px(body_fs: f32) -> f32 {
    READING_COL_CHARS * body_fs * READING_COL_AVG_CHAR_EM
}
