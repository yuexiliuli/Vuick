
// ---------------------------------------------------------------------------
// 编辑区内跳转行（Ctrl+G）——shell 层分发集成测试
// ---------------------------------------------------------------------------

use crate::actions::GoToLine;

/// 把 `GPUI_EDITOR_CONFIG` 指向空配置的临时文件，隔离真实 `%APPDATA%\gpui-editor\
/// config.json` 泄漏（K8）；返回路径，测试收尾恢复并删除。
fn goto_config_isolate() -> std::path::PathBuf {
    let cfg = std::env::temp_dir().join(format!("gpui-goto-cfg-{}.json", std::process::id()));
    std::fs::write(&cfg, "{}").unwrap();
    unsafe { std::env::set_var("GPUI_EDITOR_CONFIG", &cfg) };
    cfg
}
fn goto_config_restore(cfg: &std::path::Path) {
    unsafe { std::env::remove_var("GPUI_EDITOR_CONFIG") };
    let _ = std::fs::remove_file(cfg);
}

/// Ctrl+G（GoToLine 动作分发到活动文本选项卡）：打开编辑区内跳转行卡片（top-center
/// 卡片，不再是命令面板），输入框自动聚焦；Enter 立即跳转且卡片保持打开、不关。
#[gpui::test]
async fn goto_line_dispatches_to_active_editor(cx: &mut TestAppContext) {
    let cfg = goto_config_isolate();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = std::env::temp_dir().join(format!("gpui-goto-tab-{}.txt", std::process::id()));
    let lines: Vec<String> = (0..60).map(|i| format!("line-{i}")).collect();
    std::fs::write(&p, lines.join("\n")).unwrap();
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

    // Ctrl+G → 活动文本选项卡编辑器打开 goto 卡片，输入框自动聚焦。
    cx.dispatch_action(GoToLine);
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let (open, focused) = cx.update(|window, cx| {
        let a = app.read(cx);
        let ed = a.tabs[a.active_tab].editor.read(cx);
        let input_focused = ed
            .test_goto_input()
            .map(|i| {
                let fh = i.read(cx).focus_handle(cx);
                window.focused(cx) == Some(fh)
            })
            .unwrap_or(false);
        (ed.test_goto_state().0, input_focused)
    });
    assert!(open, "文本选项卡 Ctrl+G 应打开跳转行卡片");
    assert!(focused, "打开后输入框应自动聚焦");

    // 输入 42 + Enter → 立即跳到第 42 行（0 基 41）、卡片保持打开。
    cx.update(|window, cx| {
        let ed = app.read(cx).tabs[app.read(cx).active_tab]
            .editor
            .clone();
        let input = ed.read(cx).test_goto_input().cloned().unwrap();
        input.update(cx, |s, scx| s.set_value("42", window, scx));
    });
    for _ in 0..3 {
        cx.run_until_parked();
    }
    cx.simulate_keystrokes("enter");
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let (pos, open2) = cx.update(|_w, cx| {
        let a = app.read(cx);
        let ed = a.tabs[a.active_tab].editor.read(cx);
        (ed.cursor_position(), ed.test_goto_state().0)
    });
    assert_eq!(pos, (41, 0), "Enter 应跳到第 42 行");
    assert!(open2, "Enter 不关卡片（输入框保持焦点）");

    goto_config_restore(&cfg);
    let _ = std::fs::remove_file(&p);
}

/// 非文本选项卡（图片查看器）Ctrl+G：toast「当前选项卡不支持跳转行」，不开卡、不崩。
#[gpui::test]
async fn goto_line_non_text_tab_toasts(cx: &mut TestAppContext) {
    let cfg = goto_config_isolate();
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
            app.tabs[app.active_tab].reader = MediaReader::Image(viewer);
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

    cx.dispatch_action(GoToLine);
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let (open, toast) = cx.update(|_w, cx| {
        let a = app.read(cx);
        let ed = a.tabs[a.active_tab].editor.read(cx);
        (
            ed.test_goto_state().0,
            a.notifications
                .iter()
                .any(|n| n.message_str().contains("不支持跳转行")),
        )
    });
    assert!(!open, "非文本选项卡 Ctrl+G 不应打开跳转行卡片（不崩）");
    assert!(toast, "非文本应 toast「当前选项卡不支持跳转行」");
    goto_config_restore(&cfg);
}
