// 引号区内块级结构（2026-11-13）的 WYSIWYG 层回归：
// - 引用内表格按 `Table` 渲染、单元格可编辑、边界守卫/回车 no-op 与顶层一致；
// - 引用内回车自动续 `>` 前缀（含列表 marker / 嵌套深度）；
// - 引用内列表行显示项目符号 gutter 并带引述缩进。

#[gpui::test]
fn quoted_table_renders_editable_with_prefix_aware_hit(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "> | A | B |\n> | --- | --- |\n> | 1 | 2 |\n> | 3 | 4 |\n尾\n";
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
            .expect("应有引用内表格（多行 `>` 源行 → 一个 Table 块）");
        assert!(row.editable, "引用内表格单元格应可编辑");
        assert!(row.quoted, "引用内表格应标记 quoted（paint 画灰底带+左竖条）");
        assert_eq!(row.x_off, px(15.), "引用内表格应带引述缩进");
        let t = row.table.as_ref().expect("表格渲染数据");
        let te = row.table_edit.as_ref().expect("表格编辑排版");
        let rh = f32::from(t.row_h);
        let pad = f32::from(frame.padding);
        let xoff = f32::from(row.x_off);
        let top = f32::from(row.top);
        // 点击第 1 数据行第 1 格（源含 `> ` 前缀）→ 命中字符 '1'（前缀偏移不被算进格内容）。
        let row_ix = 1usize;
        let cell_ix = 0usize;
        let tx = te.rows[row_ix].cells[cell_ix].text_x;
        let x = pad + xoff + table_cell_x0(&t.widths, cell_ix) + tx + 1.;
        let y = top + table_row_top(t, row_ix) + rh / 2.;
        let off = source_offset_at_local(ed.read(cx), &frame, point(px(x), px(y)));
        let c = ed.read(cx).text().as_ref()[off..].chars().next().unwrap_or('_');
        assert_eq!(c, '1', "引用内表格首格应命中 '1'（hit 剥离 `> ` 前缀）");
    });
}

#[gpui::test]
fn quoted_table_cell_editing_guards_and_enter_noop(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "> | 1 | 2 |\n> | --- | --- |\n> | a | b |\n尾\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            // 数据行 "> | 1 | 2 |"（源行 0）：内容 "1" 在行内字节 4..5（`> `+`| `=4）。
            let ls = e.line_start(0);
            e.set_caret(ls + 4);
            e.replace_range(e.caret()..e.caret(), "X", cx);
            assert!(
                e.text().as_ref().contains("> | X1 | 2 |"),
                "格内输入应生效：{}",
                e.text().as_ref()
            );
            // 格内容起点退格 → 守卫（不吞 `>`/`| `）。
            e.set_caret(ls + 4);
            e.backspace(cx);
            assert!(
                e.text().as_ref().contains("> | X1 | 2 |"),
                "格起点退格应被守卫：{}",
                e.text().as_ref()
            );
            // 格内容尾删除 → 守卫（不吞 `|`）。
            e.set_caret(ls + 6);
            e.delete(cx);
            assert!(
                e.text().as_ref().contains("> | X1 | 2 |"),
                "格内容尾删除应被守卫：{}",
                e.text().as_ref()
            );
            // 表格内回车 → no-op（不拆表、不续前缀）。
            let before = e.text().as_ref().to_string();
            e.set_caret(ls + 5);
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
fn enter_in_quote_line_continues_prefix_and_list(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    // 普通引用行：回车 → 自动续 `> `。
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("> 引用行\n尾\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            e.set_caret(e.line_end(0));
            e.newline(cx);
            assert_eq!(
                e.text().as_ref(),
                "> 引用行\n> \n尾\n",
                "引用内回车应续 `> `：{}",
                e.text().as_ref()
            );
        });
    });

    // 引号内列表行：回车 → 续 `> ` + 列表 marker `- `。
    let (host2, vcx2) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("> - 列表\n尾\n"));
        WyHost { ed }
    });
    let vcx2: &mut VisualTestContext = vcx2;
    vcx2.update(|_, cx| {
        let ed = host2.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            e.set_caret(e.line_end(0));
            e.newline(cx);
            assert_eq!(
                e.text().as_ref(),
                "> - 列表\n> - \n尾\n",
                "引号内列表回车应续 `> - `：{}",
                e.text().as_ref()
            );
        });
    });

    // 嵌套引用：回车 → 续原深度 `>> `。
    let (host3, vcx3) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(">> 深层\n尾\n"));
        WyHost { ed }
    });
    let vcx3: &mut VisualTestContext = vcx3;
    vcx3.update(|_, cx| {
        let ed = host3.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            e.set_caret(e.line_end(0));
            e.newline(cx);
            assert_eq!(
                e.text().as_ref(),
                ">> 深层\n>> \n尾\n",
                "嵌套引用回车应续 `>> `：{}",
                e.text().as_ref()
            );
        });
    });

    // 普通段落回车：不续任何前缀（行为不变）。
    let (host4, vcx4) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("普通段落\n尾\n"));
        WyHost { ed }
    });
    let vcx4: &mut VisualTestContext = vcx4;
    vcx4.update(|_, cx| {
        let ed = host4.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            e.set_caret(e.line_end(0));
            e.newline(cx);
            assert_eq!(
                e.text().as_ref(),
                "普通段落\n\n尾\n",
                "普通段落回车不续前缀：{}",
                e.text().as_ref()
            );
        });
    });
}

