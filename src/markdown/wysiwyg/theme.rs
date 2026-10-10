use std::cell::Cell;
use gpui::{font, px, Font, Hsla, Pixels};
use crate::markdown::view::style::StyleParams;

/// 行内样式调色（创建时注入；后续随主题切换重建）。
///
/// **关键**：整份主题由预览的 [`StyleParams`] 一次性派生（`WyTheme::from_style`），
/// 颜色与排版数值**逐项抄自同一来源**——预览与 WYSIWYG 改样式只动 `StyleParams`，
/// 这里不再有任何缩放系数/字号的二次定义。`font_size`/`font` 在创建时由
/// [`WyEditor::new`] 从 `window.text_style()` **一次性**确定并存进主题——之后
/// `build_frame` 一律用主题值，绝不再次读 `window.text_style()`。因为 measure
/// （栈空=默认 16）与 prepaint（栈内含 `live-body` 的 `.text_size`）两阶段的
/// `text_style()` **不同**，若每帧现取会在两个字号间交替 = 滚动时「字体一会儿大
/// 一会儿小」。
#[derive(Clone)]
pub struct WyTheme {
    pub text: Hsla,
    /// 引述/次要文字色（预览 `muted_fg`）。
    pub muted: Hsla,
    /// 等宽字体（预览主题 `mono`）。
    pub mono: Font,
    /// 正文字体（应用默认字体）。
    pub font: Font,
    /// 正文字号（px）——由应用全局字号驱动（主题内正文基准）。
    pub font_size: Pixels,
    pub accent: Hsla,
    /// 链接色（预览 `StyleParams.link`，明 #0969da / 暗 #79c0ff 单源）。
    pub link: Hsla,
    /// 代码块背景（明/暗主题各自取值，与预览一致）。
    pub code_bg: Hsla,
    /// 行内代码（反引号）文字色（预览 `code_inline_fg`）。
    pub code_inline_fg: Hsla,
    /// 行内代码圆角 pill 底色（预览 `code_inline_bg`）。
    pub code_inline_bg: Hsla,
    /// 分隔线/表格边框色（预览 `divider`）。
    pub divider: Hsla,
    /// 表头底色（预览 `table_head_bg`）。
    pub table_head_bg: Hsla,
    /// 引述块背景（预览 `quote_bg`，浅灰卡片感）。
    pub quote_bg: Hsla,
    pub is_dark: bool,
    /// 正文行高 / 代码字号 / 代码行高（抄自 [`StyleParams`]，随字号覆盖变化）。
    pub(crate) para_line: Pixels,
    pub(crate) code_size: Pixels,
    pub(crate) code_line: Pixels,
    /// 各级标题字号/行高（抄自 [`StyleParams::heading_metrics`]，索引 1..=6，0 位不用）。
    heading_fs: [Pixels; 7],
    heading_lh: [Pixels; 7],
}

impl WyTheme {
    /// 从预览单一排版源 [`StyleParams`] 派生 WYSIWYG 主题（字号覆盖用 `font_size`）。
    pub(crate) fn from_style(p: &StyleParams, body_font: Font, font_size: Pixels) -> Self {
        let mut heading_fs = [px(0.); 7];
        let mut heading_lh = [px(0.); 7];
        for lv in 1..=6u8 {
            let (fs, lh) = p.heading_metrics(lv);
            heading_fs[lv as usize] = fs;
            heading_lh[lv as usize] = lh;
        }
        WyTheme {
            text: p.fg,
            muted: p.muted_fg,
            mono: font(p.mono.clone()),
            font: body_font,
            font_size,
            accent: p.accent,
            link: p.link,
            code_bg: p.code_bg,
            code_inline_fg: p.code_inline_fg,
            code_inline_bg: p.code_inline_bg,
            divider: p.divider,
            table_head_bg: p.table_head_bg,
            quote_bg: p.quote_bg,
            is_dark: p.is_dark,
            para_line: p.para_line,
            code_size: p.code_size,
            code_line: p.code_line,
            heading_fs,
            heading_lh,
        }
    }

    /// 由正文字号覆盖更新尺寸相关字段（缩放系数同样只来自
    /// [`crate::markdown::view::style`] 的 `typography_for_size` / `heading_metrics_for_size`，
    /// 不在此重复定义）。标题字号/行高也随正文字号等比缩放——预览/WYSIWYG 的 Ctrl+滚轮、
    /// 菜单调字号时 #/##/### 一起变大变小。
    pub(crate) fn set_font_size(&mut self, size: Pixels) {
        self.font_size = size;
        let body = f32::from(size).max(9.);
        let (pl, cs, cl) = crate::markdown::view::style::typography_for_size(body);
        self.para_line = pl;
        self.code_size = cs;
        self.code_line = cl;
        for lv in 1..=6u8 {
            let (fs, lh) = crate::markdown::view::style::heading_metrics_for_size(body, lv);
            self.heading_fs[lv as usize] = fs;
            self.heading_lh[lv as usize] = lh;
        }
    }

    /// 正文行高（预览 `para_line`）。
    pub fn para_line(&self) -> Pixels {
        self.para_line
    }
    /// 代码字号（预览 `code_size`）。
    pub fn code_size(&self) -> Pixels {
        self.code_size
    }
    /// 代码行高（预览 `code_line`）。
    pub fn code_line(&self) -> Pixels {
        self.code_line
    }
    /// 第 n 级标题字号（预览 `heading_metrics`：h1=24 h2=20 h3=18 h4=16 h5/6=15）。
    pub fn heading_fs(&self, level: u8) -> Pixels {
        self.heading_fs[(level as usize).clamp(1, 6)]
    }
    /// 第 n 级标题行高。
    pub fn heading_line(&self, level: u8) -> Pixels {
        self.heading_lh[(level as usize).clamp(1, 6)]
    }
}
/// 每帧光标几何（供 IME/`EntityInputHandler::bounds_for_range` 读取）。
#[derive(Clone, Copy, Default)]
pub(crate) struct CaretVis {
    pub x: Pixels,
    pub top: Pixels,
    pub line_height: Pixels,
}

thread_local! {
    static CARET_VIS: Cell<CaretVis> = const { Cell::new(CaretVis { x: px(0.), top: px(0.), line_height: px(0.) }) };
}

pub(crate) fn set_caret_vis(v: CaretVis) {
    CARET_VIS.with(|c| c.set(v));
}
pub(crate) fn caret_vis() -> CaretVis {
    CARET_VIS.with(|c| c.get())
}
/// 数学文字色（单一源：位图着色 = Unicode 回退文本色 = 渲染 fallback 一致）。
/// 取 `config::math::MATH_COLOR` 的紫（业界公式感），暗色主题提亮。
pub(crate) fn math_color(theme: &WyTheme) -> Hsla {
    let mut c: Hsla = gpui::rgb(crate::config::math::MATH_COLOR).into();
    if theme.is_dark {
        c.l = (c.l + crate::config::math::MATH_COLOR_DARK_LIFT).clamp(0.05, 1.);
    }
    c
}
