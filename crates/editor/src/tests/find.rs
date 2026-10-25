
// ---------------------------------------------------------------------------
// 编辑区内查找（Ctrl+F / F3）——行为表 + 错误映射的 `#[gpui::test]` 交互断言
// ---------------------------------------------------------------------------

use gpui::Hsla;
use std::ops::Range;

/// 取编辑器实体句柄（`host.read` 须经 `cx.update` 内部，见既有 helper 姿势）。
fn ed_of(host: &Entity<Host>, cx: &mut VisualTestContext) -> Entity<CodeEditor> {
    cx.update(|_, cx| host.read(cx).ed.clone())
}

/// 打开 find 条（模拟 Ctrl+F 分发后的落点）+ 返回输入框实体句柄。
fn open_find(host: &Entity<Host>, cx: &mut VisualTestContext) -> Entity<ui::input::InputState> {
    let ed = ed_of(host, cx);
    cx.update(|window, cx| {
        ed.update(cx, |e, ecx| e.open_find(window, ecx));
    });
    settle(cx);
    cx.update(|_, cx| ed.read(cx).test_find_input().cloned().unwrap())
}

/// 驱动输入框文本变化（等同用户打字 → 订阅置脏 + 防抖），并同步走一次重扫落地。
/// 注意：gpui 事件订阅回调在**效果刷新区间**（`run_until_parked`）投递，非 emit 同步——
/// 故 set_value 后须先 settle 让钩子把查询词写进状态，再手动触发重扫。
fn type_query(
    host: &Entity<Host>,
    cx: &mut VisualTestContext,
    q: &str,
    input: &Entity<ui::input::InputState>,
) {
    let ed = ed_of(host, cx);
    cx.update(|window, cx| {
        input.update(cx, |s, scx| s.set_value(q, window, scx));
    });
    settle(cx);
    cx.update(|window, cx| {
        ed.update(cx, |e, ecx| e.test_find_scan_now(window, ecx));
    });
    settle(cx);
}

/// 打开即聚焦输入框、不带查询；已输入过则再次打开回显最近查询。
#[gpui::test]
fn find_open_focuses_input_and_recalls_query(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "aa bb aa cc aa");
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_find(&host, cx);

    let (open, _, n, _, q) = cx.update(|_, cx| ed.read(cx).test_find_state());
    assert!(open, "Ctrl+F 应打开 find 条");
    assert_eq!(n, 0, "首次打开不自动计算匹配");
    assert!(q.is_empty(), "无最近查询 → 空查询词");
    let input_focused = cx.update(|window, cx| {
        let i = input.read(cx).focus_handle(cx);
        window.focused(cx) == Some(i)
    });
    assert!(input_focused, "打开后输入框应自动聚焦");

    // 输入 + 关闭（Esc）→ 保留上次查询；再次 Ctrl+F 回显。
    type_query(&host, cx, "aa", &input);
    cx.update(|window, cx| {
        ed.update(cx, |e, ecx| e.close_find(window, ecx));
    });
    settle(cx);
    let (open, _, n, _, _) = cx.update(|_, cx| ed.read(cx).test_find_state());
    assert!(!open && n == 0, "Esc 应关条并移除全部高亮");
    let input2 = open_find(&host, cx);
    let q2 = cx.update(|_, cx| ed.read(cx).test_find_state().4);
    assert_eq!(q2, "aa", "再次打开应回显最近查询");
    let shown = cx.update(|_, cx| input2.read(cx).value().to_string());
    assert_eq!(shown, "aa", "输入框文本应回显最近查询");
}

/// 输入词 → 防抖后：高亮全部匹配 + 首个为当前命中 + 状态「i/N 个匹配」。
#[gpui::test]
fn find_input_highlights_all_and_first_as_current(cx: &mut TestAppContext) {
    let text = "hello world hello\nHELLO hi hello\n别处";
    let (host, cx) = open(cx, text);
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_find(&host, cx);

    type_query(&host, cx, "hello", &input);

    let (_, vis, n, cur, q) = cx.update(|_, cx| ed.read(cx).test_find_state());
    assert!(vis, "输入后应绘制高亮");
    assert_eq!(n, 4, "大小写不敏感命中 4 处：行 0×2 + 行 1（HELLO/hello）×2");
    assert_eq!(cur, Some(0), "首个匹配为当前命中");
    assert_eq!(q, "hello");
    assert_eq!(cx.update(|_, cx| ed.read(cx).test_find_status()), "1/4 个匹配");

    // 渲染层：行 0 消费段含两段查找底色、当前（0..5）与非当前（12..17）底色不同。
    let row0 = cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| e.test_build_rows_snapshot(window, 0, 0))
    });
    let row0 = &row0[0].1;
    let colors = cx.update(|_, cx| ui::theme::Theme::global(cx).colors);
    let want_cur: Hsla = colors.editor_find_current_bg.into();
    let want_other: Hsla = colors.editor_find_match_bg.into();
    let segs: Vec<(Range<usize>, Hsla)> = row0
        .iter()
        .filter_map(|(r, s)| s.background_color.map(|bg| (r.clone(), bg)))
        .collect();
    assert_eq!(segs.len(), 2, "行 0 应有 2 段查找底色：{segs:?}");
    assert_eq!(segs[0].0, 0..5, "首个匹配（当前）");
    assert_eq!(segs[0].1, want_cur, "当前命中用强调色");
    assert_eq!(segs[1].0, 12..17, "第二个匹配（非当前）");
    assert_eq!(segs[1].1, want_other, "其余匹配用普通底色");
}

