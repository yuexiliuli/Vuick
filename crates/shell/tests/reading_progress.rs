//! 「阅读进度恢复」域（R1-R6 回归断言，真夹具 headless、不开 GUI）。
//!
//! 链路：打开 → 读到某位置（滚到行 / 页内偏移）→ 关 tab（`AppModel::remove_tab` 内
//! 触发 `save_reading_progress`）→ 重开 → `apply_reading_progress` 恢复 → 推进帧 →
//! 断言位置对表。
//! - 测试构建下 `is_test_build()` 短路**磁盘写**（不污染真实/隔离目录），内存 map 承担
//!   往返；存储层（save/load/cache/roundtrip）由 `session_store::reading_progress` 单测覆盖。
//! - PDF 夹具 `target/pdf测试.pdf` / EPUB 夹具 `target/测试epub.epub` 缺失 → SKIP 注明
//!   （K56：夹具不入库、缺则 SKIP 是常态，非回归）。
//! - PDF 测试用 `pdf::test_util::lock()` 串行（pdfium 工作线程单例，锁毒化级联遇之当预存）。

mod common;

use common::*;

/// 构造一个含 200 行的临时 .md 夹具（行号用 0 基 `Ln <i>` 文本，行数足够滚动）。
fn rp_md_fixture(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("gpui-rp-{tag}-{}.md", std::process::id()));
    let body = (0..200).map(|i| format!("Line {i} —— 阅读进度测试正文")).collect::<Vec<_>>().join("\n");
    std::fs::write(&p, body).unwrap();
    p
}

fn rp_txt_fixture(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("gpui-rp-{tag}-{}.txt", std::process::id()));
    let body = (0..120).map(|i| format!("text line {i}")).collect::<Vec<_>>().join("\n");
    std::fs::write(&p, body).unwrap();
    p
}

/// 打开文件 → 驱动若干帧 → 返回 (app, 该文件选项卡下标)。
fn open_file_settled(
    app: &Entity<EditorApp>,
    cx: &mut VisualTestContext,
    path: &std::path::Path,
) -> usize {
    let p = path.to_path_buf();
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
    });
    settle_frames(cx, 6);
    cx.update(|_w, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs.len() - 1
    })
}

/// 取选项卡的右栏 md 预览（WyEditor）实体（该 md tab 建有 `md_wy` 时）。
fn md_wy_of(
    app: &Entity<EditorApp>,
    cx: &mut VisualTestContext,
    idx: usize,
) -> Option<gpui::Entity<WyEditor>> {
    cx.update(|_, cx| app.read(cx).model.borrow().tabs[idx].payload.md_wy.clone())
}

/// 构造一个「逐行独立块」的临时 .md 夹具：每行是一个独立标题（H3）段 →
/// 每行自成一个块，`block_start_of_line(i)==i`、预览 `top_source_line` 可无歧义断言
/// （若用 R1 的连续行夹具，整文是一个段落块、块起点恒 0，预览收敛断言无法分辨）。
fn rp_md_block_fixture(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("gpui-rp-blk-{tag}-{}.md", std::process::id()));
    let body = (0..200)
        .map(|i| format!("### 第 {i} 节 — 阅读进度预览收敛测试"))
        .collect::<Vec<_>>()
        .join("\n\n");
    std::fs::write(&p, body).unwrap();
    p
}

