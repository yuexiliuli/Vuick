use std::time::Duration;

// WYSIWYG 光标闪烁回归（用户报告：光标位置对但静止不闪）。
// 对照 crate `ui/input.rs` 的 `caret_blinks_with_timer` 同样式：虚拟时钟驱动真实
// `cx.spawn` 的 blink 定时器，断言 `caret_shown_now()`（paint 消费的唯一光标相位）
// 随闪烁计时翻转。修复前 WYSIWYG 光标在 `element/paint` 恒画（无 blink 循环、
// 无 `caret_visible` 相位）——本测试先 FAIL，修后 PASS。
//
// ⚠️ `VisualTestContext::run_until_parked` 只排空后台 executor；推进虚拟时钟要走根
// `TestAppContext` 的 executor（先释放 vcx 再重取，见 `tests/epub.rs` 同套路）。

/// 复现 3：WYSIWYG「光标不闪烁」——聚焦后应启动 blink 循环、光标先可见；静止期
/// 结束后可见性随计时翻转（闪烁）；交互静止期常亮；失焦后循环退出不再翻转。
#[gpui::test]
async fn caret_blinks_with_timer(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("hello\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    // 未聚焦：光标不画
    let shown0 = vcx.update(|_, cx| ed.read(cx).caret_phase.caret_shown_now());
    assert!(!shown0, "未聚焦 WYSIWYG 光标应不可见");
    // 聚焦 → 光标可见（静止期开始）、blink 循环启动。gpui 测试环境不派发真实
    // focus/blur 事件：`window.focus` 只置窗口焦点态，由 `sync_focus`（生产每帧
    // request_layout 同款逻辑）检出并走 on_focus 启动循环。
    vcx.update(|window, cx| {
        window.focus(&ed.read(cx).focus_handle(), cx);
        ed.update(cx, |e, ecx| e.sync_focus(window, ecx));
    });
    let (shown, running, visible) = vcx.update(|_, cx| {
        let e = ed.read(cx);
        (
            e.caret_phase.caret_shown_now(),
            e.caret_phase.blink_running,
            e.caret_phase.caret_visible,
        )
    });
    assert!(shown, "聚焦后光标应可见");
    assert!(running, "聚焦后应启动 blink 循环");
    assert!(visible, "聚焦后 caret_visible 应为 true");
    // 让 blink 循环首拍跑起来并 park 在 500ms 定时器（首拍静止期内不翻转）。
    let _ = vcx;
    cx.run_until_parked();
    // 绕过静止期：`caret_pause_until` 基于真实墙钟 `Instant`，测试虚拟时钟推进
    // 不会让它流逝——置 None 让循环下一次 tick 直接翻转（对齐 ui/input.rs）。
    ed.update(cx, |e, _| {
        e.caret_phase.caret_pause_until = None;
    });
    cx.executor()
        .advance_clock(Duration::from_millis(500 + 10));
    cx.run_until_parked();
    let win = cx.windows()[0];
    let mut vcx2 = VisualTestContext::from_window(win, cx);
    let after = vcx2.update(|_, cx| ed.read(cx).caret_phase.caret_visible);
    assert!(!after, "光标可见性应随 blink 计时翻转（闪烁）");
    // 交互静止期：kick_caret 后短暂常亮（暂停期内不翻转）。
    vcx2.update(|_, cx| {
        ed.update(cx, |e, _| e.caret_phase.kick());
    });
    let paused_shown = vcx2.update(|_, cx| ed.read(cx).caret_phase.caret_shown_now());
    assert!(paused_shown, "kick_caret 后静止期内光标应常亮");
    // 失焦：真正 blur 窗口焦点态 + sync_focus（对齐 ui/input.rs 的 on_blur 直测）。
    vcx2.update(|window, cx| {
        window.blur(cx);
        ed.update(cx, |e, ecx| e.sync_focus(window, ecx));
    });
    // 失焦后循环还要再睡满周期才到退出判定（toggle → sleep → keep-check），分拍
    // 推进 + 每拍 park（对齐 ui/input.rs 的 advance_clock 驱动惯用法）。
    let _ = vcx2;
    for _ in 0..8 {
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
    }
    let win = cx.windows()[0];
    let mut vcx3 = VisualTestContext::from_window(win, cx);
    let (running2, shown2) = vcx3.update(|_, cx| {
        let e = ed.read(cx);
        (e.caret_phase.blink_running, e.caret_phase.caret_shown_now())
    });
    assert!(!running2, "失焦后 blink 循环应退出");
    assert!(!shown2, "失焦后光标应不可见");
}
