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
            let before_tabs = app.tabs.len();
            app.apply_import_bundle(Some(parsed), &dir, _w, cx);
            assert_eq!(app.theme_pref, ThemePref::Dark, "主题偏好即时生效");
            assert!(app.dark_mode, "暗色主题即时生效");
            assert_eq!(app.font_size, 17.0, "字号即时生效");
            assert_eq!(app.sidebar.width, 123.0, "侧栏宽度即时生效");
            assert!(!app.sidebar.width_auto, "导入显式宽度 → 固定手动宽度");
            assert!(app.session_restore, "prefs 未显式给 → 默认开（会话可恢复）");
            assert!(app.close_to_tray, "托盘开关即时生效");
            assert_eq!(app.recent_files, vec!["a.md".to_string()], "最近打开恢复");
            assert_eq!(app.tabs.len(), before_tabs, "不重排当前窗口");
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
            assert!(!app.session_restore, "显式 false 诚实跟随");
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
            app.tabs[i0].editor.update(cx, |ed, _| ed.set_text("现有甲"));
            let i1 = app.make_tab(window, cx);
            app.tabs[i1].editor.update(cx, |ed, _| ed.set_text("现有乙"));
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
            assert_eq!(app.tabs.len(), 2, "不立即重排：仍 2 个选项卡");
            assert_eq!(app.active_tab, 1, "活动选项卡不变");
        });
    });
    let contents = cx.update(|_w, cx| {
        let a = app.read(cx);
        a.tabs
            .iter()
            .map(|t| t.editor.read(cx).value().to_string())
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
            a.tabs.iter().map(|t| t.file_path.clone()).collect();
        let contents: Vec<String> =
            a.tabs.iter().map(|t| t.editor.read(cx).value().to_string()).collect();
        (a.tabs.len(), a.active_tab, paths, contents)
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
            assert_eq!(app.theme_pref, ThemePref::Light, "配置照常应用");
            assert_eq!(app.font_size, 15.0);
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
                app.theme_pref,
                app.font_size,
                app.sidebar.width,
                app.sidebar.width_auto,
                app.tabs.len(),
                app.session_restore,
            );
            app.apply_import_bundle(None, &dir, _w, cx);
            let after = (
                app.theme_pref,
                app.font_size,
                app.sidebar.width,
                app.sidebar.width_auto,
                app.tabs.len(),
                app.session_restore,
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
            assert_eq!(app.theme_pref, ThemePref::Light, "配置照常应用");
            assert_eq!(app.font_size, 14.0);
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
            let before_theme = app.theme_pref;
            app.apply_import_bundle(Some(parsed), &dir, _w, cx);
            assert_eq!(app.theme_pref, before_theme, "失效 prefs 不误应用");
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
            app.theme_pref = ThemePref::Dark;
            app.dark_mode = true;
            app.font_size = 16.0;
            app.sidebar.width = 200.0;
            app.sidebar.width_auto = false;
            let i = app.make_tab(window, cx);
            app.tabs[i].editor.update(cx, |ed, _| ed.set_text("被导出内容"));
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
