//! 渲染一张 GFM 表格为带边框网格（借鉴旧 render.rs 的表格视觉）：
//! 外边 1px 圆角边框 + 表头浅灰底/加粗 + 表头下分隔线 + 列左竖线 + 数据行上横线。
//!
//! M5-B5 P3-1c-2：表格行内容切共享 shard——本模块不再自行估算列宽，而是直接消费
//! [`layout::ShardTable`]（`shape_table_shard` 产出的**单源几何**：列宽分配/单元格
//! 高/表头数据/对齐，与 WYSIWYG `shape_table_row` 同式）。渲染与量程共用同一几何
//! （R2），`render_row_shard` 的 Table 分支据此画网格（旧 `render_table_row`/
//! `table_column_widths` 估算路径已删）。

use std::rc::Rc;

use gpui::{div, px, Div, FontWeight, Hsla, InteractiveElement, ParentElement, Pixels, SharedString, Styled};

use crate::markdown::layout::ShardTable;
use crate::markdown::parse::TableAlign;
use crate::markdown::view::style::StyleParams;

/// 按 [`ShardTable`] 几何渲染一张 GFM 表格为带边框网格。
pub(crate) fn render_table_geometry(geo: &ShardTable, p: &Rc<StyleParams>) -> gpui::Stateful<gpui::Div> {
    let border = p.divider;
    let widths = &geo.widths;
    let total: f32 = geo.total_w;
    let row_h = px(geo.row_h);

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

    grid = grid.child(render_row(
        &geo.header,
        widths,
        &geo.aligns,
        true,
        border,
        row_h,
        p,
    ));
    // 表头下分隔线
    grid = grid.child(div().h(px(1.)).w_full().flex_shrink_0().bg(border));
    for (ri, cells) in geo.rows.iter().enumerate() {
        let mut row_el = render_row(cells, widths, &geo.aligns, false, border, row_h, p);
        // 数据行之间加一条上横线（旧 render.rs 用 border_t_1）
        if ri > 0 {
            row_el = row_el.border_t_1().border_color(border);
        }
        grid = grid.child(row_el);
    }
    grid
}

/// 表格的一行：定宽单元格 + 列左竖线；表头加粗/着色，按对齐方式排版。
fn render_row(
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
