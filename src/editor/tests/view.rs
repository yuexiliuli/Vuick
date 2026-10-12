
// ---------------------------------------------------------------------------
// 视图
// ---------------------------------------------------------------------------
struct Host {
    ed: Entity<CodeEditor>,
}
impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.ed.clone()
    }
}

fn open<'a>(cx: &'a mut TestAppContext, text: &str) -> (Entity<Host>, &'a mut VisualTestContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let ed = cx.new(|cx| CodeEditor::new(cx));
        ed.update(cx, |ed, _| ed.set_text(text));
        Host { ed }
    });
    let cx: &mut VisualTestContext = cx;
    settle(cx);
    (host, cx)
}

fn settle(cx: &mut VisualTestContext) {
    for _ in 0..4 {
        cx.run_until_parked();
        let _ = cx.refresh();
    }
    cx.run_until_parked();
}

fn focus(host: &Entity<Host>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let fh = ed.read(cx).focus_handle();
        window.focus(&fh, cx);
    });
    settle(cx);
}

fn editor_text(host: &Entity<Host>, cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| host.read(cx).ed.read(cx).text_string())
}

fn editor_selection(host: &Entity<Host>, cx: &mut VisualTestContext) -> Selection {
    cx.update(|_, cx| host.read(cx).ed.read(cx).selection())
}

fn window_handle(cx: &mut VisualTestContext) -> gpui::AnyWindowHandle {
    cx.windows().first().copied().expect("有窗口")
}

#[gpui::test]
fn markdown_editor_renders_wysiwyg_styles(cx: &mut TestAppContext) {
    use std::ops::Range;
    use gpui::HighlightStyle;

    // Phase 1：`.md` 编辑区行渲染走「所见即所得」滤镜——记号淡化、正文富样式，
    // 且底层纯文本完全不变（光标/选区/撤销/保存走原路径，无「视觉↔字节」映射）。
    let text = "# 标题\n\n**加粗** `code` [链接](https://x) 与 *斜体*\n- 列表项\n> 引用\n";
    let (host, cx) = open(cx, text);

    // 非 md 语言不受滤镜影响（回归护栏：syntect 路径保持可用）
    let _rust_row = cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.set_language(Some("rust")));
        ed.update(cx, |ed, _| ed.test_line_styles(window, 0))
    });

    // 切到 markdown → 启用滤镜
    let row0 = cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.set_language(Some("markdown")));
        ed.update(cx, |ed, _| ed.test_line_styles(window, 0))
    });
    // 标题行：`#` 记号淡化 + 正文加粗
    assert!(
        row0.iter().any(|(_, s)| s.fade_out.is_some()),
        "标题记号（#）应淡化: {row0:?}"
    );
    assert!(
        row0.iter().any(|(_, s)| s.font_weight == Some(gpui::FontWeight::BOLD)),
        "标题正文应加粗: {row0:?}"
    );

    // 第 3 行：**加粗** `code` [链接](https://x) 与 *斜体*
    let row2 = cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.test_line_styles(window, 2))
    });
    let has = |row: &[(Range<usize>, HighlightStyle)], pred: fn(&HighlightStyle) -> bool| {
        row.iter().any(|(_, s)| pred(s))
    };
    assert!(
        has(&row2, |s| s.font_weight == Some(gpui::FontWeight::BOLD)),
        "应渲染加粗: {row2:?}"
    );
    assert!(
        has(&row2, |s| s.font_style == Some(gpui::FontStyle::Italic)),
        "应渲染斜体: {row2:?}"
    );
    assert!(
        has(&row2, |s| s.background_color.is_some()),
        "行内代码应有底色: {row2:?}"
    );
    assert!(
        has(&row2, |s| s.fade_out.is_some()),
        "记号应淡化: {row2:?}"
    );
    assert!(
        has(&row2, |s| s.underline.is_some()),
        "链接应有下划线: {row2:?}"
    );

    // 底层文本保持不变（所见≠所得底稿仍是纯 Markdown）
    assert_eq!(editor_text(&host, cx), text);
}

#[gpui::test]
fn font_size_change_keeps_viewport_top_anchored(cx: &mut TestAppContext) {
    // 回归（2026-09-19 用户反馈）：编辑区 Ctrl+滚轮放大/缩小字号时窗口也跟着滚动。
    // 修复 = 行高变化时把滚动目标同比换算（同 PDF 的视口锚定）——视口顶部的内容点
    // 不随行高变化跳走。这里直接改字号覆盖（等价于 Ctrl+滚轮 / Ctrl+=/- 的落地路径），
    // 断言「滚动 px 按行高比缩放、顶部绝对视觉行不变」。
    let text = (1..=300).map(|i| format!("第{i}行")).collect::<Vec<_>>().join("\n");
    let (host, cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    // 普通滚轮（非 Ctrl）滚到中段 → 作为缩放前的视口锚点。
    let b = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    let pos = point(
        px(b.origin.x.as_f32() + b.size.width.as_f32() / 2.0),
        px(b.origin.y.as_f32() + b.size.height.as_f32() / 2.0),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-3000.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(cx);
    let (d1, lh1, f0) = cx.update(|_, cx| {
        let ed = ed.read(cx);
        (ed.test_desired_scroll_px(), ed.test_metrics().1, ed.font_size_px())
    });
    assert!(d1 > 0., "应先滚到中段（d1={d1}）");
    // 放大字号 → 行高变大 → 滚动目标应按行高比缩放（视口顶部内容点锚定）。
    ed.update(cx, |ed, cx| {
        ed.set_font_override(Some(f0 + 6.0));
        cx.notify();
    });
    settle(cx);
    let (d2, lh2) = cx.update(|_, cx| {
        let ed = ed.read(cx);
        (ed.test_desired_scroll_px(), ed.test_metrics().1)
    });
    assert!(lh2 > lh1, "放大后行高应变大：{lh1} → {lh2}");
    assert!(
        (d2 - d1 * (lh2 / lh1)).abs() < lh1 * 0.5,
        "滚动目标应按行高比缩放（锚定视口）：{d1} → {d2}（期望 {:.1}）",
        d1 * (lh2 / lh1)
    );
    let row1 = (d1 / lh1).floor();
    let row2 = (d2 / lh2).floor();
    assert!(
        (row2 - row1).abs() <= 2.0,
        "缩放后顶部所在绝对视觉行应不变：{row1} → {row2}"
    );
    // 缩小（回到略小于初始）→ 反向同样锚定。
    ed.update(cx, |ed, cx| {
        ed.set_font_override(Some(f0 - 2.0));
        cx.notify();
    });
    settle(cx);
    let (d3, lh3) = cx.update(|_, cx| {
        let ed = ed.read(cx);
        (ed.test_desired_scroll_px(), ed.test_metrics().1)
    });
    assert!(lh3 < lh2, "缩小后行高应变小：{lh2} → {lh3}");
    assert!(
        (d3 - d1 * (lh3 / lh1)).abs() < lh1 * 0.5,
        "缩小同样按行高比锚定：{d1} → {d3}（期望 {:.1}）",
        d1 * (lh3 / lh1)
    );
}

#[gpui::test]
fn editor_bottom_pad_quantized_to_line_height(cx: &mut TestAppContext) {
    // 回归（专家评审 P4）：底部预留（横向滚动条厚 16px + 额外行高）向上量化到
    // line_height 整数倍——否则滚到底视口停在「行间半格」上，最后一行加载/点击
    // 命中/光标出现次像素误差。断言 pad 是行高的整数倍且 ≥ 一行。
    let (host, cx) = open(cx, "row0\nrow1\nrow2");
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1).max(1.);
    let pad = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bottom_pad_px());
    let frac = pad / lh;
    assert!(
        (frac - frac.round()).abs() < 1e-3,
        "底部预留 {pad:.2}px 应为行高 {lh:.2}px 的整数倍，实际 {} 行",
        frac
    );
    assert!(pad >= lh, "底部预留应至少覆盖一行（{pad:.2} ≥ {lh:.2}）");
}

