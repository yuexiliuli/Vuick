//! WYSIWYG 交互/命中层（自宿主 `wysiwyg/element/hit.rs` 迁入引擎）：窗口坐标 ↔ 源字节
//! （光标几何、点击命中）、代码/表格/数学横向滚动条的几何/命中/跳转与拖拽换算。
//!
//! 迁移 seam：凡需宿主态的函数改持 `rd: &RefCell<WyRenderModel>` + `host: &dyn WyRenderHost`
//! 成对入参；纯几何函数（`image_box_local` 等）保留原签名随迁。行为逐字保持。

use std::cell::RefCell;
use std::ops::Range;

use gpui::{Bounds, Pixels, Point, point, px, size};

use crate::element::*;
use crate::element::geom::{code_group_at, code_group_top, group_from_rend, group_len, render_point_of, source_to_display, table_cell_at_src, table_cell_index, table_cell_x0, table_row_at, table_row_top, text_band_at, text_row_shift, text_y_off};
use crate::frame::reading_col_shift;
use crate::model::WyRenderModel;
use crate::{WyRenderHost, docutil};
use crate::paint::{MathCardDisplay, math_card_display};
use markdown::ir::IrKind;
use markdown::parse::recognition::LinkHit;
use markdown_wysiwyg_math::display_to_source;

/// 图片右上角编辑按钮的边长（px）。命中与 paint 共用，不得分开改。
pub const IMAGE_EDIT_BTN_SIZE: f32 = 24.;
/// 编辑按钮距图片右上角的边距（px）。
pub const IMAGE_EDIT_BTN_MARGIN: f32 = 6.;
/// 行内图编辑按钮边长（px）：行内缩略图高≈行高，块图 24px 按钮太大，用小号。
pub const INLINE_IMAGE_EDIT_BTN_SIZE: f32 = 16.;
/// 行内图编辑按钮距图片右上角的边距（px）。
pub const INLINE_IMAGE_EDIT_BTN_MARGIN: f32 = 2.;
/// 图片编辑卡片的宽（px；卡片水平居中于内容区）。
pub const IMAGE_EDIT_CARD_W: f32 = 320.;
/// 数学编辑卡片的宽（px；卡片水平居中于内容区，锚定覆盖在目标公式卡顶部）。
pub const MATH_EDIT_CARD_W: f32 = 520.;
/// 数学编辑卡片的估计高（px；标题栏 + 多行编辑器区 + 内边距，点外关闭的**保守**
/// 包围盒判定用）。
pub const MATH_EDIT_CARD_H: f32 = 280.;
/// 数学编辑卡片内**多行编辑器区**的固定高（px；CodeEditor 内嵌区域，≥160）。
pub const MATH_EDIT_EDITOR_H: f32 = 200.;
/// 数学编辑卡内嵌 CodeEditor 区的**内容宽**（px）＝ 卡片宽 − 卡片内衬 2×12 − 卡片描边
/// 2×1 − 编辑器区描边 2×1。
pub const MATH_EDIT_EDITOR_W: f32 = MATH_EDIT_CARD_W - 24. - 2. - 2.;
/// 数学编辑卡内嵌编辑器的**横向尾部 gap**（px）。
pub const MATH_EDIT_TAIL_GAP_PX: f32 = 64.;