/// R1+预览：关 tab → `open_file` 重开 md，**编辑区**恢复上次源码行（R1 既有），
/// 且 **右栏预览** 随 sync 状态机收敛到同一块起点（候选 b 的 headless 复现探针：
/// 真机「md 用【打开最近】重开不复位」若根因是「编辑器恢复成功、预览未动」，
/// 本断言会 FAIL）。逐行独立块夹具使块起点 = 行号本身，无歧义。
#[gpui::test]
async fn restore_md_preview_converges_after_reopen(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = rp_md_block_fixture("pv1");
    let idx = open_file_settled(&app, cx, &p);
    let ed = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[idx].payload.editor.clone()
    });
    // 编辑区滚到第 50 节（每行独立块 → 块起点 = 50）。
    cx.update(|_, cx| {
        ed.update(cx, |e, _| e.scroll_to_line(50));
    });
    settle_frames(cx, 12);
    assert_eq!(cx.update(|_, cx| ed.read(cx).viewport_top_line()), 50, "滚后视口顶应为目标行");

    // 关 tab → save_reading_progress（内存 map）。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.remove_tab(idx, window, cx));
    });
    let saved = cx.update(|_, cx| app.read(cx).model.borrow().reading_progress.clone());
    assert!(
        saved.values().any(|rp| matches!(rp, shell::app::ReadingProgress::Markdown { top_line } if *top_line == 50)),
        "关 tab 应把行 50 存进 map"
    );

    // 重开 → 编辑区恢复行 50（R1 口径：需投递 next-frame 才触发 apply_text_restore）。
    let idx2 = open_file_settled(&app, cx, &p);
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    settle_frames(cx, 6);
    let ed2 = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[idx2].payload.editor.clone()
    });
    assert_eq!(
        cx.update(|_, cx| ed2.read(cx).viewport_top_line()),
        50,
        "重开编辑区应回到上次阅读行（R1）"
    );
    // 预览侧：右栏 WyEditor 收敛到编辑区顶行所在块起点（=50）。
    let wy = md_wy_of(&app, cx, idx2).expect("md tab 应持有 md_wy 预览宿主");
    let mut prev_line = None;
    for _ in 0..30 {
        settle_frames(cx, 2);
        let line = cx.update(|_, cx| wy.read(cx).top_source_line());
        prev_line = line;
        if line == Some(50) {
            break;
        }
    }
    assert_eq!(
        prev_line,
        Some(50),
        "重开后右栏预览应随 sync 收敛到编辑区顶块起点 50（候选 b headless 探针）"
    );
    let _ = std::fs::remove_file(&p);
}

/// R1+预览（预览为主读取面）：上次会话以**右栏预览**为滚动读取面——开 md → 编辑区滚
/// 到 50 → 预览收敛 50 → 再把预览钉到第 90 节（sync 反哺编辑区到 90）→ 关 tab → 重开
/// → 编辑区与预览都应回到 90（保存读取的是编辑区位置；需编辑区已被预览驱动到位）。
#[gpui::test]
async fn restore_md_when_preview_was_reading_surface(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = rp_md_block_fixture("pv2");
    let idx = open_file_settled(&app, cx, &p);
    let ed = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[idx].payload.editor.clone()
    });
    let wy = md_wy_of(&app, cx, idx).expect("md tab 应持有 md_wy 预览宿主");
    // 先让两侧都在 50（编辑区主导 → 预览收敛）。
    cx.update(|_, cx| {
        ed.update(cx, |e, _| e.scroll_to_line(50));
    });
    settle_frames(cx, 12);
    assert_eq!(cx.update(|_, cx| ed.read(cx).viewport_top_line()), 50, "编辑区应先到 50");
    // 以预览为主读取面：把预览钉到第 90 节 → sync 反哺编辑区到块起点 90。双栏收敛
    // 是「save 读到真实读位」的前提：预览→编辑区传播走 sync 状态机（含 Scrub/回拉
    // 判定），收敛存在数帧窗口（曾实测 ~30 帧）——夹具预算 120 帧，未收敛即 FAIL
    // （= 候选 a「sync 未对齐导致 editor 停在 0/旧值」的 headless 复现）。
    cx.update(|_, cx| {
        wy.update(cx, |w, _| w.set_scroll_target(90));
    });
    let mut both = None;
    for _ in 0..60 {
        settle_frames(cx, 2);
        let ed_top = cx.update(|_, cx| ed.read(cx).viewport_top_line());
        let wy_top = cx.update(|_, cx| wy.read(cx).top_source_line());
        both = Some((ed_top, wy_top));
        if wy_top == Some(90) && ed_top == 90 {
            break;
        }
    }
    let (ed_top, wy_top) = both.expect("收敛探测应有值");
    assert_eq!(wy_top, Some(90), "预览应停在钉定目标块起点 90（双栏收敛窗口内）");
    assert_eq!(ed_top, 90, "编辑区应被预览主导同步到 90（保存读取面）");

    // 关 tab → 保存；重开 → 两侧恢复 90。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.remove_tab(idx, window, cx));
    });
    let saved = cx.update(|_, cx| app.read(cx).model.borrow().reading_progress.clone());
    assert!(
        saved.values().any(|rp| matches!(rp, shell::app::ReadingProgress::Markdown { top_line } if *top_line == 90)),
        "关 tab 应把行 90 存进 map"
    );
    let idx2 = open_file_settled(&app, cx, &p);
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    settle_frames(cx, 6);
    let ed2 = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[idx2].payload.editor.clone()
    });
    assert_eq!(
        cx.update(|_, cx| ed2.read(cx).viewport_top_line()),
        90,
        "重开编辑区应回到 90"
    );
    let wy2 = md_wy_of(&app, cx, idx2).expect("重开后 md tab 应持有 md_wy 预览宿主");
    let mut prev_line = None;
    for _ in 0..30 {
        settle_frames(cx, 2);
        let line = cx.update(|_, cx| wy2.read(cx).top_source_line());
        prev_line = line;
        if line == Some(90) {
            break;
        }
    }
    assert_eq!(
        prev_line,
        Some(90),
        "预览为主读取面时，重开后预览应回到 90（右上栏收敛断言）"
    );
    let _ = std::fs::remove_file(&p);
}

