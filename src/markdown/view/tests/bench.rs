/// 基准：代码块/表格句柄地图「每帧整张 HashMap 克隆 vs Rc 共享」的代价对比。
/// 旧代码在 `render()` 里逐帧 `code_scroll/table_scroll/code_hl.clone()`（O(N) 深拷贝），
/// 新逻辑克隆 `Rc`（O(1) 引用计数）。打印、不构成断言；`--ignored` 运行。
#[test]
#[ignore]
fn bench_block_extras_clone_vs_rc_share() {
    use std::time::Instant;
    use std::collections::HashMap;
    use std::rc::Rc;
    let mut m: HashMap<usize, u64> = HashMap::new();
    for i in 0..1000 {
        m.insert(i, i as u64);
    }
    let shared = Rc::new(m.clone());
    let runs = 2_000_000usize;
    let t0 = Instant::now();
    let mut sink = 0u64;
    for i in 0..runs {
        let c = m.clone();
        sink = sink.wrapping_add(c.get(&(i % 1000)).copied().unwrap_or(0));
    }
    let cloned = t0.elapsed();
    let t1 = Instant::now();
    let mut sink2 = 0u64;
    for i in 0..runs {
        let c = shared.clone();
        sink2 = sink2.wrapping_add(c.get(&(i % 1000)).copied().unwrap_or(0));
    }
    let rc_share = t1.elapsed();
    assert_eq!(sink, sink2, "两种取数方式结果必须一致");
    println!(
        "1000 项句柄地图共享（{runs} 次）：每帧整张克隆={}ms, Rc 共享={}ms（提升 {:.1}x）",
        cloned.as_millis(),
        rc_share.as_millis(),
        cloned.as_secs_f64() / rc_share.as_secs_f64().max(1e-9),
    );
}

/// 基准（--ignored）：窗口模式滚动时，代码块语法高亮 LRU 缓存 + 自适应过扫的效果。
///
/// 方法：合成「代码块散布整篇」的大文档（窗口级懒解析），在文档中**滚动 200 步**
/// （每步前进 6 块、可见带 20 块），驱动 `ensure_window`：
/// - **冷**（旧行为）：每次重建后清空 LRU → 每窗所有代码块都重跑 syntect；
/// - **热**（新）：保留 LRU → 重叠代码块复用 `Rc`，只有新进窗的块才解析；
/// - **过扫重建数**：固定余量 24 vs 大余量 100（模拟自适应高视口）各自的窗口重建次数。
/// 打印、不断言；`--ignored` 运行。
#[gpui::test]
#[ignore]
fn bench_windowed_scroll_highlight_cache(cx: &mut TestAppContext) {
    use std::time::Instant;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;

    let block = "```rust\nfn main() {\n    let s = \"str\";\n    // 注释行\n    let n = s.len();\n    println!(\"{n}\");\n}\n```\n\n正文段 `code` 与[链接](https://x)\n\n";
    let mut src = String::new();
    for _ in 0..1500 {
        src.push_str(block);
    }
    let index = std::rc::Rc::new(crate::markdown::scan_blocks(&src));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(PreviewSrc::from(SharedString::from(src)), index, cx)
        });
    });
    cx.simulate_resize(size(px(700.), px(700.)));
    settle(cx);
    let n_items = cx.update(|_, cx| host.read(cx).md.read(cx).item_count());
    assert!(n_items > crate::config::limits::WINDOWED_MIN_BLOCKS);

    let steps = 200usize;
    let step = 6usize;
    let vis = 20usize;

    // —— 冷（旧）：每窗全量重解析 ——
    let t0 = Instant::now();
    let cold_rebuilds = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, _| {
            md.overdraw_margin = 24;
            let mut rebuilds = 0usize;
            let mut last = md.win_lo;
            for s in 0..steps {
                let lo = s * step;
                md.ensure_window(lo, lo + vis);
                if md.win_lo != last {
                    rebuilds += 1;
                    last = md.win_lo;
                }
                md.code_hl_cache.clear(); // 旧行为：无缓存
            }
            rebuilds
        })
    });
    let cold_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // —— 热（新）：LRU 复用 ——
    let t0 = Instant::now();
    let warm_rebuilds = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, _| {
            md.overdraw_margin = 24;
            let mut rebuilds = 0usize;
            let mut last = md.win_lo;
            for s in 0..steps {
                let lo = s * step;
                md.ensure_window(lo, lo + vis);
                if md.win_lo != last {
                    rebuilds += 1;
                    last = md.win_lo;
                }
            }
            rebuilds
        })
    });
    let warm_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let cache_len = cx.update(|_, cx| host.read(cx).md.read(cx).code_hl_cache.len());

    // —— 过扫：固定 24 vs 大余量 100（自适应高视口），重建次数对比 ——
    let rebuilds_m24 = run_scroll_rebuild_count(cx, &host, 24, steps, step, vis);
    let rebuilds_m100 = run_scroll_rebuild_count(cx, &host, 100, steps, step, vis);
    let adaptive_for_200 = super::adaptive_overdraw(200);

    println!(
        "窗口滚动 {} 步（步进 {step} 块、可见 {vis} 块，{} 块文档）：高亮冷(旧,无缓存)={cold_ms:.1} ms（{cold_rebuilds} 次重建） vs 热(新,LRU)={warm_ms:.1} ms（{warm_rebuilds} 次重建） 提升 {:.2}x；LRU 驻留 {} 块；过扫重建数 固定24={rebuilds_m24} vs 余量100={rebuilds_m100}（自适应视口200块→{adaptive_for_200}）",
        steps,
        n_items,
        cold_ms / warm_ms.max(1e-9),
        cache_len,
    );
}

