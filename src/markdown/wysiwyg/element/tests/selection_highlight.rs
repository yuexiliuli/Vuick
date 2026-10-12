// 选区高亮（`paint_selection` → `paint_text_selection_hl` → 纯函数
// `text_selection_hl_rects`）回归：长列表项（多折行）拖选整行时，每个可视行都应整行
// 高亮——首行与续行起点=行首、宽=该行整行宽，y 逐行递增 `lh`。同时锁住短项单行行为。
//
// 历史 bug：续行用 `position_for_index` 取「行首边界字节」会被归到上一可视行右缘，
// 导致整行选中时续行高亮缩成右端竖条（只有末尾一段被选中）。新几何以每可视行左缘
// 取 x=0 兜底（见 `text_selection_hl_rects` 注释）。

/// 逐换行边界组成可视行字节区间（测试内同为 gpui `wrap_boundaries` 语义的 oracle，
/// 与 `text_selection_hl_rects` 内部推导一致：换行点字节 = 边界前最后 glyph 的起点）。
fn visual_spans(w0: &gpui::WrappedLine) -> Vec<(usize, usize)> {
    let runs = &w0.unwrapped_layout.runs;
    let mut rows: Vec<(usize, usize)> = Vec::new();
    let mut prev = 0usize;
    for b in w0.wrap_boundaries() {
        let mut byte = 0usize;
        for (ri, run) in runs.iter().enumerate() {
            let last = run.glyphs.len().saturating_sub(1);
            if ri < b.run_ix {
                byte = run.glyphs.get(last).map(|g| g.index + 1).unwrap_or(byte);
            } else if ri == b.run_ix {
                byte = run
                    .glyphs
                    .get(b.glyph_ix.min(last))
                    .map(|g| g.index)
                    .unwrap_or(byte);
                break;
            }
        }
        if byte > prev {
            rows.push((prev, byte));
            prev = byte;
        }
    }
    rows.push((prev, w0.len().max(1)));
    rows
}

/// 正文在帧内的左缘 x（帧内衬 + 行级 gutter）；选区高亮与文本 paint 同源。
fn line_start_x(
    row: &crate::markdown::wysiwyg::element::ShapedRow,
    frame: &crate::markdown::wysiwyg::element::Frame,
) -> f32 {
    f32::from(frame.padding) + f32::from(row.x_off)
}

/// 长有序列表项（>2 折行）拖选整行：每个可视行整行高亮（起点=行首、宽=整行宽）。
#[gpui::test]
fn long_ordered_full_line_selection_highlights_every_wrapped_line(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    use super::{text_row_shift, text_selection_hl_rects, text_y_off};
    cx.update(crate::actions::bind_local_keymaps);
    let item = "1. 有序列表项 A testtesttesttesttesttesttesttesttesttesttesttesttest "
        .repeat(6)
        + "end\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&item));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
        let r = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Bullet)
            .expect("应有有序列表行");
        let w0 = r.wrapped.first().expect("文本行必有首 shard");
        let spans = visual_spans(w0);
        assert!(
            spans.len() >= 3,
            "长有序项应折成 ≥3 可视行，实际 {}",
            spans.len()
        );
        let ls = ed.read(cx).line_start(r.src_line);
        let le = ed.read(cx).line_end(r.src_line);
        let rects = text_selection_hl_rects(
            ed.read(cx),
            r,
            &(ls..le),
            f32::from(frame.padding),
            0.,
        );
        let x0 = line_start_x(r, &frame);
        let y0 = f32::from(r.top) + text_y_off(r) + text_row_shift(r);
        assert_eq!(
            rects.len(),
            spans.len(),
            "整行选中 → 每可视行一条高亮",
        );
        for (i, (rect, (_, b2))) in rects.iter().zip(&spans).enumerate() {
            let (x, y, w) = *rect;
            let full_w = f32::from(w0.position_for_index(*b2, r.lh).expect("行末必有位置").x);
            assert!(
                (x - x0).abs() < 0.5,
                "第 {i} 可视行高亮应从行首起：x={x:.1} 期望 {x0:.1}"
            );
            assert!(
                (w - full_w).abs() < 1.0,
                "第 {i} 可视行高亮应整行宽：w={w:.1} 期望 {full_w:.1}"
            );
            let ey = y0 + f32::from(r.lh) * i as f32;
            assert!(
                (y - ey).abs() < 0.5,
                "第 {i} 可视行高亮 y 应递增 lh：y={y:.1} 期望 {ey:.1}"
            );
        }
    });
}

