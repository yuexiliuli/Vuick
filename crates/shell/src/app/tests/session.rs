    /// 会话测试用临时目录（每次用唯一 tag + 进程号，先清后建；
    /// **禁止**触碰真实 %APPDATA% 的 config/session 目录）。
    fn session_temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gpui-session-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 清单 roundtrip：file + new 混合、顺序与 active_tab 保存一致（含空格/中文路径）。
    #[test]
    fn manifest_roundtrip() {
        use session::{SessionManifest, SessionTab};
        let dir = session_temp_dir("manifest_roundtrip");
        let m = SessionManifest {
            version: 1,
            active_tab: 1,
            tabs: vec![
                SessionTab::File { path: std::path::PathBuf::from(r"C:\x\a.txt") },
                SessionTab::New { slot: "t0".into(), name: "未命名".into() },
                SessionTab::File {
                    path: std::path::PathBuf::from(r"D:\空 格\中文 文件.md"),
                },
            ],
        };
        assert!(session::write_manifest(&dir, &m), "清单应原子写成功");
        let back = session::read_manifest(&dir).expect("清单应可读回");
        assert_eq!(back, m, "roundtrip 后清单应完全一致");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 清单损坏 / 缺失 / 非对象 → 安全回退：缺失与非法 JSON 返回 None（不恢复），
    /// 合法对象但缺字段 → 最小可用清单（不 panic）。
    #[test]
    fn manifest_corrupt_or_missing_safe_fallback() {
        let dir = session_temp_dir("manifest_corrupt");
        assert!(session::read_manifest(&dir).is_none(), "缺失清单 → None");
        std::fs::write(dir.join("session.json"), "{ 不是 JSON !!!").unwrap();
        assert!(session::read_manifest(&dir).is_none(), "损坏 JSON → None");
        std::fs::write(dir.join("session.json"), "[1,2,3]").unwrap();
        assert!(session::read_manifest(&dir).is_none(), "非对象 JSON → None");
        // 空对象（合法但缺全部字段）→ 回退最小清单，不 panic。
        std::fs::write(dir.join("session.json"), "{}").unwrap();
        let m = session::read_manifest(&dir).expect("合法空对象应回退最小清单");
        assert_eq!(m.active_tab, 0);
        assert!(m.tabs.is_empty());
        assert_eq!(m.version, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 槽位内容 roundtrip：非空 + 空内容都写文件（空内容也写）且可读回；缺失 → None。
    #[test]
    fn slot_roundtrip_including_empty() {
        let dir = session_temp_dir("slot_roundtrip");
        assert!(session::write_slot(&dir, "t0", ""));
        assert!(session::write_slot(&dir, "t1", "第一行\n第二行\ncrlf\r\n可归一"));
        // CRLF 写入后应被 LF 规范化（返回是读盘原样，断言写前已归一：直接读文件）。
        let raw = std::fs::read_to_string(dir.join("t1")).unwrap();
        assert!(!raw.contains("\r"), "槽位文件应 LF 规范化（未含 CR）");
        assert_eq!(session::read_slot(&dir, "t0").as_deref(), Some(""), "空内容也应写文件并可读回");
        assert_eq!(session::read_slot(&dir, "t1").as_deref(), Some("第一行\n第二行\ncrlf\n可归一"));
        assert!(dir.join("t0").is_file(), "空内容也写槽位文件");
        assert_eq!(session::read_slot(&dir, "t999"), None, "缺失槽位 → None");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端到端：带内容的未命名选项卡（含空内容）保存 → 新实例恢复，数量/顺序/内容/
    /// 活动选项卡一致，且恢复后清单与槽位被清理。
    #[gpui::test]
    async fn session_save_restore_unnamed_tabs(cx: &mut TestAppContext) {
        init_test(cx);
        let dir = session_temp_dir("unnamed_roundtrip");
        let (app_a, cx) = add_app_zero_tabs(cx);
        cx.update(|window, cx| {
            app_a.update(cx, |app, cx| {
                for (i, txt) in ["", "内容甲", "内容乙"].iter().enumerate() {
                    let idx = app.make_tab(window, cx);
                    app.tabs[idx].editor.update(cx, |ed, _| ed.set_text(txt));
                    if i == 2 {
                        app.activate_tab(idx, window, cx);
                    }
                }
                assert_eq!(app.active_tab, 2, "激活最后建的未命名选项卡");
            });
        });
        cx.update(|_w, cx| {
            app_a.update(cx, |app, cx| {
                assert!(session::save_session_to(app, &dir, cx), "保存会话应成功");
            });
        });
        // 落盘内容核对：3 个 new 槽位 t0..t2，空内容也在。
        let m = session::read_manifest(&dir).expect("保存后清单应存在");
        assert_eq!(m.tabs.len(), 3);
        assert_eq!(m.active_tab, 2);
        assert!(matches!(m.tabs[0], session::SessionTab::New { ref slot, .. } if slot == "t0"));
        assert_eq!(session::read_slot(&dir, "t0").as_deref(), Some(""));
        assert_eq!(session::read_slot(&dir, "t1").as_deref(), Some("内容甲"));
        assert_eq!(session::read_slot(&dir, "t2").as_deref(), Some("内容乙"));

        // 全新窗口实例恢复。
        let (app_b, cx) = add_app_zero_tabs(cx);
        cx.update(|window, cx| {
            app_b.update(cx, |app, cx| {
                assert!(session::restore_session_from(app, &dir, window, cx));
            });
        });
        let (tabs, act, contents) = cx.update(|_w, cx| {
            let a = app_b.read(cx);
            let contents: Vec<String> =
                a.tabs.iter().map(|t| t.editor.read(cx).value().to_string()).collect();
            (a.tabs.len(), a.active_tab, contents)
        });
        assert_eq!(tabs, 3, "恢复后选项卡数量应一致");
        assert_eq!(act, 2, "恢复后的活动选项卡应一致");
        assert_eq!(contents, vec!["", "内容甲", "内容乙"], "未命名内容（含空）应原样恢复");
        // 恢复完成后清理：清单与槽位删除。
        assert!(!dir.join("session.json").exists(), "恢复后清单应删除");
        assert!(!dir.join("t0").exists(), "恢复后槽位应清理");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 混合场景：既有文件（含空格/中文路径）+ 未命名混合，顺序按清单重建，活动选项卡恢复。
    #[gpui::test]
    async fn session_mixes_file_and_unnamed(cx: &mut TestAppContext) {
        init_test(cx);
        let dir = session_temp_dir("mixed");
        let f1 = std::env::temp_dir().join(format!(
            "gpui-session 空格-中文标题-{}.md",
            std::process::id()
        ));
        let f2 = std::env::temp_dir().join(format!("gpui-session-plain-{}.txt", std::process::id()));
        std::fs::write(&f1, "# 标题\n正文内容").unwrap();
        std::fs::write(&f2, "纯文本").unwrap();

        let (app_a, cx) = add_app_zero_tabs(cx);
        cx.update(|window, cx| {
            app_a.update(cx, |app, cx| {
                app.open_file(f1.to_str().unwrap(), window, cx); // idx 0 = file
                let idx1 = app.make_tab(window, cx); // idx 1 = 未命名
                app.tabs[idx1].editor.update(cx, |ed, _| ed.set_text("未命名内容"));
                app.open_file(f2.to_str().unwrap(), window, cx); // idx 2 = file
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app_a.update(cx, |app, cx| {
                assert_eq!(app.active_tab, 2, "激活最后打开的 f2");
                assert!(session::save_session_to(app, &dir, cx));
            });
        });
        let m = session::read_manifest(&dir).expect("清单应存在");
        assert!(matches!(m.tabs[0], session::SessionTab::File { .. }));
        assert!(matches!(m.tabs[1], session::SessionTab::New { .. }));
        assert!(matches!(m.tabs[2], session::SessionTab::File { .. }));
        assert_eq!(m.active_tab, 2);

        let (app_b, cx) = add_app_zero_tabs(cx);
        cx.update(|window, cx| {
            app_b.update(cx, |app, cx| {
                assert!(session::restore_session_from(app, &dir, window, cx));
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (tabs, act, paths, contents) = cx.update(|_w, cx| {
            let a = app_b.read(cx);
            let paths: Vec<Option<std::path::PathBuf>> =
                a.tabs.iter().map(|t| t.file_path.clone()).collect();
            let contents: Vec<String> =
                a.tabs.iter().map(|t| t.editor.read(cx).value().to_string()).collect();
            (a.tabs.len(), a.active_tab, paths, contents)
        });
        assert_eq!(tabs, 3, "恢复后 3 个选项卡");
        assert_eq!(act, 2);
        assert_eq!(paths[0].as_deref(), Some(f1.as_path()), "顺序 1 = f1（含中文/空格路径）");
        assert_eq!(paths[1], None, "顺序 2 = 未命名");
        assert_eq!(paths[2].as_deref(), Some(f2.as_path()), "顺序 3 = f2");
        assert!(contents[0].contains("正文内容"), "f1 内容应恢复");
        assert_eq!(contents[1], "未命名内容", "未命名内容应恢复");
        assert_eq!(contents[2], "纯文本", "f2 内容应恢复");
        let _ = std::fs::remove_file(&f1);
        let _ = std::fs::remove_file(&f2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// active_tab 越界收敛到合法值：清单 active_tab=5、仅 1 个选项卡 → 恢复后 clamp 到 0。
    #[gpui::test]
    async fn restore_clamps_out_of_range_active_tab(cx: &mut TestAppContext) {
        init_test(cx);
        let dir = session_temp_dir("active_clamp");
        use session::{SessionManifest, SessionTab};
        let m = SessionManifest {
            version: 1,
            active_tab: 5,
            tabs: vec![SessionTab::New { slot: "t0".into(), name: "未命名".into() }],
        };
        assert!(session::write_manifest(&dir, &m));
        assert!(session::write_slot(&dir, "t0", "x"));
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                assert!(session::restore_session_from(app, &dir, window, cx));
            });
        });
        let (tabs, act) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.tabs.len(), a.active_tab)
        });
        assert_eq!(tabs, 1);
        assert_eq!(act, 0, "越界 active_tab 应 clamp 到合法范围 0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 开关关闭：不写清单/槽位（顺带清遗留）、恢复直接返回 false（不恢复）。
    #[gpui::test]
    async fn switch_off_skips_save_and_restore(cx: &mut TestAppContext) {
        init_test(cx);
        let dir = session_temp_dir("switch_off");
        // 先有一个遗留清单/槽位（模拟上次开启时留下的陈旧会话）。
        use session::{SessionManifest, SessionTab};
        let m = SessionManifest {
            version: 1,
            active_tab: 0,
            tabs: vec![SessionTab::New { slot: "t0".into(), name: "未命名".into() }],
        };
        assert!(session::write_manifest(&dir, &m));
        assert!(session::write_slot(&dir, "t0", "陈旧"));

        let (app, cx) = add_app(cx);
        // 开关关 + 保存 → 只清不写（遗留清单/槽位被清理）。
        cx.update(|_w, cx| {
            app.update(cx, |app, cx| {
                app.session_restore = false;
                assert!(session::save_session_to(app, &dir, cx));
            });
        });
        assert!(!dir.join("session.json").exists(), "开关关不应写新清单");
        assert!(!dir.join("t0").exists(), "开关关应清理遗留槽位");
        // 开关关 + 恢复 → false，不动作。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.session_restore = false;
                assert!(!session::restore_session_from(app, &dir, window, cx));
            });
        });
        // 开关开但没有清单 → 恢复 false。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.session_restore = true;
                assert!(
                    !session::restore_session_from(app, &dir, window, cx),
                    "无清单时不恢复"
                );
            });
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 既有文件不存在（被删除）→ 恢复时跳过该项，其余正常恢复。
    #[gpui::test]
    async fn restore_skips_deleted_file(cx: &mut TestAppContext) {
        init_test(cx);
        let dir = session_temp_dir("deleted_file");
        let missing = std::env::temp_dir().join(format!(
            "gpui-session-gone-{}.txt",
            std::process::id()
        ));
        let present = std::env::temp_dir().join(format!(
            "gpui-session-here-{}.txt",
            std::process::id()
        ));
        std::fs::write(&present, "在").unwrap();
        use session::{SessionManifest, SessionTab};
        let m = SessionManifest {
            version: 1,
            active_tab: 1,
            tabs: vec![
                SessionTab::File { path: missing.clone() },
                SessionTab::File { path: present.clone() },
            ],
        };
        assert!(session::write_manifest(&dir, &m));
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                assert!(session::restore_session_from(app, &dir, window, cx));
            });
        });
        let (tabs, act, paths) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let paths: Vec<Option<std::path::PathBuf>> =
                a.tabs.iter().map(|t| t.file_path.clone()).collect();
            (a.tabs.len(), a.active_tab, paths)
        });
        assert_eq!(tabs, 1, "被删除文件应跳过，只恢复存在文件");
        assert_eq!(act, 0, "active_tab 越界（只 1 项）应 clamp 到 0");
        assert_eq!(paths[0].as_deref(), Some(present.as_path()));
        let _ = std::fs::remove_file(&present);
        let _ = std::fs::remove_file(&missing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 孤儿槽位清理（纯函数）：非 `session.json`、非被引用槽位、非 `.tmp` 后缀 →
    /// 删除；目录缺失 → 安全 true（不 panic、无操作）。
    #[test]
    fn prune_unreferenced_slots_deletes_orphans_keeps_refs_and_tmp() {
        use session::{SessionManifest, SessionTab};
        let dir = session_temp_dir("prune_cleanup");
        let m = SessionManifest {
            version: 1,
            active_tab: 0,
            tabs: vec![
                SessionTab::New { slot: "t0".into(), name: "未命名".into() },
                SessionTab::File { path: std::path::PathBuf::from(r"C:\a.txt") },
            ],
        };
        std::fs::write(dir.join("t0"), "引用").unwrap();
        std::fs::write(dir.join("t1"), "孤儿").unwrap();
        std::fs::write(dir.join("t2"), "孤儿").unwrap();
        std::fs::write(dir.join("t0.tmp"), "写入中").unwrap();
        std::fs::write(dir.join("session.json"), "{}").unwrap();
        assert!(session::prune_unreferenced_slots(&dir, &m), "清理应成功");
        assert!(dir.join("t0").exists(), "被引用槽位保留");
        assert!(dir.join("t0.tmp").exists(), ".tmp 中间文件保留");
        assert!(dir.join("session.json").exists(), "清单保留");
        assert!(!dir.join("t1").exists(), "孤儿 t1 删除");
        assert!(!dir.join("t2").exists(), "孤儿 t2 删除");
        assert!(
            session::prune_unreferenced_slots(&dir.join("不存在的目录"), &m),
            "目录缺失应安全返回"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C 端到端：两未命名 t0/t1 → 关掉 idx=1 → 再保存 → 盘上只剩被引用槽位
    /// （对应正确内容）+ session.json，无 t1 残留（「否→删除临时文件」落盘语义）。
    #[gpui::test]
    async fn save_prunes_orphan_slot_after_tab_close(cx: &mut TestAppContext) {
        init_test(cx);
        let dir = session_temp_dir("prune_close_slot");
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let i0 = app.make_tab(window, cx);
                app.tabs[i0].editor.update(cx, |ed, _| ed.set_text("甲"));
                let i1 = app.make_tab(window, cx);
                app.tabs[i1].editor.update(cx, |ed, _| ed.set_text("乙"));
                assert!(session::save_session_to(app, &dir, cx), "首次保存应成功");
                assert!(session::read_slot(&dir, "t0").is_some(), "首轮 t0 在盘");
                assert!(session::read_slot(&dir, "t1").is_some(), "首轮 t1 在盘");
                // 关掉 idx=1（乙）：保留 idx=0（甲）；remove_tab 自带即时持久化（测试构建
                // 短路），此处显式再存，验证裁剪。
                app.remove_tab(i1, window, cx);
                assert!(session::save_session_to(app, &dir, cx), "关后重存应成功");
            });
        });
        assert!(dir.join("session.json").exists(), "清单应在");
        assert_eq!(session::read_slot(&dir, "t0").as_deref(), Some("甲"), "保留槽位内容正确");
        assert!(!dir.join("t1").exists(), "被关选项卡的旧槽位 t1 应清理");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C 端到端（空 tab 场景）：三未命名（含空）→ 关 idx=1 → 重存 → 盘上仅 t0/t1
    /// （对应剩余两槽内容）+ session.json，无 t2 残留。
    #[gpui::test]
    async fn save_prunes_orphan_slot_with_empty_tab(cx: &mut TestAppContext) {
        init_test(cx);
        let dir = session_temp_dir("prune_close_empty");
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let _ = app.make_tab(window, cx); // 空（仅需存在，无需用下标）
                let i1 = app.make_tab(window, cx);
                app.tabs[i1].editor.update(cx, |ed, _| ed.set_text("乙"));
                let i2 = app.make_tab(window, cx);
                app.tabs[i2].editor.update(cx, |ed, _| ed.set_text("丙"));
                assert!(session::save_session_to(app, &dir, cx));
                assert_eq!(session::read_slot(&dir, "t0").as_deref(), Some(""), "空内容也在盘");
                assert_eq!(session::read_slot(&dir, "t2").as_deref(), Some("丙"), "t2 首轮在盘");
                app.remove_tab(i1, window, cx); // 关「乙」
                assert!(session::save_session_to(app, &dir, cx));
            });
        });
        let m = session::read_manifest(&dir).expect("清单应在");
        assert_eq!(m.tabs.len(), 2, "重存后 2 个未命名槽位");
        assert_eq!(session::read_slot(&dir, "t0").as_deref(), Some(""), "t0=空内容正确");
        assert_eq!(session::read_slot(&dir, "t1").as_deref(), Some("丙"), "t1=剩余内容正确");
        assert!(!dir.join("t2").exists(), "孤儿 t2 应清理");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B 判定：未命名选项卡（空 / 脏两种）关闭**一律弹确认**（不直通 remove_tab）；
    /// 选「不保存」→ 选项卡被移除（配合 C 其槽位随后清理）。
    #[gpui::test]
    async fn close_unnamed_tab_always_prompts(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        for make_dirty in [false, true] {
            cx.update(|window, cx| {
                app.update(cx, |app, cx| {
                    let i = app.make_tab(window, cx);
                    if make_dirty {
                        app.tabs[i].editor.update(cx, |ed, _| ed.set_text("内容"));
                    }
                    app.close_tab(i, window, cx);
                });
            });
            assert!(
                cx.has_pending_prompt(),
                "未命名{}关闭应弹确认（不直通 remove_tab）",
                if make_dirty { "（脏）" } else { "（空）" }
            );
            let (msg, _) = cx.pending_prompt().expect("应有在途确认框");
            assert_eq!(msg, "未保存的更改", "确认框标题一致");
            cx.simulate_prompt_answer("不保存");
            settle_frames(cx, 5);
            let tabs = cx.update(|_w, cx| app.read(cx).tabs.len());
            assert_eq!(tabs, 0, "「不保存」应移除未命名选项卡");
        }
    }

    /// B 判定：未命名关闭选「取消」→ 留在应用（不删除、不动作）。
    #[gpui::test]
    async fn close_unnamed_tab_cancel_keeps_tab(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let i = app.make_tab(window, cx);
                app.close_tab(i, window, cx);
            });
        });
        assert!(cx.has_pending_prompt(), "空未命名关闭应弹确认");
        cx.simulate_prompt_answer("Cancel");
        settle_frames(cx, 5);
        let tabs = cx.update(|_w, cx| app.read(cx).tabs.len());
        assert_eq!(tabs, 1, "取消应留在应用");
        assert!(!cx.has_pending_prompt(), "确认框已应答");
    }

    /// B 判定：命名文件选项卡干净（!dirty）→ 直通 remove_tab（不弹确认）。
    #[gpui::test]
    async fn close_clean_named_tab_directly_closes(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app_zero_tabs(cx);
        let f1 = std::env::temp_dir().join(format!("gpui-close-clean-{}.txt", std::process::id()));
        std::fs::write(&f1, "干净").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(f1.to_str().unwrap(), window, cx);
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let idx = cx.update(|_w, cx| app.read(cx).tabs.len() - 1);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.close_tab(idx, window, cx));
        });
        settle_frames(cx, 5);
        assert!(!cx.has_pending_prompt(), "命名干净 tab 不应弹确认");
        let tabs = cx.update(|_w, cx| app.read(cx).tabs.len());
        assert_eq!(tabs, 0, "命名干净 tab 应直通 remove_tab");
        let _ = std::fs::remove_file(&f1);
    }