/// 源 caret → 光标条几何。非文本行停在块首（只读）；代码块内停在对应内容行。
pub fn caret_geometry(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    f: &Frame,
) -> (bool, Pixels, Pixels) {
    let (cl, col) = docutil::row_col(host.doc(), host.caret());
    let line_start = docutil::line_start(host.doc(), cl);
    let line_end = docutil::line_end(host.doc(), cl);
    for r in &f.rows {
        if !r.src_lines.contains(&cl) {
            continue;
        }
        if !r.editable {
            if r.kind == IrKind::Blank {
                return (true, f.padding + r.x_off, r.top);
            }
            return (false, f.padding, r.top);
        }
        if let (Some(te), Some(t)) = (&r.table_edit, &r.table) {
            let Some(row_ix) = te.rows.iter().position(|rr| rr.src_line == cl) else {
                return (false, f.padding, r.top);
            };
            let row_l = &te.rows[row_ix];
            let target = col.min(line_end.saturating_sub(line_start));
            let Some((cell_ix, dcol)) = table_cell_at_src(&te.rows, cl, target) else {
                return (false, f.padding, r.top);
            };
            let cell = &row_l.cells[cell_ix];
            let cell_x0 = table_cell_x0(&t.widths, cell_ix);
            let col_x = f32::from(cell.line.x_for_index(dcol.min(cell.line.len())));
            let caret_h = f32::from(rd.borrow().theme.para_line());
            let row_top = r.top + px(table_row_top(t, row_ix));
            let ctop = row_top + px(((f32::from(t.row_h) - caret_h) / 2.).max(0.));
            let scroll = table_hscroll_clamped(rd, host, f, r);
            return (
                true,
                f.padding + px(cell_x0 + cell.text_x + col_x - scroll),
                ctop,
            );
        }
        if !r.multi_offsets.is_empty() {
            let n = r.multi_offsets.len();
            if cl > r.src_line && cl <= r.src_line + n {
                let block_start = docutil::line_start(host.doc(), r.src_line);
                let target = (line_start + col).saturating_sub(block_start);
                let rend = source_to_display(&r.byte_map, target);
                let g = group_from_rend(r, rend);
                let dcol = rend.saturating_sub(r.multi_offsets[g]).min(group_len(r, g));
                let p = r
                    .wrapped
                    .get(g)
                    .and_then(|wl| wl.position_for_index(dcol, r.lh))
                    .unwrap_or_else(|| point(px(0.), px(0.)));
                let y = r.top + px(code_group_top(r, g) + f32::from(p.y));
                let scroll = code_hscroll_clamped(rd, host, f, r);
                return (true, f.padding + r.x_off + p.x - px(scroll), y);
            }
            return (false, f.padding, r.top);
        }
        let target = col.min(line_end.saturating_sub(line_start));
        let dcol = source_to_display(&r.byte_map, target);
        let p = render_point_of(&r.wrapped, r.lh, dcol);
        let aln = r
            .align_offsets
            .get(text_band_at(r, f32::from(p.y)))
            .copied()
            .unwrap_or(px(0.));
        let caret_shift = ((f32::from(r.lh) - f32::from(f.line_h)) / 2.).max(0.);
        return (
            true,
            f.padding + r.x_off + aln + p.x,
            r.top + px(text_y_off(r) + text_row_shift(r)) + p.y + px(caret_shift),
        );
    }
    let ctop = px(f.src_line_top(cl));
    (true, f.padding, ctop)
}

/// 点命中（**元素局部坐标**）→ 源字节偏移（帧内 byte_map 反查）。
pub fn source_offset_at_local(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> usize {
    let rel_y = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if rel_y >= top && rel_y < bottom {
            if !r.editable {
                return docutil::line_end(host.doc(), r.src_line)
                    .max(docutil::line_start(host.doc(), r.src_line));
            }
            if let (Some(te), Some(t)) = (&r.table_edit, &r.table) {
                let Some(row_ix) = table_row_at(t, rel_y - top) else {
                    return host.caret();
                };
                let Some(row_l) = te.rows.get(row_ix) else {
                    return host.caret();
                };
                let scroll = table_hscroll_clamped(rd, host, frame, r);
                let rel_x = (f32::from(p.x) - f32::from(frame.padding) + scroll).max(0.);
                let cell_ix =
                    table_cell_index(&t.widths, rel_x).unwrap_or(t.widths.len().saturating_sub(1));
                let Some(cell) = row_l.cells.get(cell_ix) else {
                    return host.caret();
                };
                let cell_x0 = table_cell_x0(&t.widths, cell_ix);
                let in_cell = (rel_x - cell_x0 - cell.text_x).max(0.);
                let col = cell
                    .line
                    .closest_index_for_x(px(in_cell))
                    .min(cell.map.len().saturating_sub(1))
                    .min(cell.line.len());
                let local = *cell.map.get(col).unwrap_or(&0);
                let ls = docutil::line_start(host.doc(), row_l.src_line);
                let le = docutil::line_end(host.doc(), row_l.src_line).max(ls);
                return (ls + local).min(le);
            }
            if !r.multi_offsets.is_empty() {
                let g = code_group_at(r, rel_y - top);
                let Some(wl) = r.wrapped.get(g) else {
                    return host.caret();
                };
                let scroll = code_hscroll_clamped(rd, host, frame, r);
                let rel_x = (f32::from(p.x) - f32::from(frame.padding) - f32::from(r.x_off)
                    + scroll)
                    .max(0.);
                let rel = point(px(rel_x), px(0.));
                let dcol = match wl.closest_index_for_position(rel, r.lh) {
                    Ok(i) | Err(i) => i,
                };
                let rend = r.multi_offsets[g] + dcol.min(group_len(r, g));
                let local = display_to_source(&r.byte_map, rend);
                let ls = docutil::line_start(host.doc(), r.src_line);
                let le = docutil::line_end(host.doc(), r.src_line + r.multi_offsets.len()).max(ls);
                return (ls + local).min(le);
            }
            if let Some(w0) = r.wrapped.first() {
                let sub_y = (rel_y - top - text_y_off(r) - text_row_shift(r)).max(0.);
                let aln = r
                    .align_offsets
                    .get(text_band_at(r, sub_y))
                    .copied()
                    .unwrap_or(px(0.));
                let rel_x = (f32::from(p.x) - f32::from(frame.padding) - f32::from(r.x_off)
                    - f32::from(aln))
                    .max(0.);
                let rel = point(px(rel_x), px(sub_y));
                let dcol = match w0.closest_index_for_position(rel, r.lh) {
                    Ok(i) | Err(i) => i,
                };
                let local = display_to_source(&r.byte_map, dcol);
                let ls = docutil::line_start(host.doc(), r.src_line);
                let le = docutil::line_end(host.doc(), r.src_line);
                return (ls + local).min(le);
            }
            let ls = docutil::line_start(host.doc(), r.src_line);
            let le = docutil::line_end(host.doc(), r.src_line).max(ls);
            return ls.min(le);
        }
    }
    host.caret()
}