/// 长无序列表项（`-`）同样整行高亮（gutter 同为 18px 路径）。
#[gpui::test]
fn long_unordered_full_line_selection_highlights_every_wrapped_line(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    use super::{text_row_shift, text_selection_hl_rects, text_y_off};
    cx.update(crate::actions::bind_local_keymaps);
    let item = "- 无序列表项 B testtesttesttesttesttesttesttesttesttesttesttesttesttest "
        .repeat(6)
        + "end\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&item));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
        let r = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Bullet)
            .expect("应有无序列表行");
        let w0 = r.wrapped.first().expect("文本行必有首 shard");
        let spans = visual_spans(w0);
        assert!(spans.len() >= 3, "长无序项应折成 ≥3 可视行");
        let ls = ed.read(cx).line_start(r.src_line);
        let le = ed.read(cx).line_end(r.src_line);
        let rects = text_selection_hl_rects(
            ed.read(cx),
            r,
            &(ls..le),
            f32::from(frame.padding),
            0.,
        );
        let x0 = line_start_x(r, &frame);
        let y0 = f32::from(r.top) + text_y_off(r) + text_row_shift(r);
        assert_eq!(rects.len(), spans.len());
        for (i, (rect, (_, b2))) in rects.iter().zip(&spans).enumerate() {
            let (x, y, w) = *rect;
            let full_w = f32::from(w0.position_for_index(*b2, r.lh).expect("行末必有位置").x);
            assert!((x - x0).abs() < 0.5, "第 {i} 行应从行首起：x={x:.1} 期望 {x0:.1}");
            assert!((w - full_w).abs() < 1.0, "第 {i} 行应整行宽：w={w:.1} 期望 {full_w:.1}");
            let ey = y0 + f32::from(r.lh) * i as f32;
            assert!((y - ey).abs() < 0.5, "第 {i} 行 y 应递增 lh：y={y:.1} 期望 {ey:.1}");
        }
    });
}

/// 短列表项（单可视行）全选：仍是单条、从行首到行尾（行为与修复前一致）。
#[gpui::test]
fn short_item_full_line_selection_single_row(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    use super::{text_row_shift, text_selection_hl_rects, text_y_off};
    cx.update(crate::actions::bind_local_keymaps);
    let item = "1. 短项\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&item));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let r = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Bullet)
            .expect("应有列表行");
        let w0 = r.wrapped.first().expect("单行也应有 shard");
        assert_eq!(w0.wrap_boundaries().len(), 0, "短项不折行");
        let ls = ed.read(cx).line_start(r.src_line);
        let le = ed.read(cx).line_end(r.src_line);
        let rects = text_selection_hl_rects(
            ed.read(cx),
            r,
            &(ls..le),
            f32::from(frame.padding),
            0.,
        );
        assert_eq!(rects.len(), 1, "短项全选 → 一条高亮");
        let (x, y, w) = rects[0];
        let x0 = line_start_x(r, &frame);
        assert!((x - x0).abs() < 0.5, "单行从行首起：x={x:.1} 期望 {x0:.1}");
        let full_w = f32::from(w0.position_for_index(w0.len(), r.lh).expect("行末有位置").x);
        assert!((w - full_w).abs() < 1.0, "单行整行宽：w={w:.1} 期望 {full_w:.1}");
        let ey = f32::from(r.top) + text_y_off(r) + text_row_shift(r);
        assert!((y - ey).abs() < 0.5, "单行 y={y:.1} 期望 {ey:.1}");
    });
}

