    /// 侧边栏：默认仅收窄为活动栏（目录面板收起，占 46px 不挤正文）；
    /// 整体收起后移除；标题栏右上角有展开/收起开关（title-sidebar-toggle）。
    #[gpui::test]
    async fn sidebar_collapses_and_expands(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 默认：侧边栏在（活动栏），宽 ≈ SIDEBAR_ACTIVITY_W，目录面板未自动展开
        let bar = cx
            .debug_bounds("sidebar")
            .expect("侧边栏 sidebar 应默认渲染");
        assert!(
            (bar.size.width.as_f32() - config::layout::SIDEBAR_ACTIVITY_W).abs() < 2.0,
            "默认目录面板收起，侧边栏应只剩活动栏宽，实际={bar:?}"
        );
        assert!(
            cx.debug_bounds("title-sidebar-toggle").is_some(),
            "标题栏应有侧边栏开关 title-sidebar-toggle"
        );

        // 整体收起：sidebar 与内容区包裹层一起移除
        cx.update(|_w, cx| app.update(cx, |app, cx| app.toggle_sidebar(cx)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar").is_none(),
            "收起后侧边栏不应再渲染"
        );
        assert!(
            cx.debug_bounds("app-body-with-sidebar").is_none(),
            "收起后内容区包裹层不应再渲染"
        );

        // 再次展开：侧边栏恢复
        cx.update(|_w, cx| app.update(cx, |app, cx| app.toggle_sidebar(cx)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar").is_some(),
            "再次展开后侧边栏应恢复"
        );
    }

    /// 侧边栏「目录 / 搜索 / 文件」竖列切换栏可调用出对应界面。
    #[gpui::test]
    async fn sidebar_views_switch(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 目录面板默认收起（活动栏常驻）；先点「目录」打开目录视图
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 竖列切换栏有「目录/搜索/文件」三个按钮
        assert!(
            cx.debug_bounds("sidebar-toc").is_some(),
            "默认应为目录视图"
        );
        assert!(
            cx.debug_bounds("sidebar-activity-bar").is_some(),
            "应有竖列切换栏 sidebar-activity-bar"
        );
        assert!(
            cx.debug_bounds("sidebar-btn-toc").is_some(),
            "应有目录按钮"
        );
        assert!(
            cx.debug_bounds("sidebar-btn-search").is_some(),
            "应有搜索按钮"
        );
        assert!(
            cx.debug_bounds("sidebar-btn-files").is_some(),
            "应有文件按钮"
        );
        // 三个按钮上下竖列（目录最小 y、文件最大 y）
        let toc = cx.debug_bounds("sidebar-btn-toc").unwrap();
        let search = cx.debug_bounds("sidebar-btn-search").unwrap();
        let files = cx.debug_bounds("sidebar-btn-files").unwrap();
        assert!(
            search.origin.y > toc.origin.y && files.origin.y > search.origin.y,
            "按钮应按 目录→搜索→文件 自上而下竖列：toc={toc:?} search={search:?} files={files:?}"
        );

        // 切到搜索视图
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx)
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-search").is_some(),
            "搜索视图应渲染 sidebar-search"
        );
        assert!(
            cx.debug_bounds("sidebar-search-input").is_some(),
            "搜索视图应有输入框 sidebar-search-input"
        );
        assert!(
            cx.debug_bounds("sidebar-toc").is_none(),
            "切换到搜索后不应再渲染目录视图"
        );
    }

    /// 搜索输入框左右留白**对称**：右侧不得贴搜索面板右缘（=侧边栏与正文的分隔线）。
    /// 回归：`w_full() + mx_2()` 在 flex 列下宽度=100%+margin → 右缘 2px 溢出被面板
    /// `overflow_hidden` 裁掉 → 右侧贴边、左侧有缝（K138：锁真实对位量左右间隙，非趋势）。
    #[gpui::test]
    async fn sidebar_search_input_keeps_symmetric_margin(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let panel = cx
            .debug_bounds("sidebar-search")
            .expect("搜索视图应渲染 sidebar-search");
        let input = cx
            .debug_bounds("sidebar-search-input")
            .expect("搜索视图应有输入框 sidebar-search-input");
        let panel_right = (panel.origin.x + panel.size.width).as_f32();
        let input_left = input.origin.x.as_f32();
        let input_right = (input.origin.x + input.size.width).as_f32();
        let left_gap = input_left - panel.origin.x.as_f32();
        let right_gap = panel_right - input_right;
        assert!(
            left_gap > 0.0,
            "输入框左侧应距面板左缘留白，left_gap={left_gap}"
        );
        assert!(
            right_gap > 0.0,
            "输入框右侧应距面板右缘（分隔线）留白，不得贴边，right_gap={right_gap}"
        );
        assert!(
            (left_gap - right_gap).abs() < 0.5,
            "输入框左右留白应对称：left={left_gap} right={right_gap}"
        );
    }

    /// P2-9：活动栏「目录」图标悬停 → 200ms 防抖后出现 tooltip（浮在按钮右侧、不盖活动栏），
    /// 移出立即消失。
    #[gpui::test]
    async fn activity_bar_tooltip_delayed_appear_and_hide(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let _ = app;
        // 活动栏按钮中心（目录）
        let btn = cx
            .debug_bounds("sidebar-btn-toc")
            .expect("活动栏应渲染目录按钮");
        let c = btn.center();
        // 初始无浮层
        assert!(
            cx.debug_bounds("tooltip-sidebar-btn-toc").is_none(),
            "未悬停时不应渲染 tooltip"
        );
        // 悬停进入：防抖期内（< TOOLTIP_DELAY_MS=200ms）不显示
        cx.simulate_mouse_move(c, None, Default::default());
        cx.executor().advance_clock(std::time::Duration::from_millis(100));
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("tooltip-sidebar-btn-toc").is_none(),
            "防抖期内（100ms<200ms）tooltip 不应出现"
        );
        // 时钟再推进超过防抖 → 浮层出现，且在按钮右缘（不盖活动栏本体）
        cx.executor().advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let tip = cx
            .debug_bounds("tooltip-sidebar-btn-toc")
            .expect("延迟后 tooltip 应出现");
        assert!(
            tip.origin.x.as_f32() >= btn.origin.x.as_f32() + btn.size.width.as_f32(),
            "tooltip 应贴按钮右缘（不盖活动栏），tip={tip:?} btn={btn:?}"
        );
        // 移出 → 立即消失
        cx.simulate_mouse_move(gpui::point(px(8.), px(8.)), None, Default::default());
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("tooltip-sidebar-btn-toc").is_none(),
            "移出后 tooltip 应立即消失"
        );
    }

    /// P2-9：tooltip 浮层不拦截点击——悬停显示 tooltip 后，点击活动栏「目录」按钮
    /// 仍正常切换/收起目录面板（浮层无 hitbox、命中穿透）。
    #[gpui::test]
    async fn activity_bar_tooltip_does_not_block_click(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 悬停目录按钮 → 推进防抖 → tooltip 出现
        let btn = cx
            .debug_bounds("sidebar-btn-toc")
            .expect("活动栏应渲染目录按钮");
        cx.simulate_mouse_move(btn.center(), None, Default::default());
        cx.executor().advance_clock(std::time::Duration::from_millis(400));
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("tooltip-sidebar-btn-toc").is_some(),
            "tooltip 应已显示"
        );
        // tooltip 可见时点按钮本体 → 正常展开目录面板
        cx.simulate_click(btn.center(), Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut panel_open = cx.update(|_w, cx| app.read(cx).sidebar.panel_open);
        assert!(panel_open, "tooltip 可见时点击目录按钮仍应展开目录面板");
        assert!(
            cx.debug_bounds("sidebar-toc").is_some(),
            "点击后应渲染目录面板"
        );
        // 再点一次 → 收起（往返可用，不因 tooltip 残留状态损坏点击）
        cx.simulate_click(btn.center(), Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        panel_open = cx.update(|_w, cx| app.read(cx).sidebar.panel_open);
        assert!(!panel_open, "再点一次应收起目录面板");
    }

    /// P2-11「图标注入感」：窗口控制钮 hover/按压时图标前景由次要灰切到与底对应的
    /// 对比前景（最小化/最大化→主前景、关闭→白×），移出回落。经真实渲染路径断言：
    /// `simulate_mouse_move` 驱动 keyed hover/press 态 → `render_title_bar` 把本帧
    /// 解析出的前景色落盘到 `last_title_ctrl_fg`。
    #[gpui::test]
    async fn titlebar_window_buttons_inject_icon_fg_on_hover(cx: &mut TestAppContext) {
        fn same_color(a: gpui::Hsla, b: gpui::Hsla) -> bool {
            (a.h - b.h).abs() < 1e-5
                && (a.s - b.s).abs() < 1e-5
                && (a.l - b.l).abs() < 1e-5
                && (a.a - b.a).abs() < 1e-5
        }
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let idle = cx
            .update(|_w, cx| {
                app.read(cx)
                    .last_title_ctrl_fg
                    .expect("标题栏渲染后应落盘窗口钮前景色")
            });
        let minor = cx.update(|_w, cx| app.read(cx).text_minor());
        let primary = cx.update(|_w, cx| app.read(cx).text_primary());
        assert!(
            same_color(idle.minimize, minor) && same_color(idle.close, minor),
            "空闲时窗口钮图标应为次要灰，实际 min={:?} close={:?}",
            idle.minimize,
            idle.close
        );

        // hover 最小化 → 注入主前景（灰底对比前景）
        let min_btn = cx
            .debug_bounds("title-minimize")
            .expect("应渲染 title-minimize");
        cx.simulate_mouse_move(min_btn.center(), None, Default::default());
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let hovered = cx.update(|_w, cx| app.read(cx).last_title_ctrl_fg.unwrap());
        assert!(
            same_color(hovered.minimize, primary),
            "hover 最小化图标应注入主前景，实际 {:?}",
            hovered.minimize
        );

        // hover 关闭 → 白 ×（危险钮观感）
        let close_btn = cx
            .debug_bounds("title-close")
            .expect("应渲染 title-close");
        cx.simulate_mouse_move(close_btn.center(), None, Default::default());
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let hovered_close = cx.update(|_w, cx| app.read(cx).last_title_ctrl_fg.unwrap());
        assert!(
            same_color(hovered_close.close, white()),
            "hover 关闭图标应为白 ×，实际 {:?}",
            hovered_close.close
        );

        // 按住：按压态独立跟随（图标仍注入）
        cx.simulate_mouse_down(close_btn.center(), MouseButton::Left, Default::default());
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let pressed = cx.update(|_w, cx| app.read(cx).last_title_ctrl_fg.unwrap());
        assert!(
            same_color(pressed.close, white()),
            "按压关闭图标应保持白 ×，实际 {:?}",
            pressed.close
        );

        // 松开并移出 → 回落次要灰
        cx.simulate_mouse_up(close_btn.center(), MouseButton::Left, Default::default());
        cx.simulate_mouse_move(gpui::point(px(30.), px(300.)), None, Default::default());
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let away = cx.update(|_w, cx| app.read(cx).last_title_ctrl_fg.unwrap());
        assert!(
            same_color(away.close, minor),
            "移出后关闭图标应回落次要灰，实际 {:?}",
            away.close
        );
    }

    /// Win11 Fluent 行为：窗口控制钮组（最小化/最大化/关闭）右缘应**贴齐窗口右缘**，
    /// 关闭钮右侧不得有可见 gap。回归：`render_title_bar` 根容器此前用 `px_2`（左+右
    /// 各 8px），会在关闭钮右侧留下 8px 右内边距；改为仅左内边距 `pl_2` 后关闭钮右缘
    /// == 标题栏右缘（`title-bar` 是 `w_full` 的根容器，其右缘即窗口右缘）。
    /// 亮暗主题几何不变，但按验收要求两态都断言一次。
    #[gpui::test]
    async fn title_bar_window_controls_hug_right_edge(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 断言函数：关闭钮右缘贴齐标题栏右缘（gap ≤ 1px 容差）、且最右侧是关闭钮。
        let assert_hug = |cx: &mut gpui::VisualTestContext| {
            let bar = cx
                .debug_bounds("title-bar")
                .expect("标题栏应渲染");
            let close = cx
                .debug_bounds("title-close")
                .expect("关闭钮应渲染");
            let min_ctrl = cx
                .debug_bounds("title-minimize")
                .expect("最小化钮应渲染");
            let bar_right = bar.origin.x.as_f32() + bar.size.width.as_f32();
            let close_right = close.origin.x.as_f32() + close.size.width.as_f32();
            let close_left = close.origin.x.as_f32();
            let gap = (bar_right - close_right).max(0.);
            assert!(
                close_right <= bar_right + 1.0,
                "关闭钮右缘不得超出窗口右缘：close_right={close_right:.1} bar_right={bar_right:.1}"
            );
            assert!(
                gap <= 1.0,
                "关闭钮右缘应贴齐窗口右缘（无可见 gap），实际 gap={gap:.1}px"
            );
            assert!(
                close_left > min_ctrl.origin.x.as_f32(),
                "关闭钮应在最小化钮右侧（三钮顺序未被破坏）"
            );
        };
        // 默认主题（浅色）先断言。
        assert_hug(cx);

        // 切深色再断言（真主题切换路径 `refresh_theme_ui`，不写盘用户偏好）。
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                a.model.borrow_mut().dark_mode = true;
                a.refresh_theme_ui(cx);
                cx.notify();
            });
        });
        for _ in 0..4 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        assert_hug(cx);

        // 再切回浅色（还原现场，避免污染后续测试的 theme 全局态）。
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                a.model.borrow_mut().dark_mode = false;
                a.refresh_theme_ui(cx);
                cx.notify();
            });
        });
        for _ in 0..4 {
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        assert_hug(cx);
    }

    /// A4：侧边栏 Markdown 目录**点击跳转**——条目走统一组件 WyEditor（源码行）：
    /// 点「# 大标题 18」（文中部）→ 右栏预览 `scroll_to_line` 滚动到该标题所在块
    /// （宿主滚动句柄从顶部偏离到负向偏移，预览跟随跳转）。
    #[gpui::test]
    async fn sidebar_toc_click_scrolls_md_wy_preview(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join(format!("gpui-mdwy-toc-{}.md", std::process::id()));
        let mut doc = String::new();
        for i in 0..40 {
            doc.push_str(&format!("# 大标题 {i}\n\n正文 {i}\n\n"));
        }
        std::fs::write(&p, &doc).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx),
            );
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let vhand = cx.update(|_, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy_vscroll.clone()
        });
        let before = cx.update(|_w, _cx| (-vhand.offset().y).as_f32());
        assert_eq!(before, 0.0, "初始预览滚动在顶部");
        let row = cx
            .debug_bounds("sidebar-toc-md-18")
            .expect("应渲染第 18 个目录条目（WyEditor TOC 驱动）");
        let pt = gpui::Point::new(row.left() + px(6.), row.top() + px(2.));
        cx.simulate_click(pt, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let after = cx.update(|_w, _cx| (-vhand.offset().y).as_f32());
        assert!(
            after > 10.0,
            "点击目录应把右栏 WyEditor 预览滚动到对应标题（after scroll={after:.0}px）"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 大文档目录虚拟化回归：万级标题点「目录」不再整棵物化——
    /// 1. 只物化可视带 + 缓冲（末尾条目在元素树中不存在）；
    /// 2. 滚动后可视带随偏移迁移（顶部条目出带、中部条目入带）；
    /// 3. 带内条目可折叠（折叠父项 → 后代隐藏、缓存失效重建）且可点击跳转。
    /// 用合成大文档（1200 个标题 > 虚拟化阈值 200）走真实渲染路径。
    #[gpui::test]
    async fn sidebar_toc_many_headings_virtualized(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join(format!("gpui-toc-virt-{}.md", std::process::id()));
        let mut doc = String::new();
        for i in 0..600 {
            doc.push_str(&format!("# 第{i}章\n## 小节{i}\n正文段落 {i}。\n\n"));
        }
        std::fs::write(&p, &doc).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }

        // 顶部可见带已物化；第 1000 个条目（远在带外）不得出现在元素树中（虚拟化生效）。
        assert!(
            cx.debug_bounds("sidebar-toc-md-0").is_some()
                && cx.debug_bounds("sidebar-toc-md-5").is_some(),
            "可视带顶部条目应渲染"
        );
        assert!(
            cx.debug_bounds("sidebar-toc-md-1000").is_none(),
            "虚拟化：带外（第 1000 个）条目不应被物化"
        );

        // 滚动到中部（≈1000px）→ 可视带迁移：顶部条目出带、中部条目入带。
        // （原生滚动位移在同帧 prepaint 落位，测试轮询至带稳定，防一帧竞态。）
        let vhand = cx.update(|_, cx| app.read(cx).sidebar.toc_vscroll.clone());
        cx.update(|_w, _cx| {
            vhand.set_offset(gpui::point(gpui::px(0.), gpui::px(-1000.)));
        });
        let mut moved = false;
        for _ in 0..12 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            if cx.debug_bounds("sidebar-toc-md-0").is_none()
                && cx.debug_bounds("sidebar-toc-md-40").is_some()
            {
                moved = true;
                break;
            }
        }
        assert!(
            moved,
            "滚动后可视带应迁移：顶部条目出带、中部条目入带"
        );

        // 回到顶部：折叠第一章（有 ## 子节）→ 后代隐藏、可见性缓存重建；
        // 再展开 → 恢复。
        cx.update(|_w, _cx| {
            vhand.set_offset(gpui::point(gpui::px(0.), gpui::px(0.)));
        });
        let mut at_top = false;
        for _ in 0..12 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            if cx.debug_bounds("sidebar-toc-md-0").is_some() {
                at_top = true;
                break;
            }
        }
        assert!(at_top, "滚回顶部后顶部条目应回到可视带");
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| app.toggle_sidebar_toc_collapse(0, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-toc-md-1").is_none()
                && cx.debug_bounds("sidebar-toc-md-0").is_some()
                && cx.debug_bounds("sidebar-toc-md-2").is_some(),
            "折叠第一章后其 ## 小节隐藏、其余顶级条目不收影响"
        );
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| app.toggle_sidebar_toc_collapse(0, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-toc-md-1").is_some(),
            "展开第一章后其 ## 小节恢复"
        );

        // 带内条目点击 → 右栏 WyEditor 预览滚动到对应标题（虚拟化带宽内命中可用）。
        let vhand_wy = cx.update(|_, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy_vscroll.clone()
        });
        let before = cx.update(|_w, _cx| (-vhand_wy.offset().y).as_f32());
        let row = cx
            .debug_bounds("sidebar-toc-md-2")
            .expect("可视带内条目应可点击");
        let pt = gpui::Point::new(row.left() + px(6.), row.top() + px(2.));
        cx.simulate_click(pt, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let after = cx.update(|_w, _cx| (-vhand_wy.offset().y).as_f32());
        assert!(
            after > before + 1.0,
            "点击可视带条目应滚动右栏预览（{before:.0} → {after:.0}px）"
        );
        let _ = std::fs::remove_file(&p);
    }
    /// 有子级的条目渲染折叠箭头，点击折叠后隐藏其后代标题，再点恢复。
    #[gpui::test]
    async fn sidebar_toc_pdf_style_collapsible(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join("vuick-sidebar-toc.md");
        std::fs::write(
            &dir,
            "# 第一章 这是一个特别长的标题用于验证目录自适应宽度\n正文\n## 第一节\n小节\n# 第二章\n结尾\n",
        )
        .unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 目录面板默认收起（不挤正文）；打开目录视图后再验证自适应宽度/条目/折叠
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx);
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 目录视图宽度自适应：长标题应把侧边栏撑到超过固定默认宽（240）
        let bar = cx
            .debug_bounds("sidebar")
            .expect("侧边栏 sidebar 应存在");
        assert!(
            bar.size.width.as_f32() > 240.0,
            "目录视图应随内容自适应加宽，实际={bar:?}"
        );
        assert!(
            cx.debug_bounds("sidebar-resize").is_some(),
            "侧边栏右缘应有拖拽手柄 sidebar-resize"
        );
        // 侧边栏目录应带「目录」面板头（无关闭按钮，开关走活动栏「目录」按钮）
        assert!(
            cx.debug_bounds("sidebar-toc-header").is_some(),
            "目录视图应有面板头 sidebar-toc-header（含「目录」标题）"
        );
        assert!(
            cx.debug_bounds("sidebar-toc-close").is_none(),
            "目录视图不应再有独立的关闭按钮（由「目录」按钮再点一次关闭）"
        );

        // 目录条目 + 折叠箭头渲染
        assert!(
            cx.debug_bounds("sidebar-toc-md-0").is_some()
                && cx.debug_bounds("sidebar-toc-md-1").is_some()
                && cx.debug_bounds("sidebar-toc-md-2").is_some(),
            "侧边栏目录应有标题条目 sidebar-toc-md-*"
        );
        assert!(
            cx.debug_bounds("sidebar-toc-caret-0").is_some(),
            "h1（第一章）有子级 h2，应渲染折叠箭头 sidebar-toc-caret-0"
        );

        // 折叠 h1 → h2 后代条目隐藏
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| app.toggle_sidebar_toc_collapse(0, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-toc-md-1").is_none(),
            "折叠父项后其后代 h2 条目应隐藏"
        );
        assert!(
            cx.debug_bounds("sidebar-toc-md-2").is_some(),
            "折叠不影响其它顶级条目"
        );

        // 再展开 → h2 恢复
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| app.toggle_sidebar_toc_collapse(0, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-toc-md-1").is_some(),
            "再次展开父项后 h2 条目应恢复"
        );

        // 右缘拖拽调宽：start → update → end，宽度按位移更新，且关闭自适应
        let w0 = cx.update(|_w, cx| app.read(cx).sidebar.width);
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                app.start_sidebar_resize(300.0, cx);
                app.update_sidebar_resize(320.0, cx);
                app.end_sidebar_resize(cx);
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (w1, auto) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.sidebar.width, a.sidebar.width_auto)
        });
        assert!(
            (w1 - w0 - 20.0).abs() < 1.0,
            "拖拽后侧边栏宽应按位移更新，w0={w0:.1} w1={w1:.1}"
        );
        assert!(!auto, "手动拖拽后应关闭宽度自适应");

        // 「目录」按钮开关逻辑：再点一次只收起目录面板，侧边栏（活动栏）保留为窄条
        assert!(
            cx.update(|_w, cx| app.read(cx).sidebar.open),
            "打开文件后侧边栏应处于展开"
        );
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx);
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (open, panel) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.sidebar.open, a.sidebar.panel_open)
        });
        assert!(open && !panel, "再点「目录」应收起目录面板、侧边栏仍展开");
        assert!(
            cx.debug_bounds("sidebar-toc").is_none(),
            "再次点「目录」后目录内容应关闭"
        );
        // 侧边栏仍可见，但收窄为活动栏（46px），不留空白宽区
        let slim = cx
            .debug_bounds("sidebar")
            .expect("目录面板收起后侧边栏（活动栏）应仍在");
        assert!(
            (slim.size.width.as_f32() - config::layout::SIDEBAR_ACTIVITY_W).abs() < 2.0,
            "面板收起后侧边栏应只剩活动栏宽，实际={slim:?}"
        );
        assert!(
            cx.debug_bounds("sidebar-activity-bar").is_some(),
            "活动栏在面板收起后应仍可见"
        );
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx);
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-toc").is_some(),
            "再次点「目录」应重新展开目录面板"
        );

        // 回归：再次拖窄目录后，条目不得溢出侧边栏右缘（否则其 hover/点击会延伸到编辑器区）
        let wnow = cx.update(|_w, cx| app.read(cx).sidebar.width);
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                app.start_sidebar_resize(wnow + 50.0, cx);
                app.update_sidebar_resize(wnow - 60.0, cx);
                app.end_sidebar_resize(cx);
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let sb = cx
            .debug_bounds("sidebar")
            .expect("拖窄后侧边栏应仍存在");
        let row = cx
            .debug_bounds("sidebar-toc-md-0")
            .expect("目录条目应仍存在");
        let sb_right = sb.origin.x.as_f32() + sb.size.width.as_f32();
        let row_right = row.origin.x.as_f32() + row.size.width.as_f32();
        assert!(
            row_right <= sb_right + 1.0,
            "拖窄后目录条目应随面板裁剪、不溢出右缘：row_right={row_right:.0} sb_right={sb_right:.0}"
        );

        let _ = std::fs::remove_file(&dir);
    }

    /// 回归：侧边栏搜索的「行内容」只搜当前活动文档——在 A 文档激活时，
    /// 输入只出现在 B 文档里的关键字，不应冒出 B 的内容行（旧逻辑扫所有打开
    /// 选项卡的 content，在 PDF 预览下会误显示 EPUB 正文）。
    #[gpui::test]
    async fn sidebar_search_scoped_to_active_doc(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        let a = std::env::temp_dir().join("gpui-sz-a.md");
        let b = std::env::temp_dir().join("gpui-sz-b.md");
        std::fs::write(&a, "# 甲\n阿尔法内容仅此一处\n").unwrap();
        std::fs::write(&b, "# 乙\nshanghai 明珠塔夜景\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(a.to_str().unwrap(), window, cx));
            app.update(cx, |app, cx| app.open_file(b.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 第 0 个选项卡是初始空「未命名」，A=1、B=2。激活 A，切到搜索视图，
        // 键入只存在于 B 的关键字
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.activate_tab(1, window, cx));
            app.update(cx, |app, cx| {
                app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx);
            });
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| s.set_value("shanghai", window, cx));
        });
        // 防抖期满提交查询（否则渲染不触发扫描）
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        // 活动文档 A 不含 "shanghai" → 不应出现任何行结果（B 的行不得冒出来）
        assert!(
            cx.debug_bounds("sidebar-search-line-2-1").is_none()
                && cx.debug_bounds("sidebar-search-line-1-0").is_none()
                && cx.debug_bounds("sidebar-search-line-0-0").is_none(),
            "激活 A 时搜索 B 的关键字不应返回内容行"
        );
        // 却不影响文件名匹配（文件名含关键字的文件仍可选中）；两文件名均不含关键字，跳过

        // 切到 B 再搜同一关键字 → 应出现 B 的内容行
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.activate_tab(2, window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("sidebar-search-line-2-1").is_some(),
            "激活 B 后搜索同一关键字应出现 B 的内容行"
        );
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// 验证侧边栏搜索的「扫描结果缓存」与「输入防抖」真实生效（优化回归，防负优化）：
    /// - 查询词/活动文档未变时，反复渲染帧**不重扫**（`search_scan_count` 不增）；
    /// - 防抖窗内（< SEARCH_DEBOUNCE_MS）连续输入**零扫描**，防抖期满才提交生效并扫描一次；
    /// - 查询词变化（防抖后）才重扫（计数 +1）；
    /// - 内容命中（当前活动文档行）能出结果行。
    #[gpui::test]
    async fn sidebar_search_scan_is_cached_across_renders(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        let f = std::env::temp_dir().join("gpui-sz-cache.md");
        std::fs::write(&f, "# 甲\n阿尔法 shanghai 内容\n真义行 beta\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let ti = cx.update(|_w, cx| app.read(cx).model.borrow().active_tab);

        // 打开搜索面板（查询为空）：空查询不做扫描
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let before = cx.update(|_w, cx| app.read(cx).model.borrow().search_scan_count);
        assert_eq!(before, 0, "查询为空时不应扫描");

        // 第一次键入「shanghai」：防抖期内（<150ms）不扫描、不提交
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| s.set_value("shanghai", window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let debounced = cx.update(|_w, cx| app.read(cx).model.borrow().search_scan_count);
        assert_eq!(debounced, before, "防抖期内不应执行扫描");

        // 时钟推进越过防抖 → 提交并扫描 1 次，命中第 1 行
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        let after = cx.update(|_w, cx| app.read(cx).model.borrow().search_scan_count);
        assert_eq!(after, before + 1, "防抖期满后应精确扫描 1 次");
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().search_committed.clone()),
            "shanghai",
            "防抖提交词应等于最终输入（归一化）"
        );
        let hit_row1 = Box::leak(format!("sidebar-search-line-{ti}-1").into_boxed_str());
        assert!(
            cx.debug_bounds(hit_row1).is_some(),
            "应命中第 1 行（shanghai），ti={ti}"
        );

        // 同一查询反复渲染（模拟逐帧渲染）→ 缓存命中，不重扫
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        let steady = cx.update(|_w, cx| app.read(cx).model.borrow().search_scan_count);
        assert_eq!(steady, after, "查询未变时重复渲染不应重扫");

        // 换查询「beta」→ 防抖后扫描一次，命中第 2 行
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| s.set_value("beta", window, cx));
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        let changed = cx.update(|_w, cx| app.read(cx).model.borrow().search_scan_count);
        assert_eq!(changed, steady + 1, "查询变化应再扫描一次");
        let hit_row2 = Box::leak(format!("sidebar-search-line-{ti}-2").into_boxed_str());
        assert!(
            cx.debug_bounds(hit_row2).is_some(),
            "换查询 beta 应命中第 2 行"
        );
        let _ = std::fs::remove_file(&f);
    }

    /// 回归：快速连续键入（每次间隔 < 防抖窗）时**只扫描一次**——N 键不再 N 次
    /// O(全文) 同步重扫（大文档卡顿根因），且结果显示的是最终提交词。
    #[gpui::test]
    async fn sidebar_search_debounce_collapses_rapid_typing(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        let txt = std::env::temp_dir().join("gpui-sz-debounce.txt");
        std::fs::write(&txt, "第一行 alpha\n预览 beta\n预览编辑 gamma\ndelta\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(txt.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        // 快速键入 6 个中间词（不给防抖窗到期机会）：期间应零扫描
        for q in ["p", "pr", "pre", "prev", "previ", "预览编辑"] {
            cx.update(|window, cx| {
                let input = app.read(cx).search_state(cx);
                input.update(cx, |s, cx| s.set_value(q, window, cx));
            });
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(20));
            for _ in 0..2 {
                cx.run_until_parked();
            }
        }
        let during = cx.update(|_w, cx| app.read(cx).model.borrow().search_scan_count);
        assert_eq!(during, 0, "防抖窗外仅 20ms/键 → 全程零扫描");

        // 最后停顿超过防抖 → 只提交最终词，恰好扫描 1 次
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        let committed = cx.update(|_w, cx| app.read(cx).model.borrow().search_committed.clone());
        assert_eq!(committed, "预览编辑", "防抖应提交最终输入词");
        let scans = cx.update(|_w, cx| app.read(cx).model.borrow().search_scan_count);
        assert_eq!(scans, 1, "6 键快速输入应只扫描 1 次（最终词）");
        let _ = std::fs::remove_file(&txt);
    }

    /// 复现（shell 层整链）：侧边栏搜索框中文 IME 乱码——把 Windows 风格的 IME 事件
    /// 序列（组合更新 `replace_and_mark(None,…)` + 提交 `replace_text_in_range(None,…)`，
    /// 见 gpui_windows `handle_ime_composition_inner`）喂进 `search_state`，输入框
    /// 文本与防抖提交词都必须是干净中文「编辑」，而非拼音历史拼接（用户报告：
    /// 输「编辑」变「bbibi'abianbian'jbian'ji编辑」）。
    #[gpui::test]
    async fn sidebar_search_ime_composition_commits_clean_query(cx: &mut TestAppContext) {
        use gpui::EntityInputHandler as _;
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        // Windows IME 组合更新序列：每键重发完整组合串（range=None）
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| {
                s.replace_and_mark_text_in_range(None, "b", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bi", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bi'a", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bi'an", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bian'j", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bian'ji", None, window, cx);
            });
        });
        // 确认：提交中文（range=None）
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| {
                s.replace_text_in_range(None, "编辑", window, cx);
            });
        });
        let box_value =
            cx.update(|_w, cx| app.read(cx).search_state(cx).read(cx).value().to_string());
        assert_eq!(box_value, "编辑", "输入框文本应为干净中文（组合替换不追加）");
        // 防抖期满提交 → 扫描查询词同样是干净中文
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        let committed = cx.update(|_w, cx| app.read(cx).model.borrow().search_committed.clone());
        assert_eq!(committed, "编辑", "防抖提交的扫描词应是干净中文而非拼音拼接");
    }

    /// 零选项卡欢迎页（回归 1）：正文区整屏渲染欢迎页——插图（160×120）、主/副文案、
    /// 「打开文件…」与「新建文件」按钮都在，且**没有** `editor-area`（零 tab 不渲染编辑区）；
    /// 点「打开文件…」触发 `open_dialog`（测试为 stub，只计数不真弹窗、不建 tab）。
    #[gpui::test]
    async fn zero_tab_welcome_renders_and_open_button_opens_dialog(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 5);
        assert!(
            cx.debug_bounds("empty-state").is_some(),
            "零选项卡应显示欢迎页"
        );
        assert!(
            cx.debug_bounds("editor-area").is_none(),
            "零选项卡不应渲染编辑区"
        );
        let illus = cx
            .debug_bounds("empty-illustration")
            .expect("应渲染空态插图");
        assert!(
            illus.size.width > px(0.) && illus.size.height > px(0.),
            "插图应有固定尺寸，实际={illus:?}"
        );
        let btn = cx
            .debug_bounds("empty-open-btn")
            .expect("应渲染「打开文件…」按钮");
        assert!(
            btn.size.width > px(0.) && btn.size.height > px(0.),
            "「打开文件…」按钮应有尺寸，实际={btn:?}"
        );
        let newbtn = cx
            .debug_bounds("empty-new-btn")
            .expect("应渲染「新建文件」按钮");
        assert!(
            newbtn.size.width > px(0.) && newbtn.size.height > px(0.),
            "「新建文件」按钮应有尺寸，实际={newbtn:?}"
        );
        // 点击按钮 → 触发 open_dialog（计数断言；测试 stub 不弹原生对话框）
        cx.simulate_click(btn.center(), Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let calls = cx.update(|_w, cx| app.read(cx).open_dialog_calls);
        assert_eq!(calls, 1, "点击「打开文件…」应触发一次 open_dialog");
        assert!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.is_empty()),
            "点「打开文件…」只弹系统对话框、不应新建选项卡"
        );
    }

    /// 向窗口派发一次**真实双击**（mousedown `click_count=2` + mouseup `click_count=2`）。
    /// 走 `simulate_event` 的完整分发路径（`window.dispatch_event`），双击识别与编辑器
    /// 同套语义（`MouseDownEvent.click_count >= 2`）；`simulate_click` 恒为单击，无法覆盖。
    fn simulate_double_click(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>) {
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position: at,
            modifiers: gpui::Modifiers::default(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position: at,
            modifiers: gpui::Modifiers::default(),
            click_count: 2,
        });
    }

    /// 渲染若干帧（等动画/重绘收敛后再读 debug_bounds）。
    fn settle_frames(cx: &mut VisualTestContext, n: usize) {
        for _ in 0..n {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
    }

    /// 打开零选项卡时欢迎页的焦点句柄回归：零 tab 下 `editor_focus_handle` 回退到
    /// 应用根焦点（不 Panic、可聚焦），保证 Ctrl+N 等在欢迎页可正常派发。
    fn focus_app_root(cx: &mut VisualTestContext, app: &Entity<EditorApp>) {
        cx.update(|window, cx| {
            let h = app.read(cx).editor_focus_handle(cx);
            window.focus(&h, cx);
        });
    }

    /// 回归 2：点欢迎页「新建文件」→ 出现 1 个选项卡、欢迎页消失、编辑区渲染且可聚焦、
    /// 真实键入生效。
    #[gpui::test]
    async fn new_file_button_creates_editable_tab(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 5);
        assert!(
            cx.debug_bounds("empty-state").is_some(),
            "初始应显示欢迎页"
        );
        // 点「新建文件」按钮（用其外包定位元素，真实点击路径）。
        let btn = cx
            .debug_bounds("empty-new-btn")
            .expect("应渲染「新建文件」按钮");
        cx.simulate_click(btn.center(), Default::default());
        settle_frames(cx, 5);
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            1,
            "点「新建文件」应新建 1 个选项卡"
        );
        assert!(
            cx.debug_bounds("empty-state").is_none(),
            "欢迎页应消失（有 tab 即不再显示）"
        );
        assert!(
            cx.debug_bounds("editor-area").is_some(),
            "应渲染编辑区"
        );
        // 焦点应落在新编辑区（开箱即输入）。
        let focused = cx.update(|window, cx| {
            app.read(cx).model.borrow().tabs[0].payload.editor.read(cx).focus_handle().is_focused(window)
        });
        assert!(focused, "新建后编辑区应获得焦点");
        // 直接键入（真实键入路径）→ 内容写入且欢迎页不回来。
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[0].payload.editor.clone());
        cx.update(|_, cx| editor.update(cx, |ed, cx| ed.insert_text("hi", cx)));
        settle_frames(cx, 3);
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs[0].payload.editor.read(cx).value().to_string()),
            "hi",
            "新建选项卡应可直接键入"
        );
        assert!(
            cx.debug_bounds("empty-state").is_none(),
            "输入后欢迎页绝不出现（模型 3）"
        );
    }

    /// 回归 3：**双击**欢迎页（空白处，避开按钮）＝新建文件选项卡——与「新建文件」
    /// 按钮同语义（出现 tab、欢迎页消失、编辑区渲染）。
    #[gpui::test]
    async fn double_click_welcome_creates_tab(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 5);
        assert!(cx.debug_bounds("empty-state").is_some(), "初始应显示欢迎页");
        // 双击按钮上方的空白处（按钮行下面约在正文区中央；选偏上空白区避开按钮）
        simulate_double_click(cx, gpui::Point::new(px(400.), px(200.)));
        settle_frames(cx, 5);
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            1,
            "双击欢迎页应新建 1 个选项卡"
        );
        assert!(cx.debug_bounds("empty-state").is_none(), "欢迎页应消失");
        assert!(cx.debug_bounds("editor-area").is_some(), "应渲染编辑区");
    }

    /// 回归 4：关闭**全部**选项卡 → 回到欢迎页（零 tab、无编辑区、不 panic、不退出）。
    #[gpui::test]
    async fn closing_last_tab_returns_to_welcome(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx); // add_app 预建 1 个空未命名选项卡
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 5);
        assert!(cx.debug_bounds("editor-area").is_some(), "有 tab 时应渲染编辑区");
        assert!(cx.debug_bounds("empty-state").is_none(), "有 tab 时绝不显示欢迎页");
        // 关闭空未命名选项卡 → 一律弹确认（新口径：未命名无论脏与否都问）；
        // 选「不保存」→ 移除（回到欢迎页）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.close_tab(0, window, cx));
        });
        assert!(cx.has_pending_prompt(), "空未命名关闭应弹确认");
        cx.simulate_prompt_answer("不保存");
        settle_frames(cx, 5);
        assert!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.is_empty()),
            "关闭最后一个选项卡后应零 tab"
        );
        assert!(cx.debug_bounds("empty-state").is_some(), "应回到欢迎页");
        assert!(cx.debug_bounds("editor-area").is_none(), "零 tab 无编辑区");
    }

    /// 回归 5：**只要有 ≥1 个选项卡**（含新建出的空未命名文档）就绝不显示欢迎页——
    /// 空未命名 tab 直接是编辑区（可键入），无第二层双击步骤。
    #[gpui::test]
    async fn any_tab_suppresses_welcome(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx); // 1 个空未命名选项卡
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 5);
        assert_eq!(cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()), 1, "预建 1 个空未命名选项卡");
        assert!(cx.debug_bounds("editor-area").is_some(), "空未命名 tab 也直接渲染编辑区");
        assert!(cx.debug_bounds("empty-state").is_none(), "有 tab（哪怕空未命名）就不显示欢迎页");
        // 键入后依旧不显示欢迎页（新模型没有「欢迎页盖在有内容上」的路径）。
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[0].payload.editor.clone());
        cx.update(|_, cx| editor.update(cx, |ed, cx| ed.insert_text("x", cx)));
        settle_frames(cx, 3);
        assert!(cx.debug_bounds("empty-state").is_none(), "输入后仍不显示欢迎页");
    }

    /// 回归 6：Ctrl+N（`NewFile` 动作）在欢迎页（零 tab、焦点在应用根）可用——
    /// 新建选项卡、欢迎页消失、编辑区渲染。
    #[gpui::test]
    async fn ctrl_n_works_from_welcome(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        settle_frames(cx, 5);
        assert!(cx.debug_bounds("empty-state").is_some(), "初始应显示欢迎页");
        // 零 tab 下焦点回退到应用根（`editor_focus_handle` → 根句柄）
        focus_app_root(cx, &app);
        cx.run_until_parked();
        cx.dispatch_action(NewFile);
        settle_frames(cx, 5);
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            1,
            "Ctrl+N 应从欢迎页新建选项卡"
        );
        assert!(cx.debug_bounds("empty-state").is_none(), "欢迎页应消失");
        assert!(cx.debug_bounds("editor-area").is_some(), "应渲染编辑区");
    }


    /// 目录空态：打开无任何标题的 Markdown（有内容但无目录节点）→ 侧栏目录面板
    /// 显示「灰色图标 + 文案」空态占位（跨无标题文档 / PDF 无书签 / 无文档三态共用）。
    #[gpui::test]
    async fn sidebar_toc_empty_state_placeholder(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md = std::env::temp_dir().join(format!("gpui-toc-empty-{}.md", std::process::id()));
        std::fs::write(&md, "纯文本，没有标题\n第二行\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-toc").is_some(),
            "应渲染目录面板"
        );
        assert!(
            cx.debug_bounds("sidebar-toc-empty").is_some(),
            "无标题文档应显示目录空态占位"
        );
        let _ = std::fs::remove_file(&md);
    }

    /// 搜索空态：打开有内容的文档、输入无匹配关键字 → 侧栏搜索显示
    /// 「灰色搜索图标 + 文案」空态占位；命中时恢复正常结果列表。
    #[gpui::test]
    async fn sidebar_search_empty_state_placeholder(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let txt = std::env::temp_dir().join(format!("gpui-srch-empty-{}.txt", std::process::id()));
        std::fs::write(&txt, "hello world\nthis is a file\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(txt.to_str().unwrap(), window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        // 无匹配 → 空态占位
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| s.set_value("zzz-无匹配", window, cx));
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-search-empty").is_some(),
            "无匹配应显示搜索空态占位"
        );
        // 命中 → 恢复正常结果列表（不再有空态）
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| s.set_value("hello", window, cx));
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-search").is_some(),
            "命中搜索仍应显示结果面板"
        );
        let _ = std::fs::remove_file(&txt);
    }

    /// 回归：侧边栏搜索「非 ASCII 查询」的零分配重写语义与旧「collect + to_lowercase
    /// 整串比较」**逐例一致**（防抖用了防重扫、但扫描命中语义必须不变）。对一组含
    /// 中文/混合大小写/Unicode 折叠边界的语料，逐 (haystack, needle) 比对命中区间。
    #[test]
    fn find_ci_non_ascii_semantics_match_old() {
        use shell_scan::find_ci;
        fn find_ci_old(haystack: &str, needle: &str) -> Option<std::ops::Range<usize>> {
            if needle.is_empty() {
                return None;
            }
            if needle.is_ascii() {
                let h = haystack.as_bytes();
                let n = needle.as_bytes();
                if n.len() > h.len() {
                    return None;
                }
                let first = n[0];
                let first_alt = if first.is_ascii_alphabetic() {
                    first ^ 0x20
                } else {
                    first
                };
                let mut pos = 0usize;
                while let Some(rel) = memchr::memchr2(first, first_alt, &h[pos..]) {
                    let at = pos + rel;
                    if at + n.len() <= h.len() {
                        let mut ok = true;
                        for (j, &nb) in n.iter().enumerate().skip(1) {
                            if h[at + j].to_ascii_lowercase() != nb {
                                ok = false;
                                break;
                            }
                        }
                        if ok {
                            return Some(at..(at + n.len()));
                        }
                    }
                    pos = at + 1;
                }
                return None;
            }
            let needle_chars = needle.chars().count();
            let mut i = 0usize;
            while i < haystack.len() {
                let candidate: String = haystack[i..].chars().take(needle_chars).collect();
                if !candidate.is_empty() && candidate.to_lowercase() == needle {
                    return Some(i..(i + candidate.len()));
                }
                let ch = haystack[i..].chars().next().unwrap();
                i += ch.len_utf8();
            }
            None
        }
        let corpus = [
            "预览编辑测试 内容",
            "先预览再编辑测试",
            "编辑预览编辑",
            "纯中文行：这是预览编辑的示例预览编辑文本",
            "ABC 预览编辑 xyz",
            "空行之后\n预览编辑在第二行",
            "XvVl 大小写 MIXed 预览编辑 MID",
            "不存在的词 䶮 和结尾 𝄞",
            "İstanbul 有折叠展开字符的写法",
            "ẞeta 大写锐 s 折叠为 ß",
            "单字符行 预",
            "预",
            "",
            "长 needle 超出文本：预览编辑测试无此词",
            "尾部命中预览编辑",
        ];
        let needles = [
            "预览编辑",
            "预",
            "览编",
            "编辑",
            "测试",
            "ıstanbul",
            "istanbul",
            "ßeta",
            "beta",
            "vvvl",
            "秀米",
            "𝄞",
            "无此词",
            "alpha",
        ];
        for h in corpus {
            for n in needles {
                let got = find_ci(h, n);
                let want = find_ci_old(h, n);
                assert_eq!(
                    got, want,
                    "find_ci 语义回归：haystack={h:?} needle={n:?}（新 {got:?} vs 旧 {want:?}）"
                );
            }
        }
    }

    /// 基准：侧边栏搜索「旧每帧全扫 vs 新缓存 + 无分配内核」的代价对比。
    /// 旧逻辑：渲染帧每帧 `to_string()` 物化全文 + 每行 `to_lowercase()` 分配 + `contains`；
    /// 新逻辑：查询变化才扫一次（无分配 [`find_ci`]），其余渲染帧走 O(1) 缓存键比较。
    /// 打印、不构成断言；`--ignored` 运行（匹配项目既有 bench 惯例）。
    #[test]
    #[ignore]
    fn bench_sidebar_search_old_vs_new() {
        use shell_scan::find_ci;
        use std::time::Instant;
        let block = "# 章节标题\n\n普通段落内容 alpha beta 关键字 shanghai 你好\n\n```rust\nlet x = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let mut doc = String::with_capacity(block.len() * 12000);
        for _ in 0..12000 {
            doc.push_str(block);
        }
        let q = "shanghai";
        let lines: Vec<&str> = doc.lines().collect();
        let n = lines.len();

        // 旧内核：每行 to_lowercase() 分配后 contains（模拟每个渲染帧全量重扫）
        let t0 = Instant::now();
        let mut old_hits = 0usize;
        for _ in 0..100 {
            old_hits = lines.iter().filter(|l| l.to_lowercase().contains(q)).count();
        }
        let old = t0.elapsed();

        // 新内核：无分配 find_ci（同样每帧扫，展示内核本身提速）
        let t1 = Instant::now();
        let mut new_hits = 0usize;
        for _ in 0..100 {
            new_hits = lines.iter().filter(|l| find_ci(l, q).is_some()).count();
        }
        let new_kernel = t1.elapsed();
        assert_eq!(old_hits, new_hits, "新旧命中数必须一致");

        // 新路径整体：100 帧 = 1 次真实扫描 + 99 次 O(1) 缓存键比较
        let t2 = Instant::now();
        let mut key = String::new();
        for i in 0..100 {
            if key != q {
                key = q.to_string();
            }
            let _ = i;
        }
        let cached_path = t2.elapsed();

        println!(
            "侧边栏搜索（{n} 行）：100 帧旧内核={}ms, 新内核={}ms（内核提升 {:.1}x）, 新缓存路径(1 扫+99 次键比较)={}ms",
            old.as_millis(),
            new_kernel.as_millis(),
            old.as_secs_f64() / new_kernel.as_secs_f64().max(1e-9),
            cached_path.as_millis(),
        );
    }

    /// 回归：侧边栏「目录 / 搜索结果」面板的可见纵向滚动条——超长内容下面板渲染
    /// 自绘滚动条叠层（`sidebar-toc-vscrollbar` / `sidebar-search-vscrollbar`，
    /// 叠在滚动容器上、尺寸非零），且目录虚拟化不被滚动条破坏（带外条目仍不物化）。
    #[gpui::test]
    async fn sidebar_toc_and_search_show_vscrollbars(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join(format!("gpui-sidebar-vs-{}.md", std::process::id()));
        let mut doc = String::new();
        for i in 0..600 {
            doc.push_str(&format!("# 第{i}章\n## 小节{i}\n正文段落 {i}。\n\n"));
        }
        std::fs::write(&p, &doc).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }

        // 目录面板：滚动条叠层出现且尺寸非零；虚拟化依旧（第 1000 个条目不带外物化）
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Toc, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let toc_bar = cx
            .debug_bounds("sidebar-toc-vscrollbar")
            .expect("目录超量应显示滚动条叠层 sidebar-toc-vscrollbar");
        assert!(
            toc_bar.size.height > px(0.),
            "目录滚动条叠层应有非零高度，实际={toc_bar:?}"
        );
        assert!(
            cx.debug_bounds("sidebar-toc-scroll").is_some(),
            "目录滚动容器仍应渲染"
        );
        assert!(
            cx.debug_bounds("sidebar-toc-md-1000").is_none(),
            "虚拟化不受滚动条影响：带外条目不应物化"
        );

        // 搜索结果面板：命中多行大到可滚 → 滚动条叠层出现、滚动容器被跟踪仍渲染
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx),
            );
        });
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| s.set_value("正文", window, cx));
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-search-results").is_some(),
            "搜索结果滚动容器应渲染"
        );
        let search_bar = cx
            .debug_bounds("sidebar-search-vscrollbar")
            .expect("搜索结果超量应显示滚动条叠层 sidebar-search-vscrollbar");
        assert!(
            search_bar.size.height > px(0.),
            "搜索滚动条叠层应有非零高度，实际={search_bar:?}"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 文件夹级搜索测试辅助：泵后台扫描直到完成并返回最终 `FolderSearchScan` 克隆。
    /// （轮询上限内未完成 → panic，避免死循环挂死测试。）
    fn fs_wait_scan(
        app: &gpui::Entity<EditorApp>,
        cx: &mut VisualTestContext,
        expect_min_hits: usize,
    ) -> crate::app::sidebar::FolderSearchScan {
        let mut last: Option<crate::app::sidebar::FolderSearchScan> = None;
        for _ in 0..600 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let snap = cx.update(|_w, cx| app.read(cx).model.borrow().folder_search.clone());
            if let Some(s) = &snap {
                last = Some((**s).clone());
                if !s.running && s.total_hits >= expect_min_hits {
                    return (**s).clone();
                }
            }
        }
        panic!(
            "文件夹扫描未在轮询上限内完成：last={:?}",
            last.as_ref()
                .map(|s| (s.root.clone(), s.total_hits, s.running))
        );
    }

    /// 测试隔离：开发者真实 `%APPDATA%\vuick\config.json` 可能带「已打开文件夹」
    /// 且 `config` crate 未做 `cfg(test)` 门控（AGENTS K8）——会使「默认整个文件夹」
    /// 意外生效。搜索侧测试前置清空根，保证在「未打开文件夹」环境下运行。
    fn fs_clear_restored_folder(app: &gpui::Entity<EditorApp>, cx: &mut VisualTestContext) {
        cx.update(|_w, cx| {
            app.update(cx, |a, _cx| {
                a.sidebar.files.set_root(None);
            });
        });
    }

    /// 文件夹级搜索：范围 tab 默认规则——未打开文件夹时默认「当前文件」且「整个文件夹」
    /// 不可用（点击仅提示）；打开文件夹后**默认选中「整个文件夹」**；手动点「当前文件」
    /// 后保持手动选择。
    #[gpui::test]
    async fn sidebar_search_folder_scope_default_and_tabs(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        // 打开搜索面板（无文件夹）→ 两个范围 tab 都在，生效范围 = 当前文件
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-search-scope").is_some(),
            "范围切换 tab 条应渲染"
        );
        assert!(
            cx.debug_bounds("sidebar-search-scope-current").is_some()
                && cx.debug_bounds("sidebar-search-scope-folder").is_some(),
            "「当前文件」「整个文件夹」两个 tab 都应渲染"
        );
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).effective_search_scope()),
            crate::app::sidebar::SearchScopeTab::CurrentFile,
            "无文件夹时默认生效范围 = 当前文件"
        );
        // 点击「整个文件夹」（无文件夹）→ 仅提示，不切换
        let folder_tab = cx
            .debug_bounds("sidebar-search-scope-folder")
            .expect("应渲染「整个文件夹」tab");
        cx.simulate_click(folder_tab.center(), Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).effective_search_scope()),
            crate::app::sidebar::SearchScopeTab::CurrentFile,
            "无文件夹时点「整个文件夹」不应切换范围（保持只搜当前文件）"
        );
        let hint = cx.update(|_w, cx| app.read(cx).model.borrow().status.clone());
        assert!(
            hint.contains("先打开文件夹"),
            "无文件夹点「整个文件夹」应提示先打开文件夹，实际={hint}"
        );

        // 打开文件夹 → 生效范围自动 = 整个文件夹
        let dir = std::env::temp_dir().join(format!("gpui-fs-scope-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "needle one\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(dir.clone(), window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).effective_search_scope()),
            crate::app::sidebar::SearchScopeTab::Folder,
            "已打开文件夹时默认生效范围 = 整个文件夹"
        );
        // 手动点「当前文件」→ 手动选择被记录（不再自动跟随）
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx),
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let current_tab = cx
            .debug_bounds("sidebar-search-scope-current")
            .expect("应渲染「当前文件」tab");
        cx.simulate_click(current_tab.center(), Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).effective_search_scope()),
            crate::app::sidebar::SearchScopeTab::CurrentFile,
            "手动点「当前文件」后应保持当前文件（manual=true）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件夹级搜索：递归跨文件命中**按文件分组** + 词频计数 + 跳过规则
    /// （.git / target / 二进制 NUL 文件不出现在结果）。
    #[gpui::test]
    async fn sidebar_search_folder_groups_results_and_skips(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!("gpui-fs-grp-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("a.txt"), "alpha needle 一处\n").unwrap();
        std::fs::write(dir.join("b.md"), "beta needle one\ngamma needle two\n").unwrap();
        std::fs::write(dir.join("sub/c.txt"), "sub needle 深处\n").unwrap();
        std::fs::write(dir.join(".git/config"), "needle 不应命中（git）\n").unwrap();
        std::fs::write(dir.join("target/out.txt"), "needle 不应命中（target）\n").unwrap();
        std::fs::write(
            dir.join("bin.dat"),
            vec![b'n', b'e', b'e', b'd', b'l', b'e', 0u8, b'g', b'o'],
        )
        .unwrap();

        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(dir.clone(), window, cx));
        });
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx),
            );
        });
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| s.set_value("needle", window, cx));
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let scan = fs_wait_scan(&app, cx, 4);
        assert!(!scan.running, "扫描应已完成");
        assert_eq!(scan.total_hits, 4, "应命中 4 处（a/b/sub，两类跳过）");
        assert_eq!(scan.files.len(), 3, "应命中 3 个文件（按文件分组）");
        let names: Vec<String> = scan
            .files
            .iter()
            .map(|f| {
                f.path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        assert!(
            names.iter().any(|n| n == "a.txt") && names.iter().any(|n| n == "b.md")
                && names.iter().any(|n| n == "c.txt"),
            "应命中 a.txt / b.md / sub/c.txt，实际={names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "config" || n == "out.txt" || n == "bin.dat"),
            ".git/target/二进制 均不应出现在结果，实际={names:?}"
        );
        let b_hits = scan
            .files
            .iter()
            .find(|f| f.path.file_name().is_some_and(|s| s == "b.md"))
            .map(|f| f.hits.len())
            .unwrap();
        assert_eq!(b_hits, 2, "b.md 词频应 = 2（两行各 1 处）");
        // 渲染侧：文件头行 + 命中行都真实渲染
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-folder-file-0").is_some(),
            "首个文件分组头应渲染"
        );
        assert!(
            cx.debug_bounds("sidebar-search-folder-hit-0-0").is_some(),
            "首个命中行应渲染"
        );
        assert!(
            cx.debug_bounds("sidebar-folder-search-summary").is_some(),
            "文件夹搜索汇总头应渲染"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件夹级搜索：点击命中行 → 未打开则新建选项卡打开 + 光标定位到命中行；
    /// 再点同一命中 → 激活既有选项卡（不新建）且光标仍在命中行。
    #[gpui::test]
    async fn sidebar_search_folder_click_jumps_to_line(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!("gpui-fs-jump-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("jump.txt");
        std::fs::write(&target, "第一行 alpha\n第二行 beta\n第三行 needle 目标\n第四行\n")
            .unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(dir.clone(), window, cx));
        });
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx),
            );
        });
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| s.set_value("needle", window, cx));
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        fs_wait_scan(&app, cx, 1);
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let tabs_before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        let hit = cx
            .debug_bounds("sidebar-search-folder-hit-0-0")
            .expect("应渲染文件夹命中行");
        cx.simulate_click(hit.center(), Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (tabs_after, ti, pos) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let ti = a.model.borrow().active_tab;
            let pos = a.model.borrow().tabs[ti].payload.editor.read(cx).cursor_position();
            (a.model.borrow().tabs.len(), ti, pos)
        });
        assert_eq!(
            tabs_after,
            tabs_before + 1,
            "点击未打开文件应新建 1 个选项卡"
        );
        let opened = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[ti].payload.file_path.as_deref() == Some(target.as_path())
        });
        assert!(opened, "点击应打开 jump.txt 并激活");
        assert_eq!(pos.0, 2, "光标应落在命中行（0 基第 2 行）");
        // 再点同一命中行：不新建选项卡，激活既有并仍定位
        let tabs_before2 = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        let hit = cx
            .debug_bounds("sidebar-search-folder-hit-0-0")
            .expect("命中行仍在");
        cx.simulate_click(hit.center(), Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (tabs_after2, row2) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let ti = a.model.borrow().active_tab;
            let pos = a.model.borrow().tabs[ti].payload.editor.read(cx).cursor_position();
            (a.model.borrow().tabs.len(), pos.0)
        });
        assert_eq!(tabs_after2, tabs_before2, "再次点击已打开文件不应新建选项卡");
        assert_eq!(row2, 2, "再次点击后光标仍应在命中行");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件夹级搜索：命中条数达 [`config::limits::FOLDER_SEARCH_RESULT_CAP`]（500）
    /// 即截断并标记 truncated（不无限累积）。
    #[gpui::test]
    async fn sidebar_search_folder_truncates_at_cap(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!("gpui-fs-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 80 文件 × 每文件 7 行命中 = 560 > 500 → 必截断
        for i in 0..80 {
            let mut body = String::new();
            for j in 0..7 {
                body.push_str(&format!("第{j}行 needle {i}\n"));
            }
            std::fs::write(dir.join(format!("f{i:03}.txt")), &body).unwrap();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(dir.clone(), window, cx));
        });
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx),
            );
        });
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| s.set_value("needle", window, cx));
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let scan = fs_wait_scan(&app, cx, 500);
        assert!(scan.truncated, "达上限应标记截断");
        assert_eq!(
            scan.total_hits,
            config::limits::FOLDER_SEARCH_RESULT_CAP,
            "截断时总命中数应恰为上限 500"
        );
        assert!(
            scan.files.len() <= config::limits::FOLDER_SEARCH_RESULT_CAP,
            "分组行数不应超过上限"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件夹级搜索：无命中 → 空态；清空查询 → 回到输入提示；切换文件夹 → 旧结果
    /// 失效、按新根重扫；关闭文件夹（根失效）→ 「先打开文件夹」提示。
    #[gpui::test]
    async fn sidebar_search_folder_empty_clear_and_invalidate(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        let dir_a = std::env::temp_dir().join(format!("gpui-fs-inv-a-{}", std::process::id()));
        let dir_b = std::env::temp_dir().join(format!("gpui-fs-inv-b-{}", std::process::id()));
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::write(dir_a.join("hit.txt"), "alpha needle only\n").unwrap();
        std::fs::write(dir_b.join("other.txt"), "无命中内容\n").unwrap();

        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(dir_a.clone(), window, cx));
        });
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx),
            );
        });
        // 搜索「needle」→ A 有命中
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| s.set_value("needle", window, cx));
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let scan_a = fs_wait_scan(&app, cx, 1);
        assert_eq!(scan_a.total_hits, 1, "A 应有 1 处命中");
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-search-folder-hit-0-0").is_some(),
            "A 的命中行应渲染"
        );

        // 清空查询（重扫空 → 不显示结果，回到输入提示）
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| s.set_value("", window, cx));
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("sidebar-search-hint").is_some(),
            "清空查询应回到输入提示（空态）"
        );
        assert!(
            cx.debug_bounds("sidebar-search-folder-hit-0-0").is_none(),
            "清空查询后不应渲染旧命中行"
        );

        // 切到 B 文件夹 → 旧结果失效，按新根重扫（无命中 → 空态）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(dir_b.clone(), window, cx));
        });
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Search, cx),
            );
        });
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| s.set_value("needle", window, cx));
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let scan_b = fs_wait_scan(&app, cx, 0);
        assert_eq!(
            scan_b.root.as_deref(),
            Some(dir_b.as_path()),
            "换根后应按新根重扫"
        );
        assert_eq!(scan_b.total_hits, 0, "B 无命中");
        assert!(
            cx.debug_bounds("sidebar-search-empty").is_some(),
            "B 无命中应显示空态"
        );

        // 先手动点到「整个文件夹」（B 在 → 可用），再关闭文件夹（根失效 → None）
        // → 范围仍手动是文件夹 → 显示「先打开文件夹」提示且不搜当前文件。
        let folder_tab = cx
            .debug_bounds("sidebar-search-scope-folder")
            .expect("应渲染「整个文件夹」tab");
        cx.simulate_click(folder_tab.center(), Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).effective_search_scope()),
            crate::app::sidebar::SearchScopeTab::Folder,
            "点「整个文件夹」后范围应切到文件夹"
        );
        cx.update(|window, cx| {
            app.update(
                cx,
                |app, cx| app.open_folder_path(dir_a.join("不存在").to_path_buf(), window, cx),
            );
        });
        // 无效路径不切视图（仍停在搜索面板）：直接渲染，无需再 toggle（重复 toggle 会收起面板）。
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).sidebar.files.root.clone()),
            None,
            "失效路径应清空文件夹根"
        );
        assert!(
            cx.debug_bounds("sidebar-search-empty").is_some(),
            "无文件夹 + 文件夹范围应显示「先打开文件夹」空态"
        );
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    // -----------------------------------------------------------------
    // WP-C：文件树增强（过滤 / 右键菜单 / 新建 / 重命名 / 删除）
    // -----------------------------------------------------------------

    /// 泵文件树布局缓存就位（过滤词提交后重收集完成 + 无在途子项扫描）。
    fn ft_wait_layout(app: &gpui::Entity<EditorApp>, cx: &mut VisualTestContext) {
        for _ in 0..120 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let ready = cx.update(|_w, cx| {
                let a = app.read(cx);
                a.sidebar.files.pending.is_empty() && a.sidebar.files_layout.is_some()
            });
            if ready {
                return;
            }
        }
        panic!("文件树布局缓存未在轮询上限内就位");
    }

    /// `file_tree_invalidate` 后的树刷新等待：父目录重扫完成且出现名字 `name` 的子项。
    fn ft_wait_child(
        app: &gpui::Entity<EditorApp>,
        cx: &mut VisualTestContext,
        dir: &std::path::Path,
        name: &str,
    ) {
        for _ in 0..120 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let found = cx.update(|_w, cx| {
                app.read(cx)
                    .sidebar
                    .files
                    .children
                    .get(dir)
                    .is_some_and(|cs| cs.iter().any(|c| c.name == name))
            });
            if found {
                return;
            }
        }
        panic!("树刷新后未出现子项 {name}");
    }

    /// 打开文件夹 → 文件视图就绪（行序：root=0，目录优先、组内名字排序）。
    fn ft_open_folder(
        app: &gpui::Entity<EditorApp>,
        cx: &mut VisualTestContext,
        dir: &std::path::Path,
    ) {
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Files, cx);
            });
            app.update(cx, |app, cx| app.open_folder_path(dir.to_path_buf(), window, cx));
        });
        ft_wait_tree_loaded(app, cx, dir);
        ft_wait_layout(app, cx);
    }

    /// 文件树过滤：typeahead 按名字（不区分大小写子串）过滤含目录、清空恢复全树。
    #[gpui::test]
    async fn sidebar_file_filter_typeahead_and_clear(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        let dir = std::env::temp_dir().join(format!("gpui-ft-filter-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        std::fs::write(dir.join("alpha.txt"), "A").unwrap();
        std::fs::write(dir.join("beta.md"), "B").unwrap();
        std::fs::write(dir.join("subdir/gamma.cpp"), "C").unwrap();
        ft_open_folder(&app, cx, &dir);

        let root_name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let labels = |cx: &mut VisualTestContext| {
            cx.update(|_w, cx| {
                app.read(cx)
                    .sidebar
                    .files_layout
                    .as_ref()
                    .expect("布局应已生成")
                    .1
                    .iter()
                    .map(|r| r.label.to_string())
                    .collect::<Vec<_>>()
            })
        };
        let all = labels(cx);
        assert!(all.contains(&"subdir".to_string()) && all.contains(&"alpha.txt".to_string()));
        // 过滤 "be"（Beta 子串，大小写不敏感）→ 只剩 beta.md（根恒在），subdir/alpha 隐藏。
        cx.update(|window, cx| {
            let input = app.read(cx).files_filter_state.clone();
            input.update(cx, |s, cx| s.set_value("be", window, cx));
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(150));
        ft_wait_layout(&app, cx);
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().files_filter.clone()),
            "be",
            "过滤词应防抖提交"
        );
        let filtered = labels(cx);
        assert!(filtered.contains(&root_name), "根行恒可见");
        assert!(filtered.contains(&"beta.md".to_string()));
        assert!(
            !filtered.contains(&"alpha.txt".to_string()) && !filtered.contains(&"subdir".to_string()),
            "不匹配条目（含目录）应隐藏，实际={filtered:?}"
        );
        // 清空 → 恢复全树。
        cx.update(|window, cx| {
            let input = app.read(cx).files_filter_state.clone();
            input.update(cx, |s, cx| s.set_value("", window, cx));
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(150));
        ft_wait_layout(&app, cx);
        let restored = labels(cx);
        assert!(restored.contains(&"alpha.txt".to_string()) && restored.contains(&"subdir".to_string()));
        // 过滤无匹配 → 「无匹配结果」空态文案（仅剩根 + 占位）。
        cx.update(|window, cx| {
            let input = app.read(cx).files_filter_state.clone();
            input.update(cx, |s, cx| s.set_value("zzz_nomatch", window, cx));
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(150));
        ft_wait_layout(&app, cx);
        let none = labels(cx);
        assert!(
            none.contains(&"无匹配结果".to_string()),
            "过滤无匹配应显示空态文案，实际={none:?}"
        );
        assert_eq!(none.len(), 2, "只剩根行 + 无匹配占位，实际={none:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件树右键菜单按适用集区分：文件行 = 重命名/删除（无新建）；目录行 = 新建文件/
    /// 新建文件夹/重命名/删除。右键不误触发行点击（不打开文件）。
    #[gpui::test]
    async fn sidebar_file_right_click_menu_by_kind(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        let dir = std::env::temp_dir().join(format!("gpui-ft-rclick-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        std::fs::write(dir.join("a.txt"), "A").unwrap();
        ft_open_folder(&app, cx, &dir);
        let tabs_before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());

        // 文件行（行序 root=0、subdir=1、a.txt=2）右键 → 只有重命名/删除。
        let file_row = cx.debug_bounds("sidebar-file-row-2").expect("a.txt 行应渲染");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: file_row.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(cx.debug_bounds("file-tree-menu").is_some(), "右键文件树应弹菜单");
        assert!(cx.debug_bounds("file-tree-menu-rename").is_some());
        assert!(cx.debug_bounds("file-tree-menu-delete").is_some());
        assert!(
            cx.debug_bounds("file-tree-menu-new-file").is_none()
                && cx.debug_bounds("file-tree-menu-new-folder").is_none(),
            "文件行不应出现新建类菜单项"
        );
        // 右键不误触发行点击（未打开任何文件 / 未切换选项卡）。
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            tabs_before,
            "右键文件行不应触发行点击打开"
        );
        // Esc 关闭当前菜单。
        cx.simulate_keystrokes("escape");
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 目录行（subdir=1）右键 → 有新建文件/文件夹。
        let dir_row = cx.debug_bounds("sidebar-file-row-1").expect("subdir 行应渲染");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: dir_row.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(cx.debug_bounds("file-tree-menu-new-file").is_some(), "目录行应有「新建文件」");
        assert!(cx.debug_bounds("file-tree-menu-new-folder").is_some(), "目录行应有「新建文件夹」");
        assert!(cx.debug_bounds("file-tree-menu-rename").is_some());
        assert!(cx.debug_bounds("file-tree-menu-delete").is_some());
        cx.simulate_keystrokes("escape");
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 新建文件：右键目录 → 新建文件… → 预填默认名 → 改名回车 → 磁盘创建 + **自动打开
    /// 成选项卡** + 树刷新；重名 → 内联提示不静默；Esc 关闭。
    #[gpui::test]
    async fn sidebar_file_new_file_opens_and_dup_hint(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        let dir = std::env::temp_dir().join(format!("gpui-ft-new-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("existing.txt"), "E").unwrap();
        ft_open_folder(&app, cx, &dir);

        // 右键根目录行（0）→ 新建文件…
        let root_row = cx.debug_bounds("sidebar-file-row-0").expect("根行应渲染");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: root_row.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let new_item = cx.debug_bounds("file-tree-menu-new-file").expect("应有新建文件项");
        cx.simulate_click(new_item.center(), Default::default());
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(cx.debug_bounds("file-tree-prompt").is_some(), "应弹出输入层");
        let pv = cx.update(|_w, cx| {
            let p = app
                .read(cx)
                .file_tree_prompt
                .as_ref()
                .expect("弹层应打开")
                .read(cx);
            (p.input_value(cx), p.kind().clone())
        });
        assert_eq!(pv.0, "新建文件.md", "应预填默认名");
        assert!(
            matches!(pv.1, crate::app::file_tree_prompt::FileTreePromptKind::NewFile { .. }),
            "应为新建文件类型"
        );
        // 改名 hello.txt → 回车提交。
        let target = dir.join("hello.txt");
        let prompt_input = cx.update(|_w, cx| {
            let p = app
                .read(cx)
                .file_tree_prompt
                .as_ref()
                .unwrap()
                .read(cx);
            p.input_entity()
        });
        cx.update(|window, cx| {
            prompt_input.update(cx, |s, cx| s.set_value("hello.txt", window, cx));
        });
        cx.simulate_keystrokes("enter");
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(target.is_file(), "应创建 hello.txt");
        assert!(
            cx.update(|_w, cx| app.read(cx).file_tree_prompt.is_none()),
            "提交成功应关闭弹层"
        );
        ft_wait_child(&app, cx, &dir, "hello.txt");
        assert!(
            cx.update(|_w, cx| {
                app.read(cx)
                    .model.borrow().tabs
                    .iter()
                    .any(|t| t.payload.file_path.as_deref() == Some(target.as_path()))
            }),
            "新建文件应自动打开成选项卡"
        );

        // 重名：再次新建同名 → 内联提示不静默。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file_tree_prompt(
                    crate::app::file_tree_prompt::FileTreePromptKind::NewFile {
                        parent: dir.clone(),
                    },
                    window,
                    cx,
                );
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let prompt_input2 = cx.update(|_w, cx| {
            app.read(cx).file_tree_prompt.as_ref().unwrap().read(cx).input_entity()
        });
        cx.update(|window, cx| {
            prompt_input2.update(cx, |s, cx| s.set_value("hello.txt", window, cx));
        });
        cx.simulate_keystrokes("enter");
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("file-tree-prompt-hint").is_some(),
            "重名应内联提示"
        );
        // Esc 取消。
        cx.simulate_keystrokes("escape");
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| app.read(cx).file_tree_prompt.is_none()),
            "Esc 应关闭弹层"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件树输入弹层按钮：点「确定」提交（与回车同语义，预填默认名创建）；
    /// 点「取消」关闭且不创建。
    #[gpui::test]
    async fn sidebar_file_prompt_buttons_submit_and_cancel(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        let dir = std::env::temp_dir().join(format!("gpui-ft-btn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        ft_open_folder(&app, cx, &dir);
        // 新建文件：点「确定」→ 用预填默认名提交 → 创建 + 自动打开 + 弹层关闭。
        cx.update(|window, cx| {
            app.update(
                cx,
                |app, cx| {
                    app.open_file_tree_prompt(
                        crate::app::file_tree_prompt::FileTreePromptKind::NewFile {
                            parent: dir.clone(),
                        },
                        window,
                        cx,
                    );
                },
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let ok = cx
            .debug_bounds("file-tree-prompt-ok")
            .expect("确定按钮应渲染");
        cx.simulate_click(ok.center(), Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            dir.join("新建文件.md").is_file(),
            "点确定应创建默认名文件"
        );
        assert!(
            cx.update(|_w, cx| app.read(cx).file_tree_prompt.is_none()),
            "提交成功应关闭弹层"
        );
        // 新建文件夹：点「取消」→ 关闭且不创建。
        cx.update(|window, cx| {
            app.update(
                cx,
                |app, cx| {
                    app.open_file_tree_prompt(
                        crate::app::file_tree_prompt::FileTreePromptKind::NewFolder {
                            parent: dir.clone(),
                        },
                        window,
                        cx,
                    );
                },
            );
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let cancel = cx
            .debug_bounds("file-tree-prompt-cancel")
            .expect("取消按钮应渲染");
        cx.simulate_click(cancel.center(), Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| app.read(cx).file_tree_prompt.is_none()),
            "点取消应关闭弹层"
        );
        assert!(
            !dir.join("新建文件夹").exists(),
            "取消不应创建文件或文件夹"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 新建文件夹：右键目录 → 新建文件夹… → 回车提交 → 磁盘创建 + 树刷新。
    #[gpui::test]
    async fn sidebar_file_new_folder_creates_and_tree_refreshes(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        let dir = std::env::temp_dir().join(format!("gpui-ft-nfold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        ft_open_folder(&app, cx, &dir);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file_tree_prompt(
                    crate::app::file_tree_prompt::FileTreePromptKind::NewFolder {
                        parent: dir.clone(),
                    },
                    window,
                    cx,
                );
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let pv = cx.update(|_w, cx| {
            app.read(cx).file_tree_prompt.as_ref().unwrap().read(cx).input_value(cx)
        });
        assert_eq!(pv, "新建文件夹", "应预填默认名");
        let prompt_input = cx.update(|_w, cx| {
            app.read(cx).file_tree_prompt.as_ref().unwrap().read(cx).input_entity()
        });
        cx.update(|window, cx| {
            prompt_input.update(cx, |s, cx| s.set_value("新目录", window, cx));
        });
        cx.simulate_keystrokes("enter");
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(dir.join("新目录").is_dir(), "应创建新目录");
        ft_wait_child(&app, cx, &dir, "新目录");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 重命名：右键 → 重命名… → 预填当前名 → 改名回车 → 磁盘改名 + **已打开选项卡路径
    /// 同步** + 会话清单记录新路径 + 树刷新；目标已存在 → 内联提示。
    #[gpui::test]
    async fn sidebar_file_rename_syncs_tab_and_session(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        let dir = std::env::temp_dir().join(format!("gpui-ft-ren-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old_path = dir.join("a.txt");
        let new_path = dir.join("b.md");
        std::fs::write(&old_path, "AAA").unwrap();
        std::fs::write(dir.join("occupied.txt"), "OCC").unwrap();
        ft_open_folder(&app, cx, &dir);
        // 打开 a.txt 作为活动选项卡。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_path_or_activate(&old_path, window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| {
                app.read(cx)
                    .model.borrow().tabs
                    .iter()
                    .any(|t| t.payload.file_path.as_deref() == Some(old_path.as_path()))
            }),
            "a.txt 应已打开"
        );

        // 右键 a.txt 行（行序 root=0、a.txt=1——无子目录）→ 重命名…
        let a_row = cx.debug_bounds("sidebar-file-row-1").expect("a.txt 行应渲染");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: a_row.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let ren_item = cx.debug_bounds("file-tree-menu-rename").expect("应有重命名项");
        cx.simulate_click(ren_item.center(), Default::default());
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let pv = cx.update(|_w, cx| {
            app.read(cx).file_tree_prompt.as_ref().unwrap().read(cx).input_value(cx)
        });
        assert_eq!(pv, "a.txt", "应预填当前文件名");
        let pi = cx.update(|_w, cx| {
            app.read(cx).file_tree_prompt.as_ref().unwrap().read(cx).input_entity()
        });
        cx.update(|window, cx| {
            pi.update(cx, |s, cx| s.set_value("b.md", window, cx));
        });
        cx.simulate_keystrokes("enter");
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(!old_path.exists() && new_path.is_file(), "磁盘应改名 a.txt → b.md");
        assert!(
            cx.update(|_w, cx| {
                app.read(cx)
                    .model.borrow().tabs
                    .iter()
                    .any(|t| t.payload.file_path.as_deref() == Some(new_path.as_path()))
            }),
            "已打开选项卡路径应同步为 b.md"
        );
        ft_wait_child(&app, cx, &dir, "b.md");
        // 树刷新：布局行里有 b.md、没有 a.txt。
        let labels = cx.update(|_w, cx| {
            app.read(cx)
                .sidebar
                .files_layout
                .as_ref()
                .unwrap()
                .1
                .iter()
                .map(|r| r.label.to_string())
                .collect::<Vec<_>>()
        });
        assert!(labels.contains(&"b.md".to_string()));
        assert!(!labels.contains(&"a.txt".to_string()));

        // 会话持久化一致：显式目录 save_session_to → 清单应记录新路径。
        let sess = std::env::temp_dir().join(format!("gpui-ft-ren-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sess);
        std::fs::create_dir_all(&sess).unwrap();
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                crate::app::session::save_session_to(app, &sess, cx);
            });
        });
        let manifest = session_store::read_manifest(&sess).expect("清单应写入");
        assert!(
            manifest.tabs.iter().any(|t| {
                matches!(
                    t,
                    session_store::SessionTab::File { path }
                        if path == &new_path
                )
            }),
            "会话清单应记录重命名后的新路径"
        );
        let _ = std::fs::remove_dir_all(&sess);

        // 目标已存在 → 内联提示（重命名到 occupied.txt）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file_tree_prompt(
                    crate::app::file_tree_prompt::FileTreePromptKind::Rename {
                        path: new_path.clone(),
                        is_dir: false,
                    },
                    window,
                    cx,
                );
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let pi2 = cx.update(|_w, cx| {
            app.read(cx).file_tree_prompt.as_ref().unwrap().read(cx).input_entity()
        });
        cx.update(|window, cx| {
            pi2.update(cx, |s, cx| s.set_value("occupied.txt", window, cx));
        });
        cx.simulate_keystrokes("enter");
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("file-tree-prompt-hint").is_some(),
            "目标已存在应内联提示"
        );
        cx.simulate_keystrokes("escape");
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 删除：原生确认后 → 干净受影响选项卡同步关闭 + 磁盘删除 + 树刷新；
    /// 有未保存修改的受影响选项卡 → 不删磁盘（不可静默丢内容），提示后取消。
    #[gpui::test]
    async fn sidebar_file_delete_closes_tab_and_dirty_abort(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        let dir = std::env::temp_dir().join(format!("gpui-ft-del-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let d_path = dir.join("d.txt");
        let dirty_path = dir.join("dirty.txt");
        std::fs::write(&d_path, "D").unwrap();
        std::fs::write(&dirty_path, "DIRTY").unwrap();
        ft_open_folder(&app, cx, &dir);
        // 打开 d.txt 与 dirty.txt 两个选项卡。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_path_or_activate(&d_path, window, cx);
                app.open_path_or_activate(&dirty_path, window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let tabs_before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        assert!(
            cx.update(|_w, cx| {
                let a = app.read(cx);
                a.model.borrow().tabs
                    .iter()
                    .any(|t| t.payload.file_path.as_deref() == Some(d_path.as_path()))
            }),
            "d.txt 已打开"
        );

        // —— 干净文件删除：执行删除入口（原生确认框已由 `file_tree_delete_request`
        // 在真实环境弹，headless 直接驱动确认后的执行体）——
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.file_tree_delete(&d_path, window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(!d_path.exists(), "确认后干净文件应从磁盘删除");
        assert!(
            cx.update(|_w, cx| {
                !app.read(cx)
                    .model.borrow().tabs
                    .iter()
                    .any(|t| t.payload.file_path.as_deref() == Some(d_path.as_path()))
            }),
            "已打开选项卡应同步关闭"
        );
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            tabs_before - 1,
            "只应关掉 d.txt 一个选项卡"
        );
        ft_wait_layout(&app, cx);
        let labels = cx.update(|_w, cx| {
            app.read(cx)
                .sidebar
                .files_layout
                .as_ref()
                .unwrap()
                .1
                .iter()
                .map(|r| r.label.to_string())
                .collect::<Vec<_>>()
        });
        assert!(!labels.contains(&"d.txt".to_string()), "树应刷新去掉 d.txt");

        // —— 未保存修改的受影响选项卡 → 不删磁盘（不可静默丢内容）——
        cx.update(|_w, cx| {
            app.update(cx, |app, _cx| {
                if let Some(t) = app
                    .model.borrow_mut().tabs
                    .iter_mut()
                    .find(|t| t.payload.file_path.as_deref() == Some(dirty_path.as_path()))
                {
                    t.payload.dirty = true;
                }
            });
        });
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.file_tree_delete(&dirty_path, window, cx);
            });
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(dirty_path.is_file(), "有未保存修改应不删磁盘");
        assert!(
            cx.update(|_w, cx| {
                app.read(cx)
                    .model.borrow().tabs
                    .iter()
                    .any(|t| t.payload.file_path.as_deref() == Some(dirty_path.as_path()))
            }),
            "有未保存修改的选项卡不应被关闭"
        );
        let status = cx.update(|_w, cx| app.read(cx).model.borrow().status.to_string());
        assert!(status.contains("未保存修改"), "应提示存在未保存修改，实际={status}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件树右键菜单新组结构与分割线：文件菜单 = 7 项 + 3 分割线，目录菜单 = 9 项 +
    /// 3 分割线；五个新项的 debug_selector 全部存在。
    #[gpui::test]
    async fn sidebar_file_right_click_menu_extra_items_and_separators(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        let dir = std::env::temp_dir().join(format!("gpui-ft-x-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        std::fs::write(dir.join("a.txt"), "A").unwrap();
        ft_open_folder(&app, cx, &dir);

        // 文件行（root=0、subdir=1、a.txt=2）右键。
        let file_row = cx.debug_bounds("sidebar-file-row-2").expect("a.txt 行应渲染");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: file_row.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let probe = cx.update(|_w, cx| {
            app.read(cx)
                .file_tree_menu_entity
                .as_ref()
                .expect("菜单应打开")
                .read(cx)
                .layout_probe()
        });
        assert_eq!(
            probe,
            (10, 3),
            "文件菜单 = 7 项 + 3 分割线（总条目 10）=（总条目，分割线数），实际={probe:?}"
        );
        for sel in [
            "file-tree-menu-rename",
            "file-tree-menu-delete",
            "file-tree-menu-reveal",
            "file-tree-menu-copy-path",
            "file-tree-menu-copy-rel-path",
            "file-tree-menu-cut",
            "file-tree-menu-copy-file",
        ] {
            assert!(
                cx.debug_bounds(sel).is_some(),
                "文件菜单应含 {sel}"
            );
        }
        cx.simulate_keystrokes("escape");
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 目录行（subdir=1）右键 → 多两个新建项。
        let dir_row = cx.debug_bounds("sidebar-file-row-1").expect("subdir 行应渲染");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: dir_row.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let probe2 = cx.update(|_w, cx| {
            app.read(cx)
                .file_tree_menu_entity
                .as_ref()
                .expect("菜单应打开")
                .read(cx)
                .layout_probe()
        });
        assert_eq!(
            probe2,
            (12, 3),
            "目录菜单 = 9 项 + 3 分割线（总条目 12）=（总条目，分割线数），实际={probe2:?}"
        );
        assert!(cx.debug_bounds("file-tree-menu-new-file").is_some());
        assert!(cx.debug_bounds("file-tree-menu-reveal").is_some());
        cx.simulate_keystrokes("escape");
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 复制路径 / 复制相对路径 → 文本剪贴板断言：绝对路径（无引号）/ 相对已打开文件夹根。
    #[gpui::test]
    async fn sidebar_file_menu_copy_path_and_relative_path(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        fs_clear_restored_folder(&app, cx);
        let dir = std::env::temp_dir().join(format!("gpui-ft-cp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("a.txt");
        std::fs::write(&target, "A").unwrap();
        ft_open_folder(&app, cx, &dir);

        // 备份原文本剪贴板（写「复制路径」会覆盖真实剪贴板），测试末还原，避免污染用户剪贴板。
        let orig_clip: Option<String> = cx.update(|_w, cx| {
            cx.read_from_clipboard()
                .and_then(|i| i.text().map(|s| s.to_string()))
        });

        // 打开文件菜单（单文件：root=0、a.txt=1），点「复制路径」。
        let row = cx.debug_bounds("sidebar-file-row-1").expect("a.txt 行应渲染");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: row.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let abs_item = cx.debug_bounds("file-tree-menu-copy-path").expect("复制路径项应存在");
        cx.simulate_click(abs_item.center(), Default::default());
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let text: Option<String> = cx.update(|_w, cx| {
            cx.read_from_clipboard()
                .map(|i| i.text().unwrap_or_default())
        });
        assert_eq!(
            text.as_deref(),
            Some(target.to_string_lossy().as_ref()),
            "复制路径应为绝对路径且不带引号"
        );

        // 重新开菜单 → 点「复制相对路径」→ 相对根 = a.txt。
        let row2 = cx.debug_bounds("sidebar-file-row-1").expect("a.txt 行仍在");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: row2.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let rel_item = cx.debug_bounds("file-tree-menu-copy-rel-path").expect("复制相对路径项应存在");
        cx.simulate_click(rel_item.center(), Default::default());
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let rel: Option<String> = cx.update(|_w, cx| {
            cx.read_from_clipboard()
                .map(|i| i.text().unwrap_or_default())
        });
        assert_eq!(
            rel.as_deref(),
            Some("a.txt"),
            "复制相对路径应相对已打开文件夹根（原生分隔符、无引号）"
        );
        // 还原测试前剪贴板文本（有原内容才写回）。
        if let Some(text) = orig_clip {
            cx.update(|_w, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

