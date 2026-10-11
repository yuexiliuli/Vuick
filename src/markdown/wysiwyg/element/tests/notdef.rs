#[gpui::test]
fn window_rows_cover_every_band_realfile(cx: &mut TestAppContext) {
    // 回归：滚动闪烁 / 首屏缺内容（GUI 实测：预览区滚轮后整屏空白、打开时下半屏
    // 无内容）。根因 = `build_frame` 的带窗口用「估算高」二分截断 [`find_band_range`]，
    // Pass2 把真实高 `set` 回 base 后，物化行底（真值坐标）可能远低于带高 `band_hi`
    // → 带的下一屏没有任何行可画（视口空白一整帧/持续到下一次重建）。本测试断言
    // **每个带的物化行（真值坐标）必须覆盖到 `band_hi`**（或带已越过文档末尾）——
    // 覆盖视口的前提。用真实大文档 + 模拟 prepaint 的 512px 带量程逐档驱动。
    use crate::markdown::wysiwyg::WyEditor;
    cx.update(crate::actions::bind_local_keymaps);
    let path = "fixtures/大文档测试2.md";
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[test] skip realfile: {e}");
            return;
        }
    };
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let width = px(980.);
    let vh = 720.0f32;
    let q = 512.0f32;
    // 逐档推进的视口顶（元素局部 y）：从顶部连续滚 + 滚到 30k px 数学/表格密集区。
    let mut tops: Vec<f32> = Vec::new();
    for k in 0..24 {
        tops.push(400.0 * (k + 1) as f32);
    }
    for k in 0..16 {
        tops.push(30_000.0 + 400.0 * (k + 1) as f32);
    }
    let mut processed = 0usize;
    for top in tops {
        let band = (
            (((top - vh) / q).floor() * q).max(0.),
            ((top + 2. * vh) / q).ceil() * q,
        );
        // build_frame 是纯函数，返回的这一帧正是 prepaint 将存给 paint 用的帧——
        // 直接断言它，而不是读编辑器里可能滞后的旧帧。
        let frame = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            build_frame(ed.read(cx), window, width, band)
        });
        let (blo, bhi) = band;
        let (max_bottom, total, rows) = {
            let mb = frame
                .rows
                .iter()
                .map(|r| r.top.as_f32() + r.height.as_f32())
                .fold(0.0f32, f32::max);
            (mb, frame.total.as_f32(), frame.rows.len())
        };
        processed += 1;
        // 带未越过文档末尾（total > band_hi）时，物化行底必须 ≥ band_hi（+折叠尾部
        // 容差）。这是「下一屏有行可画」= 视口不被空白穿帮的必要条件。
        let over_end = total <= bhi + 1.0;
        if !over_end {
            assert!(
                max_bottom + 32.0 >= bhi,
                "band={band:?} top={top:.0}: materialized rows end at {max_bottom:.0}, \
                     need >= {bhi:.0} (total={total:.0}, rows={rows}) — 改带下方没有行可画"
            );
        }
        // 行区间必须覆盖带中点（bin:lo + 尾 block 可达 hi，但中间不得有物化空洞）。
        let mid = (blo + bhi) / 2.0;
        let mid_ok = frame.rows.iter().any(|r| {
            let rb = r.top.as_f32();
            let re = rb + r.height.as_f32();
            rb <= mid && re >= mid
        });
        if !over_end && max_bottom >= bhi - 32.0 {
            assert!(
                mid_ok,
                "band={band:?} top={top:.0}: rows 未覆盖带中点 {mid:.0} (rows={rows})"
            );
        }
    }
    assert!(
        processed > 20,
        "应实际驱动了 >20 个带（processed={processed}）"
    );
}

#[test]
fn layout_has_notdef_flags_missing_glyph() {
    // `.notdef`（元字形，glyph id=0）＝ Windows 冷启动回退未命中的「豆腐块」。
    let mk = |ids: &[u32]| gpui::LineLayout {
        font_size: px(16.),
        width: px(10.),
        ascent: px(12.),
        descent: px(3.),
        len: ids.len(),
        runs: vec![gpui::ShapedRun {
            font_id: gpui::FontId(0),
            glyphs: ids
                .iter()
                .enumerate()
                .map(|(i, &id)| gpui::ShapedGlyph {
                    id: gpui::GlyphId(id),
                    position: point(px(i as f32), px(0.)),
                    index: i,
                    is_emoji: false,
                })
                .collect(),
        }],
    };
    assert!(!layout_has_notdef(&mk(&[])));
    assert!(!layout_has_notdef(&mk(&[1, 2, 3])));
    assert!(layout_has_notdef(&mk(&[1, 0, 3])), "含 glyph 0 应判 notdef");
    assert!(layout_has_notdef(&mk(&[0])));
}

