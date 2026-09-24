//! 渲染一张 GFM 表格为带边框网格（借鉴旧 render.rs 的表格视觉）：
//! 外边 1px 圆角边框 + 表头浅灰底/加粗 + 表头下分隔线 + 列左竖线 + 数据行上横线，
//! 列宽按 [`crate::markdown::view::dimensions::table_column_widths`] 估算
//! （内容自适应，超宽走外层横向滚动）。

use std::rc::Rc;

use gpui::{div, px, Div, FontWeight, Hsla, InteractiveElement, ParentElement, Pixels, SharedString, Styled};

use crate::markdown::parse::{TableAlign, TableData};
use crate::markdown::view::dimensions::table_column_widths;
use crate::markdown::view::style::StyleParams;

/// 渲染一张 GFM 表格为带边框网格。
pub(crate) fn render_table(table: &TableData, p: &Rc<StyleParams>) -> gpui::Stateful<gpui::Div> {
    let border = p.divider;
    let widths = table_column_widths(table, p);
    let total: f32 = widths.iter().sum();
    let row_h = px(p.para_size.as_f32() * 1.45 + 12.0);

    let mut grid = div()
        .id("md-table")
        .debug_selector(|| "md-table".into())
        .flex()
        .flex_col()
        .w(px(total))
        .flex_shrink_0()
        .border_1()
        .border_color(border)
        .rounded(px(6.))
        .overflow_hidden();

    grid = grid.child(render_table_row(
        &table.header,
        &widths,
        &table.aligns,
        true,
        border,
        row_h,
        p,
    ));
    // 表头下分隔线
    grid = grid.child(div().h(px(1.)).w_full().flex_shrink_0().bg(border));
    for (ri, row) in table.rows.iter().enumerate() {
        let mut cells = row.clone();
        cells.resize(table.header.len(), SharedString::from(""));
        let mut row_el = render_table_row(
            &cells,
            &widths,
            &table.aligns,
            false,
            border,
            row_h,
            p,
        );
        // 数据行之间加一条上横线（旧 render.rs 用 border_t_1）
        if ri > 0 {
            row_el = row_el.border_t_1().border_color(border);
        }
        grid = grid.child(row_el);
    }
    grid
}

/// 表格的一行：定宽单元格 + 列左竖线；表头加粗/着色，按对齐方式排版。
fn render_table_row(
    cells: &[SharedString],
    widths: &[f32],
    aligns: &[TableAlign],
    is_header: bool,
    border: Hsla,
    row_h: Pixels,
    p: &StyleParams,
) -> Div {
    let mut row_el = div().flex().flex_row().w_full().flex_shrink_0();
    if is_header {
        row_el = row_el.bg(p.table_head_bg);
    }
    for (ci, cell) in cells.iter().enumerate() {
        let mut cell_el = div()
            .w(px(widths.get(ci).copied().unwrap_or(48.0)))
            .h(row_h)
            .flex_shrink_0()
            .px(px(10.))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(p.para_size)
            .text_color(p.fg);
        // 借鉴旧 render.rs：c>0 的列在左边加分割线（网格线齐平）
        if ci > 0 {
            cell_el = cell_el.border_l_1().border_color(border);
        }
        match aligns.get(ci) {
            Some(TableAlign::Center) => cell_el = cell_el.text_center(),
            Some(TableAlign::Right) => cell_el = cell_el.text_right(),
            _ => {}
        }
        if is_header {
            cell_el = cell_el.font_weight(FontWeight::BOLD);
        }
        row_el = row_el.child(cell_el.child(cell.clone()));
    }
    row_el
}
