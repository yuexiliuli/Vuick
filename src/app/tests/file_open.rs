    /// 外部文件拖放（OS FileDrop）：拖动文件到窗口并松手应打开文件，
    /// 多个文件逐个开选项卡，且激活最后拖入的文件。
    #[gpui::test]
    async fn external_file_drop_opens_tabs(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..5 {
            cx.run_until_parked();
        }

        let f1 = std::env::temp_dir().join("gpui-drop-a.txt");
        let f2 = std::env::temp_dir().join("gpui-drop-b.txt");
        std::fs::write(&f1, "AAA").unwrap();
        std::fs::write(&f2, "BBB").unwrap();
        let pos = gpui::Point::new(px(480.), px(700.));

        // 模拟资源管理器拖放文件的完整事件序列
        cx.simulate_event(gpui::FileDropEvent::Entered {
            position: pos,
            paths: gpui::ExternalPaths(vec![f1.clone(), f2.clone()].into_iter().collect()),
        });
        cx.simulate_event(gpui::FileDropEvent::Pending { position: pos });
        cx.simulate_event(gpui::FileDropEvent::Submit { position: pos });
        for _ in 0..5 {
            cx.run_until_parked();
        }

        let (tabs, active_content) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.tabs.len(),
                a.tabs[a.active_tab].editor.read(cx).value().to_string(),
            )
        });
        assert_eq!(tabs, 3, "拖入 2 个文件后应比初始多 2 个选项卡，实际={tabs}");
        assert_eq!(active_content, "BBB", "激活选项卡应载入最后拖入的文件内容");
        let _ = std::fs::remove_file(&f1);
        let _ = std::fs::remove_file(&f2);
    }

    /// 无法读取内容的文件（非 UTF-8）：状态置为失败，并推送一条可关闭的提示通知。
    #[gpui::test]
    async fn unreadable_file_shows_notification(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join("gpui-unreadable-test.bin");
        std::fs::write(&p, [0xFF, 0xFE, 0x00, 0xD8, 0x80, 0x00]).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let (status_ok, tabs_before) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (a.status_ok, a.tabs.len())
        });
        assert!(!status_ok, "打开非 UTF-8 文件应置状态为失败");
        let count = cx.update(|_window, cx| app.read(cx).notifications.len());
        assert_eq!(count, 1, "打开失败应推送一条通知");
        let tabs_after = cx.update(|_w, cx| app.read(cx).tabs.len());
        assert_eq!(
            tabs_after, tabs_before,
            "打开失败不应新增空白标签页"
        );
        // 5 秒后应自动消失（推进模拟时钟 6 秒）
        cx.executor().advance_clock(std::time::Duration::from_secs(6));
        for _ in 0..10 {
            cx.run_until_parked();
        }
        let count2 = cx.update(|_window, cx| app.read(cx).notifications.len());
        assert_eq!(count2, 0, "6 秒后打开失败通知应已自动消失");
        let _ = std::fs::remove_file(&p);
    }

    /// 大二进制（>128KB 走后台分块路径）：非 UTF-8 必须同样弹失败通知，
    /// 绝不「打开」成只剩少量合法前缀的乱码页（回归：异步分块路径曾静默吞尾）。
    #[gpui::test]
    async fn large_binary_shows_notification(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        let p = std::env::temp_dir().join("gpui-binary-large.bin");
        // 200_000 字节 > OPEN_ASYNC_MIN_BYTES(128KB) → 强制走 ChunkReader 异步分块路径
        std::fs::write(&p, vec![0xFF; 200_000]).unwrap();
        let tabs_before = cx.update(|_w, cx| app.read(cx).tabs.len());
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        // 异步分块读取需多轮推进（读盘→通道→主线程装配）
        for _ in 0..30 {
            cx.run_until_parked();
        }

        let (status_ok, tabs_after, active_content) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.status_ok,
                a.tabs.len(),
                if a.tabs.is_empty() {
                    None
                } else {
                    Some(a.tabs[a.active_tab].editor.read(cx).value().to_string())
                },
            )
        });
        assert!(!status_ok, "大二进制（非 UTF-8）应置状态为失败");
        let count = cx.update(|_window, cx| app.read(cx).notifications.len());
        assert_eq!(count, 1, "打开大二进制失败应推送一条通知");
        // 大路径先建选项卡再异步失败：选项卡保留但必须为空（未挂任何非法内容）。
        assert_eq!(tabs_after, tabs_before + 1, "大文件先建选项卡再失败");
        assert_eq!(
            active_content.as_deref(),
            Some(""),
            "编辑区不得装入非法内容（乱码前缀）"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 大文档 Markdown 预览冒烟测试：上千行也应能渲染（走虚拟化列表），不 panic。
    #[gpui::test]
    async fn large_markdown_preview_renders(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join("gpui-large-md.md");
        let mut doc = String::new();
        for i in 1..=800 {
            doc.push_str(&format!("## 标题 {i}\n"));
        }
        std::fs::write(&p, &doc).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..10 {
            cx.run_until_parked();
        }
        let (preview_on, content_lines, tabs) = cx.update(|_w, cx| {
            let a = app.read(cx);
            (
                a.tabs[a.active_tab].preview_on,
                a.tabs[a.active_tab].editor.read(cx).value().to_string().lines().count(),
                a.tabs.len(),
            )
        });
        assert!(preview_on, "md 文件打开应自动开启预览");
        assert_eq!(content_lines, 800, "内容应完整载入 800 行");
        assert_eq!(tabs, 2, "打开成功应新增一个标签页（初始 1 + 1）");
        // 切换预览数次后再渲染多帧，确认虚拟化列表路径稳定
        for _ in 0..2 {
            cx.update(|window, cx| {
                app.update(cx, |app, cx| app.toggle_preview_for_tab(app.active_tab, window, cx));
            });
            for _ in 0..5 {
                cx.run_until_parked();
            }
        }
        let still_preview = cx.update(|_w, cx| app.read(cx).show_preview());
        assert!(still_preview);
        let _ = std::fs::remove_file(&p);
    }

    /// Markdown 预览同步（A4：逐键增量桥）——编辑触发 `Edited` 后，右栏 WyEditor 预览
    /// 立即（同步重放，非后台扫描）跟随编辑区，恒与编辑区逐字节一致。MD 分支已脱离
    /// MarkdownPreview（不再喂 `md_preview`），此测试验证统一组件才是真正的「预览」。
    #[gpui::test]
    async fn preview_syncs_content_after_edit(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join("gpui-bgpreview.md");
        std::fs::write(&p, "# 初始标题\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        let preview_text = |cx: &mut VisualTestContext| {
            cx.update(|_w, cx| {
                let a = app.read(cx);
                a.tabs[a.active_tab]
                    .md_wy
                    .as_ref()
                    .map(|w| w.read(cx).text().to_string())
                    .unwrap_or_default()
            })
        };
        let t0 = preview_text(cx);
        assert!(t0.contains("初始标题"), "打开时预览应含初始标题（md_wy 同步喂源），实际 {t0:?}");

        // 真实编辑：粘贴触发 InputEvent::Change → 增量桥同步重放。
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("新增段落".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..30 {
            cx.run_until_parked();
        }
        let (t1, e1) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (
                t.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).text().to_string())
                    .unwrap_or_default(),
                t.editor.read(cx).value().to_string(),
            )
        });
        assert!(
            t1.contains("新增段落"),
            "编辑后预览应跟随新内容（增量桥），实际 {t1:?}"
        );
        assert_eq!(t1, e1, "预览应与编辑区逐字节一致");
        let _ = std::fs::remove_file(&p);
    }

    /// 快速连续两次编辑：增量桥是同步逐键重放（无后台轮次/代次之争），最终预览必
    /// 收敛 = 编辑区（含两次编辑内容），不可能被旧轮覆盖。
    #[gpui::test]
    async fn preview_sync_rapid_edits_converges_without_stale_override(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join("gpui-bgpreview2.md");
        std::fs::write(&p, "A\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        // 连续两次快速编辑（无去抖间隔——桥无去抖，逐键即同步）。
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("B".to_string()));
        cx.dispatch_action(Paste);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("C".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..30 {
            cx.run_until_parked();
        }
        let (t, e) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let t = &a.tabs[a.active_tab];
            (
                t.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).text().to_string())
                    .unwrap_or_default(),
                t.editor.read(cx).value().to_string(),
            )
        });
        assert!(
            t.contains("C") && t.contains("B"),
            "最终预览应含两次编辑内容（收敛到最新），实际 {t:?}"
        );
        assert_eq!(t, e, "预览应与编辑区一致");
        let _ = std::fs::remove_file(&p);
    }

    /// #4 回归：编辑同步的行/字数改用 Rope 的 O(1) 计数（每键不再物化全文），
    /// 数值必须与旧 `str::lines()/chars()` 语义一致——在真实编辑（粘贴触发
    /// `InputEvent::Change`）后与 `editor.value()` 的 str 推导值逐项比对。
    #[gpui::test]
    async fn editing_updates_line_char_counts_via_rope(cx: &mut TestAppContext) {
        fn assert_counts(cx: &mut VisualTestContext, app: &gpui::Entity<EditorApp>, what: &str) {
            let (lc, cc, expected_lc, expected_cc) = cx.update(|_w, cx| {
                let a = app.read(cx);
                let t = &a.tabs[a.active_tab];
                let val = t.editor.read(cx).value();
                (
                    t.line_count,
                    t.char_count,
                    val.lines().count().max(1),
                    val.chars().count(),
                )
            });
            assert_eq!(lc, expected_lc, "{what}：行数应与 str::lines() 一致");
            assert_eq!(cc, expected_cc, "{what}：字数应与 str::chars() 一致");
        }
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let p = std::env::temp_dir().join("gpui-counts.txt");
        std::fs::write(&p, "第一行\n第二行\n").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_counts(cx, &app, "打开文件后");
        // 真实编辑：粘贴触发 InputEvent::Change → 走新的 Rope O(1) 计数
        cx.update(|window, cx| {
            let focus = app.read(cx).editor_focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("第三行".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_counts(cx, &app, "粘贴编辑后");
        // 再粘贴带换行的内容，行数应增加且仍与 str 语义一致
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("A\nB".to_string()));
        cx.dispatch_action(Paste);
        for _ in 0..5 {
            cx.run_until_parked();
        }
        assert_counts(cx, &app, "粘贴多行后");
        let _ = std::fs::remove_file(&p);
    }

    /// 基准：编辑大文档时「每键同步行/字数」的代价——旧逻辑每键物化全文 +
    /// `lines()/chars()` 全量扫描（16 万行约产生 3 次 O(n) + 一次 6MB 副本，编辑卡顿主因），
    /// 新逻辑走 Rope 的 O(1) 计数零物化。打印、不构成断言；`--ignored` 运行。
    #[test]
    #[ignore]
    fn bench_edit_sync_counts_old_materialize_vs_rope() {
        use crate::editor::rope_ext::RopeExt;
        use ropey::Rope;
        use std::time::Instant;
        let line = "普通段落内容 alpha beta 关键字 shanghai 你好\n";
        let mut doc = String::with_capacity(line.len() * 160_000);
        for _ in 0..160_000 {
            doc.push_str(line);
        }
        let rope = Rope::from(doc.as_str());
        let runs = 60usize;
        // 旧：value() 物化（6MB 副本）+ lines().count() + chars().count()
        let t0 = Instant::now();
        let mut sink = 0usize;
        for _ in 0..runs {
            let v: gpui::SharedString = doc.clone().into();
            let lc = v.lines().count().max(1);
            let cc = v.chars().count();
            sink = sink.wrapping_add(lc + cc);
        }
        let old = t0.elapsed();
        // 新：Rope 的 O(1) 计数（len_chars / lines_len / byte），零物化
        let t1 = Instant::now();
        let mut sink2 = 0usize;
        for _ in 0..runs {
            let lines = rope.lines_len();
            let lc = if rope.len() > 0 && rope.byte(rope.len() - 1) == b'\n' {
                lines - 1
            } else {
                lines
            };
            let cc = rope.len_chars();
            sink2 = sink2.wrapping_add(lc.max(1) + cc);
        }
        let new_ = t1.elapsed();
        assert_eq!(sink, sink2, "新旧计数的结果必须一致");
        println!(
            "编辑每键行/字计数（{} 行 × {runs} 次）：旧(物化 6MB+lines+chars)={}ms, 新(Rope O(1))={}ms（提升 {:.1}x）",
            160_000,
            old.as_millis(),
            new_.as_millis(),
            old.as_secs_f64() / new_.as_secs_f64().max(1e-9),
        );
    }

    /// 基准：大文档预览同步里 `scan_blocks` 的代价（#4 把它从 UI 线程挪到后台）。
    /// 打印、不构成断言；`--ignored` 运行。
    #[test]
    #[ignore]
    fn bench_preview_sync_scan_blocks_off_ui_thread() {
        use std::time::Instant;
        let block = "# 章节标题\n\n正文段落 alpha beta 关键字 shanghai\n\n- 列表项\n\n```rust\nlet x = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let mut doc = String::with_capacity(block.len() * 12000);
        for _ in 0..12000 {
            doc.push_str(block);
        }
        // 旧路径 UI 线程同步成本 = value().to_string() + scan_blocks（都占 UI）
        let t0 = Instant::now();
        let idx = crate::markdown::scan_blocks(&doc);
        let scan = t0.elapsed();
        // 新路径 UI 线程只做快照（value() 的 SharedString 物化），scan_blocks 在后台
        let t1 = Instant::now();
        let _ = std::hint::black_box(gpui::SharedString::from(doc.clone()));
        let snap = t1.elapsed();
        println!(
            "大文档预览同步（{} 行 / {} MB，{} 块）：scan_blocks={}ms（已转后台不占 UI），UI 快照仅 {}ms",
            doc.lines().count(),
            doc.len() / 1024 / 1024,
            idx.blocks.len(),
            scan.as_millis(),
            snap.as_millis(),
        );
    }

    /// 回归：首次在新选项卡编辑内容后打开预览，预览应默认换行
    /// （不出现单行横向滚动），与「md 文件打开即换行」一致。
    #[gpui::test]
    async fn preview_defaults_to_wrap_after_edit_then_toggle(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // 初始空选项卡（未命名）里输入一段超长内容
        let long_text = "这是一个比较长的段落".repeat(60);
        cx.update(|_, cx| {
            let idx = app.read(cx).active_tab;
            let ed = app.read(cx).tabs[idx].editor.clone();
            ed.update(cx, |ed, _| ed.set_text(&long_text));
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }

        // 打开预览
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.toggle_preview_for_tab(app.active_tab, window, cx)
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
        }

        let wrap = cx.update(|_w, cx| {
            let a = app.read(cx);
            // P4-2：预览统一 WyEditor 恒换行（`MarkdownPreview::is_wrap` 已删）。
            a.tabs[a.active_tab].md_wy.is_some()
        });
        assert!(wrap, "编辑内容后首次打开预览应默认唤出统一换行预览宿主");
    }

    /// 语法高亮回归：打开 rust 文件，解析在后台异步进行（不卡 UI / 不 panic），
    /// 推进时钟超过防抖后解析落地并触发重渲染，编辑器区正常、内容完整载入。
    #[gpui::test]
    async fn opens_rust_file_highlight_async_no_hang(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join("gpui-syntax-async.rs");
        let mut src = String::from(
            "fn main() {\n    // 注释\n    let s = \"hello\";\n    println!(\"{} 世界\", s);\n}\n",
        );
        src.push_str(&"// filler line\n".repeat(600));
        std::fs::write(&dir, &src).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        for _ in 0..6 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        // 推进超过解析防抖（150ms），后台解析落地 → 触发重渲染
        cx.executor().advance_clock(std::time::Duration::from_millis(400));
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("editor-area").is_some(),
            "打开 rust 后编辑区应正常渲染"
        );
        assert_eq!(
            cx.update(|_w, cx| {
                app.read(cx).tabs[app.read(cx).active_tab]
                    .editor
                    .read(cx)
                    .value()
                    .to_string()
            }),
            src,
            "rust 文件内容应完整载入"
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// 回归：>1MB 文本文件走「后台打开」流水线——`open_file` 立即返回（编辑器仍为空、
    /// 不在 UI 线程同步读+统计），随后后台线程装配出完整内容（字节级一致）。同时
    /// 防「打开耗时随文件体积线性冻住 UI」这一性质被后续改动破坏。
    #[gpui::test]
    async fn opens_large_text_file_asynchronously(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "gpui-large-open-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let line = "x".repeat(80);
        let content = (0..20_000)
            .map(|i| format!("{line} {i:07}\n"))
            .collect::<String>();
        assert!(
            content.len() > crate::config::limits::OPEN_ASYNC_MIN_BYTES as usize,
            "测试文件应超过后台打开阈值，实际 {} B",
            content.len()
        );
        std::fs::write(&dir, &content).unwrap();
        let tabs_before = cx.update(|_, cx| app.read(cx).tabs.len());

        // 打开应**立即返回**且编辑器仍为空（内容由后台线程装配，不阻塞 UI）。
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        let empty_after_open = cx.update(|_, cx| {
            let a = app.read(cx);
            a.tabs[a.active_tab].editor.read(cx).value().is_empty()
        });
        assert!(empty_after_open, "大文件应异步打开：open_file 返回时编辑器应为空");

        // 驱动后台任务 + 装配，直到内容字节级一致。
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                let tab = &a.tabs[a.active_tab];
                !tab.editor_pending && tab.editor.read(cx).value() == content
            });
            if ready {
                break;
            }
            frames += 1;
            if frames > 300 {
                panic!("大文件后台打开超时：{} 帧未装配完成", frames);
            }
        }
        assert_eq!(
            cx.update(|_, cx| app.read(cx).tabs.len()),
            tabs_before + 1,
            "大文件打开应新增一个选项卡"
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// 回归：>128KB 的 Markdown 走「两段式打开」**完整流程**——快段（defer）即时挂载
    /// 编辑器内容，慢段后台并行 行宽统计 + scan_blocks 后补齐滚动条统计与预览索引。
    /// 断言两段各执行 1 次（open_fast_count/open_finish_count）、编辑器内容字节级一致、
    /// 预览块索引就绪、打开状态成功。
    #[gpui::test]
    async fn opens_large_markdown_full_flow(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "gpui-large-md-flow-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let content = (0..3000)
            .map(|i| {
                format!(
                    "# 大标题 {i}\n\n这是第 {i} 段正文，包含 *强调*、`行内代码` 与 [链接](https://x.y)。\n\n"
                )
            })
            .collect::<String>();
        assert!(
            content.len() > crate::config::limits::OPEN_ASYNC_MIN_BYTES as usize,
            "测试文件应超过后台打开阈值，实际 {} B",
            content.len()
        );
        std::fs::write(&dir, &content).unwrap();

        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                let tab = &a.tabs[i];
                !tab.editor_pending
                    && tab.editor.read(cx).value() == content
                    && tab.md_wy
                        .as_ref()
                        .map(|w| w.read(cx).is_loaded())
                        .unwrap_or(false)
            });
            if ready {
                break;
            }
            frames += 1;
            if frames > 300 {
                panic!("大 md 完整流程超时：{} 帧", frames);
            }
        }
        let (fast_count, finish_count, wy_loaded, status_ok) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            (
                a.open_fast_count,
                a.open_finish_count,
                a.tabs[i].md_wy.as_ref().map(|w| w.read(cx).is_loaded()).unwrap_or(false),
                a.status_ok,
            )
        });
        assert_eq!(fast_count, 1, "两段式快段应执行 1 次");
        assert_eq!(finish_count, 1, "两段式慢段应执行 1 次");
        assert!(wy_loaded, "右栏 WyEditor 预览应装配完成");
        assert!(status_ok, "打开状态应成功");
        let _ = std::fs::remove_file(&dir);
    }

    /// 回归：大文档打开时 ListState 为**固定小窗口**（O(1) 挂载，与全文行数无关）——
    /// mount 同步栈内即可断言 item_count 远小于全文行数（★不建 N 条 → 内存 O(1)），
    /// 防止退化成 mount 同步 O(N) 建全文（那正是内存 +~300MB / 打开冻帧的元凶）。
    #[gpui::test]
    async fn open_large_doc_mounts_list_two_phase(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let line = "x".repeat(60);
        let content = (0..12_000)
            .map(|i| format!("{line} {i:010}\n"))
            .collect::<String>();
        use crate::editor::rope_ext::RopeExt as _;
        let rope = ropey::Rope::from(content.as_str());
        let line_count = rope.lines_len();
        assert!(line_count > 4_096, "测试应远超固定窗口（512），实际 {line_count} 行");

        let ed_ent = cx.update(|_w, cx| app.read(cx).tabs[0].editor.clone());
        // 同步栈内断言两段式：mount 返回时只建了视口小列表（若非两段式，这里
        // item_count 直接等于全文行数）。
        cx.update(|_w, cx| {
            ed_ent.update(cx, |ed, cx| {
                ed.set_rope_with_stats(rope.clone(), 0, Default::default());
                let ic = ed.test_list_state().item_count();
                assert!(
                    ic < line_count && ic <= crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS,
                    "mount 同步栈内应为视口小列表：item_count={ic}（全文 {line_count} 行，\
                     两段式应只建 ≤{}）",
                    crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS
                );
                cx.notify(); // 真实打开路径 load_file_text 会 notify；这里等价驱动首帧 render
            });
        });
        // 固定窗口虚拟化：mount 即就绪（无分帧增长），列表恒为小窗口。
        let mut frames = 0;
        loop {
            cx.cx.executor().advance_clock(std::time::Duration::from_millis(20));
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            let done = cx.update(|_w, cx| {
                let ed = ed_ent.read(cx);
                !ed.test_list_is_deferred()
                    && ed.test_list_window_len() <= crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS
                    && ed.test_metrics().1 > 0.
            });
            if done {
                break;
            }
            frames += 1;
            if frames > 120 {
                panic!("固定窗口挂载超时：{frames} 帧");
            }
        }
        cx.cx.executor().advance_clock(std::time::Duration::from_millis(20));
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let (mo, vh, lh) = cx.update(|_w, cx| {
            let ed = ed_ent.read(cx);
            (
                ed.list_max_offset(),
                ed.list_viewport_height(),
                ed.test_metrics().1,
            )
        });
        let expected = (line_count.saturating_sub(1)) as f32 * lh - vh;
        assert!(
            mo > expected * 0.9,
            "量程应按全文行数健康：max_off={mo:.0}，预期≈{expected:.0}（行 {line_count}/高 {lh:.0}px）"
        );
    }

    /// 回归：大文档「分块读盘·渐进装配」——打开期间编辑器内容**渐进**出现（读多少
    /// 显示多少），首块挂载即可视（`editor_pending`=false），全部读完后内容字节级一致。
    /// 逐 tick 驱动协作式调度器，断言在某个中间时刻行数落在 (0, 全文行数) 之间——
    /// 证明内容不是「整篇读完一次性原子挂载」，而是逐块推送上屏。
    #[gpui::test]
    async fn progressive_load_shows_partial_content_early(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "gpui-progressive-mount-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        // ~3.3MB / 60k 行：远超异步阈值与首块 256KB 下限，且多于有界通道容量（多块）。
        let line = "x".repeat(48);
        let content = (0..60_000)
            .map(|i| format!("{line} {i:07}\n"))
            .collect::<String>();
        assert!(
            content.len() > crate::config::limits::OPEN_ASYNC_MIN_BYTES as usize,
            "测试文件应超过后台打开阈值，实际 {} B",
            content.len()
        );
        std::fs::write(&dir, &content).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });

        let mut saw_partial = false;
        let mut ticks = 0;
        let mut done = false;
        // 逐 tick（每步至多一个任务轮询）：观察进程中的部分内容；无余量任务即停止。
        while ticks < 3000 {
            if !cx.cx.executor().tick() {
                break;
            }
            ticks += 1;
            let (lc, pending, ready_full) = cx.update(|_w, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                let tab = &a.tabs[i];
                let ed = tab.editor.read(cx);
                let outf_block = a.status_ok && ed.value() == content;
                (ed.line_count(), tab.editor_pending, outf_block)
            });
            if !pending && lc > 0 && lc < 60_000 {
                saw_partial = true;
            }
            if ready_full {
                done = true;
            }
        }
        assert!(done, "渐进装配应收尾：{ticks} ticks 后未完成");
        assert!(
            saw_partial,
            "应观察到「部分内容先行上屏」的中间态（{ticks} ticks；若缺 256KB 首块下限/逐块外推则为整篇原子挂载）"
        );
        let (dlc, pending, final_ok) = cx.update(|_w, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            let tab = &a.tabs[i];
            (
                tab.editor.read(cx).display_line_count(),
                tab.editor_pending,
                tab.editor.read(cx).value() == content,
            )
        });
        assert!(!pending, "装配完成后 editor_pending 应复位");
        assert!(final_ok, "最终内容应与文件字节级一致");
        assert_eq!(dlc, 60_000, "最终显示行数应为全文行数");
        let _ = std::fs::remove_file(&dir);
    }

    /// A4（承接原“预览与编辑器共享源 Rope”回归）：大 md 打开（完整异步流水线 +
    /// `finish_file_text` 装预览）后，右栏统一组件 WyEditor 由**同一棵编辑器 Rope**（ropey
    /// 2.0 Arc 树，`Clone` O(1)）后台 `open_from_rope` 装配——不物化第二份源文本，
    /// MD 现在只有这一份预览解析态（不再有 MarkdownPreview 的第二份扫描）。
    #[gpui::test]
    async fn large_md_preview_covers_full_source(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "gpui-md-share-rope-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        // 块数足够多（转成 >128KB 文本）→ 走异步流水线（finish 安装 md_wy）。
        let n = 4_800;
        let content = (0..n)
            .map(|i| {
                format!("# 大标题 {i}\n\n这是第 {i} 段正文，包含 *强调*、`行内代码` 与 [链接](https://x.y)。\n\n")
            })
            .collect::<String>();
        assert!(
            content.len() > crate::config::limits::OPEN_ASYNC_MIN_BYTES as usize,
            "测试 md 应超过后台打开阈值，实际 {} B",
            content.len()
        );
        std::fs::write(&dir, &content).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        let mut frames = 0;
        loop {
            cx.run_until_parked();
            let loaded = cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                let t = &a.tabs[i];
                !t.editor_pending
                    && t.md_wy
                        .as_ref()
                        .map(|w| w.read(cx).is_loaded())
                        .unwrap_or(false)
            });
            if loaded {
                break;
            }
            frames += 1;
            if frames > 300 {
                panic!("大 md 打开/预览装配超时：{frames} 帧");
            }
        }
        // 预览全文 = 编辑区正文（md_wy 从同一棵 Rope 装配，覆盖完整源；一份解析态）。
        let (wy_text, ed_text) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            (
                a.tabs[i].md_wy.as_ref().unwrap().read(cx).text().to_string(),
                a.tabs[i].editor.read(cx).value().to_string(),
            )
        });
        assert_eq!(wy_text, ed_text, "大 md WyEditor 预览应覆盖完整源（与编辑区一致）");
        let _ = std::fs::remove_file(&dir);
    }

    /// 回归（专家评审 P1 升级版）：固定窗口虚拟化下的大文档「深跳」——Ctrl+End /
    /// 搜索跳到远超视口的行时：
    /// 1. ListState 恒是**固定小窗口**（O(WINDOW) 挂载，不建 3.2M 条 → 内存 O(1)）；
    /// 2. 跳转**瞬时落位**（重定位 `window_base` 到目标行附近并渲染：无占位符、无
    ///    「分帧增长逐块收敛」循环），比旧「深跳占位 + 逐帧增长」更快且更简单。
    #[gpui::test]
    async fn deep_jump_lands_instantly_with_window(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let line = "x".repeat(60);
        let content = (0..90_000)
            .map(|i| format!("{line} {i:010}\n"))
            .collect::<String>();
        use crate::editor::rope_ext::RopeExt as _;
        let rope = ropey::Rope::from(content.as_str());
        let line_count = rope.lines_len();

        let ed_ent = cx.update(|_w, cx| app.read(cx).tabs[0].editor.clone());
        cx.update(|_w, cx| {
            ed_ent.update(cx, |ed, cx| {
                ed.set_rope_with_stats(rope.clone(), 0, Default::default());
                cx.notify(); // 等价 load_file_text 的 notify，驱动首帧 render
            });
        });
        // 挂载即就绪：固定小窗口，无分帧增长。
        for _ in 0..5 {
            cx.cx.executor().advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let (deferred, win, base0, rows) = cx.update(|_w, cx| {
            let ed = ed_ent.read(cx);
            (
                ed.test_list_is_deferred(),
                ed.test_list_window_len(),
                ed.test_window_base(),
                ed.line_count(),
            )
        });
        assert!(!deferred, "固定窗口挂载即就绪（无两段式中间态）");
        assert!(
            win <= crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS,
            "固定窗口应为小窗口，实际 {win}"
        );
        assert_eq!(base0, 0, "未滚动前窗口基址应为 0");
        assert!(rows >= line_count, "行数应加载完整：{rows} ≥ {line_count}");

        // 深跳：选区到全文末尾 → 瞬时落位。
        cx.update(|_w, cx| {
            ed_ent.update(cx, |ed, cx| {
                ed.set_selection_range(content.len().saturating_sub(1)..content.len(), cx);
            });
        });
        for _ in 0..5 {
            cx.cx.executor().advance_clock(std::time::Duration::from_millis(1));
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let (top, base) = cx.update(|_w, cx| {
            let ed = ed_ent.read(cx);
            (ed.test_text_scroll_top().0, ed.test_window_base())
        });
        assert!(
            top >= line_count.saturating_sub(64),
            "深跳应瞬时滚到全文末尾附近：top={top}（全文 {line_count} 行）"
        );
        assert!(base > 0, "窗口基址应重定位到大行号：base={base}");
        assert!(
            top.saturating_sub(base) < crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS,
            "目标行应在窗口内：abs_top={top} base={base}"
        );
    }

    /// 回归（2026-09-16）：底部预留必须被渲染成**可见**的空白区，最后一行不得被
    /// 底部横向滚动条盖住。曾用「滚动量程里加 pad」实现，但 gpui 的 List 在下方内容
    /// 不足一屏时会**自动上移填满视口**，把预留区吃掉、最后一行贴到视口底（被滚动条
    /// 遮住）。修复：把预留做成 ListState 末尾的**真实空白条目**（占行高）。本测试在
    /// 真实 app 布局下滚到量程最大值，断言最后一行底边距视口底 ≈ pad（空白真实可见）。
    #[gpui::test]
    async fn bottom_pad_renders_visible_blank_at_max_scroll(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!("gpui-botpad-{}.txt", std::process::id()));
        let content = (0..300)
            .map(|i| format!("这是第 {i} 行内容"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&dir, &content).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        let ed_ent = {
            let mut notices = 0;
            loop {
                cx.cx.executor().advance_clock(std::time::Duration::from_millis(20));
                cx.run_until_parked();
                let _ = cx.cx.refresh();
                let ready = cx.update(|_, cx| {
                    let a = app.read(cx);
                    let i = a.tabs.len().saturating_sub(1);
                    let ed = a.tabs[i].editor.read(cx);
                    ed.line_count() > 1 && !ed.test_list_is_deferred()
                });
                if ready {
                    break;
                }
                notices += 1;
                if notices > 120 {
                    panic!("打开/增长未完成");
                }
            }
            cx.update(|_, cx| {
                let a = app.read(cx);
                let i = a.tabs.len().saturating_sub(1);
                a.tabs[i].editor.clone()
            })
        };
        let (lh, pad, row_count, item_count, want_items) = cx.update(|_, cx| {
            let ed = ed_ent.read(cx);
            (
                ed.test_metrics().1.max(1.),
                ed.test_bottom_pad_px(),
                ed.line_count(),
                ed.test_list_state().item_count(),
                ed.test_scroll_item_count(),
            )
        });
        // 固定窗口虚拟化：条目数恒为小窗口（与全文行数无关），量程按全文
        // `scroll_item_count` 计算——底部预留随量程存在于滚动最底端。
        assert!(
            item_count <= crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS,
            "固定窗口应是小窗口：item_count={item_count} ≤ {}",
            crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS
        );
        assert!(
            want_items >= row_count && want_items <= row_count + 16,
            "全文量程条目数应为行数+预留空行：want={want_items} rows={row_count}"
        );
        cx.update(|_, cx| {
            ed_ent.update(cx, |ed, _| ed.test_scroll_to_bottom());
        });
        for _ in 0..4 {
            cx.cx.executor().advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        let (ix, off, view2) = cx.update(|_, cx| {
            let ed = ed_ent.read(cx);
            let t = ed.test_text_scroll_top();
            (t.0, t.1, ed.list_viewport_height())
        });
        // 最后一行底边在视口内的 y =（最后一行距滚动顶的行数 × 行高）+ 行高 − 已滚进
        // 顶行的偏移；底部空白 = 视口高 − 该 y。基底预留应 ≈ pad 且 ≥ pad − 1 行。
        let last_top_px = (row_count as f32 - ix as f32) * lh - off;
        let last_bottom = last_top_px + lh;
        let blank_valid = view2 - last_bottom;
        // gpui 对「填充不满一屏」会回退一行把视口填满（浮点舍入所致），实际空白可能
        // 比 pad 少约 1 行；断言「可见空白 ≥ pad − 行高 − 余量」即可证明预留区真实渲染、
        // 最后一行未被横向滚动条盖住。
        assert!(
            blank_valid >= pad - lh - 2.,
            "滚到底后最后一行下方应有可见预留空白：blank={blank_valid:.1}px，pad={pad:.1}px，行高 {lh:.1}（scroll_top=({ix},{off}), view={view2}, rows={row_count}）——真实预留空行未渲染"
        );
        assert!(
            blank_valid <= pad + lh + 1.,
            "底部空白不应明显超过预留：blank={blank_valid:.1}px ≤ pad+行高={:.1}",
            pad + lh + 1.
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// 临时基准（ignored，release）：开不同体积的真实大文档，量化各阶段耗时——
    /// 同步返回 / 内容可见（快段）/ 全文 ListState 分帧增长完成 / 滚动条就绪。
    /// 运行：`cargo test --release -- --ignored bench_open_large_multiple --nocapture`
    /// 打开期间设 MD_OPEN_DEBUG=1，配合每阶段的墙钟标记定位瓶颈占比。
    #[gpui::test]
    #[ignore]
    async fn bench_open_large_multiple(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(980.), px(720.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        unsafe { std::env::set_var("MD_OPEN_DEBUG", "1") };
        for path in [
            r"fixtures/大文档测试.md",
            r"fixtures/大文档测试23MB.md",
            r"fixtures/大文档测试23MB.txt",
            r"fixtures/大文档测试116MB.txt",
        ] {
            println!("========== {path} ==========");
            let t0 = std::time::Instant::now();
            cx.update(|window, cx| {
                app.update(cx, |app, cx| app.open_file(path, window, cx));
            });
            let sync_ms = t0.elapsed().as_millis();
            println!("【open_file 同步返回】 {sync_ms} ms");
            let mut content_ms = None;
            let mut full_ms = None;
            let mut frames = 0;
            loop {
                frames += 1;
                // 推模拟时钟触发分帧增长定时器（真实应用由 OS 定时器/帧循环驱动）。
                cx.cx.executor().advance_clock(std::time::Duration::from_millis(20));
                cx.run_until_parked();
                let _ = cx.cx.refresh();
                let (line_count, _item_count, deferred) = cx.update(|_, cx| {
                    let a = app.read(cx);
                    let i = a.tabs.len().saturating_sub(1);
                    let ed = a.tabs[i].editor.read(cx);
                    (
                        ed.line_count(),
                        ed.test_list_state().item_count(),
                        ed.test_list_is_deferred(),
                    )
                });
                if content_ms.is_none() && line_count > 0 {
                    content_ms = Some(t0.elapsed().as_millis() as usize);
                }
                if !deferred && full_ms.is_none() {
                    full_ms = Some(t0.elapsed().as_millis() as usize);
                }
                if content_ms.is_some() && full_ms.is_some() {
                    break;
                }
                if frames > 4000 {
                    println!("【超时】 content={content_ms:?} full={full_ms:?}");
                    break;
                }
            }
            println!(
                "【内容可见】 {:?} ms，【ListState 就绪（固定窗口）】 {:?} ms，固定窗口条目 {}（常数），行数 {}",
                content_ms,
                full_ms,
                crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS,
                cx.update(|_, cx| {
                    let a = app.read(cx);
                    let i = a.tabs.len().saturating_sub(1);
                    a.tabs[i].editor.read(cx).line_count()
                })
            );
        }
    }

    /// 基准（ignored，release）：隔离测量大列表「统一高度重铺」的原始开销——正是旧的
    /// 一次性 defer 补建/量程自愈会用到的操作（现大文档路径已改分帧增长 + 按需实测，
    /// 不再调用）。运行：
    /// `cargo test --release -- --ignored bench_reset_uniform_height --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_reset_uniform_height(cx: &mut TestAppContext) {
        init_test(cx);
        for n in [16_000usize, 169_721] {
            let ls = gpui::ListState::new(1, gpui::ListAlignment::Top, gpui::px(16.));
            let mut best_new = u128::MAX;
            for _ in 0..3 {
                let t = std::time::Instant::now();
                ls.reset_with_uniform_height(n, gpui::px(21.5));
                best_new = best_new.min(t.elapsed().as_micros());
            }
            let mut best_two = u128::MAX;
            for _ in 0..3 {
                let t = std::time::Instant::now();
                ls.reset(n);
                let _ = ls.clone().with_uniform_item_height(gpui::px(21.5));
                best_two = best_two.min(t.elapsed().as_micros());
            }
            println!(
                "【reset_with_uniform_height】 n={n}: 单趟 new={}µs, 旧两趟(reset+uniform)={}µs",
                best_new, best_two
            );
            let _ = cx;
        }
    }

    /// 回归：两段式打开大 md 后，**纵向滚动条量程必须健康**（拖到底可到全文末尾）。
    /// 历史根因：gpui 在布局宽度变化时清空 ListState 全部 size_hint，依赖
    /// `max_offset_for_scrollbar()` 的量程坍缩到 ~0，拖到底也滚不到底（曾以「64px
    /// 量化 + ensure_uniform_hint 重挂 + notify 多画一帧」绕开/自愈，均已删除）。
    /// 现状（M1.3 P7）：编辑器恒走确定性像素模型（`list_max_offset` = 全文条目数 ×
    /// 行高 − 视口），与 gpui 换宽清 size_hint **无关**，量程恒健康；本测试的
    /// notify + refresh 只驱动一次真实布局让 `viewport_bounds` 落定，再读量程验证。
    #[gpui::test]
    async fn opens_large_md_keeps_vertical_scrollbar_healthy(cx: &mut TestAppContext) {
        init_test(cx);
        let (app, cx) = add_app(cx);
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let dir = std::env::temp_dir().join(format!(
            "gpui-scroll-healthy-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let mut content = format!("{}\n", "x".repeat(8000));
        content.push_str(
            &(0..5000)
                .map(|i| {
                    format!("# 大标题 {i}\n\n这是第 {i} 段正文，包含 *强调* 与 `code`。\n\n")
                })
                .collect::<String>(),
        );
        assert!(
            content.len() > crate::config::limits::OPEN_ASYNC_MIN_BYTES as usize,
            "测试文件应超过后台打开阈值"
        );
        std::fs::write(&dir, &content).unwrap();
        cx.update(|window, cx| {
            app.update(cx, |app, cx| app.open_file(dir.to_str().unwrap(), window, cx));
        });
        // 等待完整流程（快段 + 慢段 finish），并通过 refresh 驱动「重挂统一高度提示后
        // 的多画一帧」（gpui 测试里 notify 不会自动重画，需 refresh；真实应用由帧循环）。
        let mut frames = 0;
        loop {
            cx.cx.executor().advance_clock(std::time::Duration::from_millis(20));
            cx.run_until_parked();
            let _ = cx.cx.refresh();
            let done = cx.update(|_, cx| app.read(cx).open_finish_count >= 1);
            if done && frames > 6 {
                break;
            }
            frames += 1;
            if frames > 120 {
                break;
            }
        }
        let (lines, lh) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            let ed = a.tabs[i].editor.read(cx);
            (ed.line_count(), ed.test_metrics().1)
        });
        // 量程 = 确定性像素模型（`list_max_offset` = 全文条目数 × 行高 − 视口），与
        // gpui 换宽清 size_hint 无关（`ensure_uniform_hint` 已删除）。notify + refresh
        // 只驱动一次真实布局让 `viewport_bounds` 落定，再读 `list_max_offset` 验证。
        let ed_ent = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i].editor.clone()
        });
        cx.update(|_, cx| {
            ed_ent.update(cx, |_, cx| cx.notify());
        });
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let (mo, vh) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            let ed = a.tabs[i].editor.read(cx);
            (ed.list_max_offset(), ed.list_viewport_height())
        });
        let expected = (lines.saturating_sub(1)) as f32 * lh - vh;
        assert!(
            mo > expected * 0.9,
            "编辑器量程在再次渲染时应自愈（max_off={mo:.0}，预期≈{expected:.0}）"
        );
        // 预览初始应在顶部（同步开启前的基线；大文档装好后预览停在开头）。
        let top0 = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            (-a.tabs[i].md_wy_vscroll.offset().y).as_f32()
        });
        assert!(top0 >= -0.5, "预览初始应在顶部（top0={top0:.0}px）");
        // 编辑器「拖到底」：模拟 VerticalScrollHandle.start_drag 的重铺+冻结+拖底，
        // 断言拖到底后最后内容可见。此后编辑区与预览区经滚动同步联动（预览跟随）。
        let els = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i].editor.read(cx).test_list_state()
        });
        els.reset_with_uniform_height(lines, gpui::px(lh));
        els.scrollbar_drag_started();
        els.set_offset_from_scrollbar(gpui::Point::new(gpui::px(0.), gpui::px(-10_000_000.)));
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let eix = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i]
                .editor
                .read(cx)
                .test_list_state()
                .logical_scroll_top()
                .item_ix
        });
        assert!(
            lines.saturating_sub(eix) <= 50,
            "编辑器拖到底应到达最后内容（滚动顶行 item_ix={eix}/{lines}）"
        );
        // 预览侧（A2：统一组件 WyEditor 宿主）量程同样须健康：大文档装好后宿主
        // `#md-wy-scroll` 的量程必须远超视口（>0），不能坍缩——否则预览滚动条拖不到底。
        let (wy_loaded, max_off) = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            let t = &a.tabs[i];
            let vh = &t.md_wy_vscroll;
            (
                t.md_wy
                    .as_ref()
                    .map(|w| w.read(cx).is_loaded())
                    .unwrap_or(false),
                vh.max_offset().y.as_f32(),
            )
        });
        assert!(wy_loaded, "大文档 WyEditor 预览应完成装配（下一帧渲染即出内容）");
        assert!(
            max_off > 50.0,
            "预览纵向量程不应坍缩（max_off={max_off:.0}px，应≈全篇块高之和）"
        );
        // 注：编辑器在上一段已被「拖到底」，预览随滚动同步联动不再停于顶部——
        // 「初始在顶部」基线已在上方（编辑器拖底之前）单独断言过；这里只看量程健康。
        // 确定性「拖到底」回归（用户实测：内置滚动条在量程坍缩/未自愈时开始拖会冻结
        // 旧值 → 拖不到真正的底部）。直接推宿主滚动句柄到底，断言实际落到了末端附近。
        cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i]
                .md_wy_vscroll
                .set_offset(gpui::point(gpui::px(0.), gpui::px(-1_000_000.)));
        });
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let bottom_off = cx.update(|_, cx| {
            let a = app.read(cx);
            let i = a.tabs.len().saturating_sub(1);
            a.tabs[i].md_wy_vscroll.offset().y.as_f32()
        });
        assert!(
            bottom_off < -max_off * 0.9,
            "预览拖到底应接近真正的底部（offset={bottom_off:.0}，max_off={max_off:.0}）"
        );
        let _ = std::fs::remove_file(&dir);
    }

