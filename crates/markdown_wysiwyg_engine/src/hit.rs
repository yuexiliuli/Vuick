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
use crate::model::{HScrollKind, WyRenderModel};
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
/// 图片编辑卡片的宽（px；卡片锚在【编辑图片】按钮左缘，
/// 仅带外/未物化行的回退路径才水平居中于内容区）。
pub const IMAGE_EDIT_CARD_W: f32 = 320.;
/// 图片编辑卡片相对【编辑图片】按钮的**间距**（px）：卡片 TopLeft 锚在按钮左缘，
/// **下方优先**（y = 按钮底缘 + 该 gap），放不下翻到上方（y = 按钮顶 − 该 gap −
/// `IMAGE_EDIT_CARD_H`）。下方/上方纵向二选一在 `image_edit_anchor_now` 内完成——
/// 把「不盖按钮 / 不越编辑可视区下缘」算进去，不依赖 `anchor_layer` 的 snap 8px；
/// `snap_to_window_with_margin` 仅保留作横向与极端情况的最后兜底。
pub const IMAGE_EDIT_CARD_GAP: f32 = 6.;
/// 图片编辑卡片的**估计高**（px；`image_edit_anchor_now` 判定「下方放不放得下」用）。
/// 保守**偏大**（实际 ≈ 194 → 取 220，含 26 缓冲）：偏大 → 判定偏早翻上方、安全优先
/// （宁可少用一次下方，也不盖按钮 / 越编辑可视区下缘）。真机观感若发现「下方明明有
/// 空间却已翻上」，可把缓冲收小，但不得小于实际组合高。
///
/// 由 `WyEditor::image_edit_card_element`（`markdown_wysiwyg/src/wysiwyg/image.rs`）的
/// 实际布局逐项求和推导：
/// - 根内边距 `p(12px)` 上下各 1 → 12×2 = 24；
/// - 根纵向 gap `gap_y(8px)` × 3（标题 / 链接组 / 文本组 / 按钮行 4 子项） → 24；
/// - 标题「编辑图片」`text_size(13px)`：一行行高按 gpui 文本默认 ≈1.5em → 20；
/// - 链接组：标签「链接」`text_size(12px)` 行高 ≈18 + 组内 gap `gap_y(4px)` +
///   输入框 `h(28px)` → 50；文本组同构 → 50；
/// - 按钮行：取消 / 保存按钮 `h(26px)` → 26。
///
/// 合计 ≈ 24 + 24 + 20 + 50 + 50 + 26 = 194。
pub const IMAGE_EDIT_CARD_H: f32 = 220.;
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

/// 标题字形的行带内占比估算（px→em 系数；仅防御性 fallback——空标题 `# ` 实际经
/// `shape_text` 产出一条空 WrappedLine，走 `wrapped[0]` 真实度量，到不了本分支）。
/// 取 1.0 em（≈CJK 字形主体）：本机默认字体实测 ascent+descent≈0.86 em，
/// Latin 系 ≈1.1–1.3 em——按 1.0 落在区间中段、不显著过度，且夹到行带内；观感余量
/// 属真机可调项。
const TITLE_GLYPH_H_EM: f32 = 1.0;

/// 标题行光标竖条应覆盖的**文字字形高**（px）：
/// - 有字标题行（有 shaped text）：用该行 `wrapped[0]` 的真实 `ascent+descent`——这是
///   gpui paint 定位基线的**同一源数据**（`vendor/zed/crates/gpui/src/text_system/line.rs`
///   `paint_line`：`padding_top=(line_height−ascent−descent)/2` → 基线落在
///   `padding_top+ascent`；竖条顶=字形顶、下沿=基线+descent，正好贴文字字形）。夹到
///   `row.lh` 内：极端字体（ascent+descent>lh，如 h1 的 lh=1.3×fs 最紧）下竖条退化
///   满行带、不越带——与下方估算分支同一夹取口径，锁住「竖条底不越行带」不变式。
/// - 空标题行（`# `）：`shape_text` 对空串仍产出一条**空 `WrappedLine`**（
///   `text_system.rs` 每物理行无条件 push），故实际走上一分支取空行 layout 的真实
///   字形度量（同字号同字体 → 与有字标题观感一致）。本 fallback 仅防御「wrapped 为
///   空」的标题行（当前所有标题行都经 `shape_text`，到不了）：按 `r.lh`（== 该级
///   `heading_line`）反查标题级 → 该级 `heading_fs` × 系数估算，夹到行带内。
fn title_glyph_height(row: &ShapedRow, rd: &RefCell<WyRenderModel>) -> Pixels {
    if let Some(w0) = row.wrapped.first() {
        let l = &w0.unwrapped_layout;
        return px((f32::from(l.ascent) + f32::from(l.descent)).min(f32::from(row.lh)));
    }
    let theme = &rd.borrow().theme;
    let fs = (1..=6u8)
        .find(|&lv| (f32::from(theme.heading_line(lv)) - f32::from(row.lh)).abs() < 0.5)
        .map(|lv| theme.heading_fs(lv))
        .unwrap_or(theme.font_size);
    px((f32::from(fs) * TITLE_GLYPH_H_EM).min(f32::from(row.lh)))
}