#[gpui::test]
fn editor_renders_list_and_rows(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "第一行\n第二行\n第三行");
    let wrap = cx
        .debug_bounds("ce-list-wrap")
        .expect("编辑器列表容器应渲染");
    assert!(wrap.size.height > px(0.));
    assert!(wrap.size.width > px(0.));
    // 回归（2026-09-15 实机验证）：`list()` 必须 `.size_full()` 撑满容器。
    // 否则 List 走 Infer 测量，高度=内容高，ListState 视口高=0，
    // 整块编辑区不绘制任何行（编辑区空白、看似“无法编辑”）。
    let viewport_h = cx.update(|_, cx| host.read(cx).ed.read(cx).list_viewport_height());
    assert!(
        viewport_h > 0.,
        "list 视口应被撑满（viewport_h={viewport_h}），否则可见行数为 0"
    );
    assert_eq!(editor_text(&host, cx), "第一行\n第二行\n第三行");
}

#[gpui::test]
fn typing_then_undo_redo(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "");
    focus(&host, cx);
    let w = window_handle(cx);
    cx.simulate_keystrokes("abc");
    settle(cx);
    assert_eq!(editor_text(&host, cx), "abc");

    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-z").unwrap());
    settle(cx);
    assert_eq!(editor_text(&host, cx), "");

    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-y").unwrap());
    settle(cx);
    assert_eq!(editor_text(&host, cx), "abc");
}

#[gpui::test]
fn caret_and_backspace(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "ab");
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    ed.update(cx, |ed, _| ed.set_caret_to(1));
    settle(cx);
    let w = window_handle(cx);
    cx.dispatch_keystroke(w, Keystroke::parse("backspace").unwrap());
    settle(cx);
    assert_eq!(editor_text(&host, cx), "b");
    // 回退后光标在 0
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.head(), 0);
}

#[gpui::test]
fn select_all_then_type_replaces(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "hello");
    focus(&host, cx);
    let w = window_handle(cx);
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-a").unwrap());
    settle(cx);
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.range(), 0..5);
    cx.simulate_keystrokes("x");
    settle(cx);
    assert_eq!(editor_text(&host, cx), "x");
}

#[gpui::test]
fn mouse_click_places_caret_on_second_row(cx: &mut TestAppContext) {
    let text = "row0\nrow1\nrow2";
    let (host, cx) = open(cx, text);
    // 取行高，点击第 2 行文本区
    let lh = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_metrics().1
    });
    cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let fh = ed.read(cx).focus_handle();
        window.focus(&fh, cx);
    });
    settle(cx);
    let y = px(lh * 1.5); // 第 2 行中部
    let x = px(4.); // gutter 内 → 列 0
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(cx);
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.head(), "row0\n".len(), "应落到第 2 行行首");
}

#[gpui::test]
fn fold_block_heuristic_detects_indent_blocks(cx: &mut TestAppContext) {
    let text = "fn foo() {\n    a();\n    b();\n}\nfn bar() {\n    c();\n}";
    let (host, cx) = open(cx, text);
    let ends = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        // 0 行块：缩进 0，直到同缩进 "}" → [0,3)；4 行块（单行体）→ [4,6)。
        (ed.test_fold_block_end(0), ed.test_fold_block_end(4))
    });
    assert_eq!(ends, (Some(3), Some(6)));
    // 文件末行无后续块 → 不可折叠。
    let none = cx.update(|_, cx| host.read(cx).ed.read(cx).test_fold_block_end(6));
    assert_eq!(none, None);
}

#[gpui::test]
fn fold_reduces_scroll_range_and_expand_restores(cx: &mut TestAppContext) {
    let text = (0..20).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let (host, cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(5, 10)));
    settle(cx);
    let folded = cx.update(|_, cx| host.read(cx).ed.read(cx).test_scroll_item_count());
    // 折叠 [5,10) 隐藏 4 行 → 量程条目数应相对全文减少。
    assert!(folded < 20 + 8, "折叠后量程条目数应缩小（folded={folded}）");
    // 再次 toggle（同为 start）→ 展开恢复。
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(5, 10)));
    settle(cx);
    let expanded = cx.update(|_, cx| host.read(cx).ed.read(cx).test_scroll_item_count());
    assert!(expanded > folded, "展开后量程条目数应恢复（{folded} → {expanded}）");
}

#[gpui::test]
fn click_maps_through_fold_to_logical_line(cx: &mut TestAppContext) {
    // 20 行折叠 [5,10)（隐藏 6..9）。视觉行 7 = 逻辑行 11：点击应命中 11 行行首。
    let text = (0..20).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let (host, cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(5, 10)));
    settle(cx);
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1).max(1.);
    let y = px(lh * 7.5); // 视觉行 7
    let x = px(4.); // 行号列内 → 列 0
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(cx);
    let head = editor_selection(&host, cx).head();
    let expected = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(11, 0));
    assert_eq!(head, expected, "点击视觉行 7 应命中折叠后的逻辑行 11（{head} vs {expected}）");
}

#[gpui::test]
fn gutter_click_toggles_active_fold(cx: &mut TestAppContext) {
    let text = (0..20).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let (host, cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(5, 10)));
    settle(cx);
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1).max(1.);
    // 点击折叠首行（视觉行 5）gutter → 展开。
    let y = px(lh * 5.5);
    let x = px(2.);
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(cx);
    assert!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().is_empty()),
        "点击折叠首行 gutter 应展开折叠"
    );
}

#[gpui::test]
fn typing_inside_folded_body_expands_fold(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "aaa\nbbb\nccc\nddd");
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(1, 3))); // 隐藏行 2
    settle(cx);
    // 把光标直接放进隐藏行 2 中间，键入 → 自动展开并写入。
    let off = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(2, 1));
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.set_caret_to(off)));
    focus(&host, cx);
    cx.simulate_keystrokes("X");
    settle(cx);
    assert!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().is_empty()),
        "编辑折叠体内应自动展开折叠"
    );
    // 行 0="aaa" 行 1="bbb" 行 2="ccc"（被折叠隐藏）行 3="ddd"：在行 2 第 1 列插 X。
    assert_eq!(editor_text(&host, cx), "aaa\nbbb\ncXcc\nddd");
}

#[gpui::test]
fn ctrl_shift_bracket_folds_and_unfolds(cx: &mut TestAppContext) {
    let text = "fn foo() {\n    a();\n    b();\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    let w = window_handle(cx);
    // 光标在 0 行：Ctrl+Shift+[ → 折叠 [0,3)。
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-shift-[").unwrap());
    settle(cx);
    let folds = cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().to_vec());
    assert_eq!(folds, vec![Fold { start: 0, end: 3 }]);
    // Ctrl+Shift+] → 展开。
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-shift-]").unwrap());
    settle(cx);
    assert!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().is_empty()),
        "Ctrl+Shift+] 应展开折叠"
    );
}

#[gpui::test]
fn ruler_x_computed_from_metric_and_moves_with_hscroll(cx: &mut TestAppContext) {
    // 长行文档让横向可视范围/滚动条可用。
    let text = "x".repeat(300);
    let (host, cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (cw, _, gutter) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    // 测试窗 1920 宽 > 80 列标尺位置 → 标尺在可视区。
    let avail_w = cx.update(|_, cx| host.read(cx).ed.read(cx).test_content_w_px());
    let x0 = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_ruler_x(80, avail_w))
        .expect("80 列标尺应在可视区");
    assert!(
        (x0 - (gutter + 80.0 * cw)).abs() < 1.0,
        "标尺 x 应 = gutter + col*char_width（x0={x0}）"
    );
    // 横向滚动 ≥5 格 → 标尺随内容左移约 300px。
    let ed_b = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + 200.),
        px(ed_b.origin.y.as_f32() + 50.),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(300.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(cx);
    let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(hscroll > 0., "应真正横向滚动（hscroll={hscroll}）");
    let x1 = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_ruler_x(80, avail_w))
        .expect("80 列标尺滚动后仍应在可视区");
    assert!(
        (x1 - (x0 - hscroll)).abs() < 1.0,
        "标尺应随内容左移 hscroll（x0={x0} x1={x1} hscroll={hscroll}）"
    );
}

#[gpui::test]
fn fold_candidates_detected_for_indented_blocks(cx: &mut TestAppContext) {
    // 打开即自动探测：内层 if 块 + 外层函数块（可嵌套）。
    let text = "fn a() {\n    if x {\n        y();\n    }\n    z();\n}";
    let (host, cx) = open(cx, text);
    let cands = cx.update(|_, cx| host.read(cx).ed.read(cx).test_fold_candidates().to_vec());
    // 落地点按 `start` 升序（`candidate_starting_at` 二分查询依赖排序）；集合不变。
    assert_eq!(cands, vec![Fold { start: 0, end: 5 }, Fold { start: 1, end: 3 }]);
    // gutter 指示器口径：候选起点行有标记。
    let marks = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        (
            ed.test_has_fold_or_candidate(0),
            ed.test_has_fold_or_candidate(1),
            ed.test_has_fold_or_candidate(2),
        )
    });
    assert_eq!(marks, (true, true, false));
}

