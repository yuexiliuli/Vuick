
// ---------------------------------------------------------------------------
// 编辑区内跳转行（Ctrl+G）——行为表 + 错误映射的 `#[gpui::test]` 交互断言
// ---------------------------------------------------------------------------

use std::time::Duration;

/// 打开跳转行卡片（模拟 Ctrl+G 分发后的落点）+ 返回输入框实体句柄。
fn open_goto(host: &Entity<Host>, cx: &mut VisualTestContext) -> Entity<ui::input::InputState> {
    let ed = ed_of(host, cx);
    cx.update(|window, cx| {
        ed.update(cx, |e, ecx| e.open_goto(window, ecx));
    });
    settle(cx);
    cx.update(|_, cx| ed.read(cx).test_goto_input().cloned().unwrap())
}

/// 驱动输入框文本变化（等同用户打字 → 订阅置脏 + 防抖 + 即时刷新提示）。
/// `flush=true` 时**确定性同步**走一次落地跳转（fake 执行器不轮询 300ms 防抖定时器，
/// 与 `test_find_scan_now` 同姿势）；`flush=false` 只写文本（供 Enter/时钟推进类测试）。
fn drive_goto(
    host: &Entity<Host>,
    cx: &mut VisualTestContext,
    input: &Entity<ui::input::InputState>,
    text: &str,
    flush: bool,
) {
    cx.update(|window, cx| {
        input.update(cx, |s, scx| s.set_value(text, window, scx));
    });
    settle(cx); // 订阅回调在效果刷新区间投递：先 park 让文本写进状态 + 提示刷新
    if flush {
        let ed = ed_of(host, cx);
        cx.update(|window, cx| {
            ed.update(cx, |e, ecx| e.test_goto_flush_now(window, ecx));
        });
        settle(cx);
    }
}
/// 走**真实防抖路径**：写文本 → 订阅置脏 → 推进时钟跨过 GOTO_DEBOUNCE_MS → 落位。
fn drive_goto_debounce(
    cx: &mut VisualTestContext,
    input: &Entity<ui::input::InputState>,
    text: &str,
) {
    cx.update(|window, cx| {
        input.update(cx, |s, scx| s.set_value(text, window, scx));
    });
    settle(cx);
    cx.executor().advance_clock(Duration::from_millis(320));
    settle(cx);
}

/// 打开即聚焦输入框、卡片渲染于编辑区顶部居中（几何断言 + 元素齐备）。
#[gpui::test]
fn goto_open_focuses_input_card_at_top_center(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "aa\nbb\ncc\n");
    focus(&host, cx);
    let input = open_goto(&host, cx);
    let ed = ed_of(&host, cx);

    let (open, cached, _, hint, err) = cx.update(|_, cx| ed.read(cx).test_goto_state());
    assert!(open, "Ctrl+G 应打开跳转行卡片");
    assert!(cached.is_empty(), "首次打开无输入缓存");
    assert!(hint.is_empty() && !err, "首次打开无提示");

    let input_focused = cx.update(|window, cx| {
        let i = input.read(cx).focus_handle(cx);
        window.focused(cx) == Some(i)
    });
    assert!(input_focused, "打开后输入框应自动聚焦");

    // 卡片顶部居中（几何断言）：贴顶 ~8px、水平中心 ≈ 编辑区中心、元素齐备。
    let card = cx.debug_bounds("goto-card").expect("goto 卡片应渲染");
    let ed = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    assert!(
        (card.origin.y - ed.origin.y).as_f32().abs() < 12.,
        "卡片应贴编辑区顶部内缘（~8px）：card={card:?}"
    );
    let ed_center = ed.origin.x + ed.size.width / 2.;
    let card_center = card.origin.x + card.size.width / 2.;
    assert!(
        (card_center - ed_center).as_f32().abs() < 2.,
        "卡片应水平居中于编辑区（card_center={card_center:?} ed_center={ed_center:?}）"
    );
    // 紧凑：整卡宽 < 编辑区宽 1/2（不遮右上 find 卡的最小结构保证）。
    assert!(card.size.width < ed.size.width / 2., "goto 卡片应紧凑（{card:?}）");
    assert!(cx.debug_bounds("goto-card-input").is_some(), "goto-card-input 应渲染");
    assert!(cx.debug_bounds("goto-card-hint").is_some(), "goto-card-hint 应渲染");
    assert!(cx.debug_bounds("find-bar").is_none(), "find 未开不渲染");
}

