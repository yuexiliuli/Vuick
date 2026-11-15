    // ---------------------------------------------------------------------------
    // 多窗口（拖出拆分 / 拖回还原）
    // ---------------------------------------------------------------------------

    /// 快速读取全局 EditorApp 注册表的可观测快照：`(is_detached, tab 数, 活动选项卡含文件路径, 活动正文)`。
    fn registry_snapshot(
        cx: &mut TestAppContext,
    ) -> Vec<(bool, usize, bool, Option<String>)> {
        cx.update(|cx| {
            let reg = cx.global::<EditorAppRegistry>();
            reg.windows
                .iter()
                .filter_map(|(weak, _)| {
                    let entity = weak.upgrade()?;
                    let snap = entity.read_with(cx, |a, c| {
                        let model = a.model.borrow();
                        let tab = model.tabs.get(model.active_tab);
                        (
                            a.is_detached,
                            a.model.borrow().tabs.len(),
                            tab.and_then(|t| t.payload.file_path.clone()).is_some(),
                            tab.map(|t| t.payload.editor.read(c).value()),
                        )
                    });
                    Some(snap)
                })
                .collect()
        })
    }

    /// 选项卡「拔出」→「收纳」往返：文本、脏标记全部保留。
    #[gpui::test]
    async fn detach_adopt_round_trip_preserves_state(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("gpui-detach-roundtrip.txt");
        std::fs::write(&f, "第一行原文").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 编辑区输入 → 置脏（insert_text 属真实编辑，经 EditorEvent::Edited 置脏）
        let editor = cx.update(|_w, cx| app.read(cx).model.borrow().tabs[1].payload.editor.clone());
        cx.update(|_w, cx| editor.update(cx, |ed, cx| ed.insert_text("初始内容", cx)));
        for _ in 0..2 {
            cx.run_until_parked();
        }

        let payload = cx
            .update(|_w, cx| app.update(cx, |app, _| app.take_tab_payload(1)))
            .expect("应有可拔出的选项卡");
        cx.update(|_w, cx| app.update(cx, |app, cx| app.adopt_tab(payload, None, cx)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (text, dirty, count, has_path) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            (
                t.payload.editor.read(cx).value(),
                t.payload.dirty,
                a.model.borrow().tabs.len(),
                t.payload.file_path.is_some(),
            )
        });
        assert_eq!(
            text.to_string(),
            "初始内容第一行原文",
            "拔出→收纳后正文应保留"
        );
        assert!(dirty, "拔出→收纳后脏标记应保留");
        assert_eq!(count, 2, "收纳后仍应有 2 个选项卡（空 + 文件）");
        assert!(has_path, "文件路径应保留");
        let _ = std::fs::remove_file(&f);
    }

    /// 把选项卡拖出窗口外松开 → 拆出独立窗口：主窗口剩未命名选项卡，
    /// 新窗口带文件选项卡且 is_detached=true。
    #[gpui::test]
    async fn drag_tab_out_splits_detached_window(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("gpui-multiwin-a.txt");
        std::fs::write(&f, "AAA 独立窗口").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let tab = cx.debug_bounds("tab-1").expect("文件选项卡 tab-1 应渲染");
        let start = gpui::Point::new(
            tab.origin.x + px(10.),
            tab.origin.y + px(tab.size.height.as_f32() / 2.0),
        );
        // 按下 → 移动到超过拖拽阈值（启动 tab 拖拽）→ 移出窗口。
        // 回归（2026-10「松开才建窗」）：越出窗口边缘（尚未松手）**不得**建独立窗口，
        // 只置预位 + 跟手幽灵窗；真窗口在窗口外松开时才拆出。
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let outside = gpui::Point::new(px(-60.), px(50.));
        cx.simulate_mouse_move(outside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        // 越界未松手：不应有独立窗口、选项卡仍留在主窗口、已进入「松开才建窗」预位。
        let before_up = registry_snapshot(cx);
        assert_eq!(
            before_up.iter().filter(|(d, _, _, _)| *d).count(),
            0,
            "越出窗口边缘（未松手）不应建窗：松开才建，快照={before_up:?}"
        );
        let (armed, ghost_alive, tab_count) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().drag_outside_armed, a.drag_ghost.is_some(), a.model.borrow().tabs.len())
        });
        assert!(armed, "越界未松手应已置「松开才建窗」预位");
        assert!(ghost_alive, "越界后应有窗外跟手幽灵窗");
        assert_eq!(tab_count, 2, "越界未松手选项卡仍留在主窗口");

        // 松开（在窗口外）：不应再次拆分（仍是同一个独立窗口）
        cx.simulate_mouse_up(outside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }

        let snap = registry_snapshot(cx);
        let detached: Vec<_> = snap.iter().filter(|(d, _, _, _)| *d).collect();
        let main: Vec<_> = snap.iter().filter(|(d, _, _, _)| !*d).collect();
        assert_eq!(detached.len(), 1, "应仍只有一个独立窗口，快照={snap:?}");
        assert_eq!(detached[0].1, 1, "独立窗口应恰好 1 个选项卡");
        assert!(detached[0].2, "独立窗口的活动选项卡应有文件路径");
        assert_eq!(
            detached[0].3.as_deref(),
            Some("AAA 独立窗口"),
            "独立窗口应带原正文"
        );
        assert!(
            !main.is_empty() && main.iter().all(|(_, n, has_path, _)| *n >= 1 && !*has_path),
            "主窗口应只剩未命名选项卡，快照={snap:?}"
        );

        // 回归：新窗口应出现在鼠标释放处（源窗口左上角 + 释放点偏移），而不是与主窗口重叠。
        // 释放点在窗口外 (-60, 50) → 新窗口左上角应向左上平移(-60, 50)。
        // 源窗口 = 本 VisualTestContext 绑定的窗口（其 bounds 可直接读）；独立窗口经句柄读取
        // （当前窗口在更新栈上，直接 update 会被判“window not found”）。
        let src_origin = cx.update(|w, _| w.bounds()).origin;
        let windows: Vec<(bool, gpui::AnyWindowHandle)> = cx.update(|_w, cx| {
            let reg = cx.global::<EditorAppRegistry>();
            reg.windows
                .iter()
                .filter_map(|(weak, wh)| {
                    let e = weak.upgrade()?;
                    let detached = e.read_with(cx, |a, _| a.is_detached);
                    Some((detached, *wh))
                })
                .collect()
        });
        let det_origin = windows
            .iter()
            .find(|(detached, _)| *detached)
            .and_then(|(_, wh)| cx.update(|_w, cx| wh.update(cx, |_, w, _| w.bounds()).ok()))
            .map(|b| b.origin)
            .expect("应有独立窗口");
        let expected = src_origin + outside;
        assert!(
            (det_origin.x.as_f32() - expected.x.as_f32()).abs() < 2.0
                && (det_origin.y.as_f32() - expected.y.as_f32()).abs() < 2.0,
            "独立窗口应跟随鼠标释放位置：期望 origin={expected:?}，实际={det_origin:?}"
        );
        let ghost_after = cx.update(|_w, cx| app.read(cx).drag_ghost.is_none());
        assert!(ghost_after, "松开后跟手幽灵窗应销毁");
        let _ = std::fs::remove_file(&f);
    }

    /// 独立窗口的选项卡「还原回主窗口」：主窗口重新拿到该文件选项卡与正文。
    #[gpui::test]
    async fn detached_tab_restores_to_primary(cx: &mut TestAppContext) {
        init_test(cx);
        let (primary, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("gpui-multiwin-b.txt");
        std::fs::write(&f, "BBB 还原").unwrap();
        cx.update(|window, cx| {
            primary.update(cx, |app, cx| {
                app.open_file(f.to_str().unwrap(), window, cx)
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }

        // 从主窗口拔出该文件选项卡，开一个独立窗口收纳它
        let payload = cx
            .update(|_w, cx| primary.update(cx, |app, _| app.take_tab_payload(1)))
            .expect("应有可拔出的选项卡");
        let (_det_root, cx) = cx.add_window_view(|window, cx| {
            let app = cx.new(|cx| EditorApp::new_detached(window, cx, payload));
            ui::root::Root::new(app, window, cx)
        });
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..5 {
            cx.run_until_parked();
        }

        // 找到独立窗口的 EditorApp 实体
        let det = cx
            .update(|_w, cx| {
                let reg = cx.global::<EditorAppRegistry>();
                reg.windows
                    .iter()
                    .filter_map(|(w, _)| w.upgrade())
                    .find(|e| e.read_with(cx, |a, _| a.is_detached))
            })
            .expect("应存在独立窗口");
        assert_eq!(
            cx.update(|_w, cx| det.read_with(cx, |a, _| a.model.borrow().tabs.len())),
            1,
            "独立窗口应恰好 1 个选项卡"
        );

        // 独立窗口把选项卡还原给主窗口
        let payload2 = cx
            .update(|_w, cx| det.update(cx, |app, _| app.take_tab_payload(0)))
            .expect("独立窗口应有选项卡");
        cx.update(|_w, cx| det.update(cx, |app, cx| app.restore_tab_to_primary(payload2, cx)));
        for _ in 0..5 {
            cx.run_until_parked();
        }

        let (has_file, content) = cx.update(|_w, cx| {
            let a = primary.read(cx);
            let model = a.model.borrow();
            let tab = model.tabs.iter().find(|t| t.payload.file_path.is_some());
            (
                tab.is_some(),
                tab.map(|t| t.payload.editor.read(cx).value().to_string()),
            )
        });
        assert!(has_file, "主窗口应重新持有该文件选项卡");
        assert_eq!(content.as_deref(), Some("BBB 还原"), "正文应完整还原");
        let _ = std::fs::remove_file(&f);
    }

    /// 回归（2026-10「松开才建窗」）：独立窗口拖出选项卡到窗外松开 → 还原回主窗口，
    /// 与主窗口拆窗路径对称：越界未松手不还原、有跟手幽灵窗；松开后主窗口复原文件选项卡。
    #[gpui::test]
    async fn detached_drag_out_release_restores_to_primary(cx: &mut TestAppContext) {
        init_test(cx);
        let (primary, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("gpui-multiwin-c.txt");
        std::fs::write(&f, "CCC 拖出还原").unwrap();
        cx.update(|window, cx| {
            primary.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 从主窗口拔出该文件选项卡，交给独立窗口（与 detached_tab_restores_to_primary 同构造）
        let payload = cx
            .update(|_w, cx| primary.update(cx, |app, _| app.take_tab_payload(1)))
            .expect("应有可拔出的选项卡");
        // 开独立窗口（用 owned 的独立 VisualTestContext 模拟，不借走主窗口上下文；
        // 松开后独立窗口 `remove_window` 销毁，后续一律经主窗口上下文读取）。
        let handle = cx.open_window(gpui::size(px(980.), px(720.)), |window, cx| {
            let app = cx.new(|cx| EditorApp::new_detached(window, cx, payload));
            ui::root::Root::new(app, window, cx)
        });
        let mut det_cx = gpui::VisualTestContext::from_window(handle.into(), &cx);
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let det = cx
            .update(|_w, cx| {
                let reg = cx.global::<EditorAppRegistry>();
                reg.windows
                    .iter()
                    .filter_map(|(w, _)| w.upgrade())
                    .find(|e| e.read_with(cx, |a, _| a.is_detached))
            })
            .expect("应存在独立窗口");
        // 在独立窗口上按下 → 拖出窗外（越界未松手：不还原、幽灵跟手）
        let tab = det_cx
            .debug_bounds("tab-0")
            .expect("独立窗口选项卡应渲染");
        let start = gpui::Point::new(
            tab.origin.x + px(10.),
            tab.origin.y + px(tab.size.height.as_f32() / 2.0),
        );
        det_cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        det_cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let outside = gpui::Point::new(px(-50.), px(60.));
        det_cx.simulate_mouse_move(outside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (armed_out, ghost_alive) = cx.update(|_w, cx| {
            let a = det.read(cx);
            (a.model.borrow().drag_outside_armed, a.drag_ghost.is_some())
        });
        assert!(armed_out, "独立窗口拖出窗外应置「松开才还原」预位");
        assert!(ghost_alive, "独立窗口拖出窗外应有跟手幽灵窗");
        // 松开（窗外）→ 还原回主窗口（独立窗口随之销毁）
        det_cx.simulate_mouse_up(outside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (has_file, content) = cx.update(|_w, cx| {
            let a = primary.read(cx);
            let model = a.model.borrow();
            let tab = model.tabs.iter().find(|t| t.payload.file_path.is_some());
            (
                tab.is_some(),
                tab.map(|t| t.payload.editor.read(cx).value().to_string()),
            )
        });
        assert!(has_file, "主窗口应重新持有该文件选项卡");
        assert_eq!(content.as_deref(), Some("CCC 拖出还原"), "正文应完整还原");
        let _ = std::fs::remove_file(&f);
    }

    /// 回归：拖拽选项卡但一直停留在窗口内、窗口内松开 → 不拆独立窗口（tear-off 只在
    /// 越出窗口边缘时发生），选项卡仍留在主窗口。
    #[gpui::test]
    async fn drag_tab_inside_window_does_not_tear_off(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 拖一个未命名选项卡（tab-0），全程留在窗口内再松开
        let tab = cx.debug_bounds("tab-0").expect("选项卡 tab-0 应渲染");
        let start = gpui::Point::new(
            tab.origin.x + px(10.),
            tab.origin.y + px(tab.size.height.as_f32() / 2.0),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        // 回归（真实尺寸）：拖拽 picture 应等于真实选项卡的 laid-out 尺寸
        // （`TabBoundsRecorder` 记录 → 预览显式定宽，neither 变小 nor 溢出）。
        let tab_b = cx.debug_bounds("tab-0").expect("选项卡 tab-0 应渲染");
        let ghost_b = cx
            .debug_bounds("tab-drag-ghost")
            .expect("拖拽 picture 应已布局");
        assert!(
            (tab_b.size.width.as_f32() - ghost_b.size.width.as_f32()).abs() < 2.0
                && (tab_b.size.height.as_f32() - ghost_b.size.height.as_f32()).abs() < 2.0,
            "拖拽 picture 应与真实选项卡同尺寸：tab={:?} ghost={:?}",
            tab_b.size,
            ghost_b.size
        );
        let moved_inside = gpui::Point::new(start.x + px(120.), start.y + px(40.));
        cx.simulate_mouse_move(moved_inside, MouseButton::Left, Default::default());
        cx.simulate_mouse_up(moved_inside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }

        let snap = registry_snapshot(cx);
        assert_eq!(
            snap.iter().filter(|(d, _, _, _)| *d).count(),
            0,
            "窗口内拖拽+松开不应拆出独立窗口，快照={snap:?}"
        );
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            1,
            "主窗口选项卡应保持原样"
        );
    }

    /// 回归（2026-10「松开才建窗」）：拖出窗外（进预位 + 幽灵窗）再拖回窗口内松开 →
    /// **取消**：不建独立窗口、选项卡原位保留、预位复位、跟手幽灵窗销毁。
    #[gpui::test]
    async fn drag_tab_out_back_inside_cancels(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let tab = cx.debug_bounds("tab-0").expect("选项卡 tab-0 应渲染");
        let start = gpui::Point::new(
            tab.origin.x + px(10.),
            tab.origin.y + px(tab.size.height.as_f32() / 2.0),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        // 拖出窗外：进入「松开才建窗」预位 + 跟手幽灵窗
        cx.simulate_mouse_move(
            gpui::Point::new(px(-40.), px(60.)),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (was_armed, ghost_alive) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().drag_outside_armed, a.drag_ghost.is_some())
        });
        assert!(was_armed, "拖出窗外应已置「松开才建窗」预位");
        assert!(ghost_alive, "拖出窗外应有跟手幽灵窗");
        // 拖回窗内：预位复位；幽灵窗继续全程跟随（拖拽结束才销毁）
        let back_inside = gpui::Point::new(start.x + px(120.), start.y + px(40.));
        cx.simulate_mouse_move(back_inside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (armed_after_back, ghost_after_back) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().drag_outside_armed, a.drag_ghost.is_some())
        });
        assert!(!armed_after_back, "拖回窗内应复位「松开才建窗」预位");
        assert!(ghost_after_back, "拖回窗内幽灵窗仍全程跟随（拖拽结束才销毁）");
        // 窗内松开：取消，不建独立窗口；幽灵窗随拖拽结束销毁
        cx.simulate_mouse_up(back_inside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let snap = registry_snapshot(cx);
        assert_eq!(
            snap.iter().filter(|(d, _, _, _)| *d).count(),
            0,
            "拖出再拖回窗内松开不应建独立窗口，快照={snap:?}"
        );
        assert!(
            cx.update(|_w, cx| app.read(cx).drag_ghost.is_none()),
            "窗内松开后幽灵窗应销毁"
        );
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            1,
            "取消拖出后选项卡应原位保留"
        );
    }

    /// 回归（2026-10「松开才建窗」）：反复跨界（出→回→再出）不重复建窗、无残留；
    /// 最终窗外松开才建独立窗口且只建一个。
    #[gpui::test]
    async fn drag_tab_out_in_out_again_release_splits_once(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("gpui-multiwin-d.txt");
        std::fs::write(&f, "DDD 往返").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let tab = cx.debug_bounds("tab-1").expect("文件选项卡 tab-1 应渲染");
        let start = gpui::Point::new(
            tab.origin.x + px(10.),
            tab.origin.y + px(tab.size.height.as_f32() / 2.0),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        // ① 出窗（进入预位 + 幽灵窗）
        cx.simulate_mouse_move(
            gpui::Point::new(px(-60.), px(50.)),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..8 {
            cx.run_until_parked();
        }
        // ② 回窗内（预位复位；幽灵窗全程跟随不销毁）
        let back_inside = gpui::Point::new(start.x + px(120.), start.y + px(40.));
        cx.simulate_mouse_move(back_inside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (armed_mid, ghost_mid) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().drag_outside_armed, a.drag_ghost.is_some())
        });
        assert!(!armed_mid, "拖回窗内应复位「松开才建窗」预位");
        assert!(ghost_mid, "拖回窗内幽灵窗仍全程跟随（窗口全程复用、边缘无卡顿）");
        // ③ 再出窗（幽灵重新跟手；仍未建真窗口）
        cx.simulate_mouse_move(
            gpui::Point::new(px(-80.), px(70.)),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (armed_again, ghost_again, tab_count) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().drag_outside_armed, a.drag_ghost.is_some(), a.model.borrow().tabs.len())
        });
        assert!(armed_again, "再次越界应重新置「松开才建窗」预位");
        assert!(ghost_again, "再次越界应重新跟手幽灵窗");
        assert_eq!(tab_count, 2, "再次越界未松手选项卡仍在主窗口");
        let before_up = registry_snapshot(cx);
        assert_eq!(
            before_up.iter().filter(|(d, _, _, _)| *d).count(),
            0,
            "再次越界未松手不应建窗，快照={before_up:?}"
        );
        // ④ 窗外松开 → 建独立窗口（只建一个）
        cx.simulate_mouse_up(
            gpui::Point::new(px(-80.), px(70.)),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let snap = registry_snapshot(cx);
        assert_eq!(
            snap.iter().filter(|(d, _, _, _)| *d).count(),
            1,
            "窗外松开应恰好建一个独立窗口，快照={snap:?}"
        );
        assert!(
            cx.update(|_w, cx| app.read(cx).drag_ghost.is_none()),
            "窗外松开后（拆窗前销毁）幽灵窗应消失"
        );
        let _ = std::fs::remove_file(&f);
    }

    /// 回归（2026-10 边缘无缝）：拖拽中幽灵窗在「窗内/窗外」全程跟随同一窗口，
    /// 窗口边缘穿越不销毁重建（无创建/销毁卡顿）——拖拽开始即建、结束才销毁。
    #[gpui::test]
    async fn drag_tab_ghost_lifetime_spans_crossings(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 拖 tab-0：按下 → 窗内移动（拖拽开始即建幽灵）→ 出窗外 → 回窗内 → 窗内松开
        let tab = cx.debug_bounds("tab-0").expect("选项卡 tab-0 应渲染");
        let start = gpui::Point::new(
            tab.origin.x + px(10.),
            tab.origin.y + px(tab.size.height.as_f32() / 2.0),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        // 第二次窗内移动触发 on_drag_move（gpui 拖拽在首次越阈值那拍才建立，on_drag_move
        // 从下一拍才派发）→ 拖拽开始即建幽灵窗
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(90.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        // 拖拽开始（窗内）即已建幽灵窗
        let (ghost_start, grab_set) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.drag_ghost.is_some(), a.model.borrow().tab_drag_grab.is_some())
        });
        assert!(ghost_start, "拖拽开始（窗内）即建幽灵窗");
        assert!(grab_set, "拖拽开始应已记录抓取偏移");
        // 出窗外再回窗内：同一幽灵窗仍在（不销毁重建）
        cx.simulate_mouse_move(
            gpui::Point::new(px(-60.), px(60.)),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let ghost_out = cx.update(|_w, cx| app.read(cx).drag_ghost.is_some());
        assert!(ghost_out, "窗外跟手幽灵窗在");
        let back_inside = gpui::Point::new(start.x + px(120.), start.y + px(40.));
        cx.simulate_mouse_move(back_inside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let ghost_back = cx.update(|_w, cx| app.read(cx).drag_ghost.is_some());
        assert!(ghost_back, "回窗内同一幽灵窗仍在（无销毁重建）");
        // 窗内松开：本次拖拽结束、幽灵窗销毁
        cx.simulate_mouse_up(back_inside, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| app.read(cx).drag_ghost.is_none()),
            "拖拽结束幽灵窗应销毁"
        );
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            1,
            "窗内松开选卡原位保留"
        );
    }

    /// 回归（2026-10-28 用户真机复验）：鼠标离开窗口瞬间「屏上多出一个跟手选项卡、与原 tab
    /// 重叠」——根因 = gpui 每个窗口 prepaint 都会把 **App 级 `active_drag` picture** 画进
    /// **自己**的帧缓冲（window.rs prepaint：`cx.active_drag.take()` → 画进当前窗口）；本仓
    /// 拖拽全程的「窗外跟手幽灵窗」（PopUp 顶层窗）是独立窗口，它 prepaint 时 `active_drag`
    /// 仍为 Some → 把窗内拖拽 picture 又叠进幽灵窗，与其根卡片（`TabDragGhost ghost:true`）
    /// 同窗成两层 = 用户所见。修复：幽灵窗创建时不绘制 active_drag picture。
    ///
    /// headless 复现：拖拽开始（active_drag 在场）后 `ensure_drag_ghost` 创建幽灵窗，其首帧
    /// prepaint 会把 picture 画进幽灵窗 → 幽灵窗内 `tab-drag-ghost` 记录两条（根卡片 + 跟手
    /// picture，后者记录在最后）。断言：幽灵窗最后记录的 `tab-drag-ghost` 必须是**其自身根
    /// 卡片**（origin=(0,0)、尺寸=真实选项卡）——修复前是跟手 picture（origin≠0）→ FAIL。
    #[gpui::test]
    async fn drag_ghost_window_renders_only_its_own_card(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("gpui-ghost-single-card.txt");
        std::fs::write(&f, "单层跟手卡").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let tab = cx.debug_bounds("tab-1").expect("文件选项卡 tab-1 应渲染");
        let start = gpui::Point::new(
            tab.origin.x + px(10.),
            tab.origin.y + px(tab.size.height.as_f32() / 2.0),
        );
        // 按下 → 初次越阈值移动（建立 App 级 active_drag）→ 再移动触发 on_drag_move（建幽灵窗）
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(90.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        let ghost_handle = cx
            .update(|_w, cx| app.read(cx).drag_ghost)
            .expect("拖拽开始（窗内）即应建幽灵窗");
        let mut ghost_cx = gpui::VisualTestContext::from_window(ghost_handle, cx);
        let root = ghost_cx
            .debug_bounds("tab-drag-ghost")
            .expect("幽灵窗首帧应渲染其根卡片");
        assert_eq!(
            root.origin,
            gpui::Point::new(gpui::px(0.), gpui::px(0.)),
            "幽灵窗内最后记录的 tab-drag-ghost 必须是其自身根卡片（origin=0,0）；\
             修复前 gpui 会把 App 级 active_drag 跟手 picture 叠进幽灵窗（记录在根卡片之后，\
             origin≠0）=「窗外多出一张跟手选项卡」根因。root={root:?}"
        );
        assert!(
            (root.size.width.as_f32() - tab.size.width.as_f32()).abs() < 2.0
                && (root.size.height.as_f32() - tab.size.height.as_f32()).abs() < 2.0,
            "幽灵窗根卡片尺寸应等于真实选项卡：root={root:?} tab={tab:?}"
        );

        // 双侧一致之窗内侧：**源窗口**（普通窗口，不置抑制位）仍照常绘制窗内拖拽 picture——
        // 「其它窗口/窗内拖拽语义不受影响」。源窗口的 picture 画在 `鼠标-抓取偏移`（origin≠0）。
        let in_window_pic = cx
            .debug_bounds("tab-drag-ghost")
            .expect("源窗口仍应绘制窗内拖拽 picture（拖拽跟手）");
        assert!(
            in_window_pic.origin != gpui::Point::new(gpui::px(0.), gpui::px(0.))
                && (in_window_pic.size.width.as_f32() - tab.size.width.as_f32()).abs() < 2.0,
            "源窗口窗内拖拽 picture 应照常在（origin≠0、尺寸=真实选项卡）：pic={in_window_pic:?}",
        );

        // 双侧一致（原行为不变）：拖出窗外（幽灵可见）→ 窗外松开 → 仍正确拆出独立窗口，
        // 幽灵窗随拖拽结束销毁。
        cx.simulate_mouse_move(
            gpui::Point::new(px(-60.), px(50.)),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (armed, ghost_alive) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().drag_outside_armed, a.drag_ghost.is_some())
        });
        assert!(armed, "越出窗口未松手应置「松开才拆窗」预位");
        assert!(ghost_alive, "越出窗口跟手幽灵窗应在");
        cx.simulate_mouse_up(
            gpui::Point::new(px(-60.), px(50.)),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let snap = registry_snapshot(cx);
        assert_eq!(
            snap.iter().filter(|(d, _, _, _)| *d).count(),
            1,
            "窗外松开仍应拆出独立窗口，快照={snap:?}"
        );
        assert!(
            cx.update(|_w, cx| app.read(cx).drag_ghost.is_none()),
            "窗外松开后跟手幽灵窗应销毁"
        );
        let _ = std::fs::remove_file(&f);
    }

    /// 「提前切完整幽灵窗」几何判定（回归 2026-10 贴边预切）：窗内拖拽 picture（画在
    /// `鼠标 - 抓取偏移`，越出窗口帧缓冲部分被边缘裁掉）距窗口边缘 < `DRAG_EDGE_CLIP_MARGIN`
    /// （4px）即判「将要被遮断」→ 返回 true（提前显示同位置同尺寸完整幽灵窗）；远离边缘
    /// / 恰好贴边缘但未进入余量 = false。光标可在窗内而 picture 已探出边缘（抓取偏移大）。
    #[test]
    fn drag_picture_about_to_clip_geometry() {
        use shell_tabs::drag::{drag_picture_about_to_clip, DRAG_EDGE_CLIP_MARGIN};
        let win = gpui::size(px(980.), px(720.));
        let tab_size = gpui::size(px(120.), px(26.));
        let grab = gpui::Point::new(px(20.), px(13.));
        // 远离各边缘：picture 完整可见，false
        assert!(
            !drag_picture_about_to_clip(gpui::Point::new(px(400.), px(400.)), grab, tab_size, win),
            "窗口中央 picture 不应判为将要被遮断"
        );
        // 左缘：cursor 在 x=3（窗内），grab=20 → picture 原点=-17 < 4 → 提前切
        assert!(
            drag_picture_about_to_clip(gpui::Point::new(px(3.), px(400.)), grab, tab_size, win),
            "picture 探出左缘（光标仍在窗内）→ 应提前切幽灵窗"
        );
        // 左缘恰好 4px：origin=4，4<4 false；右缘未越 → 未遮断，不提前切
        assert!(
            !drag_picture_about_to_clip(gpui::Point::new(px(24.), px(400.)), grab, tab_size, win),
            "picture 距左缘恰 4px（未遮断）不应提前切换"
        );
        // 上缘同理：cursor y=3 → origin=-10 < 4
        assert!(
            drag_picture_about_to_clip(gpui::Point::new(px(400.), px(3.)), grab, tab_size, win),
            "picture 探出上缘 → 应提前切幽灵窗"
        );
        // 右缘：picture 右边 = pos.x - grab.x + w > win.w - 4。
        // pos.x=878 → origin=858, right=978 > 976 → true
        assert!(
            drag_picture_about_to_clip(gpui::Point::new(px(878.), px(400.)), grab, tab_size, win),
            "picture 距右缘 <4px → 应提前切幽灵窗"
        );
        // 右缘恰好 4px 外：pos.x=876 → origin=856, right=976 = 976 → false（未进入余量）
        assert!(
            !drag_picture_about_to_clip(gpui::Point::new(px(876.), px(400.)), grab, tab_size, win),
            "picture 距右缘恰 4px 不应提前切换"
        );
        // 下缘：pos.y=705 → origin=692, bottom=718 > 716 → true
        assert!(
            drag_picture_about_to_clip(gpui::Point::new(px(400.), px(705.)), grab, tab_size, win),
            "picture 距下缘 <4px → 应提前切幽灵窗"
        );
        // 余量常量必须 > 0（0 会退回「贴边才切」，仍可能露出被裁的一帧）。
        assert!(DRAG_EDGE_CLIP_MARGIN > 0.0, "边缘预切余量应为正");
    }

    /// 回归（2026-10「贴边提前切幽灵窗」）：拖拽 picture 靠近窗口边缘（光标仍在窗内）即
    /// 判「将遮断」→ 提前切完整幽灵窗（`drag_ghost_visible=true`），而「松开才建窗」预位
    /// （`drag_outside_armed`）仍保持 false（光标未越界 → 窗内松开不拆窗）；拖回窗内中央
    /// → 幽灵窗隐藏（窗内用原 gpui picture）；越出窗口 → 预位 + 幽灵窗同时显示。
    #[gpui::test]
    async fn drag_tab_near_edge_pre_shows_complete_ghost(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let tab = cx.debug_bounds("tab-0").expect("选项卡 tab-0 应渲染");
        let start = gpui::Point::new(
            tab.origin.x + px(10.),
            tab.origin.y + px(tab.size.height.as_f32() / 2.0),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        // 拖到窗口左缘附近（光标 x=2 仍在窗内）：picture 左侧（pos - grab）探出左缘 →
        // 提前切完整幽灵窗；但光标未越界 → 「松开才建窗」预位仍 false。
        let near_left = gpui::Point::new(px(2.), start.y);
        cx.simulate_mouse_move(near_left, MouseButton::Left, Default::default());
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let (ghost_visible, armed) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().drag_ghost_visible, a.model.borrow().drag_outside_armed)
        });
        assert!(ghost_visible, "picture 贴左缘（光标仍在窗内）应提前显示完整幽灵窗");
        assert!(!armed, "光标仍在窗内：不应置「松开才建窗」预位");
        // 拖回窗口中央：幽灵窗应隐藏（窗内用原 gpui picture，不叠浮窗）。
        let center = gpui::Point::new(start.x + px(120.), start.y + px(40.));
        cx.simulate_mouse_move(center, MouseButton::Left, Default::default());
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let ghost_center = cx.update(|_w, cx| app.read(cx).model.borrow().drag_ghost_visible);
        assert!(!ghost_center, "拖回窗内中央：幽灵窗应隐藏（用窗内 picture）");
        // 越出窗口 → 预位 + 幽灵窗同时显示
        cx.simulate_mouse_move(
            gpui::Point::new(px(-60.), px(50.)),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let (armed_out, visible_out) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().drag_outside_armed, a.model.borrow().drag_ghost_visible)
        });
        assert!(armed_out, "越出窗口应置「松开才建窗」预位");
        assert!(visible_out, "越出窗口幽灵窗应显示");
        // 窗内松开：取消拖拽（幽灵窗随拖拽结束销毁），选项卡原位保留
        cx.simulate_mouse_up(center, MouseButton::Left, Default::default());
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (ghost_gone, tab_count) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.drag_ghost.is_none(), a.model.borrow().tabs.len())
        });
        assert!(ghost_gone, "窗内松开幽灵窗应销毁");
        assert_eq!(tab_count, 1, "窗内松开选卡应原位保留");
    }

    /// 大文档交互延迟基准（`BIGDOC_BENCH=1` 才跑，不打断言，只打印每操作墙钟）。
    /// 用 `fixtures/大文档测试.rs`（编辑器）与 `fixtures/大文档测试2.md`（预览）实测
    /// 「输入字符 / 滚动」的单次感知延迟（`dispatch/simulate + run_until_parked` 墙钟）。
    /// 对比 HEAD：`git stash -u` 跑本测试，再 `git stash pop` 复跑。
    #[gpui::test]
    #[ignore]
    async fn bench_bigdoc_interactive(cx: &mut TestAppContext) {
        if std::env::var_os("BIGDOC_BENCH").is_none() {
            return;
        }
        init_test(cx);
        // 对齐生产：启动后台预热语法集/缓存（否则首次各语言代码块冷扫 ~200-250ms，
        // 会污染「稳态滚动/编辑」测量——生产里 prewarm 在 main.rs 后台线程早已完成）。
        syntax::prewarm();
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let show = |kind: &str, ms: f64| {
            eprintln!("[BENCH] {kind} = {ms:.1} ms");
        };
        let md_path =
            std::env::var("BIGDOC_MD").unwrap_or_else(|_| "../../fixtures/大文档测试2.md".to_string());
        let rs_path =
            std::env::var("BIGDOC_RS").unwrap_or_else(|_| "../../fixtures/大文档测试.rs".to_string());

        // ---------- 打开 .rs（代码编辑器）并等就绪 ----------
        let t0 = std::time::Instant::now();
        let rs_path = rs_path.clone();
        cx.update(|window, cx| {
            app.update(cx, |a, cx| {
                a.open_file(&rs_path, window, cx);
            });
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.model.borrow().tabs.len().saturating_sub(1);
                !a.model.borrow().tabs[i].payload.editor_pending
            });
            if ready {
                break;
            }
            frames += 1;
            if frames > 600 {
                panic!("编辑器打开超时");
            }
        }
        show("open_rust_file", t0.elapsed().as_secs_f64() * 1000.0);

        let editor = cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
            app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone()
        });

        // ---------- 编辑器输入（insert_text × 6，拆「insert」与「draw/渲染」） ----------
        let sample = "x";
        for k in 0..6 {
            let t0 = std::time::Instant::now();
            cx.update(|_w, cx| editor.update(cx, |e, cx| e.insert_text(sample, cx)));
            let t1 = std::time::Instant::now();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let t2 = std::time::Instant::now();
            show(
                &format!("editor_ins_{k}"),
                t2.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  editor_ins_{k}.ins"),
                t1.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  editor_ins_{k}.draw"),
                t2.duration_since(t1).as_secs_f64() * 1000.0,
            );
        }
        crate::prof::dump("editor_insert_text");

        // ---------- 编辑器滚动（滚轮下滚，拆「事件」与「渲染」） ----------
        for k in 0..6 {
            let t0 = std::time::Instant::now();
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::Point::new(px(700.), px(400.)),
                delta: gpui::ScrollDelta::Pixels(gpui::Point::new(px(0.), px(-120.))),
                modifiers: gpui::Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            let t1 = std::time::Instant::now();
            for _ in 0..4 {
                cx.run_until_parked();
            }
            let t2 = std::time::Instant::now();
            show(
                &format!("editor_scroll_{k}"),
                t2.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  editor_scroll_{k}.evt"),
                t1.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  editor_scroll_{k}.parks"),
                t2.duration_since(t1).as_secs_f64() * 1000.0,
            );
        }
        crate::prof::dump("editor_scroll");

        // ---------- 打开 .md（预览）并等两段就绪 ----------
        let t1 = std::time::Instant::now();
        let md_path = md_path.clone();
        cx.update(|window, cx| {
            app.update(cx, |a, cx| {
                a.open_file(&md_path, window, cx);
            });
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.model.borrow().tabs.len().saturating_sub(1);
                !a.model.borrow().tabs[i].payload.editor_pending
                    && a.model.borrow().tabs[i]
                        .payload.md_wy
                        .as_ref()
                        .map(|w| w.read(cx).is_loaded())
                        .unwrap_or(false)
            });
            if ready {
                break;
            }
            frames += 1;
            if frames > 900 {
                panic!("预览打开超时");
            }
        }
        show("open_md_file", t1.elapsed().as_secs_f64() * 1000.0);

        // ---------- 预览滚动（预览在右半区，拆「事件」与「渲染」） ----------
        for k in 0..6 {
            let t0 = std::time::Instant::now();
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::Point::new(px(900.), px(400.)),
                delta: gpui::ScrollDelta::Pixels(gpui::Point::new(px(0.), px(-120.))),
                modifiers: gpui::Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            let t1 = std::time::Instant::now();
            for _ in 0..4 {
                cx.run_until_parked();
            }
            let t2 = std::time::Instant::now();
            show(
                &format!("preview_scroll_{k}"),
                t2.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  preview_scroll_{k}.evt"),
                t1.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  preview_scroll_{k}.parks"),
                t2.duration_since(t1).as_secs_f64() * 1000.0,
            );
        }
        crate::prof::dump("preview_scroll");

        // ---------- 预览编辑区输入（md 编辑区 insert_text，拆 ins/draw） ----------
        let md_editor = cx.update(|_, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].payload.editor.clone());
        for k in 0..4 {
            let t0 = std::time::Instant::now();
            cx.update(|_, cx| md_editor.update(cx, |e, cx| e.insert_text("y", cx)));
            let t1 = std::time::Instant::now();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let t2 = std::time::Instant::now();
            show(
                &format!("md_editor_ins_{k}"),
                t2.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  md_editor_ins_{k}.ins"),
                t1.duration_since(t0).as_secs_f64() * 1000.0,
            );
            show(
                &format!("  md_editor_ins_{k}.draw"),
                t2.duration_since(t1).as_secs_f64() * 1000.0,
            );
        }
        crate::prof::dump("md_editor_insert_text");
    }

    /// 回归（用户实测 2026-10-08）：真实大文档分屏下，鼠标滚轮在**预览**上快速下滚，
    /// 滚到第一个表格块（块 99 附近）时预览「上来回跳动、滚不下去」（宿主偏移被反复
    /// 拽回 ~93px）——根因 = sync 把「编辑器跟随预览的落位」误判成「用户滚动编辑器」，
    /// 反向驱动预览（editor→preview 回声），每帧重钉预览偏移回编辑器所在块。
    ///
    /// 修复 = `MdScrollSync` 新增 `driven_editor_target` 对称护栏（+ 宽限计数）：我方
    /// 驱动编辑器落位的宽限期内，编辑器顶块变化不回显为「编辑器先动」，预览不再被拽回。
    ///
    /// 断言：预览偏移单调前进（回退 >2px 计为 bounce，且必须为 0），并能滚过表格块
    /// （修复前卡在 ~2607px，max_off 应 >3200），且最终两侧对齐（E 顶行 = 预览顶块）。
    #[gpui::test]
    async fn preview_wheel_sync_no_bounce_past_table(cx: &mut TestAppContext) {
        init_test(cx);
        syntax::prewarm();
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 大文档滚轮夹具守卫：真实档 `fixtures/大文档测试2.md`（6.4MB）不入库，本机无档即
        // SKIP（CI/装好档的机器真跑）；BIGDOC_MD 指向真实档可强制跑本回归。
        // TODO(内存构造)：曾尝试用 ~2 万行合成 md（含宽表格）替代，但 H2 断言（编辑器顶行
        // 严格单调）对合成档的块粒度/行高几何高度敏感——「编辑器微滚进一个块内 → sync 把
        // 编辑器拉回块起点」会让顶行读数回跳，只有真实档的块布局才稳定通过 H2；故回退守卫。
        let md_path = std::env::var("BIGDOC_MD")
            .unwrap_or_else(|_| "../../fixtures/大文档测试2.md".to_string());
        if !std::path::Path::new(&md_path).exists() {
            eprintln!("SKIP: missing {md_path}（6.4MB 大文档不入库）；置 BIGDOC_MD 指真实档可跑本回归");
            return;
        }
        let md_path = md_path.clone();
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.open_file(&md_path, window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                !a.model.borrow().tabs[a.model.borrow().tabs.len().saturating_sub(1)].payload.editor_pending
            });
            if ready {
                break;
            }
            frames += 1;
            if frames > 1500 {
                return;
            }
        }
        let (editor, mdw, vhand) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.model.borrow().active_tab;
            (
                a.model.borrow().tabs[i].payload.editor.clone(),
                a.model.borrow().tabs[i].payload.md_wy.clone().expect("预览应在场"),
                a.model.borrow().tabs[i].payload.md_wy_vscroll.clone(),
            )
        });
        let read = |cx: &mut VisualTestContext| -> (usize, Option<usize>, f32) {
            cx.update(|_, cx| {
                let et = editor.read(cx).viewport_top_line();
                let ps = mdw.read(cx).top_source_line();
                let off = (-vhand.offset().y).as_f32();
                (et, ps, off)
            })
        };
        let mut guard = 0;
        while read(cx).1.is_none() {
            let _ = cx.cx.refresh();
            guard += 1;
            if guard > 400 {
                return;
            }
        }

        // 预览区滚轮下滚 30 次，逐帧检查：预览偏移不得回退 >2px（一旦前进了 >50px）；
        // 且必须滚过第一个表格块（修复前 max_off 恒卡在 ~2607 / 来回跳动）。
        let mut max_off = read(cx).2;
        let mut bounced = 0usize;
        let mut max_regress = 0f32;
        for _ in 0..30 {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::Point::new(px(900.), px(400.)),
                delta: gpui::ScrollDelta::Pixels(gpui::Point::new(px(0.), px(-150.))),
                modifiers: gpui::Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            for _ in 0..4 {
                let (_et, _ps, off) = read(cx);
                if off < max_off - 2.0 && max_off > 50.0 {
                    bounced += 1;
                    max_regress = (max_off - off).max(max_regress);
                }
                max_off = max_off.max(off);
                let _ = cx.cx.refresh();
            }
        }
        assert_eq!(
            bounced, 0,
            "预览滚轮不得回跳（bounce={bounced}，最大回退 {max_regress:.1}px；修复前每步"
        );
        assert!(
            max_off > 3200.0,
            "预览应滚过第一个表格块（max_off={max_off:.1}px；修复前卡在 ~2607px）"
        );
        let (et, ps, _off) = read(cx);
        let ps = ps.expect("预览帧应就绪");
        assert_eq!(
            et, ps,
            "最终两侧应对齐到同一块（editor top={et}, preview block={ps}）"
        );

        // ===== 面板交接（用户实测 2026-10-08 追加）：编辑区↔预览区**切着滚**不得跳动 =====
        // 修复前：编辑区滚后切预览区滚 → 预览 off 在表格块处 2606.9↔2756.9 来回拽
        // （46 次回跳）；预览区滚后切编辑区滚 → sync 彻底卡死（编辑器再也滚不动）。
        let wheel_at = |x: f32, cx: &mut VisualTestContext| {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::Point::new(px(x), px(400.)),
                delta: gpui::ScrollDelta::Pixels(gpui::Point::new(px(0.), px(-150.))),
                modifiers: gpui::Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
        };

        // -- H1) 编辑区滚 → 切预览区滚：预览 off 单调前进 --
        for _ in 0..8 {
            wheel_at(700., cx);
            for _ in 0..2 {
                let _ = cx.cx.refresh();
            }
        }
        let mut max_off1 = read(cx).2;
        let mut h1 = 0usize;
        for _ in 0..15 {
            wheel_at(900., cx);
            for _ in 0..3 {
                let (_, _ps, off) = read(cx);
                if off < max_off1 - 3.0 && max_off1 > 50.0 {
                    h1 += 1;
                }
                max_off1 = max_off1.max(off);
                let _ = cx.cx.refresh();
            }
        }
        assert_eq!(
            h1, 0,
            "编辑区滚后切预览区滚，预览不得回跳（修复前 46 次）"
        );

        // -- H2) 预览区滚 → 切编辑区滚：编辑器顶行单调前进（且不得被 sync 卡死） --
        for _ in 0..8 {
            wheel_at(900., cx);
            for _ in 0..2 {
                let _ = cx.cx.refresh();
            }
        }
        let mut max_et = read(cx).0;
        let mut h2 = 0usize;
        for _ in 0..15 {
            wheel_at(700., cx);
            for _ in 0..3 {
                let (et, _ps, _off) = read(cx);
                if et < max_et && max_et > 10 {
                    h2 += 1;
                }
                max_et = max_et.max(et);
                let _ = cx.cx.refresh();
            }
        }
        assert_eq!(
            h2, 0,
            "预览区滚后切编辑区滚，编辑器顶行不得回跳且不得锁死（修复前卡死 max_et=98）"
        );
        eprintln!(
            "[regression] preview_wheel_sync_no_bounce_past_table OK: max_off={max_off:.1} H1={h1} H2={h2} max_et={max_et}"
        );
    }

    /// 诊断（#[ignore]）：真实 Windows 滚轮口径（Lines ±3）+ 1 帧切换 × 「多滚后换区」模式，
    /// 逐帧检查「编辑区→预览区」「预览区→编辑区」交接处 E/off 是否回跳（修复前用户实测）。
    #[gpui::test]
    #[ignore]
    async fn diag_handoff2(cx: &mut TestAppContext) {
        init_test(cx);
        syntax::prewarm();
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md_path = std::env::var("BIGDOC_MD")
            .unwrap_or_else(|_| "../../fixtures/大文档测试2.md".to_string());
        let md_path = md_path.clone();
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.open_file(&md_path, window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                !a.model.borrow().tabs[a.model.borrow().tabs.len().saturating_sub(1)].payload.editor_pending
            });
            if ready {
                break;
            }
            frames += 1;
            if frames > 1500 {
                return;
            }
        }
        let (editor, mdw, vhand) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.model.borrow().active_tab;
            (
                a.model.borrow().tabs[i].payload.editor.clone(),
                a.model.borrow().tabs[i].payload.md_wy.clone().expect("预览应在场"),
                a.model.borrow().tabs[i].payload.md_wy_vscroll.clone(),
            )
        });
        let read = |cx: &mut VisualTestContext| -> (usize, Option<usize>, f32) {
            cx.update(|_, cx| {
                let et = editor.read(cx).viewport_top_line();
                let ps = mdw.read(cx).top_source_line();
                let off = (-vhand.offset().y).as_f32();
                (et, ps, off)
            })
        };
        let mut guard = 0;
        while read(cx).1.is_none() {
            let _ = cx.cx.refresh();
            guard += 1;
            if guard > 400 {
                return;
            }
        }
        let wheel_lines = |x: f32, cx: &mut VisualTestContext| {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: gpui::Point::new(px(x), px(400.)),
                delta: gpui::ScrollDelta::Lines(gpui::Point::new(0.0, -3.0)),
                modifiers: gpui::Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
        };
        let mut max_off = read(cx).2;
        let mut max_et = read(cx).0;
        let mut bounce = 0usize;
        let seq = [('E', 12), ('P', 12), ('E', 12), ('P', 12), ('E', 12), ('P', 12)];
        let mut step = 0usize;
        for (which, n) in seq.into_iter() {
            for k in 0..n {
                let x = if which == 'E' { 700. } else { 900. };
                wheel_lines(x, cx);
                // 1 帧即刻读（不沉淀），模拟“切过去马上滚”
                let (et, ps, off) = read(cx);
                if off < max_off - 3.0 && max_off > 50.0 {
                    bounce += 1;
                    eprintln!(
                        "[h2] !!OFF-BOUNCE seq={step} {which}{k} off={off:.1} <- max={max_off:.1} E={et} P={ps:?}"
                    );
                }
                if et < max_et && max_et > 10 {
                    bounce += 1;
                    eprintln!(
                        "[h2] !!ET-BOUNCE seq={step} {which}{k} E={et} <- maxE={max_et} P={ps:?} off={off:.1}"
                    );
                }
                max_off = max_off.max(off);
                max_et = max_et.max(et);
                let _ = cx.cx.refresh();
            }
            step += 1;
            let (et, ps, off) = read(cx);
            eprintln!(
                "[h2] seq#{step}({which} x{n}) E={et} P={ps:?} off={off:.1} bounces={bounce}"
            );
        }
        eprintln!("[h2] DONE bounces={bounce} max_off={max_off:.1} max_et={max_et}");
    }

    // ---------------------------------------------------------------------------
    // 选项卡栏内拖拽排序（松手插入 + 白色插入指示线）
    // ---------------------------------------------------------------------------

    /// 读取窗口选项卡顺序（文件名；未命名 = None）。
    fn tab_order(
        app: &gpui::Entity<EditorApp>,
        cx: &mut VisualTestContext,
    ) -> Vec<Option<String>> {
        cx.update(|_w, cx| {
            app.read(cx)
                .model.borrow().tabs
                .iter()
                .map(|t| {
                    t.payload.file_path
                        .as_ref()
                        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                })
                .collect()
        })
    }

    /// 准备「未命名 + A + B」三选项卡；返回各 tab 的 window 边界（供定位）。
    fn add_three_tabs(
        app: &gpui::Entity<EditorApp>,
        cx: &mut VisualTestContext,
    ) -> (gpui::Bounds<gpui::Pixels>, gpui::Bounds<gpui::Pixels>, gpui::Bounds<gpui::Pixels>) {
        let f1 = std::env::temp_dir().join("gpui-reorder-a.txt");
        let f2 = std::env::temp_dir().join("gpui-reorder-b.txt");
        std::fs::write(&f1, "AAA").unwrap();
        std::fs::write(&f2, "BBB").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f1.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f2.to_str().unwrap(), window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
        }
        let b0 = cx.debug_bounds("tab-0").expect("tab-0 应渲染");
        let b1 = cx.debug_bounds("tab-1").expect("tab-1 应渲染");
        let b2 = cx.debug_bounds("tab-2").expect("tab-2 应渲染");
        let _ = std::fs::remove_file(&f1);
        let _ = std::fs::remove_file(&f2);
        (b0, b1, b2)
    }

    /// 把 tab `from` 拖到 `drop`（窗口坐标）处松开（全程窗内）。
    async fn drag_tab_release_at(
        cx: &mut VisualTestContext,
        start: gpui::Point<gpui::Pixels>,
        drop: gpui::Point<gpui::Pixels>,
    ) {
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        cx.simulate_mouse_move(drop, MouseButton::Left, Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.simulate_mouse_up(drop, MouseButton::Left, Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
        }
    }

    /// 拖 B（tab-2）到 A（tab-1）左半松开 → 插入到 A 之前：[未命名, B, A]。
    #[gpui::test]
    async fn tab_reorder_drop_on_left_half_inserts_before(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1280.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (b0, b1, b2) = add_three_tabs(&app, cx);
        let _ = b0;
        let start = gpui::Point::new(
            b2.origin.x + px(10.),
            b2.origin.y + px(b2.size.height.as_f32() / 2.0),
        );
        // A 的左半（x 取 A 左缘+4，避开与未命名 tab 的 4px 间隙误判）
        let drop = gpui::Point::new(b1.origin.x + px(4.), b1.origin.y + px(b1.size.height.as_f32() / 2.0));
        drag_tab_release_at(cx, start, drop).await;
        assert_eq!(
            tab_order(&app, cx),
            vec![None, Some("gpui-reorder-b.txt".into()), Some("gpui-reorder-a.txt".into())],
            "拖到 A 左半应插到 A 之前"
        );
        // 不拆出独立窗口
        assert_eq!(
            registry_snapshot(cx).iter().filter(|(d, _, _, _)| *d).count(),
            0,
            "栏内松手不应拆窗"
        );
    }

    /// 拖 未命名（tab-0）到 A（tab-1）右半松开 → 插入到 A 之后：[A, 未命名, B]。
    #[gpui::test]
    async fn tab_reorder_drop_on_right_half_inserts_after(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1280.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (b0, b1, b2) = add_three_tabs(&app, cx);
        let _ = b2;
        let start = gpui::Point::new(
            b0.origin.x + px(10.),
            b0.origin.y + px(b0.size.height.as_f32() / 2.0),
        );
        let drop = gpui::Point::new(
            b1.origin.x + px(b1.size.width.as_f32() * 0.75),
            b1.origin.y + px(b1.size.height.as_f32() / 2.0),
        );
        drag_tab_release_at(cx, start, drop).await;
        assert_eq!(
            tab_order(&app, cx),
            vec![
                Some("gpui-reorder-a.txt".into()),
                None,
                Some("gpui-reorder-b.txt".into())
            ],
            "拖到 A 右半应插到 A 之后"
        );
    }

    /// 拖 B（tab-2）到最左（未命名左侧空白）松开 → 插到最前面：[B, 未命名, A]。
    #[gpui::test]
    async fn tab_reorder_drag_to_bar_start_moves_to_front(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1280.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (b0, b1, b2) = add_three_tabs(&app, cx);
        let _ = b1;
        let start = gpui::Point::new(
            b2.origin.x + px(10.),
            b2.origin.y + px(b2.size.height.as_f32() / 2.0),
        );
        let drop = gpui::Point::new(
            b0.origin.x - px(20.),
            b0.origin.y + px(b0.size.height.as_f32() / 2.0),
        );
        drag_tab_release_at(cx, start, drop).await;
        assert_eq!(
            tab_order(&app, cx),
            vec![
                Some("gpui-reorder-b.txt".into()),
                None,
                Some("gpui-reorder-a.txt".into())
            ],
            "拖到最左应插到最前"
        );
    }

    /// 排序后活动选项卡跟随被拖项，且各选项卡内容（编辑器正文）不变、不拆窗。
    #[gpui::test]
    async fn tab_reorder_preserves_active_and_editor(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1280.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (b0, _b1, b2) = add_three_tabs(&app, cx);
        let _ = b0;
        // 先激活文件 B（点 tab-2），随后拖 B 到 A 左半 → active 应跟到 B 新下标
        let start = gpui::Point::new(
            b2.origin.x + px(10.),
            b2.origin.y + px(b2.size.height.as_f32() / 2.0),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_up(start, MouseButton::Left, Default::default());
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let a_bounds = cx.debug_bounds("tab-1").expect("A 应渲染");
        let drop = gpui::Point::new(
            a_bounds.origin.x + px(4.),
            a_bounds.origin.y + px(a_bounds.size.height.as_f32() / 2.0),
        );
        drag_tab_release_at(cx, start, drop).await;
        let order = tab_order(&app, cx);
        let (active, body) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().active_tab,
                a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.read(cx).value().to_string(),
            )
        });
        assert_eq!(
            order,
            vec![None, Some("gpui-reorder-b.txt".into()), Some("gpui-reorder-a.txt".into())]
        );
        assert_eq!(active, 1, "活动选项卡应跟随被拖的 B 到新下标");
        assert_eq!(body, "BBB", "被拖选项卡正文不变");
        assert_eq!(
            registry_snapshot(cx).iter().filter(|(d, _, _, _)| *d).count(),
            0,
            "栏内松手不应拆窗"
        );
    }

    /// 窗内拖拽但松开时落在选项卡行外（编辑区上方空白）→ 不排序、原位保留。
    #[gpui::test]
    async fn tab_reorder_release_outside_row_keeps_order(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1280.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (b0, b1, b2) = add_three_tabs(&app, cx);
        let _ = b1;
        let start = gpui::Point::new(
            b2.origin.x + px(10.),
            b2.origin.y + px(b2.size.height.as_f32() / 2.0),
        );
        // 拖到选项卡行下方（仍在窗口内）→ 不在行内 → 无插入位，原位保留
        let drop = gpui::Point::new(
            b2.origin.x + px(60.),
            b0.origin.y + px(b0.size.height.as_f32() + 30.),
        );
        drag_tab_release_at(cx, start, drop).await;
        assert_eq!(
            tab_order(&app, cx),
            vec![None, Some("gpui-reorder-a.txt".into()), Some("gpui-reorder-b.txt".into())],
            "行外松开不应排序"
        );
    }

    /// 拖拽悬停在目标左/右边界时渲染白色插入指示线；松手后消失。
    #[gpui::test]
    async fn tab_reorder_line_shown_on_target_then_hidden_after_release(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1280.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (_b0, b1, b2) = add_three_tabs(&app, cx);
        let start = gpui::Point::new(
            b2.origin.x + px(10.),
            b2.origin.y + px(b2.size.height.as_f32() / 2.0),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        // 悬停在 A 左半 → 指示线应渲染
        let drop = gpui::Point::new(
            b1.origin.x + px(4.),
            b1.origin.y + px(b1.size.height.as_f32() / 2.0),
        );
        cx.simulate_mouse_move(drop, MouseButton::Left, Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 拖拽中：光标命中行内目标 → tab_reorder_target 有值 + 指示线渲染
        let target = cx.update(|_w, cx| app.read(cx).model.borrow().tab_reorder_target);
        let line = cx.debug_bounds("tab-reorder-line");
        assert!(target.is_some(), "拖拽悬停目标应有插入位");
        assert!(line.is_some(), "拖拽悬停目标应渲染白色插入指示线 line={line:?}");
        // 松开 → 指示线消失
        cx.simulate_mouse_up(drop, MouseButton::Left, Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
        }
        let line_after = cx.debug_bounds("tab-reorder-line");
        let target_after = cx.update(|_w, cx| app.read(cx).model.borrow().tab_reorder_target);
        assert!(line_after.is_none(), "松手后指示线应消失");
        assert!(target_after.is_none(), "松手后插入位应清空");
    }

    /// 新触发口径：只要**被拖选项卡的边缘**进入选项卡行（鼠标光标可仍在行外/行下方），
    /// 白色插入指示线即出现；松手按光标 x 落地插入。
    #[gpui::test]
    async fn tab_reorder_line_appears_when_dragged_edge_enters_row(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1280.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (_b0, b1, b2) = add_three_tabs(&app, cx);
        let start = gpui::Point::new(
            b2.origin.x + px(10.),
            b2.origin.y + px(b2.size.height.as_f32() / 2.0),
        );
        // 目标投放点：x 落在 A（tab-1）左半；y 在选项卡行**下方**（row_bottom+10），
        // 但被拖选项卡盒（= 鼠标 - 抓取偏移，半高约 13px）的上缘仍进入行内 → 白线应出现。
        let drop = gpui::Point::new(
            b1.origin.x + px(4.),
            b1.bottom_right().y + px(10.),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(
            gpui::Point::new(start.x + px(40.), start.y),
            MouseButton::Left,
            Default::default(),
        );
        for _ in 0..2 {
            cx.run_until_parked();
        }
        cx.simulate_mouse_move(drop, MouseButton::Left, Default::default());
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let target = cx.update(|_w, cx| app.read(cx).model.borrow().tab_reorder_target);
        let line = cx.debug_bounds("tab-reorder-line");
        assert!(target.is_some(), "被拖选项卡边缘入行即应有插入位 target={target:?}");
        assert!(line.is_some(), "被拖选项卡边缘入行即应出现白线");
        // 松手 → 按光标 x 落地插入：[未命名, B, A]
        cx.simulate_mouse_up(drop, MouseButton::Left, Default::default());
        for _ in 0..6 {
            cx.run_until_parked();
        }
        assert_eq!(
            tab_order(&app, cx),
            vec![None, Some("gpui-reorder-b.txt".into()), Some("gpui-reorder-a.txt".into())],
            "选项卡边缘入行→松手应插到 A（tab-1）之前：[未命名, B, A]"
        );
    }

