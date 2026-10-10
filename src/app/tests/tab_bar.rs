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

