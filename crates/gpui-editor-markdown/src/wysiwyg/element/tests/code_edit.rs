#[gpui::test]
fn selection_drag_delete_and_select_all(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("一二三五六"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            e.begin_selection_at(0);
            e.extend_selection_to("一二三".len());
            assert_eq!(e.selection(), Some(0.."一二三".len()), "拖选得源字节区间");
            // 输入/删除应作用于选区。
            e.backspace(cx);
            assert_eq!(e.text().as_ref(), "五六", "Backspace 应删除整段选区");
            assert_eq!(e.selection(), None);
            // 全选 + 范围替换。
            e.select_all();
            assert_eq!(e.selection(), Some(0.."五六".len()));
            e.replace_range(e.selection().unwrap(), "你", cx);
            assert_eq!(e.text().as_ref(), "你", "全选范围替换");
        });
    });
}

#[gpui::test]
fn readonly_gates_all_edit_paths(cx: &mut TestAppContext) {
    // A1：`set_editable(false)` 后一切内容变更路径短路（输入/IME/退格/删除/Enter/
    // 撤销/重做/范围替换），切回 true 恢复编辑。
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("abc"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            e.set_editable(false);
            assert!(!e.is_editable(), "只读门控应生效");
            e.set_caret(1);
            // IME 组合（`replace_and_mark_utf16` 底层走 `replace_range`，被硬门控）。
            e.replace_and_mark_utf16(None, "X", None, cx);
            assert_eq!(e.text().as_ref(), "abc", "只读 IME 组合应短路");
            assert!(e.marked.is_none(), "只读 IME 不应留下组合态");
            // 平台直输（`replace_text_utf16`）。
            e.replace_text_utf16(Some(1..2), "X", cx);
            assert_eq!(e.text().as_ref(), "abc", "只读直输应短路");
            // 范围替换 / 退格 / 删除 / Enter。
            e.replace_range(1..1, "X", cx);
            assert_eq!(e.text().as_ref(), "abc", "只读 replace_range 应短路");
            e.backspace(cx);
            e.delete(cx);
            e.newline(cx);
            assert_eq!(e.text().as_ref(), "abc", "只读退格/删除/换行应短路");
            // 撤销/重做（走 `apply_raw_edit`，需单独门控）。
            e.undo(cx);
            e.redo(cx);
            assert_eq!(e.text().as_ref(), "abc", "只读撤销/重做应短路");
            // 切回可编辑 → 恢复正常编辑。
            e.set_editable(true);
            e.replace_range(1..1, "X", cx);
            assert_eq!(e.text().as_ref(), "aXbc", "切回可编辑应恢复范围替换");
        });
    });
}

#[gpui::test]
fn readonly_keeps_selection_and_caret_moves(cx: &mut TestAppContext) {
    // A1：只读下保留光标移动与点选/拖选（复制依赖它们）；只没收拢到光标的旧选区。
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("hello world"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _cx| {
            e.set_caret(5);
            assert_eq!(e.caret(), 5, "只读下仍可移动光标");
            // 拖选 / 点选仍有效（复制读 `selected_text`）。
            e.begin_selection_at(0);
            e.extend_selection_to(5);
            assert_eq!(e.selection(), Some(0..5), "只读下拖选应保留");
            assert_eq!(e.selected_text(), "hello", "只读下选中文本可读（复制用）");
            e.end_selection();
            e.select_all();
            assert_eq!(
                e.selection(),
                Some(0.."hello world".len()),
                "只读下全选应保留"
            );
            // 切只读会把残留活动选区收拢为光标（防止误导）。
            e.begin_selection_at(5);
            e.extend_selection_to(11);
            assert_eq!(e.selection(), Some(5..11), "基线拖选成立");
            e.set_editable(false);
            assert_eq!(
                e.selection(),
                None,
                "切入只读应收拢旧活动选区（无选区），光标仍可移"
            );
            e.set_caret(3);
            assert_eq!(e.caret(), 3, "只读下 set_caret 仍可用");
        });
    });
}
#[gpui::test]
fn quote_blocks_are_editable_with_indent(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("# 标题\n> 引用内容\n正文"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let quote = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Quote)
            .expect("应有引述行");
        assert!(quote.editable, "引述应可编辑（byte_map 非空）");
        assert!(f32::from(quote.x_off) > 0., "引述应有文本缩进");
        // 在引述渲染文本内落点 → 源字节在 “引用内容” 上，可正常输入。
        let off = ed.read(cx).text().as_ref().find("引").unwrap();
        ed.update(cx, |e, cx| {
            e.set_caret(off + 3);
            e.replace_range(off + 3..off + 6, "X", cx);
        });
        assert!(
            ed.read(cx).text().as_ref().contains("引X"),
            "引述行应可编辑：{}",
            ed.read(cx).text().as_ref()
        );
    });
}

