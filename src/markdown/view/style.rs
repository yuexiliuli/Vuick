//! 排版参数：渲染时从全局主题取一份“快照”，避免每行重复查主题。
//! 所有取值都对齐 gpui_component `node.rs` 的 Markdown 渲染风格：
//! 段落用 `sm`，标题用语义字号 + 加粗，代码块用 `muted` 底 + 等宽字体，
//! 引述用浅灰底 + 强调色左边框。
//!
//! 第 3 波迁移后：**颜色/等宽字体单一来源 `crate::ui::theme`**（语义色板 +
//! `Typography`），不再读 gpui-component 主题语义 token；正文/标题字号体系仍由
//! `config::layout::DEFAULT_FONT_SIZE` 驱动（统一缩放架构）。

use gpui::{px, App, Hsla, Pixels, SharedString};

/// 排版参数快照（见模块文档）。
pub(crate) struct StyleParams {
    pub(crate) mono: SharedString,
    pub(crate) fg: Hsla,
    pub(crate) muted_fg: Hsla,
    /// 代码块背景（白 #F3F4F4 / 黑 #282936），见 [`StyleParams::from_theme_with_font`]。
    pub(crate) code_bg: Hsla,
    /// 行内代码（反引号）文字色（白 #222222 / 黑 #DADADA，近正文——靠底色 + 等宽字体区分）。
    pub(crate) code_inline_fg: Hsla,
    /// 行内代码圆角 pill 底色（白 #F3F4F4 / 黑 #3E3F40）。
    pub(crate) code_inline_bg: Hsla,
    pub(crate) divider: Hsla,
    pub(crate) accent: Hsla,
    /// 链接色（`[t](u)` / wiki / tag / 裸 URL；明 #0969da / 暗 #79c0ff，单一源
    /// `ColorScheme::link`——预览 / WYSIWYG / 源码编辑器一个蓝）。
    pub(crate) link: Hsla,
    pub(crate) bg: Hsla,
    /// 引述块背景（浅灰卡片感）。
    pub(crate) quote_bg: Hsla,
    /// 表头底色（浅灰）。
    pub(crate) table_head_bg: Hsla,
    pub(crate) para_size: Pixels,
    pub(crate) para_line: Pixels,
    /// 普通文本行额外垂直间距（行高之外，用于拉开行与行之间的距离）。
    pub(crate) para_gap: Pixels,
    pub(crate) code_size: Pixels,
    pub(crate) code_line: Pixels,
    pub(crate) blank_h: Pixels,
    pub(crate) rule_h: Pixels,
    /// 当前主题是否为暗色（预览与 WYSIWYG 各自派生语法色/代码底色时共用）。
    pub(crate) is_dark: bool,
}

/// 由正文字号推导「正文行高/代码字号/代码行高」——**唯一**的缩放系数来源
/// （`0.88/1.3/1.45` 只在这里出现一次），预览与 WYSIWYG 共用，避免两处各写一套。
pub(crate) fn typography_for_size(fs: f32) -> (Pixels, Pixels, Pixels) {
    (px(fs * 1.45), px(fs * 0.88), px(fs * 1.3))
}

/// 由正文字号推导第 n 级标题的字号与行高（预览与 WYSIWYG 共用，见
/// [`StyleParams::heading_metrics`]）。设计字号基准集中在
/// `config::markdown::HEADING_DESIGN_SIZES` / `HEADING_DESIGN_BASE` / `HEADING_LINE_RATIO`
/// （正文 14px 参考字号下 h1=20 h2=18 h3/4=16 其余 15），其余字号按
/// `body_fs / HEADING_DESIGN_BASE` 等比缩放，标题相对正文的比例保持不变。
pub(crate) fn heading_metrics_for_size(body_fs: f32, level: u8) -> (Pixels, Pixels) {
    let design = match level {
        1..=6 => crate::config::markdown::HEADING_DESIGN_SIZES[level as usize],
        _ => crate::config::markdown::HEADING_DESIGN_SIZES[6],
    };
    let fs = body_fs / crate::config::markdown::HEADING_DESIGN_BASE * design;
    (px(fs), px(fs * crate::config::markdown::HEADING_LINE_RATIO))
}

impl StyleParams {
    /// 从主题构建排版参数；`font_override` 可覆盖正文/代码字号（EPUB 缩放字号用）。
    /// 颜色/等宽字体单一来自 `crate::ui::theme`（语义色板 + `Typography`）。
    pub(crate) fn from_theme_with_font(cx: &mut App, font_override: Option<f32>) -> Self {
        let theme = crate::ui::theme::Theme::global(cx);
        let colors = theme.colors;
        // 正文基准字号：默认与编辑区 / WYSIWYG 对齐（theme.typography 的缺省正文，=
        // layout::DEFAULT_FONT_SIZE，见统一缩放架构）。有覆盖时按覆盖值缩放；正文字号
        // 一变，代码/行高系数统一走 `typography_for_size`（同一缩放来源）。
        let fs = font_override
            .unwrap_or(theme.typography.default_body_size)
            .max(9.0);
        let (pl, cs, cl) = typography_for_size(fs);
        let (para, line, code, code_line) = (px(fs), pl, cs, cl);
        let is_dark = theme.is_dark();
        // 代码块/行内代码/引述/表头/链接等语义色：全部取自色板（第 3 波把
        // 原硬编码 rgb() 收编进色板，值逐字节不变）。
        Self {
            mono: theme.typography.mono.clone(),
            fg: colors.fg.into(),
            muted_fg: colors.fg_muted.into(),
            code_bg: colors.code_block_bg.into(),
            code_inline_fg: colors.code_inline_fg.into(),
            code_inline_bg: colors.code_inline_bg.into(),
            divider: colors.border.into(),
            accent: colors.accent.into(),
            link: colors.link.into(),
            bg: colors.bg.into(),
            quote_bg: colors.quote_bg.into(),
            table_head_bg: colors.table_head_bg.into(),
            para_size: para,
            para_line: line,
            para_gap: px(4.),
            code_size: code,
            code_line,
            blank_h: px(12.),
            rule_h: px(18.),
            is_dark,
        }
    }

    /// 第 n 级标题的字号与行高（基准同旧 render.rs：正文 14px 时 h1=20 h2=18 h3/4=16 其余 15）。
    /// 随正文字号 **等比缩放**——编辑区/预览/WYSIWYG 的 Ctrl+滚轮/菜单调字号时标题一起
    /// 变大变小，保持标题相对正文的比例固定。WYSIWYG 的 `WyTheme::set_font_size` 也走
    /// [`heading_metrics_for_size`] 同步重算，两处标题永远一致。
    pub(crate) fn heading_metrics(&self, level: u8) -> (Pixels, Pixels) {
        heading_metrics_for_size(self.para_size.as_f32(), level)
    }

    /// 普通正文行的估算高度（px）：段落/列表/引述等多数行的实际高度都接近这个值。
    /// 作为行级虚拟化列表「未测量行统一高度提示」（`ListState::with_uniform_item_height`），
    /// 让滚动条量程从一开始就大致正确，同时避免一次性测量整篇（大文档的内存炸弹）。
    pub(crate) fn body_row_hint(&self) -> Pixels {
        self.para_line + self.para_gap
    }
}
