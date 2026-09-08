//! 渲染单个渲染行（固定高度、不换行、超出裁剪；超宽由外层横向滚动容器处理）。
//!
//! `wrap = true` 时为**自动换行模式**：文本行高度自适应、自然折行（用于 EPUB 阅读）；
//! `wrap = false`（默认）为单行 + 横向滚动模式。

use std::rc::Rc;

use gpui::{
    div, img, px, App, FontWeight, InteractiveElement, IntoElement, ObjectFit, ParentElement,
    Styled, StyledImage,
};

use crate::markdown::parser::{Row, RowKind};
use crate::markdown::view::dimensions::{IMAGE_MAX_W, IMAGE_ROW_DEFAULT_H};
use crate::markdown::view::style::StyleParams;
use crate::markdown::view::table::render_table;

/// 渲染单个行。
pub(crate) fn render_row(
    row: &Rc<Row>,
    p: &Rc<StyleParams>,
    wrap: bool,
    _cx: &mut App,
) -> gpui::AnyElement {
    // 表格是带 id 的有状态元素，直接整体转换，不参与下方 Div 匹配
    if let RowKind::Table = row.kind {
        let table = row.table.as_ref().expect("Table 行必须携带表格结构");
        return render_table(table, p).into_any_element();
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
                let t = row.text.clone();
                return div()
                    .w_full()
                    .min_h(px(p.para_line.as_f32() + p.para_gap.as_f32() + 6.0))
                    .flex_shrink_0()
                    .pl_3()
                    .pr_3()
                    .py_1()
                    .border_l_2()
                    .border_color(p.accent)
                    .bg(p.quote_bg)
                    .whitespace_normal()
                    .text_size(p.para_size)
                    .text_color(p.muted_fg)
                    .child(t)
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
            // 代码行：整行浅灰背景带（与栅栏内其它行连成一块），等宽字体
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
                        .pl_3()
                        .pr_3()
                        .whitespace_nowrap()
                        .font_family(p.mono.clone())
                        .text_size(p.code_size)
                        .text_color(p.fg)
                        .child(t),
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
        RowKind::Quote => div()
            .h(px(p.para_line.as_f32() + p.para_gap.as_f32()))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .pl_3()
            .py_1()
            .border_l_2()
            .border_color(p.accent)
            .bg(p.quote_bg)
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
