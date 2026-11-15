    // ---------------------------------------------------------------------------
    // 选项卡右键管理菜单（关闭组 / 移入·复制新窗口）+ 空白区右键分离 回归
    // ---------------------------------------------------------------------------

    /// 右键选项卡 → 选项卡管理菜单：7 项 + 1 分割线（layout_probe (8,1)），全部新
    /// debug_selector 存在；**不**弹出栏级「新建/打开」菜单（stop_propagation + 复用
    /// 同一 overlay/菜单槽位、以条目区分）。
    #[gpui::test]
    async fn tab_right_click_menu_structure_and_no_bar_menu(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let tab = cx.debug_bounds("tab-0").expect("初始选项卡应渲染");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: tab.center(),
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
                .tab_bar_menu_entity
                .as_ref()
                .expect("选项卡菜单应打开")
                .read(cx)
                .layout_probe()
        });
        assert_eq!(
            probe,
            (8, 1),
            "选项卡菜单 = 7 项 + 1 分割线（总条目 8），实际={probe:?}"
        );
        for sel in [
            "tab-menu-close",
            "tab-menu-close-others",
            "tab-menu-close-right",
            "tab-menu-close-saved",
            "tab-menu-close-all",
            "tab-menu-move-new-window",
            "tab-menu-copy-new-window",
        ] {
            assert!(cx.debug_bounds(sel).is_some(), "选项卡菜单应含 {sel}");
        }
        // 分隔线分组内的关闭与窗口组都呈现；栏级「新建/打开」**不**出现。
        assert!(
            cx.debug_bounds("tab-bar-menu-item-new").is_none()
                && cx.debug_bounds("tab-bar-menu-item-open").is_none(),
            "右键选项卡不应冒泡到栏级新建/打开菜单"
        );
        cx.simulate_keystrokes("escape");
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 回归：右键选项卡栏**空白区**仍是既有「新建/打开」菜单（语义不动）。
        let bar = cx.debug_bounds("tab-bar-tabs").expect("选项卡栏应渲染");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: gpui::Point::new(bar.right() - px(30.), bar.center().y),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.debug_bounds("tab-bar-menu-item-new").is_some()
                && cx.debug_bounds("tab-bar-menu-item-open").is_some(),
            "选项卡栏空白区右键仍应弹「新建/打开」菜单"
        );
        assert!(
            cx.debug_bounds("tab-menu-close").is_none(),
            "空白区右键不应弹选项卡管理菜单"
        );
        cx.simulate_keystrokes("escape");
        for _ in 0..3 {
            cx.run_until_parked();
        }
    }

    /// 「关闭全部」：全干净 → 批量关干净（零选项卡欢迎页）；任一脏/未命名 → 中止
    /// 一个不关 + 状态栏提示（headless：只断言 remove 未发生与提示）。
    #[gpui::test]
    async fn tab_close_all_clean_vs_dirty_unnamed_abort(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f1 = std::env::temp_dir().join("gpui-tc-1.txt");
        let f2 = std::env::temp_dir().join("gpui-tc-2.txt");
        let f3 = std::env::temp_dir().join("gpui-tc-3.txt");
        for f in [&f1, &f2, &f3] {
            std::fs::write(f, "AAA").unwrap();
        }
        // 干净路径：3 个命名文件 tab（全干净）→ 关闭全部清空 → 欢迎页。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(f1.to_str().unwrap(), window, cx);
                app.open_file(f2.to_str().unwrap(), window, cx);
                app.open_file(f3.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_eq!(cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()), 3);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.tab_close_all(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.is_empty()),
            "全干净 → 关闭全部应清空（欢迎页）"
        );

        // 脏中止路径：一个带路径的文件 tab 置脏 → 关闭全部 → 一个不关 + 提示。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(f1.to_str().unwrap(), window, cx);
                app.open_file(f2.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(cx, |a, _cx| {
                if let Some(t) = a.model.borrow_mut().tabs.iter_mut().find(|t| t.payload.file_path.is_some()) {
                    t.payload.dirty = true;
                }
            });
        });
        let before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.tab_close_all(window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            before,
            "任一脏 → 关闭全部应中止（一个不关）"
        );
        let status = cx.update(|_w, cx| app.read(cx).model.borrow().status.to_string());
        assert!(status.contains("未保存的更改"), "应提示有未保存的更改，实际={status}");

        // 未命名中止路径：存在一个未命名（file_path=None）空选项卡 → 关闭全部也中止。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.make_tab(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let before2 = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.tab_close_all(window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            before2,
            "任一未命名 → 关闭全部应中止（一个不关）"
        );
        let status2 = cx.update(|_w, cx| app.read(cx).model.borrow().status.to_string());
        assert!(status2.contains("未保存的更改"), "未命名也应提示，实际={status2}");
        for f in [&f1, &f2, &f3] {
            let _ = std::fs::remove_file(f);
        }
    }

    /// 「关闭其他 / 关闭右侧」：全干净批量关（目标保留且内容/激活正确）；
    /// 受影响侧任一脏 → 中止一个不关。
    #[gpui::test]
    async fn tab_close_others_and_close_right(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mk = |n: usize| std::env::temp_dir().join(format!("gpui-tc-r{n}.txt"));
        for n in 1..=4 {
            std::fs::write(mk(n), format!("BODY{n}")).unwrap();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                for n in 1..=3 {
                    app.open_file(mk(n).to_str().unwrap(), window, cx);
                }
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // tabs: [1, 2, 3]（零 tab 起步开文件，依次 0,1,2）。「关闭其他」目标=2(index 1)。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.tab_close_others(1, window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let tabs = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        assert_eq!(tabs, 1, "关闭其他应只剩目标 tab，实际={tabs}");
        assert!(
            cx.update(|_w, cx| {
                app.read(cx)
                    .model.borrow().tabs
                    .iter()
                    .any(|t| t.payload.file_path.as_deref() == Some(mk(2).as_path()))
            }),
            "目标 tab(2) 应保留"
        );

        // 「关闭右侧」：重开 3 文件 tab → 关第 1 个(index 0)的右侧（2,3）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.tab_close_all(window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                for n in 1..=3 {
                    app.open_file(mk(n).to_str().unwrap(), window, cx);
                }
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // tabs: [1,2,3]，目标=1(index 0) → 关闭右侧 → [1]。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.tab_close_right(0, window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let paths: Vec<Option<std::path::PathBuf>> = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs.iter().map(|t| t.payload.file_path.clone()).collect()
        });
        assert_eq!(paths.len(), 1, "关闭右侧后应剩 1 tab");
        assert_eq!(paths[0].as_deref(), Some(mk(1).as_path()), "目标 1 保留");

        // 中止：目标右侧有一个脏 → 关闭右侧一个不关。
        cx.update(|_w, cx| {
            app.update(cx, |a, _cx| {
                if let Some(t) = a
                    .model.borrow_mut().tabs
                    .get_mut(1)
                    .filter(|t| t.payload.file_path.as_deref() == Some(mk(2).as_path()))
                {
                    t.payload.dirty = true;
                }
            });
        });
        let before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.tab_close_right(0, window, cx));
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            before,
            "目标右侧有脏 → 关闭右侧应中止（一个不关）"
        );
        for n in 1..=4 {
            let _ = std::fs::remove_file(mk(n));
        }
    }

    /// 「关闭已保存」：只关 `有路径 && !dirty`；脏 + 未命名 + 无路径空选项卡保留。
    #[gpui::test]
    async fn tab_close_saved_keeps_dirty_and_unnamed(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let fa = std::env::temp_dir().join("gpui-tcs-a.txt");
        let fb = std::env::temp_dir().join("gpui-tcs-b.txt");
        std::fs::write(&fa, "A").unwrap();
        std::fs::write(&fb, "B").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(fa.to_str().unwrap(), window, cx);
                app.open_file(fb.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // tabs: [未命名(0), A(1,clean), B(2)]；把 A(1) 置脏 → 已保存只有 B(2)。
        cx.update(|_w, cx| {
            app.update(cx, |a, _cx| {
                if let Some(t) = a.model.borrow_mut().tabs.get_mut(1) {
                    t.payload.dirty = true;
                }
            });
        });
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.tab_close_saved(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (paths, dirty) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().tabs
                    .iter()
                    .map(|t| t.payload.file_path.as_deref().map(|p| p.to_path_buf()))
                    .collect::<Vec<_>>(),
                a.model.borrow().tabs.iter().map(|t| t.payload.dirty).collect::<Vec<_>>(),
            )
        });
        assert_eq!(paths.len(), 2, "关闭已保存后应剩未命名 + 脏文件 B(实际只剩下未命名+A)");
        assert_eq!(paths[0], None, "未命名保留");
        assert_eq!(dirty[0], false, "未命名（干净）保留");
        assert_eq!(paths[1].as_deref(), Some(fa.as_path()), "脏文件保留");
        assert!(dirty[1], "脏文件 dirty 保持");
        assert!(
            !paths.iter().any(|p| p.as_deref() == Some(fb.as_path())),
            "已保存的 B 应被关闭"
        );
        let _ = std::fs::remove_file(&fa);
        let _ = std::fs::remove_file(&fb);
    }

    /// 「移入新窗口」：源窗口 -1 tab；新 detached 窗口带该 tab（路径 + 正文）；
    /// 源窗口剩单 tab 移走 → 回欢迎页。
    #[gpui::test]
    async fn tab_move_to_new_window_splits_detached(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f1 = std::env::temp_dir().join("gpui-tm-1.txt");
        let f2 = std::env::temp_dir().join("gpui-tm-2.txt");
        std::fs::write(&f1, "FILE1").unwrap();
        std::fs::write(&f2, "FILE2").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(f1.to_str().unwrap(), window, cx);
                app.open_file(f2.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // tabs: [未命名(0), F1(1), F2(2)]；把 F1(1) 移入新窗口。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.tab_move_to_new_window(1, window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let snap = registry_snapshot(cx);
        let main: Vec<_> = snap.iter().filter(|(d, ..)| !*d).collect();
        let det: Vec<_> = snap.iter().filter(|(d, ..)| *d).collect();
        assert_eq!(det.len(), 1, "应恰好 1 个 detached 窗口，快照={snap:?}");
        assert_eq!(det[0].1, 1, "detached 窗口应恰 1 tab");
        assert_eq!(
            det[0].3.as_deref(),
            Some("FILE1"),
            "detached 窗口应带 F1 正文，快照={snap:?}"
        );
        assert!(
            main.iter().all(|(_, n, _, _)| *n == 2),
            "主窗口应剩 2 tab（未命名+F2），快照={snap:?}"
        );
        // 主窗口剩 2 tab 时把 F2(活动) 也移走 → 主窗口只剩未命名（不回欢迎页）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let f2_idx = app
                    .model.borrow().tabs
                    .iter()
                    .position(|t| t.payload.file_path.as_deref() == Some(f2.as_path()))
                    .expect("F2 应在场");
                app.tab_move_to_new_window(f2_idx, window, cx);
            });
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let snap2 = registry_snapshot(cx);
        let det2: Vec<_> = snap2.iter().filter(|(d, ..)| *d).collect();
        assert_eq!(det2.len(), 2, "两次移入 → 2 个 detached 窗口");
        let main2: Vec<_> = snap2.iter().filter(|(d, ..)| !*d).collect();
        assert!(main2[0].1 == 1 && !main2[0].2, "主窗口应只剩未命名 tab");
        for f in [&f1, &f2] {
            let _ = std::fs::remove_file(f);
        }
    }

    /// 「复制到新窗口」（文本类）：源窗口 tab 数与内容**不变**；新 detached 窗口 =
    /// 同路径 + 同正文 + 同 dirty + 独立编辑器缓冲（改副本不影响源）。
    #[gpui::test]
    async fn tab_copy_to_new_window_text_snapshot_independent(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("gpui-tc-copy.txt");
        std::fs::write(&f, "第一行 copy\n第二行").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // tabs: [未命名(0), F(1)]。把 F(1) 复制到新窗口。
        let source_before = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[1];
            (a.model.borrow().tabs.len(), t.payload.dirty, t.payload.editor.read(cx).value().to_string())
        });
        assert_eq!(source_before.1, false, "打开文件默认不脏");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.tab_copy_to_new_window(1, window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let snap = registry_snapshot(cx);
        let det: Vec<_> = snap.iter().filter(|(d, ..)| *d).collect();
        assert_eq!(det.len(), 1, "应出现 1 个 detached 窗口，快照={snap:?}");
        assert_eq!(det[0].1, 1, "detached 副本窗口应恰 1 tab");
        assert_eq!(
            det[0].3.as_deref(),
            Some("第一行 copy\n第二行"),
            "副本应带同正文，快照={snap:?}"
        );
        // 源窗口不变（仍 2 tab、F 仍是干净、正文不变）。
        let source_after = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().tabs.len(),
                a.model.borrow().tabs[1].payload.dirty,
                a.model.borrow().tabs[1].payload.editor.read(cx).value().to_string(),
            )
        });
        assert_eq!(source_after.0, source_before.0, "源窗口 tab 数不变");
        assert_eq!(source_after.2, source_before.2, "源正文不变");

        // 独立性：改 detached 副本编辑器 → 源正文不受影响。
        let det_entity = cx.update(|_w, cx| {
            let reg = cx.global::<EditorAppRegistry>();
            reg.windows
                .iter()
                .find(|(weak, _)| {
                    weak.upgrade()
                        .is_some_and(|e| e.read_with(cx, |a, _| a.is_detached))
                })
                .and_then(|(weak, _)| weak.upgrade())
                .expect("应有 detached 窗口实体")
        });
        cx.update(|_w, cx| {
            det_entity.update(cx, |app, cx| {
                let ed = app.model.borrow().tabs[0].payload.editor.clone();
                ed.update(cx, |ed, _| ed.set_text("UPDATED COPY"));
                app.model.borrow_mut().tabs[0].payload.dirty = true;
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let src_final = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[1].payload.editor.read(cx).value().to_string()
        });
        assert_eq!(
            src_final,
            "第一行 copy\n第二行",
            "改副本不应影响源正文（独立编辑器缓冲）"
        );
        let _ = std::fs::remove_file(&f);
    }

    /// 回归：「复制到新窗口」源窗口活动选项卡不回跳。3 tab 激活第 0 个后复制
    /// 中间 tab（index 1）——瞬态副本经 `make_tab`（active→副本）→ `take_tab_payload`
    /// （active→末位）会把手动保持的活动指到末位；修复后应保持复制前活动原样。
    #[gpui::test]
    async fn tab_copy_to_new_window_keeps_source_active(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let fs: Vec<_> = (1..=3)
            .map(|n| {
                let f = std::env::temp_dir().join(format!("gpui-tc-keep{n}.txt"));
                std::fs::write(&f, format!("KEEP{n}")).unwrap();
                f
            })
            .collect();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                for f in &fs {
                    app.open_file(f.to_str().unwrap(), window, cx);
                }
                app.activate_tab(0, window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // tabs: [1,2,3]，active=0；复制中间的 index 1。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.tab_copy_to_new_window(1, window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let (active, tabs_len, det) = cx.update(|_w, cx| {
            let main = app.read(cx);
            let det_count = cx
                .global::<EditorAppRegistry>()
                .windows
                .iter()
                .filter(|(weak, _)| {
                    weak.upgrade()
                        .is_some_and(|e| e.read_with(cx, |a, _| a.is_detached))
                })
                .count();
            (main.model.borrow().active_tab, main.model.borrow().tabs.len(), det_count)
        });
        assert_eq!(active, 0, "复制中间 tab 后源窗口活动应保持 0，实际={active}");
        assert_eq!(tabs_len, 3, "源窗口 tab 数不变，实际={tabs_len}");
        assert_eq!(det, 1, "应恰好 1 个 detached 副本窗口，实际={det}");
        for f in &fs {
            let _ = std::fs::remove_file(f);
        }
    }
