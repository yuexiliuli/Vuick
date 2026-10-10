/// 回归：竖滚动条量程在首次渲染后由「平均行高 × 行数」铺设到接近真实总高，
/// 而不是只算可见行（否则滚动条一开始拖动不了全文）。这是确定性断言：
/// `update_height_hint` 在无布局（summary=0）时会把统一高度提示填满整列表。
#[gpui::test]
fn uniform_hint_seeds_full_scroll_range(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let mut md = cx.update(MarkdownPreview::new);
    let mut src = String::new();
    // 多种行型混合 + 一个大代码块：若平均行高漏算代码块，量程会严重偏小
    src.push_str("```rs\n");
    for i in 0..200 {
        src.push_str(&format!("let x = {i};\n"));
    }
    src.push_str("```\n\n");
    for i in 0..3000 {
        src.push_str(&match i % 5 {
            0 => format!("## 标题 {i} 内容内容内容内容内容内容内容内容内容内容内容内容\n"),
            1 => format!("- 列表项 {i} 内容内容内容内容内容内容内容内容内容内容内容内容\n"),
            2 => format!("普通段落行 {i} 内容内容内容内容内容内容内容内容内容内容内容内容\n"),
            3 => format!("> 引述内容 {i} 内容内容内容内容内容内容内容内容内容内容\n"),
            _ => format!("正文 {i}\n"),
        });
    }
    cx.update(|cx| md.set_source(&src, cx));
    let (n, hint, max_off) = cx.update(|cx| {
        let changed = md.update_height_hint(cx, 490.0);
        assert!(changed, "首次无布局（summary=0）时应触发一次重填");
        (
            md.rows.len(),
            md.est_hint,
            md.list_state.max_offset_for_scrollbar().y.as_f32(),
        )
    });
    assert!(n > 0, "应有渲染行");
    assert!(hint > 20.0, "平均行高应超过单行正文高，实际={hint:.1}");
    // 量程 = 行数 × 平均行高（尚未布局，无视口扣除）；应接近估算总高
    let expected = n as f32 * hint;
    assert!(
        (max_off - expected).abs() < expected * 0.05,
        "滚动条量程应接近 行数×平均行高：max_off={max_off:.0} expected={expected:.0} hint={hint:.1}"
    );
}

/// 回归：#3 换宽节流——宽度连续变化（拖分屏）时，O(块数) 的统一高度重建至多每
/// [`HINT_REAPPLY_INTERVAL_MS`] 一次，拖动期间不会每帧卡一下。首次建立不节流。
#[gpui::test]
fn hint_reapply_throttle_on_rapid_width_change(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let mut md = cx.update(MarkdownPreview::new);
    // 换行（wrap）文档：不同视口宽 → 折行数不同 → 平均行高不同 → 每次换宽都要重建提示
    let mut src = String::new();
    for i in 0..1200 {
        src.push_str(&format!("# 标题 {i} 内容内容内容内容内容内容内容内容内容内容\n正文段落 {i}\n\n"));
    }
    cx.update(|cx| md.set_source(&src, cx));

    let first = cx.update(|cx| md.update_height_hint(cx, 480.0));
    assert!(first, "首次（无提示记录）应立即建立，不节流");
    let mut applied = 0u8;
    for w in [481.0, 483.0, 486.0, 490.0, 495.0] {
        applied += cx.update(|cx| md.update_height_hint(cx, w)) as u8;
    }
    assert!(
        applied <= 1,
        "120ms 内连续换宽至多重建 1 次（其余被节流），实际 {applied}"
    );
}

/// 回归：#3 打字路径——源更新（块数不变）时只重测**可见窗口内**的行高，不再全量
/// remeasure 整棵 O(块数) SumTree。断言：块数不变时 ListState 的 item_count 不变、
/// 统一高度提示未被破坏（量程仍在），且窗口外未物化行保持占位。
#[gpui::test]
fn source_reapply_same_count_only_remeasures_window(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let block = "```rust\nfn f() {}\n```\n\n正文 `code`\n\n";
    let mut src = String::new();
    for _ in 0..1200 {
        src.push_str(block);
    }
    let index = std::rc::Rc::new(crate::markdown::scan_blocks(&src));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(PreviewSrc::from(SharedString::from(src.clone())), index, cx)
        });
    });
    cx.simulate_resize(size(px(600.), px(600.)));
    settle(cx);
    let n0 = cx.update(|_, cx| host.read(cx).md.read(cx).item_count());

    // 块数不变的小编辑（改一个字符再同步）
    let mut src2 = src.clone();
    src2 = src2.replacen("正文 `code`", "正文！`code`", 1);
    let index2 = std::rc::Rc::new(crate::markdown::scan_blocks(&src2));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(PreviewSrc::from(SharedString::from(src2)), index2, cx);
            cx.notify();
        });
    });
    for _ in 0..4 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }

    let (n1, win_len) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        (md.item_count(), md.win_rows.len())
    });
    assert_eq!(n1, n0, "块数不变时列表项数应保持不变（不再整棵重建）");
    assert!(
        win_len > 0 && win_len <= 200,
        "窗口行数应保持有界（win_len={win_len}）"
    );
    cx.simulate_resize(size(px(600.), px(600.)));
    settle(cx);
}

