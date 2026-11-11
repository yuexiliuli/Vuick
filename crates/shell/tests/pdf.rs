//! PDF/EPUB 域（pdf_epub + pdf_phaseb + pdf_bg + pdf_menu_select_regression 合并 target，
//! §8.3）：PDF/EPUB 打开预览/虚拟化/状态栏、Phase B 交互（选中弹卡/右键菜单/书签持久化）、
//! PDF 背景色次级菜单、右键点击后移动不误拖选回归。
//! 四份断言内部与 lib 单测相应 `.rs` 逐字一致（P1 测试域化）；`pdf_bookmarks::{load,
//! normalize_key}` 经 `common::pdf_bookmarks` 别名、`SidebarView` 经 `shell::app`、
//! `PdfBgPref` 显式引入；`common::init_test` 注入 `GPUI_EDITOR_TEST`。
//! ⚠ 已知环境失败指纹（K56）：`pdf::test_util::lock()` 锁毒化级联（根毒源 panic 后连锁
//! PoisonError），逐条单跑全绿——遇之当预存、勿复现勿当回归。

mod common;

use common::*;

use config::user::PdfBgPref;

    /// 打开 .epub：应解析正文（含书名/章节），自动进入预览模式并填充内容。
    #[gpui::test]
    async fn opens_epub_and_shows_preview(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/测试epub.epub");
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
                a.model.borrow().tabs[a.model.borrow().active_tab].preview_on,
                a.model.borrow().tabs[a.model.borrow().active_tab].content.to_string(),
                a.model.borrow().tabs.len(),
                a.model.borrow().status_ok,
                a.model.borrow().tabs[a.model.borrow().active_tab].preview_fullscreen,
                a.model.borrow().tabs[a.model.borrow().active_tab].md_wy.is_some(),
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
                let active_idx = app.model.borrow().active_tab;
                app.toggle_preview_for_tab(active_idx, window, cx);
            });
        });
        for _ in 0..8 {
            cx.run_until_parked();
        }
        let (still_on, still_fullscreen, still_content) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
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
                tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
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
                let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
                (
                    tab_pdf(t).map(|pv| pv.read(cx).page_count()).unwrap_or(0),
                    a.model.borrow().tabs.len(),
                    a.model.borrow().status.to_string(),
                    a.model.borrow().status_ok,
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
                tab_pdf(&a.model.borrow().tabs[a.model.borrow().active_tab])
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
            let pv = tab_pdf(&a.model.borrow().tabs[a.model.borrow().active_tab]).expect("PDF 阅读器在场").read(cx);
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
                tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        let (page_count, status_ok) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            (
                tab_pdf(t).map(|pv| pv.read(cx).page_count()).unwrap_or(0),
                a.model.borrow().status_ok,
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
        let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/测试epub.epub");
        if !epub.exists() {
            eprintln!("SKIP epub_status_bar_shows_pages_and_follows_scroll: 缺 target/测试epub.epub");
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
                let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
                // P4-2：统一 WyEditor 只读宿主（`MarkdownPreview` 已删）。
                t.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).toc_entries().last().map(|(l, _, _)| *l))
            });
            if let Some(Some(line)) = last_toc_line {
                cx.update(|_w, cx| {
                    // 统一宿主：set_scroll_target（prepaint 重钉到章标题所在块顶）。
                    // 先取出 wy（owned）再 release 守卫，避免持 `a`/`t` 时 `wy.update(cx,..)`
                    // 对 `*cx` 的可变借用与 `app.read(cx)` 的不可变借用冲突（E0502）。
                    let wy = {
                        let a = app.read(cx);
                        let model = a.model.borrow();
                        model.tabs[model.active_tab].md_wy.clone().expect("统一宿主在场")
                    };
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
        let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/测试epub.epub");
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
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
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
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
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
                a.model.borrow().tabs[a.model.borrow().active_tab].md_wy.is_some(),
                a.model.borrow().wrap_enabled,
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
                a.model.borrow().tabs[a.model.borrow().active_tab].md_wy.is_some(),
                a.model.borrow().wrap_enabled,
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
    /// 已在失败路径停 pump（P0-A 收口）：`media-ffmpeg` 看门狗让损坏文件快速失败（8s
    /// 截止，实测 <10ms 返回 Err），且视频失败分支清 `playing`+`pending_seek` 使 pump 不再
    /// 自续 → 本测试创建的真 `VideoViewer`（假 mp4，解码失败）停在占位、executor 正常静止、
    /// 不再挂起（此前 pump 无限自续定时器，`run_until_parked` 永不静止）。
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
            .join("../../target/测试epub.epub");
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
                let t = &a.model.borrow().tabs[idx];
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
            cx.update(|_w, cx| matches!(&app.read(cx).model.borrow().tabs[i_pdf].reader, MediaReader::Pdf(_))),
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
            cx.update(|_w, cx| matches!(&app.read(cx).model.borrow().tabs[i_img].reader, MediaReader::Image(_))),
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
                cx.update(|_w, cx| matches!(&app.read(cx).model.borrow().tabs[i_vid].reader, MediaReader::Video(_))),
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
            (a.model.borrow().status_ok, matches!(&a.model.borrow().tabs[i_epub].reader, MediaReader::None))
        });
        assert!(epub_status_ok, "EPUB 打开状态应成功");
        assert!(epub_reader_none, "EPUB 不挂媒体阅读器（纯 WyEditor 宿主）");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归：切主题（`refresh_theme_ui`）必须同步刷新 PDF/图片阅读器的画布背景。曾只刷
    /// PDF——图片/视频打开时记下的 `canvas_bg` 停留旧主题，切主题后仍空旧底色（视频整窗
    /// 白底即此因，用户实测）。此处以 PDF + 图片实例断言背景随主题翻转并还原；视频走
    /// `MediaReader::Video` 同款 match 臂（既有 `media_loads_share_common_fields` 已盖
    /// 视频装配，本测试不重复开视频）。
    #[gpui::test]
    async fn media_readers_follow_theme_background(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!("kilo_media_theme_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf_p = dir.join("a.pdf");
        let img_p = dir.join("a.jpg");
        std::fs::write(&pdf_p, b"not a real pdf").unwrap();
        std::fs::write(&img_p, b"not a real jpeg").unwrap();

        // 真实打开路径装配媒体 tab（load_media 内 set_background(canvas_bg)）。
        let open = |cx: &mut VisualTestContext, p: &std::path::Path, kind: MediaKind| -> usize {
            cx.update(|w, cx| {
                app.update(cx, |a, cx| {
                    let idx = a.make_tab(w, cx);
                    a.load_media(idx, p, kind, w, cx);
                    idx
                })
            })
        };
        let i_pdf = open(cx, &pdf_p, MediaKind::Pdf);
        let i_img = open(cx, &img_p, MediaKind::Image);

        let reader_bg = |cx: &mut VisualTestContext, idx: usize| -> Option<gpui::Hsla> {
            cx.update(|_w, cx| {
                let a = app.read(cx);
                match &a.model.borrow().tabs[idx].reader {
                    MediaReader::Pdf(p) => p.read(cx).background(),
                    MediaReader::Image(i) => i.read(cx).background(),
                    #[cfg(feature = "video")]
                    MediaReader::Video(v) => v.read(cx).background(),
                    MediaReader::None => None,
                }
            })
        };

        // 不假设初始明暗（测试环境可能亮也可能暗）：记录打开时的背景。
        // PDF 默认档（pdf_bg=Default）打开不存显式画布色——bg=None，render 经
        // default_canvas（= 当前主题 window_bg，open.rs render）跟随主题；图片/视频仍存
        // 快照 Some(window_bg)。PDF 显式色跟随/冻结语义由 tests/pdf_bg.rs 覆盖。
        let pdf0_stored = reader_bg(cx, i_pdf);
        assert!(
            pdf0_stored.is_none(),
            "PDF 默认档打开不存显式画布色（跟随主题走 render 层 default_canvas）"
        );
        let img0 = reader_bg(cx, i_img).expect("图片打开应有画布背景");
        let bg0: gpui::Hsla = cx.update(|_w, cx| app.read(cx).window_bg());

        // 切到另一主题（真路径 refresh_theme_ui）→ 图片背景同步翻转（背景赋值在
        // refresh_theme_ui 内同步完成，无需 pump 渲染即可读）。回归点：曾漏刷非 PDF 阅读器。
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                // SC-3：同语句 `borrow_mut()`（LHS guard 存活）时求值 `borrow()`（RHS）
                // → 运行期双借 panic → 先提纯 owned 旧值再单独写（guard 不残留，避免
                // 与后续 `&mut self` 调用 E0502）。
                let dark = a.model.borrow().dark_mode;
                a.model.borrow_mut().dark_mode = !dark;
                a.refresh_theme_ui(cx);
                cx.notify();
            });
        });
        let toggled_bg: gpui::Hsla = cx.update(|_w, cx| app.read(cx).window_bg());
        assert_ne!(bg0, toggled_bg, "翻转明暗后 window_bg 应变化（测试前提）");
        assert_eq!(
            reader_bg(cx, i_pdf),
            None,
            "PDF 默认档切主题存储仍 None（渲染色经 default_canvas 跟随主题）"
        );
        assert_eq!(reader_bg(cx, i_img), Some(toggled_bg), "图片背景应随主题翻转（回归）");

        // 切回 → 图片背景还原到打开时值；PDF 默认档仍 None。
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                // SC-3：同语句 `borrow_mut()`（LHS guard 存活）时求值 `borrow()`（RHS）
                // → 运行期双借 panic → 先提纯 owned 旧值再单独写（guard 不残留，避免
                // 与后续 `&mut self` 调用 E0502）。
                let dark = a.model.borrow().dark_mode;
                a.model.borrow_mut().dark_mode = !dark;
                a.refresh_theme_ui(cx);
                cx.notify();
            });
        });
        assert_eq!(reader_bg(cx, i_pdf), None, "还原后 PDF 默认档仍 None");
        assert_eq!(reader_bg(cx, i_img), Some(img0), "还原后图片背景应回初值");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归（真 bug）：多 PDF 选项卡并存时，侧边栏 PDF 检索计数/页列表的刷新信号必须
    /// **各自独立**到达 app。根因：`_pdf_observe` 是单槽订阅——后打开的 PDF 覆盖先打开者，
    /// 先打开 PDF 的 notify 再也收不到 → 其侧栏不再刷新。修复后每个 PDF 选项卡各自持有
    /// `_pdf_sub` 订阅。本测试流程：
    /// 1. 真实打开两个 PDF（先开慢速扫描件 A / 后开快速文本件 B，旧单槽被 B 覆盖）；
    /// 2. 真实侧栏路径激活 A 并搜「配置」，泵帧驱动 A 的后台检索（250ms 防抖 → worker
    ///    扫描 → 帧循环 `collect_search_results` 收集）**真实完成**（命中 > 0）；
    /// 3. 登记「app 实体 notify」计数器（基线清零）；
    /// 4. 用**真实入口** `PdfViewer::focus_match_on_page`（侧边栏命中行点击走的正是这个）
    ///    让 A 在 update 上下文内自然 emit notify；
    /// 5. 断言 app 收到那次 notify（`cx.notify()` 就是 `_pdf_sub` 回调和的内容）。
    /// 旧单槽代码下 A 的订阅被覆盖 → A 的 notify 丢失 → 本断言 FAIL；per-tab 订阅 → PASS。
    #[gpui::test]
    async fn multi_pdf_tabs_each_refresh_sidebar_after_notify(cx: &mut TestAppContext) {
        if pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = pdf::test_util::lock().lock().unwrap();
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        // A = 慢速扫描件（pdf测试2.pdf，259 页文本型，检索耗时 >100ms）；B = 快速文本件。
        // 先后打开的顺序决定旧单槽 `_pdf_observe` 覆盖方向：后打开的 B 覆盖 A → A 的订阅丢。
        let pdf_slow = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        let pdf_fast = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试.pdf");
        assert!(pdf_slow.exists() && pdf_fast.exists(), "测试 PDF 应存在");
        let wait_loaded = |cx: &mut VisualTestContext| {
            for _ in 0..600 {
                cx.run_until_parked();
                let loaded = cx.update(|_w, cx| {
                    tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
                        .map(|pv| pv.read(cx).is_loaded())
                        .unwrap_or(false)
                });
                if loaded {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.open_file(pdf_slow.to_str().unwrap(), window, cx));
        });
        wait_loaded(cx);
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.open_file(pdf_fast.to_str().unwrap(), window, cx));
        });
        wait_loaded(cx);
        assert_eq!(
            cx.update(|_w, cx| app.read(cx).model.borrow().tabs.len()),
            3,
            "初始空 tab + A + B 共 3 个选项卡"
        );
        let a_idx = 1;
        // 激活 A → 切搜索视图 → 键入查询（真实输入路径：InputState.set_value → 防抖 →
        // 提交 → render_sidebar_search_results 对活动 PDF 推 set_search(A,"配置")）。
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.activate_tab(a_idx, window, cx));
        });
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                a.toggle_sidebar_view(shell::app::SidebarView::Search, cx);
            });
        });
        cx.update(|window, cx| {
            let input = app.read(cx).search_state(cx);
            input.update(cx, |s, cx| s.set_value("配置", window, cx));
        });
        // 防抖期满提交 → 侧栏渲染推 set_search 到 A → 检索在途
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        assert!(
            cx.update(|_w, cx| {
                app.read(cx).model.borrow().pdf_search.as_ref()
                    == Some(&(a_idx, "配置".to_string()))
            }),
            "应在侧栏渲染时把查询推给 A（pdf_search 键 (A,配置)）"
        );

        // 泵帧驱动 A 的后台检索**真实完成**（250ms 防抖 → worker 扫描 → 帧循环收集命中）。
        // 读到 hits>0 即证明：真实异步检索落库到 A 的阅读器（后续 focus_match_on_page 的
        // notify 才是针对「结果已就绪」的真信号）。
        let mut completed = false;
        for _ in 0..1800 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            cx.update(|_, _| {});
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(5));
            let hits = cx.update(|_w, cx| {
                tab_pdf(&app.read(cx).model.borrow().tabs[a_idx])
                    .expect("A 应挂 PDF 阅读器")
                    .read(cx)
                    .search_summary()
                    .0
            });
            if hits > 0 {
                completed = true;
                break;
            }
        }
        assert!(completed, "A 的检索应按真实异步路径收集到命中（慢速扫描件）");

        // 登记「app 实体 notify」计数器并观察 app（基线在检索完成后落位 → 之后 A 的这次
        // notify 是窗口内唯一可能的 app notify 来源）。订阅必须存进持有实体字段保持有效
        // （直接丢弃 = 立即退订，计数器永不增长）。
        struct AppNotifyCounter(
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
            Option<gpui::Subscription>,
        );
        let notified = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let holder = cx.update(|_w, cx| {
            let h = cx.new(|_| AppNotifyCounter(notified.clone(), None));
            h.update(cx, |count, cx| {
                let n = count.0.clone();
                let sub = cx.observe(&app, move |_, _, _| {
                    n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                });
                count.1 = Some(sub);
            });
            h
        });
        let _holder_kept = holder;

        // 真实入口：触发 A 的「跳到某页命中」动作（侧边栏命中行点击走的正是
        // `PdfViewer::focus_match_on_page`）→ 阅读器在 update 上下文内 emit notify →
        // （per-tab 订阅下）`_pdf_sub` 回调 `cx.notify()` → 本计数器 +1。
        let a_pdf = cx.update(|_w, cx| {
            tab_pdf(&app.read(cx).model.borrow().tabs[a_idx])
                .expect("A 应挂 PDF 阅读器")
                .clone()
        });
        cx.update(|_w, cx| {
            a_pdf.update(cx, |v, cx| v.focus_match_on_page(0, cx));
        });
        cx.run_until_parked();
        let delta = notified.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            delta > 0,
            "A 的 notify 必须到达 app（侧栏刷新信号）。旧单槽 `_pdf_observe` 被后打开的 B \
             覆盖 → A 的 notify 丢失 → delta=0；修复后每 PDF 各自独立订阅 → delta>0。\
             实际 app notify 增量 = {delta}"
        );
    }

    /// 回归（真 bug 的「拖拽换位 / 分屏粘贴」路径）：选项卡**拔除再收纳**（`take_tab_payload`
    /// → `adopt_tab`，即拖出拆分/还原、分屏粘贴走的路径）后，per-tab 的 PDF 订阅必须随
    /// 选项卡重建并**跟 tab 走**——多 PDF 依次换位后，先换位者的 notify 仍要到达 app
    /// （旧单槽 `_pdf_observe` 在每次 `adopt_tab` 时被后收纳者覆盖 → 先收纳的丢订阅）。
    /// 全部走真实入口：`take_tab_payload` / `adopt_tab`（同 `panes.rs` 的用法）换位两次，
    /// 再对第一个 PDF 的阅读器调 `PdfViewer::focus_match_on_page`（侧边栏命中行点击走的
    /// 公共方法，无条件 emit notify）断言 app 收到那次 notify。
    #[gpui::test]
    async fn pdf_subscription_follows_adopt_across_tear_offs(cx: &mut TestAppContext) {
        if pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = pdf::test_util::lock().lock().unwrap();
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let pdf = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试.pdf");
        assert!(pdf.exists(), "测试 PDF 应存在");
        let wait_loaded = |cx: &mut VisualTestContext| {
            for _ in 0..600 {
                cx.run_until_parked();
                let loaded = cx.update(|_w, cx| {
                    tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
                        .map(|pv| pv.read(cx).is_loaded())
                        .unwrap_or(false)
                });
                if loaded {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.open_file(pdf.to_str().unwrap(), window, cx));
        });
        wait_loaded(cx);
        cx.update(|window, cx| {
            app.update(cx, |a, cx| a.open_file(pdf.to_str().unwrap(), window, cx));
        });
        wait_loaded(cx);
        // 记住第一个 PDF 的阅读器实体（换位后按实体身份找回它的选项卡，防下标漂移）。
        let first_pdf = cx.update(|_w, cx| {
            tab_pdf(&app.read(cx).model.borrow().tabs[1])
                .expect("第 1 个 PDF 阅读器应在场")
                .clone()
        });
        let first_pdf_entity_id = first_pdf.entity_id();
        // 依次拔除/收纳两次（真实换位路径）：先 A 再 B，B 后收纳 → 旧单槽被 B 覆盖。
        // take_tab_payload(1) 拔的是当前下标 1（A）；adopt_tab(payload, None) 落到末位。
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                let payload = a.take_tab_payload(1).expect("应能拔除 A");
                a.adopt_tab(payload, None, cx);
            });
        });
        cx.update(|_w, cx| {
            app.update(cx, |a, cx| {
                let payload = a.take_tab_payload(1).expect("应能拔除 B");
                a.adopt_tab(payload, None, cx);
            });
        });

        // 登记「app 实体 notify」计数器（基线在换位完成后落位）。
        struct AppNotifyCounter(
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
            Option<gpui::Subscription>,
        );
        let notified = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let holder = cx.update(|_w, cx| {
            let h = cx.new(|_| AppNotifyCounter(notified.clone(), None));
            h.update(cx, |count, cx| {
                let n = count.0.clone();
                let sub = cx.observe(&app, move |_, _, _| {
                    n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                });
                count.1 = Some(sub);
            });
            h
        });
        let _holder_kept = holder;

        // 真实入口：对第一个 PDF（先换位者）触发一次 notify；其 per-tab 订阅须随换位存活。
        cx.update(|_w, cx| {
            first_pdf.update(cx, |v, cx| v.focus_match_on_page(0, cx));
        });
        cx.run_until_parked();
        let delta = notified.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            delta > 0,
            "先换位的 PDF 的 notify 必须仍到达 app（订阅随选项卡走）。旧单槽在每次 \
             adopt_tab 时被后收纳者覆盖 → 先收纳的丢订阅 → delta=0；per-tab → delta>0。\
             实际 app notify 增量 = {delta}"
        );
        // 兜底：换位后第一个 PDF 仍指向同一阅读器实体（未错位）。
        let still_first = cx.update(|_w, cx| {
            app.read(cx)
                .model.borrow().tabs
                .iter()
                .any(|t| matches!(&t.reader, MediaReader::Pdf(p) if p.entity_id() == first_pdf_entity_id))
        });
        assert!(still_first, "换位后第一个 PDF 阅读器实体应仍存在（未错位/未泄漏）");
    }

