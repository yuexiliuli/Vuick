    /// R3：`toggle_close_to_tray` 翻转 `close_to_tray` 字段并触发持久化——
    /// 测试构建下 `persist_prefs` 短路不写盘（chrome.rs）、`tray::set_close_to_tray`
    /// 只翻 flag 不真建托盘图标（tray crate 文档），故此处断言字段翻转 + tray 状态联动。
    #[gpui::test]
    async fn toggle_close_to_tray_flips_field(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                // K8：`add_app` 经 `EditorApp::new` → `new_impl`（app/init.rs:80
                // `prefs.close_to_tray.unwrap_or(false)`）直读真实 %APPDATA% 的 config
                // （config crate 作依赖编译不带自身 cfg(test)，照常读盘）——真机配置可能含
                // 用户开启过的 "close_to_tray":true，测试须与真机配置解耦：显式归零默认关
                // 再跑翻转断言，不断言从真实 config 读到的初始值。
                app.close_to_tray = false;
                app.toggle_close_to_tray(window, cx);
                assert!(app.close_to_tray, "点击一次 → 开");
                // tray crate 状态联动：开关已翻、主窗口已知 → 应判定「点 × 隐藏到托盘」。
                tray::set_main_hwnd(12345);
                assert!(tray::should_close_to_tray(), "开关开且主窗口已知 → 进托盘");
                app.toggle_close_to_tray(window, cx);
                assert!(!app.close_to_tray, "再点 → 关");
                assert!(!tray::should_close_to_tray(), "开关关 → 点 × 真退出");
                // 收尾清掉测试进程内的托盘全局状态，避免污染其它 shell 测试。
                tray::set_main_hwnd(0);
                tray::set_close_to_tray(false);
            });
        });
    }

    /// R4：托盘开关菜单标签动态 ☑/☐（复用 session_restore_menu_item 的复选模式）。
    #[test]
    fn close_to_tray_menu_label_marks() {
        assert_eq!(
            EditorApp::close_to_tray_label(true),
            "☑ 关闭时最小化到系统托盘",
            "开启态标签带 ☑"
        );
        assert_eq!(
            EditorApp::close_to_tray_label(false),
            "☐ 关闭时最小化到系统托盘",
            "关闭态标签带 ☐"
        );
    }
