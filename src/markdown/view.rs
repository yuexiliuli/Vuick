//! 自研 Markdown 渲染器的视图层：行级虚拟化 + 自绘置顶滚动条。
//!
//! 核心思路（参考 vellum / zorite 的行级虚拟化）：
//! - 文档被拆成**一行一条**的渲染行（见 [`crate::markdown::parser`]）；
//! - 用 gpui 的 `List` 做虚拟化：只构建 / 绘制可见行，滚动时按需重建；
//! - 滚动条用 `vertical_scrollbar` 作为渲染树**最后**一个子元素叠加在最上层，
//!   不再像 gpui-component 的 TextView 那样被代码块 / 表格等不透明背景盖住。

use std::rc::Rc;

use gpui::{
    div, list, px, App, Context, FontWeight, Hsla, InteractiveElement, IntoElement, ListAlignment,
    ListState, ParentElement, Pixels, Render, SharedString, Styled, Window,
};
use gpui_component::scroll::ScrollableElement as _;

use crate::markdown::parser::{parse_to_rows, Row, RowKind};

/// 行级虚拟化在视口上下各多构建的像素量（保证快速滚动时内容连续）。
const ROW_OVERDRAW_PX: f32 = 600.0;

/// 排版参数：渲染时从全局主题取一份“快照”，避免每行重复查主题。
struct StyleParams {
    mono: SharedString,
    fg: Hsla,
    muted_fg: Hsla,
    muted_bg: Hsla,
    divider: Hsla,
    bg: Hsla,
    para_size: Pixels,
    para_line: Pixels,
    code_size: Pixels,
    code_line: Pixels,
    blank_h: Pixels,
    rule_h: Pixels,
}

impl StyleParams {
    fn from_theme(cx: &mut App) -> Self {
        let theme = gpui_component::Theme::global(cx);
        let tokens = theme.semantic_tokens();
        let colors = tokens.colors;
        let typo = tokens.typography;
        let sm = typo.sm;
        let mono = typo.mono_md;
        Self {
            mono: typo.mono.clone(),
            fg: colors.foreground,
            muted_fg: colors.muted_foreground,
            muted_bg: colors.muted,
            divider: colors.border,
            bg: colors.background,
            para_size: sm.size,
            para_line: sm.line_height,
            code_size: mono.size,
            code_line: mono.line_height,
            blank_h: px(8.),
            rule_h: px(18.),
        }
    }
}

/// 自研 Markdown 预览器（行级虚拟化）。
pub struct MarkdownPreview {
    rows: Rc<Vec<Rc<Row>>>,
    list_state: ListState,
    /// 背景色覆盖（由应用传入，与编辑区保持一致；None 用主题背景）。
    bg: Option<Hsla>,
}

impl MarkdownPreview {
    pub fn new(_cx: &mut App) -> Self {
        let list_state = ListState::new(0, ListAlignment::Top, px(ROW_OVERDRAW_PX)).measure_all();
        Self {
            rows: Rc::new(Vec::new()),
            list_state,
            bg: None,
        }
    }

    /// 设置预览背景色（打开/同步时由应用传入，使预览与编辑区同底色）。
    pub fn set_background(&mut self, bg: Hsla) {
        self.bg = Some(bg);
    }

    /// 设置预览源码（仅影响预览，原文不变）。
    pub fn set_source(&mut self, src: &str, _cx: &mut App) {
        let rows: Vec<Rc<Row>> = parse_to_rows(src).into_iter().map(Rc::new).collect();
        if rows.len() != self.list_state.item_count() {
            // 行数变化 → 重建并 measure_all，保证滚动条范围从一开始就正确
            self.list_state = ListState::new(rows.len(), ListAlignment::Top, px(ROW_OVERDRAW_PX))
                .measure_all();
        } else {
            // 行数不变但内容变了 → 重新测量高度（保留滚动位置）
            self.list_state.remeasure();
        }
        self.rows = Rc::new(rows);
    }
}

impl Render for MarkdownPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        let p = Rc::new(StyleParams::from_theme(cx));
        let bg = self.bg.unwrap_or(p.bg);
        let list_state = self.list_state.clone();

        let content = list(list_state.clone(), move |ix, _window, cx| {
            render_row(&rows[ix], &p, cx)
        })
        .size_full();

        div()
            .id("md-preview")
            .debug_selector(|| "md-preview".into())
            .size_full()
            .relative()
            .bg(bg)
            .child(content)
            // 滚动条作为最后一个子元素 → 叠加在最上层，始终可见
            .vertical_scrollbar(&list_state)
    }
}

/// 渲染单个行（固定高度、不换行、超出裁剪）。
fn render_row(row: &Rc<Row>, p: &Rc<StyleParams>, _cx: &mut App) -> gpui::AnyElement {
    let el: gpui::Div = match row.kind {
        RowKind::Blank => div().h(p.blank_h).w_full().flex_shrink_0(),
        RowKind::Rule => div()
            .h(p.rule_h)
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .px_2()
            .child(div().h(px(2.)).w_full().bg(p.divider).rounded_full()),
        RowKind::Code => {
            // 代码行：整行背景带，文本带左右 padding
            let t = row.text.clone();
            div()
                .h(p.code_line)
                .w_full()
                .flex_shrink_0()
                .bg(p.muted_bg)
                .flex()
                .items_center()
                .overflow_hidden()
                .child(
                    div()
                        .pl_2()
                        .pr_2()
                        .whitespace_nowrap()
                        .font_family(p.mono.clone())
                        .text_size(p.code_size)
                        .text_color(p.fg)
                        .child(t),
                )
        }
        RowKind::Heading(n) => {
            let scale = 1.9f32 - 0.13f32 * (n as f32 - 1.0);
            let fs = px(p.para_size.as_f32() * scale);
            let h = px(p.para_line.as_f32() * scale);
            div()
                .h(h)
                .w_full()
                .flex_shrink_0()
                .flex()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(fs)
                .text_color(p.fg)
                .font_weight(FontWeight::BOLD)
                .child(row.text.clone())
        }
        RowKind::Quote => div()
            .h(p.para_line)
            .w_full()
            .flex_shrink_0()
            .ml_2()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(p.para_size)
            .text_color(p.muted_fg)
            .child(row.text.clone()),
        RowKind::Bullet => {
            let mut text = row.text.to_string();
            if let Some(prefix) = &row.prefix {
                text = format!("{prefix} {text}");
            }
            div()
                .h(p.para_line)
                .w_full()
                .flex_shrink_0()
                .ml_3()
                .flex()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(p.para_size)
                .text_color(p.fg)
                .child(text)
        }
        RowKind::Paragraph => div()
                .h(p.para_line)
                .w_full()
                .flex_shrink_0()
                .flex()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(p.para_size)
                .text_color(p.fg)
                .child(row.text.clone()),
    };
    el.into_any_element()
}