/// 逐键实时跳（**真实防抖路径**，推进时钟跨过防抖窗）：1 → 12 → 123 → 1234 每键
/// 落位正确（光标落位 + 滚动可见）。
#[gpui::test]
fn goto_realtime_jumps_per_key_with_debounce(cx: &mut TestAppContext) {
    let lines: Vec<String> = (0..2000).map(|i| format!("line-{i}")).collect();
    let (host, cx) = open(cx, &lines.join("\n"));
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_goto(&host, cx);

    drive_goto_debounce(cx, &input, "1");
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).cursor_position()),
        (0, 0),
        "按 1 → 第 1 行"
    );
    drive_goto_debounce(cx, &input, "12");
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).cursor_position()),
        (11, 0),
        "按 2（12）→ 第 12 行"
    );
    drive_goto_debounce(cx, &input, "123");
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).cursor_position()),
        (122, 0),
        "按 3（123）→ 第 123 行"
    );
    drive_goto_debounce(cx, &input, "1234");
    let (pos, top) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.cursor_position(), e.viewport_top_line())
    });
    assert_eq!(pos, (1233, 0), "按 4（1234）→ 第 1234 行");
    assert!(
        top >= 1233usize.saturating_sub(1) && top <= 1233,
        "目标行应滚到视口顶附近：viewport_top={top}"
    );
    // 每键提示均为「→ 第 X 行 第 1 列」且非错误态。
    let (_, _, _, hint, err) = cx.update(|_, cx| ed.read(cx).test_goto_state());
    assert_eq!((hint.as_str(), err), ("→ 第 1234 行 第 1 列", false));
}

/// Enter：立即 flush（取消防抖、当场跳转），卡片**不关**、输入框保持焦点。
#[gpui::test]
fn goto_enter_flushes_keeps_card_and_focus(cx: &mut TestAppContext) {
    let lines: Vec<String> = (0..60).map(|i| format!("line-{i}")).collect();
    let (host, cx) = open(cx, &lines.join("\n"));
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_goto(&host, cx);

    // 输入 42（未落地）→ 光标不动、提示预览「→ 第 42 行 第 1 列」。
    drive_goto(&host, cx, &input, "42", false);
    let (pos_before, hint) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.cursor_position(), e.test_goto_state().3)
    });
    assert_eq!(pos_before, (0, 0), "防抖未到期（未按 Enter）不应跳转");
    assert_eq!(hint.as_str(), "→ 第 42 行 第 1 列", "输入即刷新目标预览");

    // Enter → 立即 flush（取消防抖）：跳 42、卡片保持打开、输入框保持焦点。
    cx.simulate_keystrokes("enter");
    for _ in 0..4 {
        cx.run_until_parked();
    }
    let (pos, open, hint2) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.cursor_position(), e.test_goto_state().0, e.test_goto_state().3)
    });
    assert_eq!(pos, (41, 0), "Enter 应立即跳到第 42 行");
    assert!(open, "Enter 不关卡片");
    assert_eq!(hint2.as_str(), "→ 第 42 行 第 1 列", "落点确认提示保持");
    let focused = cx.update(|window, cx| {
        let i = input.read(cx).focus_handle(cx);
        window.focused(cx) == Some(i)
    });
    assert!(focused, "Enter 后输入框保持焦点");
}