/// Enter=下一个、Shift+Enter（`find_prev`）上一个；跨循环回绕；滚动使当前命中可见。
#[gpui::test]
fn find_next_prev_wrap_and_reveal(cx: &mut TestAppContext) {
    let lines: Vec<String> = (0..80).map(|i| format!("needle-{i}")).collect();
    let (host, cx) = open(cx, &lines.join("\n"));
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_find(&host, cx);
    type_query(&host, cx, "needle", &input);

    let ls1 = cx.update(|_, cx| ed.read(cx).offset_for_line_col(1, 0));
    let ls79 = cx.update(|_, cx| ed.read(cx).offset_for_line_col(79, 0));

    // 当前 = 首个（行 0）。
    let rng = cx.update(|_, cx| ed.read(cx).test_find_current_range());
    assert_eq!(rng, Some(0..6), "「needle」在行 0 起点");

    // Enter：0 → 1（行 1）。
    cx.update(|_, cx| ed.update(cx, |e, ecx| e.find_next(ecx)));
    let (cur, pos) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.test_find_current_range(), e.cursor_position())
    });
    assert_eq!(cur, Some(ls1..ls1 + 6), "第二个匹配在行 1");
    assert_eq!(pos, (1, 0), "光标移动到命中行首列");

    // 滚动可见：命中行在视口内。
    let top = cx.update(|_, cx| ed.read(cx).viewport_top_line());
    assert!(top <= 1 && 1 < top + 60, "命中行应滚进可视区：viewport_top={top}");

    // Shift+Enter（find_prev）：1 → 0。
    cx.update(|_, cx| ed.update(cx, |e, ecx| e.find_prev(ecx)));
    let cur = cx.update(|_, cx| ed.read(cx).test_find_current_range());
    assert_eq!(cur, Some(0..6), "上一个回到行 0");

    // 回绕：从行 0 上一个 → 末尾（行 79）。
    cx.update(|_, cx| ed.update(cx, |e, ecx| e.find_prev(ecx)));
    let cur = cx.update(|_, cx| ed.read(cx).test_find_current_range());
    assert_eq!(cur, Some(ls79..ls79 + 6), "开头回绕到末尾（行 79）");
    let _ = input;
}

/// find 条内按 Enter 不插入换行（编辑器 on_action(&Enter) 被 find 条吞掉）。
#[gpui::test]
fn find_bar_enter_does_not_insert_newline(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "aa aa aa");
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let _ = open_find(&host, cx);
    let input = cx.update(|_, cx| ed.read(cx).test_find_input().cloned().unwrap());
    type_query(&host, cx, "aa", &input);
    let before = editor_text(&host, cx);
    // 输入框持焦：按 Enter（key_down 走 find 条导航；Enter action 被吞 → 不换行）。
    cx.simulate_keystrokes("enter");
    for _ in 0..4 {
        cx.run_until_parked();
    }
    let after = editor_text(&host, cx);
    assert_eq!(after, before, "find 条内 Enter 不得向编辑器插入换行");
    let cur = cx.update(|_, cx| ed.read(cx).test_find_current_range());
    assert_eq!(cur, Some(3..5), "Enter 应导航到下一个匹配");
}