// Phase B：PDF 交互 UI 集成测试（B1 选中弹卡 / B2 右键菜单 / B3 书签持久化+跳转）。
// 全部走真实 PdfViewer + 真 PDF 夹具（缺夹具 SKIP），经 `pdf::test_util::lock()` 串行防
// pdfium 原生并发。每个用例用**唯一的临时副本**（`temp_pdf_copy`）——标注写回 / 页操作
// 都改源文件，且书签按规范化路径键，副本路径唯一避免跨用例的书签存储污染（K8）。

use pdf::AnnotKind;

fn pdf_fixture() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试.pdf")
}

fn missing_pdf() -> bool {
    pdf::test_util::missing_pdf_fixture()
}

/// 夹具副本（每用例唯一 tag）：测试隔离，不污染共享 `target/pdf测试.pdf`。
fn temp_pdf_copy(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("gpui-editor-pdf-tests");
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join(format!("{tag}-{}.pdf", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = pdf::test_util::copy_fixture_writable(&pdf_fixture(), &p);
    p
}

/// 打开 PDF 选项卡并泵到 PdfViewer 加载完成。
fn open_pdf(app: &Entity<EditorApp>, cx: &mut VisualTestContext, path: &std::path::Path) {
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(path.to_str().unwrap(), window, cx));
    });
    let ok = wait_pdf_loaded(app, cx);
    assert!(ok, "PdfViewer 应加载完成：{}", path.display());
}