/// 长有序项「第 2 可视行中部开始」的部分选区：首可视行无高亮、起始行右截（x>行首、
/// 宽<整行、y 落在第 2 行），其后各行整行。覆盖左缘 `c0 > a`（行内起点）分支。
#[gpui::test]
fn partial_selection_on_wrapped_line_clips_midline(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    use super::{text_row_shift, text_selection_hl_rects, text_y_off};
    cx.update(crate::actions::bind_local_keymaps);
    let item = format!("1. {}\n", "testtesttesttesttesttesttesttesttesttesttesttesttesttest ".repeat(5) + "end");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&item));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
        let r = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Bullet)
            .expect("应有列表行");
        let w0 = r.wrapped.first().expect("文本行必有首 shard");
        let spans = visual_spans(w0);
        assert!(spans.len() >= 3, "需要 ≥3 可视行");
        // 源行首 3 字节 = `1. ` 标记；选区从第 2 可视行中部（显示索引）开始。
        let c0_disp = spans[0].1 + 3;
        let ls = ed.read(cx).line_start(r.src_line);
        let le = ed.read(cx).line_end(r.src_line);
        let from = ls + 3 + c0_disp;
        let rects = text_selection_hl_rects(
            ed.read(cx),
            r,
            &(from..le),
            f32::from(frame.padding),
            0.,
        );
        assert!(
            rects.len() == spans.len().saturating_sub(1),
            "首可视行不覆盖，其余每行一条：{}",
            rects.len()
        );
        let x0 = line_start_x(r, &frame);
        let y0 = f32::from(r.top) + text_y_off(r) + text_row_shift(r);
        // 起始行（原第 2 可视行）：x 在行内、宽为右截、y 落在第 2 行（lh 一行之下）。
        let p_start = w0.position_for_index(c0_disp, r.lh).expect("行内起点必有位置");
        let full_w_line1 = f32::from(
            w0.position_for_index(spans[1].1, r.lh)
                .expect("行末有位置")
                .x,
        );
        let (x, y, w) = rects[0];
        assert!(
            (x - (x0 + f32::from(p_start.x))).abs() < 0.5,
            "起始行左缘应为行内起点：x={x:.1} 期望 {:.1}",
            x0 + f32::from(p_start.x)
        );
        assert!(
            (w - (full_w_line1 - f32::from(p_start.x))).abs() < 1.0,
            "起始行宽应为右截：w={w:.1} 期望 {:.1}",
            full_w_line1 - f32::from(p_start.x)
        );
        let ey = y0 + f32::from(r.lh);
        assert!((y - ey).abs() < 0.5, "起始行 y 应落在第 2 可视行：y={y:.1} 期望 {ey:.1}");
        // 其后各行整行宽。
        for (i, rect) in rects.iter().enumerate().skip(1) {
            let full_w = f32::from(
                w0.position_for_index(spans[i + 1].1, r.lh)
                    .expect("行末有位置")
                    .x,
            );
            assert!(
                (rect.2 - full_w).abs() < 1.0,
                "第 {} 续行应整行宽：w={:.1} 期望 {full_w:.1}",
                i,
                rect.2
            );
        }
    });
}

/// 真实渲染路径冒烟：长有序列表项设源选区后走真实 `window.draw`（`paint_selection`
/// 与搜索/注记加亮共用，此分支以编辑器选区调用）不 panic，并命中全部可视行。
#[gpui::test]
fn long_ordered_selection_paints_via_real_path(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    use super::text_selection_hl_rects;
    cx.update(crate::actions::bind_local_keymaps);
    let item = "1. 有序列表项 A testtesttesttesttesttesttesttesttesttesttesttesttest "
        .repeat(6)
        + "end\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&item));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| {
            let ls = e.line_start(0);
            let le = e.line_end(0);
            e.begin_selection_at(ls);
            e.extend_selection_to(le);
            assert_eq!(e.selection(), Some(ls..le), "长列表行整行源选区");
        });
        // 真实绘制路径（选区经由 paint_selection 画背景）不应 panic。
        let f_smoke = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(f_smoke));
        });
        let _ = window.draw(cx);
        // 高亮几何 = 可视行数（第 0 行即长列表行）。
        let f = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
        let r = f
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Bullet)
            .expect("应有列表行");
        let rects = text_selection_hl_rects(
            ed.read(cx),
            r,
            &ed.read(cx).selection().unwrap(),
            f32::from(f.padding),
            0.,
        );
        let vlines = r.wrapped.first().expect("shard").wrap_boundaries().len() + 1;
        assert_eq!(rects.len(), vlines, "真实路径选区高亮应逐可视行");
    });
}
