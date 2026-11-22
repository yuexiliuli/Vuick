
    use std::collections::BTreeMap;

    /// 零回归锚（P1-1 判别式）：default 预设 + 空 custom 的 `Theme::apply` 必须与旧
    /// `Theme::set(mode)` 逐字节一致——撤掉 apply 改回 set 本测试即 pass；反之若 apply
    /// 暗改了默认档合并产物，本测试红 = 观感回归的第一道哨兵。
    #[gpui::test]
    fn theme_apply_default_preset_is_zero_regression(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ui::theme::Theme::apply(
                ui::theme::ThemeSpec {
                    mode: ui::theme::ThemeMode::Light,
                    preset: &ui::theme::PRESETS[0],
                    custom: BTreeMap::new(),
                },
                cx,
            );
            let t = ui::theme::Theme::global(cx);
            assert_eq!(t.mode, ui::theme::ThemeMode::Light);
            assert_eq!(
                t.colors,
                ui::theme::ColorScheme::light(),
                "default+空 custom 合并 = base 亮档（零覆盖零漂移）"
            );
            assert_eq!(
                t.chrome,
                ui::theme::ChromeColors::light(),
                "外壳配色 = 旧默认亮档（P1-1 判别式）"
            );
        });
        cx.update(|cx| {
            ui::theme::Theme::apply(
                ui::theme::ThemeSpec {
                    mode: ui::theme::ThemeMode::Dark,
                    preset: &ui::theme::PRESETS[0],
                    custom: BTreeMap::new(),
                },
                cx,
            );
            let t = ui::theme::Theme::global(cx);
            assert_eq!(t.colors, ui::theme::ColorScheme::dark());
            assert_eq!(t.chrome, ui::theme::ChromeColors::dark());
        });
    }

    /// 热更走通（P1-1 端到端）：经真实 `refresh_theme_ui` 落地 music 预设 + custom 重点色
    /// / chrome 覆写，全局主题与「外壳 chrome 消费方」同时反映——证明「有效主题
    /// （mode+preset+custom）」成套生效且自定义重点色对外壳全程可见。
    #[gpui::test]
    fn refresh_theme_ui_hot_applies_preset_and_custom(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                let mut custom = BTreeMap::new();
                custom.insert("colors.accent".to_string(), "#ff0000".to_string());
                custom.insert("chrome.window_bg".to_string(), "#112233".to_string());
                a.model.borrow_mut().dark_mode = true;
                a.model.borrow_mut().theme_preset = "music".to_string();
                a.model.borrow_mut().theme_custom = custom;
                a.refresh_theme_ui(cx);
                cx.notify();
            });
        });
        cx.update(|_w, cx| {
            let t = ui::theme::Theme::global(cx);
            assert!(t.is_dark(), "mode 随 model.dark_mode");
            assert_eq!(t.colors.accent, gpui::rgba(0xff0000ff), "custom accent 覆盖 preset accent");
            assert_eq!(t.colors.srf_deep, gpui::rgba(0x0d0b1aff), "preset music 暗档深紫坡仍生效（#0D0B1A）");
            assert_eq!(t.chrome.window_bg, gpui::rgba(0x112233ff), "custom chrome.window_bg 覆写生效");
            // P1-1：外壳消费方透明改读 Theme::global(cx).chrome → 自定义 chrome 覆盖
            // 对 `window_bg()` 全程可见（此前经 model.dark_mode 派生看不到 custom）。
            let bg: gpui::Hsla = app.read(cx).window_bg(cx);
            assert_eq!(bg, gpui::rgba(0x112233ff).into(), "外壳消费方窗口底色 = 自定义 chrome");
        });
    }

    /// prefs 往返（persist/load 的纯函数面）：改 model.theme_preset / theme_custom /
    /// music_skin → collect_prefs 汇总 → 序列化 → 反序列化 → 值一致。测试构建下
    /// persist_prefs 被 `AppHost::is_test_build()` 短路落盘、`UserPrefs::load()` 读隔离
    /// 临时目录——序列化往返即覆盖「改值 → collect → 持久化口径 → 恢复」同一链路。
    #[gpui::test]
    fn theme_prefs_collect_roundtrip(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|_w, cx| {
            app.update(cx, |a, _cx| {
                let mut custom = BTreeMap::new();
                custom.insert("colors.accent".to_string(), "#ff0000".to_string());
                a.model.borrow_mut().theme_preset = "ocean".to_string();
                a.model.borrow_mut().theme_custom = custom;
                a.model.borrow_mut().music_skin = "music".to_string();

                let prefs = a.collect_prefs();
                let back = UserPrefs::from_json_value(&prefs.to_json_value());
                assert_eq!(back.theme_preset, "ocean", "theme_preset 往返一致");
                assert_eq!(
                    back.theme_custom.get("colors.accent").map(String::as_str),
                    Some("#ff0000"),
                    "theme_custom 往返一致"
                );
                assert_eq!(back.music_skin, "music", "music_skin 往返一致");
            });
        });
    }
