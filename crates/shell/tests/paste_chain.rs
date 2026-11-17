//! Ctrl+V 真实 Windows 键链收口测试（H2/H3）。
//!
//! 背景：`ui::InputState` 已自收口 Ctrl+V(`PasteNormalized`) / Ctrl+X(`Cut`) /
//! Ctrl+Z·Y(`Undo/Redo`)（另一个会话完成），任一 InputState 聚焦时 app 根**收不到**
//! `PasteNormalized`。本文件从 shell 侧验证真实键链：
//!
//! - 侧边栏搜索框：Ctrl+V = **光标/选区插入**（非整体替换 `replace`），源编辑器不变；
//! - 编辑区 find 条 / goto 条：Ctrl+V 进查找/跳转框，源编辑器正文**不**变（「漏配粘进
//!   正文」重灾区）+ Ctrl+Z 撤销 / Ctrl+Y 重做真实链（H3）；
//! - 命令面板 palette / 最近面板 search / 文件树过滤框：Ctrl+V 进各自输入框。
//!
//! 姿态与 `src/app/tests/preview.rs` 的卡片粘贴测试同款：`init_test`（`bind_local_keymaps`，
//! 含 wy-editor / Root / Input 三种键上下文）**之后**补 `ctrl-v → PasteNormalized` 的 Input
//! 上下文覆盖绑定（与 `src/main.rs` 同序 = 真实 app 装配）→ `write_to_clipboard` 预置 →
//! `dispatch_keystroke(ctrl-v)` 断言目标输入变化、源编辑器正文不变。

mod common;

use common::*;
use gpui::{App, KeyBinding, Keystroke};

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

/// 把 `VUICK_CONFIG` 指向空配置临时文件，隔离真实 `%APPDATA%\vuick\config.json`
/// 的会话恢复 / 最近列表泄漏（K8：config 读取不受 `cfg!(test)` 门控）。
/// 返回路径，测试收尾恢复并删除。
fn config_isolate() -> std::path::PathBuf {
    let cfg = std::env::temp_dir().join(format!("vuick-paste-cfg-{}.json", std::process::id()));
    std::fs::write(&cfg, "{}").unwrap();
    unsafe { std::env::set_var("VUICK_CONFIG", &cfg) };
    cfg
}
fn config_restore(cfg: &std::path::Path) {
    unsafe { std::env::remove_var("VUICK_CONFIG") };
    let _ = std::fs::remove_file(cfg);
}

/// 与 main.rs 同序：`bind_local_keymaps`（init_test）**之后**补 `ctrl-v → PasteNormalized`
/// 覆盖绑定（Input 键上下文，更高优先级）→ 真实 app 链路：Ctrl+V → 聚焦的 InputState
/// 自收口（`on_paste` 光标/选区插入），app 根不到。
fn bind_ctrl_v_paste(cx: &mut TestAppContext) {
    cx.update(|cx: &mut App| {
        cx.bind_keys([KeyBinding::new("ctrl-v", PasteNormalized, Some("Input"))]);
    });
}

/// 渲染若干帧（等首帧/重绘收敛）。
fn settle(cx: &mut VisualTestContext, n: usize) {
    for _ in 0..n {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
    }
}

/// 取活动文本选项卡的编辑器实体。
fn active_editor(app: &Entity<EditorApp>, cx: &mut VisualTestContext) -> Entity<editor::CodeEditor> {
    cx.update(|_w, cx| {
        app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]
            .payload
            .editor
            .clone()
    })
}

// ---------------------------------------------------------------------------
// H2.1 侧边栏搜索框：Ctrl+V = 光标插入（非整体替换），源编辑器不变
// ---------------------------------------------------------------------------