/// 大文档（块数 > WINDOWED_MIN_BLOCKS）必须走「窗口级懒解析」：
/// - 不物化整篇（win_rows 远小于块数、memory_estimate 有界）
/// - 渲染不 panic、可见窗口内有真实行
/// - 目录齐全、row_for_line 基于块索引可查、滚动到深处会移动窗口
#[gpui::test]
fn large_doc_uses_windowed_lazy_parse(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 每轮迭代 8 块：Heading/Blank/Paragraph/Blank/Bullet/Blank/Code/Blank
    let iters = WINDOWED_MIN_BLOCKS / 2 + 400;
    let mut src = String::new();
    for i in 0..iters {
        src.push_str(&format!("# 标题{i}\n\n正文段落 {i}\n\n- item\n\n```rs\nlet a = {i};\n```\n\n"));
    }
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(&src, cx));
    });
    cx.simulate_resize(size(px(400.), px(600.)));
    settle(cx);

    let (n_items, win_lo, win_len, mem, mid_row, toc_n) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        (
            md.item_count(),
            md.win_lo,
            md.win_rows.len(),
            md.memory_estimate(),
            md.row_for_line(src.lines().count() / 2),
            md.toc.len(),
        )
    });
    assert!(
        n_items > WINDOWED_MIN_BLOCKS,
        "应启用窗口模式（块数={n_items} 应 > 阈值）"
    );
    assert!(win_len > 0, "渲染后应已物化窗口行");
    assert!(
        win_len < 200,
        "窗口行数应远小于全文（win_len={win_len}，全文 {n_items}）"
    );
    assert!(
        win_lo < 30,
        "初始窗口应从顶部附近开始，win_lo={win_lo}"
    );
    assert!(
        mem.1 < 1_000_000,
        "窗口模式驻留应该有界（{mem:?} 应 < 1MB）"
    );
    assert!(mid_row.is_some(), "row_for_line 应基于块索引可查（中部）");
    assert_eq!(toc_n, iters, "目录应由块索引全量提供");

    // 用像素滚动（scroll_vertical＝滚轮同款路径）连续下滚：render 每帧调度
    // ensure_window，窗口应随可见范围移动（固定窗口虚拟化下 `scroll_vertical`
    // 直接驱动窗口重定位，无「到达已布局窗口」的旧限制）。
    for _ in 0..30 {
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, view_cx| {
                md.scroll_vertical(-300.);
                view_cx.notify();
            });
        });
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }
    for _ in 0..3 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }
    let (scroll_top, win_lo2, win_len2) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        (
            md.abs_scroll_top().0,
            md.win_lo,
            md.win_rows.len(),
        )
    });
    assert!(
        scroll_top > 100,
        "像素滚动应将绝对可见块下移，abs_top_block={scroll_top}"
    );
    assert!(
        win_lo2 > 50,
        "窗口应随滚动下移，win_lo={win_lo2}（abs_top={scroll_top}）"
    );
    assert!(
        win_lo2 <= scroll_top && scroll_top < win_lo2 + win_len2 + 64,
        "窗口应覆盖可见范围，win_lo={win_lo2} len={win_len2} abs_top={scroll_top}"
    );
    assert!(win_len2 < 200, "任一时刻窗口应保持有界，win_len={win_len2}");
}