/// Esc：关闭卡片、清除提示、焦点归还编辑器。
#[gpui::test]
fn goto_esc_closes_and_restores_focus(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "aa\nbb\ncc\ndd\n");
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_goto(&host, cx);
    drive_goto(&host, cx, &input, "3", true);
    assert_eq!(cx.update(|_, cx| ed.read(cx).cursor_position()), (2, 0));

    cx.simulate_keystrokes("escape");
    for _ in 0..4 {
        cx.run_until_parked();
    }
    let (open, hint) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.test_goto_state().0, e.test_goto_state().3)
    });
    assert!(!open, "Esc 应关闭卡片");
    assert!(hint.is_empty(), "Esc 清除内联提示");
    let focused = cx.update(|window, cx| {
        let fh = ed.read(cx).focus_handle();
        window.focused(cx) == Some(fh)
    });
    assert!(focused, "Esc 后焦点归还编辑器");
}

/// 点击卡片外（编辑区文本）：关闭卡片 + 点击文本的原本行为仍生效（光标落位）。
#[gpui::test]
fn goto_click_outside_closes_and_text_click_works(cx: &mut TestAppContext) {
    let lines: Vec<String> = (0..60).map(|i| format!("line-{i}")).collect();
    let (host, cx) = open(cx, &lines.join("\n"));
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let _input = open_goto(&host, cx);
    assert_eq!(cx.update(|_, cx| ed.read(cx).cursor_position()), (0, 0), "初始光标 (0,0)");

    // 点击编辑区**卡片盒之外**的左缘 gutter（光标对齐列 0；纵向 ~3 行处）：应关闭
    // 卡片 + 光标落回点击处（文本点击原本语义生效，不吞事件）。scroll 在顶（未跳转），
    // 行 = floor(点击 y / 行高)，行高 ~14-20px → 落在第 3~5 行。
    let edb = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    let p = gpui::Point::new(edb.origin.x + px(40.), edb.origin.y + px(60.));
    cx.simulate_click(p, Default::default());
    for _ in 0..4 {
        cx.run_until_parked();
    }
    let (open2, pos, focused) = cx.update(|window, cx| {
        let e = ed.read(cx);
        (
            e.test_goto_state().0,
            e.cursor_position(),
            window.focused(cx) == Some(e.focus_handle()),
        )
    });
    assert!(!open2, "点击卡片外应关闭卡片");
    assert!(
        pos.0 >= 3 && pos.0 <= 5,
        "点击文本应落光标到视口内对应行：pos={pos:?}"
    );
    assert_eq!(pos.1, 0, "点击 gutter 应列 0：pos={pos:?}");
    assert!(focused, "点击后焦点回到编辑器");
}

/// 空输入 / 非数字：不跳、卡片内联提示「不是有效行号」（不用 toast 刷屏）、不崩。
#[gpui::test]
fn goto_empty_and_invalid_inline_hint_no_jump(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "a\nb\nc\nd\n");
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_goto(&host, cx);
    let start = cx.update(|_, cx| ed.read(cx).cursor_position());

    // 非数字 → 不跳、内联「不是有效行号」错误色。
    drive_goto(&host, cx, &input, "abc", true);
    let (pos, hint, err) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.cursor_position(), e.test_goto_state().3, e.test_goto_state().4)
    });
    assert_eq!(pos, start, "非法输入不跳转");
    assert_eq!((hint.as_str(), err), ("不是有效行号", true), "内联错误提示");

    // 多个冒号 → 同样非法。
    drive_goto(&host, cx, &input, "1:2:3", true);
    let (hint2, err2) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.test_goto_state().3, e.test_goto_state().4)
    });
    assert_eq!((hint2.as_str(), err2), ("不是有效行号", true));

    // 清空输入 → 不跳、提示区留空（占位由输入框 placeholder 承担）。
    drive_goto(&host, cx, &input, "", true);
    let (pos2, hint3) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.cursor_position(), e.test_goto_state().3)
    });
    assert_eq!(pos2, start, "空输入不跳转");
    assert!(hint3.is_empty(), "空输入提示区留空");
}

