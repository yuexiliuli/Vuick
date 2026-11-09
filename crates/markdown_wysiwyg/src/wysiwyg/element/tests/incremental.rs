#[gpui::test]
fn async_open_rope_install_matches_sync_set_source(cx: &mut TestAppContext) {
    // A 区残余结构性优化：App 大文档后台 load（`open_from_rope` 后台建 Doc +
    // `install_open` UI 装配）的产物必须与同步 `set_source_text` 完全一致——文本/
    // 行数/字符数/块索引/渲染行数/loaded 标志。覆盖混合内容：标题、段落行内样式、
    // 列表、引用、代码围栏、表格、独立图片、数学、警报。
    use ropey::Rope;
    let text = "# 标题\n\n正文段落 **加粗** `代码`\n\n- 项目一\n- 项目二\n\n> 引用\n\n```rust\nlet x = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n![img](/x.png)\n\n$E=mc^2$\n\n> [!WARNING]\n> 警告\n";
    let rope = Rope::from_str(text);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let sync = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        let bg = cx.new(|cx| WyEditor::new(window, cx));
        DualWyHost { sync, bg }
    });
    vcx.update(|window, cx| {
        let sync = host.read(cx).sync.clone();
        let bg = host.read(cx).bg.clone();
        // 后台解析 + 装配（App 大文档 load 的等价两步）。
        let r = crate::markdown::wysiwyg::WyEditor::open_from_rope(rope);
        bg.update(cx, |ed, _| ed.install_open(r));
        assert!(
            sync.read(cx).is_loaded() && bg.read(cx).is_loaded(),
            "两条路径都应收敛到已装载"
        );
        assert_eq!(
            sync.read(cx).text().to_string(),
            bg.read(cx).text().to_string(),
            "全文一致"
        );
        assert_eq!(
            sync.read(cx).line_count(),
            bg.read(cx).line_count(),
            "行数一致"
        );
        assert_eq!(
            sync.read(cx).char_count(),
            bg.read(cx).char_count(),
            "字符数一致"
        );
        assert_eq!(
            sync.read(cx).doc().block_count(),
            bg.read(cx).doc().block_count(),
            "块数一致"
        );
        for i in 0..sync.read(cx).doc().block_count() {
            assert_eq!(
                sync.read(cx).doc().block_meta(i),
                bg.read(cx).doc().block_meta(i),
                "块 {i} 元数据一致"
            );
        }
        // 全篇窗口渲染行数一致（消费同一 BlockIndex → 同一行网格）。
        let rows_sync = build_frame(sync.read(cx), window, px(600.), (0., 1.0e9))
            .rows
            .len();
        let rows_bg = build_frame(bg.read(cx), window, px(600.), (0., 1.0e9))
            .rows
            .len();
        assert_eq!(rows_sync, rows_bg, "渲染行数一致");
    });
    // 从未装载的空壳编辑器（App load 在途窗口）也应如约报告未装载。
    let (host2, vcx2) = cx.add_window_view(|window, cx| {
        let empty = cx.new(|cx| WyEditor::new(window, cx));
        WyHost { ed: empty }
    });
    vcx2.update(|_, cx| {
        assert!(
            !host2.read(cx).ed.read(cx).is_loaded(),
            "新建空编辑器未装载"
        );
    });
}

#[gpui::test]
#[ignore]
fn bench_img_band_geometry(cx: &mut TestAppContext) {
    // 诊断：图片行（src≈62-64）与标题行（src≈98）在真机滚过图片带后的几何
    // （top/height/kind 连续性）——验证"图片后看不到字"是不是布局把后续行挤出带/错位。
    use crate::markdown::wysiwyg::WyEditor;
    cx.update(crate::test_support::bind_local_keymaps);
    let path = "fixtures/大文档测试2.md";
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[diag] skip: {e}");
            return;
        }
    };
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(980.), (0., 6000.));
        eprintln!("[diag] rows materialized: {}", frame.rows.len());
        for r in frame
            .rows
            .iter()
            .filter(|r| (50..=112).contains(&r.src_line))
        {
            eprintln!(
                "[diag] src={} kind={:?} top={:.1} h={:.1}",
                r.src_line,
                r.kind,
                f32::from(r.top),
                f32::from(r.height)
            );
        }
    });
}

