#[gpui::test]
fn table_click_lands_in_correct_cell(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Table)
            .expect("应有表格");
        assert!(row.editable, "表格单元格应可编辑");
        let t = row.table.as_ref().expect("表格渲染数据");
        let te = row.table_edit.as_ref().expect("表格编辑排版");
        let rh = f32::from(t.row_h);
        let pad = f32::from(frame.padding);
        let top = f32::from(row.top);
        // (可见行, 列, 期望字符)：表头第一格 A；数据行2(源行3) 两格 3/4。
        for (row_ix, cell_ix, want) in [(0usize, 0usize, 'A'), (2, 0, '3'), (2, 1, '4')] {
            let tx = te.rows[row_ix].cells[cell_ix].text_x;
            let y = top + table_row_top(t, row_ix) + rh / 2.;
            let x = pad + table_cell_x0(&t.widths, cell_ix) + tx + 1.;
            let off = source_offset_at_local(ed.read(cx), &frame, point(px(x), px(y)));
            let c = ed.read(cx).text().as_ref()[off..]
                .chars()
                .next()
                .unwrap_or('_');
            assert_eq!(c, want, "单元格 (行{row_ix},列{cell_ix}) 应命中字符 {want}");
        }
    });
}

#[gpui::test]
fn table_caret_aligns_with_clicked_cell(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Table)
            .expect("应有表格");
        let t = row.table.as_ref().unwrap();
        let te = row.table_edit.as_ref().unwrap();
        let rh = f32::from(t.row_h);
        let pad = f32::from(frame.padding);
        let top = f32::from(row.top);
        // 数据第 2 行第 2 格（"4"@源行3）：点击 → set_caret → caret x 应贴合点击 x。
        let cell_ix = 1usize;
        let row_ix = 2usize;
        let tx = te.rows[row_ix].cells[cell_ix].text_x;
        let x = pad + table_cell_x0(&t.widths, cell_ix) + tx + 20.;
        let y = top + table_row_top(t, row_ix) + rh / 2.;
        let off = source_offset_at_local(ed.read(cx), &frame, point(px(x), px(y)));
        ed.update(cx, |e, _| e.set_caret(off));
        let (on, caretx, carety) = caret_geometry(ed.read(cx), &frame);
        assert!(on, "表格单元格应可放光标");
        assert!(
            (f32::from(caretx) - x).abs() < 12.,
            "光标 x {} 应贴合点击 x {}（<1 字宽）",
            f32::from(caretx),
            x
        );
        let row_top_y = top + table_row_top(t, row_ix);
        assert!(
            f32::from(carety) >= row_top_y - 0.5 && f32::from(carety) <= row_top_y + rh + 0.5,
            "光标 y {} 应在点击行带 [{row_top_y}, {}] 内",
            f32::from(carety),
            row_top_y + rh
        );
    });
}

#[gpui::test]
fn table_widths_derive_from_reused_cell_shapes(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::actions::bind_local_keymaps);
    // 列宽必须**恰由数据行排版所用的同一 shape 产物 `line.width()`** 决定（单次 shape 复用
    // 契约：测宽那次 shape 的行被数据行排版直接搬用，不二次 shape）。数据行比表头宽 →
    // 列宽不受表头常规测宽主导，断言可精确到位。
    let text = "| H | I |\n| --- | --- |\n| alpha | beta |\n| extremely-long-data | xyz |\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Table)
            .expect("应有表格");
        let t = row.table.as_ref().expect("表格渲染数据");
        let te = row.table_edit.as_ref().expect("表格编辑排版");
        for ci in 0..t.widths.len() {
            // 数据行（跳过表头 row0）逐格：该列最宽数据格 `line.width()`+20；与列宽 48 下限取大。
            let widest = te
                .rows
                .iter()
                .skip(1)
                .filter_map(|r| r.cells.get(ci))
                .map(|c| f32::from(c.line.width()) + 20.)
                .fold(0f32, f32::max)
                .max(48.);
            assert!(
                (t.widths[ci] - widest).abs() < 0.01,
                "列 {ci} 宽 {:.1} 应恰好 = 数据格 shape 产物宽最大值+20（与 48 下限取大）{:.1}",
                t.widths[ci],
                widest
            );
        }
    });
}

