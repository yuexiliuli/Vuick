//! 侧边栏「面板头」公共渲染（消除 toc / files 两处同构复制）。
//!
//! 结构：flex 行 + items_center + h36 + px10 + border_b_1（bar_border 色）+ semibold
//! 13.5 标题；`extra` 为标题右侧可选附加元素（如文件树根目录名截断行，flex_1）。
//! fg / border 由调用方各自取既有 token（`text_primary` / `bar_border`）传入，保持零裸色。

use gpui::{
    div, px, prelude::FluentBuilder as _, InteractiveElement, IntoElement, ParentElement, SharedString,
    Styled,
};

/// 侧栏面板头：`title` +（可选）右侧 `extra`。视觉形状 h36 / px10 / border_b_1 / 13.5 semibold。
/// `extra` 为 None 时面板头仅单子项标题（`gap_2` 无视觉影响，与既有 toc 头一致）。
pub(crate) fn sidebar_panel_header(
    id: &str,
    title: impl Into<SharedString>,
    fg: gpui::Hsla,
    border: gpui::Hsla,
    extra: Option<impl IntoElement>,
) -> impl IntoElement {
    let id_owned = id.to_string();
    div()
        .id(id_owned.clone())
        .debug_selector(move || id_owned.clone())
        .flex()
        .items_center()
        .gap_2()
        .px(px(10.))
        .h(px(36.))
        .border_b_1()
        .border_color(border)
        .child(
            div()
                .text_size(px(13.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(fg)
                .child(title.into()),
        )
        .when_some(extra, |this, e| this.child(e))
}