/// R1/R4：md 文本重开恢复转顶部源码行。
#[gpui::test]
async fn restore_md_top_line_after_reopen_and_off_switch(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = rp_md_fixture("md1");
    let idx = open_file_settled(&app, cx, &p);

    // 打开默认回到顶部。
    let ed = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[idx].payload.editor.clone()
    });
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).viewport_top_line()),
        0,
        "新开应停在顶部"
    );
    // 程序滚到源码行 50（模拟用户读到第 51 行附近）。
    cx.update(|_, cx| {
        ed.update(cx, |e, _| e.scroll_to_line(50));
    });
    settle_frames(cx, 4);
    let saved_line = cx.update(|_, cx| ed.read(cx).viewport_top_line());
    assert_eq!(saved_line, 50, "滚后视口顶应为目标行");
    assert!(
        cx.update(|_, cx| {
            let a = app.read(cx);
            !a.model.borrow().reading_progress.is_empty()
        }) == false,
        "关 tab 前 map 尚未写入（防抖在测试构建空转）——下方关 tab 才是保存点"
    );

    // 关 tab → save_reading_progress（内存 map）。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.remove_tab(idx, window, cx));
    });
    let saved_in_map = cx.update(|_, cx| app.read(cx).model.borrow().reading_progress.clone());
    assert!(
        saved_in_map.values().any(|rp| matches!(rp, shell::app::ReadingProgress::Markdown { top_line } if *top_line == 50)),
        "关 tab 应把当前行进 map（saved={saved_line}）"
    );

    // 重开 → 恢复到同一源码行（首帧后滚动：需投递 next-frame 才触发 apply_text_restore）。
    let idx2 = open_file_settled(&app, cx, &p);
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    settle_frames(cx, 6);
    let ed2 = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[idx2].payload.editor.clone()
    });
    let restored = cx.update(|_, cx| ed2.read(cx).viewport_top_line());
    assert_eq!(restored, 50, "重开应回到上次阅读行（R1）");
    let _ = std::fs::remove_file(&p);
}

/// R1（txt/纯文本）：与 md 同链路验证纯文本也被记忆。
#[gpui::test]
async fn restore_txt_top_line_after_reopen(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = rp_txt_fixture("txt1");
    let idx = open_file_settled(&app, cx, &p);
    let ed = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[idx].payload.editor.clone()
    });
    cx.update(|_, cx| {
        ed.update(cx, |e, _| e.scroll_to_line(40));
    });
    settle_frames(cx, 4);
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.remove_tab(idx, window, cx));
    });
    let idx2 = open_file_settled(&app, cx, &p);
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    settle_frames(cx, 6);
    let ed2 = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[idx2].payload.editor.clone()
    });
    assert_eq!(
        cx.update(|_, cx| ed2.read(cx).viewport_top_line()),
        40,
        "txt 重开应回到上次阅读行"
    );
    let _ = std::fs::remove_file(&p);
}

