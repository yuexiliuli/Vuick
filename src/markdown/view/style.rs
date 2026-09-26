//! 排版参数：渲染时从全局主题取一份“快照”，避免每行重复查主题。
//! 所有取值都对齐 gpui_component `node.rs` 的 Markdown 渲染风格：
//! 段落用 `sm`，标题用语义字号 + 加粗，代码块用 `muted` 底 + 等宽字体，
//! 引述用浅灰底 + 强调色左边框。

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
    /// `config::colors::LINK_LIGHT/LINK_DARK`——预览 / WYSIWYG / 源码编辑器一个蓝）。
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
    pub(crate) fn from_theme_with_font(cx: &mut App, font_override: Option<f32>) -> Self {
        let theme = gpui_component::Theme::global(cx);
        let tokens = theme.semantic_tokens();
        let colors = tokens.colors;
        let typo = tokens.typography;
        // 正文基准字号：默认与编辑区 / WYSIWYG 对齐（layout::DEFAULT_FONT_SIZE），不再取
        // 主题 sm——否则预览默认字号与编辑器不一致，三处无法统一调节。有覆盖时按覆盖值缩放；
        // 正文字号一变，代码/行高系数统一走 `typography_for_size`（同一缩放来源）。
        let fs = font_override.unwrap_or(crate::config::layout::DEFAULT_FONT_SIZE).max(9.0);
        let (pl, cs, cl) = typography_for_size(fs);
        let (para, line, code, code_line) = (px(fs), pl, cs, cl);
        let is_dark = theme.is_dark();
        // 引用旧 render.rs：引述 = 浅灰底 + 粗左边框；表头 = 浅灰底。暗色模式配深色变体。
        let (quote_bg, table_head_bg) = if is_dark {
            (gpui::rgb(0x1b2431).into(), gpui::rgb(0x1f2937).into())
        } else {
            (gpui::rgb(0xf9fafb).into(), gpui::rgb(0xf3f4f6).into())
        };
        // 代码块背景按主题定制：白 = #F3F4F4，黑 = #282936。
        let code_bg = if is_dark {
            gpui::rgb(0x282936).into()
        } else {
            gpui::rgb(0xf3f4f4).into()
        };
        // 行内代码（反引号）pill 配色按主题定制：白 = 底 #F3F4F4 + 文字 #222222，
        // 黑 = 底 #3E3F40 + 文字 #DADADA（文字近正文，靠底色 + 等宽字体区分）。
        let (code_inline_fg, code_inline_bg) = if is_dark {
            (gpui::rgb(0xdadada).into(), gpui::rgb(0x3e3f40).into())
        } else {
            (gpui::rgb(0x222222).into(), gpui::rgb(0xf3f4f4).into())
        };
        // 链接蓝：单一源 `config::colors::LINK_LIGHT/DARK`（与源码编辑器同值）。
        let link = if is_dark {
            gpui::rgb(crate::config::colors::LINK_DARK).into()
        } else {
            gpui::rgb(crate::config::colors::LINK_LIGHT).into()
        };
        Self {
            mono: typo.mono.clone(),
            fg: colors.foreground,
            muted_fg: colors.muted_foreground,
            code_bg,
            code_inline_fg,
            code_inline_bg,
            divider: colors.border,
            accent: colors.accent,
            link,
            bg: colors.background,
            quote_bg,
            table_head_bg,
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
