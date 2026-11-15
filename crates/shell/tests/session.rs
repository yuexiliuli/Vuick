//! 会话持久化 / 设置导出域（session + settings_xfer 合并 target，§8.3）：
//! - `session.rs`：清单/槽位 roundtrip、保存/恢复端到端、开关语义、hash 跳过、渐进恢复。
//! - `settings_xfer.rs`：导出/导入打包 roundtrip、即时生效、零副作用、路径穿越/超上限护栏。
//! 两份断言内部与 lib 单测相应 `.rs` 逐字一致（P1 测试域化；settings_xfer 原单独 include，
//! 现并入本目标）。`session::save_session_to/restore_session_from` 经 `common::session`
//! 别名访问（`shell::app::session` 私有模块 pub 面，§8.6）；`GPUI_EDITOR_TEST` 由
//! `common::init_test` 注入，持久化恒短路不落 %APPDATA%。

mod common;

use common::*;

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
                    app.model.borrow().tabs[idx].payload.editor.update(cx, |ed, _| ed.set_text(txt));
                    if i == 2 {
                        app.activate_tab(idx, window, cx);
                    }
                }
                assert_eq!(app.model.borrow().active_tab, 2, "激活最后建的未命名选项卡");
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
                a.model.borrow().tabs.iter().map(|t| t.payload.editor.read(cx).value().to_string()).collect();
            (a.model.borrow().tabs.len(), a.model.borrow().active_tab, contents)
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
                app.model.borrow().tabs[idx1].payload.editor.update(cx, |ed, _| ed.set_text("未命名内容"));
                app.open_file(f2.to_str().unwrap(), window, cx); // idx 2 = file
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|_w, cx| {
            app_a.update(cx, |app, cx| {
                assert_eq!(app.model.borrow().active_tab, 2, "激活最后打开的 f2");
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
                a.model.borrow().tabs.iter().map(|t| t.payload.file_path.clone()).collect();
            let contents: Vec<String> =
                a.model.borrow().tabs.iter().map(|t| t.payload.editor.read(cx).value().to_string()).collect();
            (a.model.borrow().tabs.len(), a.model.borrow().active_tab, paths, contents)
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
            (a.model.borrow().tabs.len(), a.model.borrow().active_tab)
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
                app.model.borrow_mut().session_restore = false;
                assert!(session::save_session_to(app, &dir, cx));
            });
        });
        assert!(!dir.join("session.json").exists(), "开关关不应写新清单");
        assert!(!dir.join("t0").exists(), "开关关应清理遗留槽位");
        // 开关关 + 恢复 → false，不动作。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.model.borrow_mut().session_restore = false;
                assert!(!session::restore_session_from(app, &dir, window, cx));
            });
        });
        // 开关开但没有清单 → 恢复 false。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.model.borrow_mut().session_restore = true;
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
                a.model.borrow().tabs.iter().map(|t| t.payload.file_path.clone()).collect();
            (a.model.borrow().tabs.len(), a.model.borrow().active_tab, paths)
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
                app.model.borrow().tabs[i0].payload.editor.update(cx, |ed, _| ed.set_text("甲"));
                let i1 = app.make_tab(window, cx);
                app.model.borrow().tabs[i1].payload.editor.update(cx, |ed, _| ed.set_text("乙"));
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
                app.model.borrow().tabs[i1].payload.editor.update(cx, |ed, _| ed.set_text("乙"));
                let i2 = app.make_tab(window, cx);
                app.model.borrow().tabs[i2].payload.editor.update(cx, |ed, _| ed.set_text("丙"));
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
                        app.model.borrow().tabs[i].payload.editor.update(cx, |ed, _| ed.set_text("内容"));
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
            let tabs = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
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
        let tabs = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
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
                app.model.borrow().tabs[i0].payload.editor.update(cx, |ed, _| ed.set_text("甲"));
                let i1 = app.make_tab(window, cx);
                app.model.borrow().tabs[i1].payload.editor.update(cx, |ed, _| ed.set_text("乙"));
                assert!(session::save_session_to(app, &dir, cx));
                assert_eq!(app.model.borrow_mut().session_slot_hashes.len(), 2, "两未命名 → 两指纹");

                // 只改 t0 内容：重存后盘上 t0 更新、t1 不变（盘点正确性不受跳过影响）。
                app.model.borrow().tabs[i0].payload.editor.update(cx, |ed, _| ed.set_text("甲改"));
                assert!(session::save_session_to(app, &dir, cx));
                assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some("甲改"), "变化槽位重写");
                assert_eq!(session_store::read_slot(&dir, "t1").as_deref(), Some("乙"), "未变槽位盘面原样");

                // 关掉 idx=1 → 重存：指纹淘汰 t1、只留 live 槽 t0（重编号后仍对应正确内容）。
                app.remove_tab(i1, window, cx);
                assert!(session::save_session_to(app, &dir, cx));
                assert!(
                    app.model.borrow_mut().session_slot_hashes.len() == 1 && app.model.borrow_mut().session_slot_hashes.contains_key("t0"),
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
                app.model.borrow().tabs[i].payload.editor.update(cx, |ed, _| ed.set_text("内容"));

                // 开启态保存：槽位落盘 + 指纹记录。
                app.model.borrow_mut().session_restore = true;
                assert!(session::save_session_to(app, &dir, cx));
                assert!(session_store::read_slot(&dir, "t0").is_some(), "开启态槽位在盘");

                // 关（只清不写）：盘面与指纹一并清空。
                app.model.borrow_mut().session_restore = false;
                assert!(session::save_session_to(app, &dir, cx));
                assert!(!dir.join("t0").exists(), "关闭态应清盘面");
                assert!(app.model.borrow_mut().session_slot_hashes.is_empty(), "关闭态应清指纹");

                // 重开 → 重存：必须重新写槽位（不被旧指纹误跳过）。
                app.model.borrow_mut().session_restore = true;
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
                app.model.borrow().tabs[i].payload.editor.update(cx, |ed, _| ed.set_text("内容"));
                assert!(session::save_session_to(app, &dir, cx));
                let body1 = std::fs::read_to_string(dir.join("session.json")).unwrap();
                let digest1 = app.model.borrow_mut().session_last_manifest_digest;
                assert!(digest1.is_some(), "首存应记录清单指纹");

                // 同状态重存 → 指纹未变 → 跳过清单写（清单文件保持原内容）。
                assert!(session::save_session_to(app, &dir, cx));
                assert_eq!(app.model.borrow_mut().session_last_manifest_digest, digest1, "无变化指纹不变");
                assert_eq!(
                    std::fs::read_to_string(dir.join("session.json")).unwrap(),
                    body1,
                    "无变化重存不应重写清单"
                );

                // 内容变化只重写槽位；清单不含正文 → 清单不重写（指纹不变）。
                app.model.borrow().tabs[i].payload.editor.update(cx, |ed, _| ed.set_text("内容2"));
                assert!(session::save_session_to(app, &dir, cx));
                assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some("内容2"), "内容变化重写槽位");
                assert_eq!(app.model.borrow_mut().session_last_manifest_digest, digest1, "内容不入清单 → 清单指纹不变");

                // 结构变化（新建 + 激活新 tab → active_tab 变）→ 清单指纹变并重写。
                let i2 = app.make_tab(window, cx);
                app.model.borrow().tabs[i2].payload.editor.update(cx, |ed, _| ed.set_text("新"));
                assert!(session::save_session_to(app, &dir, cx));
                assert_ne!(app.model.borrow_mut().session_last_manifest_digest, digest1, "active_tab 变化 → 清单重写");
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
                        app.model.borrow().tabs[idx].payload.editor.update(cx, |ed, _| ed.set_text(body));
                    }
                    for r in 0..rounds {
                        let a = std::time::Instant::now();
                        assert!(session::save_session_to(app, &dir, cx));
                        if r == 0 {
                            eprintln!(
                                "[bench] slot={label}: 首轮 {}ms，指纹表 {} 项，目录项 {}",
                                a.elapsed().as_secs_f64() * 1000.0,
                                app.model.borrow_mut().session_slot_hashes.len(),
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
                        app.model.borrow_mut().active_tab = r % n_tabs.max(1);
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
        let idx = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len() - 1);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.close_tab(idx, window, cx));
        });
        settle_frames(cx, 5);
        assert!(!cx.has_pending_prompt(), "命名干净 tab 不应弹确认");
        let tabs = cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len());
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
                a.model.borrow().tabs.len(),
                a.model.borrow().active_tab,
                a.model.borrow().tabs
                    .iter()
                    .map(|t| t.payload.editor.read(cx).value().to_string())
                    .collect::<Vec<String>>(),
                a.model.borrow().tabs.iter().map(|t| t.payload.file_path.clone()).collect::<Vec<Option<PathBuf>>>(),
                a.model.borrow().tabs.iter().map(|t| t.payload.dirty).collect::<Vec<bool>>(),
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
                assert!(app.model.borrow().tabs.is_empty(), "首帧前（on_next_frame 前）不得同步恢复任何 tab");
                assert!(app.model.borrow_mut().session_restoring, "恢复期间应抑制持久化");
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
            let restoring = cx_prog.update(|_w, cx| app_prog.read(cx).model.borrow().session_restoring);
            if !restoring {
                break;
            }
        }
        assert!(ran >= 3, "渐进恢复应至少逐帧推进 3 次（3 个 tab），实际 {ran}");
        assert!(
            !cx_prog.update(|_w, cx| app_prog.read(cx).model.borrow().session_restoring),
            "恢复完成后应放行持久化"
        );
        let prog_snap = cx_prog.update(|_w, cx| {
            let a = app_prog.read(cx);
            (
                a.model.borrow().tabs.len(),
                a.model.borrow().active_tab,
                a.model.borrow().tabs
                    .iter()
                    .map(|t| t.payload.editor.read(cx).value().to_string())
                    .collect::<Vec<String>>(),
                a.model.borrow().tabs.iter().map(|t| t.payload.file_path.clone()).collect::<Vec<Option<PathBuf>>>(),
                a.model.borrow().tabs.iter().map(|t| t.payload.dirty).collect::<Vec<bool>>(),
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

// ===== settings_xfer（导出/导入设置）域，并入本 target（§8.3：session = session + settings_xfer）=====
// 断言内部与 lib 单测 `src/app/tests/settings_xfer.rs` 逐字一致。
// 配置 + 会话全量导出/导入（`settings_xfer`）测试：打包格式与结构、roundtrip 全等、
// 配置即时生效、导入不重排当前窗口、会话下一启动可恢复、损坏/版本不符/部分有效 toast 路径、
// 导出落盘文件存在。全部驱动纯逻辑函数（`build_export_bundle` / `parse_export_bundle` /
// `apply_import_bundle` / `export_to_path`），对话框本体 `#[cfg(not(test))]` 不真弹。

use config::user::ThemePref;

/// 导出/导入测试用临时目录（每次唯一 tag + 进程号，先清后建；**禁止**触碰真实
/// %APPDATA% 的 config/session 目录）。
fn xfer_temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("gpui-xfer-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 槽位样例内容：含引号 / 反斜杠 / 中文 / CRLF（serde_json 字符串转义、LF 规范化路径）。
fn sample_slots() -> Vec<(String, String)> {
    vec![
        (
            "t0".into(),
            "第一行\n第二行 \"引号\" 反斜杠 \\ 结尾".to_string(),
        ),
        ("t1".into(), "crlf\r\n行 中文 内容".to_string()),
    ]
}

/// 打包格式与 roundtrip 全等：魔力头/版本/结构自含；解析回 prefs、清单、槽位逐项全等
/// （配置字段 + 会话槽位；含转义特殊字符）。
#[test]
fn export_bundle_structure_and_roundtrip() {
    let prefs = serde_json::json!({
        "theme": "dark",
        "font_size": 16.5,
        "sidebar_width": 240.0,
        "session_restore": true,
        "close_to_tray": true,
        "opened_folder": r"C:\Data\docs",
        "recent_folders": [r"C:\Data\docs"],
        "recent_files": [r"C:\a.md", r"C:\b 把.md"],
        "music_dir": r"D:\music",
        "music_liked": [r"D:\music\周杰伦 - 晴天.ogg"],
        "music_volume": 0.42,
        "music_last_track": r"D:\music\林俊杰 - 江南.mp3",
        "music_progress_secs": 123.25,
    });
    let manifest = session_store::SessionManifest {
        version: 1,
        active_tab: 1,
        tabs: vec![
            session_store::SessionTab::File { path: std::path::PathBuf::from(r"C:\doc.md") },
            session_store::SessionTab::New { slot: "t0".into(), name: "未命名".into() },
        ],
    };
    let slots = sample_slots();
    let body = session_store::build_export_bundle(&prefs, &manifest, &slots);
    // 结构断言：魔力头 + 版本自含 + prefs/session 就位。
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["app"], serde_json::json!("gpui-editor"));
    assert_eq!(v["export_version"], serde_json::json!(1));
    assert_eq!(v["prefs"], prefs);
    assert_eq!(v["session"]["manifest"]["active_tab"], serde_json::json!(1));
    assert_eq!(v["session"]["manifest"]["tabs"].as_array().unwrap().len(), 2);
    assert_eq!(v["session"]["slots"]["t0"], serde_json::json!("第一行\n第二行 \"引号\" 反斜杠 \\ 结尾"));
    // roundtrip：解析回的数据与导出数据逐项全等。
    let parsed = session_store::parse_export_bundle(&body).expect("自产自入应可解析");
    assert_eq!(parsed.prefs.as_ref().expect("prefs 应在"), &prefs, "配置字段全等");
    let (p_manifest, p_slots) = parsed.session.expect("会话应在");
    assert_eq!(p_manifest, manifest, "清单全等");
    assert_eq!(p_slots, slots, "槽位内容全等");
}

/// 损坏 / 非本应用 / 版本不符 → Err（调用方 toast「不是有效…」，零副作用）；合法包但
/// 缺 prefs（非对象）/ 缺 session（非对象）→ 各自 None（部分可解析不整体回滚）。
#[test]
fn parse_rejects_invalid_and_partial_optional() {
    for bad in ["", "不是json", "[1,2,3]", "{}", r#"{"app":"gpui-editor"}"#] {
        assert!(session_store::parse_export_bundle(bad).is_err(), "应拒绝：{bad}");
    }
    let wrong_magic = serde_json::json!({"app": "别的应用", "export_version": 1}).to_string();
    assert!(session_store::parse_export_bundle(&wrong_magic).is_err(), "魔力头不符应拒绝");
    let wrong_ver = serde_json::json!({"app": "gpui-editor", "export_version": 99}).to_string();
    assert!(session_store::parse_export_bundle(&wrong_ver).is_err(), "版本不符应拒绝");

    // prefs 非对象 → None；session 正常 → 保留（配置侧缺失可跳过）。
    let no_prefs = serde_json::json!({
        "app": "gpui-editor", "export_version": 1, "prefs": 42,
        "session": {"manifest": {"version": 1, "active_tab": 0, "tabs": []}}
    })
    .to_string();
    let p = session_store::parse_export_bundle(&no_prefs).expect("合法包应解析");
    assert!(p.prefs.is_none(), "prefs 非对象 → None");
    assert!(p.session.is_some(), "session 应保留");

    // session 非对象 → None；prefs 正常 → 保留。
    let no_session = serde_json::json!({
        "app": "gpui-editor", "export_version": 1,
        "prefs": {"theme": "light"}, "session": "坏了"
    })
    .to_string();
    let p = session_store::parse_export_bundle(&no_session).expect("合法包应解析");
    assert!(p.prefs.is_some(), "prefs 应保留");
    assert!(p.session.is_none(), "session 非对象 → None");

    // session 对象但缺 manifest（损坏）→ None。
    let bad_session = serde_json::json!({
        "app": "gpui-editor", "export_version": 1, "prefs": {"theme": "light"},
        "session": {"manifest": 42}
    })
    .to_string();
    let p = session_store::parse_export_bundle(&bad_session).expect("合法包应解析");
    assert!(p.prefs.is_some());
    assert!(p.session.is_none(), "损坏 manifest → None");
}

/// 导入后配置即时生效：主题 / 字号 / 侧栏宽度立即刷新（可观测断言）；会话照常写盘
/// 供下一启动恢复（不重排当前窗口）；toast 说明。当前窗口不重排。
#[gpui::test]
async fn import_applies_prefs_immediately(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    let body = session_store::build_export_bundle(
        &serde_json::json!({
            "theme": "dark",
            "font_size": 17.0,
            "sidebar_width": 123.0,
            "close_to_tray": true,
            "recent_files": ["a.md"],
        }),
        &session_store::SessionManifest {
            version: 1,
            active_tab: 0,
            tabs: vec![session_store::SessionTab::New { slot: "t0".into(), name: "未命名".into() }],
        },
        &[("t0".into(), "会话正文".into())],
    );
    let parsed = session_store::parse_export_bundle(&body).unwrap();
    let dir = xfer_temp_dir("apply_prefs");
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            let before_tabs = app.model.borrow().tabs.len();
            app.apply_import_bundle(Some(parsed), &dir, _w, cx);
            assert_eq!(app.model.borrow_mut().theme_pref, ThemePref::Dark, "主题偏好即时生效");
            assert!(app.model.borrow_mut().dark_mode, "暗色主题即时生效");
            assert_eq!(app.model.borrow_mut().font_size, 17.0, "字号即时生效");
            assert_eq!(app.sidebar.width, 123.0, "侧栏宽度即时生效");
            assert!(!app.sidebar.width_auto, "导入显式宽度 → 固定手动宽度");
            assert!(app.model.borrow_mut().session_restore, "prefs 未显式给 → 默认开（会话可恢复）");
            assert!(app.model.borrow_mut().close_to_tray, "托盘开关即时生效");
            assert_eq!(app.model.borrow_mut().recent_files, vec!["a.md".to_string()], "最近打开恢复");
            assert_eq!(app.model.borrow().tabs.len(), before_tabs, "不重排当前窗口");
            let toast = app.notifications.last().map(|n| n.message_str().to_string());
            assert_eq!(toast.as_deref(), Some("已导入设置（配置 + 会话；会话下一启动恢复）"));
        });
    });
    // 会话已写盘供下一启动恢复。
    assert!(dir.join("session.json").is_file(), "会话已写盘");
    assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some("会话正文"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 「恢复上次会话」在导入 prefs 里显式 false → 诚实跟随：开关关闭、会话不写（清盘面），
/// toast 如实说明（不回写、不整体回滚）。
#[gpui::test]
async fn import_explicit_false_restore_honored(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    let body = session_store::build_export_bundle(
        &serde_json::json!({ "session_restore": false }),
        &session_store::SessionManifest {
            version: 1,
            active_tab: 0,
            tabs: vec![session_store::SessionTab::New { slot: "t0".into(), name: "未命名".into() }],
        },
        &[("t0".into(), "不该落盘的正文".into())],
    );
    let parsed = session_store::parse_export_bundle(&body).unwrap();
    let dir = xfer_temp_dir("restore_off");
    // 预置一份遗留会话（模拟此前开启时留下的陈旧清单/槽位）：显式关后应被清理。
    assert!(session_store::write_slot(&dir, "t0", "陈旧"));
    assert!(session_store::write_manifest(
        &dir,
        &session_store::SessionManifest {
            version: 1,
            active_tab: 0,
            tabs: vec![session_store::SessionTab::New { slot: "t0".into(), name: "未命名".into() }],
        }
    ));
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            app.apply_import_bundle(Some(parsed), &dir, _w, cx);
            assert!(!app.model.borrow_mut().session_restore, "显式 false 诚实跟随");
            let toast = app.notifications.last().map(|n| n.message_str().to_string());
            assert_eq!(
                toast.as_deref(),
                Some("配置已导入（恢复上次会话已关闭，未写入会话）")
            );
        });
    });
    assert!(!dir.join("session.json").exists(), "显式关不写会话并清理遗留清单");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 导入会话不立即重排当前窗口：当前选项卡数量 / 内容 / 活动下标不变；会话仅写盘
