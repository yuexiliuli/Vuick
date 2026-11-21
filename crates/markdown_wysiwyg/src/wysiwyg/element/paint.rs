//! 宿主 `element/paint.rs` 薄 shim：绘制图元算法体已随引擎迁移到
//! `markdown_wysiwyg_engine::paint`，本模块保留宿主旧签名入口（paint 取
//! `&Entity<WyEditor>` 的经 `editor.update(cx, |e, cx| …)` 传入 `(&e.rd, e)`、纯图元
//! 同签名 re-export），宿主 `element/mod.rs` 的 `paint_band_rows` 等 glue 对
//! `super::paint::*` 引用原样成立。

use std::ops::Range;

use gpui::{App, Hsla, Point, Pixels, Window};
use markdown_wysiwyg_engine::element::{BlockCard, Frame, ShapedImage, ShapedRow, ShapedTable};
use markdown_wysiwyg_engine::{WyRenderHost, WyTheme};

use super::WyEditor;

/// 纯图元（同签名）直接 re-export（供既有路径原样成立；部分名仅测试消费时 lib 构建
/// 报 unused——re-export 是路径面，允许压）。
#[allow(unused_imports)]
pub(crate) use markdown_wysiwyg_engine::paint::{
    alert_color, paint_alert_chrome, paint_hscroll_thumb, paint_image_fallback,
    paint_inline_code_pills, paint_row_gutter, paint_text_row, paint_quote_band, quote_band_w,
};
/// 一块 `$$` 卡渲染表示的单一决策类型（测试经 `super::paint::MathCardDisplay` 消费）。
pub(crate) use markdown_wysiwyg_engine::paint::MathCardDisplay;

/// 表格块网格。
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_table_row(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    t: &ShapedTable,
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |e, cx| {
        let rd = &e.rd;
        let host: &dyn WyRenderHost = &*e;
        markdown_wysiwyg_engine::paint::paint_table_row(
            rd, host, origin, theme, f, row, t, window, cx,
        );
    });
}

/// 代码块面板 + 底部横向滚动条。
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_code_row(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |e, cx| {
        let rd = &e.rd;
        let host: &dyn WyRenderHost = &*e;
        markdown_wysiwyg_engine::paint::paint_code_row(rd, host, origin, theme, f, row, window, cx);
    });
}

/// 图片行（异步加载 + 占位 + 编辑按钮/调整条 + 选中描边）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_image_row(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    img: &ShapedImage,
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |e, cx| {
        let rd = &e.rd;
        let host: &dyn WyRenderHost = &*e;
        markdown_wysiwyg_engine::paint::paint_image_row(
            rd, host, origin, theme, f, row, img, window, cx,
        );
    });
}

/// 标题折叠箭头。
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_heading_chevron(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |e, cx| {
        let rd = &e.rd;
        let host: &dyn WyRenderHost = &*e;
        markdown_wysiwyg_engine::paint::paint_heading_chevron(
            rd, host, origin, theme, f, row, window, cx,
        );
    });
}

/// 行内 `$…$` 数学。
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_inline_maths(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |e, cx| {
        let rd = &e.rd;
        let host: &dyn WyRenderHost = &*e;
        markdown_wysiwyg_engine::paint::paint_inline_maths(
            rd, host, origin, theme, f, row, window, cx,
        );
    });
}

/// 行内 `![alt](src)` 缩略图。
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_inline_images(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |e, cx| {
        let rd = &e.rd;
        let host: &dyn WyRenderHost = &*e;
        markdown_wysiwyg_engine::paint::paint_inline_images(
            rd, host, origin, theme, f, row, window, cx,
        );
    });
}

/// 一块 `$$` 卡当前该画什么。
/// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub(crate) fn math_card_display(
    editor: &WyEditor,
    src_line: usize,
    key: &str,
) -> MathCardDisplay {
    let host: &dyn WyRenderHost = editor;
    let rd = &editor.rd;
    markdown_wysiwyg_engine::paint::math_card_display(rd, host, src_line, key)
}

/// 块级卡片（数学 / Mermaid）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_card(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    row: &ShapedRow,
    card: &BlockCard,
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |e, cx| {
        let rd = &e.rd;
        let host: &dyn WyRenderHost = &*e;
        markdown_wysiwyg_engine::paint::paint_card(
            rd, host, origin, theme, f, row, card, window, cx,
        );
    });
}

/// 当前选区。
pub(crate) fn editor_selection(editor: &gpui::Entity<WyEditor>, cx: &App) -> Option<Range<usize>> {
    let host: &dyn WyRenderHost = editor.read(cx);
    markdown_wysiwyg_engine::paint::editor_selection(host)
}

/// 选区高亮。
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_selection(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    range: &Range<usize>,
    hl: Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |e, cx| {
        let rd = &e.rd;
        let host: &dyn WyRenderHost = &*e;
        markdown_wysiwyg_engine::paint::paint_selection(
            rd, host, origin, theme, f, range, hl, window, cx,
        );
    });
}

/// 普通文本行选区背景矩形（几何纯函数）。
/// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub(crate) fn text_selection_hl_rects(
    e: &WyEditor,
    r: &ShapedRow,
    range: &Range<usize>,
    pad: f32,
    aln0: f32,
) -> Vec<(f32, f32, f32)> {
    let host: &dyn WyRenderHost = e;
    markdown_wysiwyg_engine::paint::text_selection_hl_rects(host, r, range, pad, aln0)
}

/// 注记跳转加亮。
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_note_marker_highlight(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |e, cx| {
        let rd = &e.rd;
        let host: &dyn WyRenderHost = &*e;
        markdown_wysiwyg_engine::paint::paint_note_marker_highlight(
            rd, host, origin, theme, f, window, cx,
        );
    });
}

/// 搜索跳转高亮。
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_search_highlight(
    editor: &gpui::Entity<WyEditor>,
    origin: Point<Pixels>,
    theme: &WyTheme,
    f: &Frame,
    window: &mut Window,
    cx: &mut App,
) {
    editor.update(cx, |e, cx| {
        let rd = &e.rd;
        let host: &dyn WyRenderHost = &*e;
        markdown_wysiwyg_engine::paint::paint_search_highlight(
            rd, host, origin, theme, f, window, cx,
        );
    });
}
