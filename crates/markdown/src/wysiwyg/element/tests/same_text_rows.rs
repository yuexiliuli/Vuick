// 相同文本行跨 `RowShapeCache` 串块回归。
//
// 行整形缓存键 `block_content_id` 是**内容指纹**（kind + aux + 首末行文本，不含
// `src_line`）——文本完全相同的不同块共享指纹；缓存行里的 `src_line`/`src_lines` 是
// **首次命中块**（`shape_docrow` 按该块 `r.src_line`/`r.lines` 钉死）钉死的身份。
// Pass2 复用缓存行时若只改 `top` 不改 `src_line`，后续相同文本块的帧行身份就串到首块：
// 源偏移反查（hit `source_offset_at_local`）与选区高亮（`text_selection_hl_rects` 用
// `line_start(src_line)` 求交）都落在首块上——真机拖选重复行时「行4、行5 跟着行1一起
// 高亮」。修复：Pass2 拿到复用行后按当前块 k 重贴权威 `src_line`/`src_lines`
//（区间口径与 `assemble_docrow`/巨块占位一致）。
//
// 语料：逐源码行成块的 4 个独立段落，0/2/3 文本完全相同（同指纹），块 1 异文分隔。
// 首帧 band 只罩块 0（入缓存、身份 = 行 0）；第二帧全带重建 → 块 2/3 命中块 0 指纹。

/// 指到某行的元素局部命中点（y 在行内中部，x 在行首文字内）。
fn row_hit_point(
    frame: &crate::markdown::wysiwyg::element::Frame,
    row: &crate::markdown::wysiwyg::element::ShapedRow,
) -> gpui::Point<gpui::Pixels> {
    use gpui::point;
    point(
        px(f32::from(frame.padding) + 2.0),
        px(f32::from(row.top) + f32::from(row.lh) / 2.),
    )
}

/// 帧里按块序的段落行（滤掉可能的 Blank 等非文本块）。
fn paragraph_rows(
    frame: &crate::markdown::wysiwyg::element::Frame,
) -> Vec<&crate::markdown::wysiwyg::element::ShapedRow> {
    use crate::markdown::ir::IrKind;
    frame.rows.iter().filter(|r| r.kind == IrKind::Paragraph).collect()
}

#[gpui::test]
fn identical_text_blocks_keep_own_src_line_across_shape_cache(cx: &mut TestAppContext) {
    use super::{source_offset_at_local, text_selection_hl_rects};
    cx.update(crate::test_support::bind_local_keymaps);
    let item = "AAA\nBBB\nAAA\nAAA\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&item));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        // 帧 1：band 只罩首块 → 仅块 0 shape 并入缓存（cid 身份 = src_line 0）。
        let f1 = build_frame(ed.read(cx), window, px(600.), (0., 1.));
        assert_eq!(paragraph_rows(&f1).len(), 1, "首帧带内只有块 0");
        // 帧 2：全带重建 → 块 2/3 命中块 0 的内容指纹（caret 行 0 强制新鲜重排）。
        let f = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let rows = paragraph_rows(&f);
        assert_eq!(rows.len(), 4, "四行段落都在带内");
        // ① 每个块的 src_line / src_lines 各自正确（修复前 2/3 串成首块 0）。
        assert_eq!(rows[0].src_line, 0);
        assert_eq!(rows[1].src_line, 1);
        assert_eq!(rows[2].src_line, 2, "命中缓存的相同文本块 #2 仍保有自己的源行");
        assert_eq!(rows[3].src_line, 3, "命中缓存的相同文本块 #3 仍保有自己的源行");
        assert_eq!(rows[0].src_lines, 0..1);
        assert_eq!(rows[1].src_lines, 1..2);
        assert_eq!(rows[2].src_lines, 2..3);
        assert_eq!(rows[3].src_lines, 3..4);
        // ② 各自行内点击 → 映射回**各自行**的源字节区间（修复前串到首块 0 的区间）。
        for (i, r) in rows.iter().enumerate() {
            let off = source_offset_at_local(ed.read(cx), &f, row_hit_point(&f, r));
            let ls = ed.read(cx).line_start(i);
            let le = ed.read(cx).line_end(i);
            assert!(
                off >= ls && off < le,
                "点击块 {i} 应映射到第 {i} 行源区间 [{ls},{le})，得到 {off}"
            );
        }
        // ③ 选中首块整行区间 → 只有首块产生高亮矩形，相同文本的块 2/3 不产生
        //（修复前共享 src_line，块 2/3 跟首块一起高亮 = 真机「行4、行5 连动」）。
        let range = {
            let e = ed.read(cx);
            e.line_start(0)..e.line_end(0)
        };
        for (i, r) in rows.iter().enumerate() {
            let rects = text_selection_hl_rects(
                ed.read(cx),
                r,
                &range,
                f32::from(f.padding),
                0.,
            );
            if i == 0 {
                assert!(!rects.is_empty(), "首块被选中应高亮自身");
            } else {
                assert!(
                    rects.is_empty(),
                    "相同文本块 #{i} 不应因首块选区被高亮（修复前共享 src_line 串块）"
                );
            }
        }
    });
}

/// 反向对照：选**中间**相同块（#1）时也只有它自己高亮，首块（#0）与尾块（#3）不连动。
/// 与上面同一语料，从「缓存命中块」视角再锁一层：真实 hit 路径（非首建）两侧都不误亮。
#[gpui::test]
fn selecting_middle_identical_block_highlights_only_itself(cx: &mut TestAppContext) {
    use super::text_selection_hl_rects;
    cx.update(crate::test_support::bind_local_keymaps);
    let item = "AAA\nBBB\nAAA\nAAA\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&item));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        // 两帧：帧 1 只罩首块入缓存（身份=0），帧 2 全带让块 2/3 命中缓存。
        let _ = build_frame(ed.read(cx), window, px(600.), (0., 1.));
        let f = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let rows = paragraph_rows(&f);
        assert_eq!(rows.len(), 4);
        let range = {
            let e = ed.read(cx);
            e.line_start(2)..e.line_end(2)
        };
        for (i, r) in rows.iter().enumerate() {
            let rects = text_selection_hl_rects(
                ed.read(cx),
                r,
                &range,
                f32::from(f.padding),
                0.,
            );
            if i == 2 {
                assert!(!rects.is_empty(), "选中的块自身应高亮");
            } else {
                assert!(
                    rects.is_empty(),
                    "非选中块 #{i} 不应被高亮（修复前 #0/#3 与 #2 共享 src_line）"
                );
            }
        }
    });
}
