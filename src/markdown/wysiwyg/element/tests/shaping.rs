#[gpui::test]
fn large_doc_windowed_shaping_shapes_only_band(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    cx.update(crate::actions::bind_local_keymaps);
    // 首个超长折行段 + 大量单行段（≈2 万块）：**窄带只 shape 带内块**，
    // 其余用 BlockMeta 估算占位——大文档再也不会整篇每帧 shape。
    let head = "一二三四五六七八九十 ".repeat(300); // 长中文行，必然折行
    let body = "paragraph line of text 0123456789".to_string();
    let mut text = head;
    for _ in 0..20_000 {
        text.push('\n');
        text.push_str(&body);
    }
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        // 只覆盖顶部 ~80px 的窄带 → 真正 shape 的块应极少。
        let frame = build_frame(ed.read(cx), window, px(300.), (0., 80.));
        let n = frame.block_count();
        assert!(n > 10_000, "应有大量块：{n}");
        let shaped = frame.rows.len();
        assert!(
            shaped > 0 && shaped < 20,
            "窄带只应物化极少量带内整形行，实际 {shaped}/{n}"
        );
        // 带外块不再物化占位：`rows` 只含带内整形行（全非空 wrapped）。
        assert!(
            frame.rows.iter().all(|r| !r.wrapped.is_empty()),
            "rows 只应有带内整形行"
        );
        assert!(
            f32::from(frame.total) > 100_000.,
            "总高应由估算铺满（>.1M px），实际 {:.0}",
            f32::from(frame.total)
        );
    });
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        // 全篇带 → 全部块物化并 shape（与旧行为一致，小文档回归护栏）。
        let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
        assert_eq!(
            frame.rows.iter().filter(|r| r.wrapped.is_empty()).count(),
            0,
            "全带应全部 shape"
        );
    });
}

#[gpui::test]
fn off_band_reuses_shaped_height_no_jump(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    cx.update(crate::actions::bind_local_keymaps);
    // 短行 + 必折行的长段：先全带 shape（记录真实高），再窄带（长段被挤到带外）
    // ——带外高度必须复用缓存真值，而不是跳回估算（滚动“字体大小抖动”的根因）。
    let text = format!(
        "short line\n{}\n{}",
        "the quick brown fox jumps over the lazy dog ".repeat(40),
        "tail\n"
    );
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let (h_real, top_real) = vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
        let rows: Vec<_> = frame.rows.iter().map(|r| (r.height, r.top)).collect();
        assert!(
            rows.len() >= 3 && rows[1].0 > px(24.) * 3.,
            "长段应多行：{:?}",
            rows[1]
        );
        (rows[1].0, rows[1].1)
    });
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        // 窄带 (0,1)：第一条短行（24px）之后即带外——带外块不物化，走布局 `block_*`。
        let frame = build_frame(ed.read(cx), window, px(300.), (0., 1.));
        assert_eq!(frame.rows.len(), 1, "窄带 (0,1) 只应物化首行");
        assert_eq!(
            frame.block_height(1),
            f32::from(h_real),
            "带外长段高度应沿用缓存真值（不跳回估算）"
        );
        assert_eq!(
            frame.block_top(1),
            f32::from(top_real),
            "带外长段 top 也应一致"
        );
        // 全带再回来 → 高度仍一致（缓存跨帧稳定）。
        let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
        assert_eq!(frame.rows[1].height, h_real);
    });
}

#[gpui::test]
fn heading_off_band_reuses_shaped_height_no_jump(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::actions::bind_local_keymaps);
    // 标题行**带外**高度必须沿用「整形真值」（heading_line×行 + 8px pad），不能按
    // 正文行高重建——否则标题每次进出可视带 total 就变矮/变高，滚动到文档末端
    // 被反复 clamp → 页面「突然跳上去 + 抖动」。这是用户实测症状的根因。
    let text = "first paragraph line\n## Heading two\nsome more body text\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let (h_real, idx) = vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
        let idx = frame
            .rows
            .iter()
            .position(|r| matches!(r.kind, IrKind::Heading(_)))
            .expect("应有标题块");
        let h_real = frame.rows[idx].height;
        assert!(
            f32::from(h_real) > f32::from(frame.line_h),
            "标题块高应大于正文行高（heading_line + pad），实际 {}",
            f32::from(h_real)
        );
        (h_real, idx)
    });
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        // 窄带 (0,1)：首段在带内，标题被挤出带 → 未物化，但布局高度沿用缓存真值。
        let frame = build_frame(ed.read(cx), window, px(300.), (0., 1.));
        assert!(frame.rows.len() <= 1, "标题在带外（不物化），rows 只含首段");
        assert!(
            (frame.block_height(idx) - f32::from(h_real)).abs() < 0.5,
            "带外标题高度应沿用整形真值 {h_real:?}，实际 {:?}",
            frame.block_height(idx)
        );
        // 全带回来 → 仍一致。
        let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
        assert_eq!(frame.rows[idx].height, h_real);
    });
}

