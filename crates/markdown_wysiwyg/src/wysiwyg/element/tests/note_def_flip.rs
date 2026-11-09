struct NoteDefWyHost {
    ed: gpui::Entity<super::WyEditor>,
}
impl gpui::Render for NoteDefWyHost {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        self.ed.clone()
    }
}

/// 按 wrap_boundaries 的 glyph 字节索引切一行为视觉段。
fn note_segments(wl: &gpui::WrappedLine) -> Vec<String> {
    let text = wl.text.as_ref();
    let bs: Vec<(usize, usize)> = wl
        .wrap_boundaries()
        .iter()
        .map(|b| (b.run_ix, b.glyph_ix))
        .collect();
    let mut items: Vec<(usize, bool)> = Vec::new();
    let mut gi = 0usize;
    for (run_ix, run) in wl.unwrapped_layout.runs.iter().enumerate() {
        for g in &run.glyphs {
            items.push((g.index, bs.contains(&(run_ix, gi))));
            gi += 1;
        }
    }
    let mut out = Vec::new();
    let mut beg = 0usize;
    for (idx, brk) in items {
        if brk && idx > beg {
            out.push(text[beg..idx].to_string());
            beg = idx;
        }
    }
    if beg < text.len() {
        out.push(text[beg..].to_string());
    }
    out
}

/// 一行的完整折行指纹（边界数 + 每 WrappedLine 宽 + 每视觉段文本）——跨帧/跨重建对比。
fn note_row_fp(row: &super::ShapedRow) -> String {
    let segs: Vec<String> = row.wrapped.iter().flat_map(|wl| note_segments(wl)).collect();
    let widths: Vec<String> = row
        .wrapped
        .iter()
        .map(|wl| format!("{:.0}", f32::from(wl.width())))
        .collect();
    let bounds: Vec<String> = row
        .wrapped
        .iter()
        .map(|wl| wl.wrap_boundaries().len().to_string())
        .collect();
    format!("wb={bounds:?} w={widths:?} segs={segs:?}")
}

/// 回归（本 bug 根因）：`clear_notdef`（干净排收尾）**必须保留**回退校验标记 `verified`。
///
/// 书尾注释章大量 CJK 行走回退字体：冷启动首排 advance 可能不稳定（真机实测 [!90] 行
/// 776 vs 782，差 6px）。`verified` 的语义 =「已做第二次真实回退重排、此后直接走正常
/// 缓存」——同一 (text,font,runs) 的回退解析与折行宽无关。旧实现 `clear_notdef` 走
/// `clear_block` 把 verified 一并清掉 → 任何一次缓存 miss 重建（点击清帧 / 宽度换代 /
/// 滚动）都重新 `fallback-verify`「首排不缓存→冷却→次排」→ 首排不稳定宽度被显示 → 同
/// 一行相邻帧 wrap 不同（句号跨行翻转）。本测试在旧码上失败（clear 后 verified=false）。
#[gpui::test]
fn clear_notdef_keeps_fallback_verified(cx: &mut gpui::TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| super::WyEditor::new(window, cx).with_text("# 标题\n\n正文\n"));
        NoteDefWyHost { ed }
    });
    let vcx: &mut gpui::VisualTestContext = vcx;
    let ed = vcx.update(|_, cx| host.read(cx).ed.clone());
    let fv = |vcx: &mut gpui::VisualTestContext| {
        vcx.update(|_, cx| ed.read(cx).fallback_verified(3))
    };
    let settled = |vcx: &mut gpui::VisualTestContext| {
        vcx.update(|_, cx| ed.read(cx).notdef_settled(3))
    };
    // 构造：块 3 已做回退校验 + 曾有过 notdef 重试（settled 计数）。
    vcx.update(|_, cx| ed.update(cx, |e, _| e.mark_fallback_verified(3)));
    vcx.update(|_, cx| ed.update(cx, |e, _| {
        let _ = e.cool_start(3);
    }));
    assert!(fv(vcx), "回退校验应已登记");
    assert!(!settled(vcx), "单次 notdef 登记不应 settled");
    // 干净排收尾：只清 notdef 重试态，**保留** verified。
    vcx.update(|_, cx| ed.update(cx, |e, _| e.clear_notdef(3)));
    assert!(fv(vcx), "clear_notdef 必须保留 verified（否则任何缓存 miss 都会重走 fallback-verify）");
    assert!(!settled(vcx), "notdef 重试态应清掉");
    // 换代（真实代变化）/编辑路径仍应能作废 verified。
    vcx.update(|_, cx| ed.update(cx, |e, _| e.clear_stale_verdicts(3)));
    assert!(!fv(vcx), "clear_stale_verdicts（换代）应作废 verified");
}