/// 供下一启动恢复。
#[gpui::test]
async fn import_session_does_not_rearrange_current_window(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app_zero_tabs(cx);
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            let i0 = app.make_tab(window, cx);
            app.model.borrow().tabs[i0].payload.editor.update(cx, |ed, _| ed.set_text("现有甲"));
            let i1 = app.make_tab(window, cx);
            app.model.borrow().tabs[i1].payload.editor.update(cx, |ed, _| ed.set_text("现有乙"));
            app.activate_tab(i1, window, cx);
        });
    });
    let body = session_store::build_export_bundle(
        &serde_json::json!({}),
        &session_store::SessionManifest {
            version: 1,
            active_tab: 0,
            tabs: vec![session_store::SessionTab::New { slot: "tt".into(), name: "未命名".into() }],
        },
        &[("tt".into(), "导入会话内容".into())],
    );
    let parsed = session_store::parse_export_bundle(&body).unwrap();
    let dir = xfer_temp_dir("no_rearrange");
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            app.apply_import_bundle(Some(parsed), &dir, _w, cx);
            assert_eq!(app.model.borrow().tabs.len(), 2, "不立即重排：仍 2 个选项卡");
            assert_eq!(app.model.borrow().active_tab, 1, "活动选项卡不变");
        });
    });
    let contents = cx.update(|_w, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs
            .iter()
            .map(|t| t.payload.editor.read(cx).value().to_string())
            .collect::<Vec<_>>()
    });
    assert_eq!(contents, vec!["现有甲", "现有乙"], "当前窗口内容不被替换");
    assert!(dir.join("session.json").is_file(), "会话已写盘供下一启动");
    assert_eq!(session_store::read_slot(&dir, "tt").as_deref(), Some("导入会话内容"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 导入会话下一启动可恢复：写回 session 目录的清单/槽位正确（夹具 + 模拟下启动
/// `restore_session_from` 全流程），文件 + 未命名混合。
#[gpui::test]
async fn import_session_restores_on_next_startup(cx: &mut TestAppContext) {
    init_test(cx);
    let f1 = std::env::temp_dir().join(format!("gpui-xfer-file-{}.txt", std::process::id()));
    std::fs::write(&f1, "文件正文").unwrap();
    let manifest = session_store::SessionManifest {
        version: 1,
        active_tab: 1,
        tabs: vec![
            session_store::SessionTab::File { path: f1.clone() },
            session_store::SessionTab::New { slot: "t0".into(), name: "未命名".into() },
        ],
    };
    let slots = vec![("t0".into(), "导入槽内容".to_string())];
    let body = session_store::build_export_bundle(&serde_json::json!({}), &manifest, &slots);
    let parsed = session_store::parse_export_bundle(&body).unwrap();
    let dir = xfer_temp_dir("next_startup");
    let (app, cx) = add_app_zero_tabs(cx);
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            app.apply_import_bundle(Some(parsed), &dir, _w, cx);
        });
    });
    // 盘面核对：清单 + 槽位（下一启动恢复的输入）。
    let back = session_store::read_manifest(&dir).expect("导入后清单应在");
    assert_eq!(back, manifest, "导入清单与导出全等");
    assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some("导入槽内容"));

    // 模拟下一启动：全新窗口实例 restore → 选项卡重建（数量/内容/活动一致）。
    let (app2, cx) = add_app_zero_tabs(cx);
    cx.update(|window, cx| {
        app2.update(cx, |app, cx| {
            assert!(session::restore_session_from(app, &dir, window, cx), "应恢复");
        });
    });
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let (tabs, act, paths, contents) = cx.update(|_w, cx| {
        let a = app2.read(cx);
        let paths: Vec<Option<std::path::PathBuf>> =
            a.model.borrow().tabs.iter().map(|t| t.payload.file_path.clone()).collect();
        let contents: Vec<String> =
            a.model.borrow().tabs.iter().map(|t| t.payload.editor.read(cx).value().to_string()).collect();
        (a.model.borrow().tabs.len(), a.model.borrow().active_tab, paths, contents)
    });
    assert_eq!(tabs, 2, "恢复 2 个选项卡");
    assert_eq!(act, 1);
    assert_eq!(paths[0].as_deref(), Some(f1.as_path()), "文件选项卡按路径重开");
    assert_eq!(contents[1], "导入槽内容", "未命名槽位内容恢复");
    let _ = std::fs::remove_file(&f1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 导出目录不可写/坏路径 → toast 失败（不崩、不落盘），对齐「失败 toast」处置。
#[gpui::test]
async fn export_to_unwritable_path_toasts_failure(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    let dir = xfer_temp_dir("export_fail");
    // 占位一个普通文件作为「父目录」→ 其下再建目录/文件必失败（模拟不可写/坏路径）。
    let blocker = dir.join("not_a_dir");
    std::fs::write(&blocker, "x").unwrap();
    let target = blocker.join("out.json");
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            app.export_to_path(&target, cx);
            let toast = app
                .notifications
                .last()
                .map(|n| n.message_str().to_string())
                .expect("应有失败 toast");
            assert!(toast.contains("导出设置失败"), "toast：{toast}");
        });
    });
    assert!(!target.exists(), "失败路径不落盘");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 导出包**无** session 段（而非损坏）→ prefs 照常应用 + toast「导出包无会话」。
