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
                prewarm_band(ed.read(cx), &f, window, bw);
                continue;
            }
            // 本带穿越整形（上一帧已对带 i-1 预热 → 命中应接近 0）。
            warm.push(ed.read(cx).take_reshapes());
            // 对本带预热下一带（i+1）：3 块/帧 × 10 帧覆盖整带。静置门控（Sprint 4b）
            // 下「同一 band 反复调用 = 静置、连续 2 帧后跳过」；真实滚动每帧带顶像素
            // 都在变（含带内微滚），这里用逐帧带顶微移模拟滚动活跃期，让预热持续推进。
            // 中间 build_frame 的 Pass2 只补带边 ≤1-2 块（进带 i+1 的入口在计数线外，
            // 末尾一并丢弃），不影响穿越整形计数。
            for j in 0..10 {
                let fw = build_frame(
                    ed.read(cx),
                    window,
                    width,
                    (bw + j as f32 * 4., bw + j as f32 * 4. + 512.),
                );
                prewarm_band(ed.read(cx), &fw, window, bw + j as f32 * 4.);
            }
            ed.read(cx).take_reshapes(); // 丢弃预热期中间 build_frame 的带边整形噪声
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
        prewarm_band(ed.read(cx), &f0, window, 0.);
        prewarm_band(ed.read(cx), &f0, window, 0.);

        // 一次跳 3 个带（快滚）：方向置 +1、vel=3 → 预算 15。
        let f1 = build_frame(ed.read(cx), window, width, (3. * q, 4. * q));
        ed.read(cx).take_reshapes(); // 清 build_frame 自身的 Pass2 整形计数
        prewarm_band(ed.read(cx), &f1, window, 3. * q);
        let d1 = ed.read(cx).take_prewarm_shapes();
        assert!(
            (12..=15).contains(&d1),
            "跨 3 带/帧应一次整形成倍块数（预算 15），实际 {d1}"
        );

        // 同一带再预热（vel=0）：回落到基础预算 3。
        prewarm_band(ed.read(cx), &f1, window, 3. * q);
        let d2 = ed.read(cx).take_prewarm_shapes();
        assert!((1..=3).contains(&d2), "静止时回落到基础预算 3，实际 {d2}");
    });
}

