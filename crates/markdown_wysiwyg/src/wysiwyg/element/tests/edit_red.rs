// 红测（复现 agent 产出，2026-11-16）：只加失败测试证明两个 bug 存在，不修生产代码。
// 实现 agent 修绿后本文件即作回归。本文件被 include! 拼进 element::tests 单个模块，
// WyEditor / WyHost / TestAppContext 等名已由同模块早先文件（byte_map.rs 等）引入，
// 这里不再重复 use（重复同名 use 会 E0252）。

// Bug A：CRLF 跨行删 `\n` 留孤立 `\r` 污染源文本（edit.rs backspace/delete）。
// Bug B：selected_image 在键盘移动光标后残留 → 退格/删除误删整张图片行（edit.rs
//        backspace:1062-1064 / delete:1111-1113 先查 selected_image）。

/* ---------- Bug A：CRLF 跨行删 `\n` 留孤立 `\r` ---------- */

/// 红测：CRLF 文档，光标在**第二行行首**（CRLF 下 `line_start(1)` 在 `\r\n` 之后）退格
/// → 应把 `\r\n` 一起删掉合并行（`"第一行第二行"`），而非只删 `\n` 留下孤立 `\r`。
///
/// 机理：`first line_end(0)` 在 CRLF 下返回 `\n` 位置 10（docutil::line_end「CRLF 下返回
/// `\n`（含 `\r`）」）；光标在下一行行首 11 时，backspace 走 edit.rs:1087
/// `floor_char_boundary(11-1)=10` → `replace_range(10..11)` 只删 `\n`（字节 10），
/// `\r`（字节 9）残留 → 源文本 `"第一行\r第二行"` 污染。
#[gpui::test]
fn crlf_backspace_at_next_line_start_removes_full_break(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("第一行\r\n第二行"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|_, cx| {
        assert_eq!(ed.read(cx).content(), "第一行\r\n第二行", "前置：CRLF 源文本保留");
        // CRLF 下 line_end(0) = `\n` 位置（含 `\r`），下一行行首在其后。
        let at = ed.read(cx).line_start(1);
        assert_eq!(at, 11, "前置：第二行行首 = 11（`\\r`@9、`\\n`@10 之后）");
        ed.update(cx, |e, _c| e.set_caret(at));
        ed.update(cx, |e, c| e.backspace(c));
    });
    let out = vcx.update(|_, cx| ed.read(cx).content());
    assert_eq!(
        out,
        "第一行第二行",
        "CRLF 第二行行首退格应连 `\\r\\n` 一起删（不得留孤立 `\\r`），实际 {out:?}"
    );
}

/// 红测：CRLF 文档，光标在**第一行行尾**（CRLF 下 caret 落在 `\n` 位置 10）按 Delete
/// → 应把 `\r\n` 一起删掉合并行（`"第一行第二行"`），而非只删 `\n` 留下孤立 `\r`。
///
/// 机理：delete 走 edit.rs:1135 `ceil_char_boundary(10+1)=11` →
/// `replace_range(10..11)` 只删 `\n`，`\r`（字节 9）残留 → `"第一行\r第二行"` 污染。
#[gpui::test]
fn crlf_delete_at_line_end_removes_full_break(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("第一行\r\n第二行"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|_, cx| {
        assert_eq!(ed.read(cx).content(), "第一行\r\n第二行", "前置：CRLF 源文本保留");
        let at = ed.read(cx).line_end(0);
        assert_eq!(at, 10, "前置：CRLF 行 0 行尾 = `\\n` 位置 10（含 `\\r`）");
        ed.update(cx, |e, _c| e.set_caret(at));
        ed.update(cx, |e, c| e.delete(c));
    });
    let out = vcx.update(|_, cx| ed.read(cx).content());
    assert_eq!(
        out,
        "第一行第二行",
        "CRLF 行尾 Delete 应连 `\\r\\n` 一起删（不得留孤立 `\\r`），实际 {out:?}"
    );
}

/* ---------- Bug B：selected_image 移动光标后残留 → 误删图片行 ---------- */

