//! 「打开最近」域（recent）：记录去重置顶/截断、打开面板四区域/空态/焦点、真实菜单
//! flyout 链路。断言内部与 lib 单测 `src/app/tests/recent.rs` 逐字一致（P1 测试域化）。
//! ⚠ 集成目标下 shell lib 非 test 编译：`record_recent_*`/`open_recent_panel` 等动作经
//! §8.6 外提清单 pub 化访问；`common::init_test` 注入 `GPUI_EDITOR_TEST` 短路持久化写盘。

mod common;

use common::*;

    /// 「打开最近」记录：`record_recent_file/folder` 去重置顶 + 每类 ≤50（纯逻辑断言）。
    #[gpui::test]
    async fn record_recent_dedup_top_and_truncate(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 测试进程共享真实 config 读路径（K8：config 依赖编译不受 cfg!(test) 门控），
        // 先清本实例最近列表，让断言与机器上真实 config 无关。
        cx.update(|_w, cx| {
            app.update(cx, |app, _| {
                app.model.borrow_mut().recent_folders.clear();
                app.model.borrow_mut().recent_files.clear();
            });
        });
        // 去重置顶：文件 a → b → a（重开 a 应置顶且无重复）。
        let a = std::env::temp_dir().join("recent-a.txt");
        let b = std::env::temp_dir().join("recent-b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let (sa, sb) = (a.to_string_lossy().into_owned(), b.to_string_lossy().into_owned());
        cx.update(|_w, cx| {
            app.update(cx, |app, _| {
                app.record_recent_file(&a);
                app.record_recent_file(&b);
                app.record_recent_file(&a);
            });
        });
        let files = cx.update(|_w, cx| app.read(cx).model.borrow().recent_files.clone());
        assert_eq!(files, vec![sa.clone(), sb.clone()], "重复打开应去重置顶（新→旧）");

        // 文件夹：重复打开只保留一条。
        let dir = std::env::temp_dir().join("recent-dir-dedup");
        let _ = std::fs::create_dir_all(&dir);
        let sd = dir.to_string_lossy().into_owned();
        cx.update(|_w, cx| {
            app.update(cx, |app, _| {
                app.record_recent_folder(&dir);
                app.record_recent_folder(&dir);
            });
        });
        let folders = cx.update(|_w, cx| app.read(cx).model.borrow().recent_folders.clone());
        assert_eq!(folders, vec![sd.clone()], "文件夹重复打开应只保留一条");

        // 超 50 条 → 最旧被挤出（每类各自独立 ≤50）。
        for i in 0..60 {
            let p = std::env::temp_dir().join(format!("recent-trunc-{i}.txt"));
            std::fs::write(&p, "x").unwrap();
            cx.update(|_w, cx| {
                app.update(cx, |app, _| app.record_recent_file(&p));
            });
        }
        let files = cx.update(|_w, cx| app.read(cx).model.borrow().recent_files.clone());
        assert_eq!(files.len(), config::limits::RECENT_MAX, "最近文件应截断到 50");
        assert_eq!(
            files[0],
            std::env::temp_dir()
                .join("recent-trunc-59.txt")
                .to_string_lossy()
                .into_owned(),
            "最新一条应排最前"
        );
        assert!(
            !files.contains(&std::env::temp_dir().join("recent-trunc-0.txt").to_string_lossy().into_owned()),
            "最旧一条应被挤出"
        );

        for i in 0..60 {
            let _ = std::fs::remove_file(std::env::temp_dir().join(format!("recent-trunc-{i}.txt")));
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 打开文件 / 文件夹成功 → 写入最近记录；打开失败（不可读文本）→ 不记录。
    #[gpui::test]
    async fn open_records_recent_and_failure_does_not(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 清本实例最近列表（K8：config load 无 cfg(test) 门控，勿依赖机器真实 config）。
        cx.update(|_w, cx| {
            app.update(cx, |app, _| {
                app.model.borrow_mut().recent_folders.clear();
                app.model.borrow_mut().recent_files.clear();
            });
        });
        let f = std::env::temp_dir().join("recent-open-good.txt");
        std::fs::write(&f, "hello").unwrap();
        let dir = std::env::temp_dir().join("recent-open-dir");
        let _ = std::fs::create_dir_all(&dir.join("sub"));
        let s = f.to_string_lossy().into_owned();
        let sd = dir.to_string_lossy().into_owned();

        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(&s, window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let files = cx.update(|_w, cx| app.read(cx).model.borrow().recent_files.clone());
        assert_eq!(files, vec![s.clone()], "打开文件成功应记录最近");

        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(dir.clone(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let folders = cx.update(|_w, cx| app.read(cx).model.borrow().recent_folders.clone());
        assert_eq!(folders, vec![sd.clone()], "打开文件夹成功应记录最近");

        // 不可读文本文件：打开失败早退 → 不记录。
        let bad = std::env::temp_dir().join("recent-open-bad.bin");
        std::fs::write(&bad, [0xFF, 0xFE, 0x00, 0xD8]).unwrap();
        let sb = bad.to_string_lossy().into_owned();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(&sb, window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (status_ok, files) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().status_ok, a.model.borrow().recent_files.clone())
        });
        assert!(!status_ok, "打开失败应置失败状态");
        assert!(
            files.iter().all(|p| p != &sb),
            "打开失败不应记录最近（早退分支）"
        );

        let _ = std::fs::remove_file(&f);
        let _ = std::fs::remove_file(&bad);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 统一的「打开最近」面板 → 渲染泵帧（deferred 浮层 + 弹簧淡入）。
    fn pump_panel(cx: &mut VisualTestContext) {
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
    }

    /// 打开面板的测试辅助：直接走宿主入口（后续各测试不经「文件菜单」链路单独验面板）。
    fn open_panel(app: &Entity<EditorApp>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_recent_panel(window, cx));
        });
        pump_panel(cx);
    }

    /// 「打开最近」面板是**居中大面板**实体（反馈 2）：面板实体就位、根「recent-panel」
    /// 渲染、列表滚动容器存在（Ctrl+P 同款外壳）。
    #[gpui::test]
    async fn recent_panel_is_centered_large_panel(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        open_panel(&app, cx);
        assert!(
            cx.update(|_w, cx| app.read(cx).recent_panel.is_some()),
            "面板实体应被宿主持有（单实体）"
        );
        let bounds = cx
            .debug_bounds("recent-panel")
            .expect("居中大面板根应渲染");
        // 面板宽 600、高 = 窗口 2/3（980×720 → 高 480），横向居中。
        assert!(
            (bounds.size.width.as_f32() - 600.).abs() <= 1.0,
            "面板宽应为 600，实际 {}",
            bounds.size.width.as_f32()
        );
        let expect_h = 720.0 * (2.0 / 3.0);
        assert!(
            (bounds.size.height.as_f32() - expect_h).abs() <= 2.0,
            "面板高应为窗口 2/3（{}），实际 {}",
            expect_h,
            bounds.size.height.as_f32()
        );
        let cx_center =
            (bounds.origin.x.as_f32() + bounds.size.width.as_f32() / 2.) - 980.0 / 2.0;
        assert!(cx_center.abs() <= 2.0, "面板应水平居中，实测中心偏移 {cx_center}");
        assert!(cx.debug_bounds("recent-panel-scroll").is_some(), "应有滚动容器");
    }

    /// 「打开最近」面板四区域渲染：区域头 + 各 ≤10 行 / 空态「暂无」+ More + Clear。
    #[gpui::test]
    async fn recent_panel_renders_four_regions(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 预置最近记录：12 个文件夹（四区域只展示前 10）+ 2 个文件。
        let folders: Vec<String> = (0..12)
            .map(|i| std::env::temp_dir().join(format!("recent-rf{i}")).to_string_lossy().into_owned())
            .collect();
        let files: Vec<String> = (0..2)
            .map(|i| std::env::temp_dir().join(format!("recent-file{i}.txt")).to_string_lossy().into_owned())
            .collect();
        cx.update(|_w, cx| {
            app.update(cx, |app, _| {
                app.model.borrow_mut().recent_folders = folders.clone();
                app.model.borrow_mut().recent_files = files.clone();
            });
        });
        open_panel(&app, cx);
        assert!(cx.debug_bounds("recent-divider-1").is_some(), "文件夹区与文件区之间应有分割线");
        assert!(cx.debug_bounds("recent-divider-2").is_some(), "文件区与 More 之间应有分割线");
        assert!(cx.debug_bounds("recent-divider-3").is_some(), "More 与 Clear 之间应有分割线");
        assert!(cx.debug_bounds("recent-folder-row-0").is_some(), "文件夹首行应渲染");
        assert!(cx.debug_bounds("recent-folder-row-9").is_some(), "文件夹第 10 行应渲染");
        assert!(cx.debug_bounds("recent-folder-row-10").is_none(), "四区域最多展示 10 条文件夹");
        assert!(cx.debug_bounds("recent-file-row-0").is_some(), "文件首行应渲染");
        assert!(cx.debug_bounds("recent-file-row-1").is_some(), "文件第 2 行应渲染");
        assert!(cx.debug_bounds("recent-more").is_some(), "More 行应渲染");
        assert!(cx.debug_bounds("recent-clear").is_some(), "Clear 行应渲染");
        assert!(cx.debug_bounds("recent-folder-empty").is_none(), "文件夹非空不显示暂无");
        assert!(cx.debug_bounds("recent-file-empty").is_none(), "文件非空不显示暂无");
        assert!(cx.debug_bounds("recent-back").is_none(), "四区域无返回行");
    }

    /// 空类别 → 无文字区域头、保留分割线 + 各行「暂无」灰字（Clear/More 仍显示）。
    #[gpui::test]
    async fn recent_panel_empty_sections_show_none(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 清本实例最近列表（K8：config load 无 cfg(test) 门控，须保证空态起点）。
        cx.update(|_w, cx| {
            app.update(cx, |app, _| {
                app.model.borrow_mut().recent_folders.clear();
                app.model.borrow_mut().recent_files.clear();
            });
        });
        open_panel(&app, cx);
        assert!(cx.debug_bounds("recent-folder-empty").is_some(), "空文件夹区显示「暂无」");
        assert!(cx.debug_bounds("recent-file-empty").is_some(), "空文件区显示「暂无」");
        assert!(cx.debug_bounds("recent-divider-1").is_some(), "空分区之间仍有分割线");
        assert!(cx.debug_bounds("recent-divider-2").is_some(), "空文件区与 More 之间仍有分割线");
        assert!(cx.debug_bounds("recent-divider-3").is_some(), "More 与 Clear 之间仍有分割线");
        assert!(cx.debug_bounds("recent-more").is_some(), "空列表下 More 行仍显示");
        assert!(cx.debug_bounds("recent-clear").is_some(), "空列表下 Clear 行仍显示");
    }

    /// 点文件行（有效）→ `open_file` 建 tab + 面板关闭 + 记录去重置顶。
    #[gpui::test]
    async fn click_recent_file_row_opens_file(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("recent-click-good.txt");
        std::fs::write(&f, "data").unwrap();
        let s = f.to_string_lossy().into_owned();
        let tabs_before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        cx.update(|_w, cx| {
            app.update(cx, |app, _| app.model.borrow_mut().recent_files = vec![s.clone()]);
        });
        open_panel(&app, cx);
        let row = cx
            .debug_bounds("recent-file-row-0")
            .expect("文件行应渲染")
            .center();
        cx.simulate_click(row, Default::default());
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (tabs, files, panel) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().tabs.len(), a.model.borrow().recent_files.clone(), a.recent_panel.is_some())
        });
        assert_eq!(tabs, tabs_before + 1, "点文件行应打开新选项卡");
        assert_eq!(files, vec![s.clone()], "打开成功后最近记录保持（去重置顶）");
        assert!(!panel, "点行后面板应关闭");
        let _ = std::fs::remove_file(&f);
    }

    /// 焦点探针：点有效文件行打开文件后——面板关闭，且焦点应落在新打开的编辑器上
    /// （不能被面板 dismiss 归还 previous_focus 抢走——动作已带走焦点时不归还）。
    #[gpui::test]
    async fn click_recent_file_row_leaves_focus_on_editor(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("focus-recent.txt");
        std::fs::write(&f, "data").unwrap();
        let s = f.to_string_lossy().into_owned();
        cx.update(|_w, cx| {
            app.update(cx, |app, _| app.model.borrow_mut().recent_files = vec![s.clone()]);
        });
        open_panel(&app, cx);
        let row = cx
            .debug_bounds("recent-file-row-0")
            .expect("文件行应渲染")
            .center();
        cx.simulate_click(row, Default::default());
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (tab_present, panel, focused) = cx.update(|window, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().tabs.get(a.model.borrow().active_tab).map(|t| t.file_path.clone()).flatten().is_some(),
                a.recent_panel.is_some(),
                window.focused(cx),
            )
        });
        assert!(tab_present, "点有效行应打开文件选项卡");
        assert!(!panel, "面板应关闭");
        let expected = cx.update(|_w, cx| app.read(cx).editor_focus_handle(cx));
        assert_eq!(
            focused.as_ref(),
            Some(&expected),
            "点有效行打开文件后焦点应落在新编辑器（不被 previous_focus 抢走）"
        );
        let _ = std::fs::remove_file(&f);
    }

    /// 焦点探针（真实菜单链路）：文件菜单「打开最近…」flyout 次级菜单 → 点有效文件行 →
    /// 打开新选项卡 + **整链关闭**（次级 + 父菜单都不在场），焦点落在新编辑器上
    /// （组件 `activate_flyout` 的非 keep_open 路径：handler 后 dismiss，动作已带走焦点
    /// 则不归还 previous_focus——[`PopupMenu`] 的 `dismiss` focus 归还守卫）。
    #[gpui::test]
    async fn click_recent_file_row_via_file_menu_opens_file_and_closes_chain(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("focus-menu-recent.txt");
        std::fs::write(&f, "data").unwrap();
        let s = f.to_string_lossy().into_owned();
        let tabs_before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        cx.update(|_w, cx| {
            app.update(cx, |app, _| app.model.borrow_mut().recent_files = vec![s.clone()]);
        });
        // 点标题栏「文件」菜单按钮 → 菜单含「打开最近…」（根行带 › 指示）→ 悬停展开次级。
        cx.simulate_click(gpui::Point::new(px(36.), px(16.)), Default::default());
        pump_panel(cx);
        assert!(
            cx.debug_bounds("menu-flyout-chevron-3").is_some(),
            "文件菜单「打开最近…」根行应带 › 箭头指示（flyout 标记）"
        );
        let root = cx
            .debug_bounds("recent-menu-open")
            .expect("文件菜单应含「打开最近…」项")
            .center();
        cx.simulate_mouse_move(root, None, Default::default());
        pump_panel(cx);
        let row = cx
            .debug_bounds("recent-file-row-0")
            .expect("flyout 次级应含最近文件行")
            .center();
        // 先移入次级子项再点（悬停守卫新鲜：点次级面板不会被父面板 capture 的
        // on_mouse_down_out 当「面板外」误关整链——真实用户路径鼠标必然先悬停再点）。
        cx.simulate_mouse_move(row, None, Default::default());
        pump_panel(cx);
        cx.simulate_click(row, Default::default());
        pump_panel(cx);
        let (tabs, focused, menu_gone, fly_gone) = {
            let tabs = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
            let focused = cx.update(|window, cx| window.focused(cx));
            (tabs, focused, cx.debug_bounds("recent-menu-open").is_none(), cx.debug_bounds("menu-flyout").is_none())
        };
        assert_eq!(tabs, tabs_before + 1, "点 flyout 最近文件行应打开新选项卡");
        assert!(fly_gone, "点行后次级菜单应关闭（menu-flyout 不在场）");
        assert!(menu_gone, "点行后父菜单应整链关闭（recent-menu-open 不在场）");
        let expected = cx.update(|_w, cx| app.read(cx).editor_focus_handle(cx));
        assert_eq!(
            focused.as_ref(),
            Some(&expected),
            "经文件菜单打开最近 → 点有效行后焦点应落在新编辑器"
        );
        let _ = std::fs::remove_file(&f);
    }

    /// 键盘：面板聚焦后 ↓ 选中文件首行 → Enter 打开文件（↑↓ 在可导航行内移动）。
    #[gpui::test]
    async fn keyboard_selects_and_enter_opens_file(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("recent-kbd.txt");
        std::fs::write(&f, "data").unwrap();
        let s = f.to_string_lossy().into_owned();
        let tabs_before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        cx.update(|_w, cx| {
            app.update(cx, |app, _| app.model.borrow_mut().recent_files = vec![s.clone()]);
        });
        open_panel(&app, cx);
        // 面板打开即聚焦自身 → ↓ 选中第一个可导航行（文件行）→ Enter 执行。
        let panel_handle = cx.update(|_w, cx| {
            app.read(cx)
                .recent_panel
                .as_ref()
                .unwrap()
                .read(cx)
                .focus_handle(cx)
        });
        assert!(
            cx.update(|window, cx| window.focused(cx).as_ref() == Some(&panel_handle)),
            "打开后焦点应落在面板 focus_handle"
        );
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("enter");
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (tabs, panel) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().tabs.len(), a.recent_panel.is_some())
        });
        assert_eq!(tabs, tabs_before + 1, "↓+Enter 应打开选中的文件行");
        assert!(!panel, "Enter 后面板应关闭");
        let _ = std::fs::remove_file(&f);
    }

    /// 点失效文件行 → 从列表移除 + 立即持久化 + 状态栏提示 + 面板关闭（不尝试打开）。
    #[gpui::test]
    async fn click_missing_recent_file_removes_and_status(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let missing = std::env::temp_dir().join("recent-missing.txt");
        let s = missing.to_string_lossy().into_owned();
        let tabs_before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        cx.update(|_w, cx| {
            app.update(cx, |app, _| app.model.borrow_mut().recent_files = vec![s.clone()]);
        });
        open_panel(&app, cx);
        let row = cx
            .debug_bounds("recent-file-row-0")
            .expect("文件行应渲染")
            .center();
        cx.simulate_click(row, Default::default());
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (tabs, files, status, status_ok, panel) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.model.borrow().tabs.len(),
                a.model.borrow().recent_files.clone(),
                a.model.borrow().status.to_string(),
                a.model.borrow().status_ok,
                a.recent_panel.is_some(),
            )
        });
        assert_eq!(tabs, tabs_before, "失效项不应尝试打开");
        assert!(files.is_empty(), "失效项应从最近列表移除");
        assert!(!status_ok, "失效项应置状态为失败");
        assert!(
            status.contains("不存在") && status.contains("已从最近列表移除"),
            "状态栏应提示「不存在，已从最近列表移除」，实际 {status}"
        );
        assert!(!panel, "点失效行后面板应关闭");
    }

    /// 点失效文件夹行 → 同样移除 + 持久化 + 状态栏提示 + 面板关闭。
    #[gpui::test]
    async fn click_missing_recent_folder_removes_and_status(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let missing = std::env::temp_dir().join("recent-missing-dir");
        let s = missing.to_string_lossy().into_owned();
        cx.update(|_w, cx| {
            app.update(cx, |app, _| app.model.borrow_mut().recent_folders = vec![s.clone()]);
        });
        open_panel(&app, cx);
        let row = cx
            .debug_bounds("recent-folder-row-0")
            .expect("文件夹行应渲染")
            .center();
        cx.simulate_click(row, Default::default());
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (folders, status, status_ok, panel) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().recent_folders.clone(), a.model.borrow().status.to_string(), a.model.borrow().status_ok, a.recent_panel.is_some())
        });
        assert!(folders.is_empty(), "失效文件夹应从最近列表移除");
        assert!(!status_ok, "失效文件夹应置状态为失败");
        assert!(
            status.contains("不存在") && status.contains("已从最近列表移除"),
            "状态栏应提示，实际 {status}"
        );
        assert!(!panel, "点失效文件夹行后面板应关闭");
    }

    /// More → 完整列表（同一面板实体 mode 切换：返回行 + 全部 ≤50 行 + 滚动）；
    /// 返回 → 切回四区域。不重建面板实体、不关闭。
    #[gpui::test]
    async fn more_switches_full_list_and_back(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let folders: Vec<String> = (0..12)
            .map(|i| std::env::temp_dir().join(format!("more-f{i}")).to_string_lossy().into_owned())
            .collect();
        let files: Vec<String> = (0..2)
            .map(|i| std::env::temp_dir().join(format!("more-file{i}.txt")).to_string_lossy().into_owned())
            .collect();
        cx.update(|_w, cx| {
            app.update(cx, |app, _| {
                app.model.borrow_mut().recent_folders = folders.clone();
                app.model.borrow_mut().recent_files = files.clone();
            });
        });
        open_panel(&app, cx);
        let panel_entity = cx.update(|_w, cx| app.read(cx).recent_panel.clone());
        assert!(panel_entity.is_some(), "面板实体应存在");
        assert!(cx.debug_bounds("recent-folder-row-11").is_none(), "四区域不展示第 12 条文件夹");
        let more = cx
            .debug_bounds("recent-more")
            .expect("More 行应渲染")
            .center();
        cx.simulate_click(more, Default::default());
        pump_panel(cx);
        // 同实体 mode 切换：实体不变、mode=All。
        let mode = cx.update(|_w, cx| app.read(cx).recent_panel.clone().unwrap().read(cx).mode());
        assert_eq!(mode, session_store::RecentMode::All, "More 应切到完整列表模式");
        assert!(
            cx.update(|_w, cx| app.read(cx).recent_panel.as_ref() == panel_entity.as_ref()),
            "More 不应重开/重建面板实体（同实体切换）"
        );
        assert!(cx.debug_bounds("recent-back").is_some(), "完整列表应有返回行");
        assert!(cx.debug_bounds("recent-folder-row-11").is_some(), "完整列表展示全部文件夹");
        assert!(cx.debug_bounds("recent-folder-row-0").is_some(), "完整列表仍从首行起");
        assert!(cx.debug_bounds("recent-more").is_none(), "完整列表不再有 More 行");
        assert!(cx.debug_bounds("recent-panel-scroll").is_some(), "完整列表应有滚动容器");

        let back = cx
            .debug_bounds("recent-back")
            .expect("返回行应渲染")
            .center();
        cx.simulate_click(back, Default::default());
        pump_panel(cx);
        let mode = cx.update(|_w, cx| app.read(cx).recent_panel.clone().unwrap().read(cx).mode());
        assert_eq!(mode, session_store::RecentMode::Regions, "返回应切回四区域");
        assert!(cx.debug_bounds("recent-more").is_some(), "返回后回到四区域（More 行出现）");
        assert!(cx.debug_bounds("recent-back").is_none(), "四区域无返回行");
        assert!(cx.debug_bounds("recent-folder-row-11").is_none(), "四区域只展示前 10 条");
    }

    /// Clear Recently Opened... → 清空两类 + 面板关闭。
    #[gpui::test]
    async fn clear_recent_empties_and_closes(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let f = std::env::temp_dir().join("clear-file.txt").to_string_lossy().into_owned();
        let d = std::env::temp_dir().join("clear-dir").to_string_lossy().into_owned();
        cx.update(|_w, cx| {
            app.update(cx, |app, _| {
                app.model.borrow_mut().recent_folders = vec![d.clone()];
                app.model.borrow_mut().recent_files = vec![f.clone()];
            });
        });
        open_panel(&app, cx);
        let clear = cx
            .debug_bounds("recent-clear")
            .expect("Clear 行应渲染")
            .center();
        cx.simulate_click(clear, Default::default());
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (folders, files, panel) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().recent_folders.clone(), a.model.borrow().recent_files.clone(), a.recent_panel.is_some())
        });
        assert!(folders.is_empty() && files.is_empty(), "Clear 应清空两类最近记录");
        assert!(!panel, "Clear 后面板应关闭");
    }

    /// 反馈 1/2 新形态（真实菜单链路）：文件菜单「打开最近…」改为 **flyout 悬浮次级菜单**
    /// ——悬停根行 → 右侧展开次级（不再打开居中大面板，recent_panel 存 dormancy）；
    /// 父菜单其它项保持可见（不原地替换）；空最近列表 → 次级显示**单个禁用占位项**
    /// 「暂无最近记录」，点击不可点（菜单保持展开、不打开任何东西）。
    #[gpui::test]
    async fn file_menu_recent_flyout_empty_shows_disabled_placeholder(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 清空最近两类（K8：config load 无 cfg(test) 门控，勿依赖机器真实 config）。
        cx.update(|_w, cx| {
            app.update(cx, |app, _| {
                app.model.borrow_mut().recent_folders.clear();
                app.model.borrow_mut().recent_files.clear();
            });
        });
        let tabs_before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        // 点标题栏「文件」菜单按钮 → 悬停「打开最近…」根行。
        cx.simulate_click(gpui::Point::new(px(36.), px(16.)), Default::default());
        pump_panel(cx);
        let root = cx
            .debug_bounds("recent-menu-open")
            .expect("文件菜单应含「打开最近…」项")
            .center();
        cx.simulate_mouse_move(root, None, Default::default());
        pump_panel(cx);
        // 空列表 → 禁用占位项「暂无最近记录」；父项保持可见（反馈 1：不原地替换）。
        assert!(cx.debug_bounds("menu-flyout").is_some(), "悬停「打开最近…」应展开次级");
        assert!(cx.debug_bounds("recent-empty").is_some(), "空列表应显示禁用占位项");
        assert!(cx.debug_bounds("recent-folder-row-0").is_none(), "空列表无文件夹行");
        assert!(cx.debug_bounds("recent-file-row-0").is_none(), "空列表无文件行");
        assert!(cx.debug_bounds("recent-clear").is_none(), "空列表不显示清空行");
        assert!(cx.debug_bounds("recent-menu-open").is_some(), "悬停后父项「打开最近…」保持可见");
        assert!(cx.debug_bounds("menu-item-0").is_some(), "悬停后父项「新建文件」保持可见");
        // 点占位项 → 不可点（不打开新 tab、菜单保持展开）。先移入占位项（触发次级面板
        // hover，armed 点击守卫）：真实用户路径鼠标必先悬停面板/行再点；禁用行无自身
        // hover，但面板级 on_hover 会置位 flyout_panel_hover。
        let empty = cx
            .debug_bounds("recent-empty")
            .expect("占位项应在场")
            .center();
        cx.simulate_mouse_move(empty, None, Default::default());
        pump_panel(cx);
        cx.simulate_click(empty, Default::default());
        pump_panel(cx);
        let tabs = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        assert_eq!(tabs, tabs_before, "点禁用占位项不应打开任何东西");
        assert!(cx.debug_bounds("menu-flyout").is_some(), "点禁用占位项后菜单应保持展开");
        assert!(cx.debug_bounds("recent-menu-open").is_some(), "点禁用占位项后父菜单保持展开");
    }

    /// 悬停「打开最近…」→ flyout 次级结构：最近文件夹 ≤10 → 分隔线 → 最近文件 ≤10 →
    /// 分隔线 → 「清空最近记录」；次级顶部 ≈ 根行顶部、左缘贴父菜单右缘（Dropdown 宿主
    /// 未接 parent_pos → 走组件流式兄弟 fallback；近窗缘适配受限，报告已注明，文件菜单
    /// 位于顶部影响小）。父项保持可见。
    #[gpui::test]
    async fn file_menu_recent_flyout_hover_shows_sections_and_geometry(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join("recent-flyout-dir");
        let _ = std::fs::create_dir_all(&dir);
        let f = std::env::temp_dir().join("recent-flyout-file.txt");
        std::fs::write(&f, "data").unwrap();
        let (fd, fs) = (dir.to_string_lossy().into_owned(), f.to_string_lossy().into_owned());
        cx.update(|_w, cx| {
            app.update(cx, |app, _| {
                app.model.borrow_mut().recent_folders = vec![fd.clone()];
                app.model.borrow_mut().recent_files = vec![fs.clone()];
            });
        });
        // 打开文件菜单 → 悬停「打开最近…」。
        cx.simulate_click(gpui::Point::new(px(36.), px(16.)), Default::default());
        pump_panel(cx);
        assert!(
            cx.debug_bounds("menu-flyout-chevron-3").is_some(),
            "「打开最近…」根行应带 › 箭头指示"
        );
        let root = cx
            .debug_bounds("recent-menu-open")
            .expect("「打开最近…」应在场")
            .center();
        cx.simulate_mouse_move(root, None, Default::default());
        pump_panel(cx);
        // 次级结构：文件夹行 + 文件行 + 清空行（分隔线不单独可断言 debug 时以结构行为主）。
        assert!(cx.debug_bounds("menu-flyout").is_some(), "悬停应展开次级");
        assert!(cx.debug_bounds("recent-folder-row-0").is_some(), "次级应含最近文件夹行");
        assert!(cx.debug_bounds("recent-file-row-0").is_some(), "次级应含最近文件行");
        assert!(cx.debug_bounds("recent-clear").is_some(), "次级应含「清空最近记录」行");
        // 父项保持可见。
        assert!(cx.debug_bounds("recent-menu-open").is_some(), "父项保持可见");
        assert!(cx.debug_bounds("menu-item-0").is_some(), "父菜单其它项保持可见");
        // 几何：次级顶部 ≈ 「打开最近…」行顶部（容差内）；左缘贴父菜单右缘（右邻、不压父）。
        let fly = cx.debug_bounds("menu-flyout").expect("次级应在场");
        let row = cx.debug_bounds("recent-menu-open").expect("根行应在场");
        let panel = cx.debug_bounds("menu-panel").expect("父面板应在场");
        let dy = (fly.top() - row.top()).abs();
        assert!(dy <= px(6.), "次级顶部应≈「打开最近…」行顶部，实际差 {dy:?}");
        let dx = fly.left() - panel.right();
        assert!(dx >= -px(6.), "次级左缘应贴父菜单右缘（右邻），实际偏移 {dx:?}");
        assert!(fly.right() > panel.right(), "次级应在父菜单右缘右侧");
        let _ = std::fs::remove_file(&f);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 点「清空最近记录」→ 两类最近列表清空 + 落盘 + **整链关闭**（次级 + 父菜单）。
    #[gpui::test]
    async fn file_menu_recent_flyout_clear_empties_and_closes_chain(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join("recent-clear-flyout-dir");
        let _ = std::fs::create_dir_all(&dir);
        let f = std::env::temp_dir().join("recent-clear-flyout.txt");
        std::fs::write(&f, "data").unwrap();
        let (fd, fs) = (dir.to_string_lossy().into_owned(), f.to_string_lossy().into_owned());
        cx.update(|_w, cx| {
            app.update(cx, |app, _| {
                app.model.borrow_mut().recent_folders = vec![fd.clone()];
                app.model.borrow_mut().recent_files = vec![fs.clone()];
            });
        });
        cx.simulate_click(gpui::Point::new(px(36.), px(16.)), Default::default());
        pump_panel(cx);
        let root = cx
            .debug_bounds("recent-menu-open")
            .expect("「打开最近…」应在场")
            .center();
        cx.simulate_mouse_move(root, None, Default::default());
        pump_panel(cx);
        let clear = cx
            .debug_bounds("recent-clear")
            .expect("清空行应在场")
            .center();
        // 先移入次级再点（见 file 行点击测试注释：悬停守卫新鲜，点次级不误触面板外关闭）。
        cx.simulate_mouse_move(clear, None, Default::default());
        pump_panel(cx);
        cx.simulate_click(clear, Default::default());
        pump_panel(cx);
        let (folders, files) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().recent_folders.clone(), a.model.borrow().recent_files.clone())
        });
        assert!(folders.is_empty() && files.is_empty(), "清空应清两类最近记录");
        assert!(cx.debug_bounds("menu-flyout").is_none(), "清空后次级应关闭");
        assert!(cx.debug_bounds("recent-menu-open").is_none(), "清空后父菜单应整链关闭");
        let _ = std::fs::remove_file(&f);
        let _ = std::fs::remove_dir_all(&dir);
    }