/// Sprint 4b 静置门控：带宽/内容/宽度全未变时，连续 `IDLE_SKIP_FRAMES` 帧后
/// `prewarm_band` 完全跳过预整形（prewarm_shapes == 0，只留登记簿）；滚动到新带或
/// 内容变化立即复位并恢复。滚动活跃期的「穿越接近缓存命中」见
/// `prewarm_band_makes_band_cross_near_cache_hit`（那里用带顶微移模拟活跃滚动）。
#[gpui::test]
fn prewarm_idle_gate_stops_after_idle_frames(cx: &mut TestAppContext) {
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

        // 建立方向并快滚 3 档（预算放大），登记门控签名。
        let f0 = build_frame(ed.read(cx), window, width, (0., q));
        prewarm_band(ed.read(cx), &f0, window, 0.); // 首帧仅登记带序号
        let f1 = build_frame(ed.read(cx), window, width, (3. * q, 4. * q));
        ed.read(cx).take_reshapes();
        prewarm_band(ed.read(cx), &f1, window, 3. * q); // 变化帧：放行并复位门控
        assert!(
            ed.read(cx).take_prewarm_shapes() > 0,
            "滚动变化帧应预整形"
        );

        // 同一 f1 继续：第 1 个无变化帧仍跑（沉降余量），第 2 个起跳过（idle 计满）。
        prewarm_band(ed.read(cx), &f1, window, 3. * q);
        assert!(
            (1..=3).contains(&ed.read(cx).take_prewarm_shapes()),
            "1 个无变化帧应仍按基础预算（3 块）预整形"
        );
        for _ in 0..3 {
            prewarm_band(ed.read(cx), &f1, window, 3. * q);
        }
        assert_eq!(
            ed.read(cx).take_prewarm_shapes(),
            0,
            "连续无变化帧达到阈值后应完全跳过预整形"
        );

        // 滚动恢复：换到新带 → 立即恢复预整形。
        let f2 = build_frame(ed.read(cx), window, width, (4. * q, 5. * q));
        ed.read(cx).take_reshapes();
        prewarm_band(ed.read(cx), &f2, window, 4. * q);
        assert!(
            ed.read(cx).take_prewarm_shapes() > 0,
            "带变（滚动意图）应立即恢复预整形"
        );

        // 内容变化（文内插字、非结构编辑）→ 内容代变，复位门控并恢复预整形。
        let at = ed.read(cx).content().find("paragraph number 40").unwrap() + 5;
        ed.update(cx, |e, ecx| e.replace_range(at..at, "x", ecx));
        let f3 = build_frame(ed.read(cx), window, width, (4. * q, 5. * q));
        ed.read(cx).take_reshapes();
        prewarm_band(ed.read(cx), &f3, window, 4. * q);
        assert!(
            ed.read(cx).take_prewarm_shapes() > 0,
            "内容变化应复位门控并恢复预整形"
        );
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

/// Step4-① 接线：后台 rope 冷开（`open_from_rope` → `install_open`）后预建槽在场、
/// 且因冷开路径已 arm `cold_topup`；首个 `build_frame` **一次性消费**预建槽并物化出
/// 基础布局（槽清空）。同步 Str 路径（`with_text`）无预建、不 arm（不影响既有单测）。
#[gpui::test]
fn install_rope_prebuild_consumed_on_first_frame(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::{Prewarm, WyEditor};
    use ropey::Rope;
    cx.update(crate::actions::bind_local_keymaps);
    let mut text = String::new();
    for i in 0..2000 {
        text.push_str(&format!("paragraph number {i} here\n"));
    }
    let rope = Rope::from(text.as_str());
    let r = WyEditor::open_from_rope(rope);
    assert!(
        r.prebuild
            .as_ref()
            .is_some_and(|p| p.len() == r.doc.block_count()),
        "rope 路径应带预建且块数对齐"
    );
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| {
            e.install_open(r);
            assert!(e.prebuild_slot.borrow().is_some(), "install 后应存预建槽");
            assert_eq!(
                e.prewarm.borrow().cold_topup,
                Prewarm::COLD_TOPUP_FRAMES,
                "后台 rope 冷开应 arm 冷开反门控"
            );
        });
        // 首个 build_frame：预建 fast path 消费槽 → 基础布局建出、槽清空。
        let _f = build_frame(ed.read(cx), window, px(900.), (0., 2000.));
        assert!(
            ed.read(cx).prebuild_slot.borrow().is_none(),
            "首帧应一次性消费预建槽"
        );
        assert!(
            ed.read(cx).block_layout.borrow().is_some(),
            "首帧后基础布局应已物化"
        );
    });
}

/// Step4-③ 冷开反门控：`install_open`（rope 冷开）arm 后，**静置签名**（滚动/内容/宽度
/// 全不变）下 `prewarm_band` 仍强制预整形 `cold_topup` 帧；耗尽后恢复静置门控（0 整形）
/// ——与 `prewarm_idle_gate_stops_after_idle_frames`（Str 路径不 arm）互补锁行为。
#[gpui::test]
fn cold_open_topup_forces_prewarm_despite_idle(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    use ropey::Rope;
    cx.update(crate::actions::bind_local_keymaps);
    let mut text = String::new();
    for i in 0..4000 {
        text.push_str(&format!("paragraph number {i} here\n"));
    }
    let rope = Rope::from(text.as_str());
    let r = WyEditor::open_from_rope(rope);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let q = 512.;
        let width = px(900.);
        ed.update(cx, |e, _| {
            e.install_open(r);
            e.set_caret(text.len());
        });
        // 首帧登记带序号（无整形）；随后**同一 top 连续帧**：冷开期强制预整形。
        let f0 = build_frame(ed.read(cx), window, width, (0., q));
        prewarm_band(ed.read(cx), &f0, window, 0.);
        let mut shaped = 0u64;
        for _ in 0..8 {
            let f = build_frame(ed.read(cx), window, width, (0., q));
            prewarm_band(ed.read(cx), &f, window, 0.);
            shaped += ed.read(cx).take_prewarm_shapes();
        }
        assert!(shaped > 0, "冷开 topup 期静置也应预整形（实际 {shaped}）");
        // topup 耗尽 → 同一签名连续帧落到静置门控：0 整形。
        let f2 = build_frame(ed.read(cx), window, width, (0., q));
        for _ in 0..4 {
            prewarm_band(ed.read(cx), &f2, window, 0.);
        }
        assert_eq!(
            ed.read(cx).take_prewarm_shapes(),
            0,
            "topup 耗尽后应恢复静置门控"
        );
        // 滚动恢复（带变）→ 正常门控立即放行（冷开期已回归、不断电）。
        let f3 = build_frame(ed.read(cx), window, width, (q, 2. * q));
        ed.read(cx).take_reshapes();
        prewarm_band(ed.read(cx), &f3, window, q);
        assert!(
            ed.read(cx).take_prewarm_shapes() > 0,
            "带变（滚动意图）应放行预整形"
        );
    });
}

