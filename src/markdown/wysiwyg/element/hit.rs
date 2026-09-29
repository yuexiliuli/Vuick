//! WYSIWYG 交互/命中层：窗口坐标 ↔ 源字节（光标几何、点击命中）、代码/表格横向滚动条的几何/命中/跳转与拖拽换算。与 `element` 共用几何。
use super::*;
use gpui::{Pixels, Point, px, point};
use crate::markdown::ir::IrKind;
use crate::markdown::parse::recognition::LinkHit;


/// 源 caret → 光标条几何。非文本行停在块首（只读）；代码块内停在对应内容行。
/// 行高用 `r.lh`（标题/代码等类型各自行高），否则光标会错位。
pub(crate) fn caret_geometry(
    editor: &WyEditor,
    f: &Frame,
) -> (bool, Pixels, Pixels) {
    let (cl, col) = editor.row_col(editor.caret());
    let line_start = editor.line_start(cl);
    let line_end = editor.line_end(cl);
    for r in &f.rows {
        if !r.src_lines.contains(&cl) {
            continue;
        }
        if !r.editable {
            return (false, px(12.), r.top);
        }
        if let (Some(te), Some(t)) = (&r.table_edit, &r.table) {
            // 表格：caret 落在某个可见行（表头/数据行）→ 定位到单元格内容处。
            let Some(row_ix) = te.rows.iter().position(|rr| rr.src_line == cl) else {
                return (false, px(12.), r.top);
            };
            let row_l = &te.rows[row_ix];
            let target = col.min(line_end.saturating_sub(line_start));
            let Some((cell_ix, dcol)) = table_cell_at_src(&te.rows, cl, target) else {
                return (false, px(12.), r.top);
            };
            let cell = &row_l.cells[cell_ix];
            let cell_x0 = table_cell_x0(&t.widths, cell_ix);
            let col_x = f32::from(cell.line.x_for_index(dcol.min(cell.line.len())));
            // 垂直居中于该行（与 paint 文字纵向居中一致；光标高用正文行高近似）。
            let caret_h = f32::from(editor.theme.para_line());
            let row_top = r.top + px(table_row_top(t, row_ix));
            let ctop = row_top + px(((f32::from(t.row_h) - caret_h) / 2.).max(0.));
            // 横向滚动时单元格内容随表平移 → 光标条反向减 scroll（paint 平移的逆映射）。
            let scroll = table_hscroll_clamped(editor, f, r);
            return (true, px(12.) + px(cell_x0 + cell.text_x + col_x - scroll), ctop);
        }
        if !r.multi_offsets.is_empty() {
            // 代码块：caret 落在围栏内容行（`src_line+1 ..= src_line+n`）才画光标，
            // 经块级 byte_map 映射到该代码行的渲染列与垂直偏移；围栏行上不画。
            let n = r.multi_offsets.len();
            if cl > r.src_line && cl <= r.src_line + n {
                let block_start = editor.line_start(r.src_line);
                let target = (line_start + col).saturating_sub(block_start);
                let rend = source_to_display(&r.byte_map, target);
                let g = group_from_rend(r, rend);
                let dcol = rend
                    .saturating_sub(r.multi_offsets[g])
                    .min(group_len(r, g));
                let p = r
                    .wrapped
                    .get(g)
                    .and_then(|wl| wl.position_for_index(dcol, r.lh))
                    .unwrap_or_else(|| point(px(0.), px(0.)));
                let y = r.top + px(code_group_top(r, g) + f32::from(p.y));
                let scroll = code_hscroll_clamped(editor, f, r);
                return (true, px(12.) + r.x_off + p.x - px(scroll), y);
            }
            return (false, px(12.), r.top);
        }
        // 单源码行文本块：源列 → 渲染列（`source_to_display`，左界）——与
        // `source_offset_at` 的反查互为逆映射，光标才落在点击处。
        let target = col.min(line_end.saturating_sub(line_start));
        let dcol = source_to_display(&r.byte_map, target);
        // 折行时 p.y 是子行偏移，叠加到块首 + 撑高下移才算准缓存所在的可视行。
        let p = render_point_of(&r.wrapped, r.lh, dcol);
        // `px(12.)` = 内容边距；引述/代码等额外缩进走 `r.x_off`（与 paint 同 x）。
        return (true, px(12.) + r.x_off + p.x, r.top + px(text_y_off(r) + text_row_shift(r)) + p.y);
    }
    // 带外（caret 所在块未物化 = 屏外）：用紧凑布局 `tops` 兜底定位，不画光标
    // （屏外被容器裁剪）。`row_col`/`line_start` 此时查的仍是块内源码行。
    let ctop = px(f.src_line_top(cl));
    (true, px(12.), ctop)
}