/// R4：开关显式关 → 重开回到顶部（即便 map 里残留旧进度也不恢复）。
#[gpui::test]
async fn explicit_off_reopens_at_top(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = rp_md_fixture("off");
    let idx = open_file_settled(&app, cx, &p);
    let ed = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[idx].payload.editor.clone()
    });
    cx.update(|_, cx| {
        ed.update(cx, |e, _| e.scroll_to_line(60));
    });
    settle_frames(cx, 4);
    // 开关显式关（先翻转两次验证「翻转随动」语义，最后停在关）。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.toggle_remember_reading_progress(window, cx); // 默认开 → 关
            app.toggle_remember_reading_progress(window, cx); // 关 → 开
            app.toggle_remember_reading_progress(window, cx); // 开 → 关
        });
    });
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.remove_tab(idx, window, cx));
    });
    // 关 tab 时开关已关 → map 不应新增/更新该进度。
    let off_map = cx.update(|_, cx| app.read(cx).model.borrow().reading_progress.clone());
    assert!(
        !off_map.values().any(|rp| matches!(rp, shell::app::ReadingProgress::Markdown { top_line } if *top_line == 60)),
        "开关关时关 tab 不应记进度"
    );
    let idx2 = open_file_settled(&app, cx, &p);
    let ed2 = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[idx2].payload.editor.clone()
    });
    assert_eq!(
        cx.update(|_, cx| ed2.read(cx).viewport_top_line()),
        0,
        "开关显式关：重开回到顶部（R4）"
    );
    let _ = std::fs::remove_file(&p);
}

/// R5：菜单项「记住阅读进度」☑/☐ 动态标签 + 勾选状态随开关翻转。
#[gpui::test]
async fn menu_label_follows_toggle(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    // 默认开。
    assert!(
        cx.update(|_, cx| app.read(cx).model.borrow().restore_reading_progress),
        "开关默认开"
    );
    // label 纯函数（菜单项渲染共用同一来源）。
    assert_eq!(EditorApp::remember_reading_progress_label(true), "☑ 记住阅读进度");
    assert_eq!(EditorApp::remember_reading_progress_label(false), "☐ 记住阅读进度");
    // 翻转 → 关：模型 flag + label 随动。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.toggle_remember_reading_progress(window, cx));
    });
    assert!(
        !cx.update(|_, cx| app.read(cx).model.borrow().restore_reading_progress),
        "翻转后应关闭"
    );
    assert_eq!(
        EditorApp::remember_reading_progress_label(
            cx.update(|_, cx| app.read(cx).model.borrow().restore_reading_progress)
        ),
        "☐ 记住阅读进度",
        "关态 label ☐"
    );
    // 再翻转 → 开：label 回到 ☑。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.toggle_remember_reading_progress(window, cx));
    });
    assert_eq!(
        EditorApp::remember_reading_progress_label(
            cx.update(|_, cx| app.read(cx).model.borrow().restore_reading_progress)
        ),
        "☑ 记住阅读进度",
        "再翻转回开态 label ☑"
    );
}