#[gpui::test]
async fn import_bundle_without_session_applies_prefs(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    // 手构一个不含 "session" 键的合法包（build_export_bundle 恒含 session，此为外部产物）。
    let body = serde_json::json!({
        "app": "gpui-editor", "export_version": 1,
        "prefs": { "theme": "light", "font_size": 15.0 }
    })
    .to_string();
    let parsed = session_store::parse_export_bundle(&body).expect("合法包应解析");
    assert!(parsed.session.is_none());
    assert!(!parsed.session_present, "缺 session 段");
    let dir = xfer_temp_dir("no_session");
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            app.apply_import_bundle(Some(parsed), &dir, _w, cx);
            assert_eq!(app.model.borrow_mut().theme_pref, ThemePref::Light, "配置照常应用");
            assert_eq!(app.model.borrow_mut().font_size, 15.0);
            let toast = app.notifications.last().map(|n| n.message_str().to_string());
            assert_eq!(toast.as_deref(), Some("配置已导入（导出包无会话）"));
        });
    });
    assert!(!dir.join("session.json").exists(), "无会话不写盘");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 非法包（Err）→ 零副作用零误应用：应用状态与当前窗口不变，toast「不是有效…」。
#[gpui::test]
async fn apply_invalid_bundle_is_zero_side_effect(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    let dir = xfer_temp_dir("invalid");
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            let before = (
                // SC-3：同语句多次 `borrow_mut()`（RefMut 独占，先后求值即运行期
                // 双借 panic）→ 纯读取改 `borrow()`（共享借可并存），断言语义不变。
                app.model.borrow().theme_pref,
                app.model.borrow().font_size,
                app.sidebar.width,
                app.sidebar.width_auto,
                app.model.borrow().tabs.len(),
                app.model.borrow().session_restore,
            );
            app.apply_import_bundle(None, &dir, _w, cx);
            let after = (
                app.model.borrow().theme_pref,
                app.model.borrow().font_size,
                app.sidebar.width,
                app.sidebar.width_auto,
                app.model.borrow().tabs.len(),
                app.model.borrow().session_restore,
            );
            assert_eq!(after, before, "非法包零副作用（不误应用）");
            let toast = app
                .notifications
                .last()
                .map(|n| n.message_str().to_string())
                .expect("应有 toast");
            assert_eq!(toast, "不是有效的 gpui-editor 设置文件");
        });
    });
    assert!(!dir.join("session.json").exists(), "非法包不写会话");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 部分可解析①：prefs 有效但 session 损坏 → 配置照常应用 + toast 说明跳过（不整体回滚）。