#[gpui::test]
fn code_block_click_lands_on_correct_source_line(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "\
```rust
let a = 1;
let b = 22;
let c = 333;
```
尾段
";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let code = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Code)
            .expect("应有代码块");
        assert!(code.editable, "代码内容行应可编辑");
        let lh = f32::from(code.lh);
        let top = f32::from(code.top);
        let pad = f32::from(frame.padding);
        // 点击三个代码行的中部（y = 块首 + 首空白带 + k×行高）→ 逐行命中内容。
        for (k, expects) in [
            (0usize, "let a = 1;"),
            (1, "let b = 22;"),
            (2, "let c = 333;"),
        ] {
            let p = point(px(pad + 30.), px(top + lh * 1.5 + lh * k as f32));
            let off = source_offset_at_local(ed.read(cx), &frame, p);
            let (line, _) = ed.read(cx).row_col(off);
            assert_eq!(line, 1 + k, "点击第 {k} 个代码行应命中源行 {}", 1 + k);
            let ls = ed.read(cx).line_start(line);
            let le = ed.read(cx).line_end(line);
            assert_eq!(
                &ed.read(cx).text().as_ref()[ls..le],
                expects,
                "命中文本应为该行内容"
            );
        }
    });
}

#[gpui::test]
fn code_block_overflow_shows_hscroll_and_shifts_hit(cx: &mut TestAppContext) {
    // 长代码行（nowrap 超出窄面板）→ 底部横向滚动条；滚动偏移使**同一可视 x**
    // 命中更多源列（paint 平移的逆映射）；拖动滚动条更新偏移（视图本地，不写源码）。
    use crate::markdown::ir::IrKind;
    use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, size};
    cx.update(crate::test_support::bind_local_keymaps);
    let text = format!("```rust\n{}\nb\n```\n", "a".repeat(400));
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(600.)),
            )));
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let code = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Code && !r.multi_offsets.is_empty())
            .expect("应有代码块");
        let blk = code.src_line;
        let (bar_x, bar_w, _, _, max_scroll) =
            super::code_hscroll_geo(ed.read(cx), &frame, code).expect("长行应超宽显示滚动条");
        assert!(max_scroll > 50., "内容应显著超宽");
        assert!(bar_w > 10., "滚动条轨道应有宽度");
        // 首内容行中部 y（组 0 = 首空白带 + 首行）。
        let y = f32::from(code.top) + f32::from(code.lh) * 1.5;
        let view_x = f32::from(frame.padding) + f32::from(code.x_off) + 60.;
        let p = point(px(view_x), px(y));
        let off0 = super::source_offset_at_local(ed.read(cx), &frame, p);
        // 滚到最右（大偏移）→ 同一可视 x 命中更后的源列。
        ed.update(cx, |e, _| e.set_code_h_scroll(blk, max_scroll));
        let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let off1 = super::source_offset_at_local(ed.read(cx), &f2, p);
        let (_, col0) = ed.read(cx).row_col(off0);
        let (_, col1) = ed.read(cx).row_col(off1);
        assert!(
            col1 > col0,
            "横向滚动后同一 x 应命中更后源列：col0={col0} col1={col1}"
        );
        // 拖动滚动条：点击轨道 90% → 偏移大；拖到 10% → 偏移小；松开清除拖拽态。
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(f2))
        });
        let band_top =
            f32::from(code.top) + f32::from(code.height) - crate::config::layout::CODE_HSCROLL_BAND;
        let band_y = band_top + crate::config::layout::CODE_HSCROLL_BAND / 2.;
        let down_x = bar_x + bar_w * 0.9;
        let up_x = bar_x + bar_w * 0.1;
        let ev_down = MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(down_x), px(band_y)),
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        ed.update(cx, |e, cx| e.on_mouse_down(&ev_down, window, cx));
        let scrolled = ed.read(cx).code_h_scroll_of(blk);
        assert!(scrolled > 0., "按下轨道应产生滚动偏移：{scrolled}");
        let ev_move = gpui::MouseMoveEvent {
            position: point(px(up_x), px(band_y)),
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        };
        ed.update(cx, |e, cx| e.on_mouse_move(&ev_move, cx));
        let scrolled2 = ed.read(cx).code_h_scroll_of(blk);
        assert!(
            scrolled2 < scrolled,
            "拖到 10% 应减小偏移：{scrolled2} < {scrolled}"
        );
        let ev_up = MouseUpEvent {
            button: MouseButton::Left,
            position: point(px(up_x), px(band_y)),
            modifiers: Modifiers::default(),
            click_count: 1,
        };
        ed.update(cx, |e, cx| e.on_mouse_up(&ev_up, cx));
        assert!(
            ed.read(cx).code_h_drag.borrow().is_none(),
            "松开应清除拖拽态"
        );
        // 视图本地状态：源码未变。
        assert_eq!(ed.read(cx).text().as_ref(), text);
    });
}

