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
            let active = a.model.borrow().tabs.get(a.model.borrow().active_tab).and_then(|t| t.payload.file_path.clone());
            (a.model.borrow().tabs.len(), active == Some(a_path.clone()))
        });
        assert_eq!(tabs, 2, "点击文件行应打开一个选项卡（初始 1 个空 tab + 1）");
        assert!(active_is_a, "应激活并打开 a.txt");
        let content = cx.update(|_w, cx| {
            let a = app.read(cx);
            a.model.borrow().tabs[a.model.borrow().active_tab].payload.editor.read(cx).value().to_string()
        });
        assert_eq!(content, "AAA", "应载入 a.txt 内容");

        // 重复点击同一行 → 不新建选项卡，仍激活既有 a.txt
        let row_a2 = cx.debug_bounds("sidebar-file-row-1").expect("a.txt 行应仍在");
        let pt2 = gpui::Point::new(row_a2.left() + px(30.), row_a2.center().y);
        cx.simulate_click(pt2, Default::default());
        ft_stabilize(cx);
        let (tabs_after, active2) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let active = a.model.borrow().tabs.get(a.model.borrow().active_tab).and_then(|t| t.payload.file_path.clone());
            (a.model.borrow().tabs.len(), active == Some(a_path.clone()))
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

    /// 启动时「上次打开的文件夹（opened_folder 持久化）」的自动重开**跟随「恢复上次会话」
    /// 开关**：关闭恢复 = 干净启动不重开上次文件夹；显式开启才把文件树恢复到该文件夹并
    /// 切「文件」视图。回归：用户关掉「恢复上次会话」后每次启动仍被重开（如 D:\cache）。
    #[gpui::test]
    async fn startup_folder_restore_gated_by_session_restore(cx: &mut TestAppContext) {
        init_test(cx);
        let base =
            std::env::temp_dir().join(format!("gpui-startup-folder-{}", std::process::id()));
        std::fs::create_dir_all(base.join("子目录")).unwrap();
        std::fs::write(base.join("a.txt"), "AAA").unwrap();
        // JSON 里用正斜杠避免反斜杠转义坏包；PathBuf 在 Windows 接受该形式。
        let folder_json = base.to_string_lossy().replace('\\', "/");
        let cfg =
            std::env::temp_dir().join(format!("gpui-folder-cfg-{}.json", std::process::id()));

        // ① 关闭「恢复上次会话」→ 启动不重开上次文件夹。
        std::fs::write(
            &cfg,
            format!(r#"{{"opened_folder": "{folder_json}", "session_restore": false}}"#),
        )
        .unwrap();
        unsafe { std::env::set_var("VUICK_CONFIG", &cfg) };
        let (app_off, cx) = add_app(cx);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let root_off = cx.update(|_w, cx| app_off.read(cx).sidebar.files.root.clone());
        unsafe { std::env::remove_var("VUICK_CONFIG") };
        assert_eq!(
            root_off,
            None,
            "关闭恢复上次会话 → 启动不应重开上次打开的文件夹"
        );

        // ② 显式开启「恢复上次会话」→ 启动把文件树恢复到上次文件夹并切「文件」视图。
        std::fs::write(
            &cfg,
            format!(r#"{{"opened_folder": "{folder_json}", "session_restore": true}}"#),
        )
        .unwrap();
        unsafe { std::env::set_var("VUICK_CONFIG", &cfg) };
        let (app_on, cx) = add_app(cx);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (root_on, view_on) = cx.update(|_w, cx| {
            let a = app_on.read(cx);
            (a.sidebar.files.root.clone(), a.sidebar.view)
        });
        unsafe { std::env::remove_var("VUICK_CONFIG") };
        let _ = std::fs::remove_file(&cfg);
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(
            root_on,
            Some(base.clone()),
            "开启恢复 → 启动恢复上次打开的文件夹"
        );
        assert_eq!(
            view_on,
            crate::app::sidebar::SidebarView::Files,
            "并切到「文件」视图"
        );
    }

    /// 生产热路径回归：目录行**左键单击**（`simulate_click`，
    /// 非程序化 `file_tree_toggle`）→ 展开 + 后台子项扫描落地（children 可见 / pending 清空）；
    /// 再次单击 → 折叠移出 expanded、children 缓存保留。旧实现目录行 on_click 绑定 Sidebar
    /// 实体（回调期间实体已租用 lease#1）→ 宿主 `sidebar_file_tree_toggle` → facade
    /// `sidebar_view.update` 回租同一实体（lease#2）→ `double_lease_panic` 硬崩溃；本用例
    /// 走真实事件链钉死该路径（修复前红 / 修复后绿）。
    #[gpui::test]
    async fn dir_row_click_expands_scans_and_collapses(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let base =
            std::env::temp_dir().join(format!("gpui-tree-rowclick-{}", std::process::id()));
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("sub/nested.txt"), "NEST").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_folder_path(base.clone(), window, cx));
        });
        ft_wait_tree_loaded(&app, cx, &base);
        ft_stabilize(cx);
        let sub = base.join("sub");
        // 行序：root=0、sub=1（子项未加载前仅渲染已扫描的根层）。
        let row = cx.debug_bounds("sidebar-file-row-1").expect("sub 目录行应渲染");
        // 单击目录行 → 展开（行 on_click → SidebarHost::sidebar_file_tree_toggle 宿主反调）。
        cx.simulate_click(row.center(), Default::default());
        ft_wait_tree_loaded(&app, cx, &sub);
        ft_stabilize(cx);
        let (expanded, kids) = cx.update(|_w, cx| {
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
        assert!(expanded, "单击目录行应把 sub 加入 expanded");
        assert_eq!(kids.len(), 1, "单击展开后应后台扫描落地子项");
        assert_eq!(kids[0].name, "nested.txt");
        assert!(
            cx.debug_bounds("sidebar-file-row-2").is_some(),
            "sub 展开后子树应物化渲染（nested.txt 行）"
        );
        // 再次单击同一行 → 折叠（移出 expanded；children 缓存保留复用，不重扫）。
        let row2 = cx.debug_bounds("sidebar-file-row-1").expect("sub 行应仍在");
        cx.simulate_click(row2.center(), Default::default());
        ft_stabilize(cx);
        let (collapsed, kids_kept) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                !a.sidebar.files.expanded.contains(&sub),
                a.sidebar.files.children.contains_key(&sub),
            )
        });
        assert!(collapsed, "再次单击应折叠 sub（移出 expanded）");
        assert!(kids_kept, "折叠应保留 children 缓存（复用不重扫）");
        assert!(
            cx.debug_bounds("sidebar-file-row-2").is_none(),
            "折叠后子树行应不再渲染"
        );
        // 根行同走「目录行」on_click 热路径（含根行）：open_folder_path 已把根
        // 展开，单击根行 → 折叠根（expanded 移出根；根行恒可见仍渲染）。
        let root_row = cx.debug_bounds("sidebar-file-row-0").expect("根行应渲染");
        cx.simulate_click(root_row.center(), Default::default());
        ft_stabilize(cx);
        let root_collapsed = cx.update(|_w, cx| {
            !app.read(cx).sidebar.files.expanded.contains(&base)
        });
        assert!(root_collapsed, "单击根行应折叠根目录");
        assert!(
            cx.debug_bounds("sidebar-file-row-0").is_some(),
            "折叠根后根行仍渲染（树根恒可见）"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// P1-34 回归：重命名文件后，已打开选项卡**查看器内部路径**须随 `payload.file_path`
    /// 同步到新名——图片查看器 `current_path()`（PDF 同径 `path()`）不再残留旧名
    /// （否则 PDF 后续保存写回旧名、图片同文件夹导航/预取指到已不存在的旧文件）。
    #[gpui::test]
    async fn rename_syncs_viewer_internal_path(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir =
            std::env::temp_dir().join(format!("vuick-app-rename-sync-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("a.jpg");
        std::fs::write(&old, b"not really a jpeg").unwrap();
        // 图片查看器装到 add_app 的初始空选项卡 0：load_media 立即建 viewer，当前显示
        // 路径可同步断言（不依赖真实解码成功）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.load_media(0, &old, MediaKind::Image, window, cx);
            });
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let img_entity = cx.update(|_w, cx| {
            let a = app.read(cx);
            match &a.model.borrow().tabs[0].payload.reader {
                MediaReader::Image(iv) => iv.clone(),
                _ => panic!("选项卡 0 应为图片查看器"),
            }
        });
        let before = cx.update(|_w, cx| img_entity.read(cx).current_path());
        assert_eq!(before, Some(old.clone()), "打开后查看器内部路径应为旧名");
        // 重命名 a.jpg → b.jpg（走弹层提交入口 = 生产路径）。
        let new = dir.join("b.jpg");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let kind = crate::app::file_tree_prompt::FileTreePromptKind::Rename {
                    path: old.clone(),
                    is_dir: false,
                };
                let r = app.file_tree_submit_prompt(&kind, "b.jpg", window, cx);
                assert!(r.is_ok(), "重命名应成功");
            });
        });
        for _ in 0..4 {
            cx.run_until_parked();
        }
        assert!(!old.exists() && new.is_file(), "磁盘应改名 a.jpg → b.jpg");
        let after = cx.update(|_w, cx| img_entity.read(cx).current_path());
        assert_eq!(
            after,
            Some(new.clone()),
            "重命名后查看器内部路径应同步为新名（P1-34）"
        );
        let payload_path = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[0].payload.file_path.clone()
        });
        assert_eq!(payload_path, Some(new.clone()), "选项卡 payload 路径应同步");
        let _ = std::fs::remove_dir_all(&dir);
    }
