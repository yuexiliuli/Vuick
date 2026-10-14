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

    /// 侧边栏「目录 / 搜索」竖列切换栏可调用出对应界面（已移除文件视图）。
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
        // 竖列切换栏有「目录/搜索」两个按钮（无「文件」）
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
            cx.debug_bounds("sidebar-btn-files").is_none(),
            "已去掉文件按钮"
        );
        // 目录按钮与搜索按钮上下竖列（搜索按钮位于目录按钮下方）
        let toc = cx.debug_bounds("sidebar-btn-toc").unwrap();
        let search = cx.debug_bounds("sidebar-btn-search").unwrap();
        assert!(
            search.origin.y > toc.origin.y,
            "搜索按钮应在目录按钮之下（竖列），toc={toc:?} search={search:?}"
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
                a.dark_mode = true;
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
                a.dark_mode = false;
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
            a.tabs[a.active_tab].md_wy_vscroll.clone()
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
            a.tabs[a.active_tab].md_wy_vscroll.clone()
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
        let dir = std::env::temp_dir().join("gpui-editor-sidebar-toc.md");
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
            let input = app.read(cx).search_state.clone();
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
        let f = std::env::temp_dir().join("gpui-sz-cache.md");
        std::fs::write(&f, "# 甲\n阿尔法 shanghai 内容\n真义行 beta\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let ti = cx.update(|_w, cx| app.read(cx).active_tab);

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
        let before = cx.update(|_w, cx| app.read(cx).search_scan_count);
        assert_eq!(before, 0, "查询为空时不应扫描");

        // 第一次键入「shanghai」：防抖期内（<150ms）不扫描、不提交
        cx.update(|window, cx| {
            let input = app.read(cx).search_state.clone();
            input.update(cx, |s, cx| s.set_value("shanghai", window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let debounced = cx.update(|_w, cx| app.read(cx).search_scan_count);
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
        let after = cx.update(|_w, cx| app.read(cx).search_scan_count);
        assert_eq!(after, before + 1, "防抖期满后应精确扫描 1 次");
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).search_committed.clone()),
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
        let steady = cx.update(|_w, cx| app.read(cx).search_scan_count);
        assert_eq!(steady, after, "查询未变时重复渲染不应重扫");

        // 换查询「beta」→ 防抖后扫描一次，命中第 2 行
        cx.update(|window, cx| {
            let input = app.read(cx).search_state.clone();
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
        let changed = cx.update(|_w, cx| app.read(cx).search_scan_count);
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
                let input = app.read(cx).search_state.clone();
                input.update(cx, |s, cx| s.set_value(q, window, cx));
            });
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(20));
            for _ in 0..2 {
                cx.run_until_parked();
            }
        }
        let during = cx.update(|_w, cx| app.read(cx).search_scan_count);
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
        let committed = cx.update(|_w, cx| app.read(cx).search_committed.clone());
        assert_eq!(committed, "预览编辑", "防抖应提交最终输入词");
        let scans = cx.update(|_w, cx| app.read(cx).search_scan_count);
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
            let input = app.read(cx).search_state.clone();
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
            let input = app.read(cx).search_state.clone();
            input.update(cx, |s, cx| {
                s.replace_text_in_range(None, "编辑", window, cx);
            });
        });
        let box_value =
            cx.update(|_w, cx| app.read(cx).search_state.read(cx).value().to_string());
        assert_eq!(box_value, "编辑", "输入框文本应为干净中文（组合替换不追加）");
        // 防抖期满提交 → 扫描查询词同样是干净中文
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        let committed = cx.update(|_w, cx| app.read(cx).search_committed.clone());
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
            cx.update(|_w, cx| app.read(cx).tabs.is_empty()),
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
            cx.update(|_w, cx| app.read(cx).tabs.len()),
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
            app.read(cx).tabs[0].editor.read(cx).focus_handle().is_focused(window)
        });
        assert!(focused, "新建后编辑区应获得焦点");
        // 直接键入（真实键入路径）→ 内容写入且欢迎页不回来。
        let editor = cx.update(|_w, cx| app.read(cx).tabs[0].editor.clone());
        cx.update(|_, cx| editor.update(cx, |ed, cx| ed.insert_text("hi", cx)));
        settle_frames(cx, 3);
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).tabs[0].editor.read(cx).value().to_string()),
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
            cx.update(|_w, cx| app.read(cx).tabs.len()),
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
        // 未编辑不弹保存框：close_tab 直通 remove_tab（真实用户「关闭最后一个 tab」路径）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.close_tab(0, window, cx));
        });
        settle_frames(cx, 5);
        assert!(
            cx.update(|_w, cx| app.read(cx).tabs.is_empty()),
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
        assert_eq!(cx.update(|_w, cx| app.read(cx).tabs.len()), 1, "预建 1 个空未命名选项卡");
        assert!(cx.debug_bounds("editor-area").is_some(), "空未命名 tab 也直接渲染编辑区");
        assert!(cx.debug_bounds("empty-state").is_none(), "有 tab（哪怕空未命名）就不显示欢迎页");
        // 键入后依旧不显示欢迎页（新模型没有「欢迎页盖在有内容上」的路径）。
        let editor = cx.update(|_w, cx| app.read(cx).tabs[0].editor.clone());
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
            cx.update(|_w, cx| app.read(cx).tabs.len()),
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
            let input = app.read(cx).search_state.clone();
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
            let input = app.read(cx).search_state.clone();
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
        use crate::app::sidebar::find_ci;
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
        use crate::app::sidebar::find_ci;
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