/// 红测：点选独立图片行后**键盘下移**（`move_down`，Down 键路径）把光标移出图片行，
/// `selected_image` 必须被清除。当前 `move_*/set_caret`（core.rs:710 / edit.rs:1304-1467）
/// 只动 caret/sel_anchor，不清 `selected_image` → 残留。
#[gpui::test]
fn selected_image_cleared_after_keyboard_move(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![甲](a.png)\n正文内容abc\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|_, cx| {
        ed.update(cx, |e, c| e.select_image(0, c));
        assert_eq!(ed.read(cx).selected_image(), Some(0), "前置：点选图片后选中态");
        // 键盘 Down（move_down）→ 光标移到下一行。
        ed.update(cx, |e, _| e.move_down());
        assert!(
            ed.read(cx).selected_image().is_none(),
            "光标移出图片行后 selected_image 应被清除（当前残留 → Bug B）"
        );
    });
}

/// 红测：点选图片 → 键盘下移光标到下一行行首 → **Backspace**。正确行为 = 只删行首
/// 换行合并行（图片源 `![甲](a.png)` 保留成文本）；当前 edit.rs:1062-1064 先命中残留的
/// `selected_image` → delete_selected_image 删掉整张图片行。
#[gpui::test]
fn backspace_after_cursor_move_edits_text_not_image_line(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![甲](a.png)\n正文内容abc\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|_, cx| {
        ed.update(cx, |e, c| e.select_image(0, c));
        ed.update(cx, |e, _| e.move_down()); // 光标 → 第二行行首
        ed.update(cx, |e, c| e.backspace(c));
    });
    let out = vcx.update(|_, cx| ed.read(cx).content());
    assert_eq!(
        out,
        "![甲](a.png)正文内容abc\n",
        "移动光标后退格应只删行首换行（图片源保留），而非误删整张图片行，实际 {out:?}"
    );
}

/// 红测：点选图片 → 键盘下移光标到下一行行首 → **Delete**。正确行为 = 删光标处字符
/// （下一个字符 `正`），图片源保留；当前 edit.rs:1111-1113 先命中残留的 `selected_image`
/// → 误删整张图片行。
#[gpui::test]
fn delete_after_cursor_move_edits_text_not_image_line(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![甲](a.png)\n正文内容abc\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|_, cx| {
        ed.update(cx, |e, c| e.select_image(0, c));
        ed.update(cx, |e, _| e.move_down()); // 光标 → 第二行行首
        ed.update(cx, |e, c| e.delete(c));
    });
    let out = vcx.update(|_, cx| ed.read(cx).content());
    assert_eq!(
        out,
        "![甲](a.png)\n文内容abc\n",
        "移动光标后 Delete 应删光标处字符（图片源保留），而非误删整张图片行，实际 {out:?}"
    );
}

/// 红测（Bug B 行内图同款）：点选行内图后键盘下移（`move_down`），`selected_inline_image`
/// 必须被清除——防止键盘移动后 Backspace/Delete 命中残留的行内图选中误删整段 `![…](…)`。
/// 与块图 `selected_image` 在 `moved()` 统一清除（同一口径）。
#[gpui::test]
fn selected_inline_image_cleared_after_keyboard_move(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("甲 ![内联图块](a.png) 乙\n正文第二行\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let frame = inline_image_harness(vcx, &ed);
    let bb_local = first_inline_local(&frame);
    vcx.update(|window, cx| {
        let w = win_of(&ed, cx, &bb_local);
        let (px_, py_) = body_click_pt(&w);
        let down = img_down(px_, py_);
        ed.update(cx, |e, c| e.on_mouse_down(&down, window, c));
        assert!(
            ed.read(cx).selected_inline_image().is_some(),
            "前置：点行内图后选中态"
        );
        // 键盘 Down（move_down）→ 光标移到下一行。
        ed.update(cx, |e, _| e.move_down());
        assert!(
            ed.read(cx).selected_inline_image().is_none(),
            "光标移出行内图行后 selected_inline_image 应被清除（当前残留 → Bug B 行内图同款）"
        );
    });
}