/// 点命中（**窗口坐标**，`MouseDownEvent.position`）→ 源字节偏移。
pub fn source_offset_at(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> usize {
    let local = to_local(rd, host, p);
    source_offset_at_local(rd, host, frame, local)
}

/// 元素局部坐标（把窗口坐标减掉元素原点 + 居中阅读栏水平移位）。
pub fn to_local(rd: &RefCell<WyRenderModel>, host: &dyn WyRenderHost, p: Point<Pixels>) -> Point<Pixels> {
    let (origin, elem_w) = host
        .last_bounds()
        .map(|b| (b.origin, f32::from(b.size.width)))
        .unwrap_or_else(|| (point(px(0.), px(0.)), 0.));
    let shift = reading_col_shift(rd, elem_w);
    point(p.x - origin.x - px(shift), p.y - origin.y)
}

/// 独立图片行的**图片盒**元素局部 bounds（图片本身，不含下方题注行）。
pub fn image_box_local(f: &Frame, row: &ShapedRow) -> Option<Bounds<Pixels>> {
    let img = row.image.as_ref()?;
    let content_w = f32::from(f.width) - f32::from(f.padding) * 2.;
    let x0 = f32::from(f.padding) + ((content_w - img.w) / 2.).max(0.);
    let top0 = f32::from(row.top);
    Some(Bounds::new(
        point(px(x0), px(top0)),
        size(px(img.w), px(img.h)),
    ))
}

/// 图片编辑按钮的元素局部矩形：钉在图片盒**右上角**（右缘内收、顶缘下移）。
pub fn image_edit_button_local_with(
    box_b: &Bounds<Pixels>,
    btn_size: f32,
    margin: f32,
) -> Bounds<Pixels> {
    let x = box_b.right() - px(btn_size + margin);
    let y = box_b.top() + px(margin);
    Bounds::new(point(x, y), size(px(btn_size), px(btn_size)))
}
pub fn image_edit_button_local(box_b: &Bounds<Pixels>) -> Bounds<Pixels> {
    image_edit_button_local_with(box_b, IMAGE_EDIT_BTN_SIZE, IMAGE_EDIT_BTN_MARGIN)
}
pub fn inline_image_edit_button_local(box_b: &Bounds<Pixels>) -> Bounds<Pixels> {
    image_edit_button_local_with(
        box_b,
        INLINE_IMAGE_EDIT_BTN_SIZE,
        INLINE_IMAGE_EDIT_BTN_MARGIN,
    )
}

/// 图片**右中边缘**的调整条（resize handle）几何：竖条贴在图片右内缘、垂直居中。
pub const IMAGE_RESIZE_HANDLE_W: f32 = 6.;
pub const IMAGE_RESIZE_HANDLE_H: f32 = 48.;
pub const IMAGE_RESIZE_HANDLE_MARGIN_R: f32 = 4.;

/// 图片调整条的元素局部矩形（右中边缘；盒高太矮时条高内缩）。
pub fn image_resize_handle_local(box_b: &Bounds<Pixels>) -> Bounds<Pixels> {
    let box_h = f32::from(box_b.size.height);
    let h = IMAGE_RESIZE_HANDLE_H.min(box_h - 10.).max(12.);
    let x = box_b.right() - px(IMAGE_RESIZE_HANDLE_W + IMAGE_RESIZE_HANDLE_MARGIN_R);
    let y = box_b.top() + (box_b.size.height - px(h)) / 2.;
    Bounds::new(point(x, y), size(px(IMAGE_RESIZE_HANDLE_W), px(h)))
}

/// 命中图片**调整条**（窗口坐标）→ `(src_line, 指针窗口 x)`；未命中 `None`。
pub fn image_resize_handle_hit_at(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, f32)> {
    let grab_x = f32::from(p.x);
    let p = to_local(rd, host, p);
    for r in &frame.rows {
        if r.kind != IrKind::Image {
            continue;
        }
        let Some(bb) = image_box_local(frame, r) else {
            continue;
        };
        let hb = image_resize_handle_local(&bb);
        if hb.contains(&p) {
            return Some((r.src_line, grab_x));
        }
    }
    None
}

/// 图片调整条的**窗口**矩形（paint 加 `origin` 后对齐）。
pub fn image_resize_handle_window(
    origin: Point<Pixels>,
    bb: &Bounds<Pixels>,
) -> Bounds<Pixels> {
    let local = image_resize_handle_local(bb);
    Bounds::new(
        point(origin.x + local.left(), origin.y + local.top()),
        local.size,
    )
}

/// 命中图片盒（窗口坐标）→ 其 `src_line`；未命中返回 `None`（悬浮按钮亮显判定）。
pub fn image_box_hit_at(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<usize> {
    let p = to_local(rd, host, p);
    for r in &frame.rows {
        if r.kind != IrKind::Image {
            continue;
        }
        let Some(b) = image_box_local(frame, r) else {
            continue;
        };
        if b.contains(&p) {
            return Some(r.src_line);
        }
    }
    None
}

/// 命中图片**编辑按钮**（窗口坐标）→ 其 `src_line`；未命中返回 `None`。
pub fn image_edit_button_hit_at(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<usize> {
    let p = to_local(rd, host, p);
    for r in &frame.rows {
        if r.kind != IrKind::Image {
            continue;
        }
        let Some(b) = image_box_local(frame, r).map(|b| image_edit_button_local(&b)) else {
            continue;
        };
        if b.contains(&p) {
            return Some(r.src_line);
        }
    }
    None
}

/// 行内 `![alt](src)` 的显示盒（**元素局部**坐标）。
pub fn inline_image_box_local(
    f: &Frame,
    row: &ShapedRow,
    ii: &InlineImageHit,
) -> Option<Bounds<Pixels>> {
    let w0 = row.wrapped.first()?;
    let p0 = w0.position_for_index(ii.display_off, row.lh)?;
    let p1 = w0.position_for_index(ii.display_off + ii.len.max(1), row.lh)?;
    let x = f.padding + row.x_off + p0.x;
    let y = row.top + p0.y;
    let show_w = (p1.x - p0.x).max(px(6.));
    Some(Bounds::new(point(x, y), size(show_w, row.lh)))
}

/// 由行内图命中反解 `InlineImageRef`：`display_off` → 经 `byte_map` 定位 `[` 的源字节，
/// 再退两位得 `!`（否则该显示偏移不是行内图起点，返回 `None`）。
pub fn inline_image_ref_of(
    row: &ShapedRow,
    ii: &InlineImageHit,
    raw: &str,
) -> Option<crate::model::InlineImageRef> {
    let post = row.byte_map.as_slice();
    let alt_src = *post.get(ii.display_off.min(post.len().saturating_sub(1)))?;
    if alt_src < 2 || alt_src > raw.len() {
        return None;
    }
    let start = alt_src - 2;
    if raw.as_bytes().get(start) != Some(&b'!') || raw.as_bytes().get(start + 1) != Some(&b'[') {
        return None;
    }
    Some(crate::model::InlineImageRef {
        src_line: row.src_line,
        display_off: ii.display_off,
        start,
    })
}

/// 命中行内图片**本体**（显示盒，窗口坐标）→ `InlineImageRef`；未命中 `None`。
pub fn inline_image_hit_at(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<crate::model::InlineImageRef> {
    let p = to_local(rd, host, p);
    for r in &frame.rows {
        if r.extra.inline_images.is_empty() {
            continue;
        }
        for ii in &r.extra.inline_images {
            let Some(b) = inline_image_box_local(frame, r, ii) else {
                continue;
            };
            if !b.contains(&p) {
                continue;
            }
            let raw = docutil::line_text(host.doc(), r.src_line);
            if let Some(reff) = inline_image_ref_of(r, ii, &raw) {
                return Some(reff);
            }
        }
    }
    None
}

/// 命中行内图片**编辑按钮**（窗口坐标）→ `InlineImageRef`；未命中 `None`。
pub fn inline_image_edit_button_hit_at(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<crate::model::InlineImageRef> {
    let p = to_local(rd, host, p);
    for r in &frame.rows {
        if r.extra.inline_images.is_empty() {
            continue;
        }
        for ii in &r.extra.inline_images {
            let Some(bb) = inline_image_box_local(frame, r, ii) else {
                continue;
            };
            let b = inline_image_edit_button_local(&bb);
            if !b.contains(&p) {
                continue;
            }
            let raw = docutil::line_text(host.doc(), r.src_line);
            if let Some(reff) = inline_image_ref_of(r, ii, &raw) {
                return Some(reff);
            }
        }
    }
    None
}

/// 命中任务框（C1）：返回 (源行, 是否已勾选)。击中「任务行 gutter（☐/☑ 字形区）」
/// 才返回，否则 None。
pub fn checkbox_hit(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool)> {
    let p = to_local(rd, host, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) {
            continue;
        }
        let Some(RowGutter::Check { checked }) = &r.extra.gutter else {
            continue;
        };
        let hi = (f32::from(frame.padding) + 24.).max(f32::from(r.x_off));
        if px >= f32::from(frame.padding) && px <= hi {
            return Some((r.src_line, *checked));
        }
    }
    None
}

/// 命中警报折叠箭头（C3）：击中警报行 gutter（[padding, padding+ALERT_GUTTER_W] × 行高）
/// 返回 (源行, 当前是否折叠)。
pub fn alert_chevron_hit(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool)> {
    let p = to_local(rd, host, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) {
            continue;
        }
        let Some(RowGutter::Alert { kind: _, fold }) = &r.extra.gutter else {
            continue;
        };
        let Some(folded) = fold else {
            continue;
        };
        if px >= f32::from(frame.padding) && px <= f32::from(frame.padding) + ALERT_GUTTER_W {
            return Some((r.src_line, *folded));
        }
    }
    None
}

/// 命中标题折叠箭头：击中标题行的 chevron 区（文本右缘 + 固定宽的命中盒）。
pub fn heading_chevron_hit(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool)> {
    let p = to_local(rd, host, p);
    let py = f32::from(p.y);
    let px = f32::from(p.x);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom && matches!(r.kind, IrKind::Heading(_))) {
            continue;
        }
        let Some((cx0, cw)) = heading_chevron_box(frame, r) else {
            continue;
        };
        if px >= cx0 && px <= cx0 + cw {
            let key = docutil::line_text(host.doc(), r.src_line).trim().to_string();
            let folded = rd.borrow().folded_headings.borrow().contains(&key);
            return Some((r.src_line, folded));
        }
    }
    None
}

/// 标题折叠箭头几何：画在**左缘 gutter**（[padding, padding+HEADING_GUTTER_W]）。
pub fn heading_chevron_box(f: &Frame, _row: &ShapedRow) -> Option<(f32, f32)> {
    Some((f32::from(f.padding), HEADING_GUTTER_W))
}

/// 命中行内 `$…$` 公式的 spacer：击中其显示区 → 返回该公式的**绝对源字节区间**
/// （含定界符 `$…$`，`InlineMathText.source`）。
pub fn inline_math_hit_at(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<Range<usize>> {
    let p = to_local(rd, host, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) || r.extra.inline_maths.is_empty() {
            continue;
        }
        let Some(w0) = r.wrapped.first() else {
            continue;
        };
        let ox = f32::from(frame.padding) + f32::from(r.x_off);
        for im in &r.extra.inline_maths {
            let (Some(p0), Some(p1)) = (
                w0.position_for_index(im.display_off, r.lh),
                w0.position_for_index(im.display_off + im.len, r.lh),
            ) else {
                continue;
            };
            let y0 = top + text_row_shift(r) + f32::from(p0.y);
            let x0 = ox + f32::from(p0.x);
            let x1 = ox + f32::from(p1.x);
            if py >= y0
                && py < y0 + f32::from(r.lh)
                && px >= x0
                && px <= x1.max(x0 + 1.)
                && !im.source.is_empty()
            {
                return Some(im.source.clone());
            }
        }
    }
    None
}

/// 命中数学块卡片（A1，`$$…$$` 折叠成的 `BlockCard::Math` 行）→ 该块首源码行行首字节。
pub fn math_card_hit(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<usize> {
    let p = to_local(rd, host, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) {
            continue;
        }
        if !matches!(&r.extra.card, Some(BlockCard::Math { .. })) {
            continue;
        }
        let x0 = f32::from(frame.padding);
        let x1 = f32::from(frame.width) - f32::from(frame.padding);
        if px >= x0 && px <= x1 {
            return Some(docutil::line_start(host.doc(), r.src_line));
        }
    }
    None
}

/// Mermaid 卡点击命中（**聚焦**用）：窗口坐标 → 该卡 `src_line`。
pub fn mermaid_card_hit(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<usize> {
    let p = to_local(rd, host, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) {
            continue;
        }
        if !matches!(&r.extra.card, Some(BlockCard::Mermaid)) {
            continue;
        }
        let x0 = f32::from(frame.padding);
        let x1 = f32::from(frame.width) - f32::from(frame.padding);
        if px >= x0 && px <= x1 {
            return Some(r.src_line);
        }
    }
    None
}

/// 命中的可点击链接目标（C5）：窗口坐标 → 行内链接显示区间集合，命中的返回 `LinkHit`。
pub fn link_hit_at(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<LinkHit> {
    let p = to_local(rd, host, p);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) || r.extra.links.is_empty() || r.wrapped.is_empty() {
            continue;
        }
        let Some(w0) = r.wrapped.first() else {
            continue;
        };
        let ox = f32::from(frame.padding) + f32::from(r.x_off);
        for lk in &r.extra.links {
            let (Some(p0), Some(p1)) = (
                w0.position_for_index(lk.disp.start, r.lh),
                w0.position_for_index(lk.disp.end, r.lh),
            ) else {
                continue;
            };
            let p0y = top + text_row_shift(r) + f32::from(p0.y);
            let p1y = top + text_row_shift(r) + f32::from(p1.y);
            let contains = if f32::from(p0.y) == f32::from(p1.y) {
                let x0 = ox + f32::from(p0.x);
                let x1 = ox + f32::from(p1.x);
                py >= p0y
                    && py < p0y + f32::from(r.lh)
                    && f32::from(p.x) >= x0
                    && f32::from(p.x) <= x1
            } else {
                let head_ok = py >= p0y
                    && py < p0y + f32::from(r.lh)
                    && f32::from(p.x) >= ox + f32::from(p0.x)
                    && f32::from(p.x) <= ox + f32::from(w0.width());
                let tail_ok = py >= p1y
                    && py < p1y + f32::from(r.lh)
                    && f32::from(p.x) >= ox
                    && f32::from(p.x) <= ox + f32::from(p1.x);
                head_ok || tail_ok
            };
            if contains {
                return Some(lk.target.clone());
            }
        }
    }
    None
}