/// R2：PDF 同 zoom 档——重开恢复 `current_page` 与页内比例。
#[gpui::test]
async fn restore_pdf_page_and_frac_same_zoom(cx: &mut TestAppContext) {
    if pdf::test_util::missing_pdf_fixture() {
        eprintln!("SKIP: missing target/pdf测试.pdf（固件不入库，本机无真 PDF）");
        return;
    }
    let _pdf_guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let pdf = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试.pdf");
    assert!(pdf.exists(), "测试 PDF 应存在");
    let idx = open_file_settled(&app, cx, &pdf);
    // 等 PdfViewer 后台加载完成。
    for _ in 0..1200 {
        cx.run_until_parked();
        let loaded = cx.update(|_w, cx| {
            tab_pdf(&app.read(cx).model.borrow().tabs[idx])
                .map(|pv| pv.read(cx).is_loaded())
                .unwrap_or(false)
        });
        if loaded {
            break;
        }
    }
    let pv = cx
        .update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[idx]).unwrap().clone());
    assert!(cx.update(|_, cx| pv.read(cx).is_loaded()), "PDF 应加载完成");
    // 滚到页内非零偏移（程序滚动到页列中部）。
    cx.update(|_, cx| {
        pv.update(cx, |v, _| v.scroll_to_top_px(1500.0));
    });
    settle_frames(cx, 6);
    let saved = cx.update(|_, cx| pv.read(cx).reading_progress().unwrap());
    let (spage, sfrac, szoom) = (saved.0, saved.1, saved.2);
    assert!(sfrac > 0.01 && sfrac < 0.999, "采样点应在页内中部（frac={sfrac}）");

    // 关 tab → 保存；重开 → 自动恢复。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.remove_tab(idx, window, cx));
    });
    let idx2 = open_file_settled(&app, cx, &pdf);
    for _ in 0..1200 {
        cx.run_until_parked();
        let loaded = cx.update(|_w, cx| {
            tab_pdf(&app.read(cx).model.borrow().tabs[idx2])
                .map(|pv| pv.read(cx).is_loaded())
                .unwrap_or(false)
        });
        if loaded {
            break;
        }
    }
    let pv2 = cx
        .update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[idx2]).unwrap().clone());
    settle_frames(cx, 6);
    let cur_page = cx.update(|_, cx| pv2.read(cx).current_page().map(|(p, _)| p));
    let restored = cx.update(|_, cx| pv2.read(cx).reading_progress().unwrap_or((0, 0.0, 1.0)));
    assert_eq!(
        cur_page,
        Some(spage + 1),
        "R2：重开后 current_page 应恢复为保存页（保存 0 基 {spage}）"
    );
    assert_eq!(restored.0, spage, "R2：重开后页下标一致");
    assert!(
        (restored.1 - sfrac).abs() < 1e-2,
        "R2：页内比例往返一致（saved={} restored={}", sfrac,
        restored.1
    );
    assert!((restored.2 - szoom).abs() < 1e-3, "R2：zoom 恢复到保存档");
}