/// 光标竖条在行带内的**垂直位移**（px，相对该行文本带顶）：
/// 标题行 = `(行带高 − 字形高)/2` → 竖条精确覆盖字形框（顶=字形顶、底=基线+descent，
/// 如 I-beam 落在该行文字上），不再「偏下/悬在行带下方」；
/// 其余（正文/代码/B lank/分隔等）保持既有 `(r.lh − f.line_h)/2` 居中口径 → 现状不变。
fn caret_shift_for(row: &ShapedRow, rd: &RefCell<WyRenderModel>, f: &Frame) -> f32 {
    if matches!(row.kind, IrKind::Heading(_)) {
        let glyph = f32::from(title_glyph_height(row, rd));
        ((f32::from(row.lh) - glyph) / 2.).max(0.)
    } else {
        ((f32::from(row.lh) - f32::from(f.line_h)) / 2.).max(0.)
    }
}

/// 光标竖条高度（px）：标题行 = 该行文字字形高（见 [`title_glyph_height`]）；文本/
/// 代码/空/分隔行 = 所在行行高 `r.lh`；表格单元保持既有 `para_line` 居中高（行内垂直
/// 居中口径不变）；未命中行兜底正文行高 `f.line_h`。独立扫行（量程高频但仅几十行），
/// 不动 [`caret_geometry`] 既有关键几何路径。
pub fn caret_bar_height(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    f: &Frame,
) -> Pixels {
    let (cl, _col) = docutil::row_col(host.doc(), host.caret());
    for r in &f.rows {
        if !r.src_lines.contains(&cl) {
            continue;
        }
        // 表格单元：光标条按既有 `para_line` 高度垂直居中于行，保持原观感。
        if let (Some(te), Some(_t)) = (&r.table_edit, &r.table)
            && te.rows.iter().any(|rr| rr.src_line == cl)
        {
            return rd.borrow().theme.para_line();
        }
        // 标题行：竖条覆盖该行文字字形（I-beam 落在标题文字上），而非整行带。
        if matches!(r.kind, IrKind::Heading(_)) {
            return title_glyph_height(r, rd);
        }
        // 其余（正文/代码/空行/分隔等）：跟随所在行行高。
        return r.lh;
    }
    f.line_h
}

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
        // 不可编辑行（空标题 `# ` / 空列表项 / 分隔线 / 空引述 / 空代码块 / Blank 等）：
        // 只要光标落在这行（点击 / 键盘导航），就画光标竖条于行首内容起点。这是既有
        // `Blank` 特殊分支的语义推广——否则「点空标题行 → 光标不画」（用户报告的
        // 「光标消失」）。只读预览的竖条由宿主绘制门控 `is_editable()` 另行抑制。
        //
        // 垂直锚 = 与可编辑分支同一推导（`text_y_off` + `text_row_shift` + 行带内
        // 位移）：标题行按字形高居中（`caret_shift_for`，竖条覆盖字形框），其余按
        // `(lh − line_h)/2`（现状）。空标题行行带更高（标题字号），光标不能锚带顶
        // （视觉偏上），锚到「该行文本将出现的字形位置」——与输入一个字符后的光标
        // 位置一致。Blank（`lh == f.line_h`）→ 位移 0、无撑高下移 → 行为零变化。
        if !r.editable {
            let caret_shift = caret_shift_for(r, rd, f);
            let ctop = r.top + px(text_y_off(r) + text_row_shift(r) + caret_shift);
            return (true, f.padding + r.x_off, ctop);
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
                f.padding + r.x_off + px(cell_x0 + cell.text_x + col_x - scroll),
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
        let caret_shift = caret_shift_for(r, rd, f);
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
                let rel_x = (f32::from(p.x) - f32::from(frame.padding) - f32::from(r.x_off)
                    + scroll)
                    .max(0.);
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
    // 未命中任何行：点击落在元素内空白区（首行之上 / 内容尾部之后）。旧行为返回当前
    // 光标 = 「点空白无反应」（用户报告的「点文档尾部之后的空白 → 光标到不了末尾」）。
    // 按 y 相对内容钳位：落在**末行之下** → 钳到内容末尾（source_end，跟手即得 +
    // 可画）；首行之上保留现状（不动，避免顶内衬 / 元素上缘点击误移光标）。
    if let Some(last) = frame.rows.last()
        && rel_y >= f32::from(last.top) + f32::from(last.height)
    {
        let ls = docutil::line_start(host.doc(), last.src_line);
        let le = docutil::line_end(host.doc(), last.src_line).max(ls);
        return le.min(host.doc().len());
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
        for lk in &r.extra.links {
            let (Some(p0), Some(p1)) = (
                w0.position_for_index(lk.disp.start, r.lh),
                w0.position_for_index(lk.disp.end, r.lh),
            ) else {
                continue;
            };
            // 逐可视带水平对齐移量（与 paint_text_row 逐带 `align_offsets[k]` 一致）：
            // head/tail 各按其所在带取，避免「跨带链接 + 标题对齐」时上下两段各错位。
            // band 查找姿势同本文件 `source_offset_at_local`（`text_band_at` 吃块首相对 y）。
            let a0 = r
                .align_offsets
                .get(text_band_at(r, f32::from(p0.y).max(0.)))
                .copied()
                .unwrap_or(px(0.));
            let a1 = r
                .align_offsets
                .get(text_band_at(r, f32::from(p1.y).max(0.)))
                .copied()
                .unwrap_or(px(0.));
            let ox0 = f32::from(frame.padding) + f32::from(r.x_off) + f32::from(a0);
            let ox1 = f32::from(frame.padding) + f32::from(r.x_off) + f32::from(a1);
            let p0y = top + text_row_shift(r) + f32::from(p0.y);
            let p1y = top + text_row_shift(r) + f32::from(p1.y);
            let contains = if f32::from(p0.y) == f32::from(p1.y) {
                let x0 = ox0 + f32::from(p0.x);
                let x1 = ox1 + f32::from(p1.x);
                py >= p0y
                    && py < p0y + f32::from(r.lh)
                    && f32::from(p.x) >= x0
                    && f32::from(p.x) <= x1
            } else {
                let head_ok = py >= p0y
                    && py < p0y + f32::from(r.lh)
                    && f32::from(p.x) >= ox0 + f32::from(p0.x)
                    && f32::from(p.x) <= ox0 + f32::from(w0.width());
                let tail_ok = py >= p1y
                    && py < p1y + f32::from(r.lh)
                    && f32::from(p.x) >= ox1
                    && f32::from(p.x) <= ox1 + f32::from(p1.x);
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

// ── P3 / Slice A 收敛：code/table/math 三段横向滚动条纯几何原语 + 薄适配器 ──────

/// 横向滚动条纯几何：条/滑块位置与宽度 + 最大滚动量（px）。
/// 原为 code/table/math 三份同构内联计算，现收敛为 [`hscroll_geom`] 的产物。
/// `pub`（非 `pub(crate)`）：引擎 `pub fn paint_hscroll_thumb` 的签名要携带它，
/// 且宿主 `element/paint.rs` 经 `pub(crate) use` 再导出（可见性不得低于函数）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HScrollGeom {
    pub bar_x: f32,
    pub bar_w: f32,
    pub thumb_x: f32,
    pub thumb_w: f32,
    pub max_scroll: f32,
}

impl HScrollGeom {
    /// 降级为公开 API 裸元组（`markdown_wysiwyg` 侧 Slice B 消费面，暂保留元组契约）。
    fn into_tuple(self) -> (f32, f32, f32, f32, f32) {
        (self.bar_x, self.bar_w, self.thumb_x, self.thumb_w, self.max_scroll)
    }
}

/// 纯几何：输入已由调用方按 x_off 三态各自算好的 `bar_x`/`viewport` 与
/// `content_w`/`scroll`。语义差异原样保留：
/// - `always_band=false`（code/math）：内容未超宽（`content_w <= viewport + 1.`）→ `None`；
/// - `always_band=true`（table）：恒返回带，`max_scroll = (content_w - viewport).max(0.)`。
pub(crate) fn hscroll_geom(
    bar_x: f32,
    viewport: f32,
    content_w: f32,
    scroll: f32,
    min_thumb: f32,
    always_band: bool,
) -> Option<HScrollGeom> {
    if !always_band && content_w <= viewport + 1. {
        return None;
    }
    let max_scroll = if always_band {
        (content_w - viewport).max(0.)
    } else {
        content_w - viewport
    };
    let scroll = scroll.min(max_scroll).max(0.);
    let frac = if max_scroll > 0. {
        scroll / max_scroll
    } else {
        0.
    };
    let bar_w = viewport.max(1.);
    let thumb_w = (bar_w * viewport / content_w)
        .max(min_thumb)
        .min(bar_w);
    let thumb_x = bar_x + (bar_w - thumb_w) * frac;
    Some(HScrollGeom {
        bar_x,
        bar_w,
        thumb_x,
        thumb_w,
        max_scroll,
    })
}

/// 命中：行 `band` 顶判定（顶 = `row_top + row_height - CODE_HSCROLL_BAND`）+ 在条上
/// 给 `(on_thumb, grab)`。与行扫描外层按行谓词 + `py in [top, bottom)` 配合使用。
pub(crate) fn hscroll_hit_row(
    py: f32,
    px: f32,
    row_top: f32,
    row_height: f32,
    geo: &HScrollGeom,
) -> Option<(bool, f32)> {
    let band_top = row_top + row_height - config::layout::CODE_HSCROLL_BAND;
    if py < band_top {
        return None;
    }
    if px >= geo.bar_x && px <= geo.bar_x + geo.bar_w {
        let on_thumb = px >= geo.thumb_x && px <= geo.thumb_x + geo.thumb_w;
        let grab = px - geo.thumb_x;
        return Some((on_thumb, grab));
    }
    None
}

/// 按 `max_scroll` 夹取横向滚动值（`min(max).max(0)`，保留原 clamp 语义）。
pub(crate) fn clamp_scroll(scroll: f32, max_scroll: f32) -> f32 {
    scroll.min(max_scroll).max(0.)
}

/// 代码块横向滚动条几何（内部 `HScrollGeom` 版；paint/hit 同 crate 消费走此类，
/// 公开 API 经 `code_hscroll_geo` 降级为元组——`markdown_wysiwyg` Slice B 消费面）。
pub(crate) fn code_hscroll_geom_inner(
    rd: &RefCell<WyRenderModel>,
    _host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> Option<HScrollGeom> {
    let panel_w = (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.);
    let inset = f32::from(row.x_off).max(1.);
    let viewport = (panel_w - inset * 2.).max(1.);
    let max_w = row
        .wrapped
        .iter()
        .map(|wl| f32::from(wl.width()))
        .fold(0.0f32, f32::max);
    let scroll = rd.borrow().h_scroll.scroll_of(HScrollKind::Code, row.src_line);
    hscroll_geom(
        f32::from(f.padding) + inset,
        viewport,
        max_w,
        scroll,
        config::layout::CODE_HSCROLL_THUMB_MIN,
        false, // always_band=false：短行不超宽 → None
    )
}

/// 代码块横向滚动条几何（内容超宽才返回）。公开 API 保持裸元组 = 既有跨 crate 契约。
pub fn code_hscroll_geo(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> Option<(f32, f32, f32, f32, f32)> {
    code_hscroll_geom_inner(rd, host, f, row).map(HScrollGeom::into_tuple)
}

/// 代码块当前生效的横向滚动偏移（px）：
pub fn code_hscroll_clamped(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> f32 {
    match code_hscroll_geom_inner(rd, host, f, row) {
        Some(g) => clamp_scroll(
            rd.borrow().h_scroll.scroll_of(HScrollKind::Code, row.src_line),
            g.max_scroll,
        ),
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
        let Some(geo) = code_hscroll_geom_inner(rd, host, frame, r) else {
            continue;
        };
        if let Some((on_thumb, grab)) = hscroll_hit_row(py, px, top, f32::from(r.height), &geo) {
            return Some((r.src_line, on_thumb, grab));
        }
    }
    None
}

/// 表格横向滚动条几何（内部 `HScrollGeom` 版；公开 API 经 `table_hscroll_geo` 降级为元组）。
pub(crate) fn table_hscroll_geom_inner(
    rd: &RefCell<WyRenderModel>,
    _host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> Option<HScrollGeom> {
    let t = row.table.as_ref()?;
    // 表格内容左缘 = 内容盒左缘 + 行 `x_off`（引号内表格缩进）；视口与滚动条
    // 均以该左缘为准，保证：滚动条对齐表格边缘、max_scroll 含右缘可达、内容
    // 遮罩不盖住引号竖条（2026-11-13，真机复查 #1/#2/#3）。
    let x_off = f32::from(row.x_off);
    let viewport = (f32::from(f.width) - f32::from(f.padding) * 2. - x_off).max(1.);
    let max_w = t.total_w.max(1.);
    let scroll = rd.borrow().h_scroll.scroll_of(HScrollKind::Table, row.src_line);
    hscroll_geom(
        f32::from(f.padding) + x_off,
        viewport,
        max_w,
        scroll,
        config::layout::CODE_HSCROLL_THUMB_MIN,
        true, // always_band=true：表格恒显带（恒 Some）
    )
}

/// 表格横向滚动条几何（内容超宽才返回）。公开 API 保持裸元组 = 既有跨 crate 契约。
pub fn table_hscroll_geo(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> Option<(f32, f32, f32, f32, f32)> {
    table_hscroll_geom_inner(rd, host, f, row).map(HScrollGeom::into_tuple)
}

/// 表格当前生效的横向滚动偏移（px）。
pub fn table_hscroll_clamped(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> f32 {
    match table_hscroll_geom_inner(rd, host, f, row) {
        Some(g) => clamp_scroll(
            rd.borrow().h_scroll.scroll_of(HScrollKind::Table, row.src_line),
            g.max_scroll,
        ),
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
        let Some(geo) = table_hscroll_geom_inner(rd, host, frame, r) else {
            continue;
        };
        if let Some((on_thumb, grab)) = hscroll_hit_row(py, px, top, f32::from(r.height), &geo) {
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

/// 数学卡横向滚动条几何（内部 `HScrollGeom` 版；公开 API 经 `math_hscroll_geo` 降级为元组）。
pub(crate) fn math_hscroll_geom_inner(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> Option<HScrollGeom> {
    if !matches!(&row.extra.card, Some(BlockCard::Math { .. })) {
        return None;
    }
    let viewport = (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.);
    let natural_w = math_natural_w(rd, host, row);
    let scroll = rd.borrow().h_scroll.scroll_of(HScrollKind::Math, row.src_line);
    hscroll_geom(
        f32::from(f.padding),
        viewport,
        natural_w,
        scroll,
        config::layout::CODE_HSCROLL_THUMB_MIN,
        false, // always_band=false：公式不超宽 → None
    )
}

/// 数学卡横向滚动条几何（公式自然宽 > 内容盒才返回）。公开 API 保持裸元组 = 既有跨 crate 契约。
pub fn math_hscroll_geo(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> Option<(f32, f32, f32, f32, f32)> {
    math_hscroll_geom_inner(rd, host, f, row).map(HScrollGeom::into_tuple)
}

/// 数学卡当前生效的横向滚动偏移（px）。
pub fn math_hscroll_clamped(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    f: &Frame,
    row: &ShapedRow,
) -> f32 {
    match math_hscroll_geom_inner(rd, host, f, row) {
        Some(g) => clamp_scroll(
            rd.borrow().h_scroll.scroll_of(HScrollKind::Math, row.src_line),
            g.max_scroll,
        ),
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
        let Some(geo) = math_hscroll_geom_inner(rd, host, frame, r) else {
            continue;
        };
        if let Some((on_thumb, grab)) = hscroll_hit_row(py, px, top, f32::from(r.height), &geo) {
            return Some((r.src_line, on_thumb, grab));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::TestAppContext;
    use crate::theme::WyTheme;

    /// C6 复现（红测）：`link_hit_at` 漏 `align_offsets`。
    /// 对照正确姿势（hit.rs `source_offset_at_local` / `caret_geometry` 逐带取
    /// `r.align_offsets[text_band_at(...)]`）：命中判定必须加行的水平对齐移量，否则
    /// paint 侧画链接带 align 偏移（EPUB 标题 `%%ALIGN:CENTER%%` 等）而命中侧不偏移 →
    /// 水平错位、标题内链接点不中。当前 `ox = padding + x_off`（漏 align）→ 对齐带内的
    /// 点击 None = 红。
    ///
    /// 测试夹具：headless `#[gpui::test]` 用独立 `WindowTextSystem`（与真实
    /// `Window::text_system()` 同构：`WindowTextSystem::new(Arc<TextSystem>)`）在测试平台
    /// 上 shape 一行真实链接文本；手动装配 `ShapedRow`（`align_offsets=[100px]`、
    /// `x_off=0`）与 `Frame`，`StubHost::last_bounds` 置窗口原点 → 元素局部 == 窗口坐标。
    #[gpui::test]
    fn link_hit_at_considers_align_offsets(cx: &mut TestAppContext) {
        let wts = gpui::WindowTextSystem::new(cx.text_system().clone());
        let row = aligned_link_row(&wts);
        // 几何推导（K177/K143）：shape 期行宽（Noop 文本 0.6em/字符，16px × 6 字符 ≈
        // 57.6px）——夹具必须 < align(100px)（对齐带内点才在新命中区且旧命中区之外）。
        let link_w = f32::from(row.wrapped.first().unwrap().width());
        assert!(
            link_w > 12. && link_w < 100.,
            "夹具行宽须落在可区分区间（>12 使旧区左缘点可命中、<100 使对齐点区分得开），实得 {link_w}"
        );
        let frame = Frame {
            padding: px(12.),
            line_h: px(20.),
            rows: vec![row],
            base: Rc::new(RefCell::new(BlockLayout::new(vec![20.], vec![0u32]))),
            deltas: Rc::new([]),
            total: px(20.),
            band: (0., 1000.),
            width: px(600.),
            rgen: 0,
            igen: 0,
            mgen: 0,
            zgen: 0,
        };
        let rd = RefCell::new(WyRenderModel::new(test_theme()));
        let stub = StubHost::new();
        let py = 10.0f32;
        // 修复后命中区 = [padding + x_off + align, +align+行宽] = [112, 112+link_w]。
        // 取带内一点（padding + 112）：修复前忽略 align → 旧命中区 [12, 12+link_w] →
        // None = 红。
        let hit = link_hit_at(
            &rd,
            &stub,
            &frame,
            point(px(f32::from(frame.padding) + 112.0), px(py)),
        );
        assert!(
            hit.is_some(),
            "对齐带内的链接点击须命中（paint 已画在 align 偏移处），修复前漏 align_offsets → None = 红"
        );
        // 语义自洽：不含 align 的原位置（padding + 1 < align，链接已被右移）不应命中。
        let unshifted = link_hit_at(
            &rd,
            &stub,
            &frame,
            point(px(f32::from(frame.padding) + 1.0), px(py)),
        );
        assert!(
            unshifted.is_none(),
            "未偏移的原位置不应命中（链接被 align=100 右移后不在该处），实得 {unshifted:?}"
        );
    }

    /// 一条「整行是链接 + 逐可视行水平对齐移量 = 100px」的单行（段落）行。
    fn aligned_link_row(wts: &gpui::WindowTextSystem) -> ShapedRow {
        let text = "ABCDEF";
        let run = gpui::TextRun {
            len: text.len(),
            font: gpui::font("monospace"),
            color: gpui::Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let wrapped: Vec<gpui::WrappedLine> = wts
            .shape_text(text.into(), px(16.), std::slice::from_ref(&run), None, None)
            .unwrap()
            .into_vec();
        assert!(!wrapped.is_empty(), "shape_text 须产出至少一行");
        ShapedRow {
            src_line: 0,
            src_lines: 0..1,
            wrapped: Rc::new(wrapped),
            has_non_ascii: false,
            sub_heights: vec![px(20.)],
            byte_map: Rc::new(vec![0; text.len() + 1]),
            editable: true,
            multi_offsets: Vec::new(),
            code_ranges: Vec::new(),
            lh: px(20.),
            x_off: px(0.),
            align_offsets: vec![px(100.)],
            height: px(20.),
            top: px(0.),
            kind: IrKind::Paragraph,
            quoted: false,
            table: None,
            table_edit: None,
            image: None,
            mm: None,
            extra: RowExtra {
                links: vec![RowLink {
                    disp: 0..text.len(),
                    target: LinkHit::Url("https://example.com".into()),
                }],
                ..Default::default()
            },
        }
    }

    /// 最小 `WyRenderHost` 桩：本红测路径（`link_hit_at` → `to_local`）只消费
    /// `last_bounds`（窗口坐标 → 元素局部换算）；其余方法给合理默认、路径内不被调用。
    /// `image_cache` 无 `&App` 不可构造实体 → 用 `unimplemented!`（绝不被本路径触达）。
    struct StubHost {
        doc: markdown::doc::Doc,
        bounds: Bounds<Pixels>,
    }
    impl StubHost {
        fn new() -> Self {
            Self {
                doc: markdown::doc::Doc::open("行1\n行2\n".into()),
                bounds: Bounds::new(point(px(0.), px(0.)), size(px(600.), px(200.))),
            }
        }
    }
    impl WyRenderHost for StubHost {
        fn doc(&self) -> &markdown::doc::Doc {
            &self.doc
        }
        fn caret(&self) -> usize {
            0
        }
        fn is_editable(&self) -> bool {
            true
        }
        fn is_epub(&self) -> bool {
            false
        }
        fn last_bounds(&self) -> Option<Bounds<Pixels>> {
            Some(self.bounds)
        }
        fn memory_image(&self, _src: &str) -> Option<std::sync::Arc<gpui::Image>> {
            None
        }
        fn image_load_resource(&self, _src: &str) -> gpui::Resource {
            gpui::Resource::Path(std::sync::Arc::from(std::path::PathBuf::from("stub.png")))
        }
        fn image_cache(&self) -> gpui::Entity<gpui::RetainAllImageCache> {
            unimplemented!("本红测路径（link_hit_at）不消费图片缓存")
        }
        fn image_intrinsic_size(&self, _src: &str) -> Option<(f32, f32)> {
            None
        }
        fn math_slot(&self, _key: &str) -> Option<markdown_wysiwyg_math::MathSlot> {
            None
        }
        fn code_parse_lookup(
            &self,
            _lang: &str,
            _code: &str,
        ) -> crate::code_parse::CodeParseLookup {
            crate::code_parse::CodeParseLookup::NotReady
        }
        fn request_code_parse(&self, _lang: &str, _code: &str, _src_line: usize) {}
        fn focused_mermaid(&self) -> Option<usize> {
            None
        }
        fn hover_image(&self) -> Option<usize> {
            None
        }
        fn selected_image(&self) -> Option<usize> {
            None
        }
        fn hover_inline_image(&self) -> Option<crate::model::InlineImageRef> {
            None
        }
        fn selected_inline_image(&self) -> Option<crate::model::InlineImageRef> {
            None
        }
        fn selection(&self) -> Option<Range<usize>> {
            None
        }
        fn search_highlight_range(&self) -> Option<Range<usize>> {
            None
        }
    }

    /// 最小主题（复刻 frame.rs 测试 `test_theme`）：本红测只需一个 `WyRenderModel`；
    /// `link_hit_at` 路径不读主题，仅构造用。
    fn test_theme() -> WyTheme {
        use markdown::view::style::StyleParams;
        let sp = StyleParams {
            mono: "monospace".into(),
            fg: gpui::Hsla::default(),
            muted_fg: gpui::Hsla::default(),
            code_bg: gpui::Hsla::default(),
            code_inline_fg: gpui::Hsla::default(),
            code_inline_bg: gpui::Hsla::default(),
            divider: gpui::Hsla::default(),
            accent: gpui::Hsla::default(),
            link: gpui::Hsla::default(),
            quote_bg: gpui::Hsla::default(),
            table_head_bg: gpui::Hsla::default(),
            mermaid_accents: [gpui::Hsla::default(); 6],
            mermaid_border: gpui::Hsla::default(),
            para_size: gpui::px(16.),
            para_line: gpui::px(23.2),
            code_size: gpui::px(14.),
            code_line: gpui::px(20.8),
            is_dark: false,
        };
        WyTheme::from_style(&sp, gpui::font("monospace"), gpui::px(16.))
    }
}
