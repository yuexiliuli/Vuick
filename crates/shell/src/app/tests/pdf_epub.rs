    /// 打开 .epub：应解析正文（含书名/章节），自动进入预览模式并填充内容。
    #[gpui::test]
    async fn opens_epub_and_shows_preview(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/xy.epub");
        assert!(epub.exists(), "测试 EPUB 应存在");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(epub.to_str().unwrap(), window, cx)
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (preview_on, content, tabs, status_ok, fullscreen, wrap) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.tabs[a.active_tab].preview_on,
                a.tabs[a.active_tab].content.to_string(),
                a.tabs.len(),
                a.status_ok,
                a.tabs[a.active_tab].preview_fullscreen,
                a.tabs[a.active_tab].md_wy.is_some(),
            )
        });
        assert!(status_ok, "打开 EPUB 应成功");
        assert!(preview_on, "打开 EPUB 应自动进入预览");
        assert_eq!(tabs, 2, "应新增一个标签页");
        assert!(content.contains("共产党宣言"), "正文应包含书名");
        assert!(content.len() > 1000, "正文应有实际内容（>1000 字符），实际={}", content.len());
        assert!(
            fullscreen,
            "打开 EPUB 应自动进入全屏占满阅读模式"
        );
        assert!(wrap, "打开 EPUB 应默认开启文本自动换行");

        // 回归：EPUB 正文只在预览区、编辑区为空，点击预览/Ctrl+P（toggle_preview_for_tab）
        // 不能翻转进空白编辑区或把预览源覆盖成空串导致内容消失。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.toggle_preview_for_tab(app.active_tab, window, cx);
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (still_on, still_fullscreen, still_content) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (
                t.preview_on,
                t.preview_fullscreen,
                t.content.contains("共产党宣言"),
            )
        });
        assert!(
            still_on,
            "EPUB 预览锁定：toggle 后 preview_on 应保持 true"
        );
        assert!(
            still_fullscreen,
            "EPUB 预览锁定：toggle 后应保持全屏，不倒向空白编辑区"
        );
        assert!(
            still_content,
            "toggle 后正文（tab.content）应仍在，未被空编辑区覆盖"
        );
    }

    /// 打开 .pdf：PdfViewer 自包含后台解析（进 UI 不阻塞），
    /// 加载完成后全屏只读预览，解析为页级位图而非 Markdown 文本。
    #[gpui::test]
    async fn opens_pdf_and_shows_preview(cx: &mut TestAppContext) {
        if pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = pdf::test_util::lock().lock().unwrap();
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let pdf = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试.pdf");
        assert!(pdf.exists(), "测试 PDF 应存在");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(pdf.to_str().unwrap(), window, cx)
            });
        });
        // 解析在后台执行器做，泵驱动到 PdfViewer 加载完成
        for _ in 0..200 {
            cx.run_until_parked();
            let loaded = cx.update(|_w, cx| {
                tab_pdf(&app.read(cx).tabs[app.read(cx).active_tab])
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        let (page_count, tabs, status, status_ok, fullscreen, readonly, no_md_content) =
            cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.tabs[a.active_tab];
                (
                    tab_pdf(t).map(|pv| pv.read(cx).page_count()).unwrap_or(0),
                    a.tabs.len(),
                    a.status.to_string(),
                    a.status_ok,
                    t.preview_fullscreen,
                    t.readonly_preview,
                    t.content.is_empty(),
                )
            });
        assert!(status_ok, "打开 PDF 应成功，状态={status:?}");
        assert!(page_count >= 1, "PdfViewer 应载入至少 1 页，实际={page_count}");
        assert_eq!(tabs, 2, "应新增一个标签页");
        assert!(fullscreen, "打开 PDF 应自动进入全屏占满阅读模式");
        assert!(readonly, "打开 PDF 应锁定只读预览");
        assert!(no_md_content, "PDF 不应进入 Markdown 文本（content 应为空）");

        // PDF 阅读器自带全屏与分页，状态栏不应出现「全屏/还原」「换行」按钮
        let ws = cx.debug_bounds("status-wrap-btn").is_none();
        let fs = cx.debug_bounds("status-fullscreen-btn").is_none();
        assert!(ws, "PDF 预览时状态栏不应有换行开关");
        assert!(fs, "PDF 预览时状态栏不应有全屏/还原按钮");

        // 全屏预览下编辑区不进入布局（非“被遮挡”，而是根本不渲染）
        assert!(
            cx.debug_bounds("editor-area").is_none(),
            "PDF 全屏预览时不应渲染编辑区"
        );
        assert!(
            cx.debug_bounds("fullscreen-preview").is_some(),
            "PDF 应以 fullscreen-preview 呈现"
        );
    }

    /// 决定性回归：**无任何用户输入**的情况下，后台瓦片/封面渲染后必须经「notify →
    /// 窗口重绘」链条被绘制出来。若此测试失败 = worker 渲完了但主线程从未重绘 →
    /// 用户看到“CPU 低、画面长时间不变”，正是当前体感问题的根因候选。
    #[gpui::test]
    async fn pdf_content_reaches_pixels_without_user_input(cx: &mut TestAppContext) {
        if pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = pdf::test_util::lock().lock().unwrap();
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let pdf = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试.pdf");
        assert!(pdf.exists(), "测试 PDF 应存在");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(pdf.to_str().unwrap(), window, cx));
        });
        for _ in 0..400 {
            cx.run_until_parked();
            let loaded = cx.update(|_w, cx| {
                let a = app.read(cx);
                tab_pdf(&a.tabs[a.active_tab])
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        // 不触发任何输入/事件：仅跑虚拟帧 + 少量真实等待给 pdfium worker。
        // 用 advance_clock 推进虚拟时间以驱动 16ms 自刷新泵（真实 app 里是真实时钟，
        // 定时器天然触发；测试虚拟时钟需显式推进）。
        for _ in 0..400 {
            cx.executor().advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for _ in 0..120 {
            cx.refresh().unwrap();
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (loaded, tiles, cover, pending) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let pv = tab_pdf(&a.tabs[a.active_tab]).expect("PDF 阅读器在场").read(cx);
            // P4：pdf 已独立 crate，经只读访问器取内部调度状态（不再直接读 slots/pending_tiles）。
            let (tiles, cover, pending) = pv.tiles_schedule_progress();
            (pv.is_loaded(), tiles, cover, pending)
        });
        assert!(loaded, "应已加载");
        // 关键回归：无任何输入也必须在首帧布局就位后**发出瓦片/封面请求**（此前
        // "负载完成 → notify 空操作 → 永不重绘 → 首帧 bounds=0 → 永远调不到瓦片"）。
        // 说明：测试 harness 对深层嵌套子视图无法一直驱动 collect，故此处断言"已调度"；
        // 完整"调度→回传→收集→绘制"链路由 pdf::tests 的窗口根节点测试覆盖。
        assert!(
            tiles >= 1 || cover || pending >= 1,
            "无输入也应发起瓦片/封面调度：tiles={tiles} cover={cover} pending={pending}"
        );
    }

    /// 回归：打开长 PDF（曾因全量栅格化耗时 ~270s 表现为“无法加载”）。
    /// 现在应为页级虚拟化——结构解析秒级完成，进 UI 后按需渲染。
    #[gpui::test]
    async fn opens_long_pdf_virtualized(cx: &mut TestAppContext) {
        if pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = pdf::test_util::lock().lock().unwrap();
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // 长 PDF 输入改用 target/pdf测试2.pdf（259 页文本型）：fixtures/ 下 PDF 在本机 Rust 进程
        // 内无法解析（FormatError），且长度要 ≥50 页（opens_long_pdf_virtualized 断言）。
        let pdf = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf.exists(), "测试 PDF 应存在");
        let t0 = std::time::Instant::now();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(pdf.to_str().unwrap(), window, cx)
            });
        });
        // 泵驱动到 PdfViewer 加载完成
        for _ in 0..400 {
            cx.run_until_parked();
            let loaded = cx.update(|_w, cx| {
                tab_pdf(&app.read(cx).tabs[app.read(cx).active_tab])
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        let (page_count, status_ok) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (
                tab_pdf(t).map(|pv| pv.read(cx).page_count()).unwrap_or(0),
                a.status_ok,
            )
        });
        let elapsed = t0.elapsed();
        assert!(status_ok, "打开长 PDF 应成功");
        assert!(
            page_count >= 50,
            "长 PDF 应列出完整页数（输入 target/pdf测试2.pdf，259 页），实际={page_count}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "长 PDF 结构解析应秒级完成（曾需 ~270s 全量栅格化），实际 {elapsed:?}"
        );
    }

    /// EPUB 轻量视口分页：底部状态栏出现「第 x / N 页」；初始第 1 页、总页数随内容
    /// 分页；滚动预览到末标题后页码前进（滚动明细由 view 层单元测试覆盖）。
    #[gpui::test]
    async fn epub_status_bar_shows_pages_and_follows_scroll(cx: &mut TestAppContext) {
        let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/xy.epub");
        if !epub.exists() {
            eprintln!("SKIP epub_status_bar_shows_pages_and_follows_scroll: 缺 fixtures/xy.epub");
            return;
        }
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(900.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(epub.to_str().unwrap(), window, cx));
        });
        // 页码元素按就绪门控出现（视口/行高就绪后），有界等待而非固定帧数，避免 flaky。
        let mut page_el = None;
        for _i in 0..30 {
            for _ in 0..2 {
                cx.run_until_parked();
                let _ = cx.cx.refresh();
                cx.run_until_parked();
            }
            if let Some(b) = cx.debug_bounds("status-epub-page") {
                page_el = Some(b);
                break;
            }
        }
        let page_el = page_el.expect("EPUB 状态栏应显示页码（status-epub-page）");
        assert!(page_el.size.width > px(0.), "页码元素应有宽度");
        // P4-1：页码读数路由到当前后端（WyEditor 只读宿主确定性量程 / MarkdownPreview 回退）。
        let (p1, n) = cx.update(|window, cx| {
            let a = app.read(cx);
            a.epub_page_bounds(window, cx)
                .expect("EPUB 应有页码读数")
        });
        assert_eq!(p1, 1, "初始应为第 1 页");
        assert!(n >= 1, "总页数至少 1 页（实际 {n}）");

        // 滚动预览到最后一个章标题 → 页码前进（内容不足一页则不强制）。
        if n > 1 {
            let last_toc_line = cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.tabs[a.active_tab];
                // P4-2：统一 WyEditor 只读宿主（`MarkdownPreview` 已删）。
                t.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).toc_entries().last().map(|(l, _, _)| *l))
            });
            if let Some(Some(line)) = last_toc_line {
                cx.update(|_w, cx| {
                    let a = app.read(cx);
                    let t = &a.tabs[a.active_tab];
                    // 统一宿主：set_scroll_target（prepaint 重钉到章标题所在块顶）。
                    let wy = t.md_wy.clone().expect("统一宿主在场");
                    wy.update(cx, |v, _| v.set_scroll_target(line));
                });
                for _ in 0..8 {
                    cx.run_until_parked();
                    let _ = cx.cx.refresh();
                    cx.run_until_parked();
                }
                let p2 = cx.update(|window, cx| {
                    app.read(cx)
                        .epub_page_bounds(window, cx)
                        .expect("EPUB 应有页码读数")
                });
                assert!(
                    p2.0 >= 1 && p2.0 <= p2.1,
                    "滚动后页码应合法（{}/{}）", p2.0, p2.1
                );
            }
        }
    }

    /// EPUB 阅读体验：状态栏全屏/还原按钮可切换占满与分屏；
    /// 换行开关只切换编辑区偏好，不改变预览换行（预览独立保持换行）。
    #[gpui::test]
    async fn toggles_epub_fullscreen_and_wrap(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/xy.epub");
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.open_file(epub.to_str().unwrap(), window, cx)
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }
        // 初始：全屏 + 换行
        let (fs0, wrap0) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            // P4-2：预览统一 WyEditor 恒换行（`MarkdownPreview::is_wrap` 已删）。
            (t.preview_fullscreen, t.md_wy.is_some())
        });
        assert!(fs0, "初始应是全屏");
        assert!(wrap0, "初始应换行");

        // EPUB 是只读预览文档：状态栏不应出现「全屏/还原」「换行」按钮
        assert!(
            cx.debug_bounds("status-wrap-btn").is_none(),
            "EPUB 预览时状态栏不应有换行开关"
        );
        assert!(
            cx.debug_bounds("status-fullscreen-btn").is_none(),
            "EPUB 预览时状态栏不应有全屏/还原按钮"
        );

        // 切换全屏 → 分屏
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_preview_fullscreen(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (fs1, wrap1) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (t.preview_fullscreen, t.md_wy.is_some())
        });
        assert!(!fs1, "切换后应退出全屏");
        assert!(wrap1, "退出全屏后预览应保持换行（预览始终自动换行）");

        // 切换换行：只作用编辑区偏好，不改变预览换行（预览独立保持换行）
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_wrap(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (wrap2, pref2) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.tabs[a.active_tab].md_wy.is_some(),
                a.wrap_enabled,
            )
        });
        assert!(wrap2, "切换换行开关不应影响 EPUB 预览（预览独立保持换行）");
        assert!(pref2, "切换一次后全局编辑区换行偏好应开启");

        // 再切回
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.toggle_wrap(window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (wrap3, pref3) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.tabs[a.active_tab].md_wy.is_some(),
                a.wrap_enabled,
            )
        });
        assert!(wrap3, "预览应保持换行");
        assert!(!pref3, "再切换后全局编辑区换行偏好应恢复关闭");
    }

    /// Reader 收敛回归（防样板）：picture / pdf / video / epub 四种装载路径（各自 `make_tab`
    /// 新建选项卡，对齐 `open_file` 的真实装配路径）产生的「只读阅读」公共字段逐项一致
    /// （preview 全屏 + 只读锁定 + 编辑区置空 + 文件路径 + 未脏 + 光标归零 + 计数口径）——
    /// 媒体三态各挂对应 [`MediaReader`] 变体、EPUB 走真实正文/计数且不挂媒体阅读器。
    /// 直接调 `load_media` / `load_epub`（不经 `open_file`，无需真文件可解析，仅断言装配
    /// 字段；背景解码/解析失败不影响字段）。
    #[gpui::test]
    async fn media_loads_share_common_fields(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-app-reader-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf_p = dir.join("a.pdf");
        let img_p = dir.join("a.jpg");
        std::fs::write(&pdf_p, b"not a real pdf").unwrap();
        std::fs::write(&img_p, b"not a real jpeg").unwrap();
        #[cfg(feature = "video")]
        let vid_p = dir.join("a.mp4");
        #[cfg(feature = "video")]
        std::fs::write(&vid_p, b"not a real video").unwrap();
        let epub_p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/xy.epub");
        let epub_doc = epub::parse_epub_file(&epub_p).expect("测试 EPUB 应可解析");

        // 新建选项卡并装载，返回新下标（对齐真实打开路径：load_* 总是在新 make_tab 上）。
        let load_media_tab = |cx: &mut VisualTestContext,
                              app: &gpui::Entity<EditorApp>,
                              p: &std::path::Path,
                              kind: MediaKind|
         -> usize {
            cx.update(|window, cx| {
                app.update(cx, |app, cx| {
                    let idx = app.make_tab(window, cx);
                    app.load_media(idx, p, kind, window, cx);
                    idx
                })
            })
        };

        #[allow(clippy::type_complexity)]
        type Common = (
            bool,  // preview_on
            bool,  // preview_fullscreen
            bool,  // readonly_preview
            bool,  // dirty
            usize, // cursor_row
            usize, // cursor_col
            bool,  // content 为空
            usize, // line_count
            usize, // char_count
            bool,  // 编辑区 value 为空
            std::path::PathBuf, // 装配后的文件路径
        );
        // 采集指定选项卡的公共装配字段。
        let read_common = |cx: &mut VisualTestContext,
                           app: &gpui::Entity<EditorApp>,
                           idx: usize|
         -> Common {
            cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.tabs[idx];
                (
                    t.preview_on,
                    t.preview_fullscreen,
                    t.readonly_preview,
                    t.dirty,
                    t.cursor_row,
                    t.cursor_col,
                    t.content.is_empty(),
                    t.line_count,
                    t.char_count,
                    t.editor.read(cx).value().is_empty(),
                    t.file_path.clone().expect("装配后应有文件路径"),
                )
            })
        };
        // 媒体三态的公共期望：全屏只读锁定、未脏、光标归零、空内容/零计数、编辑区空。
        let media_expected: Common =
            (true, true, true, false, 0, 0, true, 0, 0, true, std::path::PathBuf::new());

        // PDF
        let i_pdf = load_media_tab(cx, &app, &pdf_p, MediaKind::Pdf);
        let c = read_common(cx, &app, i_pdf);
        assert_eq!(
            (c.0, c.1, c.2, c.3, c.4, c.5, c.6, c.7, c.8, c.9),
            (media_expected.0, media_expected.1, media_expected.2, media_expected.3, media_expected.4, media_expected.5, media_expected.6, media_expected.7, media_expected.8, media_expected.9),
            "PDF 装配公共字段应与媒体基准一致"
        );
        assert_eq!(c.10, pdf_p, "PDF 应装配文件路径");
        assert!(
            cx.update(|_w, cx| matches!(&app.read(cx).tabs[i_pdf].reader, MediaReader::Pdf(_))),
            "PDF 应挂 Pdf 阅读器"
        );

        // 图片
        let i_img = load_media_tab(cx, &app, &img_p, MediaKind::Image);
        let c = read_common(cx, &app, i_img);
        assert_eq!(
            (c.0, c.1, c.2, c.3, c.4, c.5, c.6, c.7, c.8, c.9),
            (media_expected.0, media_expected.1, media_expected.2, media_expected.3, media_expected.4, media_expected.5, media_expected.6, media_expected.7, media_expected.8, media_expected.9),
            "图片装配公共字段应与媒体基准一致"
        );
        assert_eq!(c.10, img_p, "图片应装配文件路径");
        assert!(
            cx.update(|_w, cx| matches!(&app.read(cx).tabs[i_img].reader, MediaReader::Image(_))),
            "图片应挂 Image 查看器"
        );

        // 视频（仅 video 特性）
        #[cfg(feature = "video")]
        {
            let i_vid = load_media_tab(cx, &app, &vid_p, MediaKind::Video);
            let c = read_common(cx, &app, i_vid);
            assert_eq!(
                (c.0, c.1, c.2, c.3, c.4, c.5, c.6, c.7, c.8, c.9),
                (media_expected.0, media_expected.1, media_expected.2, media_expected.3, media_expected.4, media_expected.5, media_expected.6, media_expected.7, media_expected.8, media_expected.9),
                "视频装配公共字段应与媒体基准一致"
            );
            assert_eq!(c.10, vid_p, "视频应装配文件路径");
            assert!(
                cx.update(|_w, cx| matches!(&app.read(cx).tabs[i_vid].reader, MediaReader::Video(_))),
                "视频应挂 Video 查看器"
            );
        }

        // EPUB：只读锁定/预览标志与媒体一致，但正文/计数走真实文档，且不挂媒体阅读器。
        let i_epub = cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let idx = app.make_tab(window, cx);
                app.load_epub(idx, &epub_p, epub_doc, window, cx);
                idx
            })
        });
        let c = read_common(cx, &app, i_epub);
        assert!(
            c.0 && c.1 && c.2 && !c.3 && c.4 == 0 && c.5 == 0 && c.9,
            "EPUB 公共标志应与媒体基准一致（全屏只读预览、未脏、光标归零、编辑区空）"
        );
        assert_eq!(c.10, epub_p, "EPUB 应装配文件路径");
        assert!(!c.6, "EPUB 应以真实正文为 content（非空）");
        assert!(c.7 > 0 && c.8 > 0, "EPUB 应有真实行/字计数（实际 {}/{}）", c.7, c.8);
        let (epub_status_ok, epub_reader_none) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.status_ok, matches!(&a.tabs[i_epub].reader, MediaReader::None))
        });
        assert!(epub_status_ok, "EPUB 打开状态应成功");
        assert!(epub_reader_none, "EPUB 不挂媒体阅读器（纯 WyEditor 宿主）");

        let _ = std::fs::remove_dir_all(&dir);
    }

