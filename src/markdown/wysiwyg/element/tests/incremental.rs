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
    cx.update(crate::actions::bind_local_keymaps);
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
    cx.update(crate::actions::bind_local_keymaps);
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
    cx.update(crate::actions::bind_local_keymaps);
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
                *e.fold_plan.borrow_mut() = None;
                e.struct_edit_pending.set(false);
            });
            let _f2 = build_frame(ed.read(cx), window, px(900.), (0., 999_999.));
            let full = ed.read(cx).fold_plan_rc().expect("整建后应有折叠计划");
            (inc, full)
        });
        assert_eq!(
            inc.math_start, full.math_start,
            "{needle}: math_start 不一致"
        );
        assert_eq!(inc.math_body, full.math_body, "{needle}: math_body 不一致");
        assert_eq!(
            inc.fold_lines, full.fold_lines,
            "{needle}: fold_lines 不一致"
        );
        assert_eq!(
            inc.fold_bodies, full.fold_bodies,
            "{needle}: fold_bodies 不一致"
        );
        assert_eq!(
            inc.fold_split, full.fold_split,
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

/* ==================== 图片编辑：悬浮铅笔按钮 + 卡片表单 ==================== */

use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent};

