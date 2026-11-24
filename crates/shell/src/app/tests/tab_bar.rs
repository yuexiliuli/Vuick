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

    /// 回归：选项卡的 × 关闭钮必须随选项卡渲染——迁移到 shell_tabs
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

    /// VS Code 经典选项卡几何（2026-11-24 修订）：激活/非激活统一「全高 32px 单元格」
    /// （`h(px(32.)).mt(px(0.)).mb(px(0.))`、内容 items_center 居中）→ 内容中心恒 y=16：
    /// 选中文字不再位移（旧「激活 29+mt3 中心 17.5 vs 非激活 26+my3 中心 16」→ 选中下移
    /// 1.5px，即 bug 1）；hover/激活底色画在 32px 全高盒上铺满整格（旧底色画在 26/29 盒
    /// 上、3px 上下 margin 在盒外留空隙，即 bug 2）。顶部 1px 蓝强调线 = 绝对定位子元素
    /// `tab-accent-{idx}`（贴齐选项卡栏最顶缘 y0，用户指定），非盒 border（避免 border 落
    /// 在叠加位置）。激活项底缘贴栏底（32 到栏底）→ 以不透明编辑区底色盖断栏底分隔线在
    /// 选中 x 段的显示（`tab-bar-divider` 存在且 1px 高贴栏底）。
    /// 判别力：撤掉「全高 32」几何改动，本测试应红——(a) 内容中心位移 1.5px、(b) 盒高≠32。
    #[gpui::test]
    async fn active_tab_bottom_aligns_with_tab_bar_bottom(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // (a) bug 1 判别前置：读 tab-0 在「激活态」下的内容中心 y（关闭钮 `.center()`）。
        let close0_active = cx
            .debug_bounds("tab-close-0")
            .expect("激活态下 tab-0 关闭钮应渲染")
            .center()
            .y
            .as_f32();
        // add_app 预建 tab-0（激活）；再新建 tab-1 → 新 tab 成为激活、tab-0 转非激活。
        cx.update(|window, cx| app.update(cx, |app, cx| app.new_tab(window, cx)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        let bar_tabs = cx
            .debug_bounds("tab-bar-tabs")
            .expect("选项卡滚动容器 tab-bar-tabs 应渲染");
        let active = cx
            .debug_bounds("tab-1")
            .expect("激活选项卡 tab-1 应渲染");
        let inactive = cx
            .debug_bounds("tab-0")
            .expect("非激活选项卡 tab-0 应渲染");
        // (c) 激活项底缘贴齐选项卡栏底（≤1px；新盒高 32 到栏底仍成立）。
        assert!(
            (active.bottom().as_f32() - bar_tabs.bottom().as_f32()).abs() <= 1.0,
            "激活选项卡底缘应贴齐选项卡栏底：active={active:?} bar_tabs={bar_tabs:?}"
        );
        // (b) bug 2 判别：激活/非激活盒高都 == 单元格高（全高 32px、铺满整格，无 3px
        // 上下空隙）——撤掉「全高 32」该断言红。
        for (label, tab) in [("激活", &active), ("非激活", &inactive)] {
            assert!(
                tab.size.height.as_f32() >= 31.0
                    && (tab.size.height.as_f32() - bar_tabs.size.height.as_f32()).abs() <= 0.5,
                "{label}选项卡应铺满整格（全高 32）：tab={tab:?} bar_tabs={bar_tabs:?}"
            );
        }
        // (a) bug 1 判别：切换激活前后同一 tab 的内容中心 y 不变（≤0.5px；旧行为激活
        // 17.5 vs 非激活 16、差 1.5px 必红）。
        let close0_inactive = cx
            .debug_bounds("tab-close-0")
            .expect("非激活态下 tab-0 关闭钮应渲染")
            .center()
            .y
            .as_f32();
        assert!(
            (close0_inactive - close0_active).abs() <= 0.5,
            "切换激活前后 tab-0 内容中心 y 应不变（防选中文字位移）：active={close0_active} inactive={close0_inactive}"
        );
        // 顶部蓝强调线：仅激活项有（绝对定位 1px、贴齐选项卡栏最顶缘 y0），非激活无。
        let accent = cx
            .debug_bounds("tab-accent-1")
            .expect("激活选项卡应有顶部蓝线 tab-accent-1");
        assert!(
            accent.size.height.as_f32() <= 1.5
                && (accent.origin.y.as_f32() - active.origin.y.as_f32()).abs() <= 1.0,
            "顶线应 1px 高、贴齐激活项顶部（y0）：accent={accent:?} active={active:?}"
        );
        assert!(
            cx.debug_bounds("tab-accent-0").is_none(),
            "非激活选项卡不应有顶部蓝线"
        );
        // 栏底自绘分隔线：1px 高、贴栏底（选中项用不透明底盖断它在选中 x 范围内的部分）。
        let divider = cx
            .debug_bounds("tab-bar-divider")
            .expect("选项卡栏底分隔线应渲染");
        assert!(
            divider.size.height.as_f32() <= 1.5
                && (divider.bottom().as_f32() - bar_tabs.bottom().as_f32()).abs() <= 1.0,
            "分隔线应 1px 高并贴齐栏底：divider={divider:?}"
        );
    }

