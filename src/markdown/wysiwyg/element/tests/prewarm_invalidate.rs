/// D1 滚动预热：每次带穿越前把「下一量子带」提前 shape 进缓存 → 穿越时 Pass2
/// 命中（整形数 ≈ 0），冷态对照组则每穿越都整形成本。只 put 缓存、不动 base 高度。
#[gpui::test]
fn prewarm_band_makes_band_cross_near_cache_hit(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::Prewarm;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    // 连续短段：512px 量子带 ≈ 20+ 块；预热 3 块/帧 × 10 帧覆盖整带。
    let mut text = String::new();
    for i in 0..4000 {
        text.push_str(&format!("paragraph number {i} with words here\n"));
    }
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let q = 512.;
        let width = px(900.);
        // caret 放文末（任何带外），排除 reveal 干扰。
        ed.update(cx, |e, _| e.set_caret(text.len()));

        // —— 对照组：连续 6 次带穿越（全冷、无预热）——
        let mut cold: Vec<u64> = Vec::new();
        for i in 0..6usize {
            build_frame(
                ed.read(cx),
                window,
                width,
                (i as f32 * q, i as f32 * q + 512.),
            );
            cold.push(ed.read(cx).take_reshapes());
        }
        let cold_sum: u64 = cold.iter().sum();
        assert!(cold_sum > 0, "冷态穿越应有整形：{cold:?}");
        assert!(
            cold.iter().all(|&c| c > 0),
            "冷态每一穿越都应整形：{cold:?}"
        );

        // —— 预热组：同一 editor 清缓存/清预热状态后重来 ——
        ed.update(cx, |e, _| {
            e.shape_rows.borrow_mut().clear();
            *e.prewarm.borrow_mut() = Prewarm::default();
            e.set_caret(text.len());
        });
        let mut warm: Vec<u64> = Vec::new();
        for i in 0..8usize {
            let bw = i as f32 * q;
            let f = build_frame(ed.read(cx), window, width, (bw, bw + 512.));
            if i == 0 {
                // 首带冷建（不计入穿越整形）；随后对带 1 建立方向并预热。
                ed.read(cx).take_reshapes();
                prewarm_band(ed.read(cx), &f, window);
                continue;
            }
            // 本带穿越整形（上一帧已对带 i-1 执行过 10 次预热 → 命中应接近 0）。
            warm.push(ed.read(cx).take_reshapes());
            // 对本带 frame 预热下一带（i+1）：3 块/帧 × 10 帧覆盖整带。
            for _ in 0..10 {
                prewarm_band(ed.read(cx), &f, window);
            }
        }
        let warm_sum: u64 = warm.iter().sum();
        println!("[prewarm] cold={cold:?} (sum {cold_sum}) vs warm={warm:?} (sum {warm_sum})");
        // 预热后每带穿越只应补极少量整形（首个冷带除外）：总计应远小于冷态总和。
        assert!(
            warm_sum < cold_sum / 3,
            "预热后穿越应接近缓存命中：warm {warm:?} ({warm_sum}) vs cold {cold:?} ({cold_sum})"
        );
        assert!(
            warm[1..].iter().all(|&c| c <= 2),
            "进入稳态后每次穿越至多补 2 次整形：{warm:?}"
        );
    });
}

/// D1 速度自适应预算的公式单测：快滚（vel 大）时预算按比例放大、且硬封顶。
#[test]
fn prewarm_effective_budget_scales_with_velocity() {
    use crate::markdown::wysiwyg::Prewarm;
    let p = Prewarm::default();
    assert_eq!(p.blocks_per_frame, 3, "基础预算默认 3 块/帧");
    assert_eq!(p.effective_budget(0), 3, "静止/带内微滚只用基础预算");
    assert_eq!(p.effective_budget(1), 7, "匀速 1 带/帧 → 预算 7");
    assert_eq!(
        p.effective_budget(3),
        15,
        "跨 3 带/帧 → 预算 15（近一档块数）"
    );
    assert_eq!(p.effective_budget(4), 19, "跨 4 带/帧 → 预算 19");
    assert_eq!(
        p.effective_budget(100),
        Prewarm::MAX_BUDGET,
        "任意速度预算硬封顶"
    );
    assert!(
        p.effective_budget(4) >= 15,
        "快滚单帧预算应足以盖住一档的大部分块"
    );
}

