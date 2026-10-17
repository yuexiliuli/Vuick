    /// 泵后台目录扫描直到 `dir` 的直接子项缓存就位（且无在途扫描）。
    fn ft_wait_tree_loaded(
        app: &gpui::Entity<EditorApp>,
        cx: &mut VisualTestContext,
        dir: &std::path::Path,
    ) {
        for _ in 0..120 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let ready = cx.update(|_w, cx| {
                let a = app.read(cx);
                a.sidebar.files.children.contains_key(dir)
                    && a.sidebar.files.pending.is_empty()
            });
            if ready {
                return;
            }
        }
        panic!("文件树后台扫描超时：{}", dir.display());
    }

    /// 多跑几帧让布局/滚动稳定（与既有测试的轮询节奏一致）。
    fn ft_stabilize(cx: &mut VisualTestContext) {
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
    }

    /// 打开文件夹 → 设为文件树根 + 后台扫描根直接子项 + 侧边栏切「文件」视图展开。
    /// 根子项排序：文件夹优先、组内按名字字符串排序（sub 目录在前，a.txt/b.txt 在后）。
    #[gpui::test]
    async fn open_folder_sets_tree_sorted_and_switches_view(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let base =
            std::env::temp_dir().join(format!("gpui-tree-open-{}", std::process::id()));
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("b.txt"), "BBB").unwrap();
        std::fs::write(base.join("a.txt"), "AAA").unwrap();
        std::fs::write(base.join("sub/nested.txt"), "NEST").unwrap();

        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(base.clone(), window, cx));
        });
        ft_wait_tree_loaded(&app, cx, &base);
        ft_stabilize(cx);

        let (view, panel, root, ordering) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let children = a
                .sidebar
                .files
                .children
                .get(&base)
                .cloned()
                .unwrap_or_default();
            (
                a.sidebar.view,
                a.sidebar.panel_open,
                a.sidebar.files.root.clone(),
                children.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            )
        });
        assert_eq!(
            view,
            crate::app::sidebar::SidebarView::Files,
            "打开文件夹应切到文件视图"
        );
        assert!(panel, "文件夹面板应展开");
        assert_eq!(root, Some(base.clone()), "根应设为打开的文件夹");
        assert_eq!(
            ordering,
            vec![
                "sub".to_string(),
                "a.txt".to_string(),
                "b.txt".to_string()
            ],
            "根子项应目录优先 + 组内名字排序"
        );
        assert!(
            cx.debug_bounds("sidebar-files").is_some(),
            "文件面板应真实渲染"
        );
        assert!(
            cx.debug_bounds("sidebar-file-row-0").is_some(),
            "根行应渲染"
        );
        assert!(
            cx.debug_bounds("sidebar-file-row-1").is_some(),
            "根直接子项行应渲染"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 展开子目录：后台扫描其直接子项并缓存；折叠只移出 expanded、缓存保留；
    /// 再展开直接复用缓存（无 pending 重扫）。
    #[gpui::test]
    async fn expand_collapse_keeps_children_cache(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let base =
            std::env::temp_dir().join(format!("gpui-tree-fold-{}", std::process::id()));
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("sub/nested.txt"), "NEST").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(base.clone(), window, cx));
        });
        ft_wait_tree_loaded(&app, cx, &base);
        let sub = base.join("sub");
        // 展开 sub（首展 → 后台读子项）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.file_tree_toggle(sub.clone(), window, cx));
        });
        ft_wait_tree_loaded(&app, cx, &sub);
        let (expanded_after, kids) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.sidebar.files.expanded.contains(&sub),
                a.sidebar.files
                    .children
                    .get(&sub)
                    .cloned()
                    .unwrap_or_default(),
            )
        });
        assert!(expanded_after, "展开后 sub 应在 expanded");
        assert_eq!(kids.len(), 1, "sub 应缓存到嵌套文件");
        assert_eq!(kids[0].name, "nested.txt");
        assert!(kids[0].path == sub.join("nested.txt"), "条目应带完整路径");
        // 折叠：仅移出 expanded，children 缓存保留
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.file_tree_toggle(sub.clone(), window, cx));
        });
        let (collapsed, kids_kept) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                !a.sidebar.files.expanded.contains(&sub),
                a.sidebar.files.children.contains_key(&sub),
            )
        });
        assert!(collapsed, "折叠后 sub 应移出 expanded");
        assert!(kids_kept, "折叠应保留 children 缓存");
        // 再展开：直接复用缓存（无在途重扫）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.file_tree_toggle(sub.clone(), window, cx));
        });
        let (re_expanded, pending) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.sidebar.files.expanded.contains(&sub),
                a.sidebar.files.pending.is_empty(),
            )
        });
        assert!(re_expanded, "再展开应恢复 expanded");
        assert!(pending, "缓存命中不应发起重新扫描");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 点击文件行 → 复用 open_file 打开（建选项卡、载入内容）；再次点击同一行 →
    /// 激活既有选项卡、**不新建**。
    #[gpui::test]
    async fn file_row_click_opens_and_repeat_activates(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let base =
            std::env::temp_dir().join(format!("gpui-tree-click-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("a.txt"), "AAA").unwrap();
        std::fs::write(base.join("b.txt"), "BBB").unwrap();
        let a_path = base.join("a.txt");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(base.clone(), window, cx));
        });
        ft_wait_tree_loaded(&app, cx, &base);
        ft_stabilize(cx);
        // 排序：无目录 → 文件 a.txt(行1) / b.txt(行2)
        let row_a = cx.debug_bounds("sidebar-file-row-1").expect("a.txt 行应渲染");
        let pt = gpui::Point::new(row_a.left() + px(30.), row_a.center().y);
        cx.simulate_click(pt, Default::default());
        ft_stabilize(cx);
        let (tabs, active_is_a) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let active = a.tabs.get(a.active_tab).and_then(|t| t.file_path.clone());
            (a.tabs.len(), active == Some(a_path.clone()))
        });
        assert_eq!(tabs, 2, "点击文件行应打开一个选项卡（初始 1 个空 tab + 1）");
        assert!(active_is_a, "应激活并打开 a.txt");
        let content = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].editor.read(cx).value().to_string()
        });
        assert_eq!(content, "AAA", "应载入 a.txt 内容");

        // 重复点击同一行 → 不新建选项卡，仍激活既有 a.txt
        let row_a2 = cx.debug_bounds("sidebar-file-row-1").expect("a.txt 行应仍在");
        let pt2 = gpui::Point::new(row_a2.left() + px(30.), row_a2.center().y);
        cx.simulate_click(pt2, Default::default());
        ft_stabilize(cx);
        let (tabs_after, active2) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let active = a.tabs.get(a.active_tab).and_then(|t| t.file_path.clone());
            (a.tabs.len(), active == Some(a_path.clone()))
        });
        assert_eq!(tabs_after, 2, "重复点击已打开文件应激活既有，不新建");
        assert!(active2, "重复点击后仍激活 a.txt");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 未打开文件夹 → 文件视图空态；打开后空态消失；**换根清缓存**（expanded /
    /// children 只含新根）；展开空目录渲染「（空）」占位行。
    #[gpui::test]
    async fn empty_state_and_switching_root_clears_tree(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let base1 =
            std::env::temp_dir().join(format!("gpui-tree-sw1-{}", std::process::id()));
        let base2 =
            std::env::temp_dir().join(format!("gpui-tree-sw2-{}", std::process::id()));
        std::fs::create_dir_all(base1.join("empty")).unwrap();
        std::fs::write(base1.join("f.txt"), "F").unwrap();
        std::fs::create_dir_all(&base2).unwrap();
        std::fs::write(base2.join("g.txt"), "G").unwrap();

        // 未打开文件夹 → 切文件视图 → 空态
        cx.update(|_w, cx| {
            app.update(
                cx,
                |app, cx| {
                    app.toggle_sidebar_view(crate::app::sidebar::SidebarView::Files, cx)
                },
            );
        });
        ft_stabilize(cx);
        assert!(
            cx.debug_bounds("sidebar-files-empty").is_some(),
            "未打开文件夹应显示空态 sidebar-files-empty"
        );

        // 打开 base1 → 树出现、空态消失
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(base1.clone(), window, cx));
        });
        ft_wait_tree_loaded(&app, cx, &base1);
        ft_stabilize(cx);
        assert!(
            cx.debug_bounds("sidebar-files-empty").is_none(),
            "打开后空态应消失"
        );

        // 展开空目录 empty → 渲染「（空）」占位（可见行：0 根、1 empty 目录、2 f.txt、3 （空））
        let empty_sub = base1.join("empty");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.file_tree_toggle(empty_sub.clone(), window, cx));
        });
        ft_wait_tree_loaded(&app, cx, &empty_sub);
        ft_stabilize(cx);
        let empty_kids = cx.update(|_w, cx| {
            app.read(cx)
                .sidebar
                .files
                .children
                .get(&empty_sub)
                .cloned()
                .unwrap_or_default()
        });
        assert!(empty_kids.is_empty(), "空目录应缓存为空子项");
        assert!(
            cx.debug_bounds("sidebar-file-row-3").is_some(),
            "空目录应渲染「（空）」占位行"
        );

        // 换根 base2 → expanded/children 清空，只留新根（状态失效源①）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(base2.clone(), window, cx));
        });
        ft_wait_tree_loaded(&app, cx, &base2);
        let (root, has_old_tree, expanded_ok) = cx.update(|_w, cx| {
            let f = &app.read(cx).sidebar.files;
            (
                f.root.clone(),
                f.children.contains_key(&base1) || f.children.contains_key(&empty_sub),
                f.expanded.iter().all(|p| p == &base2),
            )
        });
        assert_eq!(root, Some(base2.clone()), "换根后根应为新文件夹");
        assert!(!has_old_tree, "换根应清空旧根/旧展开目录的缓存");
        assert!(expanded_ok, "换根后 expanded 应仅含新根");
        let _ = std::fs::remove_dir_all(&base1);
        let _ = std::fs::remove_dir_all(&base2);
    }

    /// 打开文件夹对话框在测试构建下不弹真实系统选择框（「打开文件夹…」入口可安全调用）；
    /// 路径无效 → 清空文件树并保持文件视图（空态 / 旧根不残留）。
    #[gpui::test]
    async fn open_folder_dialog_noop_in_test_and_invalid_path_clears(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let base =
            std::env::temp_dir().join(format!("gpui-tree-invalid-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("a.txt"), "A").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(base.clone(), window, cx));
        });
        ft_wait_tree_loaded(&app, cx, &base);
        // 测试构建：open_folder_dialog 为 #[cfg(not(test))] 空操作，不可弹窗、不崩溃。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_dialog(window, cx));
        });
        ft_stabilize(cx);
        let (view, root) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.sidebar.view, a.sidebar.files.root.clone())
        });
        assert_eq!(view, crate::app::sidebar::SidebarView::Files);
        assert_eq!(
            root,
            Some(base.clone()),
            "测试构建下 open_folder_dialog 不应改状态"
        );
        // 路径无效 → 清空树（root None）
        let ghost = base.join("ghost");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(ghost.clone(), window, cx));
        });
        let (root2, kids) = cx.update(|_w, cx| {
            let f = &app.read(cx).sidebar.files;
            (f.root.clone(), f.children.len())
        });
        assert_eq!(root2, None, "无效路径应清空文件树根");
        assert_eq!(kids, 0, "无效路径应清空全部子项缓存");
        let _ = std::fs::remove_dir_all(&base);
    }
