//! 布局 / 几何 / 字号等面值常量：px 尺寸、分栏比例、侧栏宽、字号上下限、滚动条几何。

use gpui::{px, Pixels};

/// 默认正文字号（px，用户可在「查看」菜单 / Ctrl+滚轮 调节）。
pub const DEFAULT_FONT_SIZE: f32 = 14.0;
/// 字号可调下限（px）。
pub const FONT_MIN: f32 = 10.0;
/// 字号可调上限（px）。
pub const FONT_MAX: f32 = 28.0;
/// 顶部菜单栏高度。
pub const MENU_BAR_H: Pixels = px(26.);
/// 编辑区 / 预览区分屏比例下限（编辑区宽占内容区宽比例）。
pub const SPLIT_RATIO_MIN: f32 = 0.2;
/// 编辑区 / 预览区分屏比例上限。
pub const SPLIT_RATIO_MAX: f32 = 0.8;
/// 侧边栏默认宽度（px）。
pub const SIDEBAR_WIDTH: f32 = 240.0;
/// 侧边栏「活动栏」窄条宽度（px，收起时只留活动栏）。
pub const SIDEBAR_ACTIVITY_W: f32 = 46.0;
/// 侧边栏宽度下限（px）。
pub const SIDEBAR_WIDTH_MIN: f32 = 160.0;
/// 侧边栏宽度上限（px）。
pub const SIDEBAR_WIDTH_MAX: f32 = 480.0;
/// 横向滚动条滑块距轨道两端的间距（px）。
pub const HSCROLL_GAP_RIGHT: Pixels = px(3.);
/// 横向滚动条滑块最小尺寸（px）。
pub const HSCROLL_THUMB_MIN: Pixels = px(26.);
/// WYSIWYG 代码块底部横向滚动条：可交互带高（px，覆在末空白带上，不盖文字）。
/// 与 view 的 gpui-component 滚动条同款：轨道厚 16、内衬 4、滑块 8 高、最小 48 宽。
pub const CODE_HSCROLL_BAND: f32 = 16.0;
/// WYSIWYG 代码块底部横向滚动条：滑块高（px，轨道内内衬对称）。
pub const CODE_HSCROLL_THUMB_H: f32 = 8.0;
/// WYSIWYG 代码块底部横向滚动条：滑块最小宽（px，对齐 view 的 MIN_THUMB_SIZE=48）。
pub const CODE_HSCROLL_THUMB_MIN: f32 = 48.0;
/// 编辑器垂直标尺（margin guide）的列位置（宽度单位列：CJK=2 格），多列升序。
/// **默认置空（不画）**：用户真正需要的是「括号标尺」（见 view.rs bracket guides），
/// 这条第 80 列的长竖线会显得多余。如需启用显式填列（如 `&[80, 100]`）。
pub const EDITOR_RULER_COLUMNS: &[usize] = &[];

// ---------------------------------------------------------------------------
// 括号标尺（bracket guides）竖线端点微调（× line_height 行高系数）
// ---------------------------------------------------------------------------

/// 竖线**底部**伸入闭行的行高系数：0 = 停在闭行顶部（底部留约半行空档）；
/// 1 = 画满整个闭行（穿过 `}` 字形）。取 0.4 ≈ 贴 `}` 字形顶端、与顶部距离一致。
pub const BRACKET_GUIDE_CLOSE_TAIL_LINES: f32 = 0.4;

/// 竖线**顶部**从首覆盖行（开行下一行）顶部向下缩进的行高系数：0 = 紧贴开行
/// （默认）；调大 → 顶部留白（开行与竖线起点间距）增大。与
/// [`BRACKET_GUIDE_CLOSE_TAIL_LINES`] 配套微调上下观感。
pub const BRACKET_GUIDE_OPEN_HEAD_LINES: f32 = 0.4;

/// 括号标尺竖线宽度（px）：普通（未点亮）竖线。
pub const BRACKET_GUIDE_WIDTH: f32 = 1.0;

/// 括号标尺**点亮**竖线宽度（px）：点击代码区域后高亮的最内层块竖线。
/// 与普通竖线等宽（1px），高亮仅靠强调色区分，不额外加粗、不喧宾夺主。
pub const BRACKET_GUIDE_ACTIVE_WIDTH: f32 = 1.0;

// ---------------------------------------------------------------------------
// 区域分隔发丝线 / 暗色主题区域底色 / 状态栏
// ---------------------------------------------------------------------------

/// 区域分隔发丝线颜色（**暗色主题**）：标题栏底 / 编辑区顶边 / 侧栏右缘 /
/// 状态栏边框共用，不透明、全屏一致（不再随底色半透明混色）。
pub const SEPARATOR_DARK: u32 = 0x2A2B2C;
/// 区域分隔发丝线颜色（**亮色主题**）：同上，灰调浅色发丝线。
pub const SEPARATOR_LIGHT: u32 = 0xD9D9D9;

/// 暗色主题·**编辑区/预览面板**底色（hex）。
pub const EDITOR_BG_DARK: u32 = 0x121314;
/// 暗色主题·**其余区域**底色（hex）：标题栏 / 选项卡栏 / 侧栏 / 状态栏共用。
pub const CHROME_BG_DARK: u32 = 0x191A1B;

/// 底部状态栏高度（px）。
pub const STATUS_BAR_H: f32 = 23.0;