/// D1 速度自适应的接线回归：一次跨多带（快滚）后，下一帧预热应一次整形成倍块数
/// （非固定 3 块）——证明 `prewarm_band` 已用带序号跳跃算速度、按速度放大预算。
#[gpui::test]
fn prewarm_budget_ramps_up_on_fast_band_jump(cx: &mut TestAppContext) {
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let mut text = String::new();
    for i in 0..4000 {
        text.push_str(&format!("paragraph number {i} with words here\n"));
    }
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let q = 512.;
        let width = px(900.);
        ed.update(cx, |e, _| e.set_caret(text.len()));

        // 首帧登记带 0（无方向）；再对同一带预热 → dir 仍 0、直接返回（不整形）。
        let f0 = build_frame(ed.read(cx), window, width, (0., q));
        prewarm_band(ed.read(cx), &f0, window);
        prewarm_band(ed.read(cx), &f0, window);

        // 一次跳 3 个带（快滚）：方向置 +1、vel=3 → 预算 15。
        let f1 = build_frame(ed.read(cx), window, width, (3. * q, 4. * q));
        ed.read(cx).take_reshapes(); // 清 build_frame 自身的 Pass2 整形计数
        prewarm_band(ed.read(cx), &f1, window);
        let d1 = ed.read(cx).take_prewarm_shapes();
        assert!(
            (12..=15).contains(&d1),
            "跨 3 带/帧应一次整形成倍块数（预算 15），实际 {d1}"
        );

        // 同一带再预热（vel=0）：回落到基础预算 3。
        prewarm_band(ed.read(cx), &f1, window);
        let d2 = ed.read(cx).take_prewarm_shapes();
        assert!((1..=3).contains(&d2), "静止时回落到基础预算 3，实际 {d2}");
    });
}

/// 非结构性编辑只作废**被编辑块**的整形缓存；结构性编辑**保留编辑线之前的块**、
/// 作废受影响/位移后缀（比旧「整表清空」更进一步：Enter 后 Prefix 行仍命中缓存）。
#[gpui::test]
fn non_structural_edit_invalidates_only_edited_block(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let mut text = String::new();
    for i in 0..80 {
        text.push_str(&format!("paragraph number {i} here\n"));
    }
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| e.set_caret(text.len()));
        // 带限前 ~72 行（每段 ≈22px）；caret 在文末（带外）→ 不干扰缓存断言。
        let _f = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
        let cached = ed.read(cx).shape_rows_len();
        assert!(cached > 0);
        // 归零整形计数（只统计编辑后的重建）。
        ed.read(cx).take_reshapes();
        // 在「paragraph number 5」行内做一次**非结构性**单字符替换。
        let at = text.find("number 5").unwrap() + 7;
        let blk_edit = ed
            .read(cx)
            .doc()
            .block_index_for_line(ed.read(cx).row_col(at).0);
        assert!(ed.read(cx).shape_rows_has(blk_edit), "编辑前该块应已缓存");
        ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "X", ecx));
        // 被编辑块被剔除；其它块仍复用 → 缓存条目数不变（该块条目已被删）。
        assert!(
            !ed.read(cx).shape_rows_has(blk_edit),
            "非结构性编辑应剔除被编辑块的整形缓存"
        );
        // 重建带：只有被编辑块被重排（其它块仍命中）。光标此刻在该行 → 也算光标行，
        // 仍只重排 1 行。
        let _f2 = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
        let r = ed.read(cx).take_reshapes();
        assert_eq!(r, 1, "只应重排被编辑块，实际 {r}");
        // 结构性编辑（插入换行）→ 受影响/位移的后缀作废，编辑线之前的块保留
        // （前缀复用，Pass2 免整带重 shape；旧实现整表 clear 后 Enter 每键重排整带）。
        ed.update(cx, |e, ecx| e.replace_range(at..at, "\n", ecx));
        let after = ed.read(cx).shape_rows_len();
        assert!(
            after > 0 && after < cached,
            "结构编辑应保留前缀缓存、作废后缀（{after} 于 {cached}）"
        );
        assert!(
            !ed.read(cx).shape_rows_has(blk_edit),
            "结构编辑应剔除被编辑块"
        );
    });
}

