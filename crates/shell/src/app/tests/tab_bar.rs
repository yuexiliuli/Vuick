    /// 选项卡过多时，选项卡栏应可横向滚动（出现横向滚动条），最新选项卡不再被遮挡。
    #[gpui::test]
    async fn tab_bar_scrolls_when_tabs_overflow(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(400.), px(400.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 连续新建大量选项卡，总宽超过一屏
        for _ in 0..30 {
            cx.update(|window, cx| app.update(cx, |app, cx| app.new_tab(window, cx)));
            for _ in 0..2 {
                cx.run_until_parked();
            }
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let max_w = cx.update(|_w, cx| app.read(cx).tab_bar_hscroll.handle.max_offset().x);
        assert!(
            max_w > px(0.),
            "选项卡过多时选项卡栏应可横向滚动：max_offset={max_w:?}"
        );
        // 滚动条默认自动隐藏——先“滚动”点亮它，再断言浮层存在。
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| app.poke_tab_hscroll(cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 回归：横向滚动条浮层应在选项卡栏内（底部贴边、有可见高度），
        // 而不是被推到栏外裁掉（选项卡栏去上下内边距后曾把浮层挤出）。
        let overlay = cx
            .debug_bounds("tab-bar-hscroll-overlay")
            .expect("超宽时应有横向滚动条浮层 tab-bar-hscroll-overlay");
        assert!(
            overlay.size.height > px(0.),
            "滚动条浮层应有可见高度：overlay={overlay:?}"
        );
    }

    /// 选项卡栏横向滚动条自动隐藏：默认隐藏 → 滚动(滚轮)时出现 → 停止滚动
    /// 超时后自动隐藏；拖拽滑块期间保持可见。
    #[gpui::test]
    async fn tab_bar_hscroll_auto_hides(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(400.), px(400.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 大量选项卡 → 超宽可横向滚动
        for _ in 0..30 {
            cx.update(|window, cx| app.update(cx, |app, cx| app.new_tab(window, cx)));
            for _ in 0..2 {
                cx.run_until_parked();
            }
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // ① 默认隐藏：即使超宽，可见标记为假且浮层不渲染
        let hidden = cx.update(|_w, cx| *app.read(cx).tab_bar_hscroll.visible.borrow());
        assert!(!hidden, "滚动条初始应隐藏");
        assert!(
            cx.debug_bounds("tab-bar-hscroll-overlay").is_none(),
            "隐藏时浮层不应渲染"
        );

        // ② 向选项卡滚动区派发一次横向滚轮 → 点亮并渲染浮层
        let tabs_area = cx
            .debug_bounds("tab-bar-tabs")
            .expect("应有选项卡滚动区 tab-bar-tabs");
        let wheel_pos = gpui::Point::new(
            px(tabs_area.origin.x.as_f32() + tabs_area.size.width.as_f32() / 2.0),
            px(tabs_area.origin.y.as_f32() + 8.0),
        );
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: wheel_pos,
            delta: gpui::ScrollDelta::Pixels(gpui::Point::new(px(-40.), px(0.))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let shown = cx.update(|_w, cx| *app.read(cx).tab_bar_hscroll.visible.borrow());
        assert!(shown, "滚轮滚动后滚动条应可见");
        assert!(
            cx.debug_bounds("tab-bar-hscroll-overlay").is_some(),
            "滚动后浮层应渲染"
        );

        // ③ 停止滚动超过隐藏时长 → 自动隐藏
        cx.executor().advance_clock(std::time::Duration::from_millis(2000));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let hidden_again = cx.update(|_w, cx| *app.read(cx).tab_bar_hscroll.visible.borrow());
        assert!(!hidden_again, "停止滚动超过时长后应自动隐藏");
        assert!(
            cx.debug_bounds("tab-bar-hscroll-overlay").is_none(),
            "隐藏后浮层应消失"
        );
    }

    /// 文件名展示迁移（2026-11-10）：**有名文件**选项卡悬停 → 200ms 防抖后在 tab 下方浮出
    /// 完整路径 tooltip；移出立即消失。防抖期内（<200ms）恒不显示。
    /// 注：`add_app` 预建 1 个未命名空 tab（tab-0），`open_file` 新建文件选项卡 → tab-1。
    #[gpui::test]
    async fn tab_tooltip_path_delayed_appear_geom_and_hide(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 打开临时文件 → 有名文件选项卡 tab-1。
        let f = std::env::temp_dir().join(format!("gpui-tab-tip-{}.txt", std::process::id()));
        std::fs::write(&f, "hello\nworld\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
        }
        let tab = cx
            .debug_bounds("tab-1")
            .expect("打开文件后应渲染有名文件选项卡 tab-1");
        // 初始无浮层。
        assert!(
            cx.debug_bounds("tooltip-tab-tip-1").is_none(),
            "未悬停时不应渲染 tab tooltip"
        );
        // 悬停进入：防抖期内（< TOOLTIP_DELAY_MS=200ms）不显示。
        cx.simulate_mouse_move(tab.center(), None, Default::default());
        cx.executor().advance_clock(std::time::Duration::from_millis(100));
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("tooltip-tab-tip-1").is_none(),
            "防抖期内（100ms<200ms）tooltip 不应出现"
        );
        // 时钟再推进超过防抖 → 浮层出现，且锚在 tab 下方（不盖选项卡本体/相邻选项卡）。
        cx.executor().advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let tip = cx
            .debug_bounds("tooltip-tab-tip-1")
            .expect("延迟后 tooltip 应在 tab 下方出现");
        assert!(
            tip.origin.y.as_f32() >= tab.origin.y.as_f32() + tab.size.height.as_f32(),
            "tooltip 应在 tab 下方（不盖本体），tip={tip:?} tab={tab:?}"
        );
        // 左缘对齐：此刻鼠标悬停在 tab.center() → tooltip 左缘 x 应≈鼠标 x（偏移 ≤1px，
        // 浮在指针右下方）。
        assert!(
            (tip.origin.x.as_f32() - tab.center().x.as_f32()).abs() <= 1.0,
            "tooltip 左缘 x 应对齐鼠标当时 x（悬浮 tab.center），tip={tip:?} tab={tab:?}"
        );
        // 冻结断言：tooltip 已显示后鼠标再动（x+30，仍在 tab 内）→ 位置不跟随、左缘 x 不变。
        cx.simulate_mouse_move(
            gpui::point(px(tab.center().x.as_f32() + 30.), tab.center().y),
            None,
            Default::default(),
        );
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let tip_after = cx
            .debug_bounds("tooltip-tab-tip-1")
            .expect("tooltip 应仍显示");
        assert!(
            (tip_after.origin.x.as_f32() - tip.origin.x.as_f32()).abs() <= 0.5,
            "tooltip 显示后鼠标移动不应再跟随（冻结），before={tip:?} after={tip_after:?}"
        );
        // 移出 → 立即消失。
        cx.simulate_mouse_move(gpui::point(px(8.), px(8.)), None, Default::default());
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("tooltip-tab-tip-1").is_none(),
            "移出后 tooltip 应立即消失"
        );
        let _ = std::fs::remove_file(&f);
    }

    /// 文件名展示迁移（2026-11-10）：**未命名**选项卡（无 file_path）悬停恒不显示浮层——
    /// 即使防抖时钟已充分推进（400ms>200ms）也不挂 tooltip 层（`has_path` 为假）。
    #[gpui::test]
    async fn tab_tooltip_not_shown_for_unnamed_tab(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let _ = app;
        // 默认即有未命名空选项卡 tab-0（tabs_actions 实证）。
        let tab = cx
            .debug_bounds("tab-0")
            .expect("初始未命名选项卡应渲染");
        cx.simulate_mouse_move(tab.center(), None, Default::default());
        cx.executor().advance_clock(std::time::Duration::from_millis(400));
        cx.run_until_parked();
        for _ in 0..3 {
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("tooltip-tab-tip-0").is_none(),
            "未命名选项卡悬停恒不显示路径 tooltip"
        );
    }

    /// H1 评审回归（2026-11-11）：选项卡的 × 关闭钮必须随选项卡渲染——迁移到 shell_tabs
    /// 时曾误删关闭钮子元素 `.child("×")` 成空框（纯视觉回归、无测试覆盖）。headless 无法
    /// 断言字形本身，用 debug 断言关闭钮元素渲染于选项卡内部（含 × 文本的 18×18 元素）；
    /// 一旦关闭钮或其子元素结构被移除即失败。`tab-close-{idx}` 的 debug_selector 定义于
    /// `shell_tabs::bar::render_tab_bar_item`。
    #[gpui::test]
    async fn tab_close_button_renders_inside_tab(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let _ = app;
        let tab = cx
            .debug_bounds("tab-0")
            .expect("初始未命名选项卡应渲染");
        let close = cx
            .debug_bounds("tab-close-0")
            .expect("关闭钮应渲染（含 × 文本的 18×18 元素）");
        assert_eq!(
            (
                close.size.width.as_f32().round(),
                close.size.height.as_f32().round(),
            ),
            (18., 18.),
            "关闭钮应为 18×18：close={close:?}"
        );
        assert!(
            tab.contains(&close.center()),
            "关闭钮应位于选项卡内部：tab={tab:?} close={close:?}"
        );
    }

