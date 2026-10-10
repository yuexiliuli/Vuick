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
                        let tab = a.tabs.get(a.active_tab);
                        (
                            a.is_detached,
                            a.tabs.len(),
                            tab.and_then(|t| t.file_path.clone()).is_some(),
                            tab.map(|t| t.editor.read(c).value()),
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
        let editor = cx.update(|_w, cx| app.read(cx).tabs[1].editor.clone());
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
            let t = &a.tabs[a.active_tab];
            (
                t.editor.read(cx).value(),
                t.dirty,
                a.tabs.len(),
                t.file_path.is_some(),
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
        // 回归：应在光标刚越过窗口边缘（尚未松手）时立即 tear-off 出独立窗口，
        // 而不是等到窗口外松开才突然弹窗。
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
        let before_up = registry_snapshot(cx);
        assert_eq!(
            before_up.iter().filter(|(d, _, _, _)| *d).count(),
            1,
            "越出窗口边缘（未松手）就应已拆出独立窗口，快照={before_up:?}"
        );
        assert!(
            before_up
                .iter()
                .find(|(d, _, _, _)| *d)
                .is_some_and(|(_, n, has_path, text)| {
                    *n == 1 && *has_path && text.as_deref() == Some("AAA 独立窗口")
                }),
            "越出边缘时应已把选项卡放进新窗口，快照={before_up:?}"
        );

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
            crate::ui::root::Root::new(app, window, cx)
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
            cx.update(|_w, cx| det.read_with(cx, |a, _| a.tabs.len())),
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
            let tab = a.tabs.iter().find(|t| t.file_path.is_some());
            (
                tab.is_some(),
                tab.map(|t| t.editor.read(cx).value().to_string()),
            )
        });
        assert!(has_file, "主窗口应重新持有该文件选项卡");
        assert_eq!(content.as_deref(), Some("BBB 还原"), "正文应完整还原");
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
            cx.update(|_w, cx| app.read(cx).tabs.len()),
            1,
            "主窗口选项卡应保持原样"
        );
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
        crate::syntax::prewarm();
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let show = |kind: &str, ms: f64| {
            eprintln!("[BENCH] {kind} = {ms:.1} ms");
        };
        let md_path =
            std::env::var("BIGDOC_MD").unwrap_or_else(|_| "fixtures/大文档测试2.md".to_string());
        let rs_path =
            std::env::var("BIGDOC_RS").unwrap_or_else(|_| "fixtures/大文档测试.rs".to_string());

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
                let i = a.tabs.len().saturating_sub(1);
                !a.tabs[i].editor_pending
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
            app.read(cx).tabs[app.read(cx).active_tab].editor.clone()
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
                let i = a.tabs.len().saturating_sub(1);
                !a.tabs[i].editor_pending
                    && a.tabs[i].md_preview.read(cx).memory_estimate().0 > 0
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
        let md_editor = cx.update(|_, cx| app.read(cx).tabs[app.read(cx).active_tab].editor.clone());
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
        crate::syntax::prewarm();
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md_path = std::env::var("BIGDOC_MD")
            .unwrap_or_else(|_| "fixtures/大文档测试2.md".to_string());
        let md_path = md_path.clone();
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.open_file(&md_path, window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                !a.tabs[a.tabs.len().saturating_sub(1)].editor_pending
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
            let i = a.active_tab;
            (
                a.tabs[i].editor.clone(),
                a.tabs[i].md_wy.clone().expect("预览应在场"),
                a.tabs[i].md_wy_vscroll.clone(),
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
        crate::syntax::prewarm();
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let md_path = std::env::var("BIGDOC_MD")
            .unwrap_or_else(|_| "fixtures/大文档测试2.md".to_string());
        let md_path = md_path.clone();
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.open_file(&md_path, window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                !a.tabs[a.tabs.len().saturating_sub(1)].editor_pending
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
            let i = a.active_tab;
            (
                a.tabs[i].editor.clone(),
                a.tabs[i].md_wy.clone().expect("预览应在场"),
                a.tabs[i].md_wy_vscroll.clone(),
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