/// 表格/代码**内容行**的单字符键入按几何不变 → 非结构性：`struct_ver` 不递增
/// （不触发整篇 Pass1 + fold_plan 重建）、整形缓存整体保留、只重排被编辑块。
#[gpui::test]
fn table_cell_typing_is_non_structural(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let mut text = String::from("| a | b |\n|---|---|\n");
    for i in 0..60 {
        text.push_str(&format!("| cell {i} | val |\n"));
    }
    text.push_str("para before\n");
    text.push_str("para after\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let _f = build_frame(ed.read(cx), window, px(900.), (0., 10_000.));
        let cached0 = ed.read(cx).shape_rows_len();
        assert!(cached0 > 0);
        ed.read(cx).take_reshapes();
        let sv0 = ed.read(cx).struct_ver();
        let at = text.find("cell 5").unwrap() + 3;
        let blk = ed
            .read(cx)
            .doc()
            .block_index_for_line(ed.read(cx).row_col(at).0);
        assert!(ed.read(cx).shape_rows_has(blk), "编辑前被编辑表块应已缓存");
        ed.update(cx, |e, ecx| e.replace_range(at..at, "X", ecx));
        assert_eq!(
            ed.read(cx).struct_ver(),
            sv0,
            "表内单字符键入不得递增 struct_ver（不得触发整篇 Pass1）"
        );
        assert!(
            !ed.read(cx).shape_rows_has(blk),
            "被编辑表块应从整形缓存剔除（局部作废）"
        );
        let _f2 = build_frame(ed.read(cx), window, px(900.), (0., 10_000.));
        assert_eq!(
            ed.read(cx).shape_rows_len(),
            cached0,
            "重建后缓存条目数应复原（其余块未被清除）"
        );
        let r = ed.read(cx).take_reshapes();
        assert_eq!(r, 1, "只应重排被编辑的表块，实际 {r}");
    });
}

/// 字号/主题变化 → 行整形缓存整体清空（否则旧字号的行宽/高被复用）。
#[gpui::test]
fn font_change_clears_shape_cache(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "para line aaa\npara line bbb\npara line ccc\npara line ddd\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let _f = build_frame(ed.read(cx), window, px(900.), (0., 800.));
        assert!(ed.read(cx).shape_rows_len() > 0);
        ed.update(cx, |e, _| e.set_font_size(px(18.)));
        assert_eq!(
            ed.read(cx).shape_rows_len(),
            0,
            "字号变化应整体清空行整形缓存"
        );
    });
}

/// 光标移动不触碰行整形缓存（仅清帧）→ 重建仍复用缓存。
#[gpui::test]
fn caret_move_keeps_shape_cache(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "para line aaa\npara line bbb\npara line ccc\npara line ddd\npara line eee\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| e.set_caret(text.len()));
        let _f = build_frame(ed.read(cx), window, px(900.), (0., 600.));
        let cached = ed.read(cx).shape_rows_len();
        assert!(cached > 0);
        ed.read(cx).take_reshapes();
        // 光标移到带内文本行 → 只重排该行（caret reveal 守卫）；其余缓存行均复用。
        ed.update(cx, |e, _| e.set_caret(0));
        let _f2 = build_frame(ed.read(cx), window, px(900.), (0., 600.));
        let r = ed.read(cx).take_reshapes();
        assert!(
            r <= 2,
            "光标移动最多重排光标行 + 原光标行（均未缓存），其余行复用（实际 {r}）"
        );
        assert!(
            ed.read(cx).shape_rows_len() >= 3,
            "光标移动不应清空行整形缓存"
        );
    });
}

