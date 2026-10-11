// P4-1：EPUB 迁 WyEditor 只读宿主的 app 层回归（include! 进 `app::tests`）。
// 依赖 `fixtures/xy.epub`（仓内已有）。回退开关断言用测试专用原子开关（免 env 竞态）。

/// P4-1：打开 xy.epub → 自动建 WyEditor 只读宿主（`md_wy`），预览路由判定走新宿主；
/// 只读、内容含书名、有多个章目标题；MarkdownPreview 仍喂（注记/引文/页码回退源）。
#[gpui::test]
async fn epub_opens_in_wy_readonly_host(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/xy.epub");
    assert!(epub.exists(), "测试 EPUB 应存在");
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_file(epub.to_str().unwrap(), window, cx)
        });
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }
    let (md_wy, editable, has_text, toc_len, routed) = cx.update(|_w, cx| {
        let a = app.read(cx);
        let t = &a.tabs[a.active_tab];
        (
            t.md_wy.clone(),
            t.md_wy
                .as_ref()
                .map(|w| w.read(cx).is_editable())
                .unwrap_or(true),
            t.md_wy
                .as_ref()
                .map(|w| w.read(cx).text().contains("共产党宣言"))
                .unwrap_or(false),
            t.md_wy
                .as_ref()
                .map(|w| w.read(cx).toc_entries().len())
                .unwrap_or(0),
            a.md_wy_preview().is_some(),
        )
    });
    assert!(md_wy.is_some(), "EPUB 应建 WyEditor 只读宿主");
    assert!(!editable, "EPUB 宿主应只读（禁编辑）");
    assert!(has_text, "宿主内容应含书名");
    assert!(toc_len >= 2, "应有多个章目标题（实际={toc_len}）");
    assert!(routed, "EPUB 预览应路由到 WyEditor 宿主");

    // 真实渲染进 WYSIWYG 宿主：build_frame 读 Doc（EPUB 源）+ 内存图（epubimg://）+ EPUB
    // 语义（%%ALIGN%%/标题居中）→ 应产出内存图行与对齐行（P4-1 需求 1/2 的渲染回归）。
    let (img_rows, aligned_rows, mem_images) = cx.update(|window, cx| {
        let a = app.read(cx);
        let wy = a.tabs[a.active_tab].md_wy.clone().unwrap();
        let width = window.viewport_size().width.max(px(1.));
        let frame = crate::markdown::wysiwyg::build_frame(
            wy.read(cx),
            window,
            width,
            (0., 999_999.),
        );
        let images = frame.rows.iter().filter(|r| r.image.is_some()).count();
        let aligned = frame.rows.iter().filter(|r| !r.align_offsets.is_empty()).count();
        let mem = frame
            .rows
            .iter()
            .filter(|r| r.image.as_ref().map(|i| i.src.as_ref().starts_with("epubimg://")).unwrap_or(false))
            .count();
        (images, aligned, mem)
    });
    assert!(img_rows >= 1, "WYSIWYG EPUB 宿主应含图片行（实际={img_rows}）");
    assert!(mem_images >= 1, "WYSIWYG EPUB 宿主图片行 src 应为 epubimg:// 内存图（实际={mem_images}）");
    assert!(aligned_rows >= 1, "WYSIWYG EPUB 宿主应含对齐行（%%ALIGN%%/标题居中，实际={aligned_rows}）");
}