#[gpui::test]
async fn partial_prefs_valid_session_broken(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    let body = serde_json::json!({
        "app": "gpui-editor", "export_version": 1,
        "prefs": { "theme": "light", "font_size": 14.0 },
        "session": { "manifest": 42 }
    })
    .to_string();
    let parsed = session_store::parse_export_bundle(&body).expect("合法包应解析");
    assert!(parsed.prefs.is_some());
    assert!(parsed.session.is_none());
    let dir = xfer_temp_dir("partial_prefs");
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            app.apply_import_bundle(Some(parsed), &dir, _w, cx);
            assert_eq!(app.model.borrow_mut().theme_pref, ThemePref::Light, "配置照常应用");
            assert_eq!(app.model.borrow_mut().font_size, 14.0);
            let toast = app.notifications.last().map(|n| n.message_str().to_string());
            assert_eq!(toast.as_deref(), Some("配置已导入，会话无效已跳过"));
        });
    });
    assert!(!dir.join("session.json").exists(), "损坏会话不被写入");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 部分可解析②：session 有效但 prefs 失效 → 会话照常导入 + toast 说明跳过（反向同理）。
#[gpui::test]
async fn partial_session_valid_prefs_broken(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    let body = serde_json::json!({
        "app": "gpui-editor", "export_version": 1,
        "prefs": "坏",
        "session": {
            "manifest": {"version": 1, "active_tab": 1, "tabs": [
                {"kind": "file", "path": "C:\\不存在.md"},
                {"kind": "new", "slot": "t0", "name": "未命名"}
            ]},
            "slots": {"t0": "会话内容"}
        }
    })
    .to_string();
    let parsed = session_store::parse_export_bundle(&body).expect("合法包应解析");
    assert!(parsed.prefs.is_none());
    assert!(parsed.session.is_some());
    let dir = xfer_temp_dir("partial_session");
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            let before_theme = app.model.borrow_mut().theme_pref;
            app.apply_import_bundle(Some(parsed), &dir, _w, cx);
            assert_eq!(app.model.borrow_mut().theme_pref, before_theme, "失效 prefs 不误应用");
            let toast = app.notifications.last().map(|n| n.message_str().to_string());
            assert_eq!(toast.as_deref(), Some("配置无效已跳过，会话已导入"));
        });
    });
    assert!(dir.join("session.json").is_file(), "会话照常导入");
    assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some("会话内容"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 导出落盘文件存在 + 内容可被自己再导入（roundtrip）：当前交互态（主题/字号/侧栏 +
/// 会话清单与槽位）→ `export_to_path` 原子写盘 → 解析回与源状态全等。
#[gpui::test]
async fn export_writes_file_and_reimports(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app_zero_tabs(cx);
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.model.borrow_mut().theme_pref = ThemePref::Dark;
            app.model.borrow_mut().dark_mode = true;
            app.model.borrow_mut().font_size = 16.0;
            app.sidebar.width = 200.0;
            app.sidebar.width_auto = false;
            let i = app.make_tab(window, cx);
            app.model.borrow().tabs[i].payload.editor.update(cx, |ed, _| ed.set_text("被导出内容"));
            app.activate_tab(i, window, cx);
        });
    });
    let dir = xfer_temp_dir("export_writes");
    let file = dir.join("out.json");
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            app.export_to_path(&file, cx);
            let toast = app.notifications.last().map(|n| n.message_str().to_string());
            assert!(toast.as_deref().unwrap().contains("已导出设置到"), "成功 toast 带路径");
        });
    });
    assert!(file.is_file(), "导出落盘文件存在");
    let text = std::fs::read_to_string(&file).unwrap();
    let parsed = session_store::parse_export_bundle(&text).expect("落盘文件可被自己解析");
    let p = parsed.prefs.expect("prefs");
    assert_eq!(p["theme"], serde_json::json!("dark"));
    assert_eq!(p["font_size"], serde_json::json!(16.0));
    assert_eq!(p["sidebar_width"], serde_json::json!(200.0));
    let (m, slots) = parsed.session.expect("会话");
    assert_eq!(m.tabs.len(), 1, "会话清单含被导出未命名选项卡");
    assert_eq!(m.active_tab, 0);
    assert_eq!(slots[0].0, "t0");
    assert_eq!(slots[0].1, "被导出内容", "槽位正文 roundtrip");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 安全①「槽位名路径穿越被拒」：`write_imported_session` 是导入包的**唯一下盘口**，
