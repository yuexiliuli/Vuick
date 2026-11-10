    // ---- 音乐播放器集成测试（app::tests::music）----
    // K20/K30 口径：不触碰真实音频播放/设备；MusicView::new / load_dir 纯装配可测，
    // 播放状态由 crates/music 自带集成测试兜底（headless 只证机制）。

    /// 测试专用临时目录唯一后缀（进程内自增，避免并行测试写同一临时文件竞态）。
    static MUSIC_TMP_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn music_tmp_suffix() -> String {
        let seq = MUSIC_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{}-{seq}", std::process::id())
    }

    /// 测试用空音乐目录（隔离真实 `D:\music\music`，避免 toggle 时扫描真实曲库触发
    /// 逐个真实解码探针，慢且非确定）。
    fn music_empty_dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("gpui-music-empty-{}", music_tmp_suffix()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// 把 `GPUI_EDITOR_CONFIG` 指向「复制现有配置 + `music_dir`→空目录」的临时配置，隔离
    /// `MusicView::load_default_dir` 的目录来源；返回临时配置文件路径（测试结束恢复并删除）。
    /// 现有配置损坏/不存在 → 以仅含 music_dir 的对象兜底。
    fn music_isolate_config() -> std::path::PathBuf {
        let real = config::user::UserPrefs::config_path();
        let mut value = std::fs::read_to_string(&real)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        if let serde_json::Value::Object(map) = &mut value {
            map.insert(
                "music_dir".to_string(),
                serde_json::Value::String(music_empty_dir().to_string_lossy().into_owned()),
            );
        }
        let cfg = std::env::temp_dir().join(format!(
            "gpui-music-config-{}.json",
            music_tmp_suffix()
        ));
        std::fs::write(&cfg, value.to_string()).unwrap();
        unsafe { std::env::set_var("GPUI_EDITOR_CONFIG", &cfg) };
        cfg
    }

    /// 恢复并清理（测试收尾 best-effort；target-only 运行无并行干扰）。
    fn music_restore_config(cfg: &std::path::Path) {
        unsafe { std::env::remove_var("GPUI_EDITOR_CONFIG") };
        let _ = std::fs::remove_file(cfg);
    }

    /// 音乐模式切换：toggle 开 → music_mode true 且 MusicView 已创建、真实渲染路径
    /// 命中（`music-root-shell` 壳被布局）；再 toggle → 关回编辑器界面。
    #[gpui::test]
    async fn toggle_music_enters_and_exits_mode(cx: &mut TestAppContext) {
        let cfg = music_isolate_config();
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        // 进入音乐模式
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_music_player(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let (mode, has_view) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().music_mode, a.music.is_some())
        });
        assert!(mode, "toggle 开应进入音乐模式");
        assert!(has_view, "进入音乐模式应已创建 MusicView");
        assert!(
            cx.debug_bounds("music-root-shell").is_some(),
            "音乐模式应渲染 music-root-shell 壳（真实渲染路径）"
        );
        // 退出音乐模式
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_music_player(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let mode2 = cx.update(|_w, cx| app.read(cx).model.borrow().music_mode);
        assert!(!mode2, "再次 toggle 应退出音乐模式");
        music_restore_config(&cfg);
    }

    /// 打开音频文件（.ogg 占位内容，路由只看扩展名）→ 整窗切音乐模式并请求播放；
    /// 不创建文本选项卡（`open_file` 源头拦截，覆盖文件树双击/对话框/拖放等全部入口）。
    #[gpui::test]
    async fn open_ogg_routes_to_music_mode(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let dir =
            std::env::temp_dir().join(format!("gpui-music-route-{}", music_tmp_suffix()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("示例曲目.ogg");
        std::fs::write(&p, b"OggS placeholder").unwrap();
        let tabs_before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                // 断言对象是「当前窗口音乐模式路由」：显式关掉媒体重定向开关（默认开），
                // 否则 `open_file` 会把音频改投同进程新窗口（见 media_window.rs 行为表）。
                app.model.borrow_mut().media_new_window = false;
                app.open_file(p.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let (mode, tabs, has_view) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().music_mode, a.model.borrow().tabs.len(), a.music.is_some())
        });
        assert!(mode, "打开 .ogg 应进入音乐模式");
        assert_eq!(tabs, tabs_before, "打开音频不应创建文本选项卡");
        assert!(has_view, "打开音频应创建 MusicView");
        assert!(
            cx.debug_bounds("music-root-shell").is_some(),
            "打开音频后应渲染音乐模式壳"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Esc 退出请求（`MusicEvent::ExitRequested`，MusicView 事件）→ 订阅者关闭音乐模式。
    #[gpui::test]
    async fn exit_requested_turns_off_mode(cx: &mut TestAppContext) {
        let cfg = music_isolate_config();
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_music_player(window, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        assert!(
            cx.update(|_w, cx| app.read(cx).model.borrow().music_mode),
            "前置：应先进入音乐模式"
        );
        // 直接对 MusicView emit ExitRequested → 订阅者应把 music_mode 置 false。
        cx.update(|_w, cx| {
            let view = app.read(cx).music.clone().expect("应已创建 MusicView");
            view.update(cx, |_v, cx| cx.emit(music::MusicEvent::ExitRequested));
        });
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let mode = cx.update(|_w, cx| app.read(cx).model.borrow().music_mode);
        assert!(!mode, "ExitRequested 应退出音乐模式");
        music_restore_config(&cfg);
    }