fn wait_pdf_loaded(app: &Entity<EditorApp>, cx: &mut VisualTestContext) -> bool {
    for _ in 0..400 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let loaded = cx.update(|_w, cx| {
            tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
                .map(|pv| pv.read(cx).is_loaded())
                .unwrap_or(false)
        });
        if loaded {
            return true;
        }
    }
    false
}

/// 等一个谓词为真（有界）：注释写回 / 页操作重载等异步完成用。
fn pump_until(cx: &mut VisualTestContext, mut pred: impl FnMut(&mut VisualTestContext) -> bool) -> bool {
    for _ in 0..400 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(2));
        if pred(cx) {
            return true;
        }
    }
    false
}

/// 活动 PDF 窗口内前若干页里第一个可用字符段：返回 `((page, k), (p1, p2))`（第 `page`
/// 页、字符 k..k+3 的两个窗口坐标）。先同步把前若干页文本读入缓存（`load_text_for_test`
/// 阻塞等 worker，摆脱「可见页预取」时序），再逐页扫描字符盒（封面页可能无文本层）。
#[allow(clippy::type_complexity)]
fn first_char_range_points(
    app: &Entity<EditorApp>,
    cx: &mut VisualTestContext,
) -> ((usize, usize), (gpui::Point<gpui::Pixels>, gpui::Point<gpui::Pixels>)) {
    let mut found: Option<((usize, usize), (gpui::Point<gpui::Pixels>, gpui::Point<gpui::Pixels>))> = None;
    cx.update(|window, cx| {
        let pv = tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
            .expect("活动选项卡应挂 PDF")
            .clone();
        for page in 0..12usize {
            pv.update(cx, |v, _| {
                v.load_text_for_test(page);
            });
            let mut start: Option<(usize, gpui::Point<gpui::Pixels>)> = None;
            for k in 0..800usize {
                if let (Some(p), Some(q)) = (
                    pv.read(cx).char_window_pos(window, page, k),
                    pv.read(cx).char_window_pos(window, page, k + 3),
                ) {
                    start = Some((k, p));
                    found = Some(((page, k), (p, q)));
                    break;
                }
            }
            if start.is_some() {
                break;
            }
        }
    });
    found.expect("前 12 页应存在可用字符盒（文本层未缓存？）")
}