#[gpui::test]
fn update_block_regions_incremental_matches_full_rebuild(cx: &mut TestAppContext) {
    // B4：结构编辑在多个上下文下，增量 `update_block_regions_after_edit` 产出的
    // `block_regions` 必须与整表 `rebuild_block_regions`（`with_text` 全量路径）
    // 完全一致——覆盖普通段落 Enter、表格内 Enter、围栏内容 Enter、内容删换行合并。
    use crate::markdown::wysiwyg::BlockRegion;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = concat!(
        "para before\n",
        "| h | h |\n",
        "|---|---|\n",
        "| 1 | 2 |\n",
        "| 3 | 4 |\n",
        "para mid\n",
        "```rust\n",
        "let a = 1;\n",
        "let b = 2;\n",
        "```\n",
        "para after\n",
    );
    let find = |needle: &str| text.find(needle).unwrap();
    let cases: &[(usize, &str)] = &[
        // (byte offset, 替换串)：插入 Enter / 删除换行（合并）
        (find("para mid") + 4, "\n"),      // 普通段落内 Enter
        (find("| 1 | 2 |") + 4, "\n"),     // 表格数据行内 Enter
        (find("let a = 1;") + 4, "\n"),    // 围栏内容行内 Enter
        (find("let a = 1;\nlet") + 9, ""), // 围栏内容删换行（合并）
        (find("| 3 | 4 |") + 4, ""),       // 表格内容删换行（合并）
    ];
    for &(at, rep) in cases {
        let mut edited = String::from(text);
        if rep.is_empty() {
            edited.replace_range(at..at + 1, "");
        } else {
            edited.insert_str(at, rep);
        }
        // A：增量路径（replace_range 内部走 update_block_regions_after_edit）。
        let (host_a, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let _ = vcx;
        host_a.update(cx, |w, cx| {
            w.ed.update(cx, |ed2, ecx| ed2.replace_range(at..at + 1, rep, ecx));
        });
        // B：全量路径（with_text → rebuild_block_regions）。
        let (host_b, vcx_b) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&edited));
            WyHost { ed }
        });
        let _ = vcx_b;
        let (inc_regions, full_regions): (Vec<BlockRegion>, Vec<BlockRegion>) = cx.update(|cx| {
            let inc = host_a.read(cx).ed.read(cx).block_regions.clone();
            let full = host_b.read(cx).ed.read(cx).block_regions.clone();
            (inc, full)
        });
        assert_eq!(
            inc_regions, full_regions,
            "增量与全量 block_regions 不一致（at={at} rep={rep:?}）"
        );
    }
}

/// B5：折叠计划 deltas **增量平移** == 强制整建（逐字段比）。结构 Enter 后（marker
/// 身份未变）应走 `fold_plan_deltas_shifted` 平移路径，产出必须与整建完全一致。
#[gpui::test]
fn fold_plan_incremental_shift_matches_full_rebuild(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = concat!(
        "para before\n",
        "$$x^2$$\n",
        "math body line\n",
        "$$\n",
        "> [!NOTE]-\n",
        "alert body\n",
        "alert body 2\n",
        "> done\n",
        "para mid\n",
        "```rust\n",
        "let a = 1;\n",
        "```\n",
        "para after\n",
    );
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let cases = [
        ("para mid", 4usize),
        ("para before", 4usize),
        ("alert body", 4usize),
    ];
    // 先做一次整建首穿（热缓存/基线）。
    let _f0 = vcx.update(|window, cx| {
        build_frame(host.read(cx).ed.read(cx), window, px(900.), (0., 999_999.))
    });
    for (needle, off) in cases {
        let (inc, full) = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 结构编辑：插入换行。
            ed.update(cx, |ed2, ecx| {
                let src = ed2.text();
                let at = src.find(needle).unwrap().min(src.len());
                let at = (at + off).min(src.len());
                ed2.replace_range(at..at, "\n", ecx);
            });
            let _f1 = build_frame(ed.read(cx), window, px(900.), (0., 999_999.));
            // 增量路径产出的计划。
            let inc = ed.read(cx).fold_plan_rc().expect("增量平移后应有折叠计划");
            // 强制整建：清缓存 + 清待消费标志（增量条件不满足 → 走整建）。
            ed.update(cx, |e, _| {
                *e.rd.borrow_mut().fold_plan.borrow_mut() = None;
                e.rd.borrow_mut().struct_edit_pending.set(false);
            });
            let _f2 = build_frame(ed.read(cx), window, px(900.), (0., 999_999.));
            let full = ed.read(cx).fold_plan_rc().expect("整建后应有折叠计划");
            (inc, full)
        });
        assert_eq!(
            inc.sets.math_start, full.sets.math_start,
            "{needle}: math_start 不一致"
        );
        assert_eq!(
            inc.sets.math_body, full.sets.math_body,
            "{needle}: math_body 不一致"
        );
        assert_eq!(
            inc.sets.fold_lines, full.sets.fold_lines,
            "{needle}: fold_lines 不一致"
        );
        assert_eq!(
            inc.sets.fold_bodies, full.sets.fold_bodies,
            "{needle}: fold_bodies 不一致"
        );
        assert_eq!(
            inc.sets.fold_split, full.sets.fold_split,
            "{needle}: fold_split 不一致"
        );
        assert_eq!(&*inc.deltas, &*full.deltas, "{needle}: deltas 不一致");
        assert!(
            (inc.delta_sum - full.delta_sum).abs() < 1e-3,
            "{needle}: delta_sum 不一致 {} vs {}",
            inc.delta_sum,
            full.delta_sum
        );
    }
}