/// 目标的真实键链断言：在 shell 宿主里装配 main.rs 同序的 ctrl-v→PasteNormalized
/// 绑定，聚焦 Utf8 搜索框并预置 "12"，真实方向键把光标回移一位后 Ctrl+V 粘 "X"。
#[gpui::test]
async fn search_box_ctrl_v_inserts_at_caret_not_replace(cx: &mut TestAppContext) {
    let cfg = config_isolate();
    init_test(cx);
    bind_ctrl_v_paste(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    settle(cx, 3);
    let p = std::env::temp_dir().join(format!("gpui-paste-src-{}.txt", std::process::id()));
    std::fs::write(&p, "hello\nworld\n").unwrap();
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
    });
    settle(cx, 3);
    let ed = active_editor(&app, cx);
    let src_before = cx.update(|_w, cx| ed.read(cx).value().to_string());
    assert!(src_before.contains("hello"), "源编辑器应有内容");

    // 打开侧栏搜索视图 → 搜索框渲染；聚焦搜索框并预置 "12"（光标收敛到末尾 2）。
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| app.toggle_sidebar_view(SidebarView::Search, cx));
    });
    settle(cx, 3);
    let search = cx.update(|_w, cx| app.read(cx).search_state(cx));
    cx.update(|window, cx| {
        let fh = search.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        search.update(cx, |s, cx| s.set_value("12", window, cx));
    });
    cx.run_until_parked();

    // 真实方向键把光标回移一位（12 → 1|2）→ 粘 "X" → 期望 "1X2"（插入，非整体替换）。
    let wh = cx.windows().first().copied().expect("窗口句柄");
    cx.dispatch_keystroke(wh, Keystroke::parse("left").unwrap());
    cx.run_until_parked();
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("X".to_string()));
    cx.dispatch_keystroke(wh, Keystroke::parse("ctrl-v").unwrap());
    settle(cx, 4);

    let val = cx.update(|_w, cx| search.read(cx).value().to_string());
    assert_eq!(val.as_str(), "1X2", "搜索框 Ctrl+V 应为光标插入（自收口），非整体替换：{val:?}");
    let src_after = cx.update(|_w, cx| ed.read(cx).value().to_string());
    assert_eq!(src_after, src_before, "源编辑器正文不得被误粘：{src_after:?}");

    config_restore(&cfg);
    let _ = std::fs::remove_file(&p);
}

// ---------------------------------------------------------------------------
// H2.3 编辑区 find 条 / goto 条：Ctrl+V 进输入框，源编辑器正文不变（H3 附 Ctrl+Z/Y）
// ---------------------------------------------------------------------------

/// find 条（编辑区内查找 Ctrl+F）：Ctrl+V 粘进查找框（InputState 自收口），源编辑器
/// 正文不变——「漏配粘进正文」重灾区。附 H3：Ctrl+Z 撤销粘贴、Ctrl+Y 重做。
#[gpui::test]
async fn find_bar_ctrl_v_goes_to_input_not_source_editor(cx: &mut TestAppContext) {
    let cfg = config_isolate();
    init_test(cx);
    bind_ctrl_v_paste(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    settle(cx, 3);
    let p = std::env::temp_dir().join(format!("gpui-paste-src-{}.txt", std::process::id()));
    std::fs::write(&p, "hello\nworld\n").unwrap();
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
    });
    settle(cx, 3);
    let ed = active_editor(&app, cx);

    // 聚焦编辑区 + Ctrl+F 打开 find 条。
    cx.update(|window, cx| {
        let f = app.read(cx).editor_focus_handle(cx);
        window.focus(&f, cx);
    });
    cx.run_until_parked();
    cx.dispatch_action(Find);
    settle(cx, 4);
    let find_input = cx
        .update(|_w, cx| ed.read(cx).test_find_input().cloned().expect("find 条输入框"));
    // 显式聚焦 find 输入框（打开即聚焦，此处保稳）。
    cx.update(|window, cx| {
        let fh = find_input.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
    });
    cx.run_until_parked();

    let src_before = cx.update(|_w, cx| ed.read(cx).value().to_string());
    let wh = cx.windows().first().copied().expect("窗口句柄");
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("needle".to_string()));
    cx.dispatch_keystroke(wh, Keystroke::parse("ctrl-v").unwrap());
    settle(cx, 4);

    let find_val = cx.update(|_w, cx| find_input.read(cx).value().to_string());
    assert_eq!(find_val.as_str(), "needle", "Ctrl+V 应粘进 find 条输入框：{find_val:?}");
    let src_after = cx.update(|_w, cx| ed.read(cx).value().to_string());
    assert_eq!(src_after, src_before, "源编辑器正文不得被误粘：{src_after:?}");

    // H3 代表：正则粘贴作一次撤销单元 → Ctrl+Z 撤销回空、Ctrl+Y 重做回 "needle"。
    cx.dispatch_keystroke(wh, Keystroke::parse("ctrl-z").unwrap());
    settle(cx, 3);
    let undone = cx.update(|_w, cx| find_input.read(cx).value().to_string());
    assert_eq!(undone.as_str(), "", "Ctrl+Z 应撤销粘贴：{undone:?}");
    cx.dispatch_keystroke(wh, Keystroke::parse("ctrl-y").unwrap());
    settle(cx, 3);
    let redone = cx.update(|_w, cx| find_input.read(cx).value().to_string());
    assert_eq!(redone.as_str(), "needle", "Ctrl+Y 应重做粘贴：{redone:?}");
    assert_eq!(
        cx.update(|_w, cx| ed.read(cx).value().to_string()),
        src_before,
        "撤销/重做全程源编辑器正文不得被触碰"
    );

    config_restore(&cfg);
    let _ = std::fs::remove_file(&p);
}