/// 以给定过扫余量跑一遍向前滚动，返回实际窗口重建次数（`win_lo` 变化次数）。
fn run_scroll_rebuild_count(
    cx: &mut VisualTestContext,
    host: &gpui::Entity<PreviewHost>,
    margin: usize,
    steps: usize,
    step: usize,
    vis: usize,
) -> usize {
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, _| {
            md.overdraw_margin = margin;
            let mut rebuilds = 0usize;
            let mut last = md.win_lo;
            for s in 0..steps {
                let lo = s * step;
                md.ensure_window(lo, lo + vis);
                if md.win_lo != last {
                    rebuilds += 1;
                    last = md.win_lo;
                }
            }
            rebuilds
        })
    })
}

/// 基准（--ignored）：#3 统一高度提示的 O(块数) SumTree 物化成本。仿真大文档量级
/// （16 万块）：ListState::new 与 with_uniform_item_height 各建一遍节点树，再模拟
/// 连续换宽（每次重建）10 次。打印、不断言。
#[gpui::test]
#[ignore]
fn bench_list_uniform_height_cost(cx: &mut TestAppContext) {
    use std::time::Instant;
    let (new_ms, hint_ms, x10_ms, remeasure_ms, remeasure_win_ms) = cx.update(|_cx| {
        use gpui::{ListAlignment, ListState, px};
        let n = 160_000usize;
        let t = Instant::now();
        let state = ListState::new(n, ListAlignment::Top, px(12.0));
        let new_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let state_hinted = state.clone().with_uniform_item_height(px(24.0));
        let hint_ms = t.elapsed().as_secs_f64() * 1000.0;
        // 模拟连续换宽：每次 with_uniform 全量重建
        let t = Instant::now();
        for w in 0..10 {
            let _ = state_hinted.clone().with_uniform_item_height(px(20.0 + w as f32));
        }
        let x10_ms = t.elapsed().as_secs_f64() * 1000.0;
        // 同块数换内容时的 remeasure（不重建树，仅置脏可见区）
        let t = Instant::now();
        for _ in 0..10 {
            state_hinted.remeasure();
        }
        let remeasure_ms = t.elapsed().as_secs_f64() * 1000.0;
        // 优化后：打字路径只 remeasure 可见窗口（~60 行）而非全量
        let t = Instant::now();
        for _ in 0..10 {
            state_hinted.remeasure_items(50..110);
        }
        let remeasure_win_ms = t.elapsed().as_secs_f64() * 1000.0;
        (new_ms, hint_ms, x10_ms, remeasure_ms, remeasure_win_ms)
    });
    println!(
        "16 万块 ListState：new={new_ms:.1} ms；with_uniform_item_height={hint_ms:.1} ms（+new ≈ 打开时两次数节点分配）；连续换宽 ×10（每次重建）={x10_ms:.1} ms；打字同块数全量 remeasure ×10={remeasure_ms:.1} ms vs 只重测可见窗口 ×10={remeasure_win_ms:.1} ms（{:.0}x）",
        remeasure_ms / remeasure_win_ms.max(1e-6),
    );
    let _ = cx;
}

