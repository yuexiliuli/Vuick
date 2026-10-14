//! 排版参数：渲染时从全局主题取一份“快照”，避免每行重复查主题。
//! 所有取值都对齐 gpui_component `node.rs` 的 Markdown 渲染风格：
//! 段落用 `sm`，标题用语义字号 + 加粗，代码块用 `muted` 底 + 等宽字体，
//! 引述用浅灰底 + 强调色左边框。
//!
//! 第 3 波迁移后：**颜色/等宽字体单一来源 `ui::theme`**（语义色板 +
//! `Typography`），不再读 gpui-component 主题语义 token；正文/标题字号体系仍由
//! `config::layout::DEFAULT_FONT_SIZE` 驱动（统一缩放架构）。

use gpui::{px, App, Hsla, Pixels, SharedString};

/// 排版参数快照（见模块文档）。
pub struct StyleParams {
    pub mono: SharedString,
    pub fg: Hsla,
    pub muted_fg: Hsla,
    /// 代码块背景（白 #F3F4F4 / 黑 #282936），见 [`StyleParams::from_theme_with_font`]。
    pub code_bg: Hsla,
    /// 行内代码（反引号）文字色（白 #222222 / 黑 #DADADA，近正文——靠底色 + 等宽字体区分）。
    pub code_inline_fg: Hsla,
    /// 行内代码圆角 pill 底色（白 #F3F4F4 / 黑 #3E3F40）。
    pub code_inline_bg: Hsla,
    pub divider: Hsla,
    pub accent: Hsla,
    /// 链接色（`[t](u)` / wiki / tag / 裸 URL；明 #0969da / 暗 #79c0ff，单一源
    /// `ColorScheme::link`——预览 / WYSIWYG / 源码编辑器一个蓝）。
    pub link: Hsla,
    /// 引述块背景（浅灰卡片感）。
    pub quote_bg: Hsla,
    /// 表头底色（浅灰）。
    pub table_head_bg: Hsla,
    /// Mermaid 图节点分类 accent 色板（6 色，亮/暗由 `ColorScheme` 分流；节点按出现
    /// 顺序循环取色，作者 classDef/style 显式色不参与）。
    pub mermaid_accents: [Hsla; 6],
    /// Mermaid 边线/边框主色（≈ divider 同 hue 提一档：中性灰，随明暗 token 自动）。
    pub mermaid_border: Hsla,
    pub para_size: Pixels,
    pub para_line: Pixels,
    pub code_size: Pixels,
    pub code_line: Pixels,
    /// 当前主题是否为暗色（预览与 WYSIWYG 各自派生语法色/代码底色时共用）。
    pub is_dark: bool,
}

/// 由正文字号推导「正文行高/代码字号/代码行高」——**唯一**的缩放系数来源
/// （`0.88/1.3/1.45` 只在这里出现一次），预览与 WYSIWYG 共用，避免两处各写一套。
pub(crate) fn typography_for_size(fs: f32) -> (Pixels, Pixels, Pixels) {
    (px(fs * 1.45), px(fs * 0.88), px(fs * 1.3))
}

/// 由正文字号推导第 n 级标题的字号与行高（预览与 WYSIWYG 共用，见
/// [`StyleParams::heading_metrics`]）。设计字号基准集中在
/// `config::markdown::HEADING_DESIGN_SIZES` / `HEADING_DESIGN_BASE`，
/// 行高系数在 `config::markdown::HEADING_LINE_RATIOS`（分级别）
/// （正文 14px 参考字号下 h1=24 h2=20 h3=18 h4=16 h5/6=15），其余字号按
/// `body_fs / HEADING_DESIGN_BASE` 等比缩放，标题相对正文的比例保持不变。
pub(crate) fn heading_metrics_for_size(body_fs: f32, level: u8) -> (Pixels, Pixels) {
    let lv = match level {
        1..=6 => level as usize,
        _ => 6,
    };
    let design = config::markdown::HEADING_DESIGN_SIZES[lv];
    let line_ratio = config::markdown::HEADING_LINE_RATIOS[lv];
    let fs = body_fs / config::markdown::HEADING_DESIGN_BASE * design;
    (px(fs), px(fs * line_ratio))
}

impl StyleParams {
    /// 从主题构建排版参数；`font_override` 可覆盖正文/代码字号（EPUB 缩放字号用）。
    /// 颜色/等宽字体单一来自 `ui::theme`（语义色板 + `Typography`）。
    pub fn from_theme_with_font(cx: &mut App, font_override: Option<f32>) -> Self {
        let theme = ui::theme::Theme::global(cx);
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
        // 代码块/行内代码/引述/表头/链接等语义色：全部取自色板（第 5 步谐波化后换到
        // 中性灰阶 + 蓝色系——值有意的演化，见 ui/theme.rs 钉死测试）。
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
            quote_bg: colors.quote_bg.into(),
            table_head_bg: colors.table_head_bg.into(),
            mermaid_accents: colors.mermaid_accent_palette.map(Into::into),
            mermaid_border: colors.mermaid_border.into(),
            para_size: para,
            para_line: line,
            code_size: code,
            code_line,
            is_dark,
        }
    }

    /// 第 n 级标题的字号与行高（基准同旧 render.rs：正文 14px 时 h1=24 h2=20 h3=18 h4=16
    /// h5/6=15；行高系数分级 `HEADING_LINE_RATIOS`，大标题紧、小标题透气）。
    /// 随正文字号 **等比缩放**——编辑区/预览/WYSIWYG 的 Ctrl+滚轮/菜单调字号时标题一起
    /// 变大变小，保持标题相对正文的比例固定。WYSIWYG 的 `WyTheme::set_font_size` 也走
    /// [`heading_metrics_for_size`] 同步重算，两处标题永远一致。
    pub(crate) fn heading_metrics(&self, level: u8) -> (Pixels, Pixels) {
        heading_metrics_for_size(self.para_size.as_f32(), level)
    }
}
