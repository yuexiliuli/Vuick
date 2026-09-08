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
    pub(crate) muted_bg: Hsla,
    pub(crate) divider: Hsla,
    pub(crate) accent: Hsla,
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
}

impl StyleParams {
    /// 从主题构建排版参数；`font_override` 可覆盖正文/代码字号（EPUB 缩放字号用）。
    pub(crate) fn from_theme_with_font(cx: &mut App, font_override: Option<f32>) -> Self {
        let theme = gpui_component::Theme::global(cx);
        let tokens = theme.semantic_tokens();
        let colors = tokens.colors;
        let typo = tokens.typography;
        let sm = typo.sm;
        let mono = typo.mono_md;
        // 正文基准字号：默认用主题 sm；有覆盖时按其缩放等宽字体（等宽按 0.85 比例）
        let (para, line, code, code_line) = match font_override {
            Some(fs) => {
                let fs = fs.max(9.0);
                (
                    px(fs),
                    px(fs * 1.45),
                    px(fs * 0.88),
                    px(fs * 1.3),
                )
            }
            None => (
                sm.size,
                sm.line_height,
                mono.size,
                mono.line_height,
            ),
        };
        let is_dark = theme.is_dark();
        // 引用旧 render.rs：引述 = 浅灰底 + 粗左边框；表头 = 浅灰底。暗色模式配深色变体。
        let (quote_bg, table_head_bg) = if is_dark {
            (gpui::rgb(0x1b2431).into(), gpui::rgb(0x1f2937).into())
        } else {
            (gpui::rgb(0xf9fafb).into(), gpui::rgb(0xf3f4f6).into())
        };
        Self {
            mono: typo.mono.clone(),
            fg: colors.foreground,
            muted_fg: colors.muted_foreground,
            muted_bg: colors.muted,
            divider: colors.border,
            accent: colors.accent,
            bg: colors.background,
            quote_bg,
            table_head_bg,
            para_size: para,
            para_line: line,
            para_gap: px(4.),
            code_size: code,
            code_line: code_line,
            blank_h: px(12.),
            rule_h: px(18.),
        }
    }

    /// 第 n 级标题的字号与行高（借鉴旧 render.rs：h1=20 h2=18 h3=16 其余 15）。
    pub(crate) fn heading_metrics(&self, level: u8) -> (Pixels, Pixels) {
        let fs = match level {
            1 => px(20.),
            2 => px(18.),
            3 | 4 => px(16.),
            _ => px(15.),
        };
        (fs, px(fs.as_f32() * 1.45))
    }
}
