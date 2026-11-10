// PDF 右键菜单「背景色」二级功能集成测试（brief R7 回归断言）：
// - 右键菜单含「背景色」项（根行带「›」指示）；悬停「背景色」→ 次级菜单在父菜单右侧
//   展开（默认/夜间/护眼/羊皮纸/自定义），父项（添加书签/插入/删除/提取等）**保持可见**、
//   无「返回」项；悬停普通项 → 次级收起；
// - 预设点击 → viewer.bg 立即变对应 Hsla；「默认」→ bg=None（回落跟随主题）；keep_open
//   保持菜单展开可连试；
// - 显式色下切主题 viewer.bg 不变 & 默认色下切主题 bg 保持 None（跟随主题，chrome.rs 守卫）。
// 自定义取色走 Windows 原生对话框（headless 不真弹：测试构建 pick_custom_pdf_bg no-op，
// 应用路径由 set_pdf_bg 兜底覆盖）；config 持久化 roundtrip 由 config crate 纯函数测试覆盖。

/// 打开测试 PDF 副本并右键弹菜单（返回活动 PDF 实体）。
fn bg_open_pdf_right_click(
    app: &gpui::Entity<EditorApp>,
    cx: &mut VisualTestContext,
    tag: &str,
) -> gpui::Entity<pdf::PdfViewer> {
    let p = temp_pdf_copy(tag);
    open_pdf(app, cx, &p);
    pdf_right_click(app, cx)
}

/// 点菜单项（按 debug_selector），返回其中心点。
fn bg_menu_center(cx: &mut VisualTestContext, id: &'static str) -> gpui::Point<gpui::Pixels> {
    cx.debug_bounds(id)
        .unwrap_or_else(|| panic!("菜单项 {id} 应在场"))
        .center()
}

/// 活动 PDF 阅读器实体。
fn bg_active_pdf(app: &gpui::Entity<EditorApp>, cx: &mut VisualTestContext) -> gpui::Entity<pdf::PdfViewer> {
    cx.update(|_w, cx| {
        tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone()
    })
}

/// 读应用当前 pdf_bg 偏好。
fn bg_pref(app: &gpui::Entity<EditorApp>, cx: &mut VisualTestContext) -> PdfBgPref {
    cx.update(|_w, cx| app.read(cx).model.borrow().pdf_bg)
}

/// R1/R2/R3/R6：右键菜单含「背景色」（根行带 › 指示）；悬停 → 次级菜单展开、父项全部
/// 保持可见、无「返回」项；悬停普通项 → 次级收起、父菜单保持。
#[gpui::test]
async fn pdf_right_click_bg_color_flyout_hover_and_parent_stays(cx: &mut TestAppContext) {
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
    let _pdf = bg_open_pdf_right_click(&app, cx, "bgflyout");
    // 右键菜单：背景色 + 其它父项全部在场；背景色根行带「›」指示。
    assert!(
        cx.debug_bounds("pdf-menu-bg-color").is_some(),
        "右键菜单应含「背景色」"
    );
    for id in [
        "pdf-menu-bookmark",
        "pdf-menu-insert",
        "pdf-menu-delete",
        "pdf-menu-extract",
    ] {
        assert!(cx.debug_bounds(id).is_some(), "父菜单应含「{id}」");
    }
    assert!(
        cx.debug_bounds("menu-flyout-chevron-6").is_some(),
        "背景色根行应带 › 箭头指示（父菜单内下标 6）"
    );
    // 悬停「背景色」→ 次级菜单出现（默认/夜间/护眼/羊皮纸/自定义），父项保持可见、无返回。
    let bg = bg_menu_center(cx, "pdf-menu-bg-color");
    cx.simulate_mouse_move(bg, None, gpui::Modifiers::default());
    cx.run_until_parked();
    for id in [
        "pdf-menu-bg-default",
        "pdf-menu-bg-night",
        "pdf-menu-bg-eye",
        "pdf-menu-bg-parchment",
        "pdf-menu-bg-custom",
    ] {
        assert!(cx.debug_bounds(id).is_some(), "次级菜单应含「{id}」");
    }
    assert!(
        cx.debug_bounds("pdf-menu-bg-back").is_none(),
        "真悬浮次级无「返回」项"
    );
    for id in [
        "pdf-menu-bookmark",
        "pdf-menu-insert",
        "pdf-menu-delete",
        "pdf-menu-extract",
        "pdf-menu-bg-color",
    ] {
        assert!(
            cx.debug_bounds(id).is_some(),
            "悬停后父项「{id}」应保持可见（不原地替换）"
        );
    }
    // 真实排列（「背景色」在两条分隔线之后、下标 6）下次级顶部仍须 ≈ 背景色根行顶部
    // （偏移须按真实行高累计，分割线行 ≠ 常规项行高）。
    let fly = cx.debug_bounds("menu-flyout").expect("次级应在场");
    let bg_row = cx.debug_bounds("pdf-menu-bg-color").expect("背景色行应在场");
    let dy = (fly.top() - bg_row.top()).abs();
    assert!(
        dy <= px(8.),
        "真实菜单下次级顶部应≈背景色行顶部，实际差 {dy:?}"
    );
    // 悬停普通项（添加书签）→ 次级收起、父菜单保持展开。
    let bm = bg_menu_center(cx, "pdf-menu-bookmark");
    cx.simulate_mouse_move(bm, None, gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("pdf-menu-bg-default").is_none(),
        "悬停普通项应收起次级"
    );
    assert!(
        cx.debug_bounds("pdf-menu-bg-color").is_some(),
        "次级收起后父菜单保持展开"
    );
}