/// I4 S1：巨型代码块（>64 行 / >4096B）**首现/滚动进带不再整块重 shape**——Pass2 走
/// 快速路径（估计高占位）、零整形计数、登记进 `pending_giant`；静止
/// [`Prewarm::GIANT_SETTLE_IDLE_FRAMES`] 帧后 `settle_giant_blocks` 补算权威整形（真行
/// 入缓存 + 真高写回），重建后内容正确出现。
#[gpui::test]
fn giant_code_first_frame_placeholder_then_settle(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::{Prewarm, WyEditor};
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    // 巨型围栏（400 行代码 ≫ 64 行闸，估计高必超 1600px 带）→ 首帧带内只有它。
    let mut text = String::from("```\n");
    for i in 0..400 {
        text.push_str(&format!("let value_{i} = alpha beta gamma delta epsilon;\n"));
    }
    text.push_str("```\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 首帧：巨块进带 → 只占位、零权威整形。
        let f = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
        assert_eq!(
            ed.read(cx).take_reshapes(),
            0,
            "巨块首现不得整块重 shape"
        );
        assert_eq!(
            ed.read(cx).shape_rows_len(),
            0,
            "巨块不应以权威行入整形缓存（占位期）"
        );
        assert_eq!(
            ed.read(cx).prewarm.borrow().pending_giant,
            vec![0],
            "巨块应登记进待 settle 集"
        );
        // 首帧输出 = 估计高占位空面板（无文本行）。
        assert_eq!(f.rows.len(), 1, "带内只应有巨块");
        assert!(f.rows[0].wrapped.is_empty(), "占位期不应有权威文本行");
        assert_eq!(
            f.rows[0].kind,
            crate::markdown::ir::IrKind::Code,
            "占位行仍是代码块语义（画面板背景）"
        );
        // 静止：连续同签名帧达到阈值 → settle 权威整形（一次真整形 + 入缓存 + 真高写回）。
        for _ in 0..(Prewarm::GIANT_SETTLE_IDLE_FRAMES as usize + 2) {
            prewarm_band(ed.read(cx), &f, window, 0.);
        }
        assert!(
            ed.read(cx).prewarm.borrow().pending_giant.is_empty(),
            "settle 后待处理集应清空"
        );
        assert!(
            ed.read(cx).shape_rows_has(0),
            "settle 后巨块权威行应入整形缓存"
        );
        // settle 强制下帧重建（frame 缓存置 None），重建后权威内容出现。
        assert!(
            ed.read(cx).frame.borrow().is_none(),
            "settle 应清帧缓存触发重建"
        );
        let f2 = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
        assert_eq!(
            f2.rows[0].wrapped.len(),
            400,
            "重建后巨块应显示 400 行权威内容"
        );
    });
}