/// 点命中（**元素局部坐标**）→ 源字节偏移（帧内 byte_map 反查）。
/// 命中用的行/列全是元素局部（paint 以 `origin = bounds.origin` 为起点绘制）；
/// 传入前把窗口坐标减掉元素原点即可（见 [`source_offset_at`]）。
pub(crate) fn source_offset_at_local(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> usize {
    let rel_y = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if rel_y >= top && rel_y < bottom {
            if !r.editable {
                return editor.line_end(r.src_line).max(editor.line_start(r.src_line));
            }
            if let (Some(te), Some(t)) = (&r.table_edit, &r.table) {
                // 表格：y 定可见行（表头/数据行），x 定列，再按对齐位移映射列内渲染列
                // → 单元格 byte_map 反查源字节（相对该行行首）。
                let Some(row_ix) = table_row_at(t, rel_y - top) else {
                    return editor.caret();
                };
                let Some(row_l) = te.rows.get(row_ix) else {
                    return editor.caret();
                };
                // 横向滚动时内容坐标 = 视图坐标 + scroll（paint 平移的逆映射）。
                let scroll = table_hscroll_clamped(editor, frame, r);
                let rel_x = (f32::from(p.x) - f32::from(frame.padding) + scroll).max(0.);
                let cell_ix = table_cell_index(&t.widths, rel_x)
                    .unwrap_or(t.widths.len().saturating_sub(1));
                let Some(cell) = row_l.cells.get(cell_ix) else {
                    return editor.caret();
                };
                let cell_x0 = table_cell_x0(&t.widths, cell_ix);
                let in_cell = (rel_x - cell_x0 - cell.text_x).max(0.);
                let col = cell
                    .line
                    .closest_index_for_x(px(in_cell))
                    .min(cell.map.len().saturating_sub(1))
                    .min(cell.line.len());
                let local = *cell.map.get(col).unwrap_or(&0);
                let ls = editor.line_start(row_l.src_line);
                let le = editor.line_end(row_l.src_line).max(ls);
                return (ls + local).min(le);
            }
            if !r.multi_offsets.is_empty() {
                // 代码块：y 定源码行组（`code_group_at`，首空白带归第 0 组），x 定列；
                // 经块级 byte_map 反查 块首源偏移 → 绝对源字节（钳到末内容行行尾）。
                // 横向滚动时内容坐标 = 视图坐标 + scroll（paint 平移的逆映射）。
                let g = code_group_at(r, rel_y - top);
                let Some(wl) = r.wrapped.get(g) else {
                    return editor.caret();
                };
                let scroll = code_hscroll_clamped(editor, frame, r);
                let rel_x = (f32::from(p.x) - f32::from(frame.padding) - f32::from(r.x_off)
                    + scroll)
                    .max(0.);
                let rel = point(px(rel_x), px(0.));
                // `closest_index_for_position` 的 `Err` 是越界钳制后的有效索引
                // （点过行尾/行首）；把它当 0 会让点行尾时光标跳回行首。
                let dcol = match wl.closest_index_for_position(rel, r.lh) {
                    Ok(i) | Err(i) => i,
                };
                let rend = r.multi_offsets[g] + dcol.min(group_len(r, g));
                let local = display_to_source(&r.byte_map, rend);
                let ls = editor.line_start(r.src_line);
                let le = editor.line_end(r.src_line + r.multi_offsets.len()).max(ls);
                return (ls + local).min(le);
            }
            if let Some(w0) = r.wrapped.first() {
                // x 先扣内容边距与行级缩进（引述等），y 用点击在块内的**子行偏移**
                // （`rel_y - top - 撑高下移`）：`closest_index_for_position` 据此选折行的
                // 第几可视行——写成 0 或漏掉下移量会把撑高行上的点击映射错行/错列。
                let rel_x = (f32::from(p.x) - f32::from(frame.padding) - f32::from(r.x_off))
                    .max(0.);
                let sub_y = (rel_y - top - text_y_off(r) - text_row_shift(r)).max(0.);
                let rel = point(px(rel_x), px(sub_y));
                // `closest_index_for_position` 的 `Err` 是越界钳制后的有效索引
                // （点过行尾/行首）；把它当 0 会让点行尾时光标跳回行首。
                let dcol = match w0.closest_index_for_position(rel, r.lh) {
                    Ok(i) | Err(i) => i,
                };
                let local = display_to_source(&r.byte_map, dcol);
                let ls = editor.line_start(r.src_line);
                let le = editor.line_end(r.src_line);
                return (ls + local).min(le);
            }
        }
    }
    editor.caret()
}



