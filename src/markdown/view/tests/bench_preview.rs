    /// 基准（--ignored）：源码编辑区每键 → 预览同步（source+预览链）的确定性拆账。
    /// 真实 `fixtures/大文档测试2.md`（窗口模式），在一个 `md.update` 闭包内循环
    /// N 轮「rope 插字符 + 重扫 + set_source_with_index + prepare_render（窗口路径）」，
    /// 杜绝 gpui harness 每轮 update 开销污染；各段耗时由 `prof::` 标签累计、dump 输出
    /// 每轮均摊：md.set_source / md.ss.toc / md.ss.reset / md_hint / md.hm.rebuild /
    /// md_ensure_window。打印、不断言；`--ignored` 运行。
    #[gpui::test]
    #[ignore]
    fn bench_preview_edit_sync_stage_cost(cx: &mut TestAppContext) {
        use std::time::Instant;
        crate::prof::enable();
        cx.update(crate::actions::bind_local_keymaps);
        let path = "fixtures/大文档测试2.md";
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[bench] skip realfile: {e}");
                return;
            }
        };
        let (host, vcx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let vcx: &mut VisualTestContext = vcx;
        let rope0 = ropey::Rope::from_str(&text);
        let idx0 = std::rc::Rc::new(crate::markdown::scan_blocks_rope(&rope0));
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.set_source_with_index(crate::markdown::PreviewSrc::Rope(rope0), idx0.clone(), cx);
            });
        });
        vcx.simulate_resize(size(px(980.), px(760.)));
        settle(vcx);
        let n_items = vcx.update(|_, cx| host.read(cx).md.read(cx).item_count());
        assert!(
            n_items > crate::config::limits::WINDOWED_MIN_BLOCKS,
            "需窗口模式（块数 {n_items}）"
        );

        let p = vcx.update(|_window, cx| Rc::new(StyleParams::from_theme_with_font(cx, None)));
        let n = 120usize;
        let t0 = Instant::now();
        let mut rope = ropey::Rope::from_str(&text);
        // 增量基底（镜像 app 的 `preview_prev`）：保留上一份 rope 快照 + 块索引，
        // 每键用 `scan_blocks_incremental_rope`（与 app/mod.rs 后台路径同式）——
        // 尾部分段共享 ⇒ 换源 drop 上一份索引只是 O(1) 减引用，不被 3.2MB 释放伪影污染。
        let mut prev_rope = rope.clone();
        let mut prev_idx = idx0.clone();
        let at = n_items / 2;
        let mut scans_ms = 0.0f64;
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |m, cx| {
                for i in 0..n {
                    // 把「单字符输入」建模在文档中部附近几个点轮转插入（近似逐键但避免
                    // 让同一段无限增长）。扫描是 app 后台职责，这里仅计时作参照。
                    let len = rope.len_chars();
                    let off_chars = ((at + (i % 12) * 2) % len.max(1)).min(len) as usize;
                    let off = rope.char_to_byte_idx(off_chars.min(rope.len_chars()));
                    let ts = Instant::now();
                    rope.insert(off, "x");
                    let idx = std::rc::Rc::new(crate::markdown::scan_blocks_incremental_rope(
                        &prev_rope,
                        &prev_idx,
                        &rope,
                        Some(off..off + 1),
                    ));
                    scans_ms += ts.elapsed().as_secs_f64() * 1000.0;
                    prev_rope = rope.clone();
                    prev_idx = idx.clone();
                    let src = crate::markdown::PreviewSrc::Rope(rope.clone());
                    m.set_source_with_index(src, idx, cx);
                    m.prepare_render(&p, 980.0, cx);
                }
            });
        });
        let wall = t0.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "[bench] 预览每键同步（窗口模式，{n} 轮，真实大文档，增量扫描+保留预览基底）：总 {}ms（{:.2}ms/轮）；其中消费端扫描（不含在预览内）{:.2}ms/轮；预览侧（set_source+prepare_render）{:.2}ms/轮",
            wall,
            wall / n as f64,
            scans_ms / n as f64,
            (wall - scans_ms) / n as f64,
        );
        // prof::dump 重新计算每标签总耗时（不含扫描），给出每轮均摊的精确拆账。
        crate::prof::dump("preview.edit.sync");
    }

    /// 基准（--ignored）：**合并决策的跨管线对比**——同一个大文档（真实 6.4MB，
    /// 窗口模式）上，`MarkdownPreview`（元素树 + ListState）vs `WyEditor` 只读等价
    /// 渲染（自绘 build_frame 带内整形）的「首帧 / 滚动重定位」成本。
    /// 目的（#4 统一文档层）：证明把预览迁到 WYSIWYG 自绘管线（`editable` 门控只读态）
    /// 后，只读查看的打开/滚动不劣于旧预览元素树；给合并的每一步提供 before/after 锚点。
    /// `scan_blocks_rope` 只作索引构建参照。打印、不断言；`--ignored` 运行。
    #[gpui::test]
    #[ignore]
    fn bench_preview_vs_wy_readonly(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::{WyEditor, build_frame};
        use std::time::Instant;
        crate::prof::enable();
        cx.update(crate::actions::bind_local_keymaps);
        let path = "fixtures/大文档测试2.md";
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[bench] skip realfile: {e}");
                return;
            }
        };
        let rope0 = ropey::Rope::from_str(&text);
        let t_scan = Instant::now();
        let idx0 = std::rc::Rc::new(crate::markdown::scan_blocks_rope(&rope0));
        let scan_ms = t_scan.elapsed().as_secs_f64() * 1000.0;

        struct DualHost {
            md: Entity<MarkdownPreview>,
            wy: Entity<WyEditor>,
        }
        impl Render for DualHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.md.clone()
            }
        }
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            // WYSIWYG 用同步装配（小文档/测试路径）：本基准只关心 build_frame 的
            // 整形/滚动成本，索引已由 `set_source_text` 现建，口径与预览同源。
            let wy = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            DualHost { md, wy }
        });
        let vcx: &mut VisualTestContext = vcx;
        let p = vcx.update(|_w, cx| Rc::new(StyleParams::from_theme_with_font(cx, None)));
        let wy = vcx.update(|_, cx| host.read(cx).wy.clone());
        // —— Preview（元素树）首窗：紧接 set_source（未 resize/未 settle），冷物化
        //    首个窗口 + 高度模型——否则 settle 期间真实渲染已建成缓存，测不到冷首帧。
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |m, cx| {
                m.set_source_with_index(crate::markdown::PreviewSrc::Rope(rope0), idx0, cx)
            });
        });
        let preview_first_ms = vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |m, cx| {
                let t0 = Instant::now();
                m.prepare_render(&p, 980.0, cx);
                t0.elapsed().as_secs_f64() * 1000.0
            })
        });
        vcx.simulate_resize(size(px(980.), px(760.)));
        settle(vcx);
        let n_items = vcx.update(|_, cx| host.read(cx).md.read(cx).item_count());
        assert!(
            n_items > crate::config::limits::WINDOWED_MIN_BLOCKS,
            "需窗口模式（块数 {n_items}）"
        );

        // —— Preview（元素树）滚动重定位——
        let n = 24usize;
        let t0 = Instant::now();
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |m, cx| {
                let total = m.total_h();
                for i in 0..n {
                    let target = total * (i as f32 + 1.0) / (n as f32 + 1.0);
                    m.desired_scroll.set(target);
                    m.prepare_render(&p, 980.0, cx);
                }
            });
        });
        let preview_scroll_ms = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;

        // —— WyEditor（自绘 build_frame）——
        let wy_first_ms = vcx.update(|window, cx| {
            // 首屏可视带（≈ 预览的初始窗口），不算全篇。
            let t0 = Instant::now();
            let _f = build_frame(wy.read(cx), window, px(980.), (0., 2048.));
            t0.elapsed().as_secs_f64() * 1000.0
        });
        let wy_full_ms = vcx.update(|window, cx| {
            // 全篇一次性整形（对照参考；预览不这样做）。
            let t0 = Instant::now();
            let f = build_frame(wy.read(cx), window, px(980.), (0., 999_999.));
            let total = f32::from(f.total);
            let _ = total;
            t0.elapsed().as_secs_f64() * 1000.0
        });
        let wy_scroll_ms = vcx.update(|window, cx| {
            let total = f32::from(build_frame(wy.read(cx), window, px(980.), (0., 999_999.)).total);
            let t0 = Instant::now();
            for i in 0..n {
                let lo = total * (i as f32 + 1.0) / (n as f32 + 2.0);
                let _f = build_frame(wy.read(cx), window, px(980.), (lo, lo + 1024.));
            }
            t0.elapsed().as_secs_f64() * 1000.0 / n as f64
        });

        eprintln!(
            "[bench] 只读查看对比（{} 块 / 窗口模式，真实大文档，{n} 步滚动）：\n  \
             索引构建(scan_blocks_rope) 参照 {scan_ms:.2}ms\n  \
             Preview(元素树): 首帧 {preview_first_ms:.2}ms、滚动重定位 {preview_scroll_ms:.2}ms/步\n  \
             WyEditor(自绘):  首屏带 {wy_first_ms:.2}ms、全篇整形 {wy_full_ms:.2}ms、滚动带 {wy_scroll_ms:.2}ms/步",
            n_items
        );
        crate::prof::dump("preview.vs.wy.readonly");
    }

    /// 基准（--ignored）：**新搬入功能（搜索跳转高亮）的逐帧代价 A/B**——同一注入帧，
    /// 无高亮 vs 有高亮各 `window.draw` N 帧，取差 = `paint_search_highlight` 的每帧
    /// 开销（仅命中时存在）。量化「合并 + 搬运」相对改动前多出的真实成本。`--ignored` 运行。
    #[gpui::test]
    #[ignore]
    fn bench_wy_search_highlight_paint_cost(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use std::time::Instant;
        cx.update(crate::actions::bind_local_keymaps);
        let text = "alpha beta 中文命中 beta again\nsecond line here\n";
        struct WyOnlyHost {
            ed: Entity<WyEditor>,
        }
        impl Render for WyOnlyHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.ed.clone()
            }
        }
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyOnlyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        // 不注入帧：让元素在绘制时自建（小文档很快），A/B 两侧完全同口径，
        // 差值只含 `paint_search_highlight`（命中分支）的增量。
        let n = 16usize;
        let t_no = vcx.update(|window, cx| {
            let t0 = Instant::now();
            for _ in 0..n {
                let _ = window.draw(cx);
            }
            t0.elapsed().as_secs_f64() * 1000.0 / n as f64
        });
        let t_hl = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| e.highlight_search(0, "beta", cx));
            let t0 = Instant::now();
            for _ in 0..n {
                let _ = window.draw(cx);
            }
            t0.elapsed().as_secs_f64() * 1000.0 / n as f64
        });
        eprintln!(
            "[bench] WyEditor 每帧绘制（{n} 帧均摊）：无高亮 {t_no:.4}ms、搜索高亮命中 {t_hl:.4}ms、增量 Δ{:.4}ms/帧",
            (t_hl - t_no).max(0.0)
        );
    }

    /// 基准（--ignored）：M5-B5 P3 前置「量程精确化」的决策依据——「全量 DocRow shape
    /// 量程一次」在 1 万 / 10 万块量级的真实开销。合成文档（段落块：约 64B/块，视口宽下
    /// 折成多视觉行），`Doc::open` + 全量物化 `rows_in_block_window` + Σ `shape_docrow_shard`
    /// 分三段计时；支撑「全量可承受（用全量+缓存） vs 需分段/抽样（对拍误差）」取舍。
    /// 打印、不断言；`--ignored` 运行。
    #[gpui::test]
    #[ignore]
    fn bench_docrow_shard_full_range_cost(cx: &mut TestAppContext) {
        use crate::markdown::layout::shape_docrow_shard;
        use crate::markdown::wysiwyg::WyTheme;
        use gpui::WindowTextSystem;
        use std::time::Instant;
        cx.update(crate::actions::bind_local_keymaps);
        let p = cx.update(|cx| StyleParams::from_theme_with_font(cx, None));
        let theme = WyTheme::from_style(&p, gpui::Font::default(), p.para_size);
        let ts = WindowTextSystem::new(cx.text_system().clone());
        let width = 980.0f32;
        let avail = (width - 2.0 * CONTENT_EDGE_PAD_X_PX).max(40.0);
        for &n_blocks in &[10_000usize, 100_000usize] {
            let mut src = String::with_capacity(n_blocks * 88);
            for i in 0..n_blocks {
                src.push_str(&format!(
                    "这是第 {i} 号段落：一段比较长的中文内容，用于让每块在视口宽度下折成多个视觉行以贴近真实场景。\n"
                ));
            }
            let src_len = src.len() / 1024;
            let t0 = Instant::now();
            let doc = crate::markdown::doc::Doc::open(src.into());
            let t_open = t0.elapsed().as_secs_f64() * 1000.0;
            let t1 = Instant::now();
            let rows = doc.rows_in_block_window(0, n_blocks - 1);
            let t_rows = t1.elapsed().as_secs_f64() * 1000.0;
            let t2 = Instant::now();
            let mut total_h = 0.0f32;
            let mut shard_n = 0.5f32;
            for dr in &rows {
                let shards = shape_docrow_shard(&ts, dr, &theme, avail, width);
                shard_n = shards.len() as f32;
                total_h += shards.iter().map(|s| s.h).sum::<f32>();
            }
            let t_shape = t2.elapsed().as_secs_f64() * 1000.0;
            let sum_ms = t_open + t_rows + t_shape;
            let avg = sum_ms / n_blocks as f64;
            eprintln!(
                "[bench] 全量 DocRow shape 量程：{n_blocks} 块（≈{src_len}KB 源）→ open {t_open:.1}ms / 物化 {t_rows:.1}ms / shape {t_shape:.1}ms / 合计 {sum_ms:.1}ms；平均每块 {avg:.3}ms（含物化）；Σh={total_h:.0}px avg≈{shard_n:.0}片/块",
            );
        }
    }