/// R2/R4：悬停「背景色」进次级；点「夜间」→ 画布立即 #202020；点「默认」→ 回落主题（bg=None）。
#[gpui::test]
async fn pdf_bg_preset_flyout_click_applies_and_default_resets(cx: &mut TestAppContext) {
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
    let pdf = bg_open_pdf_right_click(&app, cx, "bgpreset");
    // 悬停「背景色」→ 次级菜单展开。
    let bg = bg_menu_center(cx, "pdf-menu-bg-color");
    cx.simulate_mouse_move(bg, None, gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("pdf-menu-bg-night").is_some(),
        "悬停背景色应展开次级菜单"
    );
    // 移入次级再点「夜间」→ 画布立即 #202020（keep_open：菜单/次级保持展开）。
    let night = bg_menu_center(cx, "pdf-menu-bg-night");
    cx.simulate_mouse_move(night, None, gpui::Modifiers::default());
    cx.run_until_parked();
    cx.simulate_click(night, gpui::Modifiers::default());
    settle_frames(cx, 4);
    assert_eq!(
        pdf.read_with(cx, |v, _| v.background()),
        Some(gpui::rgb(0x20_20_20).into()),
        "夜间预设应立即落到画布"
    );
    assert_eq!(bg_pref(&app, cx), PdfBgPref::Night, "偏好应记夜间档");
    // R6-3：纸墨重着色参数（纸 #202020 + 墨色提亮 #D8D8D8，内容中间调不动）落地到位图显示层。
    assert_eq!(
        pdf.read_with(cx, |v, _| v.recolor()),
        Some(pdf::RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) }),
        "夜间预设应落地纸色+墨迹提亮参数"
    );
    assert!(cx.debug_bounds("pdf-menu-bg-night").is_some(), "keep_open：次级应保持展开");
    assert!(cx.debug_bounds("pdf-menu-bg-color").is_some(), "keep_open：父菜单应保持展开");
    // 移入次级再点「默认」→ 回落跟随主题（bg=None）+ 无重着色（直通）。
    let def = bg_menu_center(cx, "pdf-menu-bg-default");
    cx.simulate_mouse_move(def, None, gpui::Modifiers::default());
    cx.run_until_parked();
    cx.simulate_click(def, gpui::Modifiers::default());
    settle_frames(cx, 4);
    assert_eq!(
        pdf.read_with(cx, |v, _| v.background()),
        None,
        "默认档应回落跟随主题"
    );
    assert_eq!(bg_pref(&app, cx), PdfBgPref::Default, "偏好应记默认档");
    assert_eq!(
        pdf.read_with(cx, |v, _| v.recolor()),
        None,
        "默认档应无纸墨重着色（直通）"
    );
}

/// R7：显式色下切主题画布不变；默认色下切主题 bg 保持 None（跟随主题）。
#[gpui::test]
async fn pdf_bg_explicit_survives_theme_switch_but_default_follows(cx: &mut TestAppContext) {
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
    let p = temp_pdf_copy("bgtheme");
    open_pdf(&app, cx, &p);
    let pdf = bg_active_pdf(&app, cx);
    // 显式「夜间」→ 画布 #202020 + 纸墨重着色。
    cx.update(|_w, cx| {
        app.update(cx, |a, cx| a.set_pdf_bg(PdfBgPref::Night, None, cx));
    });
    let night_params = pdf::RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
    assert_eq!(pdf.read_with(cx, |v, _| v.background()), Some(gpui::rgb(0x20_20_20).into()));
    assert_eq!(pdf.read_with(cx, |v, _| v.recolor()), Some(night_params));
    // 切主题 → 显式色不被覆盖（画布与重着色均保持）。
    cx.update(|window, cx| {
        app.update(cx, |a, cx| a.toggle_theme(window, cx));
    });
    settle_frames(cx, 3);
    assert_eq!(
        pdf.read_with(cx, |v, _| v.background()),
        Some(gpui::rgb(0x20_20_20).into()),
        "显式背景下主题切换不得覆盖画布色"
    );
    assert_eq!(
        pdf.read_with(cx, |v, _| v.recolor()),
        Some(night_params),
        "显式重着色下主题切换不得清除"
    );
    // 回「默认」→ bg=None（跟随主题）+ 重着色 None。
    cx.update(|_w, cx| {
        app.update(cx, |a, cx| a.set_pdf_bg(PdfBgPref::Default, None, cx));
    });
    assert_eq!(pdf.read_with(cx, |v, _| v.background()), None, "默认档应回落跟随主题");
    assert_eq!(pdf.read_with(cx, |v, _| v.recolor()), None, "默认档应无重着色");
    // 再切主题 → 仍 None（default_canvas 随主题渲染）。
    cx.update(|window, cx| {
        app.update(cx, |a, cx| a.toggle_theme(window, cx));
    });
    settle_frames(cx, 3);
    assert_eq!(
        pdf.read_with(cx, |v, _| v.background()),
        None,
        "默认档切主题 bg 应保持 None（渲染经 default_canvas 跟随）"
    );
    assert_eq!(
        pdf.read_with(cx, |v, _| v.recolor()),
        None,
        "默认档切主题仍无重着色"
    );
}