/// clamp：0 / 负 / 超行数 → 内联「已钳制」、不 toast；列超行宽 → 静默钳到行宽
/// （沿用旧语义，提示按真实落点）。
#[gpui::test]
fn goto_clamp_landing_and_inline_hint(cx: &mut TestAppContext) {
    let mut lines: Vec<String> = (0..60).map(|i| format!("line-{i}")).collect();
    lines[41] = "abcdefg".to_string();
    let (host, cx) = open(cx, &lines.join("\n"));
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_goto(&host, cx);

    // 42:7 → (41, 6)（均 1 基转 0 基）。
    drive_goto(&host, cx, &input, "42:7", true);
    assert_eq!(cx.update(|_, cx| ed.read(cx).cursor_position()), (41, 6), "42:7 → (41,6)");

    // 42:999 列超行宽（行宽 7）→ 静默钳到行尾 (41,7)（0 基 col 7 = 行尾；不标已钳制，
    // 沿用旧语义；提示按真实落点 1 基 → 第 8 列）。
    drive_goto(&host, cx, &input, "42:999", true);
    let (pos, hint, err) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.cursor_position(), e.test_goto_state().3, e.test_goto_state().4)
    });
    assert_eq!(pos, (41, 7), "列超宽应钳到行尾");
    assert_eq!((hint.as_str(), err), ("→ 第 42 行 第 8 列", false), "提示按真实落点");

    // 9999 超行数（60 行）→ 末行 (59,0) + 内联「已钳制」。
    drive_goto(&host, cx, &input, "9999", true);
    let (pos2, hint2, err2) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.cursor_position(), e.test_goto_state().3, e.test_goto_state().4)
    });
    assert_eq!(pos2, (59, 0), "超行数应钳到末行");
    assert_eq!((hint2.as_str(), err2), ("已钳制到第 60 行 第 1 列", false));

    // 0 → 首行 (0,0) + 已钳制。
    drive_goto(&host, cx, &input, "0", true);
    let (pos3, hint3) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.cursor_position(), e.test_goto_state().3)
    });
    assert_eq!(pos3, (0, 0), "0 应钳到首行");
    assert_eq!(hint3.as_str(), "已钳制到第 1 行 第 1 列");

    // -5 → 首行 (0,0) + 已钳制。
    drive_goto(&host, cx, &input, "-5", true);
    let (pos4, hint4) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.cursor_position(), e.test_goto_state().3)
    });
    assert_eq!(pos4, (0, 0), "负数应钳到首行");
    assert_eq!(hint4.as_str(), "已钳制到第 1 行 第 1 列");
}

/// 空文档：任何行号 → 不跳、提示区留空（不发误导性「已钳制/目标」文案）、不崩。
#[gpui::test]
fn goto_empty_doc_no_jump_no_misleading_hint(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "");
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_goto(&host, cx);
    drive_goto(&host, cx, &input, "5", true);
    let (pos, hint) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.cursor_position(), e.test_goto_state().3)
    });
    assert_eq!(pos, (0, 0), "空文档不跳转（不崩）");
    assert!(hint.is_empty(), "空文档不发「已钳制/目标」误导提示");
}

/// Esc（防抖未到期）后：残留防抖任务不得在卡片关闭后落地跳转——真实定时器会在
/// 关闭后 300ms 到期，若 flush 不检查卡片打开态会出现「幽灵跳」（fake 执行器
/// 不推进时钟测不到，须显式 advance_clock 验证；K26 同类）。
#[gpui::test]
fn goto_esc_discards_pending_debounce(cx: &mut TestAppContext) {
    let lines: Vec<String> = (0..60).map(|i| format!("line-{i}")).collect();
    let (host, cx) = open(cx, &lines.join("\n"));
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_goto(&host, cx);

    // 输入 42（只写文本、防抖未到期 → 未跳）。
    drive_goto(&host, cx, &input, "42", false);
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).cursor_position()),
        (0, 0),
        "防抖未到期不应跳转"
    );

    // 防抖窗内 Esc 关闭（不 advance_clock，残留防抖任务仍挂起）。
    cx.simulate_keystrokes("escape");
    for _ in 0..4 {
        cx.run_until_parked();
    }
    assert!(
        !cx.update(|_, cx| ed.read(cx).test_goto_state().0),
        "Esc 应关闭卡片"
    );

    // 推进时钟跨过防抖窗：残留任务不得落地（光标保持原位）。
    cx.executor().advance_clock(Duration::from_millis(320));
    settle(cx);
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).cursor_position()),
        (0, 0),
        "Esc 关闭后残留防抖不得跳转（无幽灵跳）"
    );
}

