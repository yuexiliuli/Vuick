/// B3 增量派生：结构编辑（Enter）后 math 区域**行号平移**而非整篇重扫——编辑线之上
/// 不变、之下平移 Δ；无区域跨线 → `regions_ver` 不递增（不重建 derived）；区域跨线
/// （编辑落在 $$ 块内）→ 回退整篇重建（regions_ver++）。
#[gpui::test]
fn structural_enter_shifts_derived_math_regions(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "para one\n$$\n\\int_a b dx\n$$\npara two\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let _f = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let ranges = |e: &gpui::Entity<WyEditor>, cx: &App| -> Vec<std::ops::Range<usize>> {
            e.read(cx)
                .derived()
                .borrow()
                .math_regions
                .iter()
                .map(|r| r.range.clone())
                .collect()
        };
        assert_eq!(ranges(&ed, cx), vec![1usize..4], "初始 $$ 块占行 1..4");
        let v0 = ed.read(cx).regions_ver();
        // 行 0（para one）末按 Enter → 整块 $$ 下移一行；无跨线 → 增量平移。
        let at = ed.read(cx).text().as_ref().find('\n').unwrap();
        ed.update(cx, |e, ecx| e.replace_range(at..at, "\n", ecx));
        assert_eq!(
            ranges(&ed, cx),
            vec![2usize..5],
            "Enter 后 $$ 块整体下移一行"
        );
        assert_eq!(
            ed.read(cx).regions_ver(),
            v0,
            "无区域跨线 → 只用行号平移，不重建派生（B3 增量）"
        );
        // 结构编辑落在 $$ 块内（跨线）→ 回退整篇重建（区域内容变了无法纯平移）。
        let at2 = ed.read(cx).text().as_ref().find("\\int_a").unwrap() + 3;
        let v1 = ed.read(cx).regions_ver();
        ed.update(cx, |e, ecx| e.replace_range(at2..at2, "\n", ecx));
        assert_eq!(
            ed.read(cx).regions_ver(),
            v1.wrapping_add(1),
            "结构编辑落在 $$ 块内 → 区域内容变化 → 整篇重建派生"
        );
    });
}

/// B2 撤销事务合并：连续键入（同意图 + 邻接）并入一个撤销单元；光标移动打断
/// 组（移动后键入独立单元）；Enter/纸类编辑为原子单元。
#[gpui::test]
fn undo_b2_groups_typing_and_breaks_on_caret(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(""));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 连续键入 abc → 一个撤销单元。
        for ch in ["a", "b", "c"] {
            ed.update(cx, |e, ecx| {
                let at = e.caret();
                e.replace_range(at..at, ch, ecx);
            });
        }
        assert_eq!(ed.read(cx).text().as_ref(), "abc");
        ed.update(cx, |e, ecx| e.undo(ecx));
        assert_eq!(
            ed.read(cx).text().as_ref(),
            "",
            "连续键入一次撤销应整组回退（B2 合并）"
        );
        // 重做回 abc。
        ed.update(cx, |e, ecx| e.redo(ecx));
        assert_eq!(ed.read(cx).text().as_ref(), "abc");
        // 光标移动 → 打断：移动后键入 z 是独立撤销单元。
        ed.update(cx, |e, ecx| {
            e.set_caret(0);
            e.replace_range(0..0, "z", ecx);
        });
        assert_eq!(ed.read(cx).text().as_ref(), "zabc");
        ed.update(cx, |e, ecx| e.undo(ecx));
        assert_eq!(ed.read(cx).text().as_ref(), "abc", "z 独立撤销");
        ed.update(cx, |e, ecx| e.undo(ecx));
        assert_eq!(
            ed.read(cx).text().as_ref(),
            "",
            "光标移动后 z 与 abc 分属两个单元"
        );
    });
}

/// B2 撤销合并：连续退格经 `replace_range`（纯删除、首尾相接）合并成一个单元。
#[gpui::test]
fn undo_b2_groups_backspaces(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("abcdef"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, ecx| {
            e.set_caret(6);
            // 连续退格 3 个字符（6→5, 5→4, 4→3）。
            for _ in 0..3 {
                let at = e.caret();
                e.replace_range(at - 1..at, "", ecx);
            }
        });
        assert_eq!(ed.read(cx).text().as_ref(), "abc");
        // 一次撤销应还原全部 3 次退格。
        ed.update(cx, |e, ecx| e.undo(ecx));
        assert_eq!(
            ed.read(cx).text().as_ref(),
            "abcdef",
            "相邻退格合并成一个撤销单元"
        );
    });
}

/// 逆操作撤销：**结构性**编辑（Enter 拆段）撤销走增量 pass1，块结构与内容全还原。
#[gpui::test]
fn undo_after_enter_restores_merged_paragraph(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("abc\ndef"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let n_before = ed.read(cx).doc().block_count();
        ed.update(cx, |e, ecx| {
            e.set_caret(3);
            e.replace_range(3..3, "\n", ecx); // 拆成 `abc` / `def` 两段（结构编辑）
        });
        assert_eq!(ed.read(cx).text().as_ref(), "abc\n\ndef");
        ed.update(cx, |e, ecx| e.undo(ecx));
        assert_eq!(
            ed.read(cx).text().as_ref(),
            "abc\ndef",
            "撤销结构编辑应还原文本"
        );
        assert_eq!(
            ed.read(cx).doc().block_count(),
            n_before,
            "撤销结构编辑应还原块结构"
        );
        ed.update(cx, |e, ecx| e.redo(ecx));
        assert_eq!(ed.read(cx).text().as_ref(), "abc\n\ndef");
    });
}

/// 逆操作撤销：**原子**替换（选区替换为文本）撤销换回被删原文（Atomic 独立单元）。
#[gpui::test]
fn undo_selection_replace_restores_removed(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("hello world"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 假装选区替换：6..11 "world" → "rust"（removed 非空 + 插入 → Atomic）。
        ed.update(cx, |e, ecx| {
            e.set_caret(11);
            e.replace_range(6..11, "rust", ecx);
        });
        assert_eq!(ed.read(cx).text().as_ref(), "hello rust");
        ed.update(cx, |e, ecx| e.undo(ecx));
        assert_eq!(
            ed.read(cx).text().as_ref(),
            "hello world",
            "撤销应还原被替换原文"
        );
        ed.update(cx, |e, ecx| e.redo(ecx));
        assert_eq!(ed.read(cx).text().as_ref(), "hello rust");
    });
}

/// 逆操作撤销：大块粘帖（结构多段）撤销/重做内容完整（净逆操作按坐标精确回退）。
#[gpui::test]
fn undo_redo_large_paste_circle(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("start\nend\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let paste = (0..300)
            .map(|i| format!("para {i} xyz\n"))
            .collect::<String>();
        ed.update(cx, |e, ecx| {
            e.set_caret("start\n".len());
            e.replace_range("start\n".len().."start\n".len(), &paste, ecx);
        });
        let after_paste = ed.read(cx).text().as_ref().to_string();
        ed.update(cx, |e, ecx| e.undo(ecx));
        assert_eq!(
            ed.read(cx).text().as_ref(),
            "start\nend\n",
            "撤销应完整移除大块粘帖"
        );
        ed.update(cx, |e, ecx| e.redo(ecx));
        assert_eq!(
            ed.read(cx).text().as_ref(),
            &after_paste,
            "重做应完整复原大块粘帖"
        );
    });
}