#[gpui::test]
fn inline_math_grown_row_off_band_reuses_shaped_height(cx: &mut TestAppContext) {
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    // 行内公式撑高的行：带内高 = grew 的 sub_heights[0]（> 行高），带外也必须
    // 沿用该真值，不能按「行数×正文行高」重建（否则缩回原高，滚动跳）。
    let text = "a $\u{5c}int_0^1 x^2 dx$ b\nsome more body text that is longer\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let h_real = vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let buf = image::RgbaImage::from_raw(40, 140, vec![0u8; 40 * 140 * 4]).unwrap();
        let img = std::sync::Arc::new(gpui::RenderImage::new(vec![image::Frame::new(buf)]));
        ed.read(cx).math_slots().borrow_mut().insert(
            super::super::inline_math_key("\\int_0^1 x^2 dx"),
            super::super::MathSlot::Ready(img, 20., 70.),
        );
        let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
        let row = &frame.rows[0];
        let h = row.height;
        assert!(
            f32::from(h) > f32::from(row.lh) + 0.5,
            "含超高公式的行应被撑高：h={h:?} > lh={:?}",
            row.lh
        );
        h
    });
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 窄带完全落在撑高行之下 → 第 0 行带外（不物化），仍应沿用整形真值。
        let lo = f32::from(h_real);
        let frame = build_frame(ed.read(cx), window, px(300.), (lo, lo + 0.1));
        assert!(
            !frame.rows.iter().any(|r| r.src_line == 0),
            "撑高行（块 0）应带外（不物化）"
        );
        assert!(
            (frame.block_height(0) - f32::from(h_real)).abs() < 0.5,
            "带外撑高行高度应沿用整形真值 {h_real:?}，实际 {:?}",
            frame.block_height(0)
        );
    });
}

#[gpui::test]
fn image_off_band_reuses_shaped_height(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::actions::bind_local_keymaps);
    // 已加载图片行：带内真实高（按宽高比），带外估算也必须同式——否则加载后
    // 图片行一进带就高/矮跳一次，滚动到文档末端现形。
    let text = "![figure](x.png)\nfirst paragraph line\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let h_real = vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        // 预置宽高比 2:1 的真实尺寸（模拟 preseed/加载完成）。
        ed.update(cx, |e, _| {
            e.set_image_size("x.png".into(), Some((200., 100.)));
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let row = &frame.rows[0];
        assert!(matches!(row.kind, IrKind::Image), "第 0 块应为图片行");
        let img = row.image.as_ref().expect("图片行应有渲染数据");
        let h = row.height;
        // 块高 = 图片盒高 + 题注间隙 + 题注（alt 可编辑）行高（与 shape 同式）。
        let expect_h = img.h
            + crate::config::markdown::IMAGE_CAPTION_GAP_PX
            + f32::from(ed.read(cx).theme.para_line());
        assert!(
            (f32::from(h) - expect_h).abs() < 0.5,
            "块高应=图高+间隙+题注行高，实际 {h:?} 期望 {expect_h:?}"
        );
        assert!(
            (f32::from(h) - crate::config::markdown::IMAGE_ROW_DEFAULT_H).abs() > 1.,
            "2:1 图片在此宽度下应高于占位默认高，h={h:?}"
        );
        h
    });
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        // 窄带完全落在图片行之下 → 图片行带外（不物化），仍沿用整形真值。
        let lo = f32::from(h_real);
        let frame = build_frame(ed.read(cx), window, px(600.), (lo, lo + 0.1));
        assert!(
            !frame.rows.iter().any(|r| r.src_line == 0),
            "图片行（块 0）应带外（不物化）"
        );
        assert!(
            (frame.block_height(0) - f32::from(h_real)).abs() < 0.5,
            "带外图片行高度应沿用整形真值 {h_real:?}，实际 {:?}",
            frame.block_height(0)
        );
    });
}

#[gpui::test]
fn image_size_lazy_detected_on_first_in_band_shape(cx: &mut TestAppContext) {
    // 回归（A-4）：图片尺寸不再于 load 路径（set_source_text）预读全篇——打开后
    // 带外图片保持「未测」；图片行**首次进带 shape** 才按需读一次真实尺寸并入缓存
    // （大文档 3456 图参考 ~75-130ms 首帧阻塞的根因）。
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/img测试.png");
    // 图片推到首帧带（0..2048）之外：验证 open 不清扫、滚到时才惰性读。
    let mut text = String::new();
    for i in 0..120 {
        text.push_str(&format!(
            "普通第 {i} 行正文足够长让每行的折行也占高一点 no wrap words\n"
        ));
    }
    text.push_str(&format!("![a]({p})\n![b]({p})\n"));
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 打开 + 首帧（图片在带外）：image_sizes 应仍为「未尝试」。
        assert_eq!(
            ed.read(cx).image_size(p),
            None,
            "open/首帧不应预读带外图片尺寸"
        );
    });
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 建覆盖全篇的带 → 图片行首次 shape 惰性读到真实尺寸（72×72 JPEG）。
        let f = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        assert_eq!(
            f.rows.iter().filter(|r| r.image.is_some()).count(),
            2,
            "两行图片都应物化"
        );
        let got = ed.read(cx).image_size(p);
        assert!(
            matches!(got, Some(Some((w, h))) if w > 0. && h > 0.),
            "图片行首次进带 shape 应读到真实尺寸（fixtures/img测试.png），实际 {got:?}"
        );
    });
}