#[gpui::test]
fn gutter_click_on_candidate_creates_fold(cx: &mut TestAppContext) {
    // 候选已自动探测但未激活：点击 candidate 起点行的 gutter → 创建激活折叠。
    let text = "fn a() {\n    if x {\n        y();\n    }\n    z();\n}";
    let (host, cx) = open(cx, text);
    // 确认尚未激活。
    assert!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().is_empty()),
        "打开时不应有激活折叠"
    );
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1).max(1.);
    // 视觉行 1 = 逻辑行 1（if 块候选）。
    let y = px(lh * 1.5);
    let x = px(2.);
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(cx);
    let folds = cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().to_vec());
    assert_eq!(folds, vec![Fold { start: 1, end: 3 }], "点击候选起点应创建激活折叠");
}

#[gpui::test]
fn ruler_infra_renders_but_defaults_off(cx: &mut TestAppContext) {
    // margin guide 默认关闭（EDITOR_RULER_COLUMNS 为空）：叠层容器仍渲染、无竖线子元素；
    // 坐标换算（第 80 列在可视区内应给 Some(x)）保持精确，供用户配置恢复使用。
    let text = "x".repeat(300);
    let (host, cx) = open(cx, &text);
    let (cw, _, gutter) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let avail_w = cx.update(|_, cx| host.read(cx).ed.read(cx).test_content_w_px());
    let x = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_ruler_x(80, avail_w))
        .expect("第 80 列在可视区内应可通过单纯换算得到 x");
    assert!((x - (gutter + 80.0 * cw)).abs() < 1.0, "标尺换算 = gutter + col*char_width");
    assert!(
        cx.debug_bounds("ce-rulers").is_some(),
        "标尺叠层容器仍应在渲染树中"
    );
    // 默认列数为空 → 不渲染任何 ce-ruler-line（margin guide 默认隐藏）。
    assert!(
        cx.debug_bounds("ce-ruler-line").is_none(),
        "默认不渲染第 80 列竖线（用户要的是括号标尺）"
    );
}

#[gpui::test]
fn bracket_guides_detected_and_rendered(cx: &mut TestAppContext) {
    // 用户示例：三对跨行花括号 → 三条括号标尺竖线。
    let text = "impl RowData {\n    fn empty() -> Self {\n        Self {\n            a: 1,\n        }\n    }\n}";
    let (host, cx) = open(cx, text);
    let pairs = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bracket_pairs().to_vec());
    // 落地点按 `open_line` 升序（标尺视口二分依赖排序）；配对集合不变。
    assert_eq!(
        pairs,
        vec![
            crate::editor::fold::BracketPair { open_line: 0, open_col: 13, close_line: 6, guide_units: 0 },
            crate::editor::fold::BracketPair { open_line: 1, open_col: 23, close_line: 5, guide_units: 4 },
            crate::editor::fold::BracketPair { open_line: 2, open_col: 13, close_line: 4, guide_units: 8 },
        ]
    );
    // 竖线叠层确实渲染进树。
    assert!(
        cx.debug_bounds("ce-bracket-guides").is_some(),
        "括号标尺叠层应渲染"
    );
}

#[gpui::test]
fn bracket_guide_window_keeps_enclosing_pair_when_viewport_deep(cx: &mut TestAppContext) {
    // 标尺窗口化（Sprint 4a，`BRACKET_GUIDE_WINDOW_LOOKBACK_LINES`）回归：
    // 视口已滚到某外层块的**深处**（open_line 在视口顶上方、但仍在回看窗口内）时，
    // 该块的竖线仍须画出——竖线贯穿视口（高层包装块/长函数体）。若窗口下界误用
    // 视口顶（而非 视口顶−回看），深滚时外层竖线会整段漏画。
    use crate::config::limits::BRACKET_GUIDE_WINDOW_LOOKBACK_LINES as LOOKBACK;
    let mut text = String::new();
    text.push_str("fn outer() {\n"); // line 0
    text.push_str("    println!(\"head\");\n"); // line 1..9
    for _ in 2..10 {
        text.push_str("    \n");
    }
    text.push_str("    {\n"); // line 10
    for _ in 11..40 {
        text.push_str("    \n");
    }
    text.push_str("    }\n"); // line 40
    text.push_str("}\n"); // line 41
    let (host, cx) = open(cx, &text);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let top_vis = 20; // 视口在两层块中间（外层 open=0 上方远处、仍回看窗口内）。
    assert!(
        top_vis < LOOKBACK,
        "本用例的前置：20 < 回看 {LOOKBACK}"
    );
    // 视口内可见竖线段：外层 fn（x≈0）从 21 到底、内层 {（x≈4）从 21 到 39。
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(top_vis, 0.0, lh, 1200.0, 1000.0)
    });
    let outer = segs
        .iter()
        .find(|(x, _, h)| (*x - 0.0 * cw).abs() < 1.0 && *h > (40 - top_vis - 1) as f32 * lh)
        .expect("视口深处仍应有外层 fn 竖线（贯穿视口）");
    assert!(
        outer.1 < lh * 2.,
        "外层竖线应贴视口顶（从 open 之后的深处横贯）：y={}",
        outer.1
    );
    let inner = segs
        .iter()
        .find(|(x, _, h)| (*x - 4.0 * cw).abs() < 1.0 && *h > 15.)
        .expect("应有内层 { 竖线");
    assert!(
        inner.1 >= 0. && inner.2 > 0.,
        "内层竖线应在视口内可见：{inner:?}"
    );
}

#[gpui::test]
fn bracket_guide_segments_span_each_pair(cx: &mut TestAppContext) {
    use crate::config::layout::{
        BRACKET_GUIDE_CLOSE_TAIL_LINES as TAIL, BRACKET_GUIDE_OPEN_HEAD_LINES as HEAD,
    };
    // 与用户样例同构。注意 spans：竖线从开行**之后**画到闭行**内约半行**（贴 `}` 字形），
    // 端点微调一律按 config 常量算期望（用户调 HEAD/TAIL 后不打红）。
    let text = "impl RowData {\n    fn empty() -> Self {\n        Self {\n            a: 1,\n        }\n    }\n}";
    let (host, cx) = open(cx, text);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let harea_w = 1200.0;
    let view_h = 1000.0;
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(0, 0.0, lh, harea_w, view_h)
    });
    // 三条竖线：x = 块首行缩进列 0/4/8；跨度 [开行+1, 闭行) 底端再伸 TAIL、顶部缩 HEAD。
    assert_eq!(segs.len(), 3, "三对跨行花括号 → 三条竖线");
    // 外层 impl 块：x≈0cw，y=(1+HEAD)lh 起、高 (5−HEAD+TAIL)lh（行 1..5 + 尾巴）。
    let impl_guide = segs
        .iter()
        .find(|(_, y, h)| {
            (*y - (1.0 + HEAD) * lh).abs() < 0.5
                && (*h - (5.0 - HEAD + TAIL) * lh).abs() < 0.5
        })
        .expect("应有 impl 块竖线");
    assert!(
        (impl_guide.0 - 0.0 * cw).abs() < 1.0,
        "外层竖线应在块首列 0×字宽，实际 {}",
        impl_guide.0
    );
    // Self 块：x≈8cw，y=(3+HEAD)lh 起、高 (1−HEAD+TAIL)lh（行 3 + 尾巴）。
    let self_guide = segs
        .iter()
        .find(|(_, y, h)| {
            (*y - (3.0 + HEAD) * lh).abs() < 0.5
                && (*h - (1.0 - HEAD + TAIL) * lh).abs() < 0.5
        })
        .expect("应有 Self 块竖线");
    assert!(
        (self_guide.0 - 8.0 * cw).abs() < 1.0,
        "内层竖线应在块首列 8×字宽，实际 {}",
        self_guide.0
    );
}