/// 恶意槽位名（`../x` / 绝对路径 / `c:foo` 盘符相对）必须被拒——任何越出 `dir` 的
/// 文件都不写入、清单不写（零副作用）。`write_imported_session` 先整体预检后落盘，
/// 拒掉即什么也不写。
#[test]
fn write_imported_session_rejects_path_traversal_slots() {
    let dir = xfer_temp_dir("traversal");
    let escape = dir.join("..").join("gpui-xfer-escape-root.txt");
    let _ = std::fs::remove_file(&escape); // 清残留（若上次失败留下）
    // 三类非法槽位名分别建包：路径穿越、Windows 绝对路径、盘符相对。
    for bad in ["../gpui-xfer-escape-root.txt", r"C:\Windows\system32\evil.txt", r"c:evil.txt"] {
        let ok = session_store::write_imported_session(
            &dir,
            &session_store::SessionManifest {
                version: 1,
                active_tab: 0,
                tabs: vec![session_store::SessionTab::New {
                    slot: bad.into(),
                    name: "未命名".into(),
                }],
            },
            &[(bad.into(), "恶意正文".into())],
        );
        assert!(!ok, "非法槽位名应被拒：{bad}");
        assert!(!dir.join("session.json").exists(), "被拒后清单不能写：{bad}");
        assert!(!dir.join(bad).exists(), "槽位本体不能被写：{bad}");
        if bad != r"C:\Windows\system32\evil.txt" && bad != r"c:evil.txt" {
            assert!(!escape.exists(), "越出 session 目录的文件不能存在：{bad}");
        }
    }
    // 合法槽位名照常可写（不被误伤）。
    let ok = session_store::write_imported_session(
        &dir,
        &session_store::SessionManifest {
            version: 1,
            active_tab: 0,
            tabs: vec![session_store::SessionTab::New {
                slot: "t0".into(),
                name: "未命名".into(),
            }],
        },
        &[("t0".into(), "正常正文".into())],
    );
    assert!(ok);
    assert_eq!(session_store::read_slot(&dir, "t0").as_deref(), Some("正常正文"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 安全②「绕过：清单引用的槽位名受同款防护」：恶意清单把 `slots` 之外的越界槽位名
/// （`../via_manifest`）写进 manifest.tabs → 整体拒绝、不写清单（否则下启动恢复 /
/// clear_session 会经槽位名越界删除任意文件）。
#[test]
fn write_imported_session_rejects_traversal_via_manifest() {
    let dir = xfer_temp_dir("traversal_manifest");
    let ok = session_store::write_imported_session(
        &dir,
        &session_store::SessionManifest {
            version: 1,
            active_tab: 0,
            tabs: vec![session_store::SessionTab::New {
                slot: "../via_manifest".into(),
                name: "未命名".into(),
            }],
        },
        &[], // slots 空，只有清单引用越界名
    );
    assert!(!ok, "清单引用的越界槽位名应被拒");
    assert!(!dir.join("session.json").exists(), "被拒后清单不写");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 安全③「槽位正文大小上限」：超 [`session_store::MAX_IMPORT_SLOT_BYTES`] 的槽位内容 →
/// 整个会话拒绝（零写入）；边界内照常收。防恶意/退化导出包用超大槽位撑爆磁盘。
#[test]
fn write_imported_session_rejects_oversize_slot() {
    let dir = xfer_temp_dir("oversize");
    let huge = "x".repeat(session_store::MAX_IMPORT_SLOT_BYTES + 1);
    let ok = session_store::write_imported_session(
        &dir,
        &session_store::SessionManifest {
            version: 1,
            active_tab: 0,
            tabs: vec![session_store::SessionTab::New {
                slot: "t0".into(),
                name: "未命名".into(),
            }],
        },
        &[("t0".into(), huge)],
    );
    assert!(!ok, "超上限槽位应整体拒绝");
    assert!(!dir.join("session.json").exists(), "被拒后清单不写");
    assert!(!dir.join("t0").exists(), "被拒后槽位不写");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 安全 + 交互闭环：恶意槽位名经 `apply_import_bundle` 全链走到 **零写入** + toast
/// 如实说明（会话无效未应用），且不产生越界文件。证明「拒掉」在真正入口生效。
#[gpui::test]
async fn apply_malicious_slot_is_zero_write(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    let dir = xfer_temp_dir("apply_malicious");
    let escape = dir.join("..").join("gpui-xfer-apply-escape.txt");
    let _ = std::fs::remove_file(&escape);
    let body = session_store::build_export_bundle(
        &serde_json::json!({}),
        &session_store::SessionManifest {
            version: 1,
            active_tab: 0,
            tabs: vec![session_store::SessionTab::New {
                slot: "../gpui-xfer-apply-escape.txt".into(),
                name: "未命名".into(),
            }],
        },
        &[("../gpui-xfer-apply-escape.txt".into(), "恶意正文".into())],
    );
    let parsed = session_store::parse_export_bundle(&body).unwrap();
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            app.apply_import_bundle(Some(parsed), &dir, _w, cx);
            let toast = app.notifications.last().map(|n| n.message_str().to_string());
            assert_eq!(
                toast.as_deref(),
                Some("配置已导入，会话写入失败"),
                "会话侧零应用 → 如实说明"
            );
        });
    });
    assert!(!dir.join("session.json").exists(), "恶意包不写会话清单");
    assert!(!escape.exists(), "恶意包不写越界文件");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 导入 `opened_folder` 的即时生效带「根生效 + 子树可读」：设根 + 后台扫根直接子项
/// 完成后，文件树 children 已含根的直接子项（复现:只设根不扫会把树停在永无子行的死态）。
#[gpui::test]
async fn import_opened_folder_pops_children(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    let root = xfer_temp_dir("opened_folder");
    std::fs::write(root.join("子文件.txt"), "正文").unwrap();
    let body = session_store::build_export_bundle(
        &serde_json::json!({
            "opened_folder": root.to_string_lossy(),
        }),
        &session_store::SessionManifest { version: 1, active_tab: 0, tabs: vec![] },
        &[],
    );
    let parsed = session_store::parse_export_bundle(&body).unwrap();
    let dir = xfer_temp_dir("opened_folder_session");
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| {
            app.apply_import_bundle(Some(parsed), &dir, _w, cx);
        });
    });
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let (root_, kids) = cx.update(|_w, cx| {
        let a = app.read(cx);
        (
            a.sidebar.files.root.clone(),
            a.sidebar
                .files
                .children
                .get(&root)
                .map(|c| c.iter().map(|e| e.path.clone()).collect::<Vec<_>>()),
        )
    });
    assert_eq!(root_.as_deref(), Some(root.as_path()), "导入根即当前文件树根");
    assert_eq!(
        kids,
        Some(vec![root.join("子文件.txt")]),
        "根的直接子项已扫描入树（非死树）"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&dir);
}

