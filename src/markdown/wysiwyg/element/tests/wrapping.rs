#[gpui::test]
fn inline_code_pill_ranges_expand_over_whitespace(cx: &mut TestAppContext) {
    // 行内代码（反引号）在 shape 期记录渲染字节区间（已扩展空白）供 paint 自绘圆角
    // pill——与预览 `TextTrack::paint_code_ranges` 同一行为源。用贴题原句验证 `we`。
    use crate::markdown::ir::IrKind;
    cx.update(crate::actions::bind_local_keymaps);
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
        assert_eq!(
            theme.code_inline_bg, p.code_inline_bg,
            "pill 底色抄自 StyleParams"
        );
    });
}

#[gpui::test]
fn wrapped_row_click_on_second_visual_line_lands_in_second_half(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
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
        let w0 = frame.rows[0].wrapped.first().expect("行应有 shaped 结果");
        assert!(
            !w0.wrap_boundaries().is_empty(),
            "长文本应折行：wrap 边界 {} 个",
            w0.wrap_boundaries().len()
        );
        let line_h = f32::from(frame.line_h);
        let pad = f32::from(frame.padding);
        let top = f32::from(frame.rows[0].top);
        // 第一可视行与第二可视行各自的「行内靠左」点击（x 相同、y 分属两个可视行）。
        let p1 = point(px(pad + 5.), px(top + line_h * 0.5));
        let p2 = point(px(pad + 5.), px(top + line_h * 1.5));
        let o1 = source_offset_at_local(ed.read(cx), &frame, p1);
        let o2 = source_offset_at_local(ed.read(cx), &frame, p2);
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

/// 单一来源回归：WYSIWYG 帧内衬（`frame.padding`）必须等于 `CONTENT_EDGE_PAD_X_PX`
/// （与预览行内 px_3 同源，见 `config::markdown`）；命中/光标坐标（`caret_geometry`）
/// 从 `frame.padding` 推导——改内衬常量，命中/光标/划线/拖拽几何全站一致、不散落数值。
#[gpui::test]
fn frame_padding_single_source_and_caret_derives_from_it(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "普通段落 abcdefghijklmnopqrstuvwxyz\n\n```\nlet x = 1;\n```\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        assert_eq!(
            f32::from(frame.padding),
            crate::config::markdown::CONTENT_EDGE_PAD_X_PX,
            "帧内衬应读单一来源 CONTENT_EDGE_PAD_X_PX"
        );
    });
    // 光标 x 从同一内衬推导：段落行 caret=0 的 x = frame.padding + x_off（正文 x_off=0）。
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| e.set_caret(0));
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let (on, caret_x, _) = caret_geometry(ed.read(cx), &frame);
        assert!(on, "段落行应有光标");
        let para = frame
            .rows
            .iter()
            .find(|r| r.kind == crate::markdown::ir::IrKind::Paragraph)
            .expect("应有段落行");
        let expect_x = f32::from(frame.padding) + f32::from(para.x_off);
        assert!(
            (f32::from(caret_x) - expect_x).abs() < 0.5,
            "光标 x 应 = frame.padding + x_off（同一内衬推导）：expect {expect_x:.1}，实测 {:.1}",
            f32::from(caret_x)
        );
    });
}