/// 再次打开回显最近一次输入并防抖重跳（recall，同 find Esc 后 F3 复用语义）。
#[gpui::test]
fn goto_reopen_recalls_cached_input(cx: &mut TestAppContext) {
    let lines: Vec<String> = (0..2000).map(|i| format!("line-{i}")).collect();
    let (host, cx) = open(cx, &lines.join("\n"));
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_goto(&host, cx);

    // 输入 42 → 落位到 42。
    drive_goto(&host, cx, &input, "42", true);
    assert_eq!(cx.update(|_, cx| ed.read(cx).cursor_position()), (41, 0));

    // Esc 关闭（输入缓存保留）→ 再次 Ctrl+G 回显 + 防抖重跳。
    cx.simulate_keystrokes("escape");
    for _ in 0..4 {
        cx.run_until_parked();
    }
    assert!(
        !cx.update(|_, cx| ed.read(cx).test_goto_state().0),
        "Esc 已关闭"
    );
    let input2 = open_goto(&host, cx);
    let shown = cx.update(|_, cx| input2.read(cx).value().to_string());
    assert_eq!(shown, "42", "再开应回显最近输入");
    // 时钟推进 → 真实防抖重跳到 42。
    cx.executor().advance_clock(Duration::from_millis(320));
    settle(cx);
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).cursor_position()),
        (41, 0),
        "recall 应重跳到 42"
    );
}

/// find 卡与 goto 卡并存：各自打开互不破坏（状态/命中/元素齐备），关 goto 不误伤 find。
#[gpui::test]
fn goto_and_find_cards_coexist(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "aa bb aa\ncc cc aa");
    let ed = ed_of(&host, cx);
    focus(&host, cx);
    let input = open_goto(&host, cx);
    // 打开 find（同一编辑器内独立状态机）。
    let find_input = open_find(&host, cx);
    let (goto_open, find_open) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.test_goto_state().0, e.test_find_state().0)
    });
    assert!(goto_open && find_open, "goto 卡与 find 卡可同时打开");
    assert!(
        cx.debug_bounds("goto-card").is_some() && cx.debug_bounds("find-bar").is_some(),
        "两卡同时渲染"
    );

    // 各自输入各自驱动：goto 输入跳转、find 输入高亮，互不覆盖。
    drive_goto(&host, cx, &input, "1", true);
    assert_eq!(cx.update(|_, cx| ed.read(cx).cursor_position()), (0, 0));
    type_query(&host, cx, "aa", &find_input);
    assert_eq!(cx.update(|_, cx| ed.read(cx).test_find_state().2), 3, "find 匹配 3 处");
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).test_goto_state().0),
        true,
        "find 活动不破坏 goto 打开态"
    );

    // 关闭 goto（close_goto）不误伤 find。
    cx.update(|window, cx| {
        ed.update(cx, |e, ecx| e.close_goto(window, ecx));
    });
    settle(cx);
    let (goto_open2, find_open2, find_n) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.test_goto_state().0, e.test_find_state().0, e.test_find_state().2)
    });
    assert!(
        !goto_open2 && find_open2 && find_n == 3,
        "关 goto 不破坏 find（高亮仍在）"
    );
    let _ = find_input;
}