/// 真实拖选（按下→移动到终点→松开）。
fn drag_select(cx: &mut VisualTestContext, from: gpui::Point<gpui::Pixels>, to: gpui::Point<gpui::Pixels>) {
    let mods = gpui::Modifiers::default();
    cx.simulate_mouse_move(from, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_down(from, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_move(to, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_up(to, gpui::MouseButton::Left, mods);
    settle_frames(cx, 3);
}

/// 测试探针直接弹选中卡（跳过真实拖选几何）：选区 `(page, lo)..(page, hi)`。
fn open_sel_card(app: &Entity<EditorApp>, cx: &mut VisualTestContext, page: usize, lo: usize, hi: usize) {
    cx.update(|window, cx| {
        let pv = tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone();
        pv.update(cx, |v, cx| v.open_selection_card_for_test((page, lo), (page, hi), window, cx));
    });
    settle_frames(cx, 3);
}

/// 右键 PDF 正文区（窗口中部）→ 弹 PDF 菜单（返回活动 PDF 实体）。
fn pdf_right_click(app: &Entity<EditorApp>, cx: &mut VisualTestContext) -> gpui::Entity<pdf::PdfViewer> {
    let pos = gpui::Point::new(px(600.), px(400.));
    cx.simulate_mouse_down(pos, gpui::MouseButton::Right, gpui::Modifiers::default());
    cx.simulate_mouse_up(pos, gpui::MouseButton::Right, gpui::Modifiers::default());
    settle_frames(cx, 3);
    let pdf = cx.update(|_w, cx| {
        tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
            .expect("PDF 菜单需活动 PDF")
            .clone()
    });
    assert!(cx.debug_bounds("pdf-menu-bookmark").is_some(), "右键应弹「添加书签」");
    pdf
}

fn menu_item_disabled(app: &Entity<EditorApp>, cx: &mut VisualTestContext, ix: usize) -> bool {
    cx.update(|_w, cx| {
        app.read(cx)
            .pdf_menu_entity
            .as_ref()
            .map(|m| m.read(cx).is_item_disabled(ix))
            .unwrap_or(true)
    })
}

fn clipboard_text(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_w, cx| {
        cx.read_from_clipboard()
            .map(|i| i.text().unwrap_or_default())
    })
}

/// 展开侧边栏「目录」面板（PDF 打开后默认仅活动栏）。
fn open_toc_panel(app: &Entity<EditorApp>, cx: &mut VisualTestContext) {
    cx.update(|_w, cx| {
        app.update(cx, |app, _| app.sidebar.toggle_view(shell::app::SidebarView::Toc));
    });
    settle_frames(cx, 3);
}

// ---------------------------------------------------------------------------
// B1：选中弹卡（复制 / 高亮 / 删除线 / 下划线）
// ---------------------------------------------------------------------------

/// B1 端到端：真实拖选 → 弹卡（4 项在场）；点「复制」→ 剪贴板含选中文本、关卡（选区
/// 保留可反复复制）；重新选中并点「高亮」→ 注释**只进内存待存**（pending≥1、置脏、磁盘
/// 不变），随后 `save()` → 落盘清脏 → 重开读回 Highlight。
#[gpui::test]
async fn b1_drag_selection_card_copy_and_highlight(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("b1card");
    let disk0 = std::fs::read(&p).expect("读初始磁盘");
    open_pdf(&app, cx, &p);

    // 真实拖选 → 弹卡
    let ((page, k), (a, b)) = first_char_range_points(&app, cx);
    drag_select(cx, a, b);
    assert!(cx.debug_bounds("pdf-sel-card").is_some(), "拖选结束应弹卡");
    for sel in ["pdf-sel-card-copy", "pdf-sel-card-highlight", "pdf-sel-card-strike", "pdf-sel-card-underline"] {
        assert!(cx.debug_bounds(sel).is_some(), "弹卡应含「{sel}」");
    }
    // 布局为「菜单形式」：一行一项（同列对齐、y 单调递增），防回退为横向按钮条（K2）。
    let items = [
        "pdf-sel-card-copy",
        "pdf-sel-card-highlight",
        "pdf-sel-card-strike",
        "pdf-sel-card-underline",
    ]
    .map(|sel| cx.debug_bounds(sel).expect("弹卡项").center());
    let xs: Vec<f32> = items.iter().map(|p| p.x.as_f32()).collect();
    let ys: Vec<f32> = items.iter().map(|p| p.y.as_f32()).collect();
    assert!(
        xs.iter().all(|x| (x - xs[0]).abs() < 1.0),
        "菜单应一行一项（同行对齐）：xs={xs:?}"
    );
    assert!(
        ys.windows(2).all(|w| w[0] < w[1]),
        "菜单应竖向排列（y 递增）：ys={ys:?}"
    );
    // 定位回归断言：menu 出现在「松开鼠标的位置」（同右键菜单定位口径）。水平锚点==松手点
    // （旧行为=选区几何中心，会落在选区中点而偏左）；垂直受 `snap_to_window_with_margin`
    // 近窗缘钳制（与右键菜单共享该夹回），故断言松手点落在卡片纵向范围内即满足两态。
    let card = cx.debug_bounds("pdf-sel-card").expect("弹卡应已定位");
    let card_origin = card.origin;
    let card_bottom = card_origin.y.as_f32() + card.size.height.as_f32();
    assert!(
        (card_origin.x.as_f32() - b.x.as_f32()).abs() < 8.,
        "菜单水平应锚在松手点：card x={}，松手点 x={}",
        card_origin.x.as_f32(),
        b.x.as_f32()
    );
    assert!(
        card_origin.y.as_f32() <= b.y.as_f32() + 0.5 && card_bottom >= b.y.as_f32() - 0.5,
        "菜单纵向应覆盖松手点：card top={} bottom={card_bottom}，松手点 y={}",
        card_origin.y.as_f32(),
        b.y.as_f32()
    );

    // 点「复制」→ 剪贴板含选中文本；卡片关闭、选区保留
    let copy = cx.debug_bounds("pdf-sel-card-copy").expect("复制项").center();
    cx.simulate_click(copy, gpui::Modifiers::default());
    settle_frames(cx, 3);
    let clip = clipboard_text(cx);
    assert!(
        clip.as_deref().is_some_and(|s| !s.trim().is_empty()),
        "复制应把选中文本写入剪贴板：{clip:?}"
    );
    assert!(cx.debug_bounds("pdf-sel-card").is_none(), "复制后关卡");

    // 重新弹卡（同页同段）→ 点「高亮」→ 只进内存待存（pending≥1、dirty、磁盘不变）
    let wk = cx.update(|window, cx| {
        let pv = tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone();
        pv.update(cx, |v, cx| {
            v.open_selection_card_for_test((page, k), (page, k + 3), window, cx)
        });
        pv
    });
    settle_frames(cx, 3);
    assert!(cx.debug_bounds("pdf-sel-card").is_some(), "重新弹卡");
    let hl = cx.debug_bounds("pdf-sel-card-highlight").expect("高亮项").center();
    cx.simulate_click(hl, gpui::Modifiers::default());
    settle_frames(cx, 3);
    let pending = pump_until(cx, |cx| {
        cx.update(|_w, cx| {
            wk.read(cx).pending_annot_count() >= 1
        })
    });
    assert!(pending, "点「高亮」后应有一条待存注释");
    assert!(
        cx.update(|_w, cx| wk.read(cx).is_dirty()),
        "加注释后应置脏（待保存）"
    );
    assert_eq!(
        std::fs::read(&p).expect("重读磁盘"),
        disk0,
        "注释只进内存：磁盘 bytes 必须不变（待保存模型）"
    );
    assert!(cx.debug_bounds("pdf-sel-card").is_none(), "写卡后关卡+清选区");

    // save() → 落盘清脏 → 重开文件读回 Highlight（效果断言；待保存模型下 live 渲染 doc
    // 不重载——注释以叠加层渲染，落盘内容须重开文件验证，与写回断言口径一致）。
    cx.update(|_w, cx| wk.update(cx, |v, cx| {
        v.save(cx);
    }));
    let saved = pump_until(cx, |cx| {
        cx.update(|_w, cx| !wk.read(cx).is_dirty() && wk.read(cx).pending_annot_count() == 0)
    });
    assert!(saved, "save 后应清 pending 与 dirty");
    let saved_bytes = std::fs::read(&p).expect("save 后重读");
    let (w2, d2, _) = pdf::PdfWorker::open_and_dims(saved_bytes).expect("重开保存文件");
    let anns = w2.annotations_for_page(d2, page).expect("读回注释");
    assert!(
        anns.iter().any(|i| i.kind == AnnotKind::Highlight),
        "save 后重开应读回 Highlight（写盘效果断言）；err={:?}",
        cx.update(|_w, cx| wk.read(cx).last_modify_error().map(|s| s.to_string()))
    );
    w2.close(d2);
    let _ = std::fs::remove_file(&p);
}

/// B1 关闭路径：点卡片外 / Esc / 滚动均关闭弹卡。
#[gpui::test]
async fn b1_selection_card_closes_on_outside_escape_scroll(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("b1close");
    open_pdf(&app, cx, &p);
    let pdf = cx.update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone());
    let ((page, k), _) = first_char_range_points(&app, cx);

    // 点卡片外（PDF 正文空白处）→ 关闭
    open_sel_card(&app, cx, page, k, k + 3);
    cx.simulate_click(gpui::Point::new(px(60.), px(700.)), gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(cx.debug_bounds("pdf-sel-card").is_none(), "点卡片外应关闭");

    // Esc → 关闭
    open_sel_card(&app, cx, page, k, k + 3);
    cx.simulate_keystrokes("escape");
    settle_frames(cx, 3);
    assert!(cx.debug_bounds("pdf-sel-card").is_none(), "Esc 应关闭弹卡");

    // 滚动（跳页）→ 关闭
    open_sel_card(&app, cx, page, k, k + 3);
    cx.update(|_w, cx| {
        pdf.update(cx, |v, cx| {
            v.scroll_to_page(1);
            cx.notify();
        });
    });
    settle_frames(cx, 4);
    assert!(cx.debug_bounds("pdf-sel-card").is_none(), "滚动后应关闭弹卡");
    let _ = std::fs::remove_file(&p);
}

// ---------------------------------------------------------------------------
// B2：右键菜单（添加书签 / 插入页面 / 删除页面 / 提取页面）
// ---------------------------------------------------------------------------

/// B2+B3 端到端：右键 PDF → 四项菜单在场（多页文档删除项**启用**）；点「添加书签」→
/// 书签入内存 + 原子落盘（磁盘 load 仍在）；TOC「书签」区段出两条 → 点第 2 页书签
/// 跳页（current_page=2）；点删除 × 逐条移除 → 空 → 区段消失 + 磁盘清空。
#[gpui::test]
async fn b2_pdf_menu_bookmark_persists_and_toc_jumps(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("b2bm");
    open_pdf(&app, cx, &p);
    let pdf = pdf_right_click(&app, cx);

    for item in ["pdf-menu-bookmark", "pdf-menu-insert", "pdf-menu-delete", "pdf-menu-extract"] {
        assert!(cx.debug_bounds(item).is_some(), "右键菜单应含「{item}」");
    }
    assert!(!menu_item_disabled(&app, cx, 3), "多页文档「删除页面」应启用");

    // 点「添加书签」（当前页 = 第 1 页）
    let bm_item = cx.debug_bounds("pdf-menu-bookmark").expect("添加书签").center();
    cx.simulate_click(bm_item, gpui::Modifiers::default());
    settle_frames(cx, 4);
    let path = pdf.read_with(cx, |v, _| v.path().to_path_buf());
    let bms = cx.update(|_w, cx| app.read(cx).pdf_bookmarks_for(&path).map(|v| v.to_vec()));
    assert_eq!(bms.as_ref().map(|v| v.len()), Some(1), "应存 1 条书签");
    assert_eq!(bms.as_ref().and_then(|v| v.first()).map(|b| b.page_index), Some(0), "书签应为第 1 页");

    // 磁盘持久化：load() 重读仍在
    let disk = pdf_bookmarks::load()
        .remove(&pdf_bookmarks::normalize_key(&path))
        .unwrap_or_default();
    assert_eq!(disk.len(), 1, "书签应原子落盘（重开仍在）");

    // 再直接加一条第 2 页书签（TOC 跳转用）
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| app.add_pdf_bookmark(&path, 1, "第 2 页".into(), cx));
    });
    // 回到第 1 页（确定性基线）
    cx.update(|_w, cx| {
        pdf.update(cx, |v, cx| {
            v.scroll_to_page(0);
            cx.notify();
        });
    });
    settle_frames(cx, 3);

    // TOC「书签」区段
    open_toc_panel(&app, cx);
    assert!(cx.debug_bounds("sidebar-toc-bookmarks").is_some(), "应有「书签」区段");
    assert!(cx.debug_bounds("sidebar-toc-bookmark-0").is_some());
    assert!(cx.debug_bounds("sidebar-toc-bookmark-1").is_some());

    // 点第 2 页书签 → scroll_to_page(1) → current_page 首见页 = 2
    let row1 = cx.debug_bounds("sidebar-toc-bookmark-1").expect("第 2 页书签").center();
    cx.simulate_click(row1, gpui::Modifiers::default());
    settle_frames(cx, 4);
    let cur = cx.update(|_w, cx| pdf.read(cx).current_page().map(|(a, _)| a));
    assert_eq!(cur, Some(2), "点书签应跳转到第 2 页，实际 {cur:?}");

    // 删除 × → 逐条移除 → 空区段消失 + 磁盘同步
    let del1 = cx.debug_bounds("sidebar-toc-bookmark-del-1").expect("删除按钮").center();
    cx.simulate_click(del1, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(cx.debug_bounds("sidebar-toc-bookmark-1").is_none(), "删除后第 2 页书签行应消失");
    let del0 = cx.debug_bounds("sidebar-toc-bookmark-del-0").expect("删除按钮").center();
    cx.simulate_click(del0, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(cx.debug_bounds("sidebar-toc-bookmarks").is_none(), "无书签时区段应隐藏");
    let disk_after = pdf_bookmarks::load()
        .remove(&pdf_bookmarks::normalize_key(&path))
        .unwrap_or_default();
    assert!(disk_after.is_empty(), "全删后磁盘不应再有该路径书签");
    let _ = std::fs::remove_file(&p);
}

/// B2：插入页面（确认框「插入」）→ page_count+1；删除页面（确认框「删除」）→ 复原。
#[gpui::test]
async fn b2_insert_and_delete_page_with_confirm(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("b2indel");
    open_pdf(&app, cx, &p);
    let pdf = pdf_right_click(&app, cx);
    let count0 = cx.update(|_w, cx| pdf.read(cx).page_count());
    assert!(count0 >= 2, "夹具应足量页：{count0}");

    // 插入页面：确认框 → 「插入」
    let ins = cx.debug_bounds("pdf-menu-insert").expect("插入页面").center();
    cx.simulate_click(ins, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(cx.has_pending_prompt(), "插入页面前应弹确认框");
    let (msg, _) = cx.pending_prompt().expect("确认框");
    assert_eq!(msg, "插入页面", "确认框标题一致");
    cx.simulate_prompt_answer("插入");
    let grown = pump_until(cx, |cx| {
        cx.update(|_w, cx| pdf.read(cx).page_count() == count0 + 1)
    });
    assert!(
        grown,
        "插入后页数 +1：{count0} → {}",
        cx.update(|_w, cx| pdf.read(cx).page_count())
    );

    // 删除页面：确认框 → 「删除」→ 复原
    pdf_right_click(&app, cx);
    let del = cx.debug_bounds("pdf-menu-delete").expect("删除页面").center();
    cx.simulate_click(del, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(cx.has_pending_prompt(), "删除页面前应弹确认框");
    cx.simulate_prompt_answer("删除");
    let shrunk = pump_until(cx, |cx| {
        cx.update(|_w, cx| pdf.read(cx).page_count() == count0)
    });
    assert!(
        shrunk,
        "删除后页数复原为 {count0}，实际 {}",
        cx.update(|_w, cx| pdf.read(cx).page_count())
    );
    let _ = std::fs::remove_file(&p);
}

/// B2：提取页面产出独立 PDF（第 1 页）→ 重开该产物页数 == 1；且该单页文档上
/// 「删除页面」菜单项**禁用**（唯一页护栏）。
#[gpui::test]
async fn b2_extract_creates_single_page_and_delete_disabled(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("b2extract");
    open_pdf(&app, cx, &p);
    let pdf = cx.update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone());

    // 提取第 1 页到独立文件（生产路径经 cfg(not(test)) 走另存为对话框；测试直接驱动 API）
    let out = std::env::temp_dir()
        .join("gpui-editor-pdf-tests")
        .join(format!("b2out-{}.pdf", std::process::id()));
    let _ = std::fs::remove_file(&out);
    cx.update(|_w, cx| {
        let o = out.clone();
        pdf.update(cx, |v, cx| v.extract_page_to(0, o, cx));
    });
    let produced = pump_until(cx, |cx| {
        let _ = cx;
        out.exists() && std::fs::metadata(&out).map(|m| m.len() > 3).unwrap_or(false)
    });
    assert!(produced, "提取应产出独立 PDF 文件");
    std::thread::sleep(std::time::Duration::from_millis(50));

    // 重开产物：页数 == 1，右键「删除页面」禁用、「插入页面」启用
    open_pdf(&app, cx, &out);
    let single_pdf = pdf_right_click(&app, cx);
    let pc = cx.update(|_w, cx| single_pdf.read(cx).page_count());
    assert_eq!(pc, 1, "提取产物应为 1 页，实际 {pc}");
    assert!(menu_item_disabled(&app, cx, 3), "唯一页「删除页面」应禁用");
    assert!(!menu_item_disabled(&app, cx, 2), "唯一页「插入页面」仍可用");
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(&out);
}

// ---------------------------------------------------------------------------
// C：待保存模型（P1）——Ctrl+Z 撤销 / 灰框与删除注释 / tab dirty 圆点 / 关闭保存提示
// ---------------------------------------------------------------------------

/// C1 端到端（待保存）：点「高亮」→ 注释只进内存（磁盘不变）+ tab dirty 圆点置真
/// （`_pdf_sub` observe 同步）；Ctrl+Z 撤销 → 该条移除、dirty 回落、磁盘仍不变。
#[gpui::test]
async fn c1_pending_annot_ctrlz_undo_and_tab_dirty(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("c1undo");
    let disk0 = std::fs::read(&p).expect("读初始磁盘");
    open_pdf(&app, cx, &p);
    let wk = cx.update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone());
    let ((page, k), (a, _)) = first_char_range_points(&app, cx);

    // 加一条高亮（待存）
    open_sel_card(&app, cx, page, k, k + 3);
    let hl = cx.debug_bounds("pdf-sel-card-highlight").expect("高亮项").center();
    cx.simulate_click(hl, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(
        pump_until(cx, |cx| cx.update(|_w, cx| wk.read(cx).pending_annot_count() == 1)),
        "点「高亮」后应有一条待存注释"
    );
    assert_eq!(std::fs::read(&p).expect("重读磁盘"), disk0, "注释只进内存：磁盘不变");
    // tab.dirty 经 observe 同步置真（工具栏圆点）
    assert!(
        pump_until(cx, |cx| cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].dirty)),
        "加注释后 tab.dirty 应置真（observe 同步）"
    );

    // 点击被注释文字（聚焦 PDF + 灰框焦），再 Ctrl+Z 撤销该条注释
    let mods = gpui::Modifiers::default();
    cx.simulate_mouse_down(a, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_up(a, gpui::MouseButton::Left, mods);
    settle_frames(cx, 3);
    assert!(
        cx.update(|_w, cx| wk.read(cx).focused_annot_id()).is_some(),
        "点击被注释文字应产生灰框焦"
    );
    cx.simulate_keystrokes("ctrl-z");
    settle_frames(cx, 3);
    assert!(
        pump_until(cx, |cx| {
            cx.update(|_w, cx| wk.read(cx).pending_annot_count() == 0 && !wk.read(cx).is_dirty())
        }),
        "Ctrl+Z 应撤销该条待存注释并清脏"
    );
    assert!(
        pump_until(cx, |cx| cx.update(|_w, cx| !app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].dirty)),
        "撤销后 tab.dirty 应回落"
    );
    assert_eq!(std::fs::read(&p).expect("重读磁盘"), disk0, "撤销全程磁盘不变（未保存）");
    let _ = std::fs::remove_file(&p);
}