/// 命中的脚注标记（P4-2 注记迁入 WYSIWYG）：来源行 + 注记号 + 是否定义 + 标记矩形。
#[derive(Clone, Copy, Debug)]
pub struct NoteMarkerHit {
    pub src_line: usize,
    pub n: usize,
    pub is_def: bool,
    pub bounds: Bounds<Pixels>,
}

/// 某行脚注标记的**内容列局部**矩形。折行标记头尾各自成片。
fn note_marker_local_rects(f: &Frame, row: &ShapedRow) -> Vec<(NoteMarker, Bounds<Pixels>)> {
    let mut out = Vec::new();
    if row.extra.note_markers.is_empty() || row.wrapped.is_empty() {
        return out;
    }
    let Some(w0) = row.wrapped.first() else {
        return out;
    };
    let ox = f32::from(f.padding) + f32::from(row.x_off);
    for mk in &row.extra.note_markers {
        let end = mk.disp.end.min(w0.len());
        let start = mk.disp.start.min(end);
        let (Some(p0), Some(p1)) = (
            w0.position_for_index(start, row.lh),
            w0.position_for_index(end, row.lh),
        ) else {
            continue;
        };
        let b0 = (f32::from(p0.y) / f32::from(row.lh)).round() as usize;
        let b1 = (f32::from(p1.y) / f32::from(row.lh)).round() as usize;
        let a0 = row.align_offsets.get(b0).copied().unwrap_or(px(0.));
        let a1 = row.align_offsets.get(b1).copied().unwrap_or(px(0.));
        let oy = f32::from(row.top) + text_row_shift(row);
        if b0 == b1 {
            out.push((
                mk.clone(),
                Bounds::new(
                    point(
                        px(ox + f32::from(a0) + f32::from(p0.x)),
                        px(oy + f32::from(p0.y)),
                    ),
                    size(px((f32::from(p1.x) - f32::from(p0.x)).max(2.)), row.lh),
                ),
            ));
        } else {
            let head = Bounds::new(
                point(px(ox + f32::from(a0) + f32::from(p0.x)), px(oy + f32::from(p0.y))),
                size(px((f32::from(w0.width()) - f32::from(p0.x)).max(2.)), row.lh),
            );
            let tail = Bounds::new(
                point(px(ox + f32::from(a1)), px(oy + f32::from(p1.y))),
                size(px(f32::from(p1.x).max(2.)), row.lh),
            );
            out.push((mk.clone(), head));
            out.push((mk.clone(), tail));
        }
    }
    out
}