/// goto 条（编辑区内跳转行 Ctrl+G）：Ctrl+V 粘进跳转框（InputState 自收口），源编辑器正文不变。
#[gpui::test]
async fn goto_bar_ctrl_v_goes_to_input_not_source_editor(cx: &mut TestAppContext) {
    let cfg = config_isolate();
    init_test(cx);
    bind_ctrl_v_paste(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    settle(cx, 3);
    let p = std::env::temp_dir().join(format!("gpui-paste-src-{}.txt", std::process::id()));
    std::fs::write(&p, "hello\nworld\n").unwrap();
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
    });
    settle(cx, 3);
    let ed = active_editor(&app, cx);

    cx.update(|window, cx| {
        let f = app.read(cx).editor_focus_handle(cx);
        window.focus(&f, cx);
    });
    cx.run_until_parked();
    cx.dispatch_action(GoToLine);
    settle(cx, 4);
    let goto_input = cx
        .update(|_w, cx| ed.read(cx).test_goto_input().cloned().expect("goto 输入框"));
    cx.update(|window, cx| {
        let fh = goto_input.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
    });
    cx.run_until_parked();

    let src_before = cx.update(|_w, cx| ed.read(cx).value().to_string());
    let wh = cx.windows().first().copied().expect("窗口句柄");
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("42".to_string()));
    cx.dispatch_keystroke(wh, Keystroke::parse("ctrl-v").unwrap());
    settle(cx, 4);

    let goto_val = cx.update(|_w, cx| goto_input.read(cx).value().to_string());
    assert_eq!(goto_val.as_str(), "42", "Ctrl+V 应粘进 goto 输入框：{goto_val:?}");
    let src_after = cx.update(|_w, cx| ed.read(cx).value().to_string());
    assert_eq!(src_after, src_before, "源编辑器正文不得被误粘：{src_after:?}");

    config_restore(&cfg);
    let _ = std::fs::remove_file(&p);
}

// ---------------------------------------------------------------------------
// H2.2 命令面板 / 最近面板 / 文件树过滤框：Ctrl+V 进各自输入框
// ---------------------------------------------------------------------------

/// 命令面板（Ctrl+Shift+P）查询框（`ui::InputState`）：Ctrl+V 粘进查询框，源编辑器正文不变。
#[gpui::test]
async fn palette_ctrl_v_goes_to_query_input_not_source_editor(cx: &mut TestAppContext) {
    let cfg = config_isolate();
    init_test(cx);
    bind_ctrl_v_paste(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    settle(cx, 3);
    let p = std::env::temp_dir().join(format!("gpui-paste-src-{}.txt", std::process::id()));
    std::fs::write(&p, "hello\nworld\n").unwrap();
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
    });
    settle(cx, 3);
    let ed = active_editor(&app, cx);

    cx.update(|window, cx| {
        let f = app.read(cx).editor_focus_handle(cx);
        window.focus(&f, cx);
    });
    cx.run_until_parked();
    cx.dispatch_action(ToggleCommandPalette);
    settle(cx, 4);
    let palette = cx
        .update(|_w, cx| app.read(cx).palette.clone().expect("命令面板应打开"));
    let input = cx.update(|_w, cx| palette.read(cx).input_entity());
    // 面板打开即聚焦查询框，此处显式聚焦保稳。
    cx.update(|window, cx| {
        let fh = input.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
    });
    cx.run_until_parked();

    let src_before = cx.update(|_w, cx| ed.read(cx).value().to_string());
    let wh = cx.windows().first().copied().expect("窗口句柄");
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("X".to_string()));
    cx.dispatch_keystroke(wh, Keystroke::parse("ctrl-v").unwrap());
    settle(cx, 4);

    let q = cx.update(|_w, cx| input.read(cx).value().to_string());
    assert_eq!(q.as_str(), "X", "Ctrl+V 应粘进命令面板查询框：{q:?}");
    let src_after = cx.update(|_w, cx| ed.read(cx).value().to_string());
    assert_eq!(src_after, src_before, "源编辑器正文不得被误粘：{src_after:?}");

    config_restore(&cfg);
    let _ = std::fs::remove_file(&p);
}

