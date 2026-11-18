    // ---------------------------------------------------------------------------
    // 选项卡右键管理菜单（关闭组 / 移入·复制新窗口）+ 空白区右键分离 回归
    // ---------------------------------------------------------------------------

    /// 右键选项卡 → 选项卡管理菜单：11 项 + 3 分割线（layout_probe (14,3)），全部新
    /// debug_selector 存在（未命名 tab 下「复制路径/复制相对路径/在资源管理器中显示/
    /// Reveal in Explorer View」四项禁用）；**不**弹出栏级「新建/打开」菜单
    /// （stop_propagation + 复用同一 overlay/菜单槽位、以条目区分）。
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
        let (probe, d_copy_path, d_copy_rel, d_reveal, d_reveal_view) = cx.update(|_w, cx| {
            let e = app
                .read(cx)
                .tab_bar_menu
                .entity
                .as_ref()
                .expect("选项卡菜单应打开");
            (
                e.read(cx).layout_probe(),
                e.read(cx).is_item_disabled(9),
                e.read(cx).is_item_disabled(10),
                e.read(cx).is_item_disabled(12),
                e.read(cx).is_item_disabled(13),
            )
        });
        assert_eq!(
            probe,
            (14, 3),
            "选项卡菜单 = 11 项 + 3 分割线（总条目 14），实际={probe:?}"
        );
        for sel in [
            "tab-menu-close",
            "tab-menu-close-others",
            "tab-menu-close-right",
            "tab-menu-close-saved",
            "tab-menu-close-all",
            "tab-menu-move-new-window",
            "tab-menu-copy-new-window",
            "tab-menu-copy-path",
            "tab-menu-copy-rel-path",
            "tab-menu-reveal-explorer",
            "tab-menu-reveal-explorer-view",
        ] {
            assert!(cx.debug_bounds(sel).is_some(), "选项卡菜单应含 {sel}");
        }
        // 初始 tab 未命名（无路径）→ 复制/定位四项禁用（is_item_disabled 下标含分割线：
        // 9=复制路径、10=复制相对路径、12=在资源管理器中显示、13=Reveal in Explorer View）。
        assert!(
            d_copy_path && d_copy_rel && d_reveal && d_reveal_view,
            "未命名 tab：复制/定位四项应禁用，实际=({d_copy_path},{d_copy_rel},{d_reveal},{d_reveal_view})"
        );
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

    /// 「复制路径 / 复制相对路径」：右键文件 tab → 点菜单项 → 剪贴板断言（绝对路径 /
    /// 相对已打开文件夹根、原生分隔符、无引号），与文件树右键同口径（宿主复用
    /// `file_tree_copy_path/relative`）；点后菜单关闭（`tab_bar_menu` 槽位清空）。
    #[gpui::test]
    async fn tab_menu_copy_path_and_relative_path(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!("gpui-tab-cp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("b.txt");
        std::fs::write(&target, "B").unwrap();
        // 预置文件夹根 + 打开文件：tabs = [未命名(0), b.txt(1)]，相对根 = dir。
        cx.update(|_w, cx| {
            app.update(cx, |a, _cx| {
                a.sidebar.files.set_root(Some(dir.clone()));
            });
        });
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(target.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 备份原文本剪贴板（复制路径会覆盖真实剪贴板），测试末还原避免污染用户剪贴板。
        let orig_clip: Option<String> = cx.update(|_w, cx| {
            cx.read_from_clipboard()
                .and_then(|i| i.text().map(|s| s.to_string()))
        });

        // 右键文件 tab（tab-1）→ 点「复制路径」→ 剪贴板 = 绝对路径（无引号）。
        let file_tab = cx.debug_bounds("tab-1").expect("文件 tab 应渲染");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: file_tab.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let abs_item = cx.debug_bounds("tab-menu-copy-path").expect("复制路径项应存在");
        cx.simulate_click(abs_item.center(), Default::default());
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let text: Option<String> = cx.update(|_w, cx| {
            cx.read_from_clipboard()
                .map(|i| i.text().unwrap_or_default())
        });
        assert_eq!(
            text.as_deref(),
            Some(target.to_string_lossy().as_ref()),
            "复制路径应为绝对路径且不带引号"
        );
        assert!(
            !cx.update(|_w, cx| app.read(cx).tab_bar_menu.is_open()),
            "点菜单项后选项卡菜单应关闭"
        );

        // 重新右键 → 点「复制相对路径」→ 相对根 = b.txt。
        let file_tab2 = cx.debug_bounds("tab-1").expect("文件 tab 仍在");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: file_tab2.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let rel_item = cx
            .debug_bounds("tab-menu-copy-rel-path")
            .expect("复制相对路径项应存在");
        cx.simulate_click(rel_item.center(), Default::default());
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let rel: Option<String> = cx.update(|_w, cx| {
            cx.read_from_clipboard()
                .map(|i| i.text().unwrap_or_default())
        });
        assert_eq!(
            rel.as_deref(),
            Some("b.txt"),
            "复制相对路径应相对已打开文件夹根（原生分隔符、无引号）"
        );
        // 无 root 时退化绝对路径：清根 → 重开菜单 → 点「复制相对路径」→ 绝对路径。
        cx.update(|_w, cx| {
            app.update(cx, |a, _cx| a.sidebar.files.set_root(None));
        });
        let file_tab3 = cx.debug_bounds("tab-1").expect("文件 tab 仍在");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: file_tab3.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let rel_item2 = cx
            .debug_bounds("tab-menu-copy-rel-path")
            .expect("复制相对路径项应存在");
        cx.simulate_click(rel_item2.center(), Default::default());
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let rel_no_root: Option<String> = cx.update(|_w, cx| {
            cx.read_from_clipboard()
                .map(|i| i.text().unwrap_or_default())
        });
        assert_eq!(
            rel_no_root.as_deref(),
            Some(target.to_string_lossy().as_ref()),
            "无根时复制相对路径应退化复制绝对路径"
        );
        // 还原测试前剪贴板文本（有原内容才写回）。
        if let Some(text) = orig_clip {
            cx.update(|_w, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「在资源管理器中显示」：右键**文件** tab → 点菜单项 → 宿主
    /// `file_tree_reveal_in_explorer` 被调（headless 只能断言到「菜单关闭 = 回调经 activate
    /// 执行」+ 不 panic；真实 explorer 进程无法 headless 断言）；**未命名** tab 下点禁用项
    /// 「复制路径」→ 菜单保持打开（禁用不响应点击）。「Reveal in Explorer View」：文件 tab
    /// 有路径但**未打开文件夹**（root None）→ 该项仍禁用（无树可定位）。
    #[gpui::test]
    async fn tab_menu_reveal_explorer_and_untitled_disabled(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 未命名 tab（tab-0）→ 四项禁用：点禁用「复制路径」不加事件、菜单保持打开。
        let untitled_tab = cx.debug_bounds("tab-0").expect("未命名 tab 应渲染");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: untitled_tab.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let disabled = cx.update(|_w, cx| {
            let e = app
                .read(cx)
                .tab_bar_menu
                .entity
                .as_ref()
                .expect("选项卡菜单应打开");
            (
                e.read(cx).is_item_disabled(9),
                e.read(cx).is_item_disabled(10),
                e.read(cx).is_item_disabled(12),
                e.read(cx).is_item_disabled(13),
            )
        });
        assert!(
            disabled.0 && disabled.1 && disabled.2 && disabled.3,
            "未命名 tab 复制/定位四项应禁用，实际={disabled:?}"
        );
        let disabled_item = cx.debug_bounds("tab-menu-copy-path").expect("禁用项仍在场");
        cx.simulate_click(disabled_item.center(), Default::default());
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| app.read(cx).tab_bar_menu.is_open()),
            "点禁用项不应关闭菜单（禁用不响应点击）"
        );
        cx.simulate_keystrokes("escape");
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 打开文件 tab-1（**未开文件夹/root None**）：复制/资源管理器定位三项启用；
        // 「Reveal in Explorer View」仍禁用（root None）；点「在资源管理器中显示」→ 菜单
        // 关闭（host 被调、不 panic）。
        let f = std::env::temp_dir().join("gpui-tab-rev.txt");
        std::fs::write(&f, "R").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(f.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let file_tab = cx.debug_bounds("tab-1").expect("文件 tab 应渲染");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: file_tab.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        let enabled = cx.update(|_w, cx| {
            let e = app
                .read(cx)
                .tab_bar_menu
                .entity
                .as_ref()
                .expect("选项卡菜单应打开");
            (
                e.read(cx).is_item_disabled(9),
                e.read(cx).is_item_disabled(10),
                e.read(cx).is_item_disabled(12),
                e.read(cx).is_item_disabled(13),
            )
        });
        assert!(
            !enabled.0 && !enabled.1 && !enabled.2 && enabled.3,
            "文件 tab（root None）：复制三项启用、Reveal in Explorer View 禁用，实际={enabled:?}"
        );
        let reveal = cx
            .debug_bounds("tab-menu-reveal-explorer")
            .expect("在资源管理器中显示项应存在");
        cx.simulate_click(reveal.center(), Default::default());
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            !cx.update(|_w, cx| app.read(cx).tab_bar_menu.is_open()),
            "点「在资源管理器中显示」后菜单应关闭（回调经 activate 执行、不 panic）"
        );
        let _ = std::fs::remove_file(&f);
    }

    /// 「Reveal in Explorer View」E2E：打开嵌套文件夹 + 文件 tab → 右键文件 tab →
    /// 点菜单项 → 侧边栏切到 Files 视图并打开、目标全部祖先目录展开（含异步补扫）、
    /// 瞬态选中 = 目标文件、滚动定位到目标行（offset ≈ min(idx×ROW_H, max_offset)，
    /// 期望值从真实行序 + 最终 max_offset 推导，K177）。
    #[gpui::test]
    async fn tab_menu_reveal_in_explorer_view_opens_tree_highlights_scrolls(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 嵌套树根（多层 root/a/b/target.md）：a 下旁加 50 兄弟文件撑高使滚动真实发生；
        // a/b 下 target 之后再放 z00..z19 使 target 非末行、其行顶远小于「内容高−视口高」。
        let dir = std::env::temp_dir().join(format!("gpui-tab-rev-view-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/target.md"), "T").unwrap();
        for i in 0..50 {
            std::fs::write(dir.join(format!("a/fill{i:02}.txt")), "F").unwrap();
        }
        for i in 0..20 {
            std::fs::write(dir.join(format!("a/b/z{i:02}.md")), "Z").unwrap();
        }
        // 开文件夹（root 扫描 + 展开根）+ 开目标文件成 tab-1。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_folder_path(dir.clone(), window, cx);
                app.open_file(dir.join("a/b/target.md").to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let a_ready =
                cx.update(|_w, cx| app.read(cx).sidebar.files.children.contains_key(&dir.join("a")));
            if a_ready {
                break;
            }
        }
        // 右键文件 tab → 点「Reveal in Explorer View」。
        let tab = cx.debug_bounds("tab-1").expect("target tab 应渲染");
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
        let item = cx
            .debug_bounds("tab-menu-reveal-explorer-view")
            .expect("Reveal in Explorer View 项应存在");
        cx.simulate_click(item.center(), Default::default());
        for _ in 0..4 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
        }
        assert!(
            !cx.update(|_w, cx| app.read(cx).tab_bar_menu.is_open()),
            "点「Reveal in Explorer View」后菜单应关闭"
        );
        // 泵到全部在途扫描完成 + 目标行进入可见行表（异步「先扫后滚」由行表即真相保证）。
        let target = dir.join("a/b/target.md");
        let mut target_idx = None;
        for _ in 0..300 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.run_until_parked();
            let probe = cx.update(|_w, cx| {
                let a = app.read(cx);
                if !a.sidebar.files.pending.is_empty() {
                    return None;
                }
                a.sidebar.files_layout
                    .as_ref()
                    .and_then(|(_, rows)| rows.iter().position(|r| r.path == target))
            });
            if let Some(i) = probe {
                target_idx = Some(i);
                break;
            }
        }
        let idx = target_idx.expect("扫描完成后目标行应进入可见行表");
        // 断言：Files 视图打开 + 祖先展开 + 瞬态选中 + 待滚已消费。
        let (view, open, panel, expanded_a, expanded_ab, selected, scroll_pending) =
            cx.update(|_w, cx| {
                let a = app.read(cx);
                (
                    a.sidebar.view,
                    a.sidebar.open,
                    a.sidebar.panel_open,
                    a.sidebar.files.expanded.contains(&dir.join("a")),
                    a.sidebar.files.expanded.contains(&dir.join("a/b")),
                    a.sidebar.files.selected.clone(),
                    a.sidebar.files.scroll_to_selected,
                )
            });
        assert_eq!(
            view,
            crate::app::sidebar::SidebarView::Files,
            "应切到 Files 视图"
        );
        assert!(open && panel, "侧边栏与 Files 面板应打开");
        assert!(expanded_a && expanded_ab, "目标全部祖先目录应展开");
        assert_eq!(
            selected.as_deref(),
            Some(target.as_path()),
            "瞬态选中应为目标文件"
        );
        assert!(!scroll_pending, "目标行可枚举后待滚标记应被消费");
        // 断言：滚动偏移 ≈ 目标行顶（idx×ROW_H）取 clamp 到最终 max_offset（容差 2px）。
        let (off_y, max_y) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.sidebar.files_vscroll.offset().y, a.sidebar.files_vscroll.max_offset().y)
        });
        let expected = (idx as f32 * shell_sidebar::state::FILE_TREE_ROW_H).min(max_y.as_f32());
        assert!(
            (off_y.as_f32() + expected).abs() < 2.0,
            "滚动偏移应 ≈ 目标行顶 clamp：idx={idx} expected=-{expected} 实际={off_y:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