/// 命中脚注标记（**窗口坐标**）。
pub fn note_marker_hit_at(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<NoteMarkerHit> {
    let local = to_local(rd, host, p);
    for row in &frame.rows {
        for (mk, b) in note_marker_local_rects(frame, row) {
            if b.contains(&local) {
                return Some(NoteMarkerHit {
                    src_line: row.src_line,
                    n: mk.n,
                    is_def: mk.is_def,
                    bounds: b,
                });
            }
        }
    }
    None
}

/// 高亮目标行（`src_line`）内注记号 `n` 的**绝对源字节区间**（paint 画跳转加亮用）。
pub fn note_marker_src_range(
    _rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    src_line: usize,
    n: usize,
    is_def: bool,
) -> Option<Range<usize>> {
    let row = frame
        .rows
        .iter()
        .find(|r| r.src_line == src_line && !r.extra.note_markers.is_empty())?;
    let mk = row
        .extra
        .note_markers
        .iter()
        .find(|m| m.n == n && m.is_def == is_def)?;
    let map = row.byte_map.as_slice();
    let end = mk.disp.end.min(map.len().saturating_sub(1));
    let start = mk.disp.start.min(end);
    let ls = docutil::line_start(host.doc(), src_line);
    let s0 = ls + map.get(start).copied().unwrap_or(0);
    let s1 = ls + map.get(end).copied().unwrap_or(s0);
    (s1 > s0).then_some(s0..s1)
}

/// 代码块横向滚动条几何（内容超宽才返回）。
pub fn code_hscroll_geo(
    rd: &RefCell<WyRenderModel>,
    _host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> Option<(f32, f32, f32, f32, f32)> {
    let panel_w = (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.);
    let inset = f32::from(row.x_off).max(1.);
    let viewport = (panel_w - inset * 2.).max(1.);
    let max_w = row
        .wrapped
        .iter()
        .map(|wl| f32::from(wl.width()))
        .fold(0.0f32, f32::max);
    if max_w <= viewport + 1. {
        return None;
    }
    let max_scroll = max_w - viewport;
    let scroll = rd
        .borrow()
        .code_h_scroll
        .borrow()
        .get(&row.src_line)
        .copied()
        .unwrap_or(0.)
        .min(max_scroll)
        .max(0.);
    let frac = if max_scroll > 0. {
        scroll / max_scroll
    } else {
        0.
    };
    let bar_x = f32::from(f.padding) + inset;
    let bar_w = viewport.max(1.);
    let thumb_w = (bar_w * viewport / max_w)
        .max(config::layout::CODE_HSCROLL_THUMB_MIN)
        .min(bar_w);
    let thumb_x = bar_x + (bar_w - thumb_w) * frac;
    Some((bar_x, bar_w, thumb_x, thumb_w, max_scroll))
}

/// 代码块当前生效的横向滚动偏移（px）：
pub fn code_hscroll_clamped(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> f32 {
    match code_hscroll_geo(rd, host, f, row) {
        Some((_, _, _, _, max_scroll)) => rd
            .borrow()
            .code_h_scroll
            .borrow()
            .get(&row.src_line)
            .copied()
            .unwrap_or(0.)
            .min(max_scroll)
            .max(0.),
        None => 0.,
    }
}

/// 代码块横向滚动条命中（元素局部→窗口坐标）。
pub fn code_hscrollbar_hit(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool, f32)> {
    let p = to_local(rd, host, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) || r.multi_offsets.is_empty() || r.wrapped.is_empty() {
            continue;
        }
        let Some((bar_x, bar_w, thumb_x, thumb_w, _)) = code_hscroll_geo(rd, host, frame, r) else {
            continue;
        };
        let band_top =
            f32::from(r.top) + f32::from(r.height) - config::layout::CODE_HSCROLL_BAND;
        if py < band_top {
            continue;
        }
        if px >= bar_x && px <= bar_x + bar_w {
            let on_thumb = px >= thumb_x && px <= thumb_x + thumb_w;
            let grab = px - thumb_x;
            return Some((r.src_line, on_thumb, grab));
        }
    }
    None
}

/// 表格横向滚动条几何（内容超宽才返回）。
pub fn table_hscroll_geo(
    rd: &RefCell<WyRenderModel>,
    _host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> Option<(f32, f32, f32, f32, f32)> {
    let t = row.table.as_ref()?;
    let viewport = (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.);
    let max_w = t.total_w.max(1.);
    let max_scroll = (max_w - viewport).max(0.);
    let scroll = rd
        .borrow()
        .table_h_scroll
        .borrow()
        .get(&row.src_line)
        .copied()
        .unwrap_or(0.)
        .min(max_scroll)
        .max(0.);
    let frac = if max_scroll > 0. {
        scroll / max_scroll
    } else {
        0.
    };
    let bar_x = f32::from(f.padding);
    let bar_w = viewport.max(1.);
    let thumb_w = (bar_w * viewport / max_w)
        .max(config::layout::CODE_HSCROLL_THUMB_MIN)
        .min(bar_w);
    let thumb_x = bar_x + (bar_w - thumb_w) * frac;
    Some((bar_x, bar_w, thumb_x, thumb_w, max_scroll))
}

/// 表格当前生效的横向滚动偏移（px）。
pub fn table_hscroll_clamped(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> f32 {
    match table_hscroll_geo(rd, host, f, row) {
        Some((_, _, _, _, max_scroll)) => rd
            .borrow()
            .table_h_scroll
            .borrow()
            .get(&row.src_line)
            .copied()
            .unwrap_or(0.)
            .min(max_scroll)
            .max(0.),
        None => 0.,
    }
}

/// 表格横向滚动条命中（元素局部→窗口坐标）。
pub fn table_hscrollbar_hit(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool, f32)> {
    let p = to_local(rd, host, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) || r.table.is_none() {
            continue;
        }
        let Some((bar_x, bar_w, thumb_x, thumb_w, _)) = table_hscroll_geo(rd, host, frame, r) else {
            continue;
        };
        let band_top =
            f32::from(r.top) + f32::from(r.height) - config::layout::CODE_HSCROLL_BAND;
        if py < band_top {
            continue;
        }
        if px >= bar_x && px <= bar_x + bar_w {
            let on_thumb = px >= thumb_x && px <= thumb_x + thumb_w;
            let grab = px - thumb_x;
            return Some((r.src_line, on_thumb, grab));
        }
    }
    None
}

/// 数学卡内容**自然宽**（px）：位图就绪 → 排版位图逻辑宽（自然宽、**不缩小**）；
/// 排版中（Loading/缺位）且有钉住的旧位图 → 旧位图逻辑宽；都没有 → paint 侧测好的
/// Unicode 行最大宽（`math_natural_w` 侧缓存）。
pub fn math_natural_w(rd: &RefCell<WyRenderModel>, host: &dyn WyRenderHost, row: &ShapedRow) -> f32 {
    let Some(BlockCard::Math { latex, .. }) = &row.extra.card else {
        return 0.;
    };
    match math_card_display(rd, host, row.src_line, latex.trim()) {
        MathCardDisplay::Ready(_, w, _) | MathCardDisplay::Pinned(_, w, _) => w.max(1.),
        MathCardDisplay::Fallback => rd
            .borrow()
            .math_natural_w
            .borrow()
            .get(&row.src_line)
            .copied()
            .unwrap_or(0.)
            .max(0.),
    }
}

/// 数学卡横向滚动条几何（公式自然宽 > 内容盒才返回）。
pub fn math_hscroll_geo(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> Option<(f32, f32, f32, f32, f32)> {
    if !matches!(&row.extra.card, Some(BlockCard::Math { .. })) {
        return None;
    }
    let viewport = (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.);
    let natural_w = math_natural_w(rd, host, row);
    if natural_w <= viewport + 1. {
        return None;
    }
    let max_scroll = natural_w - viewport;
    let scroll = rd
        .borrow()
        .math_h_scroll
        .borrow()
        .get(&row.src_line)
        .copied()
        .unwrap_or(0.)
        .min(max_scroll)
        .max(0.);
    let frac = if max_scroll > 0. {
        scroll / max_scroll
    } else {
        0.
    };
    let bar_x = f32::from(f.padding);
    let bar_w = viewport.max(1.);
    let thumb_w = (bar_w * viewport / natural_w)
        .max(config::layout::CODE_HSCROLL_THUMB_MIN)
        .min(bar_w);
    let thumb_x = bar_x + (bar_w - thumb_w) * frac;
    Some((bar_x, bar_w, thumb_x, thumb_w, max_scroll))
}

/// 数学卡当前生效的横向滚动偏移（px）。
pub fn math_hscroll_clamped(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> f32 {
    match math_hscroll_geo(rd, host, f, row) {
        Some((_, _, _, _, max_scroll)) => rd
            .borrow()
            .math_h_scroll
            .borrow()
            .get(&row.src_line)
            .copied()
            .unwrap_or(0.)
            .min(max_scroll)
            .max(0.),
        None => 0.,
    }
}

/// 数学卡横向滚动条命中（元素局部→窗口坐标）。
pub fn math_hscrollbar_hit(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool, f32)> {
    let p = to_local(rd, host, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom && matches!(&r.extra.card, Some(BlockCard::Math { .. }))) {
            continue;
        }
        let Some((bar_x, bar_w, thumb_x, thumb_w, _)) = math_hscroll_geo(rd, host, frame, r) else {
            continue;
        };
        let band_top =
            f32::from(r.top) + f32::from(r.height) - config::layout::CODE_HSCROLL_BAND;
        if py < band_top {
            continue;
        }
        if px >= bar_x && px <= bar_x + bar_w {
            let on_thumb = px >= thumb_x && px <= thumb_x + thumb_w;
            let grab = px - thumb_x;
            return Some((r.src_line, on_thumb, grab));
        }
    }
    None
}
