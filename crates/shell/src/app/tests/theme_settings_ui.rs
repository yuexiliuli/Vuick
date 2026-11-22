
    /// 「主题…」预设选择端到端（`set_theme_preset`）：写 `model.theme_preset` →
    /// 走真实 `refresh_theme_ui` → `Theme::global` accent 变为该预设对应值（音乐紫暗档
    /// #7C5CFC）→ prefs 汇总往返一致（theme_hot.rs 同姿势）。判别式（K201）：撤掉
    /// `set_theme_preset` 里的 `refresh_theme_ui` 本测试即红。
    #[gpui::test]
    fn set_theme_preset_applies_and_persists(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                a.model.borrow_mut().dark_mode = true;
                a.set_theme_preset("music", cx);
            });
        });
        cx.update(|_w, cx| {
            let a = app.read(cx);
            assert_eq!(
                a.model.borrow().theme_preset,
                "music",
                "预设写入 model.theme_preset"
            );
            let t = ui::theme::Theme::global(cx);
            assert_eq!(
                t.colors.accent,
                gpui::rgba(0x7c5cfcff),
                "音乐紫暗档 accent #7C5CFC 经 apply 生效"
            );
            let prefs = a.collect_prefs();
            let back = UserPrefs::from_json_value(&prefs.to_json_value());
            assert_eq!(back.theme_preset, "music", "theme_preset 落盘往返一致");
        });
    }

    /// 自定义重点色覆盖 + 还原（`set_theme_accent`/`reset_theme_accent`）：
    /// 写 `theme_custom["colors.accent"]` → `Theme::global` accent == 该值（叠加在当前
    /// preset 上，不新建主题）；还原 → 删 key → accent 回落 preset 值（ocean 暗档
    /// #2DD4BF）。判别式（K201）：撤掉「还原=回落 preset」分支本测试即红。
    #[gpui::test]
    fn set_theme_accent_overrides_and_reset_restores_preset(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                a.model.borrow_mut().dark_mode = true;
                a.set_theme_preset("ocean", cx);
                a.set_theme_accent("#ff0000".to_string(), cx);
            });
        });
        cx.update(|_w, cx| {
            let a = app.read(cx);
            assert_eq!(
                a.model
                    .borrow()
                    .theme_custom
                    .get("colors.accent")
                    .map(String::as_str),
                Some("#ff0000"),
                "自定义重点色写入 theme_custom[\"colors.accent\"]"
            );
            let t = ui::theme::Theme::global(cx);
            assert_eq!(
                t.colors.accent,
                gpui::rgba(0xff0000ff),
                "自定义 accent 覆盖 preset accent"
            );
        });
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| a.reset_theme_accent(cx));
        });
        cx.update(|_w, cx| {
            let a = app.read(cx);
            assert!(
                !a.model.borrow().theme_custom.contains_key("colors.accent"),
                "还原后 theme_custom 删 key"
            );
            let t = ui::theme::Theme::global(cx);
            assert_eq!(
                t.colors.accent,
                gpui::rgba(0x2dd4bfff),
                "还原后 accent 回落 preset（ocean 暗档 #2DD4BF）"
            );
        });
    }

    /// 非法 hex 不落盘、保持最近有效值（`set_theme_accent` 前置校验）：
    /// 非 6 位 hex / 非 hex → 不改 `theme_custom`、不改 `Theme::global` accent、
    /// prefs 落盘仍为最近有效值（评审 P1-7：保留最近有效值语义）。
    #[gpui::test]
    fn invalid_hex_not_persisted_and_keeps_previous(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                a.model.borrow_mut().dark_mode = true;
                a.set_theme_accent("#00ff00".to_string(), cx);
                a.set_theme_accent("not-a-color".to_string(), cx);
                a.set_theme_accent("#12345".to_string(), cx);
            });
        });
        cx.update(|_w, cx| {
            let a = app.read(cx);
            assert_eq!(
                a.model
                    .borrow()
                    .theme_custom
                    .get("colors.accent")
                    .map(String::as_str),
                Some("#00ff00"),
                "非法 hex 覆盖被拒，保持最近有效值"
            );
            let t = ui::theme::Theme::global(cx);
            assert_eq!(
                t.colors.accent,
                gpui::rgba(0x00ff00ff),
                "Theme accent 仍为最近有效值"
            );
            let prefs = a.collect_prefs();
            let back = UserPrefs::from_json_value(&prefs.to_json_value());
            assert_eq!(
                back.theme_custom.get("colors.accent").map(String::as_str),
                Some("#00ff00"),
                "落盘仍为最近有效值（非法值不入 prefs）"
            );
        });
    }

    /// 取色入口（K167 姿势）：测试/非 windows 构建下 `pick_custom_theme_accent` 走
    /// no-op 分支（不弹原生模态、不碰模型、不 panic）——headless 只能证「机制路径不崩」；
    /// 生产路径（win32 CHOOSECOLORW）后台线程 + oneshot 回传主线程再 `set_theme_accent`
    /// 落地，其「回传后写 theme_custom + apply」语义由
    /// [`set_theme_accent_overrides_and_reset_restores_preset`] 覆盖。
    #[gpui::test]
    fn pick_custom_theme_accent_headless_noop(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.pick_custom_theme_accent(window, cx));
        });
        cx.update(|_w, cx| {
            let a = app.read(cx);
            assert!(
                a.model.borrow().theme_custom.is_empty(),
                "no-op：不写任何自定义值"
            );
            assert_eq!(
                a.model.borrow().theme_preset,
                "default",
                "no-op：不碰预设"
            );
        });
    }

    /// 「主题…」二级子菜单结构护栏：预设区全部来自 [`ui::theme::PRESETS`]（逐 id 绑定
    /// debug selector `menu-theme-preset-<id>`）+ 1 分隔线 + 自定义重点色 + 还原重点色。
    /// 当前预设 ✓ 前缀由渲染闭包读 `model.theme_preset` 实时决定（`set_theme_preset` 后
    /// reopen 即移位——行为链已在 `set_theme_preset_applies_and_persists` 断言）。
    #[gpui::test]
    fn theme_submenu_flyout_matches_presets(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|_w, cx| {
            app.update(cx, |_a, cx| {
                let weak = cx.weak_entity();
                let items = EditorApp::theme_flyout_items(&weak);
                let expected = ui::theme::PRESETS.len() + 3; // 6 预设 + 分隔线 + 自定义 + 还原
                assert_eq!(
                    items.len(),
                    expected,
                    "「主题…」子菜单 = PRESETS 全部预设 + 分隔线 + 自定义/还原"
                );
            });
        });
    }