/// 基准（--ignored）：预览纵向滚动条**拖拽每步成本**（真机「预览拖滚动条明显滞后，
/// 编辑器正常」的定位用）。两种模式各量：小文档（混合行型、真实测量高度）与窗口
/// 模式（fixtures/大文档测试2.md）。每步 = 快照句柄 set_offset 拖一个 pct + 一帧
/// 完整 render/layout/prepaint（cx.refresh），量 K 步均摊墙钟（release 为准）。
/// 打印、不断言。
#[gpui::test]
#[ignore]
fn bench_preview_scrollbar_drag_step_cost(cx: &mut TestAppContext) {
    use std::time::Instant;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let vcx: &mut VisualTestContext = vcx;

    // —— 小文档：混合行型（行高差异大 → 真实测量 ≠ 统一提示）——
    let mut src = String::new();
    for i in 0..60 {
        src.push_str(&format!(
            "# 标题 {i} 内容内容内容内容内容\n\n普通段落行 {i} 内容内容内容内容内容内容\n\n"
        ));
        src.push_str(&format!("- 列表项 {i} 内容内容内容内容\n\n"));
        src.push_str(&format!("> 引述 {i} 内容内容内容内容内容内容内容\n\n"));
        if i % 6 == 0 {
            src.push_str(&format!(
                "```rs\nlet x = {i};\nfn f() {{ g() }}\n```\n\n"
            ));
        }
    }
    vcx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(&src, cx));
    });
    vcx.simulate_resize(size(px(980.), px(760.)));
    settle(vcx);

    let small = vcx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let m = md.read(cx);
        (m.item_count(), m.index.is_some())
    });
    eprintln!("[drag-bench] small: items={} windowed={}", small.0, small.1);
    let steps_n = 120usize;
    let mut t_set = 0.0f64;
    let mut t_pp = 0.0f64;
    let mut t_frame = 0.0f64;
    let p_small = vcx.update(|_w, cx| Rc::new(StyleParams::from_theme_with_font(cx, None)));
    for k in 1..=steps_n {
        let pct = k as f32 / (steps_n as f32 + 1.0);
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, _cx| {
                let list = md.list_state.clone();
                let view = list.viewport_bounds();
                let handle = PreviewVerticalScrollHandle {
                    list,
                    content_width: 0.0,
                    content_height: md.total_h(),
                    est_hint: md.est_hint,
                    n: md.item_count(),
                    windowed: md.index.is_some(),
                    desired: md.desired_scroll.clone(),
                };
                let c = handle.content_size().height.as_f32();
                let target = -((c - view.size.height.as_f32()) * pct);
                let t = Instant::now();
                handle.set_offset(point(px(0.), px(target)));
                t_set += t.elapsed().as_secs_f64() * 1000.0;
                let t = Instant::now();
                md.prepare_render(&p_small, 980.0, _cx);
                t_pp += t.elapsed().as_secs_f64() * 1000.0;
                _cx.notify();
            });
        });
        let t = Instant::now();
        vcx.run_until_parked();
        let _ = vcx.cx.refresh();
        vcx.run_until_parked();
        t_frame += t.elapsed().as_secs_f64() * 1000.0;
    }
    eprintln!(
        "[drag-bench] small-doc: set_offset 均摊 {:.3} ms/步 | prepare_render 均摊 {:.3} ms/步 | 整帧(含布局) 均摊 {:.3} ms/步",
        t_set / steps_n as f64,
        t_pp / steps_n as f64,
        t_frame / steps_n as f64
    );

    // —— 窗口模式（真实大文档，存在才测）——
    let path = "fixtures/大文档测试2.md";
    if let Ok(text) = std::fs::read_to_string(path) {
        let rope0 = ropey::Rope::from_str(&text);
        let idx0 = std::rc::Rc::new(crate::markdown::scan_blocks_rope(&rope0));
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.set_source_with_index(
                    crate::markdown::PreviewSrc::Rope(rope0),
                    idx0.clone(),
                    cx,
                );
            });
        });
        vcx.simulate_resize(size(px(980.), px(760.)));
        for _ in 0..6 {
            vcx.run_until_parked();
            let _ = vcx.cx.refresh();
        }
        let win = vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            let m = md.read(cx);
            (m.item_count(), m.index.is_some())
        });
        eprintln!("[drag-bench] windowed: items={} windowed={}", win.0, win.1);
        let p_win = vcx.update(|_w, cx| Rc::new(StyleParams::from_theme_with_font(cx, None)));
        let mut t_set = 0.0f64;
        let mut t_pp = 0.0f64;
        let mut t_draw = 0.0f64;
        for k in 1..=steps_n {
            let pct = k as f32 / (steps_n as f32 + 1.0);
            vcx.update(|_, cx| {
                let md = host.read(cx).md.clone();
                md.update(cx, |md, _cx| {
                    let list = md.list_state.clone();
                    let view = list.viewport_bounds();
                    let handle = PreviewVerticalScrollHandle {
                        list,
                        content_width: 0.0,
                        content_height: md.total_h(),
                        est_hint: md.est_hint,
                        n: md.item_count(),
                        windowed: md.index.is_some(),
                        desired: md.desired_scroll.clone(),
                    };
                    let c = handle.content_size().height.as_f32();
                    let target = -((c - view.size.height.as_f32()) * pct);
                    let t = Instant::now();
                    handle.set_offset(point(px(0.), px(target)));
                    t_set += t.elapsed().as_secs_f64() * 1000.0;
                    let t = Instant::now();
                    md.prepare_render(&p_win, 980.0, _cx);
                    t_pp += t.elapsed().as_secs_f64() * 1000.0;
                    _cx.notify();
                });
            });
            // 第一次 park 消费 notify → 走完整 render/layout/prepaint/paint（整帧）。
            let t = Instant::now();
            vcx.run_until_parked();
            t_draw += t.elapsed().as_secs_f64() * 1000.0;
            let _ = vcx.cx.refresh();
            vcx.run_until_parked();
        }
        eprintln!(
            "[drag-bench] windowed: set_offset 均摊 {:.3} ms/步 | prepare_render 均摊 {:.3} ms/步 | 帧(含布局/绘制，first park) 均摊 {:.3} ms/步",
            t_set / steps_n as f64,
            t_pp / steps_n as f64,
            t_draw / steps_n as f64
        );

        // —— 拆账：窗口模式拖动帧的成本 = apply_desired(重建 ListState) + ensure_window(解析窗口行)？
        let (mut t_app, mut t_win, app_cnt, win_cnt) = vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            let mut t_app = 0.0f64;
            let mut t_win = 0.0f64;
            let mut app_cnt = 0u32;
            let mut win_cnt = 0u32;
            md.update(cx, |md, _cx| {
                let total = md.total_h();
                let view = md.list_state.viewport_bounds().size.height.as_f32();
                let n = md.item_count();
                for k in 1..=steps_n {
                    let pct = k as f32 / (steps_n as f32 + 1.0);
                    let target = (total - view).max(0.0) * pct;
                    let t = Instant::now();
                    md.desired_scroll.set(target);
                    md.restore_zoom_anchor();
                    md.apply_desired(md.desired_scroll.get());
                    t_app += t.elapsed().as_secs_f64() * 1000.0;
                    app_cnt += 1;
                    // 与 prepare_render 同口径的窗口区间
                    let (lo_vis, _off) = md.abs_scroll_top();
                    let hint = md.est_hint.max(20.0);
                    let vis_base = ((view / hint).ceil() as usize).max(1);
                    let overdraw = adaptive_overdraw(vis_base);
                    let vis_n = vis_base + overdraw;
                    let last = n.saturating_sub(1);
                    let need_lo = lo_vis.saturating_sub(overdraw);
                    let need_hi = lo_vis.saturating_add(vis_n).min(last);
                    let t = Instant::now();
                    md.ensure_window(need_lo, need_hi);
                    t_win += t.elapsed().as_secs_f64() * 1000.0;
                    win_cnt += 1;
                }
            });
            (t_app, t_win, app_cnt, win_cnt)
        });
        eprintln!(
            "[drag-bench] windowed 拆账: apply_desired(重定位) 均摊 {:.3} ms/步 ×{app_cnt} | ensure_window+bump(解析窗口) 均摊 {:.3} ms/步 ×{win_cnt}",
            t_app / app_cnt as f64,
            t_win / win_cnt as f64
        );
        let _ = &mut t_app;
        let _ = &mut t_win;

        // —— 拆账 2：dispatch_pending_code_hl（延后补色）单步成本——
        let (t_disp, disp_cnt) = vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            let mut t_disp = 0.0f64;
            let mut disp_cnt = 0u32;
            md.update(cx, |md, cx| {
                for _k in 1..=steps_n {
                    let t = Instant::now();
                    md.dispatch_pending_code_hl(cx);
                    t_disp += t.elapsed().as_secs_f64() * 1000.0;
                    disp_cnt += 1;
                }
            });
            (t_disp, disp_cnt)
        });
        eprintln!(
            "[drag-bench] windowed dispatch_pending_code_hl 均摊 {:.3} ms/步 ×{disp_cnt}",
            t_disp / disp_cnt as f64
        );

        // —— 慢速拖拽（相邻窗口重叠 → 增量窗口生效）——
        let (t_slow, slow_cnt) = vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            let p = Rc::new(StyleParams::from_theme_with_font(cx, md.read(cx).font_override));
            let mut t_slow = 0.0f64;
            let mut slow_cnt = 0u32;
            md.update(cx, |md, cx| {
                let total = md.total_h();
                let view = md.list_state.viewport_bounds().size.height.as_f32();
                let hint = md.est_hint.max(20.0);
                // 每次前移约 30 块（贴近真实慢拖）：窗口高度 ~112 块，必然相邻重叠。
                let step_px = 30.0 * hint;
                let mut target = 0.0f32;
                md.desired_scroll.set(0.0);
                md.prepare_render(&p, 980.0, cx);
                while target < (total - view).max(0.0) {
                    target += step_px;
                    let t = Instant::now();
                    md.desired_scroll.set(target);
                    md.restore_zoom_anchor();
                    md.apply_desired(md.desired_scroll.get());
                    md.prepare_render(&p, 980.0, cx);
                    t_slow += t.elapsed().as_secs_f64() * 1000.0;
                    slow_cnt += 1;
                }
            });
            (t_slow, slow_cnt)
        });
        eprintln!(
            "[drag-bench] windowed 慢速拖拽(Δ≈30块): prepare_render 均摊 {:.3} ms/步 ×{slow_cnt}",
            t_slow / slow_cnt as f64
        );

        // —— 拆账 3：parse_block_window_rope 增量带（Δ40）vs 全窗（Δ150）——
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                let Some(src) = md.src_buf.clone() else { return };
                let Some(idx) = md.index.clone() else { return };
                let mut t_d40 = 0.0f64;
                let mut t_d150 = 0.0f64;
                let lo0 = md.win_lo.min(idx.blocks.len().saturating_sub(150));
                for k in 0..60usize {
                    let base = (idx.blocks.len() as f32 * (k as f32 / 60.0)) as usize;
                    let t = Instant::now();
                    let _ = match &src {
                        PreviewSrc::Str(s) => crate::markdown::parse::parse_block_window(s, &idx, base, base + 40),
                        PreviewSrc::Rope(r) => crate::markdown::parse::parse_block_window_rope(r, &idx, base, base + 40),
                    };
                    t_d40 += t.elapsed().as_secs_f64() * 1000.0;
                    let t = Instant::now();
                    let _ = match &src {
                        PreviewSrc::Str(s) => crate::markdown::parse::parse_block_window(s, &idx, base, base + 150),
                        PreviewSrc::Rope(r) => crate::markdown::parse::parse_block_window_rope(r, &idx, base, base + 150),
                    };
                    t_d150 += t.elapsed().as_secs_f64() * 1000.0;
                }
                let _ = lo0;
                eprintln!(
                    "[drag-bench] parse_block_window_rope 均摊：Δ40={:.3} ms | Δ150={:.3} ms",
                    t_d40 / 60.0,
                    t_d150 / 60.0
                );
                let _ = cx;
            });
        });
    } else {
        eprintln!("[drag-bench] skip windowed: 缺 {path}");
    }
}

