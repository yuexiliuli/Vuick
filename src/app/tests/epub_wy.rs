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
    let (md_wy, editable, has_text, toc_len, routed, backend_on) = cx.update(|_w, cx| {
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
            a.epub_preview_backend_wy(),
        )
    });
    assert!(backend_on, "默认新宿主开关开启");
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

/// P4-1：回退开关（测试专用实例字段强制 MarkdownPreview）下 EPUB **不建** WyEditor
/// 宿主、预览路由回 MarkdownPreview；EPUB 仍自动开启预览（现有阅读体验不破坏）。
/// 用实例字段而非全局/env：并行测试互不干扰。
#[gpui::test]
async fn epub_fallback_backend_keeps_markdown_preview(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    cx.update(|_w, cx| {
        app.update(cx, |a, _| a.epub_backend_force_legacy = true);
    });
    let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/xy.epub");
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_file(epub.to_str().unwrap(), window, cx)
        });
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }
    let (md_wy_none, routed_none, preview_on, md_content_has) = cx.update(|_w, cx| {
        let a = app.read(cx);
        let t = &a.tabs[a.active_tab];
        (
            t.md_wy.is_none(),
            a.md_wy_preview().is_none(),
            t.preview_on,
            t.content.contains("共产党宣言"),
        )
    });
    cx.update(|_w, cx| {
        app.update(cx, |a, _| a.epub_backend_force_legacy = false);
    });
    assert!(md_wy_none, "回退下不应建 WyEditor 宿主");
    assert!(routed_none, "回退下预览路由应回 MarkdownPreview");
    assert!(preview_on, "回退下 EPUB 仍自动开启预览");
    assert!(md_content_has, "EPUB 正文仍由 content 持有（回退可读）");
}
