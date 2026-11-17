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

/* ---------- Bug C3：非 IME 普通编辑不清空 IME 组合 marked ---------- */

/// 红测（C3-主）：进入 IME 组合态（`marked` 已占位）后做一次**范围替换**（`replace_range`
/// 路径——粘贴/快捷键插入/折叠写回等一切非 IME 编辑都从它过，edit.rs:401）→ 组合态必须
/// 被清空。现状：`replace_range` **不清** `self.marked`（同管线 `apply_raw_edit`:1047 显式清），
/// 组合中途任意普通编辑后 `marked` 留陈旧区间 → Windows IME 下一次 `range=None` 经
/// `ime::byte_range`（crates/ime/lib.rs:136）把陈旧区间当组合再次替换 → 重插入/错位/重复串。
#[gpui::test]
fn replace_range_during_composition_clears_marked(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("你好"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            e.set_caret("你好".len());
            // 组合进行中：marked 占位「ni」（组合文本落在光标后，源区间 6..8）。
            e.replace_and_mark_utf16(None, "ni", None, cx);
            assert!(e.marked.is_some(), "前置：组合进行中应有 marked 占位");
            // 普通编辑（非 IME）：光标处插入一个字符。
            e.replace_range(e.caret()..e.caret(), "X", cx);
            assert!(
                e.marked.is_none(),
                "组合中途范围替换必须清空 marked（否则 Windows IME 下一步以 range=None 替换陈旧区间）"
            );
        });
    });
}

/// 红测（C3-退格）：组合中途按退格（`backspace` → `replace_range`，用户真实触发的
/// 非 IME 编辑路径）→ 同款必须清 marked。
#[gpui::test]
fn backspace_during_composition_clears_marked(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("你好"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| {
            e.set_caret("你好".len());
            e.replace_and_mark_utf16(None, "ni", None, cx);
            assert!(e.marked.is_some(), "前置：组合进行中应有 marked 占位");
            e.backspace(cx);
            assert!(
                e.marked.is_none(),
                "组合中途退格必须清空 marked（否则 Windows IME 下一步以 range=None 替换陈旧区间）"
            );
        });
    });
}

/* ---------- Bug C5：sync_math 在 paint 期直接 drop_image(None) 漏删当前窗口纹理 ---------- */

/// 红测（C5）：`sync_math`（content_sync.rs:260，paint 期调用，见 element/mod.rs:481）对
/// `pending_math_drop` / 主题切换整清 / LRU 淘汰的 `Ready` 位图直接 `cx.drop_image(img, None)`
/// （content_sync.rs:266/278/355）。而当前窗口回调跑在 gpui `update_window_erased`
/// （vendor gpui app.rs:3146）里——当前窗口被临时移出 `App.windows`，`App::drop_image(None)`
/// 只遍历 `App.windows`（app.rs:3103）→ 当前窗口 sprite atlas 条目漏删（K14：当前窗口纹理
/// 只涨不降）。复现：注入公式 Ready 位图并真实绘制（入当前窗口图集）→ 收进
/// `pending_math_drop`（install_open 换源同款「脏标记延帧释放」）→ 当前窗口 update
/// （= paint 期同构：`App.windows` 不含当前窗口）走一次 `sync_math` → 修复前当前窗口
/// 图集条目残留 → 红。
#[gpui::test]
fn sync_math_drop_releases_current_window_atlas(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("$$\na = b\n$$\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let buf = kimg::RgbaImage::from_raw(8, 8, vec![0u8; 8 * 8 * 4]).unwrap();
    let img = std::sync::Arc::new(gpui::RenderImage::new(vec![kimg::Frame::new(buf)]));
    // ① 注入公式 Ready 位图并真实绘制 → 当前窗口 sprite atlas 持有该图条目。
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        // 光标放块外 → 折叠成卡片（非 reveal）才会出 `BlockCard::Math` / 走 Ready 位图绘制。
        let end = ed.read(cx).content().len();
        ed.update(cx, |e, _| e.set_caret(end));
        let f = build_frame(ed.read(cx), window, px(600.), (0., 9999.));
        let key = f
            .rows
            .iter()
            .find_map(|r| match &r.extra.card {
                Some(super::BlockCard::Math { latex, .. }) => Some(latex.trim().to_string()),
                _ => None,
            })
            .expect("$$ 卡");
        ed.update(cx, |e, _| {
            e.math_slots().borrow_mut().insert(
                key.into(),
                super::super::MathSlot::Ready(img.clone(), 8., 8.),
            );
        });
        window.refresh();
        window.draw(cx).clear(cx);
    });
    assert!(
        vcx.update(|window, _| window.has_image_atlas_entry(&img)),
        "前置：真实绘制后当前窗口图集应持有该位图条目"
    );
    // ② 位图回收（install_open 换源同款「脏标记延帧释放」）：收进 pending_math_drop。
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| {
            e.pending_math_drop.borrow_mut().push(img.clone());
        });
    });
    // ③ 当前窗口 update（= paint 期同构：App.windows 不含当前窗口）走 sync_math：
    //    drain pending_math_drop → cx.drop_image(img, None)。修复后当前窗口图集条目应已被释放，
    //    修复前 drop(None) 遍历不到当前窗口 → 条目残留 → 红。
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let f = build_frame(ed.read(cx), window, px(600.), (0., 9999.));
        ed.update(cx, |e, ecx| e.sync_math(ecx, &f));
    });
    assert!(
        !vcx.update(|window, _| window.has_image_atlas_entry(&img)),
        "sync_math 释放位图后当前窗口图集不得残留该条目（paint 期直接 drop_image(None) 漏删当前窗口）"
    );
}