/// S2：分块 `BlockLayout` 的结构编辑 `apply_structural_edit` 产出 == 平铺参考（逐块高/
/// src 全比）。覆盖中段插入/删除、块净增减、`line_delta` 尾部平移、跨多 Chunk 边框
/// 分裂与 `off` 累加。
#[gpui::test]
#[allow(clippy::type_complexity)]
fn chunked_layout_structural_edit_matches_flat_rebuild(_cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::element::BlockLayout;
    use std::cell::RefCell;
    use std::rc::Rc;
    // 参考实现：结构编辑 = 旧块序列上 前缀 + 中段 + 尾段(src+delta)。
    fn apply_flat(
        old: &[(f32, u32)],
        keep: usize,
        middle: &[(f32, u32)],
        j: usize,
        delta: i64,
    ) -> Vec<(f32, u32)> {
        let mut out: Vec<(f32, u32)> = old[..keep].to_vec();
        out.extend_from_slice(middle);
        for &(h, s) in &old[j..] {
            out.push((h, (s as i64 + delta).max(0) as u32));
        }
        out
    }
    // 300 块（>2 Chunk），src = 块下标；多次随机结构编辑驱动边框分裂 + off 累加。
    let mut flat: Vec<(f32, u32)> = (0..300u32).map(|i| (10.0 + (i % 7) as f32, i)).collect();
    let bl: Rc<RefCell<BlockLayout>> = {
        let h: Vec<f32> = flat.iter().map(|&(hh, _)| hh).collect();
        let s: Vec<u32> = flat.iter().map(|&(_, ss)| ss).collect();
        Rc::new(RefCell::new(BlockLayout::new(h, s)))
    };
    // 3 次中段结构编辑（插入 2 块 / 插入 1 块 / 删除 2 块 + 负行偏移）。
    let cases: Vec<(usize, Vec<(f32, u32)>, usize, i64)> = vec![
        (100, vec![(12.0, 100), (13.0, 101)], 100, 1),
        (60, vec![(15.0, 60)], 61, 1),
        (10, vec![], 14, -2),
    ];
    for (keep, middle, j, delta) in cases {
        flat = apply_flat(&flat, keep, &middle, j, delta);
        let old_bor = bl.borrow();
        let n_exp = flat.len();
        let mut nb = BlockLayout::empty();
        nb.apply_structural_edit(&old_bor, keep, &middle, j, delta, n_exp);
        drop(old_bor);
        *bl.borrow_mut() = nb;
        let (hh, ss) = bl.borrow().flat_range(0, n_exp);
        for i in 0..n_exp {
            assert!(
                (hh[i] - flat[i].0).abs() < 1e-3 && ss[i] == flat[i].1,
                "i={i} 块不一致：chunked=({:?},{:?}) flat=({:?},{:?})",
                hh[i],
                ss[i],
                flat[i].0,
                flat[i].1
            );
        }
        assert_eq!(bl.borrow().n(), n_exp, "块数应一致");
        let total = bl.borrow().total();
        let ref_total: f32 = flat.iter().map(|&(h, _)| h).sum();
        assert!((total - ref_total).abs() < 1e-3, "total 应一致");
    }
    // 边界：文末追加（keep == n_old）。
    let keep = bl.borrow().n();
    let middle = vec![(99.0, 999u32)];
    flat = apply_flat(&flat, keep, &middle, keep, 0);
    let old_bor = bl.borrow();
    let mut nb = BlockLayout::empty();
    nb.apply_structural_edit(&old_bor, keep, &middle, keep, 0, flat.len());
    drop(old_bor);
    *bl.borrow_mut() = nb;
    let (hh, ss) = bl.borrow().flat_range(0, flat.len());
    let last = flat.len() - 1;
    assert_eq!((hh[last], ss[last]), (99.0, 999u32), "文末追加块应就位");
    // 边界：`new(flat)` 直接建（无 old）。
    let b = BlockLayout::new(vec![5.0, 6.0], vec![0, 1]);
    assert_eq!(b.n(), 2);
    let (hh, ss) = b.flat_range(0, 2);
    assert_eq!(hh.as_slice(), &[5.0, 6.0]);
    assert_eq!(ss.as_slice(), &[0, 1]);
}