/// R3：PDF 跨 zoom——恢复后存档 zoom 生效；换档后页内**显示比例**保持（防 frac 分母用
/// pt 高的退化——同档往返测不出，必须跨档断言）。
#[gpui::test]
async fn restore_pdf_frac_survives_zoom_switch(cx: &mut TestAppContext) {
    if pdf::test_util::missing_pdf_fixture() {
        eprintln!("SKIP: missing target/pdf测试.pdf（固件不入库，本机无真 PDF）");
        return;
    }
    let _pdf_guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let pdf = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试.pdf");
    let idx = open_file_settled(&app, cx, &pdf);
    for _ in 0..1200 {
        cx.run_until_parked();
        let loaded = cx.update(|_w, cx| {
            tab_pdf(&app.read(cx).model.borrow().tabs[idx])
                .map(|pv| pv.read(cx).is_loaded())
                .unwrap_or(false)
        });
        if loaded {
            break;
        }
    }
    let pv = cx
        .update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[idx]).unwrap().clone());
    // 档 A = 1.5：设缩放后再滚到页内中部（保存点的显示分母随档 A）。
    cx.update(|_, cx| {
        pv.update(cx, |v, _| v.set_zoom(1.5));
    });
    cx.update(|_, cx| {
        pv.update(cx, |v, _| v.scroll_to_top_px(1500.0));
    });
    settle_frames(cx, 6);
    let saved = cx.update(|_, cx| pv.read(cx).reading_progress().unwrap());
    assert!((saved.2 - 1.5).abs() < 1e-3, "保存档应为 1.5（saved.zoom={}", saved.2);

    // 关 tab → 保存；重开 → 自动恢复（应把 zoom 恢复回 1.5 + 按该档布局恢复位置）。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.remove_tab(idx, window, cx));
    });
    let idx2 = open_file_settled(&app, cx, &pdf);
    for _ in 0..1200 {
        cx.run_until_parked();
        let loaded = cx.update(|_w, cx| {
            tab_pdf(&app.read(cx).model.borrow().tabs[idx2])
                .map(|pv| pv.read(cx).is_loaded())
                .unwrap_or(false)
        });
        if loaded {
            break;
        }
    }
    let pv2 = cx
        .update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[idx2]).unwrap().clone());
    settle_frames(cx, 6);
    let restored = cx.update(|_, cx| pv2.read(cx).reading_progress().unwrap_or((0, 0.0, 0.0)));
    assert!(
        (restored.2 - 1.5).abs() < 1e-3,
        "R3：重开应恢复保存档 zoom=1.5（restored.zoom={}", restored.2
    );
    let (rpage, rfrac) = (restored.0, restored.1);
    assert_eq!(rpage, saved.0, "R3：重开页下标一致");
    assert!(
        (rfrac - saved.1).abs() < 1e-2,
        "R3：重开页内比例往返一致（saved={} restored={}", saved.1,
        rfrac
    );

    // 档 B = 2.5：程序换档（gpui 缩放重锚保持**页内显示比例**，已由 layout 单测锁契约）；
    // 若恢复公式/读数用 pt 高当分母，换档后该比例会以 `pw/pt_w` 因子漂移 → 跳出容差。
    cx.update(|_, cx| {
        pv2.update(cx, |v, _| v.set_zoom(2.5));
    });
    settle_frames(cx, 6);
    let after_zoom = cx.update(|_, cx| pv2.read(cx).reading_progress().unwrap_or((0, 0.0, 0.0)));
    assert_eq!(after_zoom.0, saved.0, "R3：换档锚点页不变");
    assert!(
        (after_zoom.1 - saved.1).abs() < 1e-2,
        "R3：换档后页内**显示**比例保持（saved={} after_zoom={}", saved.1,
        after_zoom.1
    );
}

/// R6：EPUB 重开恢复视口顶源码行（缺夹具 SKIP，非回归）。
#[gpui::test]
async fn restore_epub_top_line_after_reopen(cx: &mut TestAppContext) {
    let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/测试epub.epub");
    if !epub.exists() {
        eprintln!("SKIP: missing target/测试epub.epub（固件不入库，本机无真 EPUB）");
        return;
    }
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let idx = open_file_settled(&app, cx, &epub);
    // 等 WyEditor 宿主装配完（帧就绪后 top_source_line 才可读）。
    for _ in 0..60 {
        cx.run_until_parked();
        let has_line = cx.update(|_w, cx| {
            app.read(cx)
                .model
                .borrow()
                .tabs
                .get(idx)
                .and_then(|t| t.payload.md_wy.as_ref())
                .and_then(|w| w.read(cx).top_source_line())
                .is_some()
        });
        if has_line {
            break;
        }
    }
    // 模拟读到某章中层：set_scroll_target 直接钉到目标行（WY 自钉循环）。
    let target_line = 40usize;
    let wy = cx.update(|_, cx| app.read(cx).model.borrow().tabs[idx].payload.md_wy.clone());
    let wy = wy.expect("EPUB 选项卡应持有 md_wy 只读宿主");
    cx.update(|_, cx| {
        wy.update(cx, |wy, _| wy.set_scroll_target(target_line));
    });
    settle_frames(cx, 12);
    let saved_line = cx.update(|_, cx| {
        app.read(cx)
            .model
            .borrow()
            .tabs[idx].payload.md_wy
            .as_ref()
            .and_then(|w| w.read(cx).top_source_line())
    });
    assert!(saved_line.is_some(), "EPUB 帧就绪后应能读到顶行");
    let saved_line = saved_line.unwrap();

    // 关 tab → 保存；重开 → 自动恢复到同块起点。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.remove_tab(idx, window, cx));
    });
    let idx2 = open_file_settled(&app, cx, &epub);
    for _ in 0..60 {
        cx.run_until_parked();
        let has_line = cx.update(|_w, cx| {
            app.read(cx)
                .model
                .borrow()
                .tabs
                .get(idx2)
                .and_then(|t| t.payload.md_wy.as_ref())
                .and_then(|w| w.read(cx).top_source_line())
                .is_some()
        });
        if has_line {
            break;
        }
    }
    settle_frames(cx, 12);
    let restored_line = cx.update(|_, cx| {
        app.read(cx)
            .model
            .borrow()
            .tabs[idx2].payload.md_wy
            .as_ref()
            .and_then(|w| w.read(cx).top_source_line())
    });
    assert_eq!(
        restored_line,
        Some(saved_line),
        "R6：EPUB 重开应恢复视口顶源码行（块起点对齐）"
    );
}

