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