#[gpui::test]
fn all_text_rows_wrap_inside_content_box(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::actions::bind_local_keymaps);
    // 回归（右侧留白对称的根因）：任意文本行（段落/标题/项目符号/引述/警报）的**折行宽**
    // 必须 = 帧宽 − 帧内衬×2 − 本行 gutter(x_off)，使「padding + x_off + 折行宽」恰等于
    // 内容盒右缘（`frame.width − padding`）。修复前正文折行拿 `frame.width`（只裁左内衬），
    // 带 gutter 的行右缘越出内容盒被面板/滚动条裁——右侧留白比左侧小一个内衬/gutter。
    // 与预览 `markdown::view` 的 `avail` 语义一致（命中/光标/划线同源）。
    let text = format!(
        "# {}\n\n- {}\n\n> [!NOTE] {}\n> {}\n\n> {}\n\n{}\n",
        "很长标题".repeat(30),
        "很长的项目符号内容".repeat(30),
        "很长 callout 标题".repeat(30),
        "callout 正文".repeat(30),
        "很长引述".repeat(30),
        "很长段落".repeat(30),
    );
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            for w_px in [300.0, 500.0, 800.0] {
                let frame = build_frame(ed.read(cx), window, px(w_px), (0., 999_999.));
                let pad = f32::from(frame.padding);
                // 内容盒右缘 = 帧右缘 − 帧内衬（左内衬由绘制起点 `padding + x_off` 保证）。
                let content_right = f32::from(frame.width) - pad;
                for row in frame.rows.iter() {
                    if !matches!(
                        row.kind,
                        IrKind::Paragraph
                            | IrKind::Heading(_)
                            | IrKind::Bullet
                            | IrKind::Quote
                    ) {
                        continue;
                    }
                    let off = f32::from(row.x_off);
                    for wl in row.wrapped.iter() {
                        let right = off + f32::from(wl.width());
                        assert!(
                            right <= content_right + 0.6,
                            "帧宽 {w_px:.0}px {:?} 行右缘应 ≤ 内容盒右缘（{right:.1} > {content_right:.1}，x_off={off:.1}）",
                            row.kind
                        );
                    }
                }
            }
        });
}

#[gpui::test]
fn table_cell_edit_typing_and_boundary_guards(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let text = "| 1 | 2 |\n| --- | --- |\n| a | b |\n尾\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            // 在数据行第一格 "a"（源行 2，位 2）内输入 X。
            let ls = e.line_start(2);
            e.set_caret(ls + 2);
            e.replace_range(e.caret()..e.caret(), "X", cx);
            assert!(
                e.text().as_ref().contains("| Xa | b |"),
                "格内输入生效：{}",
                e.text().as_ref()
            );
            // 退格在格内容起点（'X'）→ no-op（守卫 `| ` 不被吃掉）。
            e.set_caret(ls + 2);
            e.backspace(cx);
            assert!(
                e.text().as_ref().contains("| Xa | b |"),
                "格起点退格应被守卫：{}",
                e.text().as_ref()
            );
            // 删除在格内容尾 → no-op（守卫 `|` 不被吃掉）。
            let ls2 = e.line_start(2);
            e.set_caret(ls2 + 4);
            e.delete(cx);
            assert!(
                e.text().as_ref().contains("| Xa | b |"),
                "格内容尾删除应被守卫：{}",
                e.text().as_ref()
            );
            // 表格内回车 → no-op（不拆表）。
            let before = e.text().as_ref().to_string();
            e.set_caret(ls2 + 3);
            e.newline(cx);
            assert_eq!(
                e.text().as_ref(),
                &before,
                "表格内回车应 no-op：{}",
                e.text().as_ref()
            );
        });
    });
}

#[gpui::test]
fn table_selection_highlight_paints_and_intersects_cells(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Table)
            .expect("应有表格");
        let te = row.table_edit.as_ref().expect("表格编辑排版");
        // 源行：表头(0)、分隔(1)、数据(2~3)、段(4)。全选表头 + 两数据行。
        ed.update(cx, |e, _| {
            e.begin_selection_at(e.line_start(0));
            e.extend_selection_to(e.line_end(3));
        });
        let sel = ed.read(cx).selection().expect("应有选区");
        // 每个可见单元格源区间均与选区相交（选区盖住整个表格 → 2 列 × 3 行 = 6 格）。
        let mut hit = 0usize;
        for row_l in &te.rows {
            let ls = ed.read(cx).line_start(row_l.src_line);
            for cell in &row_l.cells {
                if cell_selection_display(&cell.map, cell.line.len(), cell.src.clone(), ls, &sel)
                    .is_some()
                {
                    hit += 1;
                }
            }
        }
        assert_eq!(hit, 6, "2 列 × 3 可见行 = 6 格全部与选区相交");
        // 带选区注入帧并触发一次真实绘制（element 的 paint 会调用 paint_selection，
        // 表格分支不 panic 即通过）。
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
        });
        let _ = window.draw(cx);
    });
}

