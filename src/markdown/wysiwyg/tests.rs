//! `element` 的单元测试。原内联于 `element.rs`，抽取后经 `#[path]` 挂在
//! [`element`] 下语义不变：`super::` 仍指 `element`。
    use super::{
        build_frame, caret_geometry, display_to_source, source_offset_at, source_to_display,
        table_block_height, table_cell_x0, table_row_top, WyEditor,
    };
    use gpui::{
        AppContext, Context, Entity, Render, TestAppContext, VisualTestContext, Window, point, px,
    };

    struct WyHost {
        ed: Entity<WyEditor>,
    }
    impl Render for WyHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            self.ed.clone()
        }
    }

    #[test]
    fn byte_map_round_trip_is_identity_for_plain_text() {
        // 普通段落：渲染 == 源（map[k] = k，末位哨兵 = len）。
        let map: Vec<usize> = (0..=5).collect(); // "abcde" + 哨兵
        for j in 0..=5 {
            let src = display_to_source(&map, j);
            assert_eq!(src, j);
            assert_eq!(
                source_to_display(&map, src),
                j,
                "点渲染列 {j} → 源 {src} → 应回渲染列 {j}"
            );
        }
    }

    #[test]
    fn hidden_markers_map_round_trip() {
        // `**bold**`：渲染文本 "bold"，map = [2,3,4,5,6]（哨兵 6 = 源段尾）。
        let map: Vec<usize> = vec![2, 3, 4, 5, 6];
        for j in 0..4 {
            let src = display_to_source(&map, j);
            assert_eq!(src, 2 + j);
            assert_eq!(
                source_to_display(&map, src),
                j,
                "点渲染列 {j} 与光标回渲染列应一致"
            );
        }
        // 光标在段尾（源 6）→ 渲染列 4（行尾）；点行尾（渲染列 4）→ 源段尾 6。
        assert_eq!(source_to_display(&map, 6), 4);
        assert_eq!(display_to_source(&map, 4), 6);
    }

    #[gpui::test]
    fn inline_code_pill_ranges_expand_over_whitespace(cx: &mut TestAppContext) {
        // 行内代码（反引号）在 shape 期记录渲染字节区间（已扩展空白）供 paint 自绘圆角
        // pill——与预览 `TextTrack::paint_code_ranges` 同一行为源。用贴题原句验证 `we`。
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "我们`we`可以将它理解成一个**标识接口**\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let theme = ed.read(cx).theme.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let para = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Paragraph)
                .expect("应有段落行");
            assert!(
                !para.code_ranges.is_empty(),
                "含行内代码的文本行应记录 pill 区间"
            );
            // 渲染文本 = `我们we可以将它理解成一个标识接口`，`we` 在字节 [6..8)
            // （我们=2×3 字节）；两侧不是空白 → 不扩展。
            assert_eq!(
                para.code_ranges[0],
                6..8,
                "`we` pill 应恰好覆盖渲染字节 6..8"
            );
            // 圆角 pill 底色单一源：与预览 `StyleParams.code_inline_bg` 同值。
            let p = crate::markdown::view::style::StyleParams::from_theme_with_font(
                cx,
                Some(f32::from(theme.font_size)),
            );
            assert_eq!(theme.code_inline_bg, p.code_inline_bg, "pill 底色抄自 StyleParams");
        });
    }

    #[gpui::test]
    fn wrapped_row_click_on_second_visual_line_lands_in_second_half(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        // 长文本，在 300px 宽度窗口内必然折成 ≥2 个可视行。
        let words = "1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678 1234 5678";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(words));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (_, o2) = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            assert_eq!(frame.rows.len(), 1, "单段文档应只有一行（块）");
            let w0 = frame.rows[0]
                .wrapped
                .first()
                .expect("行应有 shaped 结果");
            assert!(
                w0.wrap_boundaries().len() >= 1,
                "长文本应折行：wrap 边界 {} 个",
                w0.wrap_boundaries().len()
            );
            let line_h = f32::from(frame.line_h);
            let pad = f32::from(frame.padding);
            let top = f32::from(frame.rows[0].top);
            // 第一可视行与第二可视行各自的「行内靠左」点击（x 相同、y 分属两个可视行）。
            let p1 = point(px(pad + 5.), px(top + line_h * 0.5));
            let p2 = point(px(pad + 5.), px(top + line_h * 1.5));
            let o1 = source_offset_at(ed.read(cx), &frame, p1);
            let o2 = source_offset_at(ed.read(cx), &frame, p2);
            // 第二可视行的点击必须落在第一可视行文本之后（而非被映射回第一行）。
            assert!(
                o2 > o1,
                "第二可视行点击的源偏移 {o2} 应大于第一可视行行尾 {o1}"
            );
            (o1, o2)
        });
        // 用 `source_offset_at` 反查出的第二可视行源偏移直接落光标，验证
        // `caret_geometry` 把光标竖条画在第二可视行（top = 块首 + 一行高）。
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| e.set_caret(o2));
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            let (on, _, caret_top) = caret_geometry(ed.read(cx), &frame);
            let line_h = f32::from(frame.line_h);
            let top = f32::from(frame.rows[0].top);
            let top2 = f32::from(caret_top);
            assert!(on, "折行行应可放置光标");
            assert!(
                (top2 - (top + line_h)).abs() < 0.5,
                "光标竖条应画在第二可视行：块首 top={top}，得到 {top2}"
            );
        });
    }

    #[gpui::test]
    fn large_doc_windowed_shaping_shapes_only_band(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        cx.update(gpui_component::init);
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
        cx.update(gpui_component::init);
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
            let rows: Vec<_> = frame
                .rows
                .iter()
                .map(|r| (r.height, r.top))
                .collect();
            assert!(rows.len() >= 3 && rows[1].0 > px(24.) * 3., "长段应多行：{:?}", rows[1]);
            (rows[1].0, rows[1].1)
        });
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 窄带 (0,1)：第一条短行（24px）之后即带外——带外块不物化，走布局 `block_*`。
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 1.));
            assert_eq!(frame.rows.len(), 1, "窄带 (0,1) 只应物化首行");
            assert_eq!(
                frame.block_height(1), f32::from(h_real),
                "带外长段高度应沿用缓存真值（不跳回估算）"
            );
            assert_eq!(frame.block_top(1), f32::from(top_real), "带外长段 top 也应一致");
            // 全带再回来 → 高度仍一致（缓存跨帧稳定）。
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            assert_eq!(frame.rows[1].height, h_real);
        });
    }

    #[gpui::test]
    fn heading_off_band_reuses_shaped_height_no_jump(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
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
        cx.update(gpui_component::init);
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
        cx.update(gpui_component::init);
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
            assert_eq!(f32::from(h), img.h, "块高应等于显示图高");
            assert!(
                (f32::from(h)
                    - crate::config::markdown::IMAGE_ROW_DEFAULT_H)
                    .abs()
                    > 1.,
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
    fn code_table_image_blocks_shape_with_payload(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
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
            for (_, row) in te.rows.iter().enumerate() {
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

            // 图片块：占位框（未加载），带 src/alt。
            let img_row = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Image)
                .expect("应有图片块");
            assert!(!img_row.editable);
            let im = img_row.image.as_ref().expect("图片应有渲染数据");
            assert_eq!(im.src.as_ref(), "x.png");
            assert_eq!(im.alt.as_ref(), "图");
            assert!(f32::from(img_row.height) > 0.);
        });
    }

    #[gpui::test]
    fn theme_mirrors_style_params_single_source(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("# t\n"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_window, cx| {
            let ed = host.read(cx).ed.clone();
            let theme = ed.read(cx).theme.clone();
            // WYSIWYG 主题必须逐项抄自预览单一排版源 StyleParams（改样式只动一处）。
            let p =
                crate::markdown::view::style::StyleParams::from_theme_with_font(
                    cx,
                    Some(f32::from(theme.font_size)),
                );
            assert_eq!(theme.text, p.fg, "正文色应抄自 StyleParams");
            assert_eq!(theme.code_bg, p.code_bg, "代码块背景应抄自 StyleParams");
            assert_eq!(
                theme.code_inline_fg, p.code_inline_fg,
                "行内代码文字色应抄自 StyleParams"
            );
            assert_eq!(
                theme.code_inline_bg, p.code_inline_bg,
                "行内代码 pill 底色应抄自 StyleParams"
            );
            assert_eq!(theme.divider, p.divider, "分隔线应抄自 StyleParams");
            assert_eq!(theme.table_head_bg, p.table_head_bg, "表头底色应抄自 StyleParams");
            assert_eq!(theme.para_line(), p.para_line, "正文行高应抄自 StyleParams");
            assert_eq!(theme.code_size(), p.code_size, "代码字号应抄自 StyleParams");
            assert_eq!(theme.code_line(), p.code_line, "代码行高应抄自 StyleParams");
            for lv in 1..=6 {
                let (fs, lh) = p.heading_metrics(lv);
                assert_eq!(theme.heading_fs(lv), fs, "h{lv} 字号应抄自 StyleParams");
                assert_eq!(theme.heading_line(lv), lh, "h{lv} 行高应抄自 StyleParams");
            }
        });
    }

    #[gpui::test]
    fn image_load_updates_size_and_rebuilds_frame(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let text = "![图](x.png)\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let img_row = frame
                .rows
                .iter()
                .find(|r| r.image.is_some())
                .expect("应有图片块");
            let img = img_row.image.as_ref().unwrap();
            // 未加载：占位高 = IMAGE_ROW_DEFAULT_H。
            assert_eq!(
                f32::from(img_row.height),
                crate::config::markdown::IMAGE_ROW_DEFAULT_H
            );
            // 模拟加载完成：设定源像素尺寸（200×100 = 宽高比 2:1）→ 重建帧 →
            // 显示宽取内容宽、高按宽高比。
            ed.update(cx, |e, _| {
                e.set_image_size(img.src.clone(), Some((200., 100.)));
            });
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let img_row = frame
                .rows
                .iter()
                .find(|r| r.image.is_some())
                .expect("应有图片块");
            let img = img_row.image.as_ref().unwrap();
            assert!(
                (img.h - img.w / 2.).abs() < 1.,
                "图高应保持宽高比 2:1，w={} h={}",
                img.w,
                img.h
            );
            assert!(
                (f32::from(img_row.height) - img.h).abs() < 1.,
                "块高应等于显示图高 {}",
                img.h
            );
        });
    }

    #[gpui::test]
    fn ime_composition_replaces_previous_marked_text(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        // 回归「输入“你好”→ “nnini'hni'hao你好”」：Windows 组合更新不携带区间，
        // 必须替换上一次 marked，而不是一前一后堆叠。
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("你好"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                e.set_caret("你好".len());
                e.replace_and_mark_utf16(None, "n", None, cx);
                assert_eq!(e.text().as_ref(), "你好n", "组合首键插在光标后");
                e.replace_and_mark_utf16(None, "ni", None, cx);
                assert_eq!(e.text().as_ref(), "你好ni", "组合更新应替换旧 marked，不得残留 n");
                e.replace_and_mark_utf16(None, "nih", None, cx);
                assert_eq!(e.text().as_ref(), "你好nih", "再次更新仍替换");
                // 提交：`replace_text_utf16(None)` 落在 marked 上并清掉 marked。
                e.replace_text_utf16(None, "你好", cx);
                assert_eq!(e.text().as_ref(), "你好你好", "提交文本替换组合占位，不重复");
                // 新组合不得再把“已提交中文”当旧 marked 替换。
                e.replace_and_mark_utf16(None, "n", None, cx);
                e.replace_and_mark_utf16(None, "ni", None, cx);
                assert_eq!(e.text().as_ref(), "你好你好ni", "新组合不重复旧提交");
            });
        });
    }

    #[gpui::test]
    fn ime_new_selected_range_is_relative_to_marked(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("abc"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                e.set_caret(0);
                // AppKit setMarkedText 的 selectedRange 是**相对组合文本**的偏移，
                // 不是文档绝对偏移——光标应落在偏移 1（“好”前），而非文档开头。
                e.replace_and_mark_utf16(None, "你好", Some(1..2), cx);
                assert_eq!(e.text().as_ref(), "你好abc", "无选区时组合插入光标处");
                assert_eq!(
                    e.caret(),
                    3,
                    "光标 = 组合起点 0 + 相对偏移 1 的字节（“好”前），而非文档绝对 1"
                );
            });
        });
    }

    #[gpui::test]
    fn selection_drag_delete_and_select_all(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("一二三五六"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                e.begin_selection_at(0);
                e.extend_selection_to("一二三".len());
                assert_eq!(e.selection(), Some(0.."一二三".len()), "拖选得源字节区间");
                // 输入/删除应作用于选区。
                e.backspace(cx);
                assert_eq!(e.text().as_ref(), "五六", "Backspace 应删除整段选区");
                assert_eq!(e.selection(), None);
                // 全选 + 范围替换。
                e.select_all();
                assert_eq!(e.selection(), Some(0.."五六".len()));
                e.replace_range(e.selection().unwrap(), "你", cx);
                assert_eq!(e.text().as_ref(), "你", "全选范围替换");
            });
        });
    }

    #[gpui::test]
    fn quote_blocks_are_editable_with_indent(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("# 标题\n> 引用内容\n正文"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let quote = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Quote)
                .expect("应有引述行");
            assert!(quote.editable, "引述应可编辑（byte_map 非空）");
            assert!(f32::from(quote.x_off) > 0., "引述应有文本缩进");
            // 在引述渲染文本内落点 → 源字节在 “引用内容” 上，可正常输入。
            let off = ed.read(cx).text().as_ref().find("引").unwrap();
            ed.update(cx, |e, cx| {
                e.set_caret(off + 3);
                e.replace_range(off + 3..off + 6, "X", cx);
            });
            assert!(
                ed.read(cx).text().as_ref().contains("引X"),
                "引述行应可编辑：{}",
                ed.read(cx).text().as_ref()
            );
        });
    }

    #[gpui::test]
    fn code_block_click_lands_on_correct_source_line(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "\
```rust
let a = 1;
let b = 22;
let c = 333;
```
尾段
";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code)
                .expect("应有代码块");
            assert!(code.editable, "代码内容行应可编辑");
            let lh = f32::from(code.lh);
            let top = f32::from(code.top);
            let pad = f32::from(frame.padding);
            // 点击三个代码行的中部（y = 块首 + 首空白带 + k×行高）→ 逐行命中内容。
            for (k, expects) in
                [(0usize, "let a = 1;"), (1, "let b = 22;"), (2, "let c = 333;")]
            {
                let p = point(px(pad + 30.), px(top + lh * 1.5 + lh * k as f32));
                let off = source_offset_at(ed.read(cx), &frame, p);
                let (line, _) = ed.read(cx).row_col(off);
                assert_eq!(line, 1 + k, "点击第 {k} 个代码行应命中源行 {}", 1 + k);
                let ls = ed.read(cx).line_start(line);
                let le = ed.read(cx).line_end(line);
                assert_eq!(
                    &ed.read(cx).text().as_ref()[ls..le],
                    expects,
                    "命中文本应为该行内容"
                );
            }
        });
    }

    #[gpui::test]
    fn code_block_overflow_shows_hscroll_and_shifts_hit(cx: &mut TestAppContext) {
        // 长代码行（nowrap 超出窄面板）→ 底部横向滚动条；滚动偏移使**同一可视 x**
        // 命中更多源列（paint 平移的逆映射）；拖动滚动条更新偏移（视图本地，不写源码）。
        use crate::markdown::ir::IrKind;
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, size};
        cx.update(gpui_component::init);
        let text = format!("```rust\n{}\nb\n```\n", "a".repeat(400));
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(600.), px(600.)),
                )));
            });
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code && !r.multi_offsets.is_empty())
                .expect("应有代码块");
            let blk = code.src_line;
            let (bar_x, bar_w, _, _, max_scroll) =
                super::code_hscroll_geo(ed.read(cx), &frame, code)
                    .expect("长行应超宽显示滚动条");
            assert!(max_scroll > 50., "内容应显著超宽");
            assert!(bar_w > 10., "滚动条轨道应有宽度");
            // 首内容行中部 y（组 0 = 首空白带 + 首行）。
            let y = f32::from(code.top) + f32::from(code.lh) * 1.5;
            let view_x = f32::from(frame.padding) + f32::from(code.x_off) + 60.;
            let p = point(px(view_x), px(y));
            let off0 = super::source_offset_at(ed.read(cx), &frame, p);
            // 滚到最右（大偏移）→ 同一可视 x 命中更后的源列。
            ed.update(cx, |e, _| e.set_code_h_scroll(blk, max_scroll));
            let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let off1 = super::source_offset_at(ed.read(cx), &f2, p);
            let (_, col0) = ed.read(cx).row_col(off0);
            let (_, col1) = ed.read(cx).row_col(off1);
            assert!(
                col1 > col0,
                "横向滚动后同一 x 应命中更后源列：col0={col0} col1={col1}"
            );
            // 拖动滚动条：点击轨道 90% → 偏移大；拖到 10% → 偏移小；松开清除拖拽态。
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(f2)));
            let band_top = f32::from(code.top) + f32::from(code.height)
                - crate::config::layout::CODE_HSCROLL_BAND;
            let band_y = band_top + crate::config::layout::CODE_HSCROLL_BAND / 2.;
            let down_x = bar_x + bar_w * 0.9;
            let up_x = bar_x + bar_w * 0.1;
            let ev_down = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(down_x), px(band_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev_down, window, cx));
            let scrolled = ed.read(cx).code_h_scroll_of(blk);
            assert!(scrolled > 0., "按下轨道应产生滚动偏移：{scrolled}");
            let ev_move = gpui::MouseMoveEvent {
                position: point(px(up_x), px(band_y)),
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            ed.update(cx, |e, cx| e.on_mouse_move(&ev_move, cx));
            let scrolled2 = ed.read(cx).code_h_scroll_of(blk);
            assert!(
                scrolled2 < scrolled,
                "拖到 10% 应减小偏移：{scrolled2} < {scrolled}"
            );
            let ev_up = MouseUpEvent {
                button: MouseButton::Left,
                position: point(px(up_x), px(band_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
            };
            ed.update(cx, |e, cx| e.on_mouse_up(&ev_up, cx));
            assert!(
                ed.read(cx).code_h_drag.borrow().is_none(),
                "松开应清除拖拽态"
            );
            // 视图本地状态：源码未变。
            assert_eq!(ed.read(cx).text().as_ref(), text);
        });
    }

    #[gpui::test]
    fn table_overflow_shows_hscroll_and_track_drag(cx: &mut TestAppContext) {
        // 用户报「WYSIWYG 表格没有横向滚动条」→ 对齐 preview `markdown/view` 的表格
        // 溢出横向滚动：超宽表格底部出滚动条，轨道点击跳转、滑块可拖（拖动不写源码）。
        use crate::markdown::ir::IrKind;
        use gpui::{
            Bounds, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, size,
        };
        cx.update(gpui_component::init);
        let text = "|AAAAAAAAAAAAAAAAAAAAAA|BBBBBBBBBBBBBBBBBBBBBB|CCCCCCCCCCCCCCCCCCCCCC|DDDDDDDDDDDDDDDDDDDDDD|EEEEEEEEEEEEEEEEEEEEEEE|\n|---|---|---|---|---|\n|a1|b1|c1|d1|e1|\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(300.), px(600.)),
                )));
            });
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            let (blk, bar_x, bar_w, thumb_x, _thumb_w, max_scroll, ctop, ctop_h) = {
                let tbl = frame
                    .rows
                    .iter()
                    .find(|r| r.kind == IrKind::Table && r.table.is_some())
                    .expect("应有表格");
                let (bar_x, bar_w, thumb_x, thumb_w, max_scroll) =
                    super::table_hscroll_geo(ed.read(cx), &frame, tbl)
                        .expect("宽表格应显示横向滚动条");
                assert!(max_scroll > 100., "表格应显著超宽：max_scroll={max_scroll}");
                assert!(thumb_x <= bar_x + 1., "初态滑块应在轨道左端");
                (
                    tbl.src_line,
                    bar_x,
                    bar_w,
                    thumb_x,
                    thumb_w,
                    max_scroll,
                    f32::from(tbl.top),
                    f32::from(tbl.height),
                )
            };
            assert!(thumb_x <= bar_x + 1., "初态滑块应在轨道左端");
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame)));
            let band_top = ctop + ctop_h - crate::config::layout::CODE_HSCROLL_BAND;
            let band_y = band_top + crate::config::layout::CODE_HSCROLL_BAND / 2.;
            // 轨道命中（远离左端滑块）→ 跳转（不进拖动态）。
            let jump_x = bar_x + bar_w - 4.0;
            let (blk_hit, on_thumb, _) = super::table_hscrollbar_hit(
                ed.read(cx),
                &ed
                    .read(cx)
                    .frame
                    .borrow()
                    .as_ref()
                    .map(|f| &**f)
                    .unwrap(),
                point(px(jump_x), px(band_y)),
            )
            .expect("轨道末端应在滚动条带内");
            assert_eq!(blk_hit, blk);
            assert!(!on_thumb, "远离左端滑块的轨道点不应落在滑块上");
            let ev = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(jump_x), px(band_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev, window, cx));
            let jumped = ed.read(cx).table_h_scroll_of(blk);
            assert!(jumped > 0., "点击轨道应立刻跳转：{jumped}");
            assert!(jumped <= max_scroll, "{jumped} ≤ {max_scroll}");
            assert!(
                ed.read(cx).table_h_drag.borrow().is_none(),
                "轨道跳转不应进入拖动态"
            );
            // 点滑块（滚动后滑块位置）→ 拖到最左 → 偏移归零；松开清态；源码不变。
            let f2 = ed.read(cx).frame.borrow().clone().unwrap();
            let geo2 = super::table_hscroll_geo(
                ed.read(cx),
                &f2,
                &f2.rows.iter().find(|r| r.src_line == blk).unwrap(),
            )
            .unwrap();
            let (_, _, thumb_x2, thumb_w2, _) = geo2;
            let ev2 = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(thumb_x2 + thumb_w2 / 2.), px(band_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev2, window, cx));
            assert!(
                ed.read(cx).table_h_drag.borrow().is_some(),
                "点滑块应进入拖动态"
            );
            let up_x = bar_x + bar_w * 0.05;
            let ev_move = MouseMoveEvent {
                position: point(px(up_x), px(band_y)),
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            ed.update(cx, |e, cx| e.on_mouse_move(&ev_move, cx));
            let after = ed.read(cx).table_h_scroll_of(blk);
            assert!(after < jumped, "拖到最左应减小偏移：{after} < {jumped}");
            let ev_up = MouseUpEvent {
                button: MouseButton::Left,
                position: point(px(up_x), px(band_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
            };
            ed.update(cx, |e, cx| e.on_mouse_up(&ev_up, cx));
            assert!(
                ed.read(cx).table_h_drag.borrow().is_none(),
                "松开应清除拖拽态"
            );
            assert_eq!(
                ed.read(cx).text().as_ref(),
                text,
                "表格横向滚动是视图本地状态，不应写源码"
            );
        });
    }

    #[gpui::test]
    fn table_hscroll_shifts_hit_and_caret(cx: &mut TestAppContext) {
        // 横向滚动后：同一可视 x 应命中**更后列**的单元格（命中反查 +scroll），
        // 光标条 x 应按平移逆映射 −scroll 收敛（否则点击/光标错位）。
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "|AAAAAAAAAAAAAAAAAAAAAA|BBBBBBBBBBBBBBBBBBBBBB|CCCCCCCCCCCCCCCCCCCCCC|DDDDDDDDDDDDDDDDDDDDDD|EEEEEEEEEEEEEEEEEEEEEEE|\n|---|---|---|---|---|\n|a1|b1|c1|d1|e1|\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            let tbl = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Table && r.table.is_some())
                .expect("应有表格");
            let blk = tbl.src_line;
            let (_, _, _, _, max_scroll) =
                super::table_hscroll_geo(ed.read(cx), &frame, tbl).expect("宽表格应超宽");
            let pad = f32::from(frame.padding);
            let t = tbl.table.as_ref().unwrap();
            let row_ix = 1usize; // 数据行
            let rh = f32::from(t.row_h);
            let y = f32::from(tbl.top) + super::table_row_top(t, row_ix) + rh / 2.;
            let view_x = pad + 40.;
            let p = point(px(view_x), px(y));
            let (line0, col0) = {
                let off0 = super::source_offset_at(ed.read(cx), &frame, p);
                let (l, c) = ed.read(cx).row_col(off0);
                (l, c)
            };
            // 滚到最右 → 同一可视 x 应命中更后列的单元格。
            ed.update(cx, |e, _| e.set_table_h_scroll(blk, max_scroll));
            let f2 = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            let off1 = super::source_offset_at(ed.read(cx), &f2, p);
            let (line1, col1) = ed.read(cx).row_col(off1);
            assert_eq!(line0, line1, "横向滚动不应改变命中行");
            assert!(col1 > col0, "同一 x 滚动后应命中更后列：col0={col0} col1={col1}");
            // 光标几何：同一源列（off1 在最后一列的渲染列）在 scroll=0 与 scroll=max
            // 下 x 应相差 ≈ max_scroll（paint 平移的逆映射）。
            ed.update(cx, |e, _| e.set_caret(off1));
            let (on_max, x_sc, _) = super::caret_geometry(ed.read(cx), &f2);
            assert!(on_max, "滚动后该格应可放光标");
            ed.update(cx, |e, _| e.set_table_h_scroll(blk, 0.));
            let f0 = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            let (on0, x_un, _) = super::caret_geometry(ed.read(cx), &f0);
            assert!(on0, "未滚动该格应可放光标");
            let d = f32::from(x_un - x_sc);
            assert!(
                (d - max_scroll).abs() < 2.,
                "滚动前后光标 x 差应 ≈ max_scroll（{max_scroll:.0}），实际 {d:.1}"
            );
        });
    }

    #[gpui::test]
    fn table_short_has_inert_scrollbar_band(cx: &mut TestAppContext) {
        // 窄表格：滚动条带恒定存在（preview `markdown/view` 的 Always 语义），但
        // `max_scroll = 0` → 惰性不可滚动，陈旧滚动值被 `table_hscroll_clamped` 忽略，
        // 且预留带位于网格之下（不遮内容）。
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "| A | B |\n|---|---|\n| 1 | 2 |\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let tbl = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Table && r.table.is_some())
                .expect("应有表格");
            let geo = super::table_hscroll_geo(ed.read(cx), &frame, tbl).expect("表格总有滚动条带");
            let (_, _, thumb_x, thumb_w, max_scroll) = geo;
            assert_eq!(max_scroll, 0., "窄表格不应有横向行程");
            assert!(
                (thumb_w - thumb_x).abs() > 0. && thumb_w >= 48.,
                "窄表格滑块应占满轨道（惰性）"
            );
            ed.update(cx, |e, _| e.set_table_h_scroll(tbl.src_line, 999.));
            assert_eq!(
                super::table_hscroll_clamped(ed.read(cx), &frame, tbl),
                0.,
                "内容不超宽时应忽略陈旧滚动值"
            );
            // 预留带高度恒定：块高 = 网格高 + 预留带（带外 estimate 同式 → 滚动不跳）。
            let grid_h = f32::from(super::table_block_height(
                &ed.read(cx).theme,
                tbl.table.as_ref().unwrap().rows_texts.len(),
            ));
            assert!(
                (f32::from(tbl.height) - grid_h - crate::config::markdown::TABLE_SCROLLBAR_RESERVE_PX)
                    .abs()
                    < 0.5,
                "表格块高应为网格高 + 滚动条预留带"
            );
        });
    }

    #[gpui::test]
    fn code_block_short_lines_have_no_hscroll(cx: &mut TestAppContext) {
        // 短代码行不超宽 → 无横向滚动条（`code_hscroll_geo` None），且陈旧滚动值被
        // `code_hscroll_clamped` 忽略（宽窗口下长行不再偏移）。防「内容超出代码块」回归。
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "```rust\nlet a = 1;\nlet b = 22;\n```\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code && !r.multi_offsets.is_empty())
                .expect("应有代码块");
            assert!(
                super::code_hscroll_geo(ed.read(cx), &frame, code).is_none(),
                "短行不应出现横向滚动条"
            );
            // 即便手动塞一个陈旧滚动值：paint/命中共用的 clamped 也应归 0。
            ed.update(cx, |e, _| e.set_code_h_scroll(code.src_line, 999.));
            assert_eq!(
                super::code_hscroll_clamped(ed.read(cx), &frame, code),
                0.,
                "内容不超宽时应忽略陈旧滚动值"
            );
            assert!(super::code_hscroll_geo(ed.read(cx), &frame, code).is_none());
        });
    }

    #[gpui::test]
    fn code_block_hscroll_track_click_jumps(cx: &mut TestAppContext) {
        // view 语义：点击**轨道**（不在滑块上）→ 滑块跳到点击处；点击**滑块** → 拖动。
        // 旧实现点轨道空白处无反应（手感差），此处回归点击轨道即跳转。
        use crate::markdown::ir::IrKind;
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, size};
        cx.update(gpui_component::init);
        let text = format!("```rust\n{}\nb\n```\n", "a".repeat(400));
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(600.), px(600.)),
                )));
            });
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let (blk, ctop, ctop_h, bar_x, bar_w, _thumb_x, _thumb_w, max_scroll) = {
                let code = frame
                    .rows
                    .iter()
                    .find(|r| r.kind == IrKind::Code && !r.multi_offsets.is_empty())
                    .expect("应有代码块");
                let (bar_x, bar_w, thumb_x, thumb_w, max_scroll) =
                    super::code_hscroll_geo(ed.read(cx), &frame, code).unwrap();
                // 初始 scroll=0 → 滑块贴着轨道左端。
                assert!(thumb_x <= bar_x + 1., "初态滑块应在左端");
                (
                    code.src_line,
                    f32::from(code.top),
                    f32::from(code.height),
                    bar_x,
                    bar_w,
                    thumb_x,
                    thumb_w,
                    max_scroll,
                )
            };
            // 把刚建的帧放入编辑器，供 `on_mouse_down`/命中读取。
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame)));
            let band_top = ctop + ctop_h - crate::config::layout::CODE_HSCROLL_BAND;
            let band_y = band_top + crate::config::layout::CODE_HSCROLL_BAND / 2.;
            // 点击轨道**末端**（远离初始左端滑块，必不在滑块上）→ 直接跳转（不进拖动态）。
            let jump_x = bar_x + bar_w - 4.0;
            let jump_y = band_y;
            // 断言**点击前**命中的是轨道（非滑块）。
            let (blk_pre, on_thumb_pre, _) = super::code_hscrollbar_hit(
                ed.read(cx),
                &ed.read(cx).frame.borrow().as_ref().map(|f| &**f).unwrap(),
                point(px(jump_x), px(jump_y)),
            )
            .expect("轨道末应在滚动条带内");
            assert_eq!(blk_pre, blk);
            assert!(!on_thumb_pre, "点击末端的点不应落在初始滑块上");
            let ev = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(jump_x), px(jump_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev, window, cx));
            let jumped = ed.read(cx).code_h_scroll_of(blk);
            assert!(jumped > 0., "点击轨道应立刻跳转（不拖也动）：{jumped}");
            assert!(
                ed.read(cx).code_h_drag.borrow().is_none(),
                "轨道跳转不应进入拖动态"
            );
            assert!(
                jumped <= max_scroll,
                "跳转偏移应不超过 max_scroll（{jumped} ≤ {max_scroll}）"
            );
            // 再点滑块中心 → 进入拖动态，拖动到左端归零。
            let geo2 = super::code_hscroll_geo(
                ed.read(cx),
                &ed.read(cx).frame.borrow().as_ref().map(|f| &**f).unwrap(),
                &ed.read(cx).frame.borrow().as_ref().unwrap().rows.iter().find(|r| r.src_line == blk).unwrap(),
            ).unwrap();
            let (_, _, thumb_x2, thumb_w2, _) = geo2;
            let thumb_cx = thumb_x2 + thumb_w2 / 2.;
            let ev2 = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(thumb_cx), px(jump_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev2, window, cx));
            assert!(
                ed.read(cx).code_h_drag.borrow().is_some(),
                "点滑块应进入拖动态"
            );
            let ev_move = MouseMoveEvent {
                position: point(px(bar_x + 6.), px(jump_y)),
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            ed.update(cx, |e, cx| e.on_mouse_move(&ev_move, cx));
            let dragged = ed.read(cx).code_h_scroll_of(blk);
            assert!(dragged < jumped, "拖到左端应减小偏移：{dragged} < {jumped}");
            let ev_up = MouseUpEvent {
                button: MouseButton::Left,
                position: point(px(bar_x + 6.), px(jump_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
            };
            ed.update(cx, |e, cx| e.on_mouse_up(&ev_up, cx));
            assert!(ed.read(cx).code_h_drag.borrow().is_none(), "松开应清拖拽态");
        });
    }

    #[gpui::test]
    fn code_block_hscroll_thumb_drag_tracks_pointer_without_jump(cx: &mut TestAppContext) {
        // 回归「不跟手」：旧实现 grab 从滑块**中心**量、却按**左缘**参与换算 →
        // 按下滑块瞬间 scroll 前跳半个滑块宽，拖动手感像「先在别处吸住再跑」。
        // 修后语义（对齐 gpui-component drag_pos）：grab = 指针相对滑块**左缘**偏移，
        // 按下不跳档、拖动时滑块钉住抓取点 1:1 跟随指针（thumb_x = px − grab）。
        use crate::markdown::ir::IrKind;
        use gpui::{
            Bounds, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, size,
        };
        cx.update(gpui_component::init);
        let text = format!("```rust\n{}\nb\n```\n", "a".repeat(400));
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(600.), px(600.)),
                )));
            });
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code && !r.multi_offsets.is_empty())
                .expect("应有代码块");
            let code_top = f32::from(code.top);
            let code_h = f32::from(code.height);
            let blk = code.src_line;
            let (_, _, _, _, max_scroll) = super::code_hscroll_geo(ed.read(cx), &frame, code).unwrap();
            // 滚动到中段，滑块离开左端（否则抓取点恒 0，测不出左缘语义）。
            let mid = max_scroll * 0.5;
            ed.update(cx, |e, _| {
                e.set_code_h_scroll(blk, mid);
                *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
            });
            // geo 是纯函数（读实时 scroll），用编辑器里那份帧反复重算滑块几何。
            let geo_of = |e: &WyEditor| -> (f32, f32, f32, f32, f32) {
                let f = e.frame.borrow();
                let fr = f.as_ref().map(|r| &**r).unwrap();
                let r = fr.rows.iter().find(|r| r.src_line == blk).unwrap();
                super::code_hscroll_geo(e, fr, r).unwrap()
            };
            let (bar_x, bar_w, thumb_x, thumb_w, max_scroll) = geo_of(ed.read(cx));
            let travel = bar_w - thumb_w;
            let grab = thumb_w * 0.25; // 抓取点：滑块内距左缘 1/4 宽。
            let p0_x = thumb_x + grab;
            let band_top = code_top + code_h - crate::config::layout::CODE_HSCROLL_BAND;
            let band_y = band_top + crate::config::layout::CODE_HSCROLL_BAND / 2.;

            // 1) 按下滑块：scroll 必须**保持 mid**（不跳档），并进入拖动态。
            let down = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(p0_x), px(band_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&down, window, cx));
            assert_eq!(
                ed.read(cx).code_h_scroll_of(blk),
                mid,
                "按下滑块瞬间不应跳档（old grab=中心导致前跳半个滑块宽）"
            );
            assert!(ed.read(cx).code_h_drag.borrow().is_some(), "应在拖动态");

            // 2) 拖动 +Δ：滑块钉住抓取点 1:1 跟随 → thumb_x_new = p − grab（轨内 clamp）。
            let p1_x = p0_x + 60.0;
            let m1 = MouseMoveEvent {
                position: point(px(p1_x), px(band_y)),
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            ed.update(cx, |e, cx| e.on_mouse_move(&m1, cx));
            let (_, _, thumb_x2, _, _) = geo_of(ed.read(cx));
            let expect_x = (p1_x - grab).clamp(bar_x, bar_x + travel);
            assert!(
                (thumb_x2 - expect_x).abs() < 0.5,
                "拖动应钉住抓取点（thumb 应到 {expect_x:.1}，实际 {thumb_x2:.1}）"
            );
            let expect_scroll = (expect_x - bar_x) / travel * max_scroll;
            let scrolled = ed.read(cx).code_h_scroll_of(blk);
            assert!(
                (scrolled - expect_scroll).abs() < max_scroll * 0.01,
                "scroll 应与指针位移 1:1 对应：{scrolled:.1} ≈ {expect_scroll:.1}"
            );

            // 3) 继续拖出右端 → clamp 到 max_scroll（thumb 停在轨道端，参考语义）。
            let far = MouseMoveEvent {
                position: point(px(bar_x + bar_w + 100.), px(band_y)),
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            };
            ed.update(cx, |e, cx| e.on_mouse_move(&far, cx));
            assert!(
                (ed.read(cx).code_h_scroll_of(blk) - max_scroll).abs() < 0.5,
                "拖出右端应 clamp 到 max_scroll"
            );

            let up = MouseUpEvent {
                button: MouseButton::Left,
                position: point(px(p1_x), px(band_y)),
                modifiers: Modifiers::default(),
                click_count: 1,
            };
            ed.update(cx, |e, cx| e.on_mouse_up(&up, cx));
            assert!(ed.read(cx).code_h_drag.borrow().is_none(), "松开应清拖拽态");
        });
    }

    #[gpui::test]
    fn code_block_caret_tracks_clicked_content_line(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "\
```py
one
two
three
```
tail
";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (offsets, code_top, lh) = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code)
                .expect("应有代码块");
            assert!(code.editable);
            let e = ed.read(cx);
            let offs: Vec<usize> = (1usize..=3).map(|ln| e.line_start(ln) + 1).collect();
            (offs, f32::from(code.top), f32::from(code.lh))
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            for (k, off) in offsets.iter().enumerate() {
                ed.update(cx, |e, _| e.set_caret(*off));
                let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
                let (on, _, ctop) = caret_geometry(ed.read(cx), &frame);
                assert!(on, "代码内容行应可放光标");
                // caret 顶 = 块首 + 1 行首空白带 + k×行高。
                let expect = code_top + lh * (1 + k) as f32;
                assert!(
                    (f32::from(ctop) - expect).abs() < 0.6,
                    "k={k} 光标顶应 {expect}，得 {}",
                    f32::from(ctop)
                );
            }
        });
    }

    #[gpui::test]
    fn code_block_edit_typing_backspace_enter(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let text = "```rust\nlet a = 1;\nlet b = 2;\n```\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                // 光标落在首个代码行内输入 → 源文本在围栏内更新。
                let ls = e.line_start(1);
                e.set_caret(ls + 4);
                e.replace_range(e.caret()..e.caret(), "X", cx);
                assert!(
                    e.text().as_ref().starts_with("```rust\nlet Xa = 1;"),
                    "围栏内输入生效：{}",
                    e.text().as_ref()
                );
                // 退格删去 X（插入后光标已在 X 之后）。
                e.backspace(cx);
                assert_eq!(
                    &e.text().as_ref()[ls..ls + 9],
                    "let a = 1",
                    "退格删回原位，围栏未损坏"
                );
                // 末内容行行尾的删除是 no-op（保护闭合围栏）；再回车在闭合围栏前新起代码行。
                let e2 = e.line_end(2);
                e.set_caret(e2);
                e.delete(cx);
                assert!(
                    e.text().as_ref().contains("let b = 2;\n```"),
                    "末内容行行尾删除应被守卫（闭合围栏不被吃掉）：{}",
                    e.text().as_ref()
                );
                let e2 = e.line_end(2);
                e.set_caret(e2);
                e.newline(cx);
                assert!(
                    e.text().as_ref().contains("let b = 2;\n\n```"),
                    "回车应在闭合围栏前新起一代码行：{}",
                    e.text().as_ref()
                );
                // 首内容行行首退格是 no-op（保护开围栏）。
                e.set_caret(e.line_start(1));
                e.backspace(cx);
                assert!(
                    e.text().as_ref().starts_with("```rust\n"),
                    "首内容行行首退格应被守卫（开围栏不被吃掉）：{}",
                    e.text().as_ref()
                );
            });
        });
    }

    #[gpui::test]
    fn code_block_arrow_nav_skips_fences(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let text = "前段\n```rust\nlet a = 1;\nlet b = 2;\n```\n后段\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _cx| {
                // 首内容行行首向上 → 「前段」段尾，光标不落进开围栏。
                e.set_caret(e.line_start(2));
                e.move_up();
                let (l, _) = e.row_col(e.caret());
                assert_eq!(l, 0, "首内容行向上应到行 0（前段），得 {l}");
                // 「前段」行尾向右 → 跳过开围栏进代码首内容行（行 2）。
                e.set_caret(e.line_end(0));
                e.move_right();
                let (l, col) = e.row_col(e.caret());
                assert!(l == 2 && col == 0, "前段行尾向右应到首内容行行首，得 ({l},{col})");
                // 内容行内上下移动 → 光标行始终在内容行 2/3。
                e.set_caret(e.line_start(2) + 3);
                e.move_down();
                let (l, _) = e.row_col(e.caret());
                assert_eq!(l, 3, "内容行内下移应在行 3，得 {l}");
                e.move_up();
                let (l, _) = e.row_col(e.caret());
                assert_eq!(l, 2, "内容行内上移回行 2，得 {l}");
                // 末内容行行尾向右 → 跳过闭合围栏到「后段」（行 5）段首。
                e.set_caret(e.line_end(3));
                e.move_right();
                let (l, col) = e.row_col(e.caret());
                assert!(l == 5 && col == 0, "末内容行行尾向右应到后段行首，得 ({l},{col})");
                // 后段向上 → 进代码块末内容行（行 3）。
                e.set_caret(e.line_start(5) + 2);
                e.move_up();
                let (l, _) = e.row_col(e.caret());
                assert_eq!(l, 3, "后段向上应进代码块末内容行 3，得 {l}");
            });
        });
    }

    #[gpui::test]
    fn code_block_off_band_height_matches_shaped(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        // 一长段把代码块推到 300px 带之外 → 带外估算高必须与整形式高度一致（防滚动跳变）。
        let head = "the quick brown fox jumps over the lazy dog ".repeat(300);
        let text = format!("{head}\n```rust\nlet a = 1;\nlet b = 2;\nlet c = 3;\n```\n尾行\n");
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (shaped_h, code_ix) = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
            let code_ix = frame
                .rows
                .iter()
                .position(|r| r.kind == IrKind::Code)
                .expect("应有代码块");
            let code = &frame.rows[code_ix];
            assert!(code.editable);
            (f32::from(code.height), code_ix)
        });
        let off_h = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 窄带：长段 (~1000px) 全占，代码块与尾行在带外 → 不物化，走布局高度。
            let frame = build_frame(ed.read(cx), window, px(300.), (0., 300.));
            assert!(
                !frame.rows.iter().any(|r| r.kind == IrKind::Code),
                "带外代码块不物化"
            );
            frame.block_height(code_ix)
        });
        assert!(
            (off_h - shaped_h).abs() < 1.,
            "代码块带外估算高 {off_h} 应与整形高 {shaped_h} 一致（防滚动跳变）"
        );
    }

    #[gpui::test]
    fn code_block_caret_aligns_with_click_x(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        // 回归「点击 (x,y) 实际输入在 (x+n,y)/光标盖住字符」：代码行文本内衬 = 12px
        // （`x_off`，面板 pl_3），命中/光标必须都从 `padding + x_off`(24) 起算。
        let text = "```rust\nlet a = 1;\n```\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let code = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Code)
                .expect("应有代码块");
            assert!(code.editable);
            let pad = f32::from(frame.padding);
            let off_x = f32::from(code.x_off);
            assert!((off_x - 12.).abs() < 0.1, "代码行文本内衬 = 面板 pl_3（12px），得 {off_x}");
            let lh = f32::from(code.lh);
            let text_start = pad + off_x; // 24：代码文本起始 x（面板背景在 pad=12，与正文对齐）
            // 1) 点击文本起始处 → 光标 x == 文本起始 x（不右偏、不盖字）。
            let off0 = source_offset_at(ed.read(cx), &frame, point(px(text_start), px(lh * 1.5)));
            let (line0, _) = ed.read(cx).row_col(off0);
            assert_eq!(line0, 1, "点击文本始应命中代码内容行，得行 {line0}");
            ed.update(cx, |e, _| e.set_caret(off0));
            let (on, c0, _) = caret_geometry(ed.read(cx), &frame);
            assert!(on);
            assert!(
                (f32::from(c0) - text_start).abs() < 0.6,
                "光标 x {} 应 = 文本起始 x {text_start}",
                f32::from(c0)
            );
            // 2) 点击面板内、文本左的内衬空白 → 光标收拢到行首（= 文本起始 x），不右偏。
            let off1 = source_offset_at(ed.read(cx), &frame, point(px(pad + 6.), px(lh * 1.5)));
            ed.update(cx, |e, _| e.set_caret(off1));
            let (on, c1, _) = caret_geometry(ed.read(cx), &frame);
            assert!(on);
            assert!(
                (f32::from(c1) - text_start).abs() < 0.6,
                "内衬空白点击应收拢到行首 {text_start}，得 {}",
                f32::from(c1)
            );
            // 3) 文本中部点击 → 光标落点在 <1 字宽误差内贴合点击处。
            let cx_ = text_start + 28.;
            let off2 = source_offset_at(ed.read(cx), &frame, point(px(cx_), px(lh * 1.5)));
            ed.update(cx, |e, _| e.set_caret(off2));
            let (on, c2, _) = caret_geometry(ed.read(cx), &frame);
            assert!(on);
            assert!(
                (f32::from(c2) - cx_).abs() < 14.,
                "光标 x {} 应贴近点击 x {}（<1 字宽）",
                f32::from(c2),
                cx_
            );
        });
    }

    #[gpui::test]
    fn table_click_lands_in_correct_cell(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let row = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Table)
                .expect("应有表格");
            assert!(row.editable, "表格单元格应可编辑");
            let t = row.table.as_ref().expect("表格渲染数据");
            let te = row.table_edit.as_ref().expect("表格编辑排版");
            let rh = f32::from(t.row_h);
            let pad = f32::from(frame.padding);
            let top = f32::from(row.top);
            // (可见行, 列, 期望字符)：表头第一格 A；数据行2(源行3) 两格 3/4。
            for (row_ix, cell_ix, want) in [(0usize, 0usize, 'A'), (2, 0, '3'), (2, 1, '4')] {
                let tx = te.rows[row_ix].cells[cell_ix].text_x;
                let y = top + table_row_top(t, row_ix) + rh / 2.;
                let x = pad + table_cell_x0(&t.widths, cell_ix) + tx + 1.;
                let off = source_offset_at(ed.read(cx), &frame, point(px(x), px(y)));
                let c = ed.read(cx).text().as_ref()[off..]
                    .chars()
                    .next()
                    .unwrap_or('_');
                assert_eq!(c, want, "单元格 (行{row_ix},列{cell_ix}) 应命中字符 {want}");
            }
        });
    }

    #[gpui::test]
    fn table_caret_aligns_with_clicked_cell(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let row = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Table)
                .expect("应有表格");
            let t = row.table.as_ref().unwrap();
            let te = row.table_edit.as_ref().unwrap();
            let rh = f32::from(t.row_h);
            let pad = f32::from(frame.padding);
            let top = f32::from(row.top);
            // 数据第 2 行第 2 格（"4"@源行3）：点击 → set_caret → caret x 应贴合点击 x。
            let cell_ix = 1usize;
            let row_ix = 2usize;
            let tx = te.rows[row_ix].cells[cell_ix].text_x;
            let x = pad + table_cell_x0(&t.widths, cell_ix) + tx + 20.;
            let y = top + table_row_top(t, row_ix) + rh / 2.;
            let off = source_offset_at(ed.read(cx), &frame, point(px(x), px(y)));
            ed.update(cx, |e, _| e.set_caret(off));
            let (on, caretx, carety) = caret_geometry(ed.read(cx), &frame);
            assert!(on, "表格单元格应可放光标");
            assert!(
                (f32::from(caretx) - x).abs() < 12.,
                "光标 x {} 应贴合点击 x {}（<1 字宽）",
                f32::from(caretx),
                x
            );
            let row_top_y = top + table_row_top(t, row_ix);
            assert!(
                f32::from(carety) >= row_top_y - 0.5 && f32::from(carety) <= row_top_y + rh + 0.5,
                "光标 y {} 应在点击行带 [{row_top_y}, {}] 内",
                f32::from(carety),
                row_top_y + rh
            );
        });
    }

    #[gpui::test]
    fn table_cell_edit_typing_and_boundary_guards(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let text = "| 1 | 2 |\n| --- | --- |\n| a | b |\n尾\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                // 在数据行第一格 "a"（源行 2，位 2）内输入 X。
                let ls = e.line_start(2);
                e.set_caret(ls + 2);
                e.replace_range(e.caret()..e.caret(), "X", cx);
                assert!(
                    e.text().as_ref().contains("| Xa | b |"),
                    "格内输入生效：{}",
                    e.text().as_ref()
                );
                // 退格在格内容起点（'X'）→ no-op（守卫 `| ` 不被吃掉）。
                e.set_caret(ls + 2);
                e.backspace(cx);
                assert!(
                    e.text().as_ref().contains("| Xa | b |"),
                    "格起点退格应被守卫：{}",
                    e.text().as_ref()
                );
                // 删除在格内容尾 → no-op（守卫 `|` 不被吃掉）。
                let ls2 = e.line_start(2);
                e.set_caret(ls2 + 4);
                e.delete(cx);
                assert!(
                    e.text().as_ref().contains("| Xa | b |"),
                    "格内容尾删除应被守卫：{}",
                    e.text().as_ref()
                );
                // 表格内回车 → no-op（不拆表）。
                let before = e.text().as_ref().to_string();
                e.set_caret(ls2 + 3);
                e.newline(cx);
                assert_eq!(
                    e.text().as_ref(),
                    &before,
                    "表格内回车应 no-op：{}",
                    e.text().as_ref()
                );
            });
        });
    }

    #[gpui::test]
    fn table_arrow_nav_skips_delimiter(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let text = "| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n尾\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _cx| {
                // 表头（行 0）down → 首数据行（行 2，跳过分隔行 1）。
                e.set_caret(e.line_start(0) + 2);
                e.move_down();
                assert_eq!(e.row_col(e.caret()).0, 2, "表头 down 应到首数据行");
                // 数据行 up → 上一数据行。
                e.set_caret(e.line_start(3) + 4);
                e.move_up();
                assert_eq!(e.row_col(e.caret()).0, 2, "次数据行 up 应到首数据行");
                // 首数据行 up → 表头（跳过分隔行）。
                e.move_up();
                assert_eq!(e.row_col(e.caret()).0, 0, "首数据行 up 应到表头");
                // 末数据行 down → 出表到尾段（行 4）。
                e.set_caret(e.line_start(3) + 4);
                e.move_down();
                assert_eq!(e.row_col(e.caret()).0, 4, "末数据行 down 应出表到尾段");
                // 单元格内 Left：行首 Left 被钳在格内容（不落管道空白）。
                e.set_caret(e.line_start(2) + 2); // "1"
                e.move_left();
                {
                    let (l, c) = e.row_col(e.caret());
                    assert_eq!(l, 2, "内容行 Left 不应出表");
                    assert_eq!(c, 2, "格内容起点 Left 被钳在格内（col {c}）");
                }
            });
        });
    }

    #[gpui::test]
    fn new_features_shape_gutters_cards_links(cx: &mut TestAppContext) {
        use crate::markdown::ir::IrKind;
        use super::{BlockCard, LinkHit, RowGutter};
        cx.update(gpui_component::init);
        let text = "\
# 标题

- [x] 完成
- [ ] 待办

> [!NOTE]- 提示正文
> 折叠内容

行内链接 [示例](https://example.com) 与数学 $x^2$ 同段

$$
\\int x dx
$$

```mermaid
graph TD
A-->B
```
";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            // C1: 任务行 gutter = Check；未勾选行在 src 行 3。
            let check = frame
                .rows
                .iter()
                .find(|r| matches!(&r.extra.gutter, Some(RowGutter::Check { checked: false })))
                .expect("应有未勾选任务行");
            assert_eq!(check.src_line, 3, "`- [ ] 待办` 的源行");
            // C3: 警报 marker 行 gutter = Alert；其下 body 行折叠成 0 高。
            let alert = frame
                .rows
                .iter()
                .find(|r| matches!(&r.extra.gutter, Some(RowGutter::Alert { .. })))
                .expect("应有警报行");
            assert_eq!(alert.src_line, 5, "`> [!NOTE]-` marker 源行");
            // 折叠 callout 主体（src 6）0 高不物化，高度经布局断言。
            let folded_ix = frame.block_index_for_src_line(6).expect("callout 主体块应存在");
            assert_eq!(frame.block_height(folded_ix), 0., "折叠 callout 主体应 0 高");
            // C5: 有可点击链接的行。
            let linkrow = frame
                .rows
                .iter()
                .find(|r| !r.extra.links.is_empty())
                .expect("应有链接行");
            assert!(
                linkrow.extra.links.iter().any(|l| matches!(l.target, LinkHit::Url(_))),
                "应命中 URL 链接"
            );
            // B2: 行内数学 splice 成 spacer 并翻译成 Unicode 排版（`x^2` → `x²`）。
            let math_row = frame
                .rows
                .iter()
                .find(|r| !r.extra.inline_maths.is_empty())
                .expect("应有行内数学");
            let im = &math_row.extra.inline_maths[0];
            assert!(
                im.latex.as_ref().contains('²') || im.latex.as_ref().contains('π'),
                "行内数学应翻译成 Unicode：「{}」",
                im.latex
            );
            assert!(
                !math_row.extra.links.is_empty(),
                "含 `[text](url)` 的行应为可点击链接"
            );
            // A1: 数学块折叠成 Math 卡片（整块一行）。
            let mathcard = frame
                .rows
                .iter()
                .find(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
                .expect("应有数学卡片");
            assert!(f32::from(mathcard.height) >= 40., "数学卡片高");
            assert_eq!(mathcard.src_lines, 10..13, "数学卡片覆盖 `$$…$$` 块");
            // A2: mermaid 围栏 → Mermaid 卡片。
            let mm = frame
                .rows
                .iter()
                .find(|r| matches!(&r.extra.card, Some(BlockCard::Mermaid { .. })))
                .expect("应有 mermaid 卡片");
            assert!(
                f32::from(mm.height) >= 100.,
                "mermaid 卡片高按层数计算：{}",
                f32::from(mm.height)
            );
            let _ = IrKind::Paragraph;
        });
    }

    #[gpui::test]
    fn mermaid_renders_diagram_to_image(cx: &mut TestAppContext) {
        // mermaid 卡片：层式布局（4 节点 3 边）+ 高度按层数算 + 把**同一套几何**
        // 光栅化成 PNG（盒子+连线+箭头），供外部识图核验“真图”。
        use super::{BlockCard, mm_layout};
        let _ = std::fs::create_dir_all(r"C:\Users\C22500~1\AppData\Local\Temp\kilo");
        const SRC: &str = "A[开始] --> B{判断}\nB -->|是| C[完成]\nB -->|否| D[返回]";
        cx.update(gpui_component::init);
        let text = "\
# 标题

```mermaid
graph TD
    A[开始] --> B{判断}
    B -->|是| C[完成]
    B -->|否| D[返回 A]
```

尾段
";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (card_h, n_nodes, n_edges, png_path, ink, size) =
            vcx.update(|window, cx| {
                let ed = host.read(cx).ed.clone();
                let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
                let mm = frame
                    .rows
                    .iter()
                    .find(|r| matches!(&r.extra.card, Some(BlockCard::Mermaid { .. })))
                    .expect("应有 mermaid 卡片");
                let card_h = f32::from(mm.height);
                let theme = ed.read(cx).theme.clone();
                let (placed, edges) = mm_layout(window, &theme, SRC, 0., 0., 560.);
                let (img, ink) = rasterize_mm_png(&placed, &edges);
                let path = r"C:\Users\C22500~1\AppData\Local\Temp\kilo\mermaid_check.png".to_string();
                let saved = img.save(&path).is_ok();
                assert!(saved, "应能保存 PNG");
                let size = (img.width(), img.height());
                (card_h, placed.len(), edges.len(), path, ink, size)
            });
        assert!(card_h >= 100., "卡片高按层数计算：{card_h}");
        assert_eq!(n_nodes, 4, "4 个节点");
        assert_eq!(n_edges, 3, "3 条边");
        assert!(ink > 300, "PNG 应画出盒子/连线（非白像素 {ink}）");
        eprintln!("mermaid PNG 已导出：{png_path}（{size:?}，涂墨 {ink}px）");
    }

    /// 把布局几何光栅化成 PNG（盒子边框 + 连线 + 箭头；中文标签不进位图，
    /// 由「框 + 箭头」结构本身表明这是一张流程图）。
    fn rasterize_mm_png(
        placed: &[super::MmPlaced],
        edges: &[(usize, usize, Option<String>)],
    ) -> (image::RgbaImage, usize) {
        const PAD: i32 = 40;
        if placed.is_empty() {
            return (image::RgbaImage::new(1, 1), 0);
        }
        let mut x0f = f32::MAX;
        let mut y0f = f32::MAX;
        let mut x1f = f32::MIN;
        let mut y1f = f32::MIN;
        for p in placed {
            x0f = x0f.min(p.x);
            y0f = y0f.min(p.y);
            x1f = x1f.max(p.x + p.w);
            y1f = y1f.max(p.y + p.h);
        }
        let w = ((x1f - x0f) as i32 + 2 * PAD).max(8);
        let h = ((y1f - y0f) as i32 + 2 * PAD).max(8);
        let mut img = image::RgbaImage::from_pixel(
            w as u32,
            h as u32,
            image::Rgba([255, 255, 255, 255]),
        );
        let ox = -x0f as i32 + PAD;
        let oy = -y0f as i32 + PAD;
        let mut ink = 0usize;
        let dark = [40u8, 45, 60, 255];
        let fill = [235u8, 238, 245, 255];
        // 边：三层折线 + 箭头。
        for (ia, ib, _) in edges {
            let (pa, pb) = (&placed[*ia], &placed[*ib]);
            let ax = (pa.x + pa.w / 2.) as i32 + ox;
            let ay = (pa.y + pa.h) as i32 + oy;
            let bx = (pb.x + pb.w / 2.) as i32 + ox;
            let by = pb.y as i32 + oy;
            let midy = (ay + by) / 2;
            if (bx - ax).abs() < 2 {
                mm_blit(&mut img, &mut ink, ax, ay, bx, by - 4, dark);
            } else {
                mm_blit(&mut img, &mut ink, ax, ay, ax, midy, dark);
                mm_blit(&mut img, &mut ink, ax, midy, bx, midy, dark);
                mm_blit(&mut img, &mut ink, bx, midy, bx, by - 4, dark);
            }
            mm_blit(&mut img, &mut ink, bx - 4, by - 6, bx, by - 1, dark);
            mm_blit(&mut img, &mut ink, bx + 4, by - 6, bx, by - 1, dark);
        }
        // 节点：填充 + 边框。
        for p in placed {
            let (x0, y0) = (p.x as i32 + ox, p.y as i32 + oy);
            let (x1, y1) = ((p.x + p.w) as i32 + ox, (p.y + p.h) as i32 + oy);
            for yy in y0..=y1 {
                for xx in x0..=x1 {
                    if xx >= 0 && yy >= 0 && (xx as u32) < img.width() && (yy as u32) < img.height() {
                        img.put_pixel(xx as u32, yy as u32, fill.into());
                        ink += 1;
                    }
                }
            }
            mm_blit(&mut img, &mut ink, x0, y0, x1, y0, dark);
            mm_blit(&mut img, &mut ink, x0, y1, x1, y1, dark);
            mm_blit(&mut img, &mut ink, x0, y0, x0, y1, dark);
            mm_blit(&mut img, &mut ink, x1, y0, x1, y1, dark);
        }
        (img, ink)
    }

    fn mm_blit(img: &mut image::RgbaImage, ink: &mut usize, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 4]) {
        let (w, h) = (img.width() as i32, img.height() as i32);
        let (mut x0, mut y0) = (x0, y0);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            if x0 >= 0 && y0 >= 0 && x0 < w && y0 < h {
                img.put_pixel(x0 as u32, y0 as u32, c.into());
                *ink += 1;
            }
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    #[gpui::test]
    fn callout_fold_click_path_round_trips(cx: &mut TestAppContext) {
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
        use super::{Frame, RowGutter};
        cx.update(gpui_component::init);
        let text = "> [!NOTE]- 甲\n> 乙\n> 丙\n尾";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(point(px(0.), px(0.)), size(px(600.), px(600.)))));
            });
            let chevron_point = |f: &Frame| {
                let marker = f
                    .rows
                    .iter()
                    .find(|r| matches!(&r.extra.gutter, Some(RowGutter::Alert { .. })))
                    .expect("应有警报 marker 行");
                let y = f32::from(marker.top) + f32::from(marker.height) / 2.;
                point(px(12. + 40.), px(y))
            };
            // 折叠态 → 点击展开。
            let frame1 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame1)));
            let f1 = ed.read(cx).frame.borrow().as_ref().map(std::rc::Rc::clone).unwrap();
            let ev1 = MouseDownEvent {
                button: MouseButton::Left,
                position: chevron_point(&f1),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev1, window, cx));
            assert!(ed.read(cx).text().as_ref().starts_with("> [!NOTE]+"), "点击应展开");
            // 展开态 → 再点折叠回。
            let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame2)));
            let f2 = ed.read(cx).frame.borrow().as_ref().map(std::rc::Rc::clone).unwrap();
            let ev2 = MouseDownEvent {
                button: MouseButton::Left,
                position: chevron_point(&f2),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev2, window, cx));
            assert!(ed.read(cx).text().as_ref().starts_with("> [!NOTE]-"), "再点应折叠回");
            assert_eq!(ed.read(cx).text().as_ref(), text, "往返后内容还原");
            // 关键断言：折叠后 body 行必须真正折叠（0 高）——光标在 marker 行也不得被 reveal 弹开。
            // 折叠 body 0 高不物化，高度经布局 `block_height` 断言。
            let frame3 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let body_ix = frame3.block_index_for_src_line(1).expect("callout 主体块应存在");
            assert_eq!(frame3.block_height(body_ix), 0., "点击折叠后 body 应真正折叠（回归）");
        });
    }

    #[gpui::test]
    fn two_callouts_fold_independently_with_caret_on_marker(cx: &mut TestAppContext) {
        // 回归：两个 callout 相邻（中间空行）。各自折叠/展开应互不影响；
        // 尤其「光标落在 marker 行（点击折叠后的常态）」时，该 callout 仍要真正折叠。
        cx.update(gpui_component::init);
        let text = "> [!NOTE]- 点击箭头折叠\n> 主体一\n> 主体二\n\n> [!WARNING]- 警告\n> 警告内容。\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 光标在 NOTE marker 行：两个 callout 的 body 都应折叠。
            ed.update(cx, |e, _| e.set_caret(0));
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let note_ix = frame.block_index_for_src_line(1).expect("NOTE 主体块");
            let warn_ix = frame.block_index_for_src_line(5).expect("WARNING 主体块");
            assert_eq!(frame.block_height(note_ix), 0., "光标在 marker 行时 NOTE body 也应折叠");
            assert_eq!(frame.block_height(warn_ix), 0., "WARNING body 也应折叠");
            // 展开 NOTE → 再折叠（光标仍在 NOTE marker 行）。
            ed.update(cx, |e, cx| e.toggle_alert_fold_at_line(0, cx));
            assert!(ed.read(cx).text().as_ref().starts_with("> [!NOTE]+"), "先展开");
            ed.update(cx, |e, cx| e.toggle_alert_fold_at_line(0, cx));
            assert!(ed.read(cx).text().as_ref().starts_with("> [!NOTE]-"), "再折叠回");
            let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let note2_ix = frame2.block_index_for_src_line(1).expect("NOTE 主体块");
            assert_eq!(
                frame2.block_height(note2_ix), 0.,
                "折叠后 NOTE body 应真正折叠（不是 reveal）"
            );
            // 互不影响：折叠 WARNING 不应改动 NOTE。
            ed.update(cx, |e, cx| e.toggle_alert_fold_at_line(4, cx));
            assert!(ed.read(cx).text().as_ref().starts_with("> [!NOTE]-"), "NOTE 折叠态保持");
            assert!(
                ed.read(cx).text().as_ref().find("> [!WARNING]+").is_some()
                    || ed.read(cx).text().as_ref().find("> [!WARNING]-").is_some(),
                "WARNING 折叠符存在"
            );
        });
    }

    #[gpui::test]
    fn heading_chevron_click_round_trips(cx: &mut TestAppContext) {
        // 标题折叠箭头 → 视图本地折叠态翻转（不写源码、不进撤销栈）：`## 甲` 的
        // section body 行折叠成 0 高；再点展开还原。折叠后光标停在标题行，不得被
        // reveal 弹开（与 callout 的坑 P 同一教训）。
        use super::Frame;
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
        cx.update(gpui_component::init);
        let text = "## 甲\nbody 一\nbody 二\n## 乙\n尾";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(point(px(0.), px(0.)), size(px(600.), px(600.)))));
            });
            let chevron_point = |f: &Frame| {
                let heading = f
                    .rows
                    .iter()
                    .find(|r| matches!(r.kind, crate::markdown::ir::IrKind::Heading(_)))
                    .expect("应有标题行");
                let (cx0, _) = super::heading_chevron_box(f, heading).unwrap();
                let y = f32::from(heading.top) + f32::from(heading.lh) / 2.;
                point(px(cx0 + 8.), px(y))
            };
            // 折叠 `## 甲`（单击标题行左缘箭头）：body 行真正折叠（0 高）。
            let frame1 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame1)));
            let f1 = ed.read(cx).frame.borrow().as_ref().map(std::rc::Rc::clone).unwrap();
            let ev1 = MouseDownEvent {
                button: MouseButton::Left,
                position: chevron_point(&f1),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev1, window, cx));
            assert!(
                ed.read(cx).folded_headings.borrow().contains("## 甲"),
                "点击标题箭头应折叠"
            );
            let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let body_ix = frame2.block_index_for_src_line(1).expect("body 块");
            assert_eq!(frame2.block_height(body_ix), 0., "折叠后 body 应真正折叠");
            let h2 = frame2.rows.iter().find(|r| r.src_lines.contains(&0)).unwrap();
            assert!(f32::from(h2.height) > 0., "标题行本身保留");
            // 展开回：body 高度恢复，文本不变（视图本地，不写源码）。
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame2)));
            let f2 = ed.read(cx).frame.borrow().as_ref().map(std::rc::Rc::clone).unwrap();
            let ev2 = MouseDownEvent {
                button: MouseButton::Left,
                position: chevron_point(&f2),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev2, window, cx));
            assert!(
                !ed.read(cx).folded_headings.borrow().contains("## 甲"),
                "再点应展开"
            );
            assert_eq!(ed.read(cx).text().as_ref(), text, "折叠/展开不写源码");
            let frame3 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let body3 = frame3.rows.iter().find(|r| r.src_lines.contains(&1)).unwrap();
            assert!(f32::from(body3.height) > 0., "展开后 body 恢复高度");
        });
    }

    #[gpui::test]
    fn heading_fold_stays_folded_with_caret_on_heading_line(cx: &mut TestAppContext) {
        // 回归（坑 P 同款）：点击折叠后光标停在标题行，该 section 不得被 reveal 弹开；
        // 光标严格落在 body 行内才 reveal。
        cx.update(gpui_component::init);
        let text = "# A\nbody\n## B\nmore\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                e.toggle_heading_fold_at_line(0, cx);
            });
            // 光标默认在行首（标题行）→ 折叠保持。
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let body_ix = frame.block_index_for_src_line(1).expect("body 块");
            assert_eq!(frame.block_height(body_ix), 0., "光标在标题行时折叠保持");
            // 光标移到 body（源字节 6 = "body" 起点所在行）→ reveal：body 恢复高度。
            ed.update(cx, |e, _| e.set_caret(text.find("body").unwrap()));
            let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let body2 = frame2.rows.iter().find(|r| r.src_lines.contains(&1)).unwrap();
            assert!(f32::from(body2.height) > 0., "光标在 body 行内应 reveal");
        });
    }

    #[gpui::test]
    fn toggle_heading_fold_moves_caret_out_when_folding_from_body(cx: &mut TestAppContext) {
        // 折叠时若光标在 section body 内，先把光标移到标题行末（否则 body 被 reveal，
        // 折叠无效）——与 live 的 `move_to(end)` 同款。
        cx.update(gpui_component::init);
        let text = "## Goals\n- 任务一\n- 任务二\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 光标放在列表行（body 内）。
            ed.update(cx, |e, _| e.set_caret(text.find("- 任务一").unwrap()));
            let before = ed.read(cx).caret();
            assert!(before > 6, "光标在 body 内");
            ed.update(cx, |e, cx| e.toggle_heading_fold_at_line(0, cx));
            assert!(
                ed.read(cx).folded_headings.borrow().contains("## Goals"),
                "折叠成功"
            );
            // 折叠后光标在标题行内、不在 body（第 0 行），保证折叠不被 reveal 弹开。
            let (line, _) = ed.read(cx).row_col(ed.read(cx).caret());
            assert_eq!(line, 0, "折叠时光标应移到标题行");
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let body_ix = frame.block_index_for_src_line(1).expect("body 块");
            assert_eq!(frame.block_height(body_ix), 0., "body 真正折叠");
        });
    }

    #[gpui::test]
    fn heading_fold_hides_math_cards_completely(cx: &mut TestAppContext) {
        // 回归：折叠标题，其 section 内的 `$$…$$` 数学块卡片（“公式”标签 + 公式图）
        // 必须一并折叠成 0 高，不得因 `extra.card` 继续绘制而重叠堆在标题上方。
        use super::BlockCard;
        cx.update(gpui_component::init);
        let text = "# 公式区\n$$\nx^2\n$$\n$$\n\\int dx\n$$\n尾\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 展开时：两个数学卡片都在（src_lines 覆盖 `$$…$$`）。
            let frame_open = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let open_cards: Vec<_> = frame_open
                .rows
                .iter()
                .filter(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
                .collect();
            assert_eq!(open_cards.len(), 2, "展开时应有 2 个数学卡片");
            // 折叠标题：卡片行必须消失（fold 后 math_start 不装配卡片，走普通 0 高占位）。
            ed.update(cx, |e, cx| e.toggle_heading_fold_at_line(0, cx));
            assert!(
                ed.read(cx).folded_headings.borrow().contains("# 公式区"),
                "已折叠"
            );
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let cards: Vec<_> = frame
                .rows
                .iter()
                .filter(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
                .collect();
            assert!(
                cards.is_empty(),
                "折叠标题后数学卡片不得残留绘制（{}）",
                cards.len()
            );
            // 原卡片行应折叠成 0 高（不再装配卡片），折叠块不物化，高度经布局断言。
            let m1_ix = frame.block_index_for_src_line(1).expect("`$x$` 起始行块");
            assert_eq!(frame.block_height(m1_ix), 0., "`\\$\\$` 起始行折叠成 0 高");
        });
    }

    #[gpui::test]
    fn callout_body_rows_share_alert_bar_color(cx: &mut TestAppContext) {
        // 回归：展开的 callout，其 body 行左条应与 marker 同色（警报色），不是灰条。
        // 检查 frame 里 callout body 行的 bar 配色走 alert 路径（间接断言：
        // `alert_region_kinds` 把 marker + body 行都标上 kind，paint 据此取警报色）。
        use crate::markdown::inline_style;
        use crate::markdown::recognition::AlertKind;
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "> [!NOTE]+ 提示\n> body一\n> body二\n\n> 普通引述\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let bodies: Vec<_> = frame
                .rows
                .iter()
                .filter(|r| r.kind == IrKind::Quote && r.src_line >= 1 && r.src_line <= 2)
                .collect();
            assert_eq!(bodies.len(), 2, "callout body 行应存在");
            let kinds = inline_style::alert_region_kinds(&ed.read(cx).content());
            for b in &bodies {
                assert_eq!(
                    kinds.get(&b.src_line),
                    Some(&AlertKind::Note),
                    "body 行 {} 左条应取警报色",
                    b.src_line
                );
            }
            // 普通引述行不进入警报色表。
            let _ = frame.rows.iter().find(|r| r.src_line == 4).unwrap();
            assert!(!kinds.contains_key(&4), "普通引述不应走警报色");
        });
    }

    #[gpui::test]
    fn paint_smoke_all_new_features(cx: &mut TestAppContext) {
        // 把含任务框/警报折叠/链接/行内数学/数学块/mermaid 的内容画一遍，确保不 panic。
        use super::BlockCard;
        cx.update(gpui_component::init);
        let text = "\
- [ ] 待办

# 标题折叠

> [!WARNING]+ 重要
> 内容

> 连续引述第一行
> 连续引述第二行

链接 [示例](https://a.b) 与 $x^2$

$$
\\sqrt{\\pi}
$$

```mermaid
graph TD
A-->B
```
";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            assert!(
                frame.rows.iter().any(|r| matches!(r.extra.card, Some(BlockCard::Mermaid { .. }))),
                "应渲染出 mermaid 卡片"
            );
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame)));
            // 触发一次真实绘制（measure→prepaint→paint），catch 任何渲染 panic。
            let _ = window.draw(cx);
        });
    }

    #[test]
    fn math_to_unicode_translates_superscripts_and_symbols() {
        use super::math_to_unicode;
        assert!(math_to_unicode("E=mc^2").contains('²'), "上标 2 → ²");
        assert!(math_to_unicode("\\int_0^1 x^2 dx").contains('∫'), "积分符号");
        assert!(math_to_unicode("\\sqrt{\\pi}").contains('√'), "根号");
        assert!(math_to_unicode("\\sum_{i=1}^{n}").contains('∑'), "求和");
        assert_eq!(math_to_unicode("\\tfrac{1}{3}"), "1/3", "分数 → a/b");
    }

    #[test]
    fn mermaid_graph_parses_nodes_and_edges() {
        use super::{MmShape, mermaid_graph};
        let src = "graph TD\n    A[开始] --> B{判断}\n    B -->|是| C[完成]\n    B -->|否| D[返回 A]\n";
        let g = mermaid_graph(src);
        assert_eq!(g.nodes.len(), 4, "4 节点");
        assert_eq!(g.edges.len(), 3, "3 边");
        assert!(g.nodes.iter().any(|(id, lab, _)| id == "A" && lab == "开始"), "A=开始");
        assert!(g.nodes.iter().any(|(id, lab, _)| id == "C" && lab == "完成"), "C=完成");
        // 回归：`B{判断}` 的定义不能被后续裸引用 `B -->` 覆盖成 “B”。
        assert!(
            g.nodes.iter().any(|(id, lab, _)| id == "B" && lab == "判断"),
            "B 的标签应保持为「判断」，却被后续裸引用覆盖：{:?}",
            g.nodes.iter().filter(|(id, _, _)| id == "B").collect::<Vec<_>>()
        );
        let (id, _, shp) = g.nodes.iter().find(|(id, _, _)| id == "B").unwrap();
        assert!(id == "B" && *shp == MmShape::Diamond, "B 为判断（菱形）");
        let (a, b, lab) = &g.edges[1];
        assert!(a == "B" && b == "C" && lab.as_deref() == Some("是"), "边 B→C(是)");
    }

    #[gpui::test]
    fn undo_redo_round_trip(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("abc"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| {
                e.set_caret(1);
                e.replace_range(1..1, "X", cx);
            });
            assert_eq!(ed.read(cx).text().as_ref(), "aXbc");
            ed.update(cx, |e, cx| e.undo(cx));
            assert_eq!(ed.read(cx).text().as_ref(), "abc");
            ed.update(cx, |e, cx| e.redo(cx));
            assert_eq!(ed.read(cx).text().as_ref(), "aXbc");
        });
    }

    #[test]
    fn link_runs_render_in_blue_with_underline() {
        // C5 修复：WYSIWYG 链接不该用 `colors.accent`（浅色主题 = neutral-100 灰），
        // 而是单一源 `StyleParams.link`（明 #0969da / 暗 #79c0ff，与源码编辑器同蓝）。
        use super::runs_to_text_runs;
        use gpui::FontWeight;
        let base = gpui::font("Arial");
        let mono = gpui::font("Consolas");
        let text = "点 示例 你";
        // 渲染字节 [2..4) 是链接（“示例”，两个 3 字节汉字 → 6 字节）。
        let runs = vec![((2..8), crate::markdown::parser::RunKind::Link)];
        let accent = gpui::Hsla::default();
        let link: gpui::Hsla = gpui::rgb(0x0969da).into();
        let code_inline_fg = gpui::Hsla::default();
        let out = runs_to_text_runs(
            text,
            &runs,
            &base,
            gpui::Hsla::default(),
            &mono,
            accent,
            link,
            code_inline_fg,
        );
        let link_run = out
            .iter()
            .find(|r| {
                r.color == link && r.len == 6
            })
            .expect("链接段应为蓝色 TextRun（len=6 汉字）");
        assert_eq!(link_run.color, link, "链接文字色应等于 StyleParams.link 蓝");
        assert!(
            link_run.underline.is_some(),
            "链接应有下划线"
        );
        let ul = link_run.underline.as_ref().unwrap();
        assert_eq!(ul.color, Some(link), "下划线颜色与文字同蓝");
        // 非链接段保持预设底色（基色），不误伤。
        let _ = FontWeight::default();
        assert!(
            out.iter().any(|r| r.len == 2),
            "应有链接前导“点 ”（2 字节）段"
        );
    }

    #[gpui::test]
    fn link_theme_single_source_equals_editor_blue(cx: &mut TestAppContext) {
        // 回检测试（坑 D 单一源）：WyTheme.link ← StyleParams.link ← 明/暗主题蓝，
        // 且与源码编辑器 `editor/markdown.rs` 的 LINK_LIGHT/LINK_DARK 同值。
        use crate::markdown::view::style::StyleParams;
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("none"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_window, cx| {
            let ed = host.read(cx).ed.clone();
            let theme = ed.read(cx).theme.clone();
            let p = StyleParams::from_theme_with_font(cx, Some(f32::from(theme.font_size)));
            assert_eq!(theme.link, p.link, "WyTheme.link 抄自 StyleParams.link");
            // 浅色主题（main.rs 强制 Light）：应与编辑器 LINK_LIGHT #0969da 逐字节一致。
            let expect = if p.is_dark {
                gpui::rgb(crate::config::colors::LINK_DARK).into()
            } else {
                gpui::rgb(crate::config::colors::LINK_LIGHT).into()
            };
            assert_eq!(theme.link, expect, "链接蓝单一源 config::colors::LINK_*");
        });
    }

    #[gpui::test]
    fn inline_image_hit_is_found_and_within_display_range(cx: &mut TestAppContext) {
        // B1 修复：`![alt](src)` 的 `RunKind::Image` 只覆盖 alt——byte_map[alt] 是
        // **alt 起点**（不是 `!`）。旧实现在 alt 起点处查 `![` 永远找不到（tail 没有
        // `!`），且显示区间按整段源跨度算、越出 shaped 行 → `position_for_index` 落空。
        // 现在：src 起点从 `!` 反查，显示区间收在 alt 内。
        use crate::markdown::ir::IrKind;
        cx.update(gpui_component::init);
        let text = "前文 ![内联蓝块](shot_blue.png) 后文\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let para = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Paragraph)
                .expect("应有段落行");
            assert!(
                !para.extra.inline_images.is_empty(),
                "行内图片应被命中（旧实现此处为空 → 永远不显示）"
            );
            let hit = &para.extra.inline_images[0];
            assert_eq!(hit.src.as_ref(), "shot_blue.png", "src 取自 `!(…)`");
            // 显示区间必须在 shaped 行长度内（否则 paint 的 position_for_index 返回 None）。
            let dl = para.wrapped.first().map(|w| w.len()).unwrap_or(0);
            assert!(
                hit.display_off + hit.len <= dl,
                "图片显示区间 {}..{} 须在 shaped 行 {} 内",
                hit.display_off,
                hit.display_off + hit.len,
                dl
            );
            assert_eq!(hit.len, "内联蓝块".len(), "显示区只覆盖 alt（不含标记）");
        });
    }

    #[test]
    fn math_block_env_and_line_breaks_render_cleanly() {
        // A1 修复：结构化 LaTeX 不再泄漏 `\begin{...}`/字面 `\\`。
        use super::{math_display_lines, math_to_unicode};
        // `\\` 行断 → 拆成两行（矩阵/对齐环境也如此）。
        let lines = math_display_lines("a = b \\\\ c = d");
        assert_eq!(lines.len(), 2, "`\\\\`（LaTeX 行断）应拆成两行");
        assert_eq!(lines[0], "a = b");
        assert_eq!(lines[1], "c = d");
        // `\begin{matrix}` 环境名剥掉、`&` 列分隔 → 空格。
        let m = math_to_unicode("\\begin{matrix}1 & 2 \\\\ 3 & 4\\end{matrix}");
        assert!(!m.contains("begin"), "不应泄漏 \\begin：{m}");
        assert!(!m.contains('\\'), "不应残留反斜杠：{m}");
        assert!(m.contains('1') && m.contains('2') && m.contains('3') && m.contains('4'));
        // 既有的普通公式仍工作。
        assert!(math_to_unicode("\\int_0^1 x^2 dx").contains('∫'));
    }

    #[test]
    fn math_to_segments_split_sup_sub_for_limit_layout() {
        // A1 回归：`∫_{-\infty}^{\infty}` 里 `∞` 没有 Unicode 上/下标码位，字符串版只能
        // 正文大小平排（`∫₋∞∞`）。卡片现在把顶层 `^{…}`/`_{…}` 拆成 Sup/Sub 段 →
        // 绘制时小字号 + 上下偏移，`∞` 也能正确显示成极限。
        use super::{math_to_segments, math_to_unicode, MathPos};
        let src = r"\int_{-\infty}^{\infty} e^{-x^2} dx = \sqrt{\pi}";
        let segs = math_to_segments(src);
        assert!(
            segs.iter().any(|s| s.pos == MathPos::Sub && s.text.contains('∞')),
            "下标组应拆出 Sub 段（含 ∞）：{segs:?}"
        );
        assert!(
            segs.iter().any(|s| s.pos == MathPos::Sup && s.text.contains('∞')),
            "上标组应拆出 Sup 段（含 ∞）：{segs:?}"
        );
        // 平铺后 = 旧的扁平字符串（行内数学/兼容路径输出不变）。
        let flat: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(flat, math_to_unicode(src), "segments 平铺应与 math_to_unicode 一致");
        // `e^{-x^2}` 的指数也应在 Sup 段里。
        assert!(
            segs.iter().any(|s| s.pos == MathPos::Sup && s.text.contains('²')),
            "e^(-x^2) 的指数应在 Sup 段：{segs:?}"
        );
        // `\sum_{i=1}^{n}`：下标/上标各一段，且平铺含 ∑。
        let sum_segs = math_to_segments(r"\sum_{i=1}^{n} i");
        assert!(sum_segs.iter().any(|s| s.pos == MathPos::Sub && s.text.contains('ᵢ')));
        assert!(sum_segs.iter().any(|s| s.pos == MathPos::Sup && s.text.contains('ⁿ')));
    }

    #[test]
    fn inline_math_fit_h_keeps_readable_size_within_band() {
        // 行内公式「不能太小也不能溢出」的平衡（撑高行方案）：
        // - 矮公式按原生字号缩放（fs/FONT_SIZE），不做额外缩小；
        // - 高公式（22px 排 `\int_0^1` 约 70px）显示高 = 1× 字号缩放值，但封顶
        //   `INLINE_MATH_BAND × 行高`（撑高行容纳，而不是压扁到 0.95×行高）。
        use super::inline_math_fit_h;
        let fs = px(16.);
        let lh = px(19.);
        // 矮公式：按原生字号缩放（fs/FONT_SIZE×h），不被行高压扁。
        let short = inline_math_fit_h(fs, lh, 25.0);
        assert!((short - 16.0 / 22.0 * 25.0).abs() < 1e-3, "矮公式按字号缩放：{short}");
        assert!(short <= 19.0, "25px 高公式 × 字号缩放后仍在行高内：{short}");
        // 中等公式：字号缩放略超行高 → 所在行会小幅撑高（不再压扁）。
        let mid = inline_math_fit_h(fs, lh, 30.0);
        assert!((mid - 16.0 / 22.0 * 30.0).abs() < 1e-3, "仍按字号缩放：{mid}");
        // 高公式（`\int_0^1 x^2 dx` 22px 下 h≈70.5）：比旧 `0.95×行高`（≈18）大得多，
        // 且绝不超 `INLINE_MATH_BAND × 行高`（只压超高算子公式，其余保持字号缩放）。
        let tall = inline_math_fit_h(fs, lh, 70.5);
        assert!(tall > 19.0 * 0.95, "超高算子仍大于 0.95×行高：{tall}");
        assert!(
            tall <= 19.0 * crate::config::math::INLINE_MATH_BAND + 1e-3,
            "不超 INLINE_MATH_BAND×行高上限：{tall}"
        );
        // 0 高兜底 ≥1px（不除零）。
        assert!(inline_math_fit_h(fs, lh, 0.0) >= 1.0);
    }

    #[gpui::test]
    fn inline_math_double_click_enters_edit(cx: &mut TestAppContext) {
        // B2 双击编辑：双击 `$…$` spacer → 光标进公式（`$` 后 1 字节）→ shape 期
        // reveal 显示可编辑的原始 LaTeX（不再 splice 成 spacer）；光标移走恢复。
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
        cx.update(gpui_component::init);
        let text = "前置文字 $x^2+\\alpha$ 后置文字\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(600.), px(600.)),
                )));
            });
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let para = frame
                .rows
                .iter()
                .find(|r| !r.extra.inline_maths.is_empty())
                .expect("应有行内公式行");
            let im = para.extra.inline_maths[0].clone();
            assert!(!im.source.is_empty(), "spacer 应带绝对源区间");
            let w0 = para.wrapped.first().unwrap();
            let p0 = w0.position_for_index(im.display_off, para.lh).unwrap();
            let (src_line, x_off, top, lh) = (
                para.src_line,
                f32::from(para.x_off),
                f32::from(para.top),
                para.lh,
            );
            let x = f32::from(frame.padding) + x_off + f32::from(p0.x) + 2.0;
            let y = top + f32::from(lh) / 2.;
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame)));
            let ev = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(x), px(y)),
                modifiers: Modifiers::default(),
                click_count: 2,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev, window, cx));
            let caret = ed.read(cx).caret();
            assert!(
                caret > im.source.start && caret < im.source.end,
                "双击应把光标放进公式内：caret={caret} ∈ {:?}",
                im.source
            );
            // reveal：重建帧后该行公式不再 splice。
            let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let para2 = frame2
                .rows
                .iter()
                .find(|r| r.src_lines.contains(&src_line))
                .unwrap();
            assert!(
                para2.extra.inline_maths.is_empty(),
                "光标在公式内应 reveal（不 splice spacer）"
            );
            // 光标移走 → 恢复 splice（公式感回来）。
            ed.update(cx, |e, _| e.set_caret(0));
            let frame3 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let para3 = frame3
                .rows
                .iter()
                .find(|r| r.src_lines.contains(&src_line))
                .unwrap();
            assert!(
                !para3.extra.inline_maths.is_empty(),
                "光标离开公式应恢复 splice"
            );
        });
    }

    #[gpui::test]
    fn math_block_single_click_keeps_card_double_click_edits(cx: &mut TestAppContext) {
        // A1 交互语义修复：单击数学块卡片**不改光标**（不会因 caret 进块被 reveal 回
        // 源码字符串）；**双击**把光标放进块首 → 整块 reveal 源码进入编辑。
        use super::BlockCard;
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
        cx.update(gpui_component::init);
        let text = "$$\n\\int_0^1 x^2 dx\n$$\n尾行\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(600.), px(600.)),
                )));
            });
            ed.update(cx, |e, _| {
                e.set_caret(text.find("尾").unwrap())
            }); // 光标在块外（末行）
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let x = f32::from(frame.padding) + 40.;
            let y = {
                let card = frame
                    .rows
                    .iter()
                    .find(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
                    .expect("应有数学卡片");
                f32::from(card.top) + f32::from(card.height) / 2.
            };
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame)));
            // 单击：光标不动，卡片保持（不 reveal）。
            let ev1 = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(x), px(y)),
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev1, window, cx));
            let caret_before = ed.read(cx).caret();
            assert_eq!(caret_before, text.find("尾").unwrap(), "单击卡片不得移动光标");
            let f1 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            assert!(
                f1.rows
                    .iter()
                    .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
                "单击后卡片仍在（未 reveal）"
            );
            // 双击：光标进块首（源码行 0）→ reveal 源码（不再有卡片）。
            let ev2 = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(x), px(y)),
                modifiers: Modifiers::default(),
                click_count: 2,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev2, window, cx));
            let caret = ed.read(cx).caret();
            assert!(ed.read(cx).row_col(caret).0 == 0, "双击应把光标放进块首");
            let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            assert!(
                !f2.rows
                    .iter()
                    .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
                "双击后 reveal 源码（无卡片）"
            );
        });
    }

    #[gpui::test]
    fn math_block_single_line_double_click_reveals_immediately(cx: &mut TestAppContext) {
        // 单行 `$$…$$` 数学块（`$$\sum…$$` 独占一行）：双击必须**直接**还原成源码
        // 字符串，不得先变成行内小公式再双击一次。根因：光标放在行首 0 列时
        // `splice_math_text` 的 `c > span.start` 不成立 → 仍被 splice 成小号行内公式；
        // 现在放 `$$` 行内容内 1 字节 → 块 reveal 与行内 splice reveal 同时满足。
        use super::BlockCard;
        use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
        cx.update(gpui_component::init);
        let text = "$$\\sum_{i=1}^{n} i = \\frac{n(n+1)}{2}$$\n尾行\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_last_bounds(Some(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(600.), px(600.)),
                )));
            });
            ed.update(cx, |e, _| e.set_caret(text.find("尾").unwrap())); // 光标在块外
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let card = frame
                .rows
                .iter()
                .find(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
                .expect("单行 `$$…$$` 也应是数学卡片");
            let (x, y) = (
                f32::from(frame.padding) + 40.,
                f32::from(card.top) + f32::from(card.height) / 2.,
            );
            ed.update(cx, |e, _| *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame)));
            let ev = MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(x), px(y)),
                modifiers: Modifiers::default(),
                click_count: 2,
                first_mouse: false,
            };
            ed.update(cx, |e, cx| e.on_mouse_down(&ev, window, cx));
            // 双击一次：光标在行内（>0 列），卡片消失、不再 splice（直接是字符串）。
            let caret = ed.read(cx).caret();
            let (_, col) = ed.read(cx).row_col(caret);
            assert!(col > 0, "双击后光标应进入块行内部，col={col}");
            let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            assert!(
                !f2.rows
                    .iter()
                    .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
                "双击后应无卡片（reveal 源码）"
            );
            let row0 = f2.rows.iter().find(|r| r.src_lines.contains(&0)).unwrap();
            assert!(
                row0.extra.inline_maths.is_empty(),
                "单行块双击应直接还原字符串（不得再 splice 成行内公式）"
            );
            // 光标移出块（到尾行）后恢复卡片渲染。
            ed.update(cx, |e, _| e.set_caret(text.find("尾").unwrap()));
            let f3 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            assert!(
                f3.rows.iter().any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
                "光标移出后应恢复数学卡片（公式感）"
            );
        });
    }

    #[gpui::test]
    fn sync_math_claims_block_and_inline_slots(cx: &mut TestAppContext) {
        // 数学排版集成（参考 zorite ensure_content_math）：内容含 `$$…$$` 块 + `$…$`
        // 行内公式 → `sync_math`（**可见带懒光栅**）按「去定界符的 LaTeX」为键把当前带内
        // 两块都占为 Loading（真正位图由后台 executor 光栅；CPU 级正确性已由
        // math_typeset 单测覆盖）。
        use gpui::App;
        cx.update(gpui_component::init);
        let text = "$$\n\\int_0^1 x^2 dx\n$$\n内联 $e^{i\\pi}+1=0$ 结束";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let frame = vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            // 光标放在文档末尾（离开数学块与行内公式）→ 块折叠成卡片、行内公式被
            // splice 带出到 frame.rows → `sync_math` 才能按带内公式 claim 占槽。
            let n = ed.read(cx).content().len();
            ed.update(cx, |e, _| e.set_caret(n));
            build_frame(ed.read(cx), window, px(600.), (0., 999_999.))
        });
        vcx.update(|_window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, ecx| e.sync_math(ecx, &frame));
            let slots = ed.read(cx).math_slots();
            let b = slots.borrow();
            assert!(
                b.contains_key(r"\int_0^1 x^2 dx"),
                "块公式应占位（Loading）：keys={:?}",
                b.keys()
            );
            assert!(
                b.contains_key(super::super::inline_math_key(r"e^{i\pi}+1=0").as_ref()),
                "行内公式应占位（Loading，带 inline: 前缀键）：keys={:?}",
                b.keys()
            );
        });
    }

    #[gpui::test]
    fn math_card_height_uses_typeset_size_when_slot_ready(cx: &mut TestAppContext) {
        // 数学排版切 RaTeX 后：公式就绪 → 卡片高按排版位图逻辑高（`20 + h`），
        // 不再按 Unicode 行数（`20 + n×22`）——上限的分式/根号终于有足够高度。
        use crate::markdown::inline_style;
        use gpui::App;
        cx.update(gpui_component::init);
        let text = "$$\na \\\\ b\n$$"; // 两行 → Unicode 回退高 = 20+44=64
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_window, cx: &mut App| {
            // 一个 40×24 的假位图逻辑尺寸 20×12（RenderImage 是纯数据构造，无需 GPU）。
            let buf = image::RgbaImage::from_raw(40, 24, vec![0u8; 40 * 24 * 4]).unwrap();
            let img = std::sync::Arc::new(gpui::RenderImage::new(vec![image::Frame::new(buf)]));
            let latex = "a \\\\ b"; // math_regions 的 source（fenced 去 `$$` 后）
            let ed = host.read(cx).ed.clone();
            ed.read(cx).math_slots().borrow_mut().insert(
                latex.to_string().into(),
                super::super::MathSlot::Ready(img, 20., 12.),
            );
            let e = ed.read(cx);
            let reg = inline_style::math_regions(&e.content())
                .into_iter()
                .next()
                .expect("应扫到一块公式");
            assert_eq!(reg.source, "a \\\\ b");
            // 就绪后高 = 20 + max(h,16) = 20+16 = 36（而非 Unicode 两行的 64）。
            let h = super::math_card_height(&e, &reg);
            assert_eq!(f32::from(h), 36.0, "就绪应占位排版高度，而非 Unicode 行高");
            // 未就绪（新公式）→ 回退 Unicode 行高 = 20 + 2×22 = 64。
            e.math_slots().borrow_mut().clear();
            let h2 = super::math_card_height(&e, &reg);
            assert_eq!(f32::from(h2), 64.0, "未就绪应回退 Unicode 逐行高");
        });
    }

    #[gpui::test]
    fn inline_math_grown_row_caret_and_click_stay_aligned(cx: &mut TestAppContext) {
        // 用户实测：行内公式把行撑高后，点击该行「闪烁光标位置不对」——光标几何与
        // 点击映射必须都按撑高下移量（text_row_shift）定位，否则光标落在块顶空白处。
        use gpui::App;
        cx.update(gpui_component::init);
        let text = "a $\\int_0^1 x^2 dx$ b";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            // 注入就绪位图（假数据，纯 CPU，无需 GPU）：40×70 逻辑 px（≈22px 排版的
            // `\int_0^1` 高）→ 行内 fit = min(fs/22×70, 2.2×行高) ≈ 41.8px > 行高 19。
            let buf = image::RgbaImage::from_raw(80, 140, vec![0u8; 80 * 140 * 4]).unwrap();
            let img = std::sync::Arc::new(gpui::RenderImage::new(vec![image::Frame::new(buf)]));
            ed.read(cx).math_slots().borrow_mut().insert(
                super::super::inline_math_key("\\int_0^1 x^2 dx"),
                super::super::MathSlot::Ready(img, 40., 70.),
            );
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let row = &frame.rows[0];
            let lh = f32::from(row.lh);
            let band = f32::from(row.height);
            assert!(band > lh + 1.0, "行内公式应撑高行：band={band} > lh={lh}");
            // 文本带 = [shift, shift+lh]（撑高的下移量，与 paint/公式同源）。
            let w0 = row.wrapped.first().unwrap();
            let orig = lh * (w0.wrap_boundaries().len() + 1) as f32;
            let shift = ((band - orig) / 2.).max(0.);
            let pad = f32::from(frame.padding);
            // 点击文本带中部（正文 "a … b" 区域）→ 源偏移应落在行内。
            let y = shift + lh / 2.;
            let x = pad + 30.;
            let off = super::source_offset_at(ed.read(cx), &frame, point(px(x), px(y)));
            let len = ed.read(cx).content().len() as f32;
            assert!(off as f32 <= len, "点击应解析到行内源偏移：off={off}");
            ed.update(cx, |e, _| e.set_caret(off));
            let (on, _caretx, carety) = caret_geometry(ed.read(cx), &frame);
            assert!(on, "该行可放光标");
            assert!(
                f32::from(carety) >= shift - 1.0 && f32::from(carety) <= shift + lh + 1.0,
                "光标 y {} 应在撑高后的文本带 [{shift}, {}] 内，不能回到块顶空白",
                f32::from(carety),
                shift + lh
            );
        });
    }

    #[gpui::test]
    fn keys_ctrl_a_c_v_select_copy_paste_round_trip(cx: &mut TestAppContext) {
        // D1/Ctrl 修复：Windows 上 Ctrl 报 `modifiers.control`，而 `cmd-*` 绑定是
        // `modifiers.platform`（= Win 键）→ 旧实现 Ctrl+C/V/A 全部失效。现在与
        // `live.rs` 一样显式补 `ctrl-a/c/v` 绑定，经真实键位派发验证整条链路。
        use gpui::{App, Keystroke};
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("hello world"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        // 安装 WYSIWYG 键位（与 main.rs 的 bind_keys 装配一致）。
        vcx.update(|_w, cx: &mut App| crate::markdown::wysiwyg::bind_keys(cx));
        // 聚焦编辑器 → 键位上下文生效。
        vcx.update(|window, cx| {
            let fh = host.read(cx).ed.read(cx).focus_handle();
            window.focus(&fh, cx);
        });
        let wh = vcx.windows().first().copied().expect("窗口句柄");

        // Ctrl+A 全选。
        vcx.dispatch_keystroke(wh, Keystroke::parse("ctrl-a").unwrap());
        vcx.update(|_w, cx| {
            assert_eq!(
                host.read(cx).ed.read(cx).selection(),
                Some(0.."hello world".len()),
                "Ctrl+A 应全选"
            );
        });
        // Ctrl+C 复制 → 剪贴板。
        vcx.dispatch_keystroke(wh, Keystroke::parse("ctrl-c").unwrap());
        let clip: Option<String> = vcx.update(|_w, cx: &mut App| {
            cx.read_from_clipboard().map(|i| i.text().unwrap_or_default())
        });
        assert_eq!(clip.as_deref(), Some("hello world"), "Ctrl+C 应写入剪贴板");
        // 光标置开头后 Ctrl+V 粘贴（选区替换为插入）。
        vcx.update(|_w, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| {
                e.set_caret(0);
            });
        });
        vcx.dispatch_keystroke(wh, Keystroke::parse("ctrl-v").unwrap());
        vcx.update(|_w, cx| {
            assert_eq!(
                host.read(cx).ed.read(cx).text().as_ref(),
                "hello worldhello world",
                "Ctrl+V 应在光标处粘贴剪贴板内容"
            );
        });
    }

    #[gpui::test]
    fn plain_edit_reuses_derived_cache(cx: &mut TestAppContext) {
        // 纯文本键入（非结构、行签名不变、不在数学块内）不得触发派生整篇重建：
        // `derived` Rc 指针不变 + regions 版本不递增——这是「输入不卡死」的基石
        // （老实现每键对全篇 `math_regions`×2 + `alert_*`×2 + 行内逐行扫 ≈1s）。
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        let text =
            "$$\n\\int_x dx\n$$\n含有 $e^{i\\pi}$ 的一行\n末尾普通段落 here\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (rc0, rv0) = vcx.update(|_w, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let d = ed.read(cx).derived();
            assert_eq!(d.borrow().math_regions.len(), 1, "应扫到这 1 个 $$ 块");
            (d, ed.read(cx).regions_ver())
        });
        // 在「末尾普通段落 here」行中间键入 'a'（纯文本，无 $ / > / ```）。
        vcx.update(|_w, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let at = ed.read(cx).content().find("here").unwrap() + 2;
            ed.update(cx, |e, ecx| e.replace_range(at..at, "a", ecx));
        });
        vcx.update(|_w, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let d1 = ed.read(cx).derived();
            assert!(
                std::rc::Rc::ptr_eq(&rc0, &d1),
                "纯文本键入应复用 derived Rc（不整篇重建）"
            );
            assert_eq!(ed.read(cx).regions_ver(), rv0, "regions 版本应不变");
            assert_eq!(d1.borrow().math_regions.len(), 1, "块区域数不变");
            assert_eq!(
                d1.borrow().math_regions[0].source, "\\int_x dx",
                "数学块 source 应保持"
            );
        });
    }

    #[gpui::test]
    fn math_interior_edit_patches_block_source(cx: &mut TestAppContext) {
        // 编辑 $$ 块**内部**（非边界行）：不整篇重建，只修补该块 `source`——离开块后
        // 卡片显示更新后的公式（老实现要么整篇重建、要么卡片陈旧显示旧公式）。
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        let text = "$$\n\\int_a dx\n$$\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (rc0, rv0) = vcx.update(|_w, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let d = ed.read(cx).derived();
            assert_eq!(d.borrow().math_regions[0].source, "\\int_a dx");
            (d, ed.read(cx).regions_ver())
        });
        // 把内部行 `\int_a dx` 的 'a' 改成 'b'。
        vcx.update(|_w, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let at = ed.read(cx).content().find("\\int_a").unwrap() + 5; // 落在 'a'
            ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "b", ecx));
        });
        vcx.update(|_w, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let d1 = ed.read(cx).derived();
            assert!(
                std::rc::Rc::ptr_eq(&rc0, &d1),
                "数学块内部编辑不应整篇重建"
            );
            assert_eq!(ed.read(cx).regions_ver(), rv0, "regions 版本应不变");
            assert_eq!(
                d1.borrow().math_regions[0].source, "\\int_b dx",
                "块 source 应被单块修补为新公式"
            );
        });
    }

    #[gpui::test]
    fn dollar_edit_rebuilds_derived(cx: &mut TestAppContext) {
        // 键入 `$`（行签名变化）→ 派生整篇重建 + regions 版本递增（新 `$…$` 公式
        // 由「可见带懒光栅」在下一帧 shape 后经 frame.rows claim，见 sync_math 集成）。
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("abc\n"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (rc0, rv0) = vcx.update(|_w, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let d = ed.read(cx).derived();
            (d, ed.read(cx).regions_ver())
        });
        // 行首写入一个完整的行内公式 `$x$`。
        vcx.update(|_w, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, ecx| e.replace_range(0..0, "$x$", ecx));
        });
        vcx.update(|_w, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let d1 = ed.read(cx).derived();
            assert!(
                !std::rc::Rc::ptr_eq(&rc0, &d1),
                "出现 `$` 使行签名变化 → 派生应整篇重建"
            );
            assert_ne!(ed.read(cx).regions_ver(), rv0, "regions 版本应递增");
        });
    }

    #[gpui::test]
    fn inline_math_interior_edit_reuses_derived(cx: &mut TestAppContext) {
        // 行内公式**内部**键入（不新增/删除 `$`）：派生不用整篇重建；新公式（`$y$`）
        // 由下一帧 shape 带出到 `frame.rows[].extra.inline_maths` → `sync_math`（可见带懒
        // 光栅）按 `inline:y` claim 占槽（不再需要全篇行内清单）。
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("text $x$ here\n"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (rc0, rv0) = vcx.update(|_w, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let d = ed.read(cx).derived();
            (d, ed.read(cx).regions_ver())
        });
        vcx.update(|_w, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let at = ed.read(cx).content().find("$x$").unwrap() + 1;
            ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "y", ecx));
        });
        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let d1 = ed.read(cx).derived();
            assert!(
                std::rc::Rc::ptr_eq(&rc0, &d1),
                "行内公式内键入不应整篇重建"
            );
            assert_eq!(ed.read(cx).regions_ver(), rv0);
            // 光标移出 `$y$`（否则 reveal 显示源码、不 splice）→ 下一帧带内整形行
            // 带出 `$y$`（sync_math 据此 claim `inline:y`）。
            let n = ed.read(cx).content().len();
            ed.update(cx, |e, _| e.set_caret(n));
            let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let lx: Vec<_> = frame
                .rows
                .iter()
                .flat_map(|r| r.extra.inline_maths.iter().map(|m| m.latex.to_string()))
                .collect();
            assert_eq!(lx, vec!["y".to_string()], "帧行内公式应已更新为 $y$：{lx:?}");
        });
    }

    #[gpui::test]
    fn fold_plan_reused_across_caret_move_and_reveal_applies(cx: &mut TestAppContext) {
        // 折叠计划缓存**不含 caret**：光标在普通行间移动 → `fold_plan` Rc 复用（不再
        // 每键/每次光标动整算折叠增量——旧实现 48ms/次）；但光标进数学块 → reveal 生效
        // （该块还原源码、不装卡片），离开块又折叠回卡片。
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        let text = "$$\n\\int_a dx\n$$\n普通段落 one\n普通段落 two\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        let (plan0, len0) = vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, _| e.set_caret(0)); // 文档开头（`$$\n…`）
            let f = build_frame(ed.read(cx), window, px(400.), (0., 999_999.));
            let plan = ed.read(cx).fold_plan_rc().expect("build_frame 应已建折叠计划");
            (
                plan,
                f.rows.iter().filter(|r| r.extra.card.is_some()).count(),
            )
        });
        assert_eq!(len0, 0, "caret 在数学块内 → 不装卡片（reveal 显示源码）");
        // 移到普通行（caret 离开数学块）→ fold_plan 复用 + 卡片出现。
        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let at = ed.read(cx).content().find("one").unwrap();
            ed.update(cx, |e, _| e.set_caret(at));
            let f = build_frame(ed.read(cx), window, px(400.), (0., 999_999.));
            let plan1 = ed.read(cx).fold_plan_rc().expect("折叠计划应已缓存");
            assert!(
                std::rc::Rc::ptr_eq(&plan0, &plan1),
                "仅移动光标不应重建折叠计划"
            );
            assert_eq!(
                f.rows.iter().filter(|r| r.extra.card.is_some()).count(),
                1,
                "caret 离开数学块 → 折叠为卡片"
            );
        });
    }

    /// 行整形缓存（滚动复用）：同带重建不重 shape（仅光标行例外）、重叠带只 shape 新进块。
    #[gpui::test]
    fn shaped_row_cache_reuses_rows_across_band_rebuilds(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        let mut text = String::new();
        for i in 0..120 {
            text.push_str(&format!("para line {i} some more words here to wrap\n"));
        }
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let n = text.len();
            // 光标放文档末端（不在可见带内）→ 带内无「光标行 reveal」干扰。
            ed.update(cx, |e, _| e.set_caret(n));
            let _f1 = build_frame(ed.read(cx), window, px(900.), (0., 800.));
            let r1 = ed.read(cx).take_reshapes();
            assert!(r1 > 0, "首帧应实际整形：{r1}");
            let cached1 = ed.read(cx).shape_rows_len();
            assert!(cached1 > 0, "首帧后缓存应有条目：{cached1}");
            // 同带重建：全部命中缓存 → 不再整形。
            let _f2 = build_frame(ed.read(cx), window, px(900.), (0., 800.));
            assert_eq!(
                ed.read(cx).take_reshapes(),
                0,
                "同带重建应全部命中行整形缓存"
            );
            // 重叠带（下移 300px）：只 shape 新进块的少量行，远少于首帧。
            let _f3 = build_frame(ed.read(cx), window, px(900.), (300., 1100.));
            let r3 = ed.read(cx).take_reshapes();
            assert!(
                r3 < r1,
                "重叠带只应重排新进块（{r3} < 首帧 {r1}）"
            );
        });
    }

    /// 非结构性编辑只作废**被编辑块**的整形缓存；结构性编辑整体清空。
    #[gpui::test]
    fn non_structural_edit_invalidates_only_edited_block(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
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
            let blk_edit = ed.read(cx).doc().block_index_for_line(ed.read(cx).row_col(at).0);
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
            // 结构性编辑（插入换行）→ 整体清空。
            ed.update(cx, |e, ecx| e.replace_range(at..at, "\n", ecx));
            assert_eq!(
                ed.read(cx).shape_rows_len(),
                0,
                "结构性编辑（换行）应整体清空行整形缓存"
            );
        });
    }

    /// 字号/主题变化 → 行整形缓存整体清空（否则旧字号的行宽/高被复用）。
    #[gpui::test]
    fn font_change_clears_shape_cache(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
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
        cx.update(gpui_component::init);
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

    /// B3 增量派生：结构编辑（Enter）后 math 区域**行号平移**而非整篇重扫——编辑线之上
    /// 不变、之下平移 Δ；无区域跨线 → `regions_ver` 不递增（不重建 derived）；区域跨线
    /// （编辑落在 $$ 块内）→ 回退整篇重建（regions_ver++）。
    #[gpui::test]
    fn structural_enter_shifts_derived_math_regions(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        let text = "para one\n$$\n\\int_a b dx\n$$\npara two\n";
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let _f = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
            let ranges =
                |e: &gpui::Entity<WyEditor>, cx: &App| -> Vec<std::ops::Range<usize>> {
                    e.read(cx).derived().borrow().math_regions.iter().map(|r| r.range.clone()).collect()
                };
            assert_eq!(ranges(&ed, cx), vec![1usize..4], "初始 $$ 块占行 1..4");
            let v0 = ed.read(cx).regions_ver();
            // 行 0（para one）末按 Enter → 整块 $$ 下移一行；无跨线 → 增量平移。
            let at = ed.read(cx).text().as_ref().find('\n').unwrap();
            ed.update(cx, |e, ecx| e.replace_range(at..at, "\n", ecx));
            assert_eq!(ranges(&ed, cx), vec![2usize..5], "Enter 后 $$ 块整体下移一行");
            assert_eq!(
                ed.read(cx).regions_ver(),
                v0,
                "无区域跨线 → 只用行号平移，不重建派生（B3 增量）"
            );
            // 结构编辑落在 $$ 块内（跨线）→ 回退整篇重建（区域内容变了无法纯平移）。
            let at2 = ed.read(cx).text().as_ref().find("\\int_a").unwrap() + 3;
            let v1 = ed.read(cx).regions_ver();
            ed.update(cx, |e, ecx| e.replace_range(at2..at2, "\n", ecx));
            assert_eq!(
                ed.read(cx).regions_ver(),
                v1.wrapping_add(1),
                "结构编辑落在 $$ 块内 → 区域内容变化 → 整篇重建派生"
            );
        });
    }

    /// B2 撤销事务合并：连续键入（同意图 + 邻接）并入一个撤销单元；光标移动打断
    /// 组（移动后键入独立单元）；Enter/纸类编辑为原子单元。
    #[gpui::test]
    fn undo_b2_groups_typing_and_breaks_on_caret(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(""));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            // 连续键入 abc → 一个撤销单元。
            for ch in ["a", "b", "c"] {
                ed.update(cx, |e, ecx| {
                    let at = e.caret();
                    e.replace_range(at..at, ch, ecx);
                });
            }
            assert_eq!(ed.read(cx).text().as_ref(), "abc");
            ed.update(cx, |e, ecx| e.undo(ecx));
            assert_eq!(
                ed.read(cx).text().as_ref(),
                "",
                "连续键入一次撤销应整组回退（B2 合并）"
            );
            // 重做回 abc。
            ed.update(cx, |e, ecx| e.redo(ecx));
            assert_eq!(ed.read(cx).text().as_ref(), "abc");
            // 光标移动 → 打断：移动后键入 z 是独立撤销单元。
            ed.update(cx, |e, ecx| {
                e.set_caret(0);
                e.replace_range(0..0, "z", ecx);
            });
            assert_eq!(ed.read(cx).text().as_ref(), "zabc");
            ed.update(cx, |e, ecx| e.undo(ecx));
            assert_eq!(ed.read(cx).text().as_ref(), "abc", "z 独立撤销");
            ed.update(cx, |e, ecx| e.undo(ecx));
            assert_eq!(ed.read(cx).text().as_ref(), "", "光标移动后 z 与 abc 分属两个单元");
        });
    }

    /// B2 撤销合并：连续退格经 `replace_range`（纯删除、首尾相接）合并成一个单元。
    #[gpui::test]
    fn undo_b2_groups_backspaces(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("abcdef"));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|_window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, ecx| {
                e.set_caret(6);
                // 连续退格 3 个字符（6→5, 5→4, 4→3）。
                for _ in 0..3 {
                    let at = e.caret();
                    e.replace_range(at - 1..at, "", ecx);
                }
            });
            assert_eq!(ed.read(cx).text().as_ref(), "abc");
            // 一次撤销应还原全部 3 次退格。
            ed.update(cx, |e, ecx| e.undo(ecx));
            assert_eq!(ed.read(cx).text().as_ref(), "abcdef", "相邻退格合并成一个撤销单元");
        });
    }

    /// 手动性能基准：定位 B1 rope 单真源后 build_frame 的成本构成——同一大文档在
    /// **编辑前（Str 源）** vs **编辑后（Rope 源）** 于不同**带高**下的重建成本：
    /// 若随带线性 → 每块物化/装配；若恒定 → 帧内固定代价（整篇扫描/复制）。
    #[gpui::test]
    #[ignore]
    fn bench_frame_str_vs_rope(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        let para = "alpha beta gamma delta epsilon zeta eta theta one two three four five six seven eight nine ten\n";
        let mut text = String::new();
        while text.len() < 6_000_000 {
            text.push_str(para);
        }
        let ref_text = text.clone();
        for band_h in [200f32, 900., 4000., 40_000.] {
            // ── Str 对照：全新（未编辑）编辑器。
            let (host_s, vcx_s) = cx.add_window_view(|window, cx| {
                let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&ref_text));
                WyHost { ed }
            });
            let vcx_s: &mut VisualTestContext = vcx_s;
            let mut ts = 0u128;
            vcx_s.update(|window, cx: &mut App| {
                for k in 0..6 {
                    let band = (k as f32 * 20., k as f32 * 20. + band_h);
                    let t0 = std::time::Instant::now();
                    let f = build_frame(host_s.read(cx).ed.read(cx), window, px(1200.), band);
                    let _ = f.total;
                    ts += t0.elapsed().as_millis();
                }
            });
            // ── Rope：同一文本，编辑一次 → 转 Rope 源。
            let (host_r, vcx_r) = cx.add_window_view(|window, cx| {
                let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&ref_text));
                WyHost { ed }
            });
            let vcx_r: &mut VisualTestContext = vcx_r;
            let mut tr = 0u128;
            vcx_r.update(|window, cx: &mut App| {
                let ed = host_r.read(cx).ed.clone();
                let at = ed.read(cx).text().as_ref().find("one two three").unwrap() + 4;
                ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "x", ecx));
                for k in 0..6 {
                    let band = (k as f32 * 20., k as f32 * 20. + band_h);
                    let t0 = std::time::Instant::now();
                    let f = build_frame(ed.read(cx), window, px(1200.), band);
                    let _ = f.total;
                    tr += t0.elapsed().as_millis();
                }
            });
            eprintln!(
                "[bench] plain-6MB band_h={band_h}: Str {:.2}ms vs Rope {:.2}ms",
                ts as f64 / 6.,
                tr as f64 / 6.
            );
        }
    }

    /// 手动性能基准：**结构编辑（插入换行 / Enter）**的完整周期成本（edit + build_frame）
    /// ——B3 增量 Pass1 前后对比用：Enter 贴文尾时高度重估只扫「编辑线上」前缀复用；
    /// 并行测「文首 Enter」（前缀少、重估近似全篇）作上界。
    #[gpui::test]
    #[ignore]
    fn bench_structural_enter(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        let mut text = String::new();
        for i in 0..3_000 {
            text.push_str(&format!("$$\n\\int_{i} x dx\n$$\n"));
            text.push_str(&format!("text line {i} with $e^{{i\\pi}}+1={i}$ inline math here\n"));
            text.push_str(&format!("> [!NOTE]- alert body line {i}\n>   continuation {i}\n"));
            text.push_str(&format!("```rust\nfn f{i}() {{}}\n```\n"));
            text.push_str(&format!("| h{i} | h2 |\n| --- | --- |\n| c{i} | d |\n"));
        }
        text.push_str("\n\n");
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        // ── 文尾 Enter（前缀复用最多）。
        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let mut at = ed.read(cx).text().as_ref().len();
            // 预热一帧 + 2 次 Enter（把首次 Pass1/折叠计划/derived 全量重建跑掉），
            // 之后计时的是**稳态 Enter**（增量路径）。
            let _warm = build_frame(ed.read(cx), window, px(1200.), (0., 900.));
            for _ in 0..2 {
                ed.update(cx, |e, ecx| {
                    e.set_caret(at);
                    e.replace_range(at..at, "\n", ecx);
                    at = e.caret();
                });
                build_frame(ed.read(cx), window, px(1200.), (0., 900.));
            }
            let mut t = 0u128;
            for _ in 0..12 {
                let t0 = std::time::Instant::now();
                ed.update(cx, |e, ecx| {
                    e.set_caret(at);
                    e.replace_range(at..at, "\n", ecx);
                    at = e.caret();
                });
                build_frame(ed.read(cx), window, px(1200.), (0., 900.));
                t += t0.elapsed().as_millis();
            }
            eprintln!("[bench] Enter at TAIL+cycle: {:.2}ms/次", t as f64 / 12.);
        });
    }

    /// 手动性能基准：量化 A1「命中先行」的收益——缓存预热后**同带缓存重建**（只物化
    /// miss / caret 块）对比旧行为的「整带 `rows_in_block_window` 物化」单次成本
    /// （旧实现每帧/每带重建都先付整带物化再查缓存）。
    #[gpui::test]
    #[ignore]
    fn bench_cached_band_rebuild_vs_materialize(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        let mut text = String::new();
        for i in 0..3_000 {
            text.push_str(&format!("$$\n\\int_{i} x dx\n$$\n"));
            text.push_str(&format!("text line {i} with $e^{{i\\pi}}+1={i}$ inline math here\n"));
            text.push_str(&format!("> [!NOTE]- alert body line {i}\n>   continuation {i}\n"));
            text.push_str(&format!("```rust\nfn f{i}() {{}}\n```\n"));
            text.push_str(&format!("| h{i} | h2 |\n| --- | --- |\n| c{i} | d |\n"));
        }
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let band = (0., 800.);
            let warm = build_frame(ed.read(cx), window, px(900.), band);
            let nband_blocks = warm.rows.len().max(1);
            // (a) 同带缓存重建（A1 后只物化 caret 行 + 未命中块）。
            let mut t_a = 0u128;
            for _ in 0..20 {
                let t0 = std::time::Instant::now();
                let f = build_frame(ed.read(cx), window, px(900.), band);
                let _ = f.total;
                t_a += t0.elapsed().as_millis();
            }
            // (b) 旧行为每重建必付的「整带物化」成本（同量级块数）。
            let mut t_b = 0u128;
            for _ in 0..20 {
                let t0 = std::time::Instant::now();
                let rows = ed
                    .read(cx)
                    .doc()
                    .rows_in_block_window(0, nband_blocks.saturating_sub(1));
                let _ = rows.len();
                t_b += t0.elapsed().as_millis();
            }
            eprintln!(
                "[bench] cached same-band rebuild: {:.2}ms; full-band materialize(旧每帧必付): {:.2}ms (band blocks={})",
                t_a as f64 / 20.,
                t_b as f64 / 20.,
                nband_blocks
            );
            // 全文档带（`(0., 999999.)`，屏外/整表重建路径）：缓存预热后重建对比整带物化。
            let band_all = (0., 999_999.);
            let _warm_all = build_frame(ed.read(cx), window, px(900.), band_all);
            let n_all = ed.read(cx).doc().block_count();
            let mut t_a2 = 0u128;
            for _ in 0..10 {
                let t0 = std::time::Instant::now();
                let f = build_frame(ed.read(cx), window, px(900.), band_all);
                let _ = f.total;
                t_a2 += t0.elapsed().as_millis();
            }
            let mut t_b2 = 0u128;
            for _ in 0..10 {
                let t0 = std::time::Instant::now();
                let rows = ed
                    .read(cx)
                    .doc()
                    .rows_in_block_window(0, n_all.saturating_sub(1));
                let _ = rows.len();
                t_b2 += t0.elapsed().as_millis();
            }
            eprintln!(
                "[bench] full-doc cached rebuild: {:.2}ms; full-doc materialize(旧每帧必付): {:.2}ms (blocks={})",
                t_a2 as f64 / 10.,
                t_b2 as f64 / 10.,
                n_all
            );
        });
    }

    /// 手动性能基准（`--ignored` + `--nocapture` 才跑）：在「结构密集」大文档上量化
    /// 编辑/滚动的单次成本，供人工复测比对（debug 模式即与 `cargo run` 同构）。
    #[gpui::test]
    #[ignore]
    fn bench_dense_doc_edit_and_scroll(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        // 仿 `大文档测试2.md`：每 6 行一组「数学块 + 行内公式 + callout + 代码 + 表格」，
        // 末尾附一个纯文本段（普通键入的落点）。
        let mut text = String::new();
        for i in 0..3_000 {
            text.push_str(&format!("$$\n\\int_{i} x dx\n$$\n"));
            text.push_str(&format!("text line {i} with $e^{{i\\pi}}+1={i}$ inline math here\n"));
            text.push_str(&format!("> [!NOTE]- alert body line {i}\n>   continuation {i}\n"));
            text.push_str(&format!("```rust\nfn f{i}() {{}}\n```\n"));
            text.push_str(&format!("| h{i} | h2 |\n| --- | --- |\n| c{i} | d |\n"));
        }
        text.push_str("plain paragraph zone one\nplain paragraph zone two\n");
        let nbytes = text.len();
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;

        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let t0 = std::time::Instant::now();
            let f = build_frame(ed.read(cx), window, px(900.), (0., 800.));
            let dt = t0.elapsed().as_millis();
            let nreg = ed.read(cx).derived().borrow().math_regions.len();
            eprintln!("[bench] open+first band frame: {dt}ms ({nbytes}B, blocks={}, math_regions={nreg})", f.block_count());
        });

        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let mut rebuilt = 0u32;
            // 锚点只取一次（此后 1:1 字符替换，长度不变 → 锚点恒有效）。
            let at = ed.read(cx).content().find("zone one").unwrap() + 5;
            let t0 = std::time::Instant::now();
            // **真实逐键循环**：replace_range + 下一帧 build_frame（caret 每键前进，
            // 与真实应用一致——`set_caret`/`replace_range` 都会清帧触发重建）。
            for k in 0..30 {
                let v0 = ed.read(cx).regions_ver();
                ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "x", ecx));
                if ed.read(cx).regions_ver() != v0 {
                    rebuilt += 1;
                }
                let band = (k as f32 * 20., k as f32 * 20. + 800.);
                build_frame(ed.read(cx), window, px(900.), band);
            }
            let dt = t0.elapsed().as_millis();
            eprintln!("[bench] 30 typing full-cycles (edit+frame): {dt}ms ({:.1}ms/key, rebuilt={rebuilt}/30)", dt as f64 / 30.);
        });

        vcx.update(|_w, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let mut rebuilt = 0u32;
            let at = ed.read(cx).content().find("\\int_1 x dx").unwrap() + 5;
            let t0 = std::time::Instant::now();
            for _ in 0..10 {
                let v0 = ed.read(cx).regions_ver();
                ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "2", ecx));
                if ed.read(cx).regions_ver() != v0 {
                    rebuilt += 1;
                }
            }
            let dt = t0.elapsed().as_millis();
            eprintln!("[bench] 10 math-interior edits: {dt}ms ({:.1}ms/key, rebuilt={rebuilt}/10)", dt as f64 / 10.);
        });

        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let mut t = 0u128;
            let mut t2 = 0u128;
            for k in 0..20 {
                let t0 = std::time::Instant::now();
                let band = (k as f32 * 4000., k as f32 * 4000. + 800.);
                let f = build_frame(ed.read(cx), window, px(900.), band);
                let _ = f.total;
                t += t0.elapsed().as_millis();
                // 模拟「公式光栅逐个完成」：mgen 变化 → fold_plan 重算（基础布局不再整篇重建）。
                let t1 = std::time::Instant::now();
                let band2 = (k as f32 * 4000. + 2000., k as f32 * 4000. + 2800.);
                let f2 = build_frame(ed.read(cx), window, px(900.), band2);
                let _ = f2.total;
                t2 += t1.elapsed().as_millis();
            }
            eprintln!("[bench] 20 band rebuilds: {t}ms ({:.1}ms/band)", t as f64 / 20.);
            eprintln!("[bench] 20 near-band rebuilds (caret-in-band): {t2}ms ({:.1}ms/band)", t2 as f64 / 20.);
        });

        // 光标移动（不动内容）→ `set_caret` 清帧 → build_frame 每步成本。
        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let n = ed.read(cx).content().len();
            let mut t = 0u128;
            for k in 0..20 {
                let off = (n * k / 20).min(n.saturating_sub(1));
                ed.update(cx, |e, _| e.set_caret(off));
                let t0 = std::time::Instant::now();
                let band = (k as f32 * 4000., k as f32 * 4000. + 800.);
                let f = build_frame(ed.read(cx), window, px(900.), band);
                let _ = f.total;
                t += t0.elapsed().as_millis();
            }
            eprintln!("[bench] 20 caret-move frame rebuilds: {t}ms ({:.1}ms/step)", t as f64 / 20.);
        });
    }

    /// 编辑路径分项剖析：整串复制 / SharedString 转 / Doc::reopen / 通知 各占多少。
    #[gpui::test]
    #[ignore]
    fn bench_edit_path_split(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        let mut text = String::new();
        let para = "alpha beta gamma delta epsilon zeta eta theta one two three four five six seven eight nine ten\n";
        while text.len() < 6_000_000 {
            text.push_str(para);
        }
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let at = ed.read(cx).content().find("one two three").unwrap() + 4;
            let s = ed.read(cx).content();
            let r = at..at + 1;
            // (a) 整串复制。
            let mut t = 0u128;
            let mut next = String::new();
            for _ in 0..20 {
                let t0 = std::time::Instant::now();
                let mut n = String::with_capacity(s.len() - r.len() + 1);
                n.push_str(&s[..r.start]);
                n.push_str("x");
                n.push_str(&s[r.end..]);
                next = n;
                t += t0.elapsed().as_millis();
            }
            eprintln!("[bench] (a) full-string copy: {:.2}ms", t as f64 / 20.);
            // (b) SharedString::from。
            let mut t = 0u128;
            for _ in 0..20 {
                let t0 = std::time::Instant::now();
                let _ss = gpui::SharedString::from(next.clone());
                t += t0.elapsed().as_millis();
            }
            eprintln!("[bench] (b) SharedString::from(String): {:.2}ms", t as f64 / 20.);
            // (c) Doc::edit（原地 rope 编辑，替代旧 `reopen` 的整串拼接+重建）。
            let mut t = 0u128;
            let s_ss = ed.read(cx).text();
            let mut doc = crate::markdown::doc::Doc::open(s_ss.clone());
            for _ in 0..20 {
                let t0 = std::time::Instant::now();
                doc.edit(r.clone(), "x");
                t += t0.elapsed().as_millis();
            }
            eprintln!("[bench] (c) Doc::edit incremental: {:.2}ms", t as f64 / 20.);
            // (e) 新零拷贝路径：String → Box<str> → Arc<str> → SharedString。
            let mut t = 0u128;
            for _ in 0..20 {
                let mut x = String::with_capacity(s.len());
                x.push_str(&s[..r.start]);
                x.push_str("x");
                x.push_str(&s[r.end..]);
                let t0 = std::time::Instant::now();
                let _ss =
                    gpui::SharedString::from(std::sync::Arc::<str>::from(x.into_boxed_str()));
                t += t0.elapsed().as_millis();
            }
            eprintln!("[bench] (e) new zero-copy concat+SharedString: {:.2}ms", t as f64 / 20.);
            // (f) rebuild_block_regions（replace_range 每键 O(块数) 扫描）。
            let mut t = 0u128;
            let mut doc2 = crate::markdown::doc::Doc::open(s_ss.clone());
            for _ in 0..20 {
                doc2.edit(r.clone(), "x");
                let n = doc2.block_count();
                let t0 = std::time::Instant::now();
                let mut regions: usize = 0;
                for i in 0..n {
                    if let Some(m) = doc2.block_meta(i) {
                        if matches!(
                            m.kind,
                            crate::markdown::parser::RowKind::Code
                                | crate::markdown::parser::RowKind::Table
                        ) {
                            regions += 1;
                        }
                    }
                }
                let _ = regions;
                t += t0.elapsed().as_millis();
            }
            eprintln!("[bench] (f) rebuild_block_regions scan (n={}): {:.2}ms", doc2.block_count(), t as f64 / 20.);
            // (d) board emits 未计入（UI 层）。总 edit-only 对照。
            let _ = window;
            let _ = ed;
        });
    }

    #[gpui::test]
    #[ignore]
    fn bench_scale_with_doc_size(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use gpui::App;
        cx.update(gpui_component::init);
        // —— 1) 纯文本大文档：键入成本随字节数线性放大？——
        let mut text = String::new();
        let para = "alpha beta gamma delta epsilon zeta eta theta one two three four five six seven eight nine ten\n";
        let mut mb = 0usize;
        while mb < 6_000_000 {
            text.push_str(para);
            mb = text.len();
        }
        let (host, _vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = _vcx;
        vcx.update(|window, cx: &mut App| {
            let ed = host.read(cx).ed.clone();
            let at = ed.read(cx).content().find("one two three").unwrap() + 4;
            let mut t = 0u128;
            // replace_range 单独（不 rebuild frame）→ 量化整串复制 + reopen。
            for _ in 0..30 {
                let t0 = std::time::Instant::now();
                ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "x", ecx));
                t += t0.elapsed().as_millis();
            }
            eprintln!(
                "[bench] plain-6MB edit-only: {t}ms ({:.1}ms/key, text={}B)",
                t as f64 / 30.,
                ed.read(cx).content().len()
            );
            // 完整键入周期（edit + band rebuild）。
            let mut t2 = 0u128;
            for k in 0..30 {
                let t0 = std::time::Instant::now();
                ed.update(cx, |e, ecx| e.replace_range(at..at + 1, "y", ecx));
                let band = (k as f32 * 20., k as f32 * 20. + 900.);
                build_frame(ed.read(cx), window, px(1200.), band);
                t2 += t0.elapsed().as_millis();
            }
            eprintln!(
                "[bench] plain-6MB typing-cycle: {t2}ms ({:.1}ms/key)",
                t2 as f64 / 30.
            );
        });

        // —— 2) 代码密集文档：滚动重建里代码块占多少（语法高亮重解析）？——
        let mut code = String::new();
        for i in 0..2_000 {
            code.push_str(&format!("```rust\nfn compute_{i}(a: u32, b: u32) -> u32 {{\n    let sum = a + b * {i};\n    let diff = a.saturating_sub(b);\n    if sum > diff {{ return sum; }}\n    diff\n}}\n```\n"));
            code.push_str(&format!("some paragraph text {i} after code block\n"));
        }
        let (host2, _vcx2) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&code));
            WyHost { ed }
        });
        let vcx2: &mut VisualTestContext = _vcx2;
        let mut t3 = 0u128;
        let mut t4 = 0u128;
        vcx2.update(|window, cx: &mut App| {
            let ed = host2.read(cx).ed.clone();
            let n = ed.read(cx).doc().block_count();
            let total = n;
            let _ = ed.read(cx).derived();
            // 预热基础布局 + 派生扫描。
            build_frame(ed.read(cx), window, px(1200.), (0., 2400.));
            for k in 0..12 {
                let top = (total * k / 12).min(total.saturating_sub(1)) as f32;
                let band = (top * 22., top * 22. + 2400.); // 每块 ~22px 的近似推进
                let t0 = std::time::Instant::now();
                let f = build_frame(ed.read(cx), window, px(1200.), band);
                t3 += t0.elapsed().as_millis();
                let _ = f.rows.len();
                // 前进半个屏幕（512px 量程内不同带 → 命中 1 次重建）。
                let t1 = std::time::Instant::now();
                let band2 = (top * 22. + 300., top * 22. + 2700.);
                let _ = build_frame(ed.read(cx), window, px(1200.), band2);
                t4 += t1.elapsed().as_millis();
            }
            eprintln!(
                "[bench] code-heavy band rebuild: fresh {:.1}ms, +300px-shift {:.1}ms (blocks={}, code blocks≈{})",
                t3 as f64 / 12.,
                t4 as f64 / 12.,
                total,
                total / 2
            );
        });
    }

    #[gpui::test]
    fn handler_large_doc_cjk_roundtrip_no_full_materialize(cx: &mut TestAppContext) {
        use gpui::EntityInputHandler;
        cx.update(gpui_component::init);
        // 大文档：8000 行混合 ASCII/中文/emoji——行索引目标落在文档中后部（半篇~整篇）。
        // 同时含尾随换行（幻影尾行）与多字节字符，覆盖 UTF-16 换算的边界。
        let mut text = String::new();
        for i in 0..8000 {
            if i % 17 == 0 {
                text.push('中');
            }
            if i % 31 == 0 {
                text.push('\u{1f680}');
            }
            text.push_str(&format!("line {i}\n"));
        }
        text.push_str("你好 target 尾\n");
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            WyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // `reference` 逐编辑镜像 doc，作为全文比对与 UTF-16 换算的参照。
            let mut reference = text.clone();
            let tail = "你好 target 尾\n";
            ed.update(cx, |e, cx| {
                let calls0 = e.text_shared_calls();
                // ---- 阶段 A：纯协议查询/编辑，全部不物化全文（期间不调 e.text()）----
                // ① 总长 = 全文 UTF-16 单元数（O(1) 计数器）。
                assert_eq!(
                    e.text_length_utf16(window, cx),
                    Some(reference.encode_utf16().count()),
                    "total_utf16 应等于全文 UTF-16 长"
                );
                // ② text_for_range：文档末尾一段（跨中文/emoji/换行），切片精确。
                let total_u16 = reference.encode_utf16().count();
                let start_u16 = total_u16 - tail.encode_utf16().count();
                let mut actual = None;
                let got = e
                    .text_for_range(start_u16..total_u16, &mut actual, window, cx)
                    .expect("text_for_range 应命中");
                assert_eq!(got, tail, "text_for_range 切片应精确（含中文/emoji/换行）");
                assert_eq!(actual, Some(start_u16..total_u16), "actual_range 应回填");
                // ③ 选区：光标到文末 → UTF-16 末端零宽选区。
                e.set_caret(reference.len());
                let sel = e.selected_text_range(true, window, cx).unwrap();
                assert_eq!(sel.range, total_u16..total_u16, "caret 在文末");
                assert!(!sel.reversed);
                // ④ set_selected_text_range 设置中后部，UTF-16 往返无损。
                let mid_u16 = total_u16 / 2;
                e.set_selected_text_range(mid_u16..mid_u16 + 1, window, cx);
                assert!(e.caret() <= reference.len(), "caret 应钳在字节界内");
                let sel2 = e.selected_text_range(true, window, cx).unwrap();
                assert_eq!(sel2.range, mid_u16..mid_u16 + 1, "UTF-16 往返无损");
                // ⑤ marked 组合：字节存储 → UTF-16 换算精确；取消（ESC）清占位。
                let caret_byte = reference.len() - tail.len();
                e.set_caret(caret_byte);
                let mark_u16 = reference[..caret_byte].encode_utf16().count();
                e.replace_and_mark_utf16(None, "组合", None, cx);
                reference.insert_str(caret_byte, "组合");
                let marked = e.marked_text_range(window, cx).expect("有 marked");
                assert_eq!(marked.start, mark_u16, "marked 起点 UTF-16");
                assert_eq!(marked.end - marked.start, "组合".encode_utf16().count());
                e.replace_text_utf16(None, "", cx);
                reference.replace_range(
                    caret_byte..caret_byte + "组合".len(),
                    "",
                );
                // ⑥ Windows 组合序列（range=None，替换旧 marked 不堆叠）：n→nih→提交"你好"。
                e.set_caret(reference.len());
                e.replace_and_mark_utf16(None, "n", None, cx);
                e.replace_and_mark_utf16(None, "nih", None, cx);
                e.replace_text_utf16(None, "你好", cx);
                reference.push_str("你好");
                // ⑦ 显式 UTF-16 区间（macOS 风格）走 utf16_to_byte 换算：文末插 X。
                let pos_u16 = reference.encode_utf16().count();
                e.replace_text_utf16(Some(pos_u16..pos_u16), "X", cx);
                reference.push('X');
                // ---- 断言：阶段 A 的查询/编辑全程零次 text_shared（O(n) 全拷）----
                assert_eq!(
                    e.text_shared_calls(),
                    calls0,
                    "EntityInputHandler 的查询/编辑不得调用 text_shared——必须走行索引/切片/增量计数"
                );
                // ---- 收尾：全文精确比对（e.text() 触发 text_shared 属正常宿主写回路径）----
                assert_eq!(&e.text(), &reference, "大文档协议层编辑后全文与参照逐字节一致");
            });
        });
    }