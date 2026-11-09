//! WYSIWYG 元素内部「坐标 / 几何」纯函数：渲染列↔源列、代码块/表格块的行组与
//! 单元格几何换算。全部为叶子纯函数（无 window/theme/editor 依赖），供 shape /
//! build_frame / hit / paint 共用——视觉前端（hit、paint）也经 `element::*` 重导出取用。
//!
//! 与 [`super::BlockLayout`] 的 Fenwick 前缀和、[`super::Frame`] 的 `deltas` 稀疏覆盖
//! 分开：这里只管「单块内部的坐标换算」，块间定位在 `Frame`/`BlockLayout`。

use std::ops::Range;

use gpui::{Pixels, Point, point, px};

use super::{IrKind, ShapedRow, ShapedTable, TableRowCellsLayout};

/// 源列 → 渲染列（左界：首个 `byte_map[v] >= target`；与
/// `markdown_wysiwyg_math::display_to_source` 互逆，光标才落回点击处——用 `>` 会整格右移一字符）。
pub fn source_to_display(byte_map: &[usize], target: usize) -> usize {
    byte_map.partition_point(|&b| b < target)
}

/// 单个可编辑块的渲染列 → 块内光标位置（`position_for_index` 对折行返回
/// `(子行内 x, 子行偏移·行高)`，谓以块首为原点）。
pub fn render_point_of(wrapped: &[gpui::WrappedLine], line_h: Pixels, dcol: usize) -> Point<Pixels> {
    if let Some(w0) = wrapped.first() {
        w0.position_for_index(dcol, line_h)
            .unwrap_or_else(|| point(px(0.), px(0.)))
    } else {
        point(px(0.), px(0.))
    }
}

/// 文本行正文因行内公式撑高而整体下移的量（px；未撑高 = 0）——paint 文本/公式、
/// 光标几何、点击命中、选区、链接 hitbox 必须共用，否则撑高行上光标/点击/选区
/// 会错位（用户实测：点击该行闪烁光标位置不对）。
pub fn text_row_shift(row: &ShapedRow) -> f32 {
    let Some(w0) = row.wrapped.first() else {
        return 0.0;
    };
    let band = row.sub_heights.first().copied().unwrap_or(row.lh);
    let orig = row.lh * (w0.wrap_boundaries().len() + 1) as f32;
    ((f32::from(band) - f32::from(orig)) / 2.).max(0.)
}

/// 可编辑文本在行内的**垂直起点**（相对 `row.top`，px）——图片行的题注（alt）在
/// 图片盒之下（`image.h` + 题注间隙）；普通文本行为 0（其上再叠 [`text_row_shift`]
/// 的行内公式撑高下移）。光标几何 / 点击命中 / 选区绘制 / 文本 paint 必须共用，
/// 否则图片题注上的光标/选区会错位。
pub fn text_y_off(row: &ShapedRow) -> f32 {
    if row.kind == IrKind::Image {
        row.image
            .as_ref()
            .map(|i| i.h + config::markdown::IMAGE_CAPTION_GAP_PX)
            .unwrap_or(0.)
    } else {
        0.
    }
}

/// 行首装饰（列表符号 / 有序号 / 任务框）应垂直对齐的「首可视行文字中心 y」
/// （相对块 `top` 的偏移，px；绝对 y = 本值 + 帧 origin.y）：= 首带起点
/// （[`text_y_off`]）+ 行内公式撑高下移（[`text_row_shift`]）+ 半行高。与
/// `paint_text_row` 画第一折行的几何**同一推导**（首带内 `wrapped[0]` 的文本中心）。
/// 整块 `height`（= 各折行带高之和）用于取块中点会把标记拉到第 N/2 折行行
/// （多行列表项 `1.` 跑到中间折行行的 bug 根源）——标记垂直定位**禁用** `height`。
pub fn first_band_center_y(row: &ShapedRow) -> f32 {
    f32::from(row.top) + text_y_off(row) + text_row_shift(row) + f32::from(row.lh) / 2.
}

/// 表格单元格源区间与选区的交集 → `(渲染内起始字节, 渲染内结束字节)`。
/// `map` = 单元格 `TableCellLayout.map`（渲染字节→源字节，相对该行行首，末项哨兵）；
/// `cell_src` 相对行首；`row_line_start` = 该源行的**绝对**行首；`sel` = 全局源选区
///（归一化）。无交集返回 `None`。纯函数（不依赖 window），`paint_selection` 与单测共用。
pub fn cell_selection_display(
    map: &[usize],
    cell_text_len: usize,
    cell_src: Range<usize>,
    row_line_start: usize,
    sel: &Range<usize>,
) -> Option<(usize, usize)> {
    let start = row_line_start + cell_src.start;
    let end = row_line_start + cell_src.end;
    let from = sel.start.max(start).min(end);
    let to = sel.end.max(start).min(end);
    if to <= from {
        return None;
    }
    let d0 = source_to_display(map, from - row_line_start).min(cell_text_len);
    let d1 = source_to_display(map, to - row_line_start).min(cell_text_len);
    let (d0, d1) = if d0 <= d1 { (d0, d1) } else { (d1, d0) };
    (d1 > d0).then_some((d0, d1))
}

