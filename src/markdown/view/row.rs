//! 渲染单个渲染行（固定高度、不换行、超出裁剪；超宽由外层横向滚动容器处理）。
//!
//! `wrap = true` 时为**自动换行模式**：文本行高度自适应、自然折行（用于 EPUB 阅读）；
//! `wrap = false`（默认）为单行 + 横向滚动模式。

use std::rc::Rc;

use gpui::{
    div, img, px, App, FontWeight, InteractiveElement, IntoElement, ObjectFit, ParentElement,
    ScrollHandle, StatefulInteractiveElement, Styled, StyledImage,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};

use crate::markdown::parser::{Row, RowKind};
use crate::markdown::view::dimensions::{IMAGE_MAX_W, IMAGE_ROW_DEFAULT_H};
use crate::markdown::view::style::StyleParams;
use crate::markdown::view::table::render_table;

/// 渲染一行代码（等宽、不换行、浅灰底）。
fn code_band(t: gpui::SharedString, p: &StyleParams) -> gpui::Div {
    div()
        .debug_selector(|| "md-code-band".into())
        .h(p.code_line)
        .w_full()
        .pl_3()
        .pr_3()
        .whitespace_nowrap()
        .font_family(p.mono.clone())
        .text_size(p.code_size)
        .text_color(p.fg)
        .bg(p.muted_bg)
        .child(t)
}