#[gpui::test]
fn notdef_retry_state_machine(cx: &mut TestAppContext) {
    // 直接驱动 notdef 重试状态机（不依赖平台回退）：
    // - 未登记块 → notdef_settled=false、cooling_row=None（不打扰正常 pass）；
    // - cool_start 推进真实平台重排计数，达 NOTDEF_RETRY_ATTEMPTS 上限返回「给出」
    //   并置 settled（判定确无字形 → 之后命中含 notdef 的缓存不再当 miss 重排）；
    // - clear_notdef 后 settled 清空、恢复原态。
    use crate::markdown::wysiwyg::NOTDEF_RETRY_ATTEMPTS;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let settled = |vcx: &mut VisualTestContext, block: usize| -> bool {
        vcx.update(|_, cx| host.read(cx).ed.read(cx).notdef_settled(block))
    };
    assert!(!settled(vcx, 3), "未登记块不应 settled");
    assert!(
        vcx.update(|_, cx| host.read(cx).ed.read(cx).cooling_row(3))
            .is_none(),
        "未登记块不应处于冷却"
    );
    // 连打坏重排：第 k 次返回「未给出」（k < 上限）、第 N 次返回「给出」并 settled。
    let mut gave_up_at = None;
    for k in 1..=NOTDEF_RETRY_ATTEMPTS {
        let ok = vcx.update(|_, cx| host.read(cx).ed.read(cx).cool_start(3));
        if ok && gave_up_at.is_none() {
            gave_up_at = Some(k);
        }
    }
    assert_eq!(
        gave_up_at,
        Some(NOTDEF_RETRY_ATTEMPTS),
        "恰在上限次给出（长期坏块不再空转）"
    );
    assert!(settled(vcx, 3), "给出后应标记 settled（确无字形）");
    // 单次登记（未达上限）：应不给出；clear 后 settled 清空。
    assert!(
        !vcx.update(|_, cx| host.read(cx).ed.read(cx).cool_start(7)),
        "未达上限不应给出"
    );
    vcx.update(|_, cx| host.read(cx).ed.read(cx).clear_notdef(7));
    assert!(!settled(vcx, 7), "clear 后应清 settled");
}

#[gpui::test]
fn stale_verdicts_cleared_on_generation_change(cx: &mut TestAppContext) {
    // 换代自愈语义：图片解码完成触发 `igen` 换代重建时，若块在上代（DWrite 退化期）
    // 被误判 settled（预算烧光）/ verified（非零占位字形），`clear_stale_verdicts`
    // 应作废这些上一代判决，让本块按新代重新校验/重试——不再整次运行永久豆腐块。
    use crate::markdown::wysiwyg::NOTDEF_RETRY_ATTEMPTS;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let settled = |vcx: &mut VisualTestContext, b: usize| -> bool {
        vcx.update(|_, cx| host.read(cx).ed.read(cx).notdef_settled(b))
    };
    let verified = |vcx: &mut VisualTestContext, b: usize| -> bool {
        vcx.update(|_, cx| host.read(cx).ed.read(cx).fallback_verified(b))
    };
    // 构造"上一代判决"：块 5 被置 verified；块 7 被置 settled（NOTDEF_RETRY_ATTEMPTS
    // 次重排仍坏，值为常数 8，勿在测试里硬编码具体次数）。
    vcx.update(|_, cx| host.read(cx).ed.read(cx).mark_fallback_verified(5));
    for _ in 0..NOTDEF_RETRY_ATTEMPTS {
        vcx.update(|_, cx| {
            host.read(cx).ed.read(cx).cool_start(7);
        });
    }
    assert!(verified(vcx, 5), "上代：fallback 已校验");
    assert!(settled(vcx, 7), "上代：notdef 已确无字形");
    // 换代：逐块作废，重置后恢复"未判决"。
    vcx.update(|_, cx| {
        host.read(cx).ed.read(cx).clear_stale_verdicts(5);
        host.read(cx).ed.read(cx).clear_stale_verdicts(7);
    });
    assert!(!verified(vcx, 5), "换代后重新校验");
    assert!(!settled(vcx, 7), "换代后重新重试");
}