/// 点命中（**窗口坐标**，`MouseDownEvent.position`）→ 源字节偏移。
pub(crate) fn source_offset_at(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> usize {
    // window 坐标先换回元素局部（否则元素被 `live-body` 内边距/滚动/窗口位置偏移后，
    // 点击会映射到错误的行/列），再走统一的局部命中。
    let origin = editor.last_bounds().map(|b| b.origin).unwrap_or_else(|| point(px(0.), px(0.)));
    let local = point(p.x - origin.x, p.y - origin.y);
    source_offset_at_local(editor, frame, local)
}



/// 元素局部坐标（把窗口坐标减掉元素原点）。
pub(crate) fn to_local(editor: &WyEditor, p: Point<Pixels>) -> Point<Pixels> {
    let origin = editor.last_bounds().map(|b| b.origin).unwrap_or_else(|| point(px(0.), px(0.)));
    point(p.x - origin.x, p.y - origin.y)
}



/// 命中任务框（C1）：返回 (源行, 是否已勾选)。击中「任务行 gutter（☐/☑ 字形区）」
/// 才返回，否则 None（与 `paint_row_gutter` 的 GUTTER_MARK_X 几何一致）。
pub(crate) fn checkbox_hit(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool)> {
    let p = to_local(editor, p);
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
        // 字形区 ≈ [padding, padding + 24)（与任务行正文缩进 x_off 内）。
        let hi = (f32::from(frame.padding) + 24.).max(f32::from(r.x_off));
        if px >= f32::from(frame.padding) && px <= hi {
            return Some((r.src_line, *checked));
        }
    }
    None
}



/// 命中警报折叠箭头（C3）：击中警报行 gutter（[padding, padding+ALERT_GUTTER_W] × 行高）
/// 返回 (源行, 当前是否折叠)。
pub(crate) fn alert_chevron_hit(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool)> {
    let p = to_local(editor, p);
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
/// 返回 (源行, 当前是否折叠)。几何与 `paint_heading_chevron` 共用。
pub(crate) fn heading_chevron_hit(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool)> {
    let p = to_local(editor, p);
    let py = f32::from(p.y);
    let px = f32::from(p.x);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom && matches!(r.kind, IrKind::Heading(_))) {
            continue;
        }
        let Some((cx0, cw)) = heading_chevron_box(frame, r) else {
            // 带外（未 shape）标题行没有 wrapped：非可视区，跳过。
            continue;
        };
        if px >= cx0 && px <= cx0 + cw {
            let key = editor.line_text(r.src_line).trim().to_string();
            let folded = editor.folded_headings.borrow().contains(&key);
            return Some((r.src_line, folded));
        }
    }
    None
}