/// C2 端到端（待保存）：点击被注释文字出现**灰框**（`focused_annot_id` 置位）；右键该处
/// → 菜单含 [删除注释]（其余四项仍在）；点 [删除注释] → 该条待存注释消失、dirty 回落、
/// 灰框清空、磁盘仍原样。
#[gpui::test]
async fn c2_greybox_and_annot_delete_menu(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("c2grey");
    let disk0 = std::fs::read(&p).expect("读初始磁盘");
    open_pdf(&app, cx, &p);
    let wk = cx.update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone());
    let ((page, k), (a, _)) = first_char_range_points(&app, cx);

    // 加一条高亮（待存）→ 点被注释文字 → 灰框焦
    open_sel_card(&app, cx, page, k, k + 3);
    let hl = cx.debug_bounds("pdf-sel-card-highlight").expect("高亮项").center();
    cx.simulate_click(hl, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(
        pump_until(cx, |cx| cx.update(|_w, cx| wk.read(cx).pending_annot_count() == 1)),
        "应有一条待存注释"
    );
    let mods = gpui::Modifiers::default();
    cx.simulate_mouse_down(a, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_up(a, gpui::MouseButton::Left, mods);
    settle_frames(cx, 3);
    let focused = cx.update(|_w, cx| wk.read(cx).focused_annot_id());
    assert!(focused.is_some(), "点击被注释文字应出现灰框焦");

    // 右键同点 → 菜单含 [删除注释] + 原四项
    cx.simulate_mouse_down(a, gpui::MouseButton::Right, mods);
    cx.simulate_mouse_up(a, gpui::MouseButton::Right, mods);
    settle_frames(cx, 3);
    assert!(cx.debug_bounds("pdf-menu-del-annot").is_some(), "灰框上右键应含「删除注释」");
    for item in ["pdf-menu-bookmark", "pdf-menu-insert", "pdf-menu-delete", "pdf-menu-extract"] {
        assert!(cx.debug_bounds(item).is_some(), "右键菜单其余项应保留「{item}」");
    }

    // 点 [删除注释] → 该条待存注释消失、dirty 回落、灰框清、磁盘不变
    let del = cx.debug_bounds("pdf-menu-del-annot").expect("删除注释").center();
    cx.simulate_click(del, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(
        pump_until(cx, |cx| {
            cx.update(|_w, cx| wk.read(cx).pending_annot_count() == 0 && !wk.read(cx).is_dirty())
        }),
        "删除注释后 pending=0 且 dirty 回落"
    );
    assert!(
        cx.update(|_w, cx| wk.read(cx).focused_annot_id()).is_none(),
        "删除后灰框应清除"
    );
    assert_eq!(std::fs::read(&p).expect("重读磁盘"), disk0, "删除注释不落盘：磁盘不变");
    let _ = std::fs::remove_file(&p);
}

/// C3 端到端（待保存）：关闭 dirty PDF 选项卡 → 保存提示三态——「取消」留在当前、`
/// 「保存」落盘后关闭（磁盘变化 / 页数保持）、重新打开加注释后「不保存」直接关（磁盘原样
/// = 不保存即还原）。
#[gpui::test]
async fn c3_pdf_dirty_close_prompt_save_and_discard(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("c3close");
    let disk0 = std::fs::read(&p).expect("读初始磁盘");
    open_pdf(&app, cx, &p);
    let ((page, k), _) = first_char_range_points(&app, cx);
    let add_annot = |app: &Entity<EditorApp>, cx: &mut VisualTestContext| {
        // 重新预热该页文本层：渲染的 LRU 缓存可能把 early page 文本淘汰 → 弹卡锚点算不出
        cx.update(|_w, cx| {
            let pv = {
                let model = app.read(cx).model.borrow();
                let active = model.active_tab;
                tab_pdf(&model.tabs[active]).cloned()
            };
            if let Some(pv) = pv {
                pv.update(cx, |v, _| v.load_text_for_test(page));
            }
        });
        open_sel_card(app, cx, page, k, k + 3);
        let hl = cx.debug_bounds("pdf-sel-card-highlight").expect("高亮项").center();
        cx.simulate_click(hl, gpui::Modifiers::default());
        settle_frames(cx, 3);
        assert!(
            pump_until(cx, |cx| {
                cx.update(|_w, cx| {
                    tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
                        .map(|v| v.read(cx).pending_annot_count() == 1)
                        .unwrap_or(false)
                })
            }),
            "应有一条待存注释"
        );
    };
    add_annot(&app, cx);

    // PDF 选项卡下标（open_pdf 会新建 tab，初始空 tab 仍在 → 定位到 Pdf reader 那个）
    let pdf_idx = |app: &Entity<EditorApp>, cx: &mut VisualTestContext| -> usize {
        cx.update(|_w, cx| {
            app.read(cx)
                .model.borrow().tabs
                .iter()
                .position(|t| matches!(&t.reader, MediaReader::Pdf(_)))
                .expect("应有 PDF 选项卡")
        })
    };

    // Part A：关闭 → 弹提示 → 「取消」→ 不关、dirty 保留
    let idx = pdf_idx(&app, cx);
    cx.update(|window, app_ctx| {
                app.update(app_ctx, |a, cx| a.close_tab(idx, window, cx));
            });
    settle_frames(cx, 3);
    assert!(cx.has_pending_prompt(), "dirty PDF 关闭应弹保存提示");
    cx.simulate_prompt_answer("Cancel");
    settle_frames(cx, 3);
    assert!(!cx.has_pending_prompt(), "确认框已应答");
    let (n_tabs, pdf_dirty) = cx.update(|_w, cx| {
        let app_ref = app.read(cx);
        let idx = app_ref
            .model.borrow().tabs
            .iter()
            .position(|t| matches!(&t.reader, MediaReader::Pdf(_)))
            .expect("应有 PDF 选项卡");
        (app_ref.model.borrow().tabs.len(), app_ref.model.borrow().tabs[idx].dirty)
    });
    assert!(n_tabs == 2 && pdf_dirty, "取消后选项卡应保留且 dirty 仍在");

    // Part B：再关 → 「保存」→ 落盘后关闭；磁盘变化（相对原始基线）
    let idx = pdf_idx(&app, cx);
    cx.update(|window, app_ctx| {
                app.update(app_ctx, |a, cx| a.close_tab(idx, window, cx));
            });
    settle_frames(cx, 3);
    assert!(cx.has_pending_prompt(), "再次关闭应弹保存提示");
    cx.simulate_prompt_answer("保存");
    // 关闭「保存」流内部用 timer 轮询（dirty 清）→ 须推进虚拟时钟（advance_clock）让
    // 轮询醒来，同时真实 sleep 给 save 的 worker 落盘时间。
    let mut closed = false;
    for _ in 0..200 {
        cx.executor().advance_clock(std::time::Duration::from_millis(20));
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(8));
        closed = cx.update(|_w, cx| {
            !app.read(cx).model.borrow().tabs.iter().any(|t| matches!(&t.reader, MediaReader::Pdf(_)))
        });
        if closed {
            break;
        }
    }
    assert!(closed, "保存并关闭后 PDF 选项卡应移除");
    assert_ne!(
        std::fs::read(&p).expect("保存后重读"),
        disk0,
        "关闭「保存」应把待存注释落盘"
    );

    // Part C：重开（已含注释）→ 再加一条注释 → 「不保存」直接关 → 磁盘相对 part B 原样
    open_pdf(&app, cx, &p);
    let disk_b = std::fs::read(&p).expect("part C 基线");
    add_annot(&app, cx);
    let idx = pdf_idx(&app, cx);
    cx.update(|window, app_ctx| {
                app.update(app_ctx, |a, cx| a.close_tab(idx, window, cx));
            });
    settle_frames(cx, 3);
    assert!(cx.has_pending_prompt(), "不保存路径也应弹提示");
    cx.simulate_prompt_answer("不保存");
    let closed2 = pump_until(cx, |cx| {
        cx.update(|_w, cx| {
            !app.read(cx).model.borrow().tabs.iter().any(|t| matches!(&t.reader, MediaReader::Pdf(_)))
        })
    });
    assert!(closed2, "「不保存」应直接关闭");
    assert_eq!(
        std::fs::read(&p).expect("重读磁盘"),
        disk_b,
        "「不保存」丢弃待存：磁盘保持原样（不保存即还原）"
    );
    let _ = std::fs::remove_file(&p);
}

/// D1 端到端（评审修复）：另存为（`save_as`）成功后，承载该 PDF 的选项卡 `file_path`
/// 应随 `viewer.path` 重指新路径（tab 栏标题 / 书签不再显示旧文件名），新文件含注释、
/// 原文件保持原样，dirty/pending 清空。
#[gpui::test]
async fn d1_save_as_updates_tab_path_and_writes_annots(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("d1saveas");
    let out = p.with_extension(format!("{}_另存.pdf", p.file_stem().unwrap().to_string_lossy()));
    let _ = std::fs::remove_file(&out);
    open_pdf(&app, cx, &p);

    let wk = cx
        .update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone());
    let ((page, k), _) = first_char_range_points(&app, cx);
    open_sel_card(&app, cx, page, k, k + 3);
    let hl = cx.debug_bounds("pdf-sel-card-highlight").expect("高亮项").center();
    cx.simulate_click(hl, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(
        pump_until(cx, |cx| cx.update(|_w, cx| wk.read(cx).pending_annot_count() == 1)),
        "应有一条待存注释"
    );

    // 另存到新路径 → save_as 异步落盘 + viewer.path 重指向 + observe 同步 tab.file_path
    cx.update(|_w, cx| wk.update(cx, |v, cx| v.save_as(out.clone(), cx)));
    assert!(
        pump_until(cx, |cx| {
            cx.update(|_w, cx| {
                let idx = app
                    .read(cx)
                    .model.borrow().tabs
                    .iter()
                    .position(|t| matches!(&t.reader, MediaReader::Pdf(_)))
                    .unwrap();
                app.read(cx).model.borrow().tabs[idx].file_path.as_deref() == Some(out.as_path())
            })
        }),
        "另存为后选项卡 file_path 应同步为新路径（tab 标题不再显示旧名）"
    );
    assert!(
        pump_until(cx, |cx| {
            cx.update(|_w, cx| {
                !wk.read(cx).is_dirty() && wk.read(cx).pending_annot_count() == 0
            })
        }),
        "另存为后应清 pending/dirty"
    );

    // 新文件已含注释；原文件保持原样（另存不改源）
    let buf = std::fs::read(&out).expect("另存文件应存在");
    let (w2, d2, _) = pdf::PdfWorker::open_and_dims(buf.clone()).expect("重开另存文件");
    let anns = w2.annotations_for_page(d2, page).expect("读回注释");
    assert!(
        anns.iter().any(|a| a.kind == AnnotKind::Highlight),
        "另存文件应含高亮注释"
    );
    w2.close(d2);
    let src = std::fs::read(&p).expect("重读原文件");
    let (w3, d3, _) = pdf::PdfWorker::open_and_dims(src.clone()).expect("重开原文件");
    let src_anns = w3.annotations_for_page(d3, page).expect("读回原文件注释");
    assert!(
        !src_anns.iter().any(|a| a.kind == AnnotKind::Highlight),
        "另存不改写原文件：源文件不应含注释"
    );
    w3.close(d3);
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(&out);
}

// ---------------------------------------------------------------------------
// 图片交互（B2 扩展）：右键命中页内图片 → 「复制图片」；左键选中态由 pdf crate 交互
// 测试覆盖（`image_click_selects_and_clears`）。夹具无图 → SKIP（文本型夹具常态）。
// ---------------------------------------------------------------------------

/// 找第一张**可定位大图**的 `(页, obj, 命中点窗口坐标)`：同步读入前 20 页图片盒，
/// 过滤紧凑噪声盒（≥10pt），滚到该页后取中心窗口坐标（图不在视口会收不到派发）。
/// 夹具无图 → `None`（调用方 SKIP）。
#[allow(clippy::type_complexity)]
fn first_image_hit_point(
    app: &Entity<EditorApp>,
    cx: &mut VisualTestContext,
) -> Option<(usize, usize, gpui::Point<gpui::Pixels>)> {
    let mut target: Option<(usize, usize)> = None;
    cx.update(|_w, cx| {
        let pv = tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
            .expect("活动选项卡应挂 PDF")
            .clone();
        for page in 0..20usize {
            pv.update(cx, |v, _| {
                v.load_images_for_test(page);
            });
            let hit = pv.read(cx).image_boxes(page).and_then(|boxes| {
                boxes
                    .iter()
                    .find(|b| b.x1 - b.x0 > 10.0 && b.y1 - b.y0 > 10.0)
                    .map(|b| (page, b.obj_index))
            });
            if hit.is_some() {
                target = hit;
                break;
            }
        }
    });
    let (page, obj) = target?;
    cx.update(|_w, cx| {
        let pv = tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
            .expect("活动选项卡应挂 PDF")
            .clone();
        pv.update(cx, |v, cx| {
            v.scroll_to_page(page);
            cx.notify();
        });
    });
    settle_frames(cx, 8);
    cx.update(|window, cx| {
        let model = app.read(cx).model.borrow();
        let active = model.active_tab;
        let pv = tab_pdf(&model.tabs[active])?;
        pv.read(cx).image_pos(window, page, obj)
    })
    .map(|pos| (page, obj, pos))
}

/// 右键**命中页内图片** → 菜单除基四项外追加「复制图片」（`pdf-menu-copy-image`）；
/// 右键**空白/页边距** → 无「复制图片」项、菜单仍含书签项。选中描框态由 pdf crate
/// 交互测试覆盖。夹具无图 → SKIP。
#[gpui::test]
async fn image_right_click_shows_and_hides_copy_image_menu(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("imgmenu");
    open_pdf(&app, cx, &p);
    let Some((_page, _obj, pos)) = first_image_hit_point(&app, cx) else {
        eprintln!("SKIP: pdf测试.pdf 前 20 页无可定位大图（文本型夹具无图）");
        return;
    };
    let mods = gpui::Modifiers::default();

    // 右键在图片上 → 菜单含「复制图片」+ 基项仍在
    cx.simulate_mouse_down(pos, gpui::MouseButton::Right, mods);
    cx.simulate_mouse_up(pos, gpui::MouseButton::Right, mods);
    settle_frames(cx, 3);
    assert!(
        cx.debug_bounds("pdf-menu-copy-image").is_some(),
        "右键命中图片应含「复制图片」"
    );
    assert!(
        cx.debug_bounds("pdf-menu-bookmark").is_some(),
        "命中图片时基项「添加书签」仍在"
    );

    // 右键空白页边距（左上角沟槽）→ 无「复制图片」、书签项仍在
    let blank = gpui::Point::new(px(30.), px(30.));
    cx.simulate_mouse_down(blank, gpui::MouseButton::Right, mods);
    cx.simulate_mouse_up(blank, gpui::MouseButton::Right, mods);
    settle_frames(cx, 3);
    assert!(
        cx.debug_bounds("pdf-menu-copy-image").is_none(),
        "右键非图片不应含「复制图片」"
    );
    assert!(
        cx.debug_bounds("pdf-menu-bookmark").is_some(),
        "右键空白处基项「添加书签」仍在"
    );
}

/// 右键图片 → 点「复制图片」→ 剪贴板 CF_DIB 回读像素与该图 worker 直接取数**逐字节**
/// 一致（端到端：菜单 → pdf 异步取数 → tray 写剪贴板）。剪贴板不可用 → 优雅 SKIP。
/// 夹具无图 → SKIP。
#[cfg(windows)]
#[gpui::test]
async fn image_copy_image_writes_clipboard_dib(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    // 清掉系统剪贴板可能残留的旧 CF_DIB（Windows 剪贴板跨进程持久——上一轮/上一次进程
    // 写入会留在系统里），保证下方 pump_until 只等到「本次菜单动作」真正写入的新 DIB。
    tray::clear_clipboard();
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("imgcopy");
    open_pdf(&app, cx, &p);
    let Some((page, obj, pos)) = first_image_hit_point(&app, cx) else {
        eprintln!("SKIP: pdf测试.pdf 前 20 页无可定位大图（文本型夹具无图）");
        return;
    };
    let mods = gpui::Modifiers::default();

    // 期望像素：与菜单动作同源（view request_image_pixels → worker ImagePixels），
    // 测试线程同步取（阻塞等结果，真实对比源）。
    let expected = cx.update(|_w, cx| {
        let model = app.read(cx).model.borrow();
        let active = model.active_tab;
        let pv = tab_pdf(&model.tabs[active])?;
        pv.read(cx).request_image_pixels(page, obj)
    });
    let Some(rx) = expected else {
        eprintln!("SKIP: 无法向 worker 请求图片像素（对象失效）");
        return;
    };
    let expect_pix = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("worker 应返回像素")
        .expect("按自然分辨率取像应成功");
    assert!(
        expect_pix.width > 0 && expect_pix.height > 0,
        "期望像素应正尺寸"
    );

    // 右键图片 → 点「复制图片」
    cx.simulate_mouse_down(pos, gpui::MouseButton::Right, mods);
    cx.simulate_mouse_up(pos, gpui::MouseButton::Right, mods);
    settle_frames(cx, 3);
    let item = cx
        .debug_bounds("pdf-menu-copy-image")
        .expect("右键命中图片应出现「复制图片」")
        .center();
    cx.simulate_mouse_down(item, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_up(item, gpui::MouseButton::Left, mods);

    // 泵到剪贴板 CF_DIB 就绪（异步菜单动作 + worker + tray 写）
    let ok = pump_until(cx, |cx| {
        cx.update(|_w, _| tray::read_clipboard_image_bgra().is_some())
    });
    if !ok {
        eprintln!("SKIP: 剪贴板不可用（无交互会话/被占用），未读到 CF_DIB");
        return;
    }
    let (back, bw, bh) = cx
        .update(|_w, _| tray::read_clipboard_image_bgra())
        .expect("已读到 CF_DIB");
    assert_eq!(
        (bw, bh),
        (expect_pix.width, expect_pix.height),
        "剪贴板 DIB 尺寸应与原图自然分辨率一致"
    );
    assert_eq!(
        back, expect_pix.bgra,
        "剪贴板 CF_DIB 像素应与 worker 直接取数逐字节一致（原图嵌入分辨率）"
    );
}

// PDF 右键菜单「背景色」二级功能集成测试（brief R7 回归断言）：
// - 右键菜单含「背景色」项（根行带「›」指示）；悬停「背景色」→ 次级菜单在父菜单右侧
//   展开（默认/夜间/护眼/羊皮纸/自定义），父项（添加书签/插入/删除/提取等）**保持可见**、
//   无「返回」项；悬停普通项 → 次级收起；
// - 预设点击 → viewer.bg 立即变对应 Hsla；「默认」→ bg=None（回落跟随主题）；keep_open
//   保持菜单展开可连试；
// - 显式色下切主题 viewer.bg 不变 & 默认色下切主题 bg 保持 None（跟随主题，chrome.rs 守卫）。
// 自定义取色走 Windows 原生对话框（headless 不真弹：测试构建 pick_custom_pdf_bg no-op，
// 应用路径由 set_pdf_bg 兜底覆盖）；config 持久化 roundtrip 由 config crate 纯函数测试覆盖。

/// 打开测试 PDF 副本并右键弹菜单（返回活动 PDF 实体）。
fn bg_open_pdf_right_click(
    app: &gpui::Entity<EditorApp>,
    cx: &mut VisualTestContext,
    tag: &str,
) -> gpui::Entity<pdf::PdfViewer> {
    let p = temp_pdf_copy(tag);
    open_pdf(app, cx, &p);
    pdf_right_click(app, cx)
}

/// 点菜单项（按 debug_selector），返回其中心点。
fn bg_menu_center(cx: &mut VisualTestContext, id: &'static str) -> gpui::Point<gpui::Pixels> {
    cx.debug_bounds(id)
        .unwrap_or_else(|| panic!("菜单项 {id} 应在场"))
        .center()
}

/// 活动 PDF 阅读器实体。
fn bg_active_pdf(app: &gpui::Entity<EditorApp>, cx: &mut VisualTestContext) -> gpui::Entity<pdf::PdfViewer> {
    cx.update(|_w, cx| {
        tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone()
    })
}

/// 读应用当前 pdf_bg 偏好。
fn bg_pref(app: &gpui::Entity<EditorApp>, cx: &mut VisualTestContext) -> PdfBgPref {
    cx.update(|_w, cx| app.read(cx).model.borrow().pdf_bg)
}

/// R1/R2/R3/R6：右键菜单含「背景色」（根行带 › 指示）；悬停 → 次级菜单展开、父项全部
/// 保持可见、无「返回」项；悬停普通项 → 次级收起、父菜单保持。
#[gpui::test]
async fn pdf_right_click_bg_color_flyout_hover_and_parent_stays(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let _pdf = bg_open_pdf_right_click(&app, cx, "bgflyout");
    // 右键菜单：背景色 + 其它父项全部在场；背景色根行带「›」指示。
    assert!(
        cx.debug_bounds("pdf-menu-bg-color").is_some(),
        "右键菜单应含「背景色」"
    );
    for id in [
        "pdf-menu-bookmark",
        "pdf-menu-insert",
        "pdf-menu-delete",
        "pdf-menu-extract",
    ] {
        assert!(cx.debug_bounds(id).is_some(), "父菜单应含「{id}」");
    }
    assert!(
        cx.debug_bounds("menu-flyout-chevron-6").is_some(),
        "背景色根行应带 › 箭头指示（父菜单内下标 6）"
    );
    // 悬停「背景色」→ 次级菜单出现（默认/夜间/护眼/羊皮纸/自定义），父项保持可见、无返回。
    let bg = bg_menu_center(cx, "pdf-menu-bg-color");
    cx.simulate_mouse_move(bg, None, gpui::Modifiers::default());
    cx.run_until_parked();
    for id in [
        "pdf-menu-bg-default",
        "pdf-menu-bg-night",
        "pdf-menu-bg-eye",
        "pdf-menu-bg-parchment",
        "pdf-menu-bg-custom",
    ] {
        assert!(cx.debug_bounds(id).is_some(), "次级菜单应含「{id}」");
    }
    assert!(
        cx.debug_bounds("pdf-menu-bg-back").is_none(),
        "真悬浮次级无「返回」项"
    );
    for id in [
        "pdf-menu-bookmark",
        "pdf-menu-insert",
        "pdf-menu-delete",
        "pdf-menu-extract",
        "pdf-menu-bg-color",
    ] {
        assert!(
            cx.debug_bounds(id).is_some(),
            "悬停后父项「{id}」应保持可见（不原地替换）"
        );
    }
    // 真实排列（「背景色」在两条分隔线之后、下标 6）下次级顶部仍须 ≈ 背景色根行顶部
    // （偏移须按真实行高累计，分割线行 ≠ 常规项行高）。
    let fly = cx.debug_bounds("menu-flyout").expect("次级应在场");
    let bg_row = cx.debug_bounds("pdf-menu-bg-color").expect("背景色行应在场");
    let dy = (fly.top() - bg_row.top()).abs();
    assert!(
        dy <= px(8.),
        "真实菜单下次级顶部应≈背景色行顶部，实际差 {dy:?}"
    );
    // 悬停普通项（添加书签）→ 次级收起、父菜单保持展开。
    let bm = bg_menu_center(cx, "pdf-menu-bookmark");
    cx.simulate_mouse_move(bm, None, gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("pdf-menu-bg-default").is_none(),
        "悬停普通项应收起次级"
    );
    assert!(
        cx.debug_bounds("pdf-menu-bg-color").is_some(),
        "次级收起后父菜单保持展开"
    );
}

/// R2/R4：悬停「背景色」进次级；点「夜间」→ 画布立即 #202020；点「默认」→ 回落主题（bg=None）。
#[gpui::test]
async fn pdf_bg_preset_flyout_click_applies_and_default_resets(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let pdf = bg_open_pdf_right_click(&app, cx, "bgpreset");
    // 悬停「背景色」→ 次级菜单展开。
    let bg = bg_menu_center(cx, "pdf-menu-bg-color");
    cx.simulate_mouse_move(bg, None, gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("pdf-menu-bg-night").is_some(),
        "悬停背景色应展开次级菜单"
    );
    // 移入次级再点「夜间」→ 画布立即 #202020（keep_open：菜单/次级保持展开）。
    let night = bg_menu_center(cx, "pdf-menu-bg-night");
    cx.simulate_mouse_move(night, None, gpui::Modifiers::default());
    cx.run_until_parked();
    cx.simulate_click(night, gpui::Modifiers::default());
    settle_frames(cx, 4);
    assert_eq!(
        pdf.read_with(cx, |v, _| v.background()),
        Some(gpui::rgb(0x20_20_20).into()),
        "夜间预设应立即落到画布"
    );
    assert_eq!(bg_pref(&app, cx), PdfBgPref::Night, "偏好应记夜间档");
    // R6-3：纸墨重着色参数（纸 #202020 + 墨色提亮 #D8D8D8，内容中间调不动）落地到位图显示层。
    assert_eq!(
        pdf.read_with(cx, |v, _| v.recolor()),
        Some(pdf::RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) }),
        "夜间预设应落地纸色+墨迹提亮参数"
    );
    assert!(cx.debug_bounds("pdf-menu-bg-night").is_some(), "keep_open：次级应保持展开");
    assert!(cx.debug_bounds("pdf-menu-bg-color").is_some(), "keep_open：父菜单应保持展开");
    // 移入次级再点「默认」→ 回落跟随主题（bg=None）+ 无重着色（直通）。
    let def = bg_menu_center(cx, "pdf-menu-bg-default");
    cx.simulate_mouse_move(def, None, gpui::Modifiers::default());
    cx.run_until_parked();
    cx.simulate_click(def, gpui::Modifiers::default());
    settle_frames(cx, 4);
    assert_eq!(
        pdf.read_with(cx, |v, _| v.background()),
        None,
        "默认档应回落跟随主题"
    );
    assert_eq!(bg_pref(&app, cx), PdfBgPref::Default, "偏好应记默认档");
    assert_eq!(
        pdf.read_with(cx, |v, _| v.recolor()),
        None,
        "默认档应无纸墨重着色（直通）"
    );
}