/// 渲染单个行。`code_handle` 为代码块稳定的横向滚动句柄（None 时自动创建）。
pub(crate) fn render_row(
    row: &Rc<Row>,
    p: &Rc<StyleParams>,
    wrap: bool,
    code_handle: Option<ScrollHandle>,
    _cx: &mut App,
) -> gpui::AnyElement {
    // 表格是带 id 的有状态元素，直接整体转换，不参与下方 Div 匹配；
    // 外层加 px_3 内缩，避免表格网格顶到左边缘（与正文对齐）。
    if let RowKind::Table = row.kind {
        let table = row.table.as_ref().expect("Table 行必须携带表格结构");
        return div()
            .w_full()
            .flex_shrink_0()
            .px_3()
            .py_1()
            .child(render_table(table, p))
            .into_any_element();
    }
    // 独立图片行：按图片固有比例显示，高度随可用宽度自适应（缩小窗口不出现大片留白）。
    // 宽不超过 IMAGE_MAX_W；未知尺寸（URL/不可读）回退固定 IMAGE_ROW_DEFAULT_H。
    // src 为空/加载失败时显示 alt 作为 fallback。
    if let RowKind::Image = row.kind {
        let src = row.src.clone().unwrap_or_default().to_string();
        let alt = row.text.clone();
        let fallback_color = p.muted_fg;

        let img_el = match row.img_data.clone() {
            // 内存图片（EPUB 不落盘）：ImageSource::Image 走 ImageDecoder 由字节解码
            Some(image) => img(gpui::ImageSource::Image(image))
                .object_fit(ObjectFit::Contain)
                .w_full()
                .h_full()
                .with_fallback(move || {
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.))
                        .text_color(fallback_color.clone())
                        .child(alt.clone())
                        .into_any_element()
                })
                .into_any_element(),
            None if src.is_empty() => div()
                .text_size(px(13.))
                .text_color(fallback_color)
                .child(alt)
                .into_any_element(),
            None => {
                // 本地绝对路径 → PathBuf（ImageAssetLoader 走 Resource::Path = fs::read）；
                // URL → &str（走向 Resource::Uri，经 HTTP 加载）。
                let src_is_url = src.starts_with("http://")
                    || src.starts_with("https://")
                    || src.starts_with("file://");
                let source: gpui::ImageSource = if src_is_url {
                    src.as_str().into()
                } else {
                    std::path::PathBuf::from(&src).into()
                };
                img(source)
                    .object_fit(ObjectFit::Contain)
                    .w_full()
                    .h_full()
                    .with_fallback(move || {
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(13.))
                            .text_color(fallback_color.clone())
                            .child(alt.clone())
                            .into_any_element()
                    })
                    .into_any_element()
            }
        };

        // 已知固尺寸：图片盒宽 = min(视口宽, IMAGE_MAX_W)，高由宽 * h/w 得出
        //（aspect_ratio 使高度随可用宽度缩放 → 缩小窗口不会留白）；
        // 外层铺满宽度并 justify_center，使图片盒在窄于 600px 的视口里也水平居中。
        let img_w = row.img_w;
        let img_h = row.img_h;
        let box_ = match (img_w, img_h) {
            (Some(w), Some(h)) if w > 0 && h > 0 => div()
                .w_full()
                .max_w(px(IMAGE_MAX_W))
                .aspect_ratio(w as f32 / h as f32)
                .debug_selector(|| "md-img-box".into()),
            _ => div()
                .w(px(IMAGE_MAX_W))
                .max_w_full()
                .h(px(IMAGE_ROW_DEFAULT_H))
                .min_h(px(IMAGE_ROW_DEFAULT_H))
                .debug_selector(|| "md-img-box".into()),
        };
        return div()
            .w_full()
            .flex_shrink_0()
            .px_3()
            .py_1()
            .flex()
            .justify_center()
            .items_center()
            .child(box_.child(img_el))
            .into_any_element();
    }

    // ---------- 自动换行模式：仅对文本行生效，代码/规则/空行保持原样 ----------
    if wrap {
        // 换行模式下代码保持不换行：整个围栏折叠成一个渲染行（多行紧贴相连成连续块）。
        // 该块按内容宽撑开（content_w 计入最宽代码行 → 外层 md-harea 统一横向滚动，
        // 底部单一横向滚动条可拖动）；正文仍限制在视口宽内折行。
        if row.kind == RowKind::Code {
            let lines = row.code_lines.clone();
            let p_ = p.clone();
            let block_h = lines.len() as f32 * p.code_line.as_f32();
            let h = code_handle.unwrap_or_else(ScrollHandle::new);
            let column = div()
                .debug_selector(|| "md-code-row".into())
                .flex()
                .flex_col()
                .flex_shrink_0()
                .min_w_full()
                .children(lines.iter().map(|l| code_band(l.clone(), &p_)));
            return div()
                .debug_selector(|| "md-code-hscroll".into())
                .h(px(block_h))
                .w_full()
                .flex_shrink_0()
                .px_3()
                .child(
                    div()
                        .debug_selector(|| "md-code-hscroll-inner".into())
                        .w_full()
                        .h_full()
                        .relative()
                        .overflow_hidden()
                        .child(
                            div()
                                .id("md-code-scroll")
                                .w_full()
                                .h_full()
                                .flex()
                                .flex_row()
                                .overflow_x_scroll()
                                .track_scroll(&h)
                                // 滚轮放代码块上：代码可横向滚动时 stop_propagation，让滚轮只横向滚代码块、
                                // 页面（纵向）不再跟着滚动；不可横向滚动时放行交给页面（纵向）滚动。
                                .on_scroll_wheel({
                                    let h = h.clone();
                                    move |_event, _window, cx| {
                                        if h.max_offset().x > px(0.) {
                                            cx.stop_propagation();
                                        }
                                    }
                                })
                                .child(column),
                        )
                        .child(
                            div()
                                .debug_selector(|| "md-code-scrollbar".into())
                                .absolute()
                                .inset_0()
                                .child(
                                    Scrollbar::horizontal(&h)
                                        .mode(ScrollbarMode::Always)
                                        .viewport_from_layout(),
                                ),
                        ),
                )
                .into_any_element();
        }
        match row.kind {
            RowKind::Paragraph => {
                let t = row.text.clone();
                return div()
                    .w_full()
                    .min_h(px(p.para_line.as_f32() + p.para_gap.as_f32() + 6.0))
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .whitespace_normal()
                    .text_size(p.para_size)
                    .text_color(p.fg)
                    .child(t)
                    .into_any_element();
            }
            RowKind::Heading(n) => {
                let (fs, lh) = p.heading_metrics(n);
                let t = row.text.clone();
                return div()
                    .w_full()
                    .min_h(lh)
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .whitespace_normal()
                    .text_size(fs)
                    .text_color(p.fg)
                    .font_weight(FontWeight::BOLD)
                    .child(t)
                    .into_any_element();
            }
            RowKind::Quote => {
                // 引述：外层 px_3 内缩（与正文对齐），背景带/左边框移到内层元素，
                // 避免背景带顶到左边缘（同代码块做法）。
                let t = row.text.clone();
                return div()
                    .w_full()
                    .min_h(px(p.para_line.as_f32() + p.para_gap.as_f32() + 6.0))
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .child(
                        div()
                            .debug_selector(|| "md-quote-band".into())
                            .w_full()
                            .pl_3()
                            .pr_3()
                            .py_1()
                            .border_l_2()
                            .border_color(p.accent)
                            .bg(p.quote_bg)
                            .whitespace_normal()
                            .text_size(p.para_size)
                            .text_color(p.muted_fg)
                            .child(t),
                    )
                    .into_any_element();
            }
            RowKind::Bullet => {
                let mut text = row.text.to_string();
                if let Some(prefix) = &row.prefix {
                    text = format!("{prefix} {text}");
                }
                return div()
                    .w_full()
                    .min_h(px(p.para_line.as_f32() + p.para_gap.as_f32() + 6.0))
                    .flex_shrink_0()
                    .pl_5()
                    .pr_3()
                    .py_1()
                    .whitespace_normal()
                    .text_size(p.para_size)
                    .text_color(p.fg)
                    .child(text)
                    .into_any_element();
            }
            _ => {}
        }
    }

    let el: gpui::Div = match row.kind {
        RowKind::Blank => div().h(p.blank_h).w_full().flex_shrink_0(),
        RowKind::Rule => div()
            .h(p.rule_h)
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .px_3()
            .child(div().h(px(1.)).w_full().bg(p.divider).rounded_full()),
        RowKind::Code => {
            // 代码块：整栏折叠为一行的所有代码行按 h(code_line) 纵向堆叠成连续块，
            // 每行浅灰背景带（左右内缩 px_3 与正文对齐），等宽字体。超宽由外层横向滚动。
            let lines = row.code_lines.clone();
            let p_ = p.clone();
            let block_h = lines.len() as f32 * p.code_line.as_f32();
            div()
                .h(px(block_h))
                .w_full()
                .flex_shrink_0()
                .px_3()
                .child(
                    div()
                        .debug_selector(|| "md-code-row".into())
                        .h_full()
                        .w_full()
                        .flex()
                        .flex_col()
                        .children(lines.iter().map(|l| code_band(l.clone(), &p_))),
                )
        }
        RowKind::Heading(n) => {
            let (fs, lh) = p.heading_metrics(n);
            div()
                .h(lh)
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
        RowKind::Quote => {
            // 引述：外层 px_3 内缩（与正文对齐），背景带/左边框移到内层元素，
            // 避免背景带和“>”边条顶到左边缘（同代码块做法）。
            let t = row.text.clone();
            div()
                .h(px(p.para_line.as_f32() + p.para_gap.as_f32()))
                .w_full()
                .flex_shrink_0()
                .px_3()
                .flex()
                .items_center()
                .overflow_hidden()
                .child(
                    div()
                        .debug_selector(|| "md-quote-band".into())
                        .h_full()
                        .w_full()
                        .pl_3()
                        .pr_3()
                        .border_l_2()
                        .border_color(p.accent)
                        .bg(p.quote_bg)
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(p.para_size)
                        .text_color(p.muted_fg)
                        .child(t),
                )
        }
        RowKind::Bullet => {
            let mut text = row.text.to_string();
            if let Some(prefix) = &row.prefix {
                text = format!("{prefix} {text}");
            }
            div()
                .h(px(p.para_line.as_f32() + p.para_gap.as_f32()))
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
            .h(px(p.para_line.as_f32() + p.para_gap.as_f32()))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(p.para_size)
            .text_color(p.fg)
            .child(row.text.clone()),
        RowKind::Table => unreachable!("表格已在上方提前返回"),
        RowKind::Image => unreachable!("图片行已在上方提前返回"),
    };
    el.into_any_element()
}