/// 标题折叠箭头几何：画在**左缘 gutter**（[padding, padding+HEADING_GUTTER_W]，正文
/// 已右移 `x_off = HEADING_GUTTER_W`）；返回元素局部坐标 (x0, w)。paint 与 hit-test 共用。
pub(crate) fn heading_chevron_box(f: &Frame, _row: &ShapedRow) -> Option<(f32, f32)> {
    Some((f32::from(f.padding), HEADING_GUTTER_W))
}



/// 命中行内 `$…$` 公式的 spacer：击中其显示区 → 返回该公式的**绝对源字节区间**
/// （含定界符 `$…$`，`InlineMathText.source`）。双击命中时把光标塞进区间起点 + 1
/// reveal 出可编辑的原始 LaTeX（B2 双击编辑）。
pub(crate) fn inline_math_hit_at(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<Range<usize>> {
    let p = to_local(editor, p);
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
            // spacer 恒在首可视行（`visual_block_line_for_index` = 0），只按首行 y 命中。
            let y0 = top + text_row_shift(r) + f32::from(p0.y);
            let x0 = ox + f32::from(p0.x);
            let x1 = ox + f32::from(p1.x);
            if py >= y0
                && py < y0 + f32::from(r.lh)
                && px >= x0
                && px <= x1.max(x0 + 1.)
                && !im.source.is_empty() {
                    return Some(im.source.clone());
                }
        }
    }
    None
}



/// 命中数学块卡片（A1，`$$…$$` 折叠成的 `BlockCard::Math` 行）：击中卡片范围 →
/// 返回该块的**首源码行行首绝对字节**（把光标放进去即 reveal 源码进入编辑）。
/// 单击命中不应移动光标（否则 reveal 回源码字符串——用户看到的「单击变回字符串」）；
/// 双击编辑由调用方在 `click_count >= 2` 时把光标放到这里。
pub(crate) fn math_card_hit(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> Option<usize> {
    let p = to_local(editor, p);
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
        // 卡片铺满内容宽：左 = padding，右 = padding + 内容宽。
        let x0 = f32::from(frame.padding);
        let x1 = f32::from(frame.width) - f32::from(frame.padding);
        if px >= x0 && px <= x1 {
            return Some(editor.line_start(r.src_line));
        }
    }
    None
}



/// Mermaid 卡点击命中（**聚焦**用）：窗口坐标 → 该卡 `src_line`。点击后该卡获得焦点，
/// 此后**只有它**响应滚轮缩放；未聚焦时滚轮一律放行给宿主正常滚动。
pub(crate) fn mermaid_card_hit(editor: &WyEditor, frame: &Frame, p: Point<Pixels>) -> Option<usize> {
    let p = to_local(editor, p);
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
        // 卡片铺满内容宽：左 = padding，右 = padding + 内容宽。
        let x0 = f32::from(frame.padding);
        let x1 = f32::from(frame.width) - f32::from(frame.padding);
        if px >= x0 && px <= x1 {
            return Some(r.src_line);
        }
    }
    None
}