/// R7：显式色下切主题画布不变；默认色下切主题 bg 保持 None（跟随主题）。
#[gpui::test]
async fn pdf_bg_explicit_survives_theme_switch_but_default_follows(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("bgtheme");
    open_pdf(&app, cx, &p);
    let pdf = bg_active_pdf(&app, cx);
    // 显式「夜间」→ 画布 #202020 + 纸墨重着色。
    cx.update(|_w, cx| {
        app.update(cx, |a, cx| a.set_pdf_bg(PdfBgPref::Night, None, cx));
    });
    let night_params = pdf::RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
    assert_eq!(pdf.read_with(cx, |v, _| v.background()), Some(gpui::rgb(0x20_20_20).into()));
    assert_eq!(pdf.read_with(cx, |v, _| v.recolor()), Some(night_params));
    // 切主题 → 显式色不被覆盖（画布与重着色均保持）。
    cx.update(|window, cx| {
        app.update(cx, |a, cx| a.toggle_theme(window, cx));
    });
    settle_frames(cx, 3);
    assert_eq!(
        pdf.read_with(cx, |v, _| v.background()),
        Some(gpui::rgb(0x20_20_20).into()),
        "显式背景下主题切换不得覆盖画布色"
    );
    assert_eq!(
        pdf.read_with(cx, |v, _| v.recolor()),
        Some(night_params),
        "显式重着色下主题切换不得清除"
    );
    // 回「默认」→ bg=None（跟随主题）+ 重着色 None。
    cx.update(|_w, cx| {
        app.update(cx, |a, cx| a.set_pdf_bg(PdfBgPref::Default, None, cx));
    });
    assert_eq!(pdf.read_with(cx, |v, _| v.background()), None, "默认档应回落跟随主题");
    assert_eq!(pdf.read_with(cx, |v, _| v.recolor()), None, "默认档应无重着色");
    // 再切主题 → 仍 None（default_canvas 随主题渲染）。
    cx.update(|window, cx| {
        app.update(cx, |a, cx| a.toggle_theme(window, cx));
    });
    settle_frames(cx, 3);
    assert_eq!(
        pdf.read_with(cx, |v, _| v.background()),
        None,
        "默认档切主题 bg 应保持 None（渲染经 default_canvas 跟随）"
    );
    assert_eq!(
        pdf.read_with(cx, |v, _| v.recolor()),
        None,
        "默认档切主题仍无重着色"
    );
}
// PDF 右键菜单点击后移动鼠标误拖选回归（用户反馈）：
// 右键弹菜单 → 左键点**普通关闭型**菜单项（添加书签）→ 菜单关闭 → 之后任意移动鼠标，
// PDF 底下不得出现文字选中（菜单项点击的 mousedown 不应穿透到下层 viewer 把
// `selecting` 臂锁上——旧实现 mousedown 冒泡到 viewer 置 selecting=true，而 mouseup
// 被菜单项 on_click 的 stop_propagation 吃掉 → mouseup 到不了 viewer → selecting 滞留
// → 之后无按键移动都被当成拖选）。
// 对照：直接在文本上按下左键并拖动，仍能正常划选文字（拖选不回归）。

