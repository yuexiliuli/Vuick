//! 排版参数：渲染时从全局主题取一份“快照”，避免每行重复查主题。
//! 所有取值都对齐 gpui_component `node.rs` 的 Markdown 渲染风格：
//! 段落用 `sm`，标题用语义字号 + 加粗，代码块用 `muted` 底 + 等宽字体，
//! 引述用浅灰底 + 强调色左边框。

use gpui::{font, px, rgba, App, Hsla, Pixels, SharedString};

/// WYSIWYG 语法配色的「外壳（chrome）」输入：面板/弹层配色属于宿主外壳
/// （app 自己色板），不是 markdown 语义，由调用方注入。
pub(crate) struct SyntaxChrome {
    /// 语法记号色（`**`、`[` …）与 warning/caution 的强调色。
    pub(crate) marker: Hsla,
    /// 行内代码文字色。
    pub(crate) code_fg: Hsla,
    /// 引述文字 + 左边框色（弱化调）。
    pub(crate) quote: Hsla,
    pub(crate) popover_bg: Hsla,
    pub(crate) popover_border: Hsla,
    pub(crate) popover_fg: Hsla,
    pub(crate) popover_danger: Hsla,
}

/// 排版参数快照（见模块文档）。
pub(crate) struct StyleParams {
    pub(crate) mono: SharedString,
    pub(crate) fg: Hsla,
    pub(crate) muted_fg: Hsla,
    /// 代码块背景（白 #F3F4F4 / 黑 #282936），见 [`StyleParams::from_theme_with_font`]。
    pub(crate) code_bg: Hsla,
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
        // 代码块背景按主题定制：白 = #F3F4F4，黑 = #282936。
        let code_bg = if is_dark {
            gpui::rgb(0x282936).into()
        } else {
            gpui::rgb(0xf3f4f4).into()
        };
        Self {
            mono: typo.mono.clone(),
            fg: colors.foreground,
            muted_fg: colors.muted_foreground,
            code_bg,
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

    /// 普通正文行的估算高度（px）：段落/列表/引述等多数行的实际高度都接近这个值。
    /// 作为行级虚拟化列表「未测量行统一高度提示」（`ListState::with_uniform_item_height`），
    /// 让滚动条量程从一开始就大致正确，同时避免一次性测量整篇（大文档的内存炸弹）。
    pub(crate) fn body_row_hint(&self) -> Pixels {
        self.para_line + self.para_gap
    }

    /// 由「单一主题映射源」（本参数）派生 WYSIWYG 编辑器的 [`SyntaxStyle`]。
    ///
    /// markdown 语义色全部取自本参数（`code_bg`/`accent`/`divider`/`mono`）；只读的
    /// 外壳 chrome（面板/弹层、记号/代码前景）用 [`SyntaxChrome`] 注入。app 的
    /// `wysiwyg_style` 是唯一调用方，不再手拼 [`SyntaxStyle`]（两套配色一处定义）。
    pub(crate) fn syntax_style(&self, chrome: SyntaxChrome) -> crate::markdown::SyntaxStyle {
        let accent = self.accent;
        crate::markdown::SyntaxStyle {
            marker: chrome.marker,
            code: chrome.code_fg,
            code_bg: self.code_bg,
            link: accent,
            tag: accent,
            quote: chrome.quote,
            alert_note: accent,
            alert_tip: accent,
            alert_important: accent,
            alert_warning: chrome.marker,
            alert_caution: chrome.marker,
            alert_icons: None,
            property_icon: None,
            rule: self.divider,
            mark_bg: rgba(0xFFD6004D).into(),
            block_label: None,
            block_label_gen: 0,
            block_ref_count: None,
            popover_bg: chrome.popover_bg,
            popover_border: chrome.popover_border,
            popover_fg: chrome.popover_fg,
            popover_hover: accent,
            popover_divider: self.divider,
            popover_danger: chrome.popover_danger,
            mono: font(self.mono.clone()),
        }
    }
}
