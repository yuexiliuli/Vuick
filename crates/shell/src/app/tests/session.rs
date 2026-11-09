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
        use session_store::{SessionManifest, SessionTab};
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
        assert!(session_store::write_manifest(&dir, &m), "清单应原子写成功");
        let back = session_store::read_manifest(&dir).expect("清单应可读回");
        assert_eq!(back, m, "roundtrip 后清单应完全一致");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 清单损坏 / 缺失 / 非对象 → 安全回退：缺失与非法 JSON 返回 None（不恢复），
    /// 合法对象但缺字段 → 最小可用清单（不 panic）。
    #[test]
    fn manifest_corrupt_or_missing_safe_fallback() {
        let dir = session_temp_dir("manifest_corrupt");
        assert!(session_store::read_manifest(&dir).is_none(), "缺失清单 → None");
        std::fs::write(dir.join("session.json"), "{ 不是 JSON !!!").unwrap();
        assert!(session_store::read_manifest(&dir).is_none(), "损坏 JSON → None");
        std::fs::write(dir.join("session.json"), "[1,2,3]").unwrap();
        assert!(session_store::read_manifest(&dir).is_none(), "非对象 JSON → None");
        // 空对象（合法但缺全部字段）→ 回退最小清单，不 panic。
        std::fs::write(dir.join("session.json"), "{}").unwrap();
        let m = session_store::read_manifest(&dir).expect("合法空对象应回退最小清单");
        assert_eq!(m.active_tab, 0);
        assert!(m.tabs.is_empty());
        assert_eq!(m.version, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 槽位内容 roundtrip：非空 + 空内容都写文件（空内容也写）且可读回；缺失 → None。
    #[test]
    fn slot_roundtrip_including_empty() {
        let dir = session_temp_dir("slot_roundtrip");
        assert!(session_store::write_slot(&dir, "t0", ""));
        assert!(session_store::write_slot(&dir, "t1", "第一行\n第二行\ncrlf\r\n可归一"));
        // CRLF 写入后应被 LF 规范化（返回是读盘原样，断言写前已归一：直接读文件）。
        let raw = std::fs::read_to_string(dir.join("t1")).unwrap();
        assert!(!raw.contains("\r"), "槽位文件应 LF 规范化（未含 CR）");
        assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some(""), "空内容也应写文件并可读回");
        assert_eq!(session_store::read_slot(&dir, "t1").as_deref(), Some("第一行\n第二行\ncrlf\n可归一"));
        assert!(dir.join("t0").is_file(), "空内容也写槽位文件");
        assert_eq!(session_store::read_slot(&dir, "t999"), None, "缺失槽位 → None");
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
        let m = session_store::read_manifest(&dir).expect("保存后清单应存在");
        assert_eq!(m.tabs.len(), 3);
        assert_eq!(m.active_tab, 2);
        assert!(matches!(m.tabs[0], session_store::SessionTab::New { ref slot, .. } if slot == "t0"));
        assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some(""));
        assert_eq!(session_store::read_slot(&dir, "t1").as_deref(), Some("内容甲"));
        assert_eq!(session_store::read_slot(&dir, "t2").as_deref(), Some("内容乙"));

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
        let m = session_store::read_manifest(&dir).expect("清单应存在");
        assert!(matches!(m.tabs[0], session_store::SessionTab::File { .. }));
        assert!(matches!(m.tabs[1], session_store::SessionTab::New { .. }));
        assert!(matches!(m.tabs[2], session_store::SessionTab::File { .. }));
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
        use session_store::{SessionManifest, SessionTab};
        let m = SessionManifest {
            version: 1,
            active_tab: 5,
            tabs: vec![SessionTab::New { slot: "t0".into(), name: "未命名".into() }],
        };
        assert!(session_store::write_manifest(&dir, &m));
        assert!(session_store::write_slot(&dir, "t0", "x"));
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
        use session_store::{SessionManifest, SessionTab};
        let m = SessionManifest {
            version: 1,
            active_tab: 0,
            tabs: vec![SessionTab::New { slot: "t0".into(), name: "未命名".into() }],
        };
        assert!(session_store::write_manifest(&dir, &m));
        assert!(session_store::write_slot(&dir, "t0", "陈旧"));

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
        use session_store::{SessionManifest, SessionTab};
        let m = SessionManifest {
            version: 1,
            active_tab: 1,
            tabs: vec![
                SessionTab::File { path: missing.clone() },
                SessionTab::File { path: present.clone() },
            ],
        };
        assert!(session_store::write_manifest(&dir, &m));
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
        use session_store::{SessionManifest, SessionTab};
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
        assert!(session_store::prune_unreferenced_slots(&dir, &m), "清理应成功");
        assert!(dir.join("t0").exists(), "被引用槽位保留");
        assert!(dir.join("t0.tmp").exists(), ".tmp 中间文件保留");
        assert!(dir.join("session.json").exists(), "清单保留");
        assert!(!dir.join("t1").exists(), "孤儿 t1 删除");
        assert!(!dir.join("t2").exists(), "孤儿 t2 删除");
        assert!(
            session_store::prune_unreferenced_slots(&dir.join("不存在的目录"), &m),
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
                assert!(session_store::read_slot(&dir, "t0").is_some(), "首轮 t0 在盘");
                assert!(session_store::read_slot(&dir, "t1").is_some(), "首轮 t1 在盘");
                // 关掉 idx=1（乙）：保留 idx=0（甲）；remove_tab 自带即时持久化（测试构建
                // 短路），此处显式再存，验证裁剪。
                app.remove_tab(i1, window, cx);
                assert!(session::save_session_to(app, &dir, cx), "关后重存应成功");
            });
        });
        assert!(dir.join("session.json").exists(), "清单应在");
        assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some("甲"), "保留槽位内容正确");
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
                assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some(""), "空内容也在盘");
                assert_eq!(session_store::read_slot(&dir, "t2").as_deref(), Some("丙"), "t2 首轮在盘");
                app.remove_tab(i1, window, cx); // 关「乙」
                assert!(session::save_session_to(app, &dir, cx));
            });
        });
        let m = session_store::read_manifest(&dir).expect("清单应在");
        assert_eq!(m.tabs.len(), 2, "重存后 2 个未命名槽位");
        assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some(""), "t0=空内容正确");
        assert_eq!(session_store::read_slot(&dir, "t1").as_deref(), Some("丙"), "t1=剩余内容正确");
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

    /// 哈希跳过（正确性）：只改 t0 → 重存后盘上 t0 更新、t1 原样；关选项卡 → 指纹表
    /// 只留 live 槽（不随建/关无限膨胀）。等价的「全量重写」行为不变（盘面正确），
    /// 附指纹表规模断言。
    #[gpui::test]
    async fn hash_skip_rewrites_changed_slot_and_bounds_map(cx: &mut TestAppContext) {
        init_test(cx);
        let dir = session_temp_dir("hash_skip");
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let i0 = app.make_tab(window, cx);
                app.tabs[i0].editor.update(cx, |ed, _| ed.set_text("甲"));
                let i1 = app.make_tab(window, cx);
                app.tabs[i1].editor.update(cx, |ed, _| ed.set_text("乙"));
                assert!(session::save_session_to(app, &dir, cx));
                assert_eq!(app.session_slot_hashes.len(), 2, "两未命名 → 两指纹");

                // 只改 t0 内容：重存后盘上 t0 更新、t1 不变（盘点正确性不受跳过影响）。
                app.tabs[i0].editor.update(cx, |ed, _| ed.set_text("甲改"));
                assert!(session::save_session_to(app, &dir, cx));
                assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some("甲改"), "变化槽位重写");
                assert_eq!(session_store::read_slot(&dir, "t1").as_deref(), Some("乙"), "未变槽位盘面原样");

                // 关掉 idx=1 → 重存：指纹淘汰 t1、只留 live 槽 t0（重编号后仍对应正确内容）。
                app.remove_tab(i1, window, cx);
                assert!(session::save_session_to(app, &dir, cx));
                assert!(
                    app.session_slot_hashes.len() == 1 && app.session_slot_hashes.contains_key("t0"),
                    "指纹只留 live 槽（内存不膨胀）"
                );
                assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some("甲改"));
            });
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归：开关关（清盘面+清指纹）→ 重开 → 重存，槽位必须重新落盘——旧指纹不得让
    /// 重开后的首次保存误判「内容未变」而跳过写槽位（否则恢复读到缺失槽位 → 空内容）。
    #[gpui::test]
    async fn switch_off_on_resave_rewrites_slots(cx: &mut TestAppContext) {
        init_test(cx);
        let dir = session_temp_dir("reoff_on_resave");
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let i = app.make_tab(window, cx);
                app.tabs[i].editor.update(cx, |ed, _| ed.set_text("内容"));

                // 开启态保存：槽位落盘 + 指纹记录。
                app.session_restore = true;
                assert!(session::save_session_to(app, &dir, cx));
                assert!(session_store::read_slot(&dir, "t0").is_some(), "开启态槽位在盘");

                // 关（只清不写）：盘面与指纹一并清空。
                app.session_restore = false;
                assert!(session::save_session_to(app, &dir, cx));
                assert!(!dir.join("t0").exists(), "关闭态应清盘面");
                assert!(app.session_slot_hashes.is_empty(), "关闭态应清指纹");

                // 重开 → 重存：必须重新写槽位（不被旧指纹误跳过）。
                app.session_restore = true;
                assert!(session::save_session_to(app, &dir, cx));
                assert_eq!(
                    session_store::read_slot(&dir, "t0").as_deref(),
                    Some("内容"),
                    "重开后槽位重新落盘"
                );
            });
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 性能量级探针：拆解底层文件原语成本（`#[ignore]`，仅人工驱动）。定位「跳过槽位写后
    /// 每轮仍 ~25ms」的常数项（本机疑为防病毒每文件创建扫描）：隔离 清单原子写 / 纯 fs::write
    /// / remove_file / rename / prune / 槽位写，各 300 次取均值（真实临时目录，不碰 %APPDATA%）。
    #[test]
    #[ignore]
    fn bench_session_parts() {
        use std::time::Instant;
        let dir = session_temp_dir("bench-parts");
        let body = serde_json::json!({ "version": 1, "active_tab": 0, "tabs": [] }).to_string();
        let target = dir.join("session.json");
        let tmp = dir.join("session.json.tmp");
        let n = 300usize;
        // 先预热（首文件触达含防病毒冷扫描，不测）。
        std::fs::write(dir.join("warm"), "x").unwrap();

        let t = Instant::now();
        for _ in 0..n {
            let _ = std::fs::write(&tmp, &body);
            let _ = std::fs::remove_file(&target);
            let _ = std::fs::rename(&tmp, &target);
        }
        eprintln!(
            "[bench-parts] fs::write+remove+rename(模拟原子写): {:.1}us/次",
            t.elapsed().as_secs_f64() * 1e6 / n as f64
        );
        let t = Instant::now();
        for _ in 0..n {
            let _ = std::fs::write(&target, &body);
        }
        eprintln!(
            "[bench-parts] fs::write 直接覆盖(1次文件打开): {:.1}us/次",
            t.elapsed().as_secs_f64() * 1e6 / n as f64
        );
        let t = Instant::now();
        for _ in 0..n {
            let _ = std::fs::rename(&tmp, &target);
        }
        eprintln!(
            "[bench-parts] fs::rename(已存在目标): {:.1}us/次",
            t.elapsed().as_secs_f64() * 1e6 / n as f64
        );
        let t = Instant::now();
        for _ in 0..n {
            assert!(session_store::write_slot(&dir, "t0", ""));
        }
        eprintln!(
            "[bench-parts] write_slot(空槽位原子写): {:.1}us/次",
            t.elapsed().as_secs_f64() * 1e6 / n as f64
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 清单无变化重入跳过（白盒探针）：首存写清单并记指纹；同状态再存 → 指纹相等 →
    /// 清单不再重写（盘面保持首次结果，零文件 IO）。真实变化（改内容后 active_tab 或
    /// 槽位改变）→ 指纹变化 → 清单重写自愈。
    #[gpui::test]
    async fn manifest_skip_noop_resave_and_selfheal(cx: &mut TestAppContext) {
        init_test(cx);
        let dir = session_temp_dir("manifest_skip");
        let (app, cx) = add_app_zero_tabs(cx);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let i = app.make_tab(window, cx);
                app.tabs[i].editor.update(cx, |ed, _| ed.set_text("内容"));
                assert!(session::save_session_to(app, &dir, cx));
                let body1 = std::fs::read_to_string(dir.join("session.json")).unwrap();
                let digest1 = app.session_last_manifest_digest;
                assert!(digest1.is_some(), "首存应记录清单指纹");

                // 同状态重存 → 指纹未变 → 跳过清单写（清单文件保持原内容）。
                assert!(session::save_session_to(app, &dir, cx));
                assert_eq!(app.session_last_manifest_digest, digest1, "无变化指纹不变");
                assert_eq!(
                    std::fs::read_to_string(dir.join("session.json")).unwrap(),
                    body1,
                    "无变化重存不应重写清单"
                );

                // 内容变化只重写槽位；清单不含正文 → 清单不重写（指纹不变）。
                app.tabs[i].editor.update(cx, |ed, _| ed.set_text("内容2"));
                assert!(session::save_session_to(app, &dir, cx));
                assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some("内容2"), "内容变化重写槽位");
                assert_eq!(app.session_last_manifest_digest, digest1, "内容不入清单 → 清单指纹不变");

                // 结构变化（新建 + 激活新 tab → active_tab 变）→ 清单指纹变并重写。
                let i2 = app.make_tab(window, cx);
                app.tabs[i2].editor.update(cx, |ed, _| ed.set_text("新"));
                assert!(session::save_session_to(app, &dir, cx));
                assert_ne!(app.session_last_manifest_digest, digest1, "active_tab 变化 → 清单重写");
                let body3 = std::fs::read_to_string(dir.join("session.json")).unwrap();
                assert_ne!(body3, body1, "清单文件应随 active_tab 重写");
            });
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 性能量级探针（`#[ignore]`，常规测试不跑，仅人工 `cargo test -- --ignored` 驱动）：
    /// N 个未命名选项卡 × 内容分级，连续跑 R 轮 `save_session_to`（含「读有效文本 + 写
    /// 槽位 + 写清单 + prune 目录扫描」），打印每轮平均毫秒——用于量化「切换选项卡/结构
    /// 变化即镜像全量重写槽位」的主线程 IO 成本，及 hash 跳过优化后的相对收益。
    /// 只打印、不断言；目录为进程独占临时会话目录，不触碰真实 %APPDATA%。
    #[gpui::test]
    #[ignore]
    async fn bench_save_session_slots(cx: &mut TestAppContext) {
        init_test(cx);
        let big1k = "x".repeat(1024);
        let big100k = "内容行甲\n".repeat(5000);
        let cases: Vec<(&str, usize, &str)> = vec![
            ("empty 8tabs", 8, ""),
            ("1KB 8tabs", 8, &big1k),
            ("100KB 4tabs", 4, &big100k),
        ];
        for (label, n_tabs, body) in cases {
            let dir = session_temp_dir(&format!("bench-{label}"));
            let (app, cx) = add_app_zero_tabs(cx);
            let rounds = 40usize;
            let t0 = std::time::Instant::now();
            cx.update(|window, cx| {
                app.update(cx, |app, cx| {
                    for _ in 0..n_tabs {
                        let idx = app.make_tab(window, cx);
                        app.tabs[idx].editor.update(cx, |ed, _| ed.set_text(body));
                    }
                    for r in 0..rounds {
                        let a = std::time::Instant::now();
                        assert!(session::save_session_to(app, &dir, cx));
                        if r == 0 {
                            eprintln!(
                                "[bench] slot={label}: 首轮 {}ms，指纹表 {} 项，目录项 {}",
                                a.elapsed().as_secs_f64() * 1000.0,
                                app.session_slot_hashes.len(),
                                std::fs::read_dir(&dir).map(|it| it.count()).unwrap_or(0)
                            );
                        }
                    }
                    let t_first = t0.elapsed();
                    let t_skip = std::time::Instant::now();
                    for _ in 0..(rounds * 3) {
                        assert!(session::save_session_to(app, &dir, cx));
                    }
                    let skip_ms = (t_skip.elapsed().as_secs_f64() * 1000.0) / (rounds * 3) as f64;
                    eprintln!(
                        "[bench] slot={label}: 建表+首轮= {:.1}ms；内容未变跳过轮均值 {:.4} ms/轮",
                        t_first.as_secs_f64() * 1000.0,
                        skip_ms
                    );
                    // 「切选项卡」等价场景：内容全不变，仅 active_tab 轮换 → 槽位全跳过、
                    // 清单每次必写（active_tab 变了）。
                    let t_sw = std::time::Instant::now();
                    let sw_n = 24usize;
                    for r in 0..sw_n {
                        app.active_tab = r % n_tabs.max(1);
                        assert!(session::save_session_to(app, &dir, cx));
                    }
                    let sw_ms = t_sw.elapsed().as_secs_f64() * 1000.0 / sw_n as f64;
                    eprintln!(
                        "[bench] slot={label}: 仅切 active_tab（清单每轮改写）{:.3} ms/轮",
                        sw_ms
                    );
                });
            });
            let ms = t0.elapsed().as_secs_f64() * 1000.0 / rounds as f64;
            eprintln!("[bench] slot={label}: n_tabs={n_tabs} rounds={rounds} => {ms:.3} ms/轮");
            let _ = std::fs::remove_dir_all(&dir);
        }
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

    /// A1 回归：渐进恢复（首帧后逐帧逐个选项卡）最终态与改前同步恢复完全一致；且
    /// **首帧前不做同步恢复**——`schedule_restore` 只登记帧回调、不开任何 tab。驱动：
    /// 显式临时目录 + `simulate_next_frame` 逐帧投递（对照 preview 里 on_next_frame
    /// 测试的既有姿势）。断言：恢复前零 tab / 恢复抑制生效 → 逐帧后 顺序/活动/内容/
    /// 路径/dirty 与同步版一致 → 清单/槽位被清理 → 放行持久化。
    #[gpui::test]
    async fn progressive_restore_matches_sync_final_state(cx: &mut TestAppContext) {
        init_test(cx);
        let dir = session_temp_dir("progressive_vs_sync");
        let f1 = std::env::temp_dir().join(format!("gpui-prog-sync-{}-a.rs", std::process::id()));
        let f2 = std::env::temp_dir().join(format!("gpui-prog-sync-{}-b.txt", std::process::id()));
        std::fs::write(&f1, "fn main() {\n    let x = 1;\n}\n").unwrap();
        std::fs::write(&f2, "line1\nline2\nline3\n").unwrap();
        let m = session_store::SessionManifest {
            version: 1,
            active_tab: 1,
            tabs: vec![
                session_store::SessionTab::File { path: f1.clone() },
                session_store::SessionTab::New { slot: "t0".into(), name: "未命名".into() },
                session_store::SessionTab::File { path: f2.clone() },
            ],
        };
        assert!(session_store::write_manifest(&dir, &m), "清单应写入");
        session_store::write_slot(&dir, "t0", "未命名内容甲");

        // 同步版基准（改前行为）：同清单恢复后的最终态。
        let (app_sync, cx_sync) = add_app_zero_tabs(cx);
        cx_sync.update(|window, cx| {
            app_sync.update(cx, |app, cx| {
                assert!(session::restore_session_from(app, &dir, window, cx));
            });
        });
        let sync_snap = cx_sync.update(|_w, cx| {
            let a = app_sync.read(cx);
            (
                a.tabs.len(),
                a.active_tab,
                a.tabs
                    .iter()
                    .map(|t| t.editor.read(cx).value().to_string())
                    .collect::<Vec<String>>(),
                a.tabs.iter().map(|t| t.file_path.clone()).collect::<Vec<Option<PathBuf>>>(),
                a.tabs.iter().map(|t| t.dirty).collect::<Vec<bool>>(),
            )
        });

        // 渐进版：新实例 schedule_restore → 首帧前零 tab（A1 核心）→ 逐帧泵到完成。
        // 注意：同步版恢复已完成并清盘（清单/槽位被删，属既有语义）→ 此处**重新播种**
        // 同清单 + 槽位，供渐进版原样恢复（最终态对拍才有意义）。
        assert!(session_store::write_manifest(&dir, &m), "渐进版清单应重新写入");
        session_store::write_slot(&dir, "t0", "未命名内容甲");
        let (app_prog, cx_prog) = add_app_zero_tabs(cx);
        cx_prog.update(|window, cx| {
            app_prog.update(cx, |app, cx| {
                let man = session_store::read_manifest(&dir).expect("清单应可读回");
                app.schedule_restore(window, cx, dir.clone(), man);
                assert!(app.tabs.is_empty(), "首帧前（on_next_frame 前）不得同步恢复任何 tab");
                assert!(app.session_restoring, "恢复期间应抑制持久化");
            });
        });
        let mut ran = 0usize;
        for _ in 0..16 {
            cx_prog.run_until_parked();
            let d = cx_prog.update(|window, cx| window.simulate_next_frame(cx));
            cx_prog.run_until_parked();
            if d > 0 {
                ran += 1;
            }
            let restoring = cx_prog.update(|_w, cx| app_prog.read(cx).session_restoring);
            if !restoring {
                break;
            }
        }
        assert!(ran >= 3, "渐进恢复应至少逐帧推进 3 次（3 个 tab），实际 {ran}");
        assert!(
            !cx_prog.update(|_w, cx| app_prog.read(cx).session_restoring),
            "恢复完成后应放行持久化"
        );
        let prog_snap = cx_prog.update(|_w, cx| {
            let a = app_prog.read(cx);
            (
                a.tabs.len(),
                a.active_tab,
                a.tabs
                    .iter()
                    .map(|t| t.editor.read(cx).value().to_string())
                    .collect::<Vec<String>>(),
                a.tabs.iter().map(|t| t.file_path.clone()).collect::<Vec<Option<PathBuf>>>(),
                a.tabs.iter().map(|t| t.dirty).collect::<Vec<bool>>(),
            )
        });
        assert_eq!(
            prog_snap, sync_snap,
            "渐进恢复最终态应与同步恢复完全一致（顺序/活动/内容/路径/dirty）"
        );
        assert!(!dir.join("session.json").exists(), "恢复后清单应删除");
        assert!(!dir.join("t0").exists(), "恢复后槽位应清理");
        let _ = std::fs::remove_file(&f1);
        let _ = std::fs::remove_file(&f2);
        let _ = std::fs::remove_dir_all(&dir);
    }