/// 代码块内 dy（相对块首 y）→ 源码行组序号（`sub_heights` 累积；块首 1 行空白带
/// 归第 0 组——代码块 `row.lh` = code_line，与 paint 的首空白带同高）。
pub fn code_group_at(row: &ShapedRow, dy: f32) -> usize {
    if row.sub_heights.is_empty() {
        return 0;
    }
    let mut acc = f32::from(row.lh);
    for (k, sh) in row.sub_heights.iter().enumerate() {
        acc += f32::from(*sh);
        if dy < acc {
            return k;
        }
    }
    row.sub_heights.len() - 1
}

/// 文本块内 dy（相对块首，**未叠** `text_y_off`/撑高下移）→ 可视行带序号：
/// 逐 `sub_heights` 累积（每带 = 一个 `WrappedLine`）。`paint_text_row` 的逐带水平
/// 对齐移量（`align_offsets[k]`）与命中/光标映射共用同一带索引，点击/光标才与绘制
/// 同 x。空 `sub_heights`（带外块）返回 0。
pub fn text_band_at(row: &ShapedRow, dy: f32) -> usize {
    if row.sub_heights.is_empty() {
        return 0;
    }
    let mut acc = 0f32;
    for (k, sh) in row.sub_heights.iter().enumerate() {
        acc += f32::from(*sh);
        if dy < acc {
            return k;
        }
    }
    row.sub_heights.len() - 1
}

/// 块级渲染字节 → 所在源码行组序号（`multi_offsets` 起始偏移左界折半）。
pub fn group_from_rend(row: &ShapedRow, rend: usize) -> usize {
    if row.multi_offsets.is_empty() {
        return 0;
    }
    row.multi_offsets
        .partition_point(|&o| o <= rend)
        .saturating_sub(1)
        .min(row.multi_offsets.len() - 1)
}

/// 第 `g` 个源码行组的**渲染字节长**（组内列上限）。
pub fn group_len(row: &ShapedRow, g: usize) -> usize {
    let total = row.byte_map.len().saturating_sub(1);
    let start = row.multi_offsets.get(g).copied().unwrap_or(0);
    let next = row
        .multi_offsets
        .get(g + 1)
        .copied()
        .unwrap_or(total)
        .min(total);
    next.saturating_sub(start).max(1)
}

/// 代码块第 `g` 个源码行组的垂直偏移（相对块首，含 1 行首空白带）。
pub fn code_group_top(row: &ShapedRow, g: usize) -> f32 {
    let mut acc = f32::from(row.lh);
    for sh in row.sub_heights.iter().take(g) {
        acc += f32::from(*sh);
    }
    acc
}

/// 表格块内 dy（相对块首）→ 可见行号（0=表头，i=数据行 i）。布局与
/// [`paint_table_row`] 同式（上边框 1px、行间分隔 1px）。带外/空白间隙返回 None。
pub fn table_row_at(t: &ShapedTable, dy: f32) -> Option<usize> {
    let rh = f32::from(t.row_h);
    if dy < 1. {
        return None;
    }
    if dy < 1. + rh {
        return Some(0);
    }
    let mut y = 1. + rh;
    for i in 0..t.rows_texts.len() {
        y += 1.; // 行间分隔线
        if dy < y + rh {
            return Some(1 + i);
        }
        y += rh;
    }
    None
}

/// 可见行 `row_ix` 的块内顶部 y（与 [`table_row_at`] 互逆）。
pub fn table_row_top(t: &ShapedTable, row_ix: usize) -> f32 {
    if row_ix == 0 {
        return 1.;
    }
    1. + f32::from(t.row_h) + (row_ix - 1) as f32 * (1. + f32::from(t.row_h)) + 1.
}

/// 内容坐标 x → 列号（循环列宽；超出总宽返回 None）。
pub fn table_cell_index(widths: &[f32], x: f32) -> Option<usize> {
    let mut acc = 0f32;
    for (ci, w) in widths.iter().enumerate() {
        if x < acc + *w {
            return Some(ci);
        }
        acc += *w;
    }
    None
}

/// 列号 → 该列左缘坐标（内容坐标）。
pub fn table_cell_x0(widths: &[f32], cell_ix: usize) -> f32 {
    widths.iter().take(cell_ix).sum()
}

/// src caret（相对行首）→ 渲染字节（`map` 左界，<>`source_to_display`）。
pub fn table_cell_display(map: &[usize], target: usize) -> usize {
    map.partition_point(|&s| s < target)
}

/// src caret（相对行首）→ 表格单元格几何：(单元格列号, 列内渲染列)。找不到返回 None。
pub fn table_cell_at_src(
    rows: &[TableRowCellsLayout],
    row_line: usize,
    target: usize,
) -> Option<(usize, usize)> {
    let row = rows.iter().find(|r| r.src_line == row_line)?;
    let nonempty: Vec<usize> = row
        .cells
        .iter()
        .enumerate()
        .filter(|(_, c)| c.src.start < c.src.end)
        .map(|(i, _)| i)
        .collect();
    let cell_ix = row
        .cells
        .iter()
        .position(|c| {
            c.src.start < c.src.end && target >= c.src.start && target <= c.src.end
        })
        .unwrap_or_else(|| {
            // 落在单元格间隙（管道/空白区）：取最近的非空格尾；都无则取第一个非空格。
            nonempty
                .iter()
                .rev()
                .copied()
                .find(|&i| row.cells[i].src.start <= target)
                .or_else(|| nonempty.first().copied())
                .unwrap_or(0)
        });
    let dcol = table_cell_display(&row.cells[cell_ix].map, target);
    let dcol = dcol.min(row.cells[cell_ix].line.len());
    Some((cell_ix, dcol))
}