#[gpui::test]
fn bracket_guide_overlapping_same_column_merged(cx: &mut TestAppContext) {
    use crate::config::layout::{
        BRACKET_GUIDE_CLOSE_TAIL_LINES as TAIL, BRACKET_GUIDE_OPEN_HEAD_LINES as HEAD,
    };
    // 同列（缩进 0）嵌套的两个块：原会画两条重叠竖线（视觉加粗/变色），合并成一根。
    let text = "fn a() {\n{\n    x();\n}\n}";
    let (host, cx) = open(cx, text);
    let (_, lh, _) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(0, 0.0, lh, 1200.0, 1000.0)
    });
    // 两根（fn a 的 [1,4) 与内层 { 的 [2,3)）同列 → 合并成一根 [1,4)，端点半行微调。
    assert_eq!(segs.len(), 1, "同列重叠竖线应合并为一根");
    assert!(
        (segs[0].1 - (1.0 + HEAD) * lh).abs() < 0.5
            && (segs[0].2 - (3.0 - HEAD + TAIL) * lh).abs() < 0.5,
        "合并段应覆盖行 1..3 + 端点半行（实际 y={} h={}）",
        segs[0].1,
        segs[0].2
    );
}

#[gpui::test]
fn guide_click_highlights_pair_and_elsewhere_clears(cx: &mut TestAppContext) {
    // 语义（用户确认）：标尺不可点选；点体内某行 → 点亮该行所属**最内层**括号块竖线；
    // 点非块内行（如顶层）→ 清除。命中断言：行 3 ∈ Self 块 [2,4)、行 2 = 开行 → 外层。
    let text = "impl RowData {\n    fn empty() -> Self {\n        Self {\n            a: 1,\n        }\n    }\n}\nfn other() {}";
    let (host, cx) = open(cx, text);
    // 口径断言（不依赖鼠标坐标）。
    let (case, case2, case7) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.read(cx);
        (
            ed.test_guide_for_logical_row(3),
            ed.test_guide_for_logical_row(2),
            ed.test_guide_for_logical_row(7),
        )
    });
    assert_eq!(case, Some(2), "行 3 ∈ Self 块 [2,4) → 点亮 Self 开行 2");
    assert_eq!(case2, Some(1), "行 2 = Self 开行（非块内）→ 归外层 fn 块");
    assert_eq!(case7, None, "行 7 = 顶层 fn 行 → 无命中");
    let (lh, gutter) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.read(cx).test_metrics();
        (ed.1.max(1.), ed.2)
    });
    let mods = Modifiers::default();
    // 点体内（行 3 中部，x 取 gutter+10px → 文本区）。
    let y = px(lh * 3.5);
    let x = px(gutter + 10.);
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    // 点亮 Self 块。
    let lit = cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_guide());
    assert_eq!(lit, Some(2), "点行 3 应点亮 Self 块（open_line=2）");
    // 点顶层行 7 → 清除。
    let y7 = px(lh * 7.5);
    cx.simulate_mouse_move(point(x, y7), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y7), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y7), MouseButton::Left, mods);
    let lit = cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_guide());
    assert_eq!(lit, None, "点顶层行 7 应清除点亮");
}

#[gpui::test]
fn mouse_click_lands_on_ascii_char_boundaries(cx: &mut TestAppContext) {
    // 回归护栏：等宽 ASCII 命中测试在接入真实行布局后仍应精确落在字符边界。
    let line = "abcdef";
    let (host, cx) = open(cx, line);
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let y = px(lh * 0.5);
    for (k, expected_head) in [(1usize, 1usize), (2, 2), (3, 3), (6, 6)] {
        let x = cx.update(|window, cx| {
            px(gutter + ed.read(cx).test_char_boundary_x(window, 0, expected_head))
        });
        let mods = Modifiers::default();
        cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
        settle(cx);
        assert_eq!(
            editor_selection(&host, cx).head(),
            expected_head,
            "点击第 {k} 个 ASCII 字符右边界应落在第 {k} 字符后"
        );
    }
}

#[gpui::test]
fn mouse_click_lands_on_cjk_char_boundaries(cx: &mut TestAppContext) {
    // 回归（2026-09-15 真机反馈）：中文行点击中间输入，实际偏移到更后面。
    // 根因：命中测试按「每字符 = 1×ASCII 字符宽」等宽近似，而 CJK 全角字实际
    // 宽 2× → 点击第 k 个字被算成第 2k 个字。现在用真实行布局
    // `closest_index_for_x` 做像素→字符边界映射，点击即落点。
    let line = "中华文字"; // 4 个全角字：每字 3 字节
    let (host, cx) = open(cx, line);
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let y = px(lh * 0.5);
    for (k, expected_head) in [(1usize, 3usize), (2, 6), (3, 9), (4, 12)] {
        let x = cx.update(|window, cx| {
            px(gutter + ed.read(cx).test_char_boundary_x(window, 0, expected_head))
        });
        let mods = Modifiers::default();
        cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
        settle(cx);
        let head = editor_selection(&host, cx).head();
        assert_eq!(head, expected_head, "点击第 {k} 个全角字右边界应落在第 {k} 字后");
    }
}

#[gpui::test]
fn mouse_drag_selects_across_rows_rust_vs_plain(cx: &mut TestAppContext) {
    // 复现调查：大文档测试.rs 无法鼠标选择、而 .txt/.md 正常。
    // 组件内鼠标逻辑与语言无关，此测试把「带 Rust 高亮」与「纯文本」并排验证，
    // 确认是否为语言/高亮路径引起。
    for lang in [Some("rust"), Some("markdown"), None] {
        let text = "fn main() {\n    let s = \"hello\";\n    // 注释\n}\n";
        let (host, cx) = open(&mut *cx, text);
        let ed = cx.update(|_, cx| host.read(cx).ed.clone());
        ed.update(cx, |ed, _| ed.set_language(lang));
        focus(&host, cx);
        settle(cx);
        let ed = cx.update(|_, cx| host.read(cx).ed.clone());
        let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
        let x1 = cx.update(|window, cx| {
            px(gutter + ed.read(cx).test_char_boundary_x(window, 0, 2))
        });
        let x2 = cx.update(|window, cx| {
            px(gutter + ed.read(cx).test_char_boundary_x(window, 2, 3))
        });
        let y1 = px(lh * 0.5);
        let y2 = px(lh * 2.5);
        let mods = Modifiers::default();
        cx.simulate_mouse_move(point(x1, y1), MouseButton::Left, mods);
        cx.simulate_mouse_down(point(x1, y1), MouseButton::Left, mods);
        cx.simulate_mouse_move(point(x2, y2), MouseButton::Left, mods);
        cx.simulate_mouse_up(point(x2, y2), MouseButton::Left, mods);
        settle(cx);
        let sel = editor_selection(&host, cx);
        assert!(
            !sel.is_collapsed(),
            "lang={lang:?}: 拖选应形成选区，sel={sel:?}"
        );
        assert_eq!(sel.range().start, 2, "lang={lang:?} 选区起点");
        assert_eq!(
            sel.range().end,
            "fn main() {\n    let s = \"hello\";\n".len() + 3,
            "lang={lang:?} 选区终点应在第 3 行第 4 字符后"
        );
    }
}

/// 回归（拖选生命周期终点之一，场景 c）：**拖到元素/窗口外松开**时元素级
/// `on_mouse_up`（受 `hitbox.is_hovered` 门控）不会派发——必须由 Capture 阶段的
/// `on_mouse_up_out` 复位 `dragging`，否则残留拖选态会让「后续仅移动鼠标」继续把
/// 选区从 anchor 一路拉长（与 P4 菜单项 `stop_propagation` 截断 MouseUp 同源）。
#[gpui::test]
fn mouse_drag_released_outside_does_not_extend_on_next_move(cx: &mut TestAppContext) {
    let text = "fn main() {\n    let s = \"hello\";\n    // 注释\n    let t = 1;\n}\n";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let x1 = cx.update(|window, cx| {
        px(gutter + ed.read(cx).test_char_boundary_x(window, 0, 2))
    });
    let x2 = cx.update(|window, cx| {
        px(gutter + ed.read(cx).test_char_boundary_x(window, 2, 3))
    });
    let y1 = px(lh * 0.5);
    let y2 = px(lh * 2.5);
    let mods = Modifiers::default();
    // 编辑区内按下 → 拖到第 3 行 → 形成跨行选区（此时 dragging=true）。
    cx.simulate_mouse_move(point(x1, y1), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x1, y1), MouseButton::Left, mods);
    cx.simulate_mouse_move(point(x2, y2), MouseButton::Left, mods);
    settle(cx);
    let extended = editor_selection(&host, cx);
    assert!(!extended.is_collapsed(), "拖选应已形成选区：{extended:?}");
    // 在远离编辑区/窗口的「窗口外」松开：元素级 on_mouse_up（hitbox 门控）不派发，
    // 走 on_mouse_up_out 复位拖选态。
    let far = point(px(-1000.), px(-1000.));
    cx.simulate_mouse_up(far, MouseButton::Left, mods);
    settle(cx);
    // 之后仅移动鼠标（不按任何键）到文档更深处：拖选态已复位 → 不得再把选区拉长。
    let far_in = point(x2, px(lh * 3.5));
    cx.simulate_mouse_move(far_in, MouseButton::Left, mods);
    settle(cx);
    let after = editor_selection(&host, cx);
    assert_eq!(
        (after.range().start, after.range().end),
        (extended.range().start, extended.range().end),
        "窗口外松开后仅移动鼠标不得继续扩选：{extended:?} → {after:?}"
    );
}