/// 防回归：f916fce 把文本折行宽改成「帧宽 − 帧内衬×2 − 行 gutter」后，引述/警报背景带
/// 曾按「帧宽 − 内衬×2 − 再内缩」画宽 → 带右缘比内容盒右缘短 24px，窄窗折行时最右
/// ~1.5 个汉字漏背景。本测试锁定渲染侧不变量（paint/shape 同源几何）：
/// ① 背景带宽（`quote_band_w`，paint 唯一取值）== 内容盒宽（帧宽 − 2×内衬）；
/// ② 每一折行可视行的文字右缘 ≤ 内容盒右缘（帧宽 − 内衬）——全宽背景带盖住全部文字；
/// ③ 折叠/展开两种 `[!NOTE]` 态都对。窄窗（折行）与全屏（不折行）都断言。
#[gpui::test]
fn quote_band_covers_all_wrapped_text_right_edges(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::actions::bind_local_keymaps);
    // 引述/警报正文：掺入 ASCII 高宽 token（测试环境 CJK 字宽≈0，ASCII 才撑起折行），
    // 使窄窗（300/400）内每行必然折成 ≥2 可视行；后带 `[!NOTE]-`（默认折叠），
    // 测试里先展开让 body 行参与带内整形、再折叠回验证 0 高不物化。
    let body = "这是很长的一段引用文字，用来在窄窗口里触发折行，最右侧会出现被背景漏盖的字符与符号，比如箭头→ ref-1234567890 xyz@mail.com END。";
    let long = body.repeat(8);
    let text = format!(
        "> {long}\n> {long}\n> [!NOTE]- 警报标题这一行也很长，会触发折行背景带检查 END\n> {long}\n"
    );
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text.as_str()));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            // 展开 `[!NOTE]-`（`-` 默认折叠；预览里点箭头/光标 reveal 即此态），让 body 行
            // 参与带内整形——折叠 body 不物化、无背景画。
            ed.update(cx, |e, cx| e.toggle_alert_fold_at_line(2, cx));
            assert!(ed.read(cx).text().contains("> [!NOTE]+"), "应已展开为 +");
            for width in [300.0_f32, 400.0_f32, 800.0_f32, 1600.0_f32] {
                let expect_wrap = width < 800.;
                let frame = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
                let pad = f32::from(frame.padding);
                let avail = f32::from(frame.width) - 2. * pad;
                let content_right = f32::from(frame.width) - pad; // 内容盒右缘（= 带右缘合同）
                // ① 背景带宽单一来源 helper == 内容盒宽（paint 与测试共用；回归 → 这里断）。
                let band_w = quote_band_w(&frame);
                assert!(
                    (band_w - avail).abs() < 0.5,
                    "宽 {width}: 带宽应为内容盒宽 avail={avail:.1}，实际 {band_w:.1}"
                );
                assert!(
                    band_w + pad <= content_right + 0.5,
                    "宽 {width}: 带右缘 {:.1} 不应越出内容盒右缘 {content_right:.1}",
                    band_w + pad
                );
                let quotes: Vec<_> = frame
                    .rows
                    .iter()
                    .filter(|r| r.kind == IrKind::Quote && !r.wrapped.is_empty())
                    .collect();
                assert!(
                    quotes.len() >= 3,
                    "宽 {width}: 展开态应含 2 引述 + 警报 marker + body，实际 {}",
                    quotes.len()
                );
                // ② 每一条折行可视行的文字右缘 ≤ 内容盒右缘（全宽背景带必盖住；折行行
                //    `wl.width()` 会被折行宽封顶 → 恰为 content_right，正是旧带漏盖的区间）。
                let mut max_text_right = 0.0_f32;
                let mut wrapped_rows = 0usize;
                for row in &quotes {
                    let x_off = f32::from(row.x_off);
                    assert!(
                        x_off >= 0.,
                        "宽 {width}: 引述/警报 x_off 不应为负（{x_off:.1}）"
                    );
                    if row
                        .wrapped
                        .iter()
                        .any(|wl| !wl.wrap_boundaries().is_empty())
                    {
                        wrapped_rows += 1;
                    }
                    // 一个源码行 = 一个 WrappedLine（子行在 wrap_boundaries 内）；折行行的
                    // `wl.width()` 会被折行宽封顶 → tr = content_right，正是旧带漏盖的区间。
                    for wl in row.wrapped.iter() {
                        let tr = pad + x_off + f32::from(wl.width());
                        max_text_right = max_text_right.max(tr);
                        assert!(
                            tr <= content_right + 0.5,
                            "宽 {width}: 文字右缘 {tr:.1} 超出内容盒右缘 {content_right:.1}（带右缘 {band_w:.0}+{pad:.0}={:.1}），背景会漏盖",
                            band_w + pad
                        );
                    }
                }
                if expect_wrap {
                    assert!(
                        wrapped_rows > 0,
                        "窄窗（{width}）应有折行（scan wrap_boundaries）的引述块，测试才能锁定漏盖回归"
                    );
                    assert!(
                        max_text_right > content_right - 120.,
                        "窄窗文字右缘应接近内容盒右缘（折行确实顶到右缘），实际 max={max_text_right:.1}"
                    );
                }
            }
            // ③ 折叠回 `-`：body 隐藏（0 高不物化），marker 行自身带不变量照旧成立。
            ed.update(cx, |e, cx| e.toggle_alert_fold_at_line(2, cx));
            assert!(ed.read(cx).text().contains("> [!NOTE]-"), "应折叠回 -");
            let frame = build_frame(ed.read(cx), window, px(400.), (0., 999_999.));
            let pad = f32::from(frame.padding);
            let content_right = f32::from(frame.width) - pad;
            assert!(
                quote_band_w(&frame) + pad <= content_right + 0.5,
                "折叠态带右缘 {:.1} 不越出内容盒右缘 {content_right:.1}",
                quote_band_w(&frame) + pad
            );
            let marker = frame
                .rows
                .iter()
                .find(|r| r.kind == IrKind::Quote && r.extra.gutter.is_some())
                .expect("折叠态应有警报 marker 行");
            for wl in marker.wrapped.iter() {
                let tr = pad + f32::from(marker.x_off) + f32::from(wl.width());
                assert!(
                    tr <= content_right + 0.5,
                    "折叠态 marker 文字右缘 {tr:.1} 也应 ≤ 内容盒右缘 {content_right:.1}"
                );
            }
            // body 高应为 0（真正折叠，非 reveal）。
            let body_ix = frame.block_index_for_src_line(3).expect("警报 body 块");
            assert_eq!(
                frame.block_height(body_ix),
                0.,
                "折叠后警报 body 应 0 高（不物化）"
            );
        });
}