/// M5-B3-1 子项 2：`fp_sets` 与布局 epoch 解耦——宽度换代（epoch 变、region/折叠结构
/// 不变）只重算 deltas，marker 集合 `Rc` **跨计划复用**（不再每键 O(#区域) 哈希重建；
/// 同理由结构与折叠态决定，与布局无关）。
#[gpui::test]
fn fold_plan_sets_reused_across_epoch_only_rebuild(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = concat!(
        "para\n",
        "$$\n",
        "\\int dx\n",
        "$$\n",
        "> [!NOTE]-\n",
        "alert body\n",
        "para tail\n",
    );
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let (sets0, deltas0) = vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let _f0 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let plan = ed.read(cx).fold_plan_rc().expect("折叠计划");
        assert_eq!(plan.sets.math_start.len(), 1, "1 个数学块");
        assert_eq!(plan.sets.fold_bodies.len(), 1, "1 个 alert 折叠体");
        (plan.sets.clone(), plan.deltas.clone())
    });
    // 宽度 600 → 700：epoch（布局代）变化，region/折叠结构不变 → sets 复用同一 Rc，
    // deltas 是布局相关量（卡高/行高随宽度），允许重算为新的。
    let (sets1, deltas1) = vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let _f1 = build_frame(ed.read(cx), window, px(700.), (0., 999_999.));
        let plan = ed.read(cx).fold_plan_rc().expect("折叠计划");
        (plan.sets.clone(), plan.deltas.clone())
    });
    assert!(
        std::rc::Rc::ptr_eq(&sets0, &sets1),
        "仅宽度换代不应重建 marker 集合（fp_sets 与 epoch 解耦）"
    );
    let _ = (&deltas0, &deltas1);
}