#[gpui::test]
fn caret_image_is_laid_out_when_focused(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "abc\nxyz");
    focus(&host, cx);
    settle(cx);
    // 回归（2026-09-15 实机）：光标此前画画的是 colors.accent（Light 主题 =#f5f5f5
    // 白底近白 → 看不见）；这里断言光标元素真实布局出来，且尺寸 = 2px×行高。
    let b = cx
        .debug_bounds("ce-caret")
        .expect("焦点状态下应渲染光标元素");
    assert!(
        b.size.width.as_f32() > 1.0 && b.size.height.as_f32() > 0.0
            && b.origin.y < px(40.),
        "光标应落在第 1 行、宽≥1px、高>0：{b:?}"
    );
}

#[gpui::test]
fn gutter_fixed_while_content_scrolls_horizontally(cx: &mut TestAppContext) {
    // 回归（2026-09-15 用户反馈）：编辑区内容横向滚动时，行号也跟着滚走。
    // 现在行号是编辑区左缘的固定列（`ce-gutter`，独立 List 与文本锁步）——
    // 横向滚动内容后行号列左缘必须纹丝不动，而文本内容必须真的被推走。
    // 测试窗 1920px 宽 → 最长行需远超可视区才有横向可滚量程。
    let long = "一".repeat(600);
    let (host, cx) = open(cx, &format!("{long}\n第二行"));
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let gutter0 = cx
        .debug_bounds("ce-gutter")
        .expect("行号固定列应渲染");
    assert!(
        gutter0.size.width.as_f32() > 0.,
        "行号列应有宽度：{gutter0:?}"
    );
    // 向编辑区中心派发横向滚轮（向右 = 内容左移，正 delta）。事件坐标与
    // `debug_bounds` 同空间（不能用 `window.bounds()`——那是平台坐标）。
    let ed_b = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + ed_b.size.width.as_f32() / 2.0),
        px(ed_b.origin.y.as_f32() + ed_b.size.height.as_f32() / 2.0),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(120.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(cx);
    let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(hscroll > 0., "横向滚动应真正发生（hscroll={hscroll}）");
    let gutter1 = cx.debug_bounds("ce-gutter").expect("行号列仍在");
    assert_eq!(
        gutter0.origin.x,
        gutter1.origin.x,
        "行号列左缘不应随内容横向滚动（{gutter0:?} → {gutter1:?}）"
    );
    assert!(
        gutter1.origin.x.as_f32().abs() < 1.0,
        "行号应贴在编辑区左缘（origin.x={}）",
        gutter1.origin.x.as_f32()
    );
}

#[gpui::test]
fn ime_composition_replaces_marked_text_and_commits(cx: &mut TestAppContext) {
    // 回归：IME 组合曾把每次 `replace_and_mark(None, …)` 追加到旧组合后面
    // （产生 'nn'...垃圾残留），提交后也不清 marked → 下次组合把已提交文本当
    // 旧 marked 再次替换。参照 gpui-component 的组合流：None 区间 = 替换当前
    // marked；普通 `replace_text_in_range` 提交后自动结束组合。
    let (host, cx) = open(cx, "");
    focus(&host, cx);
    cx.update(|window, app| {
        let ed = host.read(app).ed.clone();
        ed.update(app, |ed, cx| {
            ed.replace_text_in_range(None, "prefix ", window, cx);
            // 组合第 1 字：n → ni → 你（每次更新替换旧的 marked）
            ed.replace_and_mark_text_in_range(None, "n", None, window, cx);
            ed.replace_and_mark_text_in_range(None, "ni", None, window, cx);
            ed.replace_and_mark_text_in_range(None, "你", None, window, cx);
            // 提交第 1 字（结束本次组合，marked 清空）
            ed.replace_text_in_range(None, "你", window, cx);
            // 组合第 2 字并提交
            ed.replace_and_mark_text_in_range(None, "h", None, window, cx);
            ed.replace_and_mark_text_in_range(None, "ha", None, window, cx);
            ed.replace_and_mark_text_in_range(None, "好", None, window, cx);
            ed.replace_text_in_range(None, "好", window, cx);
        });
    });
    assert_eq!(editor_text(&host, cx), "prefix 你好");
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.head(), "prefix 你好".len());
    // 提交后再开一次组合，不应把已提交的 "prefix 你好" 当旧 marked 覆盖
    cx.update(|window, app| {
        let ed = host.read(app).ed.clone();
        ed.update(app, |ed, cx| {
            ed.replace_and_mark_text_in_range(None, "x", None, window, cx);
            ed.replace_text_in_range(None, "!", window, cx);
        });
    });
    assert_eq!(editor_text(&host, cx), "prefix 你好!");
}

#[gpui::test]
fn ime_new_selected_range_is_relative_to_marked_text(cx: &mut TestAppContext) {
    // 回归 1（崩溃）：`new_selected_range` 曾被当「文档绝对 UTF-16 偏移」→ 光标
    // 跳到文档开头附近，落点多字节字符中间时后续插入触发
    // `byte index is not on a char boundary` panic（buffer.rs apply_edit）。
    // 回归 2（跳开头）：同一根因让大文档任意位置输入中文时插入位置跑回文首。
    let (host, cx) = open(cx, "一二三四五");
    let ed = cx.update(|_, app| host.read(app).ed.clone());
    // 光标移到文末（第 5 字后，字节 15）
    ed.update(cx, |ed, _| ed.set_caret_to(15));
    // 文末开始组合 "ni"，IMS 光标停在组合内第 1 个 UTF-16 单元后（相对 1..1）
    cx.update(|window, app| {
        ed.update(app, |ed, cx| {
            ed.replace_and_mark_text_in_range(None, "ni", Some(1..1), window, cx);
        });
    });
    // 光标 = 文末 + 组合内偏移 1 单元（"n"=1 字节）→ 16，绝不能跳回文档开头
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.head(), 15 + 1, "光标应停在组合内相对位置而非文档开头");
    assert_eq!(editor_text(&host, cx), "一二三四五ni");
    // 再输一次组合，替换 marked、不崩溃、光标仍在组合内
    cx.update(|window, app| {
        ed.update(app, |ed, cx| {
            ed.replace_and_mark_text_in_range(None, "你i", Some(1..1), window, cx);
        });
    });
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.head(), 15 + "你".len(), "光标应停在组合内 '你' 之后");
    assert_eq!(editor_text(&host, cx), "一二三四五你i");
    // 提交
    cx.update(|window, app| {
        ed.update(app, |ed, cx| {
            ed.replace_text_in_range(None, "你", window, cx);
        });
    });
    assert_eq!(editor_text(&host, cx), "一二三四五你");
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.head(), 15 + "你".len());
}

#[gpui::test]
fn cjk_long_line_does_not_narrow_content_width(cx: &mut TestAppContext) {
    // 回归（用户实测）：纯中文长行曾因「字符数×等宽」低估字形宽 → 行宽小于真实
    // 文本宽 → StyledText 行内折行 → 行号错位、点下一行文本重叠。
    // 现在宽度按「宽度单位」计，内容宽必须 ≥ 真实最宽行的整形宽（即不会折行）。
    let cjk = "一".repeat(200); // 200 全角字 = 400 格 = 600 字节
    let (host, cx) = open(cx, &format!("{cjk}\n第二行"));
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (cw, _, _) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let content_w = cx.update(|_, cx| ed.read(cx).test_content_w_px());
    // 真实整形宽（行内末字边界 x，byte_col=600 = 行尾）
    let shaped = cx.update(|window, cx| {
        ed.read(cx)
            .test_char_boundary_x(window, 0, 200 * 3)
    });
    assert!(
        content_w >= shaped - 1.0 && content_w >= 400.0 * cw - 1.0,
        "内容宽应 ≥ 真实最宽行（content={content_w:.1} shaped宽={shaped:.1} 400×charw={:.1}）",
        400.0 * cw
    );
}