/// 回归：窗口模式 **增量窗口扩展**（拖滚动条/慢滚只解析新进入的带头/尾带、拼接复用
/// 既有行）必须与「整窗重解析」逐行一致——防拼接错位/丢行/顺序颠倒。用与
/// `prepare_render` 同步的目标序列驱动（apply_desired + ensure_window 交替前进），
/// 几次相邻重叠的重建后，把最终窗口文本与一次性整窗重解析对拍。
#[gpui::test]
fn windowed_incremental_window_matches_full_reparse(cx: &mut TestAppContext) {
    use crate::markdown::parse::parse_block_window_rope;
    use ropey::Rope;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let iters = crate::config::limits::WINDOWED_MIN_BLOCKS + 500;
    let mut src = String::new();
    for i in 0..iters {
        src.push_str(&format!(
            "# 标题{i}\n\n正文段落 {i}，含一些文字。\n\n- 列表 {i}\n\n```rs\nlet a = {i};\n```\n\n"
        ));
    }
    let rope0 = Rope::from_str(&src);
    let idx0 = std::rc::Rc::new(crate::markdown::scan_blocks_rope(&rope0));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(crate::markdown::PreviewSrc::Rope(rope0.clone()), idx0.clone(), cx)
        });
    });
    cx.simulate_resize(size(px(980.), px(760.)));
    settle(cx);
    assert!(
        cx.update(|_, cx| host.read(cx).md.read(cx).index.is_some()),
        "应启用窗口模式"
    );

    // 用一家家重叠的小步前进驱动多次增量重建（模拟慢速拖滚动条），覆盖
    // 「仅尾带追加」「仅首带前插」「首尾双伸」三种拼接分支。
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, _cx| {
            let view = md.list_state.viewport_bounds().size.height.as_f32();
            let hint = md.est_hint.max(20.0);
            let step = 18.0 * hint;
            let total = md.total_h();
            let mut target = 0.0f32;
            let n = md.item_count();
            let last = n.saturating_sub(1);
            // 依次推进，每次按 prepare_render 同口径建房（apply + ensure 交替，
            // 覆盖「仅尾带」「仅首带」「首尾双伸」三种增量拼接分支）。
            while target < (total - view).max(0.0) {
                target += step;
                md.desired_scroll.set(target);
                md.restore_zoom_anchor();
                md.apply_desired(md.desired_scroll.get());
                let (cur_lo_vis, _off) = md.abs_scroll_top();
                let h = md.est_hint.max(20.0);
                let vis_base = ((view / h).ceil() as usize).max(1);
                md.overdraw_margin = adaptive_overdraw(vis_base);
                let vis_n = vis_base.saturating_add(md.overdraw_margin);
                let need_lo = cur_lo_vis.saturating_sub(md.overdraw_margin);
                let need_hi = cur_lo_vis.saturating_add(vis_n).min(last);
                md.ensure_window(need_lo, need_hi);
            }
        });
    });
    // 对拍：最终窗口 [win_lo, +len) 与整窗重解析逐行文本一致。
    let (win_lo, win_rows, ref_rows) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        let lo = md.win_lo;
        let hi = lo.saturating_add(md.win_rows.len()).saturating_sub(1);
        let ref_rows = parse_block_window_rope(&rope0, &idx0, lo, hi);
        (lo, md.win_rows.clone(), ref_rows)
    });
    let join = |rs: &[crate::markdown::parse::Row]| -> String {
        rs.iter().map(|r| r.text.as_ref()).collect::<Vec<_>>().join("\n")
    };
    assert!(!win_rows.is_empty(), "应有窗口行");
    assert_eq!(
        join(&win_rows),
        join(&ref_rows),
        "增量拼接窗口（win_lo={win_lo}）应与整窗重解析逐行一致"
    );
}

    /// 预览与编辑器共享源 Rope（第二步）：
    /// - 窗口模式 `set_source_with_index(PreviewSrc::Rope(editor_rope))` 后，内部
    ///   `src_buf` 与传入 rope **共享同一棵 Arc 树**（ropey 2.0 `Rope` 是 Arc 树、
    ///   `Clone` O(1)）——大 md 预览不常驻「+1× 源文本」（那是共享句柄，不是副本）；
    /// - COW 语义：此后编辑原 rope（用户打字），预览持有的快照内容不受影响
    ///   （ropey 沿编辑路径 `Arc::make_mut`，未编辑子树仍共享）。
    #[gpui::test]
    fn windowed_preview_shares_editor_rope(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;
        // 窗口模式的源文本（块数 > WINDOWED_MIN_BLOCKS）。
        use ropey::Rope;
        let iters = crate::config::limits::WINDOWED_MIN_BLOCKS + 600;
        let mut src = String::new();
        for i in 0..iters {
            src.push_str(&format!("# 标题 {i}\n\n正文段落 {i} *强调*\n\n```rs\nlet a = {i};\n```\n\n"));
        }
        let mut editor_rope = Rope::from(src.as_str());
        let snapshot = editor_rope.clone();
        let index = std::rc::Rc::new(crate::markdown::scan_blocks_rope(&snapshot));
        assert!(
            index.blocks.len() > crate::config::limits::WINDOWED_MIN_BLOCKS,
            "测试应走窗口模式"
        );
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.set_source_with_index(PreviewSrc::Rope(snapshot.clone()), index, cx);
            });
        });
        cx.simulate_resize(size(px(400.), px(600.)));
        settle(cx);

        let shared = cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.read(cx).test_src_rope_shared_with(&editor_rope)
        });
        assert!(
            shared,
            "预览源码应与编辑器共享同一棵 Rope（O(1) 克隆；false = 被物化成副本）"
        );

        // COW：编辑编辑器 rope（模拟打字），预览持有的快照内容仍等于原文。
        editor_rope.insert(editor_rope.len(), "\n末尾追加");
        assert_ne!(editor_rope.to_string(), src, "编辑器 rope 应已分叉");
        let src_text = cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.read(cx).test_src_text()
        });
        assert_eq!(
            src_text.as_deref(),
            Some(src.as_str()),
            "预览快照在编辑器编辑后应保持原内容（COW，不受影响）"
        );
        // 摆动：预编译器不 panic；其余不深究（本测试只负责共享语义）。
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }

