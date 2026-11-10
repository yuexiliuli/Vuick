    // ---- 媒体窗口打开重定向（app::tests::media_window）----
    // 路由断言只依赖「扩展名 → 分流 → 窗口/选项卡装配」的同步结果，不依赖解码成功：
    // 视频用占位假字节 .mp4（解码失败停在占位，见 tabs_actions 同款姿势），音频用占位
    // .ogg（路由只看扩展名）。「媒体窗口」定义 = music_mode 或 active_tab 为视频。

    /// 只读全局注册表的窗口总数（EditorAppRegistry 在每次构造时登记）。
    fn registry_window_count(cx: &mut TestAppContext) -> usize {
        cx.update(|cx| cx.global::<EditorAppRegistry>().windows.len())
    }

    /// 快速读取某窗口的可观测状态：(music 模式, 活动选项卡文件路径, tab 数)。
    fn media_win_snap(
        cx: &mut TestAppContext,
        e: &gpui::Entity<EditorApp>,
    ) -> (bool, Option<std::path::PathBuf>, usize) {
        cx.update(|cx| {
            let a = e.read(cx);
            let model = a.model.borrow();
            let tab = model.tabs.get(model.active_tab);
            (
                a.model.borrow().music_mode,
                tab.and_then(|t| t.file_path.clone()),
                a.model.borrow().tabs.len(),
            )
        })
    }

    /// 活动选项卡是否为视频（「媒体窗口」判定的视频半边）。
    #[cfg(feature = "video")]
    fn active_is_video(cx: &mut TestAppContext, e: &gpui::Entity<EditorApp>) -> bool {
        cx.update(|cx| {
            let a = e.read(cx);
            a.model.borrow().tabs
                .get(a.model.borrow().active_tab)
                .map(|t| matches!(t.reader, MediaReader::Video(_)))
                .unwrap_or(false)
        })
    }

    /// 按实体 id 过滤出「非给定窗口」的注册表强实体（找另一窗口用）。
    fn other_app_entity(
        cx: &mut TestAppContext,
        exclude: &gpui::Entity<EditorApp>,
    ) -> Vec<gpui::Entity<EditorApp>> {
        cx.update(|cx| {
            let reg = cx.global::<EditorAppRegistry>();
            reg.windows
                .iter()
                .filter_map(|(weak, _)| {
                    let e = weak.upgrade()?;
                    (e.entity_id() != exclude.entity_id()).then_some(e)
                })
                .collect()
        })
    }

    /// 占位视频文件（假字节，解码失败停在占位；路由只看扩展名，见 tabs_actions 同款）。
    #[cfg(feature = "video")]
    fn placeholder_video(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gpui-mediawnd-v-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&d);
        let p = d.join(format!("{tag}.mp4"));
        std::fs::write(&p, b"not really a video").unwrap();
        p
    }

    /// 占位音频文件（.ogg，路由只看扩展名）。
    fn placeholder_audio(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gpui-mediawnd-a-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&d);
        let p = d.join(format!("{tag}.ogg"));
        std::fs::write(&p, b"OggS placeholder").unwrap();
        p
    }

    /// 纯决策函数 `resolve_media_open_target` 与行为表逐行一致（16 组真值全覆盖，
    /// 其中「关(any)」8 组、开分支 8 组）。
    #[test]
    fn media_window_resolve_behavior_table() {
        use super::media_window::MediaOpenTarget::*;
        use super::media_window::resolve_media_open_target;
        for media_new in [false, true] {
            for reuse in [false, true] {
                for self_media in [false, true] {
                    for other in [false, true] {
                        let got =
                            resolve_media_open_target(media_new, reuse, self_media, other);
                        // 逐行按行为表推导期望（与实现同表，防改乱回归断言）。
                        let want = if !media_new {
                            Here
                        } else if !reuse {
                            NewWindow
                        } else if self_media {
                            Here
                        } else if other {
                            ReuseExisting
                        } else {
                            NewWindow
                        };
                        assert_eq!(
                            got, want,
                            "行为表: media_new={media_new} reuse={reuse} self_media={self_media} other={other}"
                        );
                    }
                }
            }
        }
    }

    /// `resolve_recorded_size` 取值语义（delta：主/媒体窗口发起、盘上/内存最新性）：
    /// 媒体发起者只信自身内存（防抖未落盘也正确）；非媒体发起者盘上最新优先、内存兜底。
    #[test]
    fn media_window_recorded_size_resolution() {
        use super::media_window::{MediaWindowKind, resolve_recorded_size};
        let m = gpui::size(px(800.), px(600.));
        let v = gpui::size(px(900.), px(540.));
        // 媒体发起者：只看自身内存（盘上内容忽略——防抖窗口内盘上尚旧）。
        assert_eq!(
            resolve_recorded_size(
                MediaWindowKind::Music,
                true,
                Some(m),
                Some(v),
                Some((100.0, 100.0)),
                Some((200.0, 200.0)),
            ),
            Some(m),
            "媒体发起者用自身内存"
        );
        assert_eq!(
            resolve_recorded_size(MediaWindowKind::Music, true, None, Some(v), Some((100.0, 100.0)), Some((200.0, 200.0))),
            None,
            "媒体发起者内存空 → 不落盘兜底"
        );
        // 非媒体发起者（主窗口）：盘上最新记录优先；
        assert_eq!(
            resolve_recorded_size(MediaWindowKind::Video, false, Some(m), Some(v), Some((100.0, 100.0)), Some((960.0, 540.0))),
            Some(gpui::size(px(960.), px(540.))),
            "主窗口发起：盘上视频最新记录优先"
        );
        // 盘上无记录 → 内存兜底（构造期从 prefs 读入的现值）。
        assert_eq!(
            resolve_recorded_size(MediaWindowKind::Music, false, Some(m), Some(v), None, None),
            Some(m),
            "盘上无 → 内存兜底"
        );
        assert_eq!(
            resolve_recorded_size(MediaWindowKind::Music, false, None, Some(v), None, Some((777.0, 555.0))),
            None,
            "music 只看音乐组：video 组的盘上记录不影响 music（Music mem/disk 均 None → None）"
        );
    }

    /// 行为表 a：配置关（media_new_window=false）→ 打开视频仍在当前窗口（窗口数不变）。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn media_window_config_off_opens_video_in_current_window(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let v1 = placeholder_video("off1");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.model.borrow_mut().media_new_window = false;
                app.model.borrow_mut().reuse_media_window = false;
                app.open_file(v1.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_eq!(registry_window_count(cx), 1, "配置关：窗口数应不变");
        assert!(active_is_video(cx, &app), "视频应在当前窗口打开为活动选项卡");
        assert_eq!(
            media_win_snap(cx, &app).1.as_deref(),
            Some(v1.as_path()),
            "当前窗口活动选项卡应即所开视频"
        );
        let _ = std::fs::remove_file(&v1);
    }

    /// 行为表 c：开+复用开+无媒体窗口 → +1 窗口，主窗口 tabs 不变。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn media_window_config_on_reuse_on_no_media_creates_new_window(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let tabs_before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        let v1 = placeholder_video("new1");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(v1.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_eq!(registry_window_count(cx), 2, "无媒体窗口 → 应新开 1 个窗口");
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            tabs_before,
            "源窗口（非媒体）tabs 应不变"
        );
        let others = other_app_entity(cx, &app);
        assert_eq!(others.len(), 1, "应恰好多出 1 个窗口");
        assert!(
            cx.update(|_w, cx| others[0].read(cx).is_media_window()),
            "新窗口应是媒体窗口（视频活动选项卡）"
        );
        assert!(active_is_video(cx, &others[0]), "新窗口活动选项卡应是该视频");
        let _ = std::fs::remove_file(&v1);
    }

    /// 行为表 b：开+复用开+已有媒体窗口 → 新视频投到该媒体窗口（窗口数不变），
    /// 当前非媒体窗口内容不变。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn media_window_config_on_reuse_on_has_media_window_routes_there(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 先造一个媒体窗口 B：A（非媒体）开视频1 → 重定向新开 B（视频1）。
        let v1 = placeholder_video("b1");
        let v2 = placeholder_video("b2");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(v1.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_eq!(registry_window_count(cx), 2, "前置：应先拆出 1 个媒体窗口");
        let others = other_app_entity(cx, &app);
        assert_eq!(others.len(), 1);
        let media_win = others[0].clone();
        assert!(active_is_video(cx, &media_win), "前置：B 应为视频媒体窗口");

        // 再从 A（非媒体，活动仍是空选项卡）开视频2 → 复用 B，窗口数不变。
        let tabs_before_a = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(v2.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_eq!(
            registry_window_count(cx),
            2,
            "复用已有媒体窗口：窗口数应不变"
        );
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            tabs_before_a,
            "源非媒体窗口内容应不变"
        );
        assert_eq!(
            media_win_snap(cx, &media_win).1.as_deref(),
            Some(v2.as_path()),
            "目标媒体窗口应切到视频2"
        );
        let _ = std::fs::remove_file(&v1);
        let _ = std::fs::remove_file(&v2);
    }

    /// 行为表 d：开+复用开+当前是媒体窗口 → 窗口数不变且当前窗口切换内容。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn media_window_config_on_reuse_on_current_is_media_switches_in_place(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 先把 A 变成媒体窗口：关开关直接在当前窗口开视频1。
        let v1 = placeholder_video("d1");
        let v2 = placeholder_video("d2");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.model.borrow_mut().media_new_window = false;
                app.open_file(v1.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_eq!(registry_window_count(cx), 1);
        assert!(active_is_video(cx, &app), "前置：A 应为视频媒体窗口");

        // 开开关+复用开 → 再开视频2：当前是媒体窗口 → 复用当前（窗口数不变、切换内容）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.model.borrow_mut().media_new_window = true;
                app.model.borrow_mut().reuse_media_window = true;
                app.open_file(v2.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_eq!(registry_window_count(cx), 1, "复用当前媒体窗口：窗口数应不变");
        assert_eq!(
            media_win_snap(cx, &app).1.as_deref(),
            Some(v2.as_path()),
            "当前媒体窗口应切换内容到视频2"
        );
        let _ = std::fs::remove_file(&v1);
        let _ = std::fs::remove_file(&v2);
    }

    /// 行为表 e：开+复用关 → 每次打开视频 +1 窗口（当前播放不被新窗打断）。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn media_window_config_on_reuse_off_opens_new_window_each_time(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let tabs_before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        let v1 = placeholder_video("e1");
        let v2 = placeholder_video("e2");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.model.borrow_mut().reuse_media_window = false;
                app.open_file(v1.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_eq!(registry_window_count(cx), 2, "复用关：第一次应新开窗口");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(v2.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_eq!(registry_window_count(cx), 3, "复用关：每次打开都应新开 1 个窗口");
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            tabs_before,
            "源窗口 tabs 应始终不变"
        );
        let _ = std::fs::remove_file(&v1);
        let _ = std::fs::remove_file(&v2);
    }

    /// 行为表 f：当前是媒体窗口 + 复用关 → +1 窗口且当前播放内容不变。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn media_window_config_on_reuse_off_from_media_window_keeps_current(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // A 先成媒体窗口（关开关直接在当前开视频1）。
        let v1 = placeholder_video("f1");
        let v2 = placeholder_video("f2");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.model.borrow_mut().media_new_window = false;
                app.open_file(v1.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        // 开开关、复用关 → 开视频2：+1 窗口且 A 当前内容不变。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.model.borrow_mut().media_new_window = true;
                app.model.borrow_mut().reuse_media_window = false;
                app.open_file(v2.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_eq!(registry_window_count(cx), 2, "媒体窗口 + 复用关 → 应新开 1 个窗口");
        assert_eq!(
            media_win_snap(cx, &app).1.as_deref(),
            Some(v1.as_path()),
            "当前媒体窗口播放内容应不变（仍视频1）"
        );
        assert!(active_is_video(cx, &app));
        let others = other_app_entity(cx, &app);
        assert_eq!(others.len(), 1);
        assert_eq!(
            media_win_snap(cx, &others[0]).1.as_deref(),
            Some(v2.as_path()),
            "新窗口应打开视频2"
        );
        let _ = std::fs::remove_file(&v1);
        let _ = std::fs::remove_file(&v2);
    }

    /// 音频：开+复用+无媒体窗口 → 新开窗口且**完整进入音乐模式**（整窗 MusicView）。
    #[gpui::test]
    async fn media_window_audio_redirects_to_new_music_mode_window(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let tabs_before = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
        let s = placeholder_audio("song");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(s.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_eq!(registry_window_count(cx), 2, "音频默认开+复用、无媒体窗口 → 新开窗口");
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            tabs_before,
            "源窗口不变（音频不进当前窗口）"
        );
        let others = other_app_entity(cx, &app);
        assert_eq!(others.len(), 1);
        let (music_mode, _, _) = media_win_snap(cx, &others[0]);
        assert!(music_mode, "新窗口应完整进入音乐模式");
        assert!(
            cx.update(|_w, cx| others[0].read(cx).music.is_some()),
            "新窗口应创建 MusicView"
        );
        let _ = std::fs::remove_file(&s);
    }

    /// 菜单翻转：media_new_window 关时 reuse 翻转被拒绝（置灰双保险）；开时正常翻转。
    #[gpui::test]
    async fn media_window_media_switch_toggles_update_fields(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let read_flags = |app: &gpui::Entity<EditorApp>, cx: &mut TestAppContext| {
            cx.update(|cx| {
                let a = app.read(cx);
                (a.model.borrow().media_new_window, a.model.borrow().reuse_media_window)
            })
        };
        assert_eq!(
            read_flags(&app, cx),
            (true, true),
            "两开关默认开启"
        );
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_media_new_window(window, cx));
        });
        assert_eq!(
            read_flags(&app, cx),
            (false, true),
            "media_new_window 关，reuse 不受影响"
        );
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_reuse_media_window(window, cx));
        });
        assert_eq!(
            read_flags(&app, cx),
            (false, true),
            "media_new_window 关时 reuse 翻转应被拒绝（置灰无意义）"
        );
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_media_new_window(window, cx));
        });
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_reuse_media_window(window, cx));
        });
        assert_eq!(
            read_flags(&app, cx),
            (true, false),
            "开时 reuse 翻转应生效"
        );
    }

    /// 文本文件不受媒体重定向影响（PDF/图片/文本/EPUB 均不经过 is_media 分流）。
    #[gpui::test]
    async fn media_window_text_open_not_redirected(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let t = std::env::temp_dir().join(format!("gpui-mediawnd-t-{}.txt", std::process::id()));
        std::fs::write(&t, "纯文本").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(t.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_eq!(registry_window_count(cx), 1, "文本不应触发媒体窗口重定向");
        assert_eq!(
            media_win_snap(cx, &app).1.as_deref(),
            Some(t.as_path()),
            "文本应在当前窗口打开为活动选项卡"
        );
        let _ = std::fs::remove_file(&t);
    }

    // ---- 媒体窗口尺寸记录 + 自定义（delta：media_window.rs）----

    /// 只读「非给定窗口」的注册表条目（实体 + 窗口句柄，读新窗口 bounds 用）。
    fn other_window_handles(
        cx: &mut TestAppContext,
        exclude: &gpui::Entity<EditorApp>,
    ) -> Vec<(gpui::Entity<EditorApp>, AnyWindowHandle)> {
        cx.update(|cx| {
            let reg = cx.global::<EditorAppRegistry>();
            reg.windows
                .iter()
                .filter_map(|(weak, wh)| {
                    let e = weak.upgrade()?;
                    (e.entity_id() != exclude.entity_id()).then_some((e, *wh))
                })
                .collect()
        })
    }

    /// 读某窗口句柄的当前 bounds.size（测试平台 `open_window` 按 `WindowBounds` 落位）。
    fn window_size(
        cx: &mut TestAppContext,
        wh: AnyWindowHandle,
    ) -> Option<gpui::Size<Pixels>> {
        cx.update(|cx| wh.update(cx, |_, w, _| w.bounds().size).ok())
    }

    /// 尺寸记录未设 → 新开媒体窗口**跟随当前窗口大小**（`window.bounds()`），解决
    /// 「新窗口比原窗口大不一致」；窗口数 +1（无媒体窗口）。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn media_window_new_window_follows_source_size_when_unrecorded(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let src_size = cx.update(|w, _| w.bounds().size);
        assert_eq!(src_size, gpui::size(px(980.), px(720.)));
        let v1 = placeholder_video("sz1");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(v1.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_eq!(registry_window_count(cx), 2, "未记录 → 新开 1 个窗口");
        let others = other_window_handles(cx, &app);
        assert_eq!(others.len(), 1);
        assert_eq!(
            window_size(cx, others[0].1),
            Some(src_size),
            "未记录尺寸时新窗口应跟随发起窗口大小"
        );
        let _ = std::fs::remove_file(&v1);
    }

    /// 记录了 video 尺寸 → 开视频新窗口按该尺寸；**音乐记录不影响视频**（两字段独立）。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn media_window_video_recorded_size_applies_and_ignores_music(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(cx, |app, _| {
                // 故意给一组不同的音乐尺寸：视频新窗应只跟 video 记录（互不影响）。
                app.model.borrow_mut().music_window_size = Some(gpui::size(px(300.), px(200.)));
                app.model.borrow_mut().video_window_size = Some(gpui::size(px(900.), px(540.)));
            });
        });
        let v1 = placeholder_video("szv");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(v1.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let others = other_window_handles(cx, &app);
        assert_eq!(others.len(), 1);
        assert_eq!(
            window_size(cx, others[0].1),
            Some(gpui::size(px(900.), px(540.))),
            "视频新窗口应按 video 记录尺寸（忽略音乐记录）"
        );
        let _ = std::fs::remove_file(&v1);
    }

    /// 记录了 music 尺寸 → 开音频新窗口按该尺寸（音乐/视频分开记录生效）。
    #[gpui::test]
    async fn media_window_music_recorded_size_applies(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app.update(cx, |app, _| {
                app.model.borrow_mut().music_window_size = Some(gpui::size(px(800.), px(600.)));
            });
        });
        let s = placeholder_audio("szm");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(s.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let others = other_window_handles(cx, &app);
        assert_eq!(others.len(), 1);
        assert_eq!(
            window_size(cx, others[0].1),
            Some(gpui::size(px(800.), px(600.))),
            "音频新窗口应按 music 记录尺寸"
        );
        let _ = std::fs::remove_file(&s);
    }

    /// `record_media_window_size` 按角色分写：kind=Music → 只动 music 字段；kind=Video →
    /// 只动 video 字段；kind=None（非媒体窗口）→ 两字段都不动。同尺寸重复记录不覆盖。
    /// 注意：kind=Music 必须经 open_music_path 造出真实 MusicView——直接置 music_mode=true
    /// 而 music 为空时，下一帧渲染会走 layout.rs 兜底把 music_mode 重置回 false。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn media_window_record_size_updates_by_kind(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // kind = None（默认空选项卡、非音乐模式）→ 不记录。
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                app.record_media_window_size(gpui::size(px(640.), px(480.)), cx);
            });
        });
        let (m0, v0) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().music_window_size, a.model.borrow().video_window_size)
        });
        assert_eq!(m0, None, "非媒体窗口不应记录音乐尺寸");
        assert_eq!(v0, None, "非媒体窗口不应记录视频尺寸");
        // kind = Music：开音频（当前窗口音乐模式，创建真实 MusicView）→ 只更新 music 字段。
        let s = placeholder_audio("kindm");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.model.borrow_mut().media_new_window = false;
                app.open_file(s.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert!(
            cx.update(|_w, cx| {
                let a = app.read(cx);
                a.model.borrow().music_mode && a.music.is_some()
            }),
            "前置：应处于真实音乐模式（含 MusicView）"
        );
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                app.record_media_window_size(gpui::size(px(800.), px(600.)), cx);
            });
        });
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                app.record_media_window_size(gpui::size(px(700.), px(500.)), cx);
            });
        });
        // 同尺寸重复记录：字段保持不变（去重，不重复标记/写盘）。
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                app.record_media_window_size(gpui::size(px(700.), px(500.)), cx);
            });
        });
        let (m1, v1) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().music_window_size, a.model.borrow().video_window_size)
        });
        assert_eq!(m1, Some(gpui::size(px(700.), px(500.))), "音乐记录跟随最新 resize");
        assert_eq!(v1, None, "音乐模式下不应记录视频尺寸");
        // kind = Video：退出音乐模式 + 活动选项卡为视频 → 只更新 video 字段。
        cx.update(|_w, cx| app.update(cx, |app, _| app.model.borrow_mut().music_mode = false));
        let vp = placeholder_video("kindv");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(vp.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert!(active_is_video(cx, &app), "前置：活动选项卡应为视频");
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                app.record_media_window_size(gpui::size(px(900.), px(540.)), cx);
            });
        });
        let (m2, v2) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().music_window_size, a.model.borrow().video_window_size)
        });
        assert_eq!(m2, Some(gpui::size(px(700.), px(500.))), "视频记录不影响音乐字段");
        assert_eq!(v2, Some(gpui::size(px(900.), px(540.))), "视频记录应就位");
        let _ = std::fs::remove_file(&s);
        let _ = std::fs::remove_file(&vp);
    }

    /// 专用媒体窗口（`new_media_window` 构造）的 `observe_window_bounds` → resize 自动
    /// 记录（端到端，测试平台 simulate_resize 走真实 resize 回调 → bounds 观察者路径）。
    /// 用视频角色避免 MusicView 依赖；最大化/全屏跳过分支无法 headless 模拟，标 [待实证]。
    #[cfg(feature = "video")]
    #[gpui::test]
    async fn media_window_resize_records_on_new_media_window(cx: &mut TestAppContext) {
        init_test(cx);
        // 以 new_media_window 姿势建专用媒体窗口（startup=false → 注册 bounds 观察者）。
        let holder: std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<EditorApp>>>> =
            std::rc::Rc::default();
        let (_root, cx) = cx.add_window_view(|window, cx| {
            let app = cx.new(|cx| EditorApp::new_media_window(window, cx));
            *holder.borrow_mut() = Some(app.clone());
            ui::root::Root::new(app, window, cx)
        });
        cx.simulate_resize(gpui::size(px(500.), px(400.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let app = holder.borrow().clone().unwrap();
        // 初始（kind=None）resize → 不记录。
        let (m0, v0) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().music_window_size, a.model.borrow().video_window_size)
        });
        assert_eq!(m0, None, "kind=None 的 resize 不应记录");
        assert_eq!(v0, None);
        // 置为视频角色后 resize → 观察者应自动记录视频尺寸。
        let vp = placeholder_video("obsv");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.model.borrow_mut().media_new_window = false;
                app.open_file(vp.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert!(active_is_video(cx, &app), "前置：活动选项卡应为视频");
        cx.simulate_resize(gpui::size(px(700.), px(520.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (m1, v1) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.model.borrow().music_window_size, a.model.borrow().video_window_size)
        });
        assert_eq!(m1, None, "视频 resize 不应写音乐字段");
        assert_eq!(
            v1,
            Some(gpui::size(px(700.), px(520.))),
            "专用媒体窗口 resize 应自动记录视频尺寸（真实 bounds 观察者路径）"
        );
        let _ = std::fs::remove_file(&vp);
    }