#[gpui::test]
fn gutter_scroll_locked_to_text_scroll(cx: &mut TestAppContext) {
    // 回归（用户实测）：滚轮滚动时行号「翻动/闪烁」——旧实现是每次 render 手工按
    // `logical_scroll_top` 重建行号叠层，与 List 行绘制不同步。现在行号列是独立的
    // 第二个 `List`（独立 ListState），由 `sync_gutter_scroll` 与文本区逐帧锁步。
    let text = (1..=100).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let (host, cx) = open(cx, &text);
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (t0, g0) = (
        cx.update(|_, cx| ed.read(cx).test_text_scroll_top()),
        cx.update(|_, cx| ed.read(cx).test_gutter_scroll_top()),
    );
    assert_eq!(t0, g0, "初始行号应与文本对齐（{t0:?} vs {g0:?}）");
    // 向下滚 6 格（每格 3 行 ≈ 3×行高）
    let ed_b = cx.debug_bounds("code-editor").expect("编辑器应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + 200.),
        px(ed_b.origin.y.as_f32() + 100.),
    );
    for _ in 0..6 {
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: pos,
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-30.))),
            modifiers: Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        settle(cx);
    }
    let (t1, g1) = (
        cx.update(|_, cx| ed.read(cx).test_text_scroll_top()),
        cx.update(|_, cx| ed.read(cx).test_gutter_scroll_top()),
    );
    assert!(t1.0 > t0.0, "应已向下滚动（{t0:?} → {t1:?}）");
    assert_eq!(t1, g1, "滚动后行号应与文本锁步一致（{t1:?} vs {g1:?}）");
}

#[gpui::test]
fn empty_line_typing_keeps_hscroll_zero(cx: &mut TestAppContext) {
    // 回归（用户实测）：大文档里「空行输入内容时，界面会突然往右弹，然后又弹回」。
    // 根因：`reveal_caret` 横向分支在 col < margin 时把 hscroll 设成 col（逐字左移
    // 内容），等到 col=4 越过 margin 后走 col-right_edge 又 saturating 回 0 →
    // 每 4 个字符一次「往右弹又弹回」。修复：左缘 reveal 目标 = col-margin（col<margin
    // 时为 0，不动），右缘逻辑不变。
    // 文档含超长行以撑起横向量程（content_w > 视口）——否则 hscroll 量程为 0，
    // 弹跳被量程夹掉不可见。
    let text = "x".repeat(600) + "\n\n短行";
    let (host, cx) = open(cx, &text);
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let max0 = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(max0 > 0., "超长行应撑起横向量程（max={max0}）");
    // 光标放到空行（行 1，0 基）行首
    let off = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(1, 0));
    ed.update(cx, |ed, _| ed.set_caret_to(off));
    settle(cx);
    let h0 = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert_eq!(h0, 0., "空行(未横向滚动)初始 hscroll 应为 0");
    // 连续输入超 margin 个字符（跨过 0..3 的左缘 margin 区）→ hscroll 保持 0
    for c in "abcde".chars() {
        ed.update(cx, |ed, cx| ed.insert_text(&c.to_string(), cx));
        settle(cx);
        let h = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
        assert_eq!(h, 0., "空行输入 {c} 后不应触发横向滚动（hscroll={h}）");
    }
}

#[gpui::test]
fn hscroll_max_shrinks_after_deleting_widest_content(cx: &mut TestAppContext) {
    // 回归（用户实测）：横向滚动条拉到最右 → 末尾追加超宽内容（量程变大）→
    // 删除后量程不缩回。方案 A+B：删除置脏后（量程可见 + 首次/节流）精确重算使
    // max_line_units 收敛，render 再把 h_handle 夹回新量程——不逐键 O(n)。
    let (host, cx) = open(cx, "start\nline2");
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let base_max = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    // 光标移到文末，追加 600 个 x 的超宽内容
    let len = cx.update(|_, cx| host.read(cx).ed.read(cx).text_string().len());
    ed.update(cx, |ed, _| ed.set_caret_to(len));
    let wide = "x".repeat(600);
    ed.update(cx, |ed, cx| ed.insert_text(&wide, cx));
    settle(cx);
    let wide_max = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        wide_max > base_max + 1.0,
        "追加超宽内容后量程应增大（{base_max} → {wide_max}）"
    );
    // 撤销 = 删除刚追加的超宽内容 → 量程应缩回
    let w = window_handle(cx);
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-z").unwrap());
    settle(cx);
    let shrunk_max = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        shrunk_max <= base_max + 1.0,
        "删除超宽内容后量程应缩回（wide={wide_max} → shrunk={shrunk_max}，base={base_max}）"
    );
}