/// Cut-1a：跨行删除/合并不再一刀切 `regions_ver++`——`shift_derived_after_edit` 改为显式
/// 判定「编辑线落在区域内 **或** 某区域**起始边界**落在被删除换行吸收的行区间」才回退
/// 整建；否则纯行号平移。本测试遍历围绕数学块/警报/标题折叠体的各种跨行合并落点，
/// 断言**每一次**合并后 `derived()`（无论走平移还是整建路径）与强制整建逐字段一致——
/// 锁定增量平移的边界语义与整篇重建零偏差。
#[gpui::test]
fn derived_merge_shift_matches_full_rebuild(cx: &mut TestAppContext) {
    use crate::markdown::parse::inline_style::MathAlign;
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    use std::ops::Range;
    cx.update(crate::test_support::bind_local_keymaps);
    // 行 0 `## App` 折叠体 [0,2)；行 3-5 `$$` 数学块 [3,6)；行 6-7 `> [!NOTE]` 警报
    // 折叠体 [6,8)；行 8/9 为纯文本合并落点（不碰任何区域边界 → 走平移路径）；
    // 行 11-15 尾部再放一个 `$$` 数学块 [11,14) + `> [!TIP]` 警报 [14,16)——平移类落点
    // （8/9）必须真平移**编辑线之下**的区域，与强制整建比对（否则平移只覆盖「上方不变」
    // 平凡情形，`start > edit_line → shift` 分支无人锁）。
    let text = concat!(
        "## App\n",
        "app body\n",
        "# Tail\n",
        "$$\n",
        "x^2\n",
        "$$\n",
        "> [!NOTE]-\n",
        "> alert body\n",
        "intro A\n",
        "intro B\n",
        "out\n",
        "$$\n",
        "z^2\n",
        "$$\n",
        "> [!TIP]-\n",
        "> tip body\n",
    );
    // derived 快照（平移/整建后与强制整建比对）：MathRegion 未 derive PartialEq，
    // 投影 `(range, source, align)` 再比。
    let snap = |ed: &crate::markdown::wysiwyg::WyEditor| {
        let d = ed.derived();
        let d = d.borrow();
        let math: Vec<(Range<usize>, String, MathAlign)> = d
            .math_regions
            .iter()
            .map(|r| (r.range.clone(), r.source.clone(), r.align))
            .collect();
        (
            math,
            d.alert_fold_regions.clone(),
            d.alert_region_kinds.clone(),
            d.heading_fold_bodies.clone(),
        )
    };
    // 每个落点独立起一个编辑器（防前次合并污染行号/区域）。
    for line_end in [0usize, 1, 2, 4, 5, 7, 8, 9] {
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            // 折叠 `## App` → heading_fold_bodies 非空（crut-1a 要覆盖三类区域）。
            ed.update(cx, |e, ecx| e.toggle_heading_fold_at_line(0, ecx));
            let _f0 = build_frame(ed.read(cx), window, px(900.), (0., 999_999.));
            // 跨行合并：删掉行 `line_end` 结尾的 `\n`（其位置 = `line_end(line)`；删除
            // 后下行并入本行，`edit_line` = line_end）。
            ed.update(cx, |e, ecx| {
                let at = e.line_end(line_end).min(e.len().saturating_sub(1));
                e.replace_range(at..at + 1, "", ecx);
            });
            let _f1 = build_frame(ed.read(cx), window, px(900.), (0., 999_999.));
            // 合并后（平移或整建路径产出的）derived。
            let inc = { let ed = ed.read(cx); snap(ed) };
            // 强制整建：regions_ver++ → 下帧 `derived()` 从全文重扫。
            ed.update(cx, |e, _| { let rd = e.rd.borrow_mut(); rd.regions_ver.set(rd.regions_ver.get().wrapping_add(1)); });
            let full = { let ed = ed.read(cx); snap(ed) };
            assert_eq!(
                inc.0, full.0,
                "line_end={line_end}: 合并后 math_regions 与强制整建不一致"
            );
            assert_eq!(
                inc.1, full.1,
                "line_end={line_end}: 合并后 alert_fold_regions 与强制整建不一致"
            );
            assert_eq!(
                inc.2, full.2,
                "line_end={line_end}: 合并后 alert_region_kinds 与强制整建不一致"
            );
            assert_eq!(
                inc.3, full.3,
                "line_end={line_end}: 合并后 heading_fold_bodies 与强制整建不一致"
            );
        });
    }
}

/// Cut-1a 补口：纯文本 doc（Cut-1b 已把**空 derived** 缓存为主态）里粘贴新建 `$$` 围栏
/// 块 / `> [!]` 警报——结构编辑走「平移判定成功」路径时若漏掉「编辑窗口新建了区域
/// 起始行」，`derived()` 命中旧 `regions_ver` 的空 Rc → 数学块/警报隐形（平移只能搬旧
/// 区域、无法新建）。断言一次建帧后 derived 已含该区域（由 `edit_window_creates_region_marker`
/// 回退整篇重建保证）。
#[gpui::test]
fn pasting_new_region_marker_into_plain_doc_rebuilds_derived(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "plain head\n\nplain tail\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 首帧：计数器全空 → Cut-1b 缓存空 derived（模拟真实纯文本 doc 的主态）。
        let _f0 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let at = ed.read(cx).content().find("\n\n").unwrap() + 1;
        // `\n` 跨行 → structural → 平移判定（此前 derived 全空，本方判定会误走平移）。
        ed.update(cx, |e, ecx| {
            e.replace_range(at..at, "$$\nx^2\n$$\n> [!NOTE]-\n> n body\n", ecx)
        });
        let _f1 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let d = ed.read(cx).derived();
        let d = d.borrow();
        assert_eq!(
            d.math_regions.len(),
            1,
            "粘贴 `$$` 围栏应重建出数学块（不得命中空 derived 滞空）"
        );
        assert_eq!(
            d.alert_fold_regions.len(),
            1,
            "粘贴 `> [!NOTE]-` 应重建出警报折叠体（不得命中空 derived 滞空）"
        );
    });
}

/* ==================== 图片编辑：悬浮铅笔按钮 + 卡片表单 ==================== */

use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent};