/// 回归（真实渲染路径）：书尾注释章 [!90] 定义行——点击（兜底选区清帧）与 measure 宽度
/// 换代（800↔806，真机点击必现）之后，该行的折行指纹必须与稳态**完全一致**（不出现
/// 「句号跨行」翻转），且 `fallback`/notdef 判决在宽度换代下不被作废。
#[gpui::test]
fn note_def_click_width_churn_keeps_wrap_stable(cx: &mut gpui::TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    // epub 尚未迁为独立 crate（P4 S13，仍在根包），markdown 测试不能依赖根包
    // `crate::epub::parse_epub_file`；这里用内联 markdown 构造等价的「书尾注释章」
    // 文档（CJK 密集 + [!90] 注记定义行 + 注记正文表），复现同一回归场景。
    let text = concat!(
        "# 注释章\n\n",
        "正文段落：中国共产党与中国革命道路的探索，经过了风风雨雨、漫长而艰苦的历程，\n",
        "汇聚了无数先烈的智慧、经验与宝贵牺牲。\n\n",
        "[!90]　与卡尔·马克思、弗里德里希·恩格斯等人合著的《共产党宣言》\n",
        "以及相关著作的注记正文，用于验证书尾注释章大量 CJK 行走回退字体的折行稳定性。\n\n",
        "更多正文[^90]内容：回退字体的宽度换代（800↔806）不得让注记行折行翻转。\n",
    );
    let text = text.to_string();
    let notes = std::collections::BTreeMap::from([(
        90usize,
        "与卡尔·马克思、弗里德里希·恩格斯等人合著的《共产党宣言》注记正文……".to_string(),
    )]);
    let nl = text.lines().position(|l| l.contains("[!90]")).expect("[!90] 行");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| super::WyEditor::new(window, cx).with_text(&text));
        NoteDefWyHost { ed }
    });
    let vcx: &mut gpui::VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| {
            e.set_editable(false);
            e.set_epub(true);
            e.set_notes(notes.clone());
        });
        let _ = window;
    });
    let ed = vcx.update(|_, cx| host.read(cx).ed.clone());
    let block = vcx.update(|_, cx| {
        let e = ed.read(cx);
        (0..e.doc().block_count())
            .find(|&i| e.doc().block_meta(i).map(|m| m.src_line as usize) == Some(nl))
            .expect("找到 [!90] 的块")
    });
    let w806 = gpui::px(806.);
    let w800 = gpui::px(800.);
    // 稳态 806（含首排 fallback-verify 自愈完成后）。
    let steady = vcx.update(|window, cx| {
        super::build_frame(ed.read(cx), window, w806, (0., 999_999.))
    });
    let steady_fp = {
        let row = steady
            .rows
            .iter()
            .find(|r| r.src_line == nl)
            .expect("稳态应有 [!90] 行");
        note_row_fp(row)
    };
    let _ = block;
    // 宽度换代（点击后 measure 在 800/846/806 间反复）：先 800 再 806 —— 806 帧为显示帧。
    for w in [w800, w806, w800, w806] {
        vcx.update(|window, cx| {
            super::build_frame(ed.read(cx), window, w, (0., 999_999.));
        });
    }
    let after_cycle = vcx.update(|window, cx| {
        let f = super::build_frame(ed.read(cx), window, w806, (0., 999_999.));
        let row = f.rows.iter().find(|r| r.src_line == nl).expect("应有 [!90] 行");
        (note_row_fp(row), ed.read(cx).fallback_verified(block))
    });
    assert_eq!(
        after_cycle.0, steady_fp,
        "[!90] 行在 800↔806 宽度换代后折行必须回到稳态（不得句号跨行翻转）"
    );
    // 模拟点击（兜底选区清帧）：把光标放到 [!90] 段正文 → 重建后仍与稳态一致。
    let click_off = vcx.update(|_, cx| {
        let e = ed.read(cx);
        e.line_start(nl) + e.line_text(nl).find("等人").unwrap_or(4)
    });
    ed.update(vcx, |e, _| e.begin_selection_at(click_off));
    assert!(
        vcx.update(|_, cx| ed.read(cx).rd.borrow().frame.borrow().is_none()),
        "点击正文应清帧（触发重建）"
    );
    let post_click = vcx.update(|window, cx| {
        let f = super::build_frame(ed.read(cx), window, w806, (0., 999_999.));
        let row = f.rows.iter().find(|r| r.src_line == nl).expect("应有 [!90] 行");
        note_row_fp(row)
    });
    assert_eq!(
        post_click, steady_fp,
        "点击 [!90] 段后重建，该行折行必须回到稳态（不得出现句号跨行两态翻转）"
    );
}