/// Esc 关闭：移除全部高亮、保留 last query 供 F3 复用、焦点归还编辑器。
#[gpui::test]
fn find_esc_clears_highlights_and_restores_focus(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "aa bb aa");
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_find(&host, cx);
    type_query(&host, cx, "aa", &input);
    let (_, vis, n, _, _) = cx.update(|_, cx| ed.read(cx).test_find_state());
    assert!(vis && n == 2, "输入后有高亮");

    cx.update(|window, cx| {
        ed.update(cx, |e, ecx| e.close_find(window, ecx));
    });
    settle(cx);
    let (open, vis, n, cur, q) = cx.update(|_, cx| ed.read(cx).test_find_state());
    assert!(!open && !vis && n == 0 && cur.is_none(), "Esc 清高亮");
    assert_eq!(q, "aa", "last query 保留");
    let focused = cx.update(|window, cx| {
        let fh = ed.read(cx).focus_handle();
        window.focused(cx) == Some(fh)
    });
    assert!(focused, "Esc 后焦点归还编辑器");
    let _ = input;
}

/// F3/Shift+F3：复用上次查询（关条后亦可）——重新拉起高亮并从首个走下一个/上一个。
#[gpui::test]
fn find_f3_reuses_last_query_when_closed(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "foo foo foo");
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_find(&host, cx);
    type_query(&host, cx, "foo", &input);
    // 关条（Esc）：高亮清除、last_query="foo"。
    cx.update(|window, cx| {
        ed.update(cx, |e, ecx| e.close_find(window, ecx));
    });
    settle(cx);

    // F3（关条）：复用上次查询 → 重新高亮 + 从首个走下一个（匹配 1）。
    cx.update(|_, cx| ed.update(cx, |e, ecx| e.find_next(ecx)));
    let (vis, n, cur) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.test_find_state().1, e.test_find_state().2, e.test_find_current_range())
    });
    assert!(vis && n == 3, "F3 应重新拉起高亮");
    assert_eq!(cur, Some(4..7), "F3 复用「foo」从首个走到下一个");

    // Shift+F3：上一个 → 0。
    cx.update(|_, cx| ed.update(cx, |e, ecx| e.find_prev(ecx)));
    let cur = cx.update(|_, cx| ed.read(cx).test_find_current_range());
    assert_eq!(cur, Some(0..3), "Shift+F3 回到首个");
    let _ = input;
}

/// F3：从未查找过（无上次查询）→ 不动作。
#[gpui::test]
fn find_f3_no_last_query_noop(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "nothing here");
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let before = cx.update(|_, cx| ed.read(cx).selection().head());
    cx.update(|_, cx| ed.update(cx, |e, ecx| e.find_next(ecx)));
    cx.update(|_, cx| ed.update(cx, |e, ecx| e.find_prev(ecx)));
    let after = cx.update(|_, cx| ed.read(cx).selection().head());
    assert_eq!(before, after, "无上次查询 → F3/Shift+F3 不动作");
}

/// 空文档：find 条正常打开、状态「无匹配」，不崩。
#[gpui::test]
fn find_empty_doc_ok(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "");
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_find(&host, cx);
    type_query(&host, cx, "x", &input);
    let (open, vis, n, cur) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.test_find_state().0, e.test_find_state().1, e.test_find_state().2, e.test_find_state().3)
    });
    assert!(open, "空文档 find 条正常打开");
    // 重扫落地后「高亮层已武装见」但无匹配可画（is_active=false），状态「无匹配」。
    assert_eq!((vis, n, cur), (true, 0, None), "空文档无匹配");
    assert_eq!(cx.update(|_, cx| ed.read(cx).test_find_status()), "无匹配");
}

/// 有内容但无匹配：状态「无匹配」，Enter/Shift+Enter 不动作、不回绕。
#[gpui::test]
fn find_no_match_enter_noop(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "abc def ghi");
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_find(&host, cx);
    type_query(&host, cx, "zzz", &input);
    assert_eq!(cx.update(|_, cx| ed.read(cx).test_find_status()), "无匹配");
    let head0 = cx.update(|_, cx| ed.read(cx).selection().head());
    cx.update(|_, cx| ed.update(cx, |e, ecx| e.find_next(ecx)));
    cx.update(|_, cx| ed.update(cx, |e, ecx| e.find_prev(ecx)));
    let head1 = cx.update(|_, cx| ed.read(cx).selection().head());
    assert_eq!(head0, head1, "无匹配时 Enter/Shift+Enter 不动作");
    let _ = input;
}

/// 匹配上限截断：>FIND_MATCH_CAP 匹配 → 只收集上限个数、状态「>N 个匹配」、导航限内回绕。
#[gpui::test]
fn find_cap_truncates_and_status(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, &"x ".repeat(6000));
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_find(&host, cx);
    type_query(&host, cx, "x", &input);
    let (n, cur) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.test_find_state().2, e.test_find_current_range())
    });
    assert_eq!(n, crate::editor::find::FIND_MATCH_CAP, "达上限停止收集");
    assert_eq!(cur, Some(0..1), "首 N 个匹配快显且首个为当前");
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).test_find_status()),
        format!(">{} 个匹配", crate::editor::find::FIND_MATCH_CAP)
    );
    // 导航在 0..5000 内回绕（从 0 向前到上限末）。
    cx.update(|_, cx| ed.update(cx, |e, ecx| e.find_prev(ecx)));
    let last_off = (crate::editor::find::FIND_MATCH_CAP - 1) * 2;
    let cur = cx.update(|_, cx| ed.read(cx).test_find_current_range());
    assert_eq!(cur, Some(last_off..last_off + 1), "开头回绕到上限末");
}