/// P4-1：章节翻转（上一章/下一章）调用 WyEditor 的「章节间滚动」接口：
/// 下一章 → 滚动目标设为相邻章标题源码行；上一章 → 回到首章。
#[gpui::test]
async fn epub_flip_chapter_moves_between_headings(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/xy.epub");
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_file(epub.to_str().unwrap(), window, cx)
        });
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }
    // 下一章：应把滚动目标设到「当前章之后的章标题」源码行。
    let (moved, target_is_heading) = cx.update(|_w, cx| {
        let moved = app.update(cx, |a, cx| a.epub_flip_chapter(1, cx));
        let idx = app.read(cx).active_tab;
        let wy = app.read(cx).tabs[idx].md_wy.clone();
        let is_heading = wy
            .as_ref()
            .map(|w| {
                let tgt = w.read(cx).scroll_target_line();
                let entries = w.read(cx).toc_entries();
                tgt.map(|l| entries.iter().any(|&(e, _, _)| e == l))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        (moved, is_heading)
    });
    assert!(moved, "多章且未到末章应能翻下一章");
    assert!(target_is_heading, "滚动目标应为某个章标题源码行");
    // 让重钉落位（prepaint 逐帧把目标块精确顶写回宿主）：有界等待「顶行已离开首块」。
    let idx0 = cx.update(|_w, cx| app.read(cx).active_tab);
    let initial_top = cx.update(|_w, cx| {
        app.read(cx).tabs[idx0]
            .md_wy
            .as_ref()
            .and_then(|w| w.read(cx).top_source_line())
            .unwrap_or(0)
    });
    let mut landed = false;
    for _ in 0..60 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let top = cx.update(|_w, cx| {
            app.read(cx).tabs[idx0]
                .md_wy
                .as_ref()
                .and_then(|w| w.read(cx).top_source_line())
                .unwrap_or(0)
        });
        if top != initial_top {
            landed = true;
            break;
        }
    }
    assert!(landed, "翻章应驱动 WyEditor 滚动落位（顶行应离开首块）");
    // 上一章回到首章（已落位 → 当前在第二章 → 可翻回）。
    let moved_back = cx.update(|_w, cx| app.update(cx, |a, cx| a.epub_flip_chapter(-1, cx)));
    assert!(moved_back, "上一章应可翻回");
}

// P4-2：注记悬浮/引文跳转在 EPUB 全流程（app 层）回归见 `app/tests/pdf_epub.rs`
// 与 `markdown/wysiwyg/element/tests/epub.rs`（`wy_note_*`）。回退开关测试已随
// `MarkdownPreview` 后端删除（M5-B5 P4-2 终态）。

/// P4-2：注记/引文在**统一正确宿主**（只读 WyEditor，MD 与 EPUB 同一管线）的 app 层
/// 回归——打开含脚注 `[^1]`/`[!1]` 的 md，走真实 `open_file → ensure_md_wy` 喂源后：
/// 引文索引 refs/defs/details 以**源码行**记（与 EPUB 宿主同机制）；`navigate_marker`
/// 双向跳转设置对端滚动目标 + 加亮。换源后索引随 content_gen 失效。
#[gpui::test]
async fn md_wy_notes_citation_index_and_navigation_app_level(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = std::env::temp_dir().join("gpui-wy-notes-app.md");
    std::fs::write(&p, "正文引用[^1]\n\n[!1] 注记正文\n").unwrap();
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }
    let (cit_ref_line, cit_def_line, note_txt, routed) = cx.update(|_w, cx| {
        let a = app.read(cx);
        let t = &a.tabs[a.active_tab];
        let wy = t.md_wy.clone().expect("MD 打开应建统一 WyEditor 预览");
        let cit = wy.read(cx).citation_index();
        (
            cit.refs.get(&1).map(|(l, _)| *l),
            cit.defs.get(&1).map(|(l, _)| *l),
            wy.read(cx).note_text_for(1),
            a.md_wy_preview().is_some(),
        )
    });
    assert!(routed, "MD 预览应路由到 WyEditor（统一宿主）");
    assert_eq!(cit_ref_line, Some(0), "文内 `[^1]` 应收录（行 0）");
    assert_eq!(cit_def_line, Some(2), "定义 `[!1]` 应收录（行 2）");
    assert_eq!(note_txt.as_deref(), Some("注记正文"), "缺 EPUB 注记表时正文回退 details");
    // 引用 → 定义 双向跳转（同一 update 内读，scroll_target 下一帧被 prepaint 消费）。
    let (stl, mhl) = cx.update(|_w, cx| {
        let a = app.read(cx);
        let wy = a.tabs[a.active_tab].md_wy.clone().unwrap();
        let hit = crate::markdown::wysiwyg::NoteMarkerHit {
            src_line: 0,
            n: 1,
            is_def: false,
            bounds: gpui::Bounds::new(gpui::point(px(0.), px(0.)), gpui::size(px(10.), px(10.))),
        };
        wy.update(cx, |w, cx| {
            w.navigate_marker(&hit, cx);
            (w.scroll_target_line(), w.marker_hl_test())
        })
    });
    assert_eq!(stl, Some(2), "引用 → 定义：滚动目标应为定义行");
    assert_eq!(mhl, Some((2, 1, true)), "定义标记应加亮 (2, n=1, is_def)");
    let _ = std::fs::remove_file(&p);
}