/// 会话恢复测试用临时会话目录（唯一 tag + 进程号，先清后建；**禁止**触碰真实
/// `%APPDATA%\gpui-editor\session`——与 `tests/session.rs` 同类隔离姿势）。
fn rp_session_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("gpui-rp-session-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 驱动「单文件会话」的**渐进恢复**（生产启动路径 `schedule_restore`/`restore_step`）：
/// 读清单 → `schedule_restore` 登记首帧后恢复 → 逐帧 `simulate_next_frame` 泵到
/// `session_restoring` 复位 → 再投一帧触发 md 的 `apply_text_restore` 滚动 → 收敛。
/// 顺序镜像 `tests/session.rs::progressive_restore_matches_sync_final_state` 的既有姿势。
fn run_session_restore(
    app: &Entity<EditorApp>,
    cx: &mut VisualTestContext,
    dir: &std::path::Path,
) {
    let man = session_store::read_manifest(dir).expect("清单应可读回");
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.schedule_restore(window, cx, dir.to_path_buf(), man);
        });
    });
    let mut restoring = true;
    for _ in 0..24 {
        cx.run_until_parked();
        let _ = cx.update(|window, cx| window.simulate_next_frame(cx));
        cx.run_until_parked();
        restoring = cx.update(|_w, cx| app.read(cx).model.borrow().session_restoring);
        if !restoring {
            break;
        }
    }
    assert!(!restoring, "渐进恢复应完成（`session_restoring` 复位）");
    let _ = cx.update(|window, cx| window.simulate_next_frame(cx));
    settle_frames(cx, 6);
}

/// T1（R1 复现）：会话恢复主流程「读到中途 → 关 app → 重开 app」——开 md → 滚到行 50 →
/// 经保存点（`remove_tab` 触发 `save_reading_progress`）落盘 → 构造会话清单（`session_restore`
/// 默认开）→ 渐进恢复（`schedule_restore`/`restore_step`）→ 推进帧 → 断言恢复套进度到行 50。
/// 修复前 FAIL：`apply_reading_progress` 的 `session_restoring` 守卫把恢复期全部拦死 →
/// 恢复出的 tab 停在顶部（`viewport_top_line()==0`），即用户看到的「阅读进度没作用」。
#[gpui::test]
async fn session_restore_applies_saved_reading_progress(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app_zero_tabs(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = rp_md_fixture("sess-restore");
    let idx = open_file_settled(&app, cx, &p);
    let ed = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[idx].payload.editor.clone()
    });
    // 模拟读到行 50（同 R1：滚后视口顶应为目标行）。
    cx.update(|_, cx| {
        ed.update(cx, |e, _| e.scroll_to_line(50));
    });
    settle_frames(cx, 4);
    assert_eq!(cx.update(|_, cx| ed.read(cx).viewport_top_line()), 50, "滚后视口顶应为目标行");
    // 经保存点落盘（remove_tab → save_reading_progress → 内存 map）→ 确认行 50 已存。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.remove_tab(idx, window, cx));
    });
    let saved = cx.update(|_, cx| app.read(cx).model.borrow().reading_progress.clone());
    assert!(
        saved.values().any(|rp| matches!(rp, shell::app::ReadingProgress::Markdown { top_line } if *top_line == 50)),
        "关 tab 应把行 50 存进 map（供会话恢复套用）"
    );

    // 构造单文件会话清单并渐进恢复（session_restore 默认开）。
    let dir = rp_session_dir("restore-applies-md");
    let m = session_store::SessionManifest {
        version: 1,
        active_tab: 0,
        tabs: vec![session_store::SessionTab::File { path: p.clone() }],
    };
    assert!(session_store::write_manifest(&dir, &m), "清单应写入");
    run_session_restore(&app, cx, &dir);

    // 恢复出的 tab（下标 0，连 remove 后零 tab 经恢复重建）应套进度到行 50。
    let ed2 = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[0].payload.editor.clone()
    });
    assert_eq!(
        cx.update(|_, cx| ed2.read(cx).viewport_top_line()),
        50,
        "会话恢复应把已存档进度套到行 50（T1/R1：恢复期不再拦 apply_reading_progress）"
    );
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_dir_all(&dir);
}