/// 命中的可点击链接目标（C5）：窗口坐标 → 行内链接显示区间集合，命中的返回 `LinkHit`。
pub(crate) fn link_hit_at(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<LinkHit> {
    let p = to_local(editor, p);
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
                py >= p0y && py < p0y + f32::from(r.lh) && f32::from(p.x) >= x0 && f32::from(p.x) <= x1
            } else {
                // 折行：头段或尾段
                let head_ok = py >= p0y && py < p0y + f32::from(r.lh)
                    && f32::from(p.x) >= ox + f32::from(p0.x)
                    && f32::from(p.x) <= ox + f32::from(w0.width());
                let tail_ok = py >= p1y && py < p1y + f32::from(r.lh)
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



/// 代码块横向滚动条几何（内容超宽才返回）。
/// 返回**元素局部** x（paint 加 `origin.x`，命中直接用）：
/// `(bar_x, bar_w, thumb_x, thumb_w, max_scroll)`。`viewport_text` = 面板内文本可视宽
/// （面板宽 − 左右内衬 `x_off`×2）；`max_scroll` = 最宽行 − viewport。
pub(crate) fn code_hscroll_geo(
    editor: &WyEditor,
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
    let scroll = editor.code_h_scroll_of(row.src_line).min(max_scroll).max(0.);
    let frac = if max_scroll > 0. { scroll / max_scroll } else { 0. };
    let bar_x = f32::from(f.padding) + inset;
    let bar_w = viewport.max(1.);
    // 与 view 的 gpui-component 同式：thumb = container/scroll_area × container，最小 48。
    let thumb_w =
        (bar_w * viewport / max_w)
            .max(crate::config::layout::CODE_HSCROLL_THUMB_MIN)
            .min(bar_w);
    let thumb_x = bar_x + (bar_w - thumb_w) * frac;
    Some((bar_x, bar_w, thumb_x, thumb_w, max_scroll))
}



/// 代码块当前生效的横向滚动偏移（px）：内容不超宽 → 0（忽略陈旧滚动值，宽窗口下
/// 长行不再偏移）；超宽 → 按 `max_scroll` clamp。paint / 命中 / 光标 / 选区共用。
pub(crate) fn code_hscroll_clamped(editor: &WyEditor, f: &Frame, row: &ShapedRow) -> f32 {
    match code_hscroll_geo(editor, f, row) {
        Some((_, _, _, _, max_scroll)) => editor
            .code_h_scroll_of(row.src_line)
            .min(max_scroll)
            .max(0.),
        None => 0.,
    }
}



/// 代码块横向滚动条命中（元素局部→窗口坐标）：命中**底部滚动条带**
/// （轨道 ~`CODE_HSCROLL_BAND` 高）→ 返回
/// `(块首源行, 是否点在滑块上, 指针相对滑块左缘的水平偏移)`。
/// 命中轨道（不在滑块上）→ 调用方让滑块跳到点击处；点在滑块上 → 进入拖动。
/// 与 `paint_code_row` 的滚动条几何同源（view 的 gpui-component 语义：
/// **抓取点 = 指针相对滑块左缘的偏移**——按下瞬间不跳档，拖动时滑块钉住该点
/// 1:1 跟随指针；旧实现从滑块**中心**量 grab，按下就前跳半个滑块宽 → 不跟手）。
pub(crate) fn code_hscrollbar_hit(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool, f32)> {
    let p = to_local(editor, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) || r.multi_offsets.is_empty() || r.wrapped.is_empty() {
            continue;
        }
        let Some((bar_x, bar_w, thumb_x, thumb_w, _)) = code_hscroll_geo(editor, frame, r) else {
            continue;
        };
        // 底部滚动条带：轨道顶（面板底 − `CODE_HSCROLL_BAND`）之间。
        let band_top = f32::from(r.top) + f32::from(r.height) - crate::config::layout::CODE_HSCROLL_BAND;
        if py < band_top {
            continue;
        }
        if px >= bar_x && px <= bar_x + bar_w {
            let on_thumb = px >= thumb_x && px <= thumb_x + thumb_w;
            // 抓取点 = 指针相对滑块**左缘**偏移（拖动时 `thumb_x = px - grab`，
            // 与 gpui-component 的 `drag_pos` 同语义：按下不跳、移动即钉住跟手）。
            let grab = px - thumb_x;
            return Some((r.src_line, on_thumb, grab));
        }
    }
    None
}



/// 表格横向滚动条几何（内容超宽才返回）。
/// 返回**元素局部** x（paint 加 `origin.x`，命中直接用）：
/// `(bar_x, bar_w, thumb_x, thumb_w, max_scroll)`。`viewport` = 内容区宽
/// （帧宽 − 左右内边距 `padding`×2，与 `paint_table_row` 的网格同宽，表格无 x_off）；
/// `max_scroll` = 表格总宽 − viewport。对齐 preview `markdown/view` 的表格横向滚动：
/// 表格底部**恒定预留一条滚动条带**，滚动条始终可见（Always 语义）——不超宽时
/// 滑块占满整条轨道（惰性、不可拖动），超宽时可拖/轨道跳转。不超宽只影响
/// `max_scroll`/thumb，不影响 height/命中（预留带在网格之下、本无内容）。
pub(crate) fn table_hscroll_geo(
    editor: &WyEditor,
    f: &Frame,
    row: &ShapedRow,
) -> Option<(f32, f32, f32, f32, f32)> {
    let t = row.table.as_ref()?;
    let viewport = (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.);
    let max_w = t.total_w.max(1.);
    let max_scroll = (max_w - viewport).max(0.);
    let scroll = editor.table_h_scroll_of(row.src_line).min(max_scroll).max(0.);
    let frac = if max_scroll > 0. { scroll / max_scroll } else { 0. };
    let bar_x = f32::from(f.padding);
    let bar_w = viewport.max(1.);
    // 与 view 的 gpui-component 同式：thumb = container/scroll_area × container，最小 48。
    // 不超宽时 viewport ≥ max_w → thumb 被 clamp 成满宽（惰性轨道）。
    let thumb_w = (bar_w * viewport / max_w)
        .max(crate::config::layout::CODE_HSCROLL_THUMB_MIN)
        .min(bar_w);
    let thumb_x = bar_x + (bar_w - thumb_w) * frac;
    Some((bar_x, bar_w, thumb_x, thumb_w, max_scroll))
}



/// 表格当前生效的横向滚动偏移（px）：内容不超宽 → 0（忽略陈旧滚动值，宽窗口下
/// 表格不再偏移）；超宽 → 按 `max_scroll` clamp。paint / 命中 / 光标共用。
pub(crate) fn table_hscroll_clamped(editor: &WyEditor, f: &Frame, row: &ShapedRow) -> f32 {
    match table_hscroll_geo(editor, f, row) {
        Some((_, _, _, _, max_scroll)) => editor
            .table_h_scroll_of(row.src_line)
            .min(max_scroll)
            .max(0.),
        None => 0.,
    }
}



/// 表格横向滚动条命中（元素局部→窗口坐标）：命中**底部滚动条带**
/// （轨道 ~`CODE_HSCROLL_BAND` 高）→ 返回 `(块首源行, 是否点在滑块上,
/// 指针相对滑块左缘的水平偏移)`。语义与 [`code_hscrollbar_hit`] 一致
/// （点在滑块 → 拖动；点在轨道 → 轨道跳转）。
pub(crate) fn table_hscrollbar_hit(
    editor: &WyEditor,
    frame: &Frame,
    p: Point<Pixels>,
) -> Option<(usize, bool, f32)> {
    let p = to_local(editor, p);
    let px = f32::from(p.x);
    let py = f32::from(p.y);
    for r in &frame.rows {
        let top = f32::from(r.top);
        let bottom = top + f32::from(r.height);
        if !(py >= top && py < bottom) || r.table.is_none() {
            continue;
        }
        let Some((bar_x, bar_w, thumb_x, thumb_w, _)) = table_hscroll_geo(editor, frame, r) else {
            continue;
        };
        // 底部滚动条带：轨道顶（块底 − `CODE_HSCROLL_BAND`）之间。
        let band_top = f32::from(r.top) + f32::from(r.height) - crate::config::layout::CODE_HSCROLL_BAND;
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