#[gpui::test]
#[ignore]
fn bench_wysiwyg_cold_open_realfile(cx: &mut TestAppContext) {
    // A-4 首帧空白诊断：真实 6.4MB 文档冷开逐段墙钟 + KILO_PROF 拆账。拆 4 段：
    // ① Doc::open 全量解析（scan_blocks 6.4MB→16 万块）② WyEditor+with_text（load：
    // set_source_text = 再解析 + rebuild_block_regions + preseed_image_sizes 逐图读尺寸）
    // ③ 首帧真实 element 管线（measure→prepaint→paint，冷 pass1+layout+带整形+warmemoji）
    // ④ 后续同带 build_frame（应命中缓存）。定位 10-30s 首帧空白的真实大头。
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let path = "fixtures/大文档测试2.md";
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[bench] skip realfile: {e}");
            return;
        }
    };
    // ① Doc::open 解析拆账。
    let t0 = std::time::Instant::now();
    let doc = crate::markdown::doc::Doc::open(gpui::SharedString::from(&*text));
    let _ = doc.line_count();
    let doc_open = t0.elapsed().as_secs_f64() * 1000.0;
    // ② WyEditor 构造（= load 路径；with_text 内部再走一遍 set_source_text）。
    let t0 = std::time::Instant::now();
    let (host, _vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let loader = t0.elapsed().as_secs_f64() * 1000.0;
    // ③ 首帧真实 element 管线（不放 vcx 借用，用外层 cx 驱动 refresh）。
    let t0 = std::time::Instant::now();
    for _ in 0..3 {
        let _ = cx.refresh();
        cx.run_until_parked();
    }
    let elem_first = t0.elapsed().as_secs_f64() * 1000.0 / 3.0;
    crate::prof::dump("wy.cold.open");
    // ④ 后续同带 build_frame（命中缓存）。从窗口重建 vcx。
    let win = cx.windows()[0];
    let mut vcx = VisualTestContext::from_window(win, cx);
    let t0 = std::time::Instant::now();
    let f0 = vcx.update(|window, cx: &mut App| {
        build_frame(host.read(cx).ed.read(cx), window, px(980.), (0., 1600.))
    });
    let _ = f0.rows.len();
    let warm_bf = t0.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "[bench] cold-open: Doc::open={doc_open:.1}ms, with_text(load)={loader:.1}ms, 首3帧element={elem_first:.1}ms/帧, 同带bf={warm_bf:.3}ms/帧"
    );
}

#[gpui::test]
#[ignore]
fn bench_doc_open_internal_distribution(cx: &mut TestAppContext) {
    // A 区残余结构性优化：headless 拆 Doc::open + set_source_text 内部逐段墙钟
    // （scan_blocks 内部分布：line_starts 行内段 / split / classify 构建；Doc::open 其余：
    // 二次 line_starts / Rope / 字符计数+UTF-16；set_source_text 的 marker_cnt / regions）。
    // 并测新后台 load 路径的 **UI 线程成本**（`install_open` 装配，O(块) 级别）——
    // 证明整篇 O(n) 解析已挪出 UI 线程。
    // 跑法：cargo test --release --bin gpui-editor bench_doc_open_internal_distribution -- --ignored --nocapture，KILO_PROF=1 环境变量。
    let path = "fixtures/大文档测试2.md";
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[bench] skip realfile: {e}");
            return;
        }
    };
    let t0 = std::time::Instant::now();
    let _doc = crate::markdown::doc::Doc::open(gpui::SharedString::from(&*text));
    let doc_open = t0.elapsed().as_secs_f64() * 1000.0;
    crate::prof::dump("doc.open");
    // set_source_text 的额外段（doc 已完成，只剩 marker 计数 + regions 重建 + 帧作废）。
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| crate::markdown::wysiwyg::WyEditor::new(window, cx));
        WyHost { ed }
    });
    let t0 = std::time::Instant::now();
    vcx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.set_source_text(&text))
    });
    let set_src = t0.elapsed().as_secs_f64() * 1000.0;
    crate::prof::dump("set_source_text");
    // 新后台 load 路径拆账：后台全量（open_from_rope） vs UI 线程装配（install_open）。
    let rope = ropey::Rope::from_str(&text);
    let t0 = std::time::Instant::now();
    let r = crate::markdown::wysiwyg::WyEditor::open_from_rope(rope);
    let bg_parse = t0.elapsed().as_secs_f64() * 1000.0;
    let (host2, vcx2) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| crate::markdown::wysiwyg::WyEditor::new(window, cx));
        WyHost { ed }
    });
    let t0 = std::time::Instant::now();
    vcx2.update(|_, cx| {
        let ed = host2.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.install_open(r));
    });
    let install = t0.elapsed().as_secs_f64() * 1000.0;
    crate::prof::dump("install_open");
    eprintln!(
        "[bench] Doc::open={doc_open:.1}ms, set_source_text(extra)={set_src:.1}ms, 后台open_from_rope={bg_parse:.1}ms, **UI装配install_open={install:.3}ms**"
    );
}