/// 文本变更失效重算：编辑后匹配表按 (buffer 版本, 查询词) 失效，导航重扫出新结果。
#[gpui::test]
fn find_invalidates_after_edit(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "foo foo");
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_find(&host, cx);
    type_query(&host, cx, "foo", &input);
    assert_eq!(cx.update(|_, cx| ed.read(cx).test_find_state().2), 2, "初始 2 个匹配");

    // 在文末插入一个 foo → 3 个匹配。
    cx.update(|_, cx| {
        ed.update(cx, |e, ecx| e.insert_text(" foo", ecx));
    });
    settle(cx);
    // 编辑置脏后 F3/Enter 导航触发重扫 → 当前重定位到首个、随后走到下一个（4..7）。
    cx.update(|_, cx| ed.update(cx, |e, ecx| e.find_next(ecx)));
    let (n, cur) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.test_find_state().2, e.test_find_current_range())
    });
    assert_eq!(n, 3, "编辑后重算应含新匹配（缓存按版本失效）");
    assert_eq!(cur, Some(4..7), "从重定位的首个走到下一个（第二个 foo）");
    let _ = input;
}

/// 中文/emoji 前的查找：字节偏移不串位（UTF-16/IME 安全，行内命中恰在字符边界）。
#[gpui::test]
fn find_cjk_emoji_byte_offsets_no_shift(cx: &mut TestAppContext) {
    let text = "中文查找查找🎉\n下一行查找";
    let (host, cx) = open(cx, text);
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_find(&host, cx);
    type_query(&host, cx, "查找", &input);
    let m = cx.update(|_, cx| ed.read(cx).test_find_matches());
    assert_eq!(m, vec![6..12, 12..18, 32..38], "中文/emoji 前匹配字节区间不串位");
    let cur = cx.update(|_, cx| ed.read(cx).test_find_current_range());
    assert_eq!(cur, Some(6..12), "首个命中=行 0 第一处");
}

/// 打开 find 条并渲染多帧 + 滚动：不破坏 scrollbar 命中/滚动/覆盖定位（不崩 + 可滚）。
#[gpui::test]
fn find_bar_does_not_break_scrolling(cx: &mut TestAppContext) {
    let lines: Vec<String> = (0..200).map(|i| format!("row-{i}")).collect();
    let (host, cx) = open(cx, &lines.join("\n"));
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_find(&host, cx);
    type_query(&host, cx, "row", &input);
    // 多帧渲染（find 条常开）+ 滚动到底：编辑器照常可滚、窗口基址推进。
    for _ in 0..4 {
        let _ = cx.refresh();
        cx.run_until_parked();
    }
    cx.update(|_, cx| ed.update(cx, |e, _| e.test_scroll_to_bottom()));
    settle(cx);
    let top_row = cx.update(|_, cx| ed.read(cx).viewport_top_line());
    assert!(top_row > 150, "find 条打开时竖向滚动仍生效：viewport_top={top_row}");
    let _ = input;
}

/// 大文档（约 130k 行）匹配收集：到上限截断、状态「>N 个匹配」、不崩。
/// 默认 `#[ignore]`（真机墙钟探针——收集墙钟在 debug 下随机器抖动，K3 只比相对值）；
/// 常规套件用中小夹具覆盖同一逻辑（`find_cap_truncates_and_status`）。
#[gpui::test]
#[ignore = "大文档真机墙钟探针，手动运行：只验证不崩 + 上限截断"]
fn find_large_doc_ignored_cap(cx: &mut TestAppContext) {
    let lines: Vec<String> = (0..130_000).map(|i| format!("log line {i} needle-{i}")).collect();
    let (host, cx) = open(cx, &lines.join("\n"));
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_find(&host, cx);
    type_query(&host, cx, "needle", &input);
    let (n, status) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.test_find_state().2, e.test_find_status())
    });
    assert_eq!(n, crate::editor::find::FIND_MATCH_CAP, "大文档命中应被上限截断");
    assert_eq!(status, format!(">{} 个匹配", crate::editor::find::FIND_MATCH_CAP));
    let _ = input;
}