/// T2（R2 复现）：存档 N（非 0）→ 会话恢复 → 推进帧 → 断言语义：
/// (a) 恢复套进度到行 N（R1 同口径，作为恢复是否生效的观察锚）；
/// (b) 恢复生命周期后任一落盘点（此处用 `remove_tab` 触发 `save_reading_progress`，等价
/// 生产「恢复后防抖到点 flush」的落盘点）写入的必须是恢复后的真实值 N、**绝非 0**——
/// 已存档进度不被覆写成 0（R2）。
///
/// headless 注：生产覆写 0 的那一发走 `READING_PROGRESS_DEBOUNCE_MS` 防抖（测试构建把
/// 定时器整体编译掉，不可直接观测）；T2 以「恢复后落盘点写入值」做端到端护栏，R2 的
/// `rp_last` 播种 + `check_reading_progress`「None→Some 首观测不落盘」从源头取消那次
/// 防抖调度，两条路径都保证恢复期/恢复后不写 0。
#[gpui::test]
async fn session_restore_never_clobbers_saved_progress_to_zero(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app_zero_tabs(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = rp_md_fixture("sess-clobber");
    // 直接注入存档 N（等价用户读到行 50 后落盘；省去 T1 已覆盖的「先开→滚→关」步骤）。
    let key = session_store::reading_progress::normalize_key(&p);
    cx.update(|_w, cx| {
        app.update(cx, |app, _cx| {
            app.model
                .borrow_mut()
                .reading_progress
                .insert(key.clone(), shell::app::ReadingProgress::Markdown { top_line: 50 });
        });
    });

    // 会话渐进恢复。
    let dir = rp_session_dir("restore-no-clobber");
    let m = session_store::SessionManifest {
        version: 1,
        active_tab: 0,
        tabs: vec![session_store::SessionTab::File { path: p.clone() }],
    };
    assert!(session_store::write_manifest(&dir, &m), "清单应写入");
    run_session_restore(&app, cx, &dir);

    // (a) 恢复应套进度（R1；同时保证 (b) 的落盘点读到的是恢复后的真实位置而非 0）。
    let ed = cx.update(|_, cx| {
        let a = app.read(cx);
        a.model.borrow().tabs[0].payload.editor.clone()
    });
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).viewport_top_line()),
        50,
        "T2(a)：会话恢复应套进度到行 50"
    );
    // (b) 恢复生命周期内 map 必须仍为 N（恢复期绝不覆写 0）。
    let mid = cx.update(|_, cx| app.read(cx).model.borrow().reading_progress.clone());
    assert_eq!(
        mid.get(&key),
        Some(&shell::app::ReadingProgress::Markdown { top_line: 50 }),
        "T2/R2：恢复过程中 map 不得被覆写成 0"
    );
    // (c) 恢复后首个落盘点（关 tab）写入的必须是恢复值 N，绝非 0（R2 端到端护栏）。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.remove_tab(0, window, cx));
    });
    let fin = cx.update(|_, cx| app.read(cx).model.borrow().reading_progress.clone());
    assert_eq!(
        fin.get(&key),
        Some(&shell::app::ReadingProgress::Markdown { top_line: 50 }),
        "T2/R2：恢复后落盘点写入恢复值 N、绝不覆写 0"
    );
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_dir_all(&dir);
}
