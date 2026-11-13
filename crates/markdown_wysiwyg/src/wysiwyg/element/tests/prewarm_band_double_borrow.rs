// 真实崩溃回归：`RefCell already borrowed`（`shape.rs lazy_ensure_image_size`）。
//
// 机制：滚动时 element prepaint 每帧调 `prewarm_band`，其在 `shape_docrow` 的实参里
// 直接写 `&rd.borrow().theme` / `&rd.borrow().theme.font` —— 该临时 `Ref<WyRenderModel>`
// 借用到整条调用语句结束才 drop，跨整整条 `shape_docrow` 调用存活；图片行 shape →
// `lazy_ensure_image_size` 内 `rd.borrow_mut()` 即撞 `RefCell already borrowed` panic。
// （`build_frame` Pass2 传的是 `let theme = rd.borrow().theme.clone()` 克隆后的 `&theme`，
// 故只有滚动预热/巨块补算这两条 `&rd.borrow()` 实参路径触发。修复 = 调用前先 clone。）
//
// 本测试：文档 = 200 个短段落后一张真实 PNG 图片行（带外、未缓存、未预读），照真实滚动
// 时序驱动 `prewarm_band`：先注册首带，再滚到图片前一带 → 预热目标带恰好含图片行 →
// `shape_docrow`（&rd.borrow() 实参）→ 崩溃。修复前红、修复后绿。

#[gpui::test]
fn prewarm_band_double_borrow(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    // 真实 PNG（`resolve_image_src` 对盘符绝对路径原样返回 → `image_intrinsic_size` 读到
    // 真实尺寸 → `lazy_ensure_image_size` 走 `borrow_mut` 写缓存 = 崩溃前提）。
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/黑主题.png");
    if !std::path::Path::new(p).exists() {
        eprintln!("SKIP: missing {p}（真实 PNG 夹具，缺席即跳过本诊断）");
        return;
    }
    // 大量短段落把图片推到可视带之外（图片行 src_line = 200，首帧/首带不带它）。
    let mut text = String::new();
    for i in 0..200 {
        text.push_str(&format!("short body paragraph line {i}\n"));
    }
    text.push_str(&format!("![a]({p})\n"));
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let q = 512.;
        let width = px(900.);
        // caret 放行首（≠ 图片行，否则 prewarm 的 `caret_line != srcline` 会跳过图片）。
        ed.update(cx, |e, _| e.set_caret(0));

        let img_block = ed.read(cx).doc().block_index_for_line(200);
        let (_top_img, m) = {
            // 首帧只建带 0：Pass2 物化 0..512px 的段落块，图片在带外不被 shape。
            let f0 = build_frame(ed.read(cx), window, width, (0., q));
            let top = f0.block_top(img_block);
            (top, (top / q) as i32)
        };
        // 当前视口带 = 图片前一带 [ (m-1)q, mq )；prewarm 目标带 = 下一带 [mq, (m+1)q )。
        // 图片顶 ∈ [mq, mq+q) ⇒ 目标带恰含图片行。
        let b_cur = (m - 1) as f32 * q;
        let f_cur = build_frame(ed.read(cx), window, width, (b_cur, b_cur + q));
        // 前置复核：目标带（下一带）确实包含图片块（base 估算定位）。
        let (tlo, thi) = (b_cur + q, b_cur + 2. * q);
        let (bi0, bi1) = f_cur.band_block_range(tlo, thi);
        assert!(
            (bi0..bi1).contains(&img_block),
            "前置：图片块 {img_block} 应位于预热目标带 [{tlo},{thi})（实际块段 [{bi0},{bi1})）"
        );
        // 前置复核：当前带帧不含图片行（未被 shape、尺寸未预读）。
        assert!(
            !f_cur.rows.iter().any(|r| r.kind == IrKind::Image),
            "前置：当前带帧不应含图片行"
        );
        assert_eq!(
            ed.read(cx).image_size(p),
            None,
            "前置：图片尺寸应仍为未尝试"
        );
        // 照真实滚动时序驱动 prewarm：
        // 1) 首带注册 last_band_idx（不整形）；
        // 2) 滚到图片前一带 → 带序号变化（dir=+1）→ 目标带 = 含图片行的下一带。
        //    每帧 scroll_top 微移 0.5px 伪装滚动活跃，绕过 Prewarm 静置门控。
        let f_reg = build_frame(ed.read(cx), window, width, (0., q));
        prewarm_band(ed.read(cx), &f_reg, window, 20.);
        // ⚠️ 预期在此崩溃：shape_docrow 的 `&rd.borrow().theme` 实参跨调用存活，撞
        // `lazy_ensure_image_size` 的 `rd.borrow_mut()`。崩溃即诊断成立（红）。用
        // RUST_BACKTRACE=1 跑以取完整栈。
        for j in 0..300 {
            prewarm_band(ed.read(cx), &f_cur, window, b_cur + j as f32 * 0.5);
            if ed.read(cx).image_size(p).is_some() {
                break; // 走到图片行且未崩（理论不该发生）→ 停
            }
        }
        // 若走到这里 = 未复现（黄）；断言图片已入缓存以确认路径被执行过。
        assert!(
            ed.read(cx).image_size(p).is_some(),
            "未复现：prewarm 300 次仍未读到图片尺寸"
        );
    });
}