#[gpui::test]
fn quoted_bullet_shows_gutter_and_quote_indent(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "> - 列表项\n> - 第二项\n尾\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let bullets: Vec<_> = frame
            .rows
            .iter()
            .filter(|r| r.kind == IrKind::Bullet)
            .collect();
        assert_eq!(bullets.len(), 2, "应有 2 个引用内列表行");
        assert!(bullets[0].quoted, "引用内列表行应标记 quoted（画灰底带+左竖条）");
        // 项目符号 gutter（列表项标记可见）+ 引述缩进。
        assert!(
            bullets[0].extra.gutter.is_some(),
            "引用内列表应显示项目符号 gutter"
        );
        assert!(
            f32::from(bullets[0].x_off) > 15.,
            "引用内列表行应带引述缩进（gutter+引述）"
        );
    });
}

// 真机复查 #1/#2/#3（2026-11-13）：引用内**超宽**表格的横向滚动条几何必须与
// 表格内容左缘对齐（`padding + x_off`），滚动上限按「收窄视口」计价（右缘可达），
// 内容遮罩不盖引号竖条。
#[gpui::test]
fn quoted_wide_table_hscroll_aligns_with_table_edge(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "> | 列A | 列B | 列C | 列D | 列E |\n> | --- | --- | --- | --- | --- |\n> | 内容甲 | 内容乙 | 内容丙 | 内容丁 | 内容戊 |\n尾\n";
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
            .expect("应有引用内表格");
        let geo = super::table_hscroll_geo(ed.read(cx), &frame, row)
            .expect("超宽表格应有横向滚动条");
        let (bar_x, bar_w, _, _, max_scroll) = geo;
        let xoff = f32::from(row.x_off);
        let pad = f32::from(frame.padding);
        // #1：滚动条左缘对齐表格内容左缘（`padding + x_off`），不抵引号竖条。
        assert!(
            (bar_x - (pad + xoff)).abs() < 0.01,
            "滚动条应随表格内容左缘对齐：bar_x={bar_x} 期望={}",
            pad + xoff
        );
        // #3：视口口径 = 宽 − 2×pad − x_off，滚动上限使最右列可达。
        let viewport = (600. - 2. * pad - xoff).max(1.);
        let total = row.table.as_ref().unwrap().total_w;
        let expect = (total - viewport).max(0.);
        assert!(
            (max_scroll - expect).abs() < 0.01,
            "滚动上限应含收窄视口：max_scroll={max_scroll} 期望={expect}"
        );
        assert!(bar_w > 0. && bar_w <= 600. - 2. * pad, "滚动条宽应有界");
    });
}