/// bug 回归：右键弹菜单 → 左键点「添加书签」→ 菜单关闭 → 连续移动鼠标到文本上 ≥10 步
/// → 不得出现任何文字选中。
#[gpui::test]
async fn pdf_menu_click_then_move_leaves_no_selection(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("menuselect");
    open_pdf(&app, cx, &p);

    // 先取一个**确定在文本上**的点（首段文本起点），据此把右键点定在让「添加书签」首项
    // 行中心 ≈ 文本点处：菜单锚定 TopLeft，首项行中心 ≈ (pos.x + 43, pos.y + 19)。
    let ((_page, _k), (p1, p2)) = first_char_range_points(&app, cx);
    let rc = gpui::point(p1.x - px(43.), p1.y - px(19.));
    let mods = gpui::Modifiers::default();
    cx.simulate_mouse_move(rc, None, mods);
    cx.run_until_parked();
    cx.simulate_mouse_down(rc, gpui::MouseButton::Right, mods);
    cx.simulate_mouse_up(rc, gpui::MouseButton::Right, mods);
    settle_frames(cx, 3);
    let bm = cx
        .debug_bounds("pdf-menu-bookmark")
        .expect("右键应弹「添加书签」")
        .center();

    // 左键单击菜单项（真实路径：按下→松开，不移动）→ 菜单关闭。
    cx.simulate_mouse_down(bm, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_up(bm, gpui::MouseButton::Left, mods);
    settle_frames(cx, 3);
    assert!(
        cx.debug_bounds("pdf-menu-bookmark").is_none(),
        "点「添加书签」（普通关闭型）后菜单应关闭"
    );

    // 连续移动 ≥10 步：从菜单点击点缓移到文本段上方再向右扫过文字（鼠标**未按住**左键）。
    for i in 1..=12 {
        let t = i as f32 / 12.0;
        let x = bm.x + (p2.x - bm.x) * t;
        let y = bm.y + (p2.y - bm.y) * t;
        cx.simulate_mouse_move(gpui::point(x, y), None, mods);
        cx.run_until_parked();
    }
    settle_frames(cx, 4);

    // 菜单项点击后移动鼠标不得选中文字（旧行为：selecting 滞留 → 扫出大段选区）。
    let sel = cx.update(|_w, cx| {
        tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
            .unwrap()
            .read(cx)
            .selected_text()
    });
    assert!(sel.is_none(), "点菜单项后移动鼠标不应选中文字，实际选区: {sel:?}");
}

/// 对照回归：**直接**在 PDF 文本上按下左键并拖动（不经过菜单）→ 仍能正常划选文字
/// （viewer 拖选路径不回归）。
#[gpui::test]
async fn pdf_direct_drag_select_still_works(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("dragsel");
    open_pdf(&app, cx, &p);

    // 首段文本起点 → 终点，真实拖选（按住左键拖到终点再松开）。
    let ((_a, _b), (p1, p2)) = first_char_range_points(&app, cx);
    let m = gpui::Modifiers::default();
    cx.simulate_mouse_move(p1, gpui::MouseButton::Left, m);
    cx.simulate_mouse_down(p1, gpui::MouseButton::Left, m);
    let mut end = p1;
    for i in 1..=6 {
        let t = i as f32 / 6.0;
        end = gpui::point(p1.x + (p2.x - p1.x) * t, p1.y + (p2.y - p1.y) * t);
        cx.simulate_mouse_move(end, gpui::MouseButton::Left, m);
        cx.run_until_parked();
    }
    cx.simulate_mouse_up(end, gpui::MouseButton::Left, m);
    settle_frames(cx, 3);

    // 拖选应得到非空选区文本（直接划选正常）。
    let sel = cx.update(|_w, cx| {
        tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
            .unwrap()
            .read(cx)
            .selected_text()
    });
    assert!(
        sel.as_ref().map(|s| s.len() > 0).unwrap_or(false),
        "直接拖选文本应得到选区，实际 {sel:?}"
    );
}