#[gpui::test]
fn hscroll_range_is_exact_not_quantized_and_reaches_content_end(cx: &mut TestAppContext) {
    // 回归（P7 治本）：历史补丁把横向量程按 64px 向上量化（每 ~8 字符一档），量程
    // 最多比真实内容宽多 63px → 滚到“真底”后内容右缘停在可视区末端左侧一段空白，
    // 且最宽行末尾输入要依赖「64px 量化 + 120ms 冷却」量程才不坍缩。
    // 现状：量程 = 容器**实测** `h_handle.max_offset().x` =（Buffer O(1) 精确的最宽行
    // 宽 − 文本区可视宽），不量化、无浮余：滚到最右恰好露出内容右缘（不缺内容），
    // 量程随最宽行逐字符精确变化（无 64px 一档突跳、不闪烁）。
    let text = "x".repeat(400) + "\n短行";
    let (host, cx) = open(cx, &text);
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let cw = cx.update(|_, cx| ed.read(cx).test_metrics().0).max(1.0);
    // 用实际布局出的文本区可视宽（`ce-harea` = 编辑区宽 − 行号列宽）做基准，
    // 不依赖测试窗硬编码宽度。
    let harea_w = cx
        .debug_bounds("ce-harea")
        .expect("文本区容器应渲染")
        .size
        .width
        .as_f32();
    let expected_exact = (400.0 * cw).max(harea_w) - harea_w;
    assert!(expected_exact > 0., "超宽行应撑起横向量程（expected={expected_exact:.1}）");
    let max_x = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        (max_x - expected_exact).abs() < 2.0,
        "横向量程应等于真实内容宽−可视宽（无 64px 量化上浮）：max={max_x:.1} 精确={expected_exact:.1}"
    );
    // 滚到最右（真实轮齿距事件）→ 应停在量程末端：内容右缘恰好落在可视区右缘
    // （旧量化会留下一段空白：right=可视−63px）。
    let ed_b = cx.debug_bounds("code-editor").expect("编辑器应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + 200.),
        px(ed_b.origin.y.as_f32() + 30.),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(1_000_000.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(cx);
    let h = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(
        (h - max_x).abs() < 4.0,
        "滚到最右应停在量程末端（h={h:.1} max={max_x:.1}）"
    );
    let right = 400.0 * cw - h;
    assert!(
        (right - harea_w).abs() < 8.0,
        "滚到底后内容右缘应恰好落在可视区右缘（right={right:.1} harea_w={harea_w:.1}）；旧 64px 量化会留下大段空白"
    );
    // 最宽行末尾继续输入：量程应逐字符精确增长（= 10×格宽），不是 64px 一档突跳。
    ed.update(cx, |ed, _| ed.set_caret_to(400));
    for _ in 0..10 {
        ed.update(cx, |ed, cx| ed.insert_text("x", cx));
    }
    settle(cx);
    let max2 = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    let grown_exact = expected_exact + 10.0 * cw;
    assert!(
        (max2 - grown_exact).abs() < 3.0,
        "最宽行末尾打字量程应逐字符精确增长（max2={max2:.1} expected={grown_exact:.1}）"
    );
}

#[gpui::test]
fn click_near_line_start_keeps_horizontal_scroll(cx: &mut TestAppContext) {
    // 回归（用户实测）：点击行首/前几个字符曾把内容强行拉去最左（内容左右晃动）。
    // 根因：鼠标点击也走 `reveal_caret` 的横向 margin 规则（col<margin → hscroll=col）。
    // 现在点击/拖动只做竖向 reveal，横向滚动只能由键盘/输入/滚轮驱动。
    let long = "x".repeat(600);
    let (host, cx) = open(cx, &format!("short\n{long}"));
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    // 先横向滚到右侧
    let ed_b = cx.debug_bounds("code-editor").expect("编辑器应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + 200.),
        px(ed_b.origin.y.as_f32() + lh * 0.5),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(400.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(cx);
    let before = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(before > 0., "应先横向滚动（before={before}）");
    // 点击短行行首（空白处/首个字符）——不得把内容拖回最左
    let y = px(lh * 0.5);
    let x = px(gutter + 2.0);
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(cx);
    let after = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(
        after.abs() >= before - 0.1,
        "点击不应重置横向滚动（before={before} → after={after}）"
    );
    // 空白行同样不应重置
    let blank_y = px(lh * 2.5);
    cx.simulate_mouse_move(point(x, blank_y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, blank_y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, blank_y), MouseButton::Left, mods);
    settle(cx);
    let after2 = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(
        after2.abs() >= before - 0.1,
        "点击行首空白也不应重置横向滚动（{before} → {after2}）"
    );
}

#[gpui::test]
fn typing_long_cjk_keeps_caret_visible(cx: &mut TestAppContext) {
    // 回归（用户实测）：内容超出窗口后，新输入不可见（光标没跟随）。
    // 根因：横向 reveal 用「字符列」，中文每字占 2 格 → 少滚一半。改按「宽度单位」
    // 列后，中文长行光标会滚回可视区。
    let (host, cx) = open(cx, "");
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (cw, _, _gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let visible = cx.update(|_, cx| ed.read(cx).test_visible_chars());
    // 打远超可视格的中文（400 字 = 800 格）
    cx.simulate_keystrokes(&"你".repeat(400));
    settle(cx);
    let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(hscroll > 0., "超窗输入应自动横向滚到光标处（hscroll={hscroll}）");
    // 光标近似 x（单位列 800 格）× 格宽 − 已滚像素应落在可视区 [0, 可视格×格宽]
    let caret_x_approx = 800.0 * cw;
    let visible_px = (visible.saturating_sub(4)) as f32 * cw;
    assert!(
        caret_x_approx - hscroll <= visible_px + cw * 2.0
            && caret_x_approx - hscroll + cw >= 0.0,
        "光标应滚回可视区（caret_x={caret_x_approx:.0} hscroll={hscroll:.0} 可视≈{visible_px:.0}）"
    );
}

#[gpui::test]
fn hscroll_bar_max_tracks_content_growth(cx: &mut TestAppContext) {
    // 回归（用户实测）：底部横向滚动条不随内容变化。量程应跟随 content_w_px 增长。
    let (host, cx) = open(cx, "");
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let max0 = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(max0 == 0., "短内容量程应为 0（max0={max0}）");
    // 输入超过可视宽度 → content_w_px 增长 → 量程应 > 0
    cx.simulate_keystrokes(&"x".repeat(400));
    settle(cx);
    let max1 = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        max1 > 0.,
        "内容变宽后横向滚动条量程应 > 0（max1={max1}）"
    );
    // 追加更长 → 量程继续增长
    cx.simulate_keystrokes(&"x".repeat(400));
    settle(cx);
    let max2 = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        max2 > max1 + 1.0,
        "更宽内容应让量程继续增长（{max1} → {max2}）"
    );
}

#[gpui::test]
fn vertical_range_stays_healthy_while_typing_in_widest_line(cx: &mut TestAppContext) {
    // 回归（用户实测）：在最宽长行末尾持续输入时，纵向滚动条“突然变长又变短”。
    // 旧根因：内容宽逐键变化 → gpui List 布局宽度变化时清空全部 size_hint →
    // 依赖 `max_offset_for_scrollbar()` 的量程坍缩，`ensure_uniform_hint` 的 120ms
    // 冷却又会阻止持续打字时的自愈（量程一直缩成极小）；曾用「64px 量化」临时绕开。
    // 现状（P7 治本）：纵向量程 = 确定性像素模型（`list_max_offset` = 全文行数 ×
    // 行高 − 视口），与 gpui 换宽清 size_hint 无关；量程恒健康，**无需冷却、无需
    // 量化**——持续输入最宽行末尾，量程立即正确（也无需 sleep 等自愈）。
    let mut text = "x".repeat(10000);
    for i in 1..=500 {
        text.push_str(&format!("\nline{i}"));
    }
    let (host, cx) = open(cx, &text);
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let healthy = cx.update(|_, cx| host.read(cx).ed.read(cx).list_max_offset());
    assert!(healthy > 6000., "初始纵向量程应健康（{healthy}）");
    // 光标移到最宽行（第 1 行）末尾
    ed.update(cx, |ed, _| ed.set_caret_to(10000));
    // 持续输入 80 字符（跨大量 64px 粒度）：量程必须始终健康（无坍缩、无冷却期）。
    for _ in 0..80 {
        ed.update(cx, |ed, cx| ed.insert_text("x", cx));
        settle(cx);
    }
    // 立即断言（不等任何冷却）——确定性量程随内容即时正确。
    let after = cx.update(|_, cx| host.read(cx).ed.read(cx).list_max_offset());
    assert!(after > 6000., "持续输入后纵向量程应保持健康（{after}）");
}

#[gpui::test]
fn reveal_keeps_caret_visible_while_typing_in_long_line(cx: &mut TestAppContext) {
    // 回归（用户实测）：长行末尾持续输入时「横向滚动条算错，最新内容看不见」。
    // 根因：`reveal_caret` 把带 +8 估算余量的 visible_chars 当真实可视格数 → 光标被
    // 滚到真实右缘之外 ~4 格（实测 screen=1911 > 视口1883）。修复：先减掉 +8 余量。
    let mut text = "x".repeat(10000);
    for i in 1..=500 {
        text.push_str(&format!("\nline{i}"));
    }
    let (host, cx) = open(cx, &text);
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (cw, _, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let viewport_px = 1920.0 - gutter; // 测试窗 1920 宽
    ed.update(cx, |ed, _| ed.set_caret_to(10000));
    for _ in 0..60 {
        ed.update(cx, |ed, cx| ed.insert_text("x", cx));
        settle(cx);
        let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
        let caret_units = cx.update(|_, cx| host.read(cx).ed.read(cx).test_caret_units());
        let screen = caret_units as f32 * cw - hscroll;
        assert!(
            (0.0..=viewport_px + 0.5).contains(&screen),
            "光标应滚回可视区（screen={screen:.0} ≤ 视口{viewport_px:.0}）"
        );
    }
}

#[gpui::test]
fn scroll_range_survives_width_change(cx: &mut TestAppContext) {
    // 回归（2026-09-15 实机）：gpui 在宽度变化时清空全部 size_hint，未测条目高度按
    // 0 计 → 依赖 `max_offset_for_scrollbar()` 的量程坍缩、滚动条量程失真。
    // 现状（P7 治本）：纵向量程 = 确定性像素模型（`list_max_offset` = 全文行数 ×
    // 行高 − 视口），与 gpui 换宽清 size_hint **无关**；已删除 `ensure_uniform_hint`
    // 的 120ms 换宽冷却——换宽后量程**立即**健康，无需等冷却自愈。
    let text = (1..=300)
        .map(|i| format!("第{i}行"))
        .collect::<Vec<_>>()
        .join("\n");
    let (host, cx) = open(cx, &text);
    // 横向内容宽度跟随窗口宽（短文档 content=avail_w），换宽让 gpui List 清空全部
    // size_hint 也无所谓——量程是确定性 `reach_px`，resize 落定即正确（不等冷却）。
    fn reflow_and_assert(host: &Entity<Host>, cx: &mut VisualTestContext, w: f32) -> f32 {
        cx.simulate_resize(size(px(w), px(600.)));
        // 多 pump 几帧等 viewport_bounds 布局落定，但不 sleep：无冷却可等。
        settle(cx);
        let m = cx.update(|_, cx| host.read(cx).ed.read(cx).list_max_offset());
        assert!(m > 1000., "换宽后量程应立即健康（m={m:.1} w={w}）");
        m
    }
    let mut last_max = reflow_and_assert(&host, cx, 300.);
    assert!(
        last_max > 1000.,
        "首次渲染量程应接近 300 行（last_max={last_max:.1}）"
    );
    // 再换两次宽 → 量程保持健康（无冷却依赖）。
    for w in [440., 520.] {
        last_max = reflow_and_assert(&host, cx, w);
    }
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1);
    let pad = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bottom_pad_px());
    assert!(
        (last_max - (300.0 * lh + pad - 600.0)).abs() < lh * 4.,
        "量程应≈300×行高+底部预留−视口高（{last_max:.1} vs {:.1}）",
        300.0 * lh + pad - 600.0
    );
}

// ---------------------------------------------------------------------------
// 粘贴/列表窗口（回归：空标签粘贴只显示首行）
// ---------------------------------------------------------------------------

#[gpui::test]
fn empty_new_tab_paste_not_collapsed_to_first_row(cx: &mut TestAppContext) {
    // 回归（用户实测「新建空标签 → 粘贴多行 → 编辑器只显示第一行」）：
    // `CodeEditor::new` 建空标签时 ListState 若按 `row_count`(1) 初始化，则固定窗口
    // 虚拟化从未建立（`resync_list` 只在 set_text/开文件/撤销重做时重置为
    // `LIST_WINDOW_ROWS`）；`apply_desired_to_window` 的「窗口覆盖」判定又会误判
    // 无需补建 → 粘贴 300 行后列表仍只有 1 项 → `list()` 只渲染首行。
    // 修复：`new()` 直接以固定窗口 `LIST_WINDOW_ROWS` 初始化。
    use crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS as WINDOW;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let ed = cx.new(|cx| CodeEditor::new(cx));
        Host { ed }
    });
    let cx: &mut VisualTestContext = cx;
    settle(cx);
    let win0 = cx.update(|_, cx| host.read(cx).ed.read(cx).test_list_window_len());
    assert_eq!(
        win0, WINDOW,
        "空标签首帧 ListState 就应为固定窗口 {WINDOW} 项（而不是 1 项）"
    );
    // 粘贴 300 行多行文本。
    let text = (0..300).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    ed.update(cx, |ed, cx| ed.paste_text(&text, cx));
    settle(cx);
    let (lc, vis, win, sic) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.read(cx);
        (
            ed.line_count(),
            ed.test_folds().visual_line_count(),
            ed.test_list_window_len(),
            ed.test_scroll_item_count(),
        )
    });
    assert_eq!(lc, 300, "数据行数应完整");
    assert_eq!(vis, 300, "视觉行数应完整");
    assert!(sic >= vis, "滚动量程应覆盖全部视觉行（sic={sic} vis={vis}）");
    assert_eq!(
        win, WINDOW,
        "粘贴后固定窗口条目数应保持 = {WINDOW}（实际 {win}）——否则只渲染首行"
    );
}

