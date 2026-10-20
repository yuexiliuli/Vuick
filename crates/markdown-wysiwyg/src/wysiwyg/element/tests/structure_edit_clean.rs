/// 回归（真 bug）：结构编辑把一处块换成**同 src_line 的新块**时，以 src_line 为键的
/// 视图态（`*_h_scroll`/`mermaid_zoom`/`math_natural_w`）此前不清 → 旧块的横向滚动/
/// 缩放残留会误套到新块。这里走编辑管道真实路径：建代码块 → 设视图态非零 → 结构替换
/// 该块 → 断言同 src_line 的 5 类视图态全部清零（不误套）。
#[gpui::test]
fn structure_edit_clears_src_line_view_state(cx: &mut TestAppContext) {
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "前文\n```rust\nlet x = 1;\n```\n后文\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 旧代码块首源码行 = 1；先设非零视图态。
        ed.read(cx).set_code_h_scroll(1, 200.);
        ed.read(cx).set_table_h_scroll(1, 77.);
        ed.read(cx).set_math_h_scroll(1, 88.);
        ed.read(cx).set_math_natural_w(1, 4000.);
        ed.read(cx).set_mermaid_zoom(1, 2.0);
        assert_eq!(ed.read(cx).code_h_scroll_of(1), 200., "前置：滚动应先写入");
        // 结构替换：整段代码块（含围栏）换成另一个代码块（cross_newline + 围栏 → structural）。
        let src = ed.read(cx).content().find("```rust").unwrap();
        let end = ed.read(cx).content().len();
        ed.update(cx, |e, ecx| {
            e.replace_range(src..end, "```python\nprint(1)\n```\n", ecx);
        });
        // 新块落在同一 src_line=1 → 旧视图态必须清零，不得误套。
        assert_eq!(ed.read(cx).code_h_scroll_of(1), 0., "旧代码块横向滚动应清");
        assert_eq!(ed.read(cx).table_h_scroll_of(1), 0., "旧表格横向滚动应清");
        assert_eq!(ed.read(cx).math_h_scroll_of(1), 0., "旧数学横向滚动应清");
        assert_eq!(ed.read(cx).math_natural_w_of(1), 0., "旧数学自然宽应清");
        assert_eq!(ed.read(cx).mermaid_zoom(1), 1.0, "旧 mermaid 缩放应清");
    });
}

/// 映射层单测：`clear_src_line_view_state` 只清「结构编辑受影响行窗」内的键——窗内
/// 5 类键全清；窗外（块内容未变）保留，避免误清用户有意保留的滚动。
#[gpui::test]
fn clear_src_line_view_state_scopes_to_edit_window(cx: &mut TestAppContext) {
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("abc\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 窗内（edit_line=2, max_nl=1 → [2,3]）与窗外（0、5）都写入 5 类键。
        for l in [0usize, 2, 3, 5] {
            ed.read(cx).set_code_h_scroll(l, 100.);
            ed.read(cx).set_table_h_scroll(l, 200.);
            ed.read(cx).set_math_h_scroll(l, 300.);
            ed.read(cx).set_math_natural_w(l, 4000.);
            ed.read(cx).set_mermaid_zoom(l, 1.5);
        }
        // 触发被清路径（结构编辑清理入口调用同款）。
        ed.update(cx, |e, _| e.clear_src_line_view_state(2, 1));
        for l in [0usize, 5] {
            assert_eq!(ed.read(cx).code_h_scroll_of(l), 100., "{l} 窗外代码滚动保留");
            assert_eq!(ed.read(cx).table_h_scroll_of(l), 200., "{l} 窗外表格滚动保留");
            assert_eq!(ed.read(cx).math_h_scroll_of(l), 300., "{l} 窗外数学滚动保留");
            assert_eq!(ed.read(cx).math_natural_w_of(l), 4000., "{l} 窗外自然宽保留");
            assert_eq!(ed.read(cx).mermaid_zoom(l), 1.5, "{l} 窗外缩放保留");
        }
        for l in [2usize, 3] {
            assert_eq!(ed.read(cx).code_h_scroll_of(l), 0., "{l} 窗内代码滚动清");
            assert_eq!(ed.read(cx).table_h_scroll_of(l), 0., "{l} 窗内表格滚动清");
            assert_eq!(ed.read(cx).math_h_scroll_of(l), 0., "{l} 窗内数学滚动清");
            assert_eq!(ed.read(cx).math_natural_w_of(l), 0., "{l} 窗内自然宽清");
            assert_eq!(ed.read(cx).mermaid_zoom(l), 1.0, "{l} 窗内缩放清");
        }
    });
}