#[gpui::test]
fn code_table_image_blocks_shape_with_payload(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "\
```rust
let a = 1;
let b = 2;
let c = 3;
```

| A | B |
| --- | --- |
| 1 | 2 |
| 3 | 4 |

![图](x.png)
";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let theme = ed.read(cx).theme.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));

        // 代码块：3 行逐行 shape、内容行可编辑、块高 = (行数+2)×code_line。
        let code = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Code)
            .expect("应有代码块");
        assert!(code.editable, "代码内容行应可编辑");
        assert_eq!(code.wrapped.len(), 3, "3 行代码应各一个 WrappedLine");
        // 每源码行起始渲染字节偏移（"let a/ b/ c = 3;" 各 10 字节 + 2 个 '\n' 分隔）。
        assert_eq!(code.multi_offsets, vec![0, 11, 22], "每行渲染起始字节");
        assert_eq!(
            code.byte_map.len(),
            "let a = 1;\nlet b = 2;\nlet c = 3;".len() + 1,
            "块级 byte_map = 整块渲染文本长 + 哨兵"
        );
        // 渲染「块级映射」往返：第 2 行中间列 → 块内源偏移 → 应指到 `let c = 3;`。
        let line2 = "let c = 3;";
        let ok = (0..line2.len()).all(|i| {
            let src = display_to_source(&code.byte_map, 22 + i);
            let col = src.saturating_sub(display_to_source(&code.byte_map, 22));
            line2.as_bytes().get(col) == line2.as_bytes().get(i)
        });
        assert!(ok, "第 2 行渲染字节应回落到该行对应源字节");
        let expect_h = f32::from(theme.code_line()) * 5.;
        assert!(
            (f32::from(code.height) - expect_h).abs() < 1.,
            "代码块高=(行数+2)×code_line，实际 {}",
            f32::from(code.height)
        );

        // 表格块：表头 2 列 × 数据 2 行，渲染数据齐备、单元格可编辑。
        let table = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Table)
            .expect("应有表格块");
        assert!(table.editable, "表格单元格应可编辑");
        let t = table.table.as_ref().expect("表格应有渲染数据");
        assert_eq!(t.header_texts.len(), 2);
        assert_eq!(t.rows_texts.len(), 2);
        assert_eq!(t.header_texts[0].as_ref(), "A");
        assert_eq!(t.rows_texts[1][1].as_ref(), "4");
        assert!(t.total_w > 0.);
        // 编辑排版：表头(1) + 数据行(2) = 3 个可见行，每行 2 格，源行号与块一致。
        let te = table.table_edit.as_ref().expect("表格应有编辑排版");
        assert_eq!(te.rows.len(), 3, "表头 + 2 数据行");
        for row in te.rows.iter() {
            assert_eq!(row.cells.len(), 2, "每行 2 格");
        }
        assert_eq!(te.rows[0].src_line, 6, "表头源行");
        assert_eq!(te.rows[1].src_line, 8, "首数据行源行（隔开分隔行）");
        assert_eq!(te.rows[2].src_line, 9, "次数据行源行");
        assert!(
            (f32::from(table.height)
                - (f32::from(table_block_height(&theme, 2))
                    + crate::config::markdown::TABLE_SCROLLBAR_RESERVE_PX))
                .abs()
                < 1.,
            "表格块高与估算同式（网格 + 滚动条预留带）"
        );

        // 图片块：占位框（未加载），题注（alt）可编辑、带 src + 源码映射。
        let img_row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Image)
            .expect("应有图片块");
        assert!(img_row.editable, "图片题注（alt）应可编辑");
        let im = img_row.image.as_ref().expect("图片应有渲染数据");
        assert_eq!(im.src.as_ref(), "x.png");
        // 题注行 = alt 文本，shape 成可编辑 wrapped 单行；byte_map = 源 `![` 之后。
        assert_eq!(
            img_row.wrapped.first().map(|w| w.text.as_ref()),
            Some("图"),
            "题注行文本应为 alt"
        );
        assert_eq!(
            img_row.byte_map.len(),
            "图".len() + 1,
            "alt 显示↔源映射长度"
        );
        assert_eq!(
            img_row.byte_map.first().copied(),
            Some(2),
            "alt 首显示字节应映射到源 `![` 之后（偏移 2）"
        );
        assert!(f32::from(img_row.height) > 0.);
    });
}

