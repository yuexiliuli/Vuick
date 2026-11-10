// PDF 右键菜单点击后移动鼠标误拖选回归（用户反馈）：
// 右键弹菜单 → 左键点**普通关闭型**菜单项（添加书签）→ 菜单关闭 → 之后任意移动鼠标，
// PDF 底下不得出现文字选中（菜单项点击的 mousedown 不应穿透到下层 viewer 把
// `selecting` 臂锁上——旧实现 mousedown 冒泡到 viewer 置 selecting=true，而 mouseup
// 被菜单项 on_click 的 stop_propagation 吃掉 → mouseup 到不了 viewer → selecting 滞留
// → 之后无按键移动都被当成拖选）。
// 对照：直接在文本上按下左键并拖动，仍能正常划选文字（拖选不回归）。

/// bug 回归：右键弹菜单 → 左键点「添加书签」→ 菜单关闭 → 连续移动鼠标到文本上 ≥10 步
/// → 不得出现任何文字选中。
#[gpui::test]
async fn pdf_menu_click_then_move_leaves_no_selection(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("menuselect");
    open_pdf(&app, cx, &p);

    // 先取一个**确定在文本上**的点（首段文本起点），据此把右键点定在让「添加书签」首项
    // 行中心 ≈ 文本点处：菜单锚定 TopLeft，首项行中心 ≈ (pos.x + 43, pos.y + 19)。
    let ((_page, _k), (p1, p2)) = first_char_range_points(&app, cx);
    let rc = gpui::point(p1.x - px(43.), p1.y - px(19.));
    let mods = gpui::Modifiers::default();
    cx.simulate_mouse_move(rc, None, mods);
    cx.run_until_parked();
    cx.simulate_mouse_down(rc, gpui::MouseButton::Right, mods);
    cx.simulate_mouse_up(rc, gpui::MouseButton::Right, mods);
    settle_frames(cx, 3);
    let bm = cx
        .debug_bounds("pdf-menu-bookmark")
        .expect("右键应弹「添加书签」")
        .center();

    // 左键单击菜单项（真实路径：按下→松开，不移动）→ 菜单关闭。
    cx.simulate_mouse_down(bm, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_up(bm, gpui::MouseButton::Left, mods);
    settle_frames(cx, 3);
    assert!(
        cx.debug_bounds("pdf-menu-bookmark").is_none(),
        "点「添加书签」（普通关闭型）后菜单应关闭"
    );

    // 连续移动 ≥10 步：从菜单点击点缓移到文本段上方再向右扫过文字（鼠标**未按住**左键）。
    for i in 1..=12 {
        let t = i as f32 / 12.0;
        let x = bm.x + (p2.x - bm.x) * t;
        let y = bm.y + (p2.y - bm.y) * t;
        cx.simulate_mouse_move(gpui::point(x, y), None, mods);
        cx.run_until_parked();
    }
    settle_frames(cx, 4);

    // 菜单项点击后移动鼠标不得选中文字（旧行为：selecting 滞留 → 扫出大段选区）。
    let sel = cx.update(|_w, cx| {
        tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
            .unwrap()
            .read(cx)
            .selected_text()
    });
    assert!(sel.is_none(), "点菜单项后移动鼠标不应选中文字，实际选区: {sel:?}");
}

/// 对照回归：**直接**在 PDF 文本上按下左键并拖动（不经过菜单）→ 仍能正常划选文字
/// （viewer 拖选路径不回归）。
#[gpui::test]
async fn pdf_direct_drag_select_still_works(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("dragsel");
    open_pdf(&app, cx, &p);

    // 首段文本起点 → 终点，真实拖选（按住左键拖到终点再松开）。
    let ((_a, _b), (p1, p2)) = first_char_range_points(&app, cx);
    let m = gpui::Modifiers::default();
    cx.simulate_mouse_move(p1, gpui::MouseButton::Left, m);
    cx.simulate_mouse_down(p1, gpui::MouseButton::Left, m);
    let mut end = p1;
    for i in 1..=6 {
        let t = i as f32 / 6.0;
        end = gpui::point(p1.x + (p2.x - p1.x) * t, p1.y + (p2.y - p1.y) * t);
        cx.simulate_mouse_move(end, gpui::MouseButton::Left, m);
        cx.run_until_parked();
    }
    cx.simulate_mouse_up(end, gpui::MouseButton::Left, m);
    settle_frames(cx, 3);

    // 拖选应得到非空选区文本（直接划选正常）。
    let sel = cx.update(|_w, cx| {
        tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
            .unwrap()
            .read(cx)
            .selected_text()
    });
    assert!(
        sel.as_ref().map(|s| s.len() > 0).unwrap_or(false),
        "直接拖选文本应得到选区，实际 {sel:?}"
    );
}