/// 最近面板（「打开最近…」大面板）搜索框（`ui::InputState`）：Ctrl+V 粘进搜索框，
/// 源编辑器正文不变。
#[gpui::test]
async fn recent_panel_search_ctrl_v_goes_to_input_not_source_editor(cx: &mut TestAppContext) {
    let cfg = config_isolate();
    init_test(cx);
    bind_ctrl_v_paste(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    settle(cx, 3);
    let p = std::env::temp_dir().join(format!("gpui-paste-src-{}.txt", std::process::id()));
    std::fs::write(&p, "hello\nworld\n").unwrap();
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
    });
    settle(cx, 3);
    let ed = active_editor(&app, cx);

    // 打开最近面板（搜索框懒初始化为首次渲染）。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_recent_panel(window, cx));
    });
    settle(cx, 5);
    let panel = cx
        .update(|_w, cx| app.read(cx).recent_panel.clone().expect("最近面板应打开"));
    let input = cx.update(|_w, cx| panel.read(cx).input_entity().expect("最近面板搜索框"));
    cx.update(|window, cx| {
        let fh = input.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
    });
    cx.run_until_parked();

    let src_before = cx.update(|_w, cx| ed.read(cx).value().to_string());
    let wh = cx.windows().first().copied().expect("窗口句柄");
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("X".to_string()));
    cx.dispatch_keystroke(wh, Keystroke::parse("ctrl-v").unwrap());
    settle(cx, 4);

    let q = cx.update(|_w, cx| input.read(cx).value().to_string());
    assert_eq!(q.as_str(), "X", "Ctrl+V 应粘进最近面板搜索框：{q:?}");
    let src_after = cx.update(|_w, cx| ed.read(cx).value().to_string());
    assert_eq!(src_after, src_before, "源编辑器正文不得被误粘：{src_after:?}");

    config_restore(&cfg);
    let _ = std::fs::remove_file(&p);
}

/// 文件树「过滤文件名」框（`ui::InputState`，真实点击聚焦路径）：Ctrl+V 粘进过滤框，
/// 防抖提交过滤词，源编辑器正文不变。
#[gpui::test]
async fn files_filter_ctrl_v_goes_to_input_not_source_editor(cx: &mut TestAppContext) {
    let cfg = config_isolate();
    init_test(cx);
    bind_ctrl_v_paste(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    settle(cx, 3);
    let p = std::env::temp_dir().join(format!("gpui-paste-src-{}.txt", std::process::id()));
    std::fs::write(&p, "hello\nworld\n").unwrap();
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
    });
    settle(cx, 3);
    let ed = active_editor(&app, cx);

    // 打开一个文件夹 → 文件树 + 过滤框渲染（root 同步设值，过滤框立即随 Files 视图渲染）。
    let dir = std::env::temp_dir().join(format!("gpui-paste-dir-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "A").unwrap();
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_folder_path(dir.clone(), window, cx));
    });
    settle(cx, 5);
    let box_bounds = cx
        .debug_bounds("sidebar-files-filter-input")
        .expect("文件树过滤框应渲染");

    // 真实点击聚焦过滤框 → 真实键入 "12" + 左移光标 → Ctrl+V 粘 "X" → 过滤词提交 "1x2"。
    cx.simulate_click(box_bounds.center(), Default::default());
    settle(cx, 2);
    let wh = cx.windows().first().copied().expect("窗口句柄");
    cx.dispatch_keystroke(wh, Keystroke::parse("1").unwrap());
    cx.dispatch_keystroke(wh, Keystroke::parse("2").unwrap());
    cx.dispatch_keystroke(wh, Keystroke::parse("left").unwrap());
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("X".to_string()));
    cx.dispatch_keystroke(wh, Keystroke::parse("ctrl-v").unwrap());
    // 防抖期满提交过滤词（FILES_FILTER_DEBOUNCE_MS=120 → 推 300ms）。
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(300));
    settle(cx, 4);

    let filter = cx.update(|_w, cx| app.read(cx).model.borrow().files_filter.clone());
    assert_eq!(filter.as_str(), "1x2", "Ctrl+V 应粘进过滤框并防抖提交（小写化）：{filter:?}");
    let src_after = cx.update(|_w, cx| ed.read(cx).value().to_string());
    assert_eq!(src_after, "hello\nworld\n", "源编辑器正文不得被误粘：{src_after:?}");

    config_restore(&cfg);
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_dir_all(&dir);
}