// ---------------------------------------------------------------------------
// M5-B2-3 行级缓存：无编辑滚动多帧后渲染不变 / 编辑后缓存失效
// ---------------------------------------------------------------------------

#[gpui::test]
fn row_cache_reuses_scrolled_rows_and_invalidates_on_edit(cx: &mut TestAppContext) {
    // M5-B2-3：`build_rows` 行级缓存——滚动帧只物化新进窗口的行。重连线序断言：
    // 1) 无编辑滚动多帧后，重叠行渲染（文本 + 样式段）逐位不变且命中缓存（浅复用）；
    // 2) 编辑（buffer.version()+1）后缓存键失效，同一窗口下行取数路径如实反映新文本，
    //    未编辑行不受影响。
    let text = (0..200)
        .map(|i| format!("line{i:03} fn f{i}() {{ let v = {i}; // {i} }}"))
        .collect::<Vec<_>>()
        .join("\n");
    let (host, cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());

    // 帧 1：窗口 A = 逻辑行 0..50（真实 build_rows 路径，与 render 一帧同源）。
    let a = cx.update(|window, cx| {
        ed.update(cx, |ed, _| ed.test_build_rows_snapshot(window, 0, 50))
    });
    assert_eq!(a.len(), 51, "窗口 A 应含 51 行");

    // 帧 2：无编辑滚动 +10 行 → 窗口 B = 10..60。
    let hits0 = cx.update(|_, cx| ed.read(cx).test_row_cache().0);
    let b = cx.update(|window, cx| {
        ed.update(cx, |ed, _| ed.test_build_rows_snapshot(window, 10, 60))
    });
    let hits1 = cx.update(|_, cx| ed.read(cx).test_row_cache().0);

    // 无编辑滚动多帧后渲染不变：B 与 A 重叠的行（10..49）逐位一致。
    for i in 10..50 {
        assert_eq!(b[i - 10].0, a[i].0, "滚动复用行文本应逐位一致（行 {i}）");
        assert_eq!(b[i - 10].1, a[i].1, "滚动复用行样式应逐位一致（行 {i}）");
    }
    // 滚动帧只物化新进窗口的行：41 行重叠应基本全命中（而非整窗重物化）。
    assert!(
        hits1 - hits0 >= 40,
        "无编辑滚动应大量复用缓存（hits {hits0} → {hits1}）"
    );

    // 帧 3：编辑窗口内的行 20 → version+1 → 缓存键整体失效 → 数据路径反映新文本。
    let off = cx.update(|_, cx| ed.read(cx).offset_for_line_col(20, 4));
    cx.update(|_, cx| {
        ed.update(cx, |ed, cp| {
            ed.set_caret_to(off);
            ed.insert_text("X", cp);
        })
    });
    let c = cx.update(|window, cx| {
        ed.update(cx, |ed, _| ed.test_build_rows_snapshot(window, 10, 60))
    });
    assert!(
        c[10].0.contains('X'),
        "编辑后窗口行 20 应反映新文本（缓存未吞编辑）: {:?}",
        c[10].0
    );
    // 未编辑行（行 11 / 22）文本不受编辑影响：缓存失效不引入失真。
    assert_eq!(c[0].0, b[0].0, "未编辑行文本不应漂移（行 10）");
    assert_eq!(c[12].0, b[12].0, "未编辑行文本不应漂移（行 22）");
}

/// 方案2 护栏：`apply_live_replay` 的「先干跑自检后落盘」契约——基线一致时按序重放成功、
/// 产物 == 期望全文；基线错位（越界 / 切中多字节字符 / 终态不符，如 CRLF/BOM/漏 op 的
/// 偏移漂移）时**不改动缓冲区**、返回 `false`，由调用方（`app::commit_live`）回退行级
/// diff。若护栏失败也把缓冲区改掉，fallback 的 diff 基线就会取到污损文本，内容必错。
#[gpui::test]
fn live_replay_guard_never_mutates_on_drift(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let ed = cx.new(|cx| CodeEditor::new(cx));
        // 基线一致（happy path）：1 条 op 按序重放 → true 且内容 == 期望。
        ed.update(cx, |ed, _| {
            ed.set_text("a\nb\n");
            let ops = vec![(0..0, gpui::SharedString::from("+"))];
            assert!(ed.apply_live_replay(&ops, "+a\nb\n"), "基线一致时应重放成功");
            assert_eq!(ed.text_string(), "+a\nb\n", "重放后内容应等于期望全文");
        });
        // 终态错位：op 重放到本缓冲区的产物 != 期望（模拟外部漂移基线）→ 不改动、false。
        ed.update(cx, |ed, _| {
            ed.set_text("a\nb\n");
            let before = ed.text_string();
            let ops = vec![(0..0, gpui::SharedString::from("+"))];
            assert!(
                !ed.apply_live_replay(&ops, "+X\na\nb\n"),
                "终态不符时应判定漂移并拒绝"
            );
            assert_eq!(ed.text_string(), before, "护栏失败不得改动缓冲区");
        });
        // 越界错位：op 区间超出当前文档尾 → 不改动、false。
        ed.update(cx, |ed, _| {
            ed.set_text("a\nb\n");
            let before = ed.text_string();
            let ops = vec![(99..120, gpui::SharedString::from("x"))];
            assert!(
                !ed.apply_live_replay(&ops, "x"),
                "越界 op 应被护栏拒绝"
            );
            assert_eq!(ed.text_string(), before, "越界时不得改动缓冲区");
        });
        // 切中多字节字符：op 区间切破 UTF-8 字符边界 → 不改动、false。
        ed.update(cx, |ed, _| {
            ed.set_text("中文内容");
            let before = ed.text_string();
            let ops = vec![(1..2, gpui::SharedString::from("X"))];
            assert!(
                !ed.apply_live_replay(&ops, "X中文内容"),
                "切中字符边界的 op 应被护栏拒绝"
            );
            assert_eq!(ed.text_string(), before, "切中字符时不得改动缓冲区");
        });
    });
}


