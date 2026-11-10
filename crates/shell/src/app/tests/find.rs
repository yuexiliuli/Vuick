
// ---------------------------------------------------------------------------
// 编辑区内查找（Ctrl+F / F3）——shell 层分发集成测试
// ---------------------------------------------------------------------------

use crate::actions::{Find, FindNext, FindPrev};

/// 把 `GPUI_EDITOR_CONFIG` 指向空配置的临时文件，隔离真实 `%APPDATA%\gpui-editor\
/// config.json` 泄漏（K8）；返回路径，测试收尾恢复并删除。
fn find_config_isolate() -> std::path::PathBuf {
    let cfg = std::env::temp_dir().join(format!("gpui-find-cfg-{}.json", std::process::id()));
    std::fs::write(&cfg, "{}").unwrap();
    unsafe { std::env::set_var("GPUI_EDITOR_CONFIG", &cfg) };
    cfg
}
fn find_config_restore(cfg: &std::path::Path) {
    unsafe { std::env::remove_var("GPUI_EDITOR_CONFIG") };
    let _ = std::fs::remove_file(cfg);
}

/// Ctrl+F（Find 动作分发到活动文本选项卡）：打开 find 条 + 输入即高亮全部 + 首个当前 +
/// F3/Shift+F3 复用上次查询上一个/下一个 + Esc 后 F3 仍复用。
#[gpui::test]
async fn find_dispatch_opens_bar_types_and_navigates(cx: &mut TestAppContext) {
    let cfg = find_config_isolate();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = std::env::temp_dir().join(format!("gpui-find-tab-{}.txt", std::process::id()));
    std::fs::write(&p, "foo foo bar\nfoo baz").unwrap();
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
    });
    for _ in 0..3 {
        cx.run_until_parked();
    }
    cx.update(|window, cx| {
        let f = app.read(cx).editor_focus_handle(cx);
        window.focus(&f, cx);
    });
    cx.run_until_parked();

    // Ctrl+F → 活动文本选项卡编辑器打开 find 条。
    cx.dispatch_action(Find);
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let ed = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[a.model.borrow().active_tab].editor.clone()
    });
    let (open, _, _, _, _) = cx.update(|_, cx| ed.read(cx).test_find_state());
    assert!(open, "文本选项卡 Ctrl+F 应打开 find 条");

    // 输入 "foo" → 高亮（3 个匹配）+ 首个为当前。
    // 订阅回调在效果刷新区间投递（非 emit 同步）：set_value 后先 park 让查询词落位，
    // 再同步驱动重扫（fake 执行器不轮询 200ms 防抖定时器）。
    cx.update(|window, cx| {
        let input = ed.read(cx).test_find_input().cloned().unwrap();
        input.update(cx, |s, scx| s.set_value("foo", window, scx));
    });
    for _ in 0..3 {
        cx.run_until_parked();
    }
    cx.update(|window, cx| {
        ed.update(cx, |e, ecx| e.test_find_scan_now(window, ecx));
    });
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let (vis, n, cur) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.test_find_state().1, e.test_find_state().2, e.test_find_current_range())
    });
    assert!(vis && n == 3, "输入后应高亮 3 个匹配");
    assert_eq!(cur, Some(0..3), "首个匹配为当前命中");

    // F3 → 下一个（匹配 2："foo"@4..7）。
    cx.dispatch_action(FindNext);
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let cur = cx.update(|_, cx| ed.read(cx).test_find_current_range());
    assert_eq!(cur, Some(4..7), "F3 复用到下一个匹配");

    // Shift+F3 → 上一个（回匹配 1）。
    cx.dispatch_action(FindPrev);
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let cur = cx.update(|_, cx| ed.read(cx).test_find_current_range());
    assert_eq!(cur, Some(0..3), "Shift+F3 回上一个匹配");

    // 关条（Esc）后 F3：移除高亮但保留上次查询，F3 仍可复用走下一个。
    cx.update(|window, cx| {
        ed.update(cx, |e, ecx| e.close_find(window, ecx));
    });
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let (open2, vis2) = cx.update(|_, cx| {
        let e = ed.read(cx);
        (e.test_find_state().0, e.test_find_state().1)
    });
    assert!(!open2 && !vis2, "Esc 应关条并移除高亮");
    cx.dispatch_action(FindNext);
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let cur = cx.update(|_, cx| ed.read(cx).test_find_current_range());
    assert_eq!(cur, Some(4..7), "Esc 后 F3 仍复用上次查询走下一个");

    find_config_restore(&cfg);
    let _ = std::fs::remove_file(&p);
}

/// 非文本选项卡（图片查看器）Ctrl+F：toast「当前选项卡不支持查找」，不开条、不崩。
#[gpui::test]
async fn find_non_text_tab_toasts_not_opens(cx: &mut TestAppContext) {
    let cfg = find_config_isolate();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    // 把活动选项卡换成图片阅读器（非文本）。
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            let viewer = cx.new(|cx| {
                image_viewer::ImageViewer::new(std::path::PathBuf::from("___"), cx)
            });
            let idx = app.model.borrow().active_tab;
            app.model.borrow_mut().tabs[idx].reader = MediaReader::Image(viewer);
        });
    });
    for _ in 0..3 {
        cx.run_until_parked();
    }
    cx.update(|window, cx| {
        let f = app.read(cx).editor_focus_handle(cx);
        window.focus(&f, cx);
    });
    cx.run_until_parked();

    cx.dispatch_action(Find);
    cx.dispatch_action(FindNext);
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let (open, toast) = cx.update(|_w, cx| {
        let a = app.read(cx);
        let ed = a.model.borrow().tabs[a.model.borrow().active_tab].editor.read(cx);
        (
            ed.test_find_state().0,
            a.notifications
                .iter()
                .any(|n| n.message_str().contains("不支持查找")),
        )
    });
    assert!(!open, "非文本选项卡 Ctrl+F 不应打开 find 条（不崩）");
    assert!(toast, "非文本应 toast「当前选项卡不支持查找」");
    find_config_restore(&cfg);
}