/// I4 S2：巨块内**逐键键入不整块重 shape**（每键整形计数 0）、权威内容滞后、静止 settle
/// 后内容正确补上；随后再次键入仍不整块（复用钉住陈旧行），再次静止再补——闭环成立。
#[gpui::test]
fn giant_code_typing_skips_whole_block_until_settle(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::{Prewarm, WyEditor};
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let mut text = String::from("```\n");
    for i in 0..400 {
        text.push_str(&format!("let value_{i} = alpha beta gamma delta epsilon;\n"));
    }
    text.push_str("```\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 光标放到巨块内容中部（编辑行附近）。
        let at = text.find("let value_40").unwrap() + 5;
        ed.update(cx, |e, _| e.set_caret(at));
        let _f = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
        ed.read(cx).take_reshapes();
        // 连续多键键入：每键都不整块（巨块在带内、光标在其内）；每键用**新鲜帧**喂
        // prewarm_band（真实 prepaint 节奏：签名变 → settle 复位，不擅自整块）。
        for k in 0..3u32 {
            let ins = if k % 2 == 0 { "x" } else { "y" };
            ed.update(cx, |e, ecx| e.replace_range(at..at, ins, ecx));
            let fk = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
            assert_eq!(
                ed.read(cx).take_reshapes(),
                0,
                "巨块内键入不得触发整块权威重 shape（键 {k}）"
            );
            prewarm_band(ed.read(cx), &fk, window, 0.);
        }
        assert!(
            !ed.read(cx).shape_rows_has(0),
            "键入期不应有权威行入缓存（内容滞后）"
        );
        assert_eq!(
            ed.read(cx).prewarm.borrow().pending_giant,
            vec![0],
            "键入期巨块仍登记待 settle"
        );
        // 静止补算：权威整形落后键一次性补齐 → 内容正确出现。
        for _ in 0..(Prewarm::GIANT_SETTLE_IDLE_FRAMES as usize + 2) {
            let f2 = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
            prewarm_band(ed.read(cx), &f2, window, 0.);
        }
        assert!(
            ed.read(cx).shape_rows_has(0),
            "静止 settle 后权威内容应入缓存"
        );
        let f3 = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
        assert_eq!(
            f3.rows[0].wrapped.len(),
            400,
            "settle 后巨块显示的是整块权威内容"
        );
        // 再次键入（编辑作废缓存 → 复用钉 + 重新登记），仍不整块。
        ed.update(cx, |e, ecx| e.replace_range(at..at, "z", ecx));
        build_frame(ed.read(cx), window, px(900.), (0., 1600.));
        assert_eq!(
            ed.read(cx).take_reshapes(),
            0,
            "settle 后再次键入仍不整块重 shape"
        );
        assert!(
            !ed.read(cx).shape_rows_has(0),
            "编辑作废后缓存条目应被剔除（钉住行仍供显示）"
        );
    });
}

/// I4 回归护栏：**非巨块（小围栏）行为与现状完全一致**——<64 行代码块照常逐键整块
/// 重 shape（整形计数 1/键、不登记待 settle），不受巨块闸影响。
#[gpui::test]
fn small_fence_unchanged_per_key_reshape(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    // 10 行为小围栏（远低于 64 行闸）。
    let mut text = String::from("```\n");
    for i in 0..10 {
        text.push_str(&format!("let v_{i} = alpha beta;\n"));
    }
    text.push_str("```\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let at = text.find("let v_5").unwrap() + 5;
        ed.update(cx, |e, _| e.set_caret(at));
        let _f = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
        ed.read(cx).take_reshapes();
        // 小围栏逐键 → 仍按现状整块重排（恰 1 次整形/键），且不登记待 settle。
        for k in 0..3u32 {
            let ins = if k % 2 == 0 { "x" } else { "y" };
            ed.update(cx, |e, ecx| e.replace_range(at..at, ins, ecx));
            build_frame(ed.read(cx), window, px(900.), (0., 1600.));
            assert_eq!(
                ed.read(cx).take_reshapes(),
                1,
                "小围栏每键应整块重 shape 1 次（键 {k}）"
            );
        }
        assert_eq!(
            ed.read(cx).prewarm.borrow().pending_giant,
            Vec::<usize>::new(),
            "小围栏不应走巨块快速路径"
        );
        assert!(
            ed.read(cx).shape_rows_has(0),
            "小围栏权威行照常入缓存"
        );
    });
}

