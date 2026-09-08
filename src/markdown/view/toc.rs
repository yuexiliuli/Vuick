//! 目录：右上角悬浮按钮（gpui-component Button + DropdownMenu），
//! 点击弹出下拉菜单列出标题；条目 hover 背景/圆角由组件主题自动处理（含从下往上移动）。
//! 条目内部样式读取当前主题色，保证黑白切换后依旧清晰。

use gpui::{
    div, px, Context, FontWeight, Hsla, IntoElement, ParentElement, Styled, WeakEntity, Window,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};

use super::MarkdownPreview;

/// 构建右上角“☰”目录触发按钮（含下拉菜单）。
pub(crate) fn build_toc_trigger(
    weak: WeakEntity<MarkdownPreview>,
    toc: Vec<(usize, u8, String)>,
    fg: Hsla,
) -> impl IntoElement {
    Button::new("md-toc-toggle")
        .label("☰")
        .ghost()
        .w(px(30.))
        .h(px(30.))
        .rounded(px(6.))
        .text_size(px(15.))
        .text_color(fg)
        .dropdown_menu(
            move |menu: PopupMenu, _window: &mut Window, cx: &mut Context<PopupMenu>| {
                let mut m = menu;
                // 读取当前主题语义色（作用于自定义目录条目内容）
                let theme = gpui_component::Theme::global(cx);
                let colors = theme.semantic_tokens().colors;
                let color = colors.foreground;
                let muted = colors.muted_foreground;
                let accent = colors.accent;
                if toc.is_empty() {
                    m = m.item(PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .px_3()
                            .py_1()
                            .text_size(px(13.))
                            .text_color(muted)
                            .child("（空目录）")
                    }));
                } else {
                    for &(row_ix, level, ref text) in &toc {
                        let label = text.clone();
                        let weak = weak.clone();
                        let color = color.clone();
                        let muted = muted.clone();
                        let accent = accent.clone();
                        m = m.item(
                            PopupMenuItem::element(move |_, _| {
                                let indent = (level.saturating_sub(1) as f32) * 16.0;
                                div()
                                    .w_full()
                                    .pl(px(indent))
                                    .pr_2()
                                    .py_0()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(5.))
                                            .h(px(5.))
                                            .rounded_full()
                                            .bg(if level == 1 { accent } else { muted }),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_size(if level == 1 { px(13.5) } else { px(13.) })
                                            .text_color(color)
                                            .font_weight(if level == 1 {
                                                FontWeight::SEMIBOLD
                                            } else {
                                                FontWeight::NORMAL
                                            })
                                            .child(label.clone()),
                                    )
                            })
                            .on_click(move |_, _window, cx| {
                                // 点击后组件菜单自动 dismiss（PopupMenuItem::ElementItem）
                                let _ = weak.update(cx, |this, cx| {
                                    this.scroll_to_row(row_ix);
                                    cx.notify();
                                });
                            }),
                        );
                    }
                }
                m
            },
        )
}