/// 防回归：行首装饰（有序号 `N.` / 项目符号 / 任务框）必须垂直对齐**首可视行文字中心**。
/// 旧 `paint_row_gutter` 用 `row.height / 2`（整块各折行带之和的中点）当标记中心——折成
/// N 行时标记掉到第 N/2 折行行（有序长项 `1.` 跑到中间折行行的 bug 根源；单行项因
/// `height≈lh` 恰好看着对）。修复后标记中心 = `first_band_center_y`，恰为 `paint_text_row`
/// 画第一折行的几何推导（`top + text_y_off + text_row_shift + lh/2`）。
/// 同一「真实折行渲染路径」（`build_frame`）内锁定：长有序/无序/任务项各 ▶ 折成 ≥3 子行
/// （测试前提）+ 首行中心断言（与 paint_text_row 同式）+ 旧公式偏离 ≥1 行高（证明断言
/// 真能锁住「标记掉到中间折行行」的回归）；短项 ▶ 单行中心 = `top + lh/2`（修复前后一致）。
#[gpui::test]
fn list_marker_vertical_center_tracks_first_wrapped_line(cx: &mut TestAppContext) {
    use super::{first_band_center_y, text_row_shift, text_y_off};
    cx.update(crate::actions::bind_local_keymaps);
    // 测试环境 CJK 字宽≈0，ASCII 才撑起折行：长项用 ASCII token 重复到必然折成多行，
    // 短项保持单行（锁「单行行为不变」）。
    let long = "word-1234567890 ".repeat(60);
    let text = format!(
        "1. 有序列表项 A {long}END\n- 无序列表项 B {long}END\n- [ ] 任务列表项 C {long}END\n2. 短项 END\n"
    );
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text.as_str()));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
        let gutters: Vec<_> = frame
            .rows
            .iter()
            .filter(|r| r.extra.gutter.is_some())
            .collect();
        assert!(
            gutters.len() >= 4,
            "应有 ≥4 个 gutter 行（有序长/无序长/任务长/短项），实际 {}",
            gutters.len()
        );
        for (ix, row) in gutters.iter().enumerate() {
            let w0 = row.wrapped.first().expect("gutter 行应有整形结果");
            let top = f32::from(row.top);
            let lh = f32::from(row.lh);
            // paint_text_row 画第一折行的中心 = 首带起点 + 撑高下移 + 半行高。
            let expect = top + text_y_off(row) + text_row_shift(row) + lh * 0.5;
            let center = first_band_center_y(row);
            assert!(
                (center - expect).abs() < 0.5,
                "[{ix}] 标记中心应=首行文字中心 {expect:.2}，实际 {center:.2}（top={top:.2} lh={lh:.2}）"
            );
            let sub_lines = w0.wrap_boundaries().len() + 1;
            let old = top + f32::from(row.height) * 0.5;
            if sub_lines >= 3 {
                // 旧公式（height/2）落在 ≥2 子带 → 与首行中心差 ≥1 lh：此断言真能锁回归。
                assert!(
                    (old - expect).abs() >= lh - 0.5,
                    "[{ix}] 折成 {sub_lines} 行时旧 height/2 应偏离首行中心 ≥1lh（差分 {:.2}）",
                    (old - expect).abs()
                );
            } else {
                // 短项（单行）：`height == lh` → 旧/新一致，修复后单行行为不变。
                assert_eq!(sub_lines, 1, "[{ix}] 短项不应折行");
                assert!(
                    (old - expect).abs() < 0.5,
                    "[{ix}] 单行项标记中心仍应=top+lh/2（修复前后一致），实际 {center:.2}"
                );
            }
        }
    });
}

