    // ---------------------------------------------------------------------
    // P2-8 命令面板：命令模式（Ctrl+Shift+P）+ 文件模式（Ctrl+P）
    // ---------------------------------------------------------------------

    /// 命令模式：Ctrl+Shift+P（ToggleCommandPalette）打开 → 键入过滤缩减命令 →
    /// ↓ 导航 → Enter 执行「新建文件」→ 面板关闭（DismissEvent 清引用）；
    /// 再开 → Esc 关闭且焦点归还打开前的编辑区。
    #[gpui::test]
    async fn command_palette_open_filter_run_and_escape(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 聚焦编辑区（应用级动作经焦点祖先链派发到 App 根 on_action）。
        cx.update(|window, cx| {
            let f = app.read(cx).editor_focus_handle(cx);
            window.focus(&f, cx);
        });
        cx.run_until_parked();

        // Ctrl+Shift+P → 命令面板打开，初始列出全部命令并选中首项。
        cx.dispatch_action(ToggleCommandPalette);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (total, sel) = cx.update(|_w, cx| {
            let p = app.read(cx).palette.as_ref().unwrap().read(cx);
            (p.visible_len(), p.selected())
        });
        assert!(total > 5, "命令面板应列出全部命令，实际={total}");
        assert_eq!(sel, Some(0), "初始应选中首项");

        // 键入「新建」→ 过滤后只剩「新建文件」一条。
        let palette = cx.update(|_w, cx| app.read(cx).palette.as_ref().unwrap().clone());
        cx.update(|window, cx| {
            let input = palette.read(cx).input_entity();
            input.update(cx, |s, cx| s.set_value("新建", window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (len, label, query) = cx.update(|_w, cx| {
            let p = palette.read(cx);
            (
                p.visible_len(),
                p.visible_label(0).map(str::to_string),
                p.query().to_string(),
            )
        });
        assert_eq!(query, "新建", "查询词应写进面板状态");
        assert_eq!(len, 1, "过滤「新建」应只剩 1 条命令，实际={len}");
        assert_eq!(label.as_deref(), Some("新建文件"));

        // ↓：过滤后仅 1 条 → 环绕回 0；再 Enter 执行「新建文件」。
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        let sel2 = cx.update(|_w, cx| palette.read(cx).selected());
        assert_eq!(sel2, Some(0), "单条过滤下 ↓ 应环绕回 0，实际={sel2:?}");
        cx.simulate_keystrokes("enter");
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (closed, tabs_len) = cx.update(|_w, cx| {
            (app.read(cx).palette.is_none(), app.read(cx).tabs.len())
        });
        assert!(closed, "Enter 执行后面板应关闭（DismissEvent 清引用）");
        assert_eq!(tabs_len, 2, "「新建文件」应新增一个选项卡，实际={tabs_len}");

        // 再开 → Esc 关闭，且焦点归还打开前的编辑区。
        cx.update(|window, cx| {
            let expected = app.read(cx).editor_focus_handle(cx);
            window.focus(&expected, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(ToggleCommandPalette);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(cx.update(|_w, cx| app.read(cx).palette.is_some()), "Ctrl+Shift+P 应再打开面板");
        cx.simulate_keystrokes("escape");
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (closed2, focused_opt) = cx.update(|window, cx| {
            (app.read(cx).palette.is_none(), window.focused(cx))
        });
        assert!(closed2, "Esc 应关闭面板");
        let expected = cx.update(|_w, cx| app.read(cx).editor_focus_handle(cx));
        assert_eq!(
            focused_opt.as_ref(),
            Some(&expected),
            "关闭后焦点应归还打开前的编辑区"
        );

        // 点击面板外（中心 600px 面板之外的点）→ 关闭（deferred 层 on_mouse_down_out 路径）。
        cx.dispatch_action(ToggleCommandPalette);
        cx.run_until_parked();
        assert!(cx.update(|_w, cx| app.read(cx).palette.is_some()), "面板应再次打开");
        cx.simulate_click(gpui::Point::new(px(900.), px(400.)), Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let closed3 = cx.update(|_w, cx| app.read(cx).palette.is_none());
        assert!(closed3, "点击面板外应关闭命令面板");
    }

    /// 文件模式：Ctrl+P（QuickOpenFile）列出已打开文件（含路径提示）→
    /// 键入文件名过滤 → Enter 切到对应选项卡并关闭面板。
    #[gpui::test]
    async fn quick_open_file_lists_and_switches_tabs(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 打开两个真实文件：A 先（下标 1）、B 后（下标 2，活动）。
        let a = std::env::temp_dir().join("gpui-pal-a.md");
        let b = std::env::temp_dir().join("gpui-pal-b.md");
        std::fs::write(&a, "# A\nhello\n").unwrap();
        std::fs::write(&b, "# B\nworld\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(a.to_str().unwrap(), window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(b.to_str().unwrap(), window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let tab_count = cx.update(|_w, cx| app.read(cx).tabs.len());
        assert_eq!(tab_count, 3, "未命名 + A + B 共 3 个选项卡");

        // 聚焦编辑区后 Ctrl+P → 文件模式面板，列出已打开文件（含「打开文件…」入口）。
        cx.update(|window, cx| {
            let f = app.read(cx).editor_focus_handle(cx);
            window.focus(&f, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(QuickOpenFile);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let palette = cx.update(|_w, cx| app.read(cx).palette.as_ref().unwrap().clone());
        let (mode, n, first_label) = cx.update(|_w, cx| {
            let p = palette.read(cx);
            (
                p.mode(),
                p.visible_len(),
                p.visible_label(0).map(str::to_string),
            )
        });
        assert_eq!(
            mode,
            ui::palette::PaletteMode::Files,
            "Ctrl+P 应为文件模式"
        );
        assert!(n >= 4, "应列出 未命名 + A + B + 打开文件… 至少 4 条，实际={n}");
        assert_eq!(first_label.as_deref(), Some("未命名"), "首项应为首个打开的（未命名）选项卡");

        // 键入 A 的文件名 → 过滤到只剩 A。
        cx.update(|window, cx| {
            let input = palette.read(cx).input_entity();
            input.update(cx, |s, cx| s.set_value("gpui-pal-a.md", window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (len, label) = cx.update(|_w, cx| {
            let p = palette.read(cx);
            (p.visible_len(), p.visible_label(0).map(str::to_string))
        });
        assert_eq!(len, 1, "过滤应只剩 A，实际={len}");
        assert_eq!(label.as_deref(), Some("gpui-pal-a.md"));

        // Enter → 切到 A 的选项卡（下标 1）并关闭面板。
        cx.simulate_keystrokes("enter");
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (closed, active) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.palette.is_none(), a.active_tab)
        });
        assert!(closed, "切换后面板应关闭");
        assert_eq!(active, 1, "Enter 后应切回 A 的选项卡，实际={active}");

        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// 面板高度 = 窗口 2/3：列表项多时面板不再探出屏幕底，列表区按剩余高度定高，
    /// 过长内容在容器内内部滚动（右侧自绘滚动条随之出现，见 scrollbar 组件
    /// `hides_when_content_fits`）。
    #[gpui::test]
    async fn command_palette_height_capped_and_list_scrolls(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        let win_h = 600.0_f32;
        cx.simulate_resize(gpui::size(px(700.), px(win_h)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 聚焦编辑区（应用级动作经焦点祖先链派发到 App 根 on_action）。
        cx.update(|window, cx| {
            let f = app.read(cx).editor_focus_handle(cx);
            window.focus(&f, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(ToggleCommandPalette);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(cx.update(|_w, cx| app.read(cx).palette.is_some()), "命令面板应打开");

        // ① 面板高度 = 窗口 2/3（不随 16+ 条命令的总高增长）。
        let panel = cx.debug_bounds("command-palette").expect("面板应渲染");
        let expect_h = px(win_h * 2.0 / 3.0);
        assert!(
            (panel.size.height - expect_h).abs() < px(1.0),
            "面板高度应为窗口 2/3（{expect_h:?}），实际={:?}",
            panel.size.height
        );
        // 面板顶部 96px 偏移 + 2/3 高恒在窗口内。
        assert!(
            panel.bottom() <= px(win_h),
            "面板不得探出窗口底部：bottom={:?}（窗口高 {win_h}）",
            panel.bottom()
        );

        // ② 列表滚动容器有界（剩余高度）：16+ 条命令在容器内可滚动，而非把面板撑高。
        let scroll = cx
            .debug_bounds("command-palette-scroll")
            .expect("列表滚动容器应渲染");
        assert!(scroll.size.height > px(0.), "列表滚动容器应有正高度：{scroll:?}");
        assert!(
            scroll.size.height < panel.size.height - px(40.),
            "列表滚动容器应为剩余高度并被钳在面板内：scroll={scroll:?} panel={panel:?}"
        );
        let max_off = cx.update(|_w, cx| {
            let p = app.read(cx).palette.as_ref().unwrap().read(cx);
            p.scroll_handle().max_offset().y
        });
        assert!(
            max_off > px(0.),
            "列表溢出时应在容器内可滚动：max_offset={max_off:?}"
        );
    }

    /// 回归（用户报告 #3）：打开文件后 Ctrl+Shift+P 打开面板，面板输入框必须可聚焦与可键入，
    /// 点击不得穿透到下层编辑器。
    ///
    /// 覆盖两条：
    /// ① 打开真实文件（焦点在编辑区）→ 触发 Ctrl+P 路径动作 `ToggleCommandPalette` →
    ///    面板打开时输入框持有焦点；随后键入字符确实写入查询框。
    /// ② 焦点被夺回编辑器（模拟点击穿透后果）→ 鼠标点击面板输入框中心 →
    ///    焦点应收纳到面板输入框（不得停留在编辑器）；随后键入仍写入查询框。
    ///
    /// 历史根因：面板浮层是 `deferred(absolute+flex)` 时 hitbox 位置失真（第一段探针
    /// 最小结构复现），且浮层无 BlockMouse 屏障，点击命中面板元素后仍继续命中下方编辑器、
    /// 编辑器 on_mouse_down 把焦点抢回。修复：全窗包裹去 flex、显式 left/top 定位 + `.occlude()`。
    #[gpui::test]
    async fn palette_input_focusable_and_clickable_after_open_file(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 真实文件（热键路径：先打开文件，焦点落在编辑区）
        let p = std::env::temp_dir().join(format!("gpui-pal-reg-{}.rs", std::process::id()));
        std::fs::write(&p, "fn main() {}\n").unwrap();
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

        // ① 打开面板（Ctrl+P 同一动作路径）→ 输入框持有焦点
        cx.dispatch_action(ToggleCommandPalette);
        for _ in 0..6 {
            cx.run_until_parked();
        }
        let palette = cx.update(|_w, cx| app.read(cx).palette.as_ref().expect("面板应已打开").clone());
        let input_handle = cx.update(|_w, cx| palette.read(cx).input_handle(cx));
        let focused_on_open = cx.update(|window, cx| window.focused(cx));
        assert_eq!(
            focused_on_open.as_ref(),
            Some(&input_handle),
            "打开面板时输入框应持有焦点"
        );
        cx.simulate_keystrokes("cop");
        cx.run_until_parked();
        let query_after_open = cx.update(|_w, cx| palette.read(cx).query().to_string());
        assert_eq!(query_after_open, "cop", "打开后键入应写入查询框");

        // ② 焦点夺回编辑器（模拟点击穿透的后果）→ 点击输入框应收纳焦点
        cx.update(|window, cx| {
            let f = app.read(cx).editor_focus_handle(cx);
            window.focus(&f, cx);
        });
        cx.run_until_parked();
        let editor_handle = cx.update(|_w, cx| app.read(cx).editor_focus_handle(cx));
        assert_eq!(
            cx.update(|window, cx| window.focused(cx)).as_ref(),
            Some(&editor_handle),
            "前置：焦点应在编辑区"
        );
        let qb = cx.debug_bounds("command-palette-query").expect("查询框应渲染");
        let click_point = gpui::Point::new(qb.origin.x + qb.size.width / 2., qb.origin.y + qb.size.height / 2.);
        cx.simulate_click(click_point, Default::default());
        cx.run_until_parked();
        cx.run_until_parked();
        let focused_after_click = cx.update(|window, cx| window.focused(cx));
        assert_eq!(
            focused_after_click.as_ref(),
            Some(&input_handle),
            "点击面板输入框应收纳焦点（不得因穿透停留在编辑器）"
        );
        cx.simulate_keystrokes("y");
        cx.run_until_parked();
        let query_after_click_typing = cx.update(|_w, cx| palette.read(cx).query().to_string());
        assert_eq!(
            query_after_click_typing, "copy",
            "点击聚焦后键入应写入查询框"
        );

        // 收尾：Esc 关闭后焦点回落编辑区
        cx.simulate_keystrokes("escape");
        for _ in 0..6 {
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| app.read(cx).palette.is_none()),
            "Esc 应关闭面板"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 外壳无缝直连（阶段一回归）——三块方直角区块的「零缝隙」几何（真实渲染路径
    /// `debug_bounds` 断言）：
    /// - 贴边直角：侧栏左缘贴窗（`origin.x == 0`）、正文块右缘贴窗（`right == 窗宽`）、
    ///   状态栏左/右/底缘贴窗（`origin.x == 0`、`right == 窗宽`、`bottom == 窗高`）；
    /// - 无缝直连（零间距）：侧栏右缘 ↔ 正文块左缘、正文块顶 ↔ 选项卡栏底、
    ///   正文块底 ↔ 状态栏顶，三处间距均 ≈ 0；
    /// - 面板收起时无右缘手柄（`sidebar-resize`）也无旧空间隔（`sidebar-gap`）；
    ///   面板展开时右缘调宽手柄 `sidebar-resize` 渲染（绝对细线内嵌侧栏右缘，非分隔带）；
    /// - 整体收起时 `app-body-with-sidebar` 包裹层一并消失（`sidebar-gap` 恒不存在）。
    #[gpui::test]
    async fn shell_seamless_geometry(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let _ = app;
        let win_w = 980.0f32;
        let win_h = 720.0f32;

        // —— 侧栏展开（默认：活动栏 + 面板收起）——
        let sb = cx
            .debug_bounds("sidebar")
            .expect("侧边栏 sidebar 应渲染");
        let main = cx
            .debug_bounds("app-main-card")
            .expect("正文块 app-main-card 应渲染");
        let tb = cx
            .debug_bounds("tab-bar")
            .expect("选项卡栏 tab-bar 应渲染");
        let st = cx
            .debug_bounds("status-bar")
            .expect("状态栏 status-bar 应渲染");
        // 贴边直角四处。
        assert!(
            sb.origin.x.as_f32() <= 0.5,
            "侧栏左缘应贴窗（直角），sb={sb:?}"
        );
        assert!(
            (main.right().as_f32() - win_w).abs() < 1.0,
            "正文块右缘应贴窗（直角），main={main:?}"
        );
        assert!(
            st.origin.x.as_f32() <= 0.5,
            "状态栏左缘应贴窗（直角），st={st:?}"
        );
        assert!(
            (st.right().as_f32() - win_w).abs() < 1.0,
            "状态栏右缘应贴窗（直角），st={st:?}"
        );
        assert!(
            (st.bottom().as_f32() - win_h).abs() < 1.0,
            "状态栏底缘应贴窗（直角），st={st:?}"
        );
        // 无缝直连（零间距）：侧栏右缘 → 正文块左缘、正文块顶 → 选项卡栏底、
        // 正文块底 → 状态栏顶。
        let seam_sb_main = main.origin.x.as_f32() - sb.right().as_f32();
        assert!(
            (seam_sb_main).abs() < 1.5,
            "侧栏↔正文应无缝直连（无间距），实际={seam_sb_main}"
        );
        let seam_tb_main = main.origin.y.as_f32() - tb.bottom().as_f32();
        assert!(
            (seam_tb_main).abs() < 1.5,
            "正文↔选项卡栏应无缝直连（无间距），实际={seam_tb_main}"
        );
        let seam_main_st = st.origin.y.as_f32() - main.bottom().as_f32();
        assert!(
            (seam_main_st).abs() < 1.5,
            "正文↔状态栏应无缝直连（无间距），实际={seam_main_st}"
        );
        // 面板收起（默认）：无右缘调宽手柄，也无旧 sidebar-gap 空间隔元素。
        assert!(
            cx.debug_bounds("sidebar-resize").is_none(),
            "面板收起不应有无右缘调宽手柄"
        );
        assert!(
            cx.debug_bounds("sidebar-gap").is_none(),
            "无缝直连后不应再有 sidebar-gap 空间隔元素"
        );

        // —— 面板展开：侧栏右缘调宽手柄渲染（内嵌细线，非分隔带）——
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-resize").is_some(),
            "面板展开时侧栏右缘应有调宽手柄 sidebar-resize"
        );
        assert!(
            cx.debug_bounds("sidebar-gap").is_none(),
            "面板展开也不应有 sidebar-gap 空间隔元素"
        );

        // —— 整体收起：内容区包裹层与旧间距元素都消失 ——
        cx.update(|_w, cx| app.update(cx, |app, cx| app.toggle_sidebar(cx)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_none(),
            "整体收起后内容区包裹层不应渲染"
        );
        assert!(
            cx.debug_bounds("sidebar-gap").is_none(),
            "整体收起后不应有 sidebar-gap"
        );
    }

    /// 零选项卡欢迎页也住在正文卡里（`app-main-card`），卡片化不破坏欢迎页布局。
    #[gpui::test]
    async fn welcome_page_stays_in_main_card(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let _ = app;
        assert!(
            cx.debug_bounds("app-main-card").is_some(),
            "零 tab 欢迎页应仍在正文卡内"
        );
        assert!(
            cx.debug_bounds("app-empty-body").is_some(),
            "欢迎页主体 app-empty-body 应渲染"
        );
        assert!(
            cx.debug_bounds("editor-area").is_none(),
            "零 tab 无编辑区（回归）"
        );
    }

