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
    use super::{MathPos, math_to_segments, math_to_unicode};
    let src = r"\int_{-\infty}^{\infty} e^{-x^2} dx = \sqrt{\pi}";
    let segs = math_to_segments(src);
    assert!(
        segs.iter()
            .any(|s| s.pos == MathPos::Sub && s.text.contains('∞')),
        "下标组应拆出 Sub 段（含 ∞）：{segs:?}"
    );
    assert!(
        segs.iter()
            .any(|s| s.pos == MathPos::Sup && s.text.contains('∞')),
        "上标组应拆出 Sup 段（含 ∞）：{segs:?}"
    );
    // 平铺后 = 旧的扁平字符串（行内数学/兼容路径输出不变）。
    let flat: String = segs.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        flat,
        math_to_unicode(src),
        "segments 平铺应与 math_to_unicode 一致"
    );
    // `e^{-x^2}` 的指数也应在 Sup 段里。
    assert!(
        segs.iter()
            .any(|s| s.pos == MathPos::Sup && s.text.contains('²')),
        "e^(-x^2) 的指数应在 Sup 段：{segs:?}"
    );
    // `\sum_{i=1}^{n}`：下标/上标各一段，且平铺含 ∑。
    let sum_segs = math_to_segments(r"\sum_{i=1}^{n} i");
    assert!(
        sum_segs
            .iter()
            .any(|s| s.pos == MathPos::Sub && s.text.contains('ᵢ'))
    );
    assert!(
        sum_segs
            .iter()
            .any(|s| s.pos == MathPos::Sup && s.text.contains('ⁿ'))
    );
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
    assert!(
        (short - 16.0 / 22.0 * 25.0).abs() < 1e-3,
        "矮公式按字号缩放：{short}"
    );
    assert!(short <= 19.0, "25px 高公式 × 字号缩放后仍在行高内：{short}");
    // 中等公式：字号缩放略超行高 → 所在行会小幅撑高（不再压扁）。
    let mid = inline_math_fit_h(fs, lh, 30.0);
    assert!(
        (mid - 16.0 / 22.0 * 30.0).abs() < 1e-3,
        "仍按字号缩放：{mid}"
    );
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
    cx.update(crate::actions::bind_local_keymaps);
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
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
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
    cx.update(crate::actions::bind_local_keymaps);
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
        ed.update(cx, |e, _| e.set_caret(text.find("尾").unwrap())); // 光标在块外（末行）
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
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
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
        assert_eq!(
            caret_before,
            text.find("尾").unwrap(),
            "单击卡片不得移动光标"
        );
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
    cx.update(crate::actions::bind_local_keymaps);
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
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
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
            f3.rows
                .iter()
                .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
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
    cx.update(crate::actions::bind_local_keymaps);
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
fn math_card_height_is_single_value_across_slot_states(cx: &mut TestAppContext) {
    // 块级卡高**单值**：只由源码显示行决定（普通行 `MATH_DISPLAY_LINE_H`、
    // 高算子行 `MATH_DISPLAY_LINE_TALL_H`），排版槽 Loading/Ready/Failed 同高
    // ——替换位图/回退不跳卡高（块级输入「大块异常↔正常块」两态回弹的根因是旧实现
    // Ready 用 `20+位图高`、回退用 `20+n×22` 两个不同高来回跳）。
    use crate::config::math;
    use crate::markdown::parse::inline_style;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    // 高算子单行（∫ + √）→ 20 + 70 = 90，两态同值。
    let text = "$$\n\\int_{-\\infty}^{\\infty} e^{-x^2} dx = \\sqrt{\\pi}\n$$\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let reg = inline_style::math_regions(&ed.read(cx).content())
            .into_iter()
            .next()
            .expect("应扫到一块公式");
        // 未就绪（回退）与就绪（任何位图）卡高必须**完全一致**。
        let h_empty = f32::from(super::math_card_height(ed.read(cx), &reg));
        let buf = image::RgbaImage::from_raw(40, 24, vec![0u8; 40 * 24 * 4]).unwrap();
        let img = std::sync::Arc::new(gpui::RenderImage::new(vec![image::Frame::new(buf)]));
        ed.read(cx).math_slots().borrow_mut().insert(
            reg.source.clone().into(),
            super::super::MathSlot::Ready(img, 5000., 68.5),
        );
        let h_ready = f32::from(super::math_card_height(ed.read(cx), &reg));
        assert!(
            (h_empty - h_ready).abs() < 0.01,
            "Loading 与 Ready 卡高必须同值：h_empty={h_empty} h_ready={h_ready}"
        );
        assert!(
            (h_empty - (20. + math::MATH_DISPLAY_LINE_TALL_H)).abs() < 0.01,
            "高算子单行估高 = 20 + 70 = 90（实测位图 68.5 高刚好放得下、不缩）: {h_empty}"
        );
        // 普通两行（`\\` 行断，无高算子）→ 20 + 2×40 = 100，同样两态同值。
        ed.update(cx, |e, _| {
            e.math_slots().borrow_mut().clear();
            e.set_source_text("$$\na \\\\ b\n$$");
        });
        let reg2 = inline_style::math_regions(&ed.read(cx).content())
            .into_iter()
            .next()
            .expect("第二块公式");
        let he = f32::from(super::math_card_height(ed.read(cx), &reg2));
        assert!(
            (he - (20. + 2. * math::MATH_DISPLAY_LINE_H)).abs() < 0.01,
            "普通两行估高 = 20 + 80 = 100: {he}"
        );
    });
}

#[gpui::test]
fn math_ready_bumps_gen_and_rebuilds_card_height(cx: &mut TestAppContext) {
    // 回归（样式 bug：单行数学块灰底比公式矮/公式越界）：`sync_math` 的
    // Loading→Ready 完成必须自增 `math_gen`，使帧/折叠计划重建（把卡上画的位图从
    // Unicode 回退换成真位图、并按新自然宽启用横向滚动）。旧实现 `insert(key,
    // Ready).is_none()` 判定恒假（key 必先以 Loading 占位 → insert 返回 Some(Loading)），
    // `on_math_ready` 从不触发 → `math_gen` 恒 0 → 卡片永远停在 Unicode 回退绘制。
    // （卡高本身改为与槽位无关的单值估计：`math_card_height` 不再需要就绪来换高，
    // 换代只换图与宽，不再「大块异常↔正常」跳卡高。）
    use crate::markdown::parse::inline_style;
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    // 与 `math_typeset::tests::rasterizes_to_nonempty_bgra` 同款——ratex 确定支持。
    // 块后跟一行正文：光标放到文尾落在块外（否则落在闭合 `$$` 行会 reveal 成源码）。
    let text = "$$\n\\int_{-\\infty}^{\\infty} e^{-x^2} dx = \\sqrt{\\pi}\n$$\n后文";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let frame = vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 光标放到文档尾（`$$` 之外）→ 数学块折叠成卡片（不 reveal）。
        let n = ed.read(cx).content().len();
        ed.update(cx, |e, _| e.set_caret(n));
        let f = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        assert!(
            f.rows.iter().any(|r| r.extra.card.is_some()),
            "帧应含数学卡片行"
        );
        f
    });
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, ecx| e.sync_math(ecx, &frame));
        let slots = ed.read(cx).math_slots();
        assert!(
            slots
                .borrow()
                .contains_key(r"\int_{-\infty}^{\infty} e^{-x^2} dx = \sqrt{\pi}"),
            "sync_math 应先以 Loading 占位"
        );
    });
    // 驱动后台光栅 + 前台续体（on_math_ready 自增 math_gen）。
    // 注意：`VisualTestContext::run_until_parked` 只排空后台 executor，
    // `Context::spawn` 的续体（this.update → on_math_ready）要在前台 dispatcher
    // 上跑——必须用 `TestAppContext::run_until_parked`，故先释放 vcx 再重取。
    let _ = vcx;
    cx.run_until_parked();
    let win = cx.windows()[0];
    let mut vcx = VisualTestContext::from_window(win, cx);
    let (mgenv, mch, row_h, slot_dbg) = vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let genv = ed.read(cx).math_gen();
        let slots = ed.read(cx).math_slots();
        let slot_dbg = format!(
            "{:?}",
            slots
                .borrow()
                .get(r"\int_{-\infty}^{\infty} e^{-x^2} dx = \sqrt{\pi}")
                .map(|s| match s {
                    super::super::MathSlot::Loading => "Loading",
                    super::super::MathSlot::Failed => "Failed",
                    super::super::MathSlot::Ready(..) => "Ready",
                })
        );
        let reg = inline_style::math_regions(&ed.read(cx).content())
            .into_iter()
            .next()
            .expect("应扫到一块公式");
        assert_eq!(
            reg.source,
            r"\int_{-\infty}^{\infty} e^{-x^2} dx = \sqrt{\pi}"
        );
        let h = f32::from(super::math_card_height(ed.read(cx), &reg));
        let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let row_h = f2
            .rows
            .iter()
            .find(|r| r.extra.card.is_some())
            .map(|r| f32::from(r.height))
            .unwrap_or(0.);
        (genv, h, row_h, slot_dbg)
    });
    assert!(
        mgenv > 0,
        "公式完成应自增 math_gen（旧 bug：恒 0）：mgen={mgenv} slot={slot_dbg}"
    );
    assert!(
        mch > 42.0 + 1.,
        "公式就绪后卡片应按位图高（> 42px 回退高）：mch={mch}"
    );
    assert!(
        (row_h - mch).abs() < 1.0,
        "重建帧的卡片行高应等于就绪高：row_h={row_h} mch={mch}"
    );
    assert!(row_h > 42.0 + 1.0, "重建帧不应再停在回退高：row_h={row_h}");
}

#[gpui::test]
fn math_ready_invalidates_only_affected_rows_still_renders_ready(cx: &mut TestAppContext) {
    // 回归（A-3）：公式就绪不得再让整带无关块全 miss 重排——旧方案 `mgen` 进全局
    // shape_key，头less 实测每个公式完成整带 67/68 行全重排一次。改动后：行内公式
    // 行因 spacer 换代由 `on_math_ready` **定向作废**重排（且必须真的换真图）、
    // `$$` 卡行在 Pass2 blocked 不入缓存无需作废、其余段落保持命中。同时正确性不丢：
    // 行内行就绪后帧挂 `ready` 位图、`$$` 卡高按真位图（> 42px 回退高）。
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
    let text = "para0 some words here\npara1 some more words\n$$\nu = v^2\n$$\ninline $a+b$ tail\npara2 words enough\npara3 words enough\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let buf = image::RgbaImage::from_raw(60, 40, vec![0u8; 60 * 40 * 4]).unwrap();
    let img = std::sync::Arc::new(gpui::RenderImage::new(vec![image::Frame::new(buf)]));
    let block_key = vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| e.set_caret(0));
        let f = build_frame(ed.read(cx), window, px(600.), (0., 9999.));
        let block_key = f
            .rows
            .iter()
            .find_map(|r| match &r.extra.card {
                Some(super::BlockCard::Math { latex, .. }) => Some(latex.trim().to_string()),
                _ => None,
            })
            .expect("$$ 卡");
        let inline_key = f
            .rows
            .iter()
            .find_map(|r| r.extra.inline_maths.first())
            .map(|m| m.key.to_string())
            .expect("行内公式");
        // 真实管线：sync_math 登记「行内公式→行」并占位 Loading（无窗口下 spawn 不跑，
        // 下面的手动 Ready + on_math_ready 就是那个「后台完成」）。
        ed.update(cx, |e, ecx| e.sync_math(ecx, &f));
        let _ = ed.read(cx).take_reshapes();
        // 行内公式就绪：只该行重排（其余命中），且新帧该行挂 Ready 位图。
        ed.update(cx, |e, ecx| {
            e.math_slots().borrow_mut().insert(
                inline_key.as_str().into(),
                super::super::MathSlot::Ready(img.clone(), 30., 40.),
            );
            e.on_math_ready(&inline_key, ecx);
        });
        let f2 = build_frame(ed.read(cx), window, px(600.), (0., 9999.));
        let resh = ed.read(cx).take_reshapes();
        assert!(
            resh <= 3,
            "行内公式就绪应只重排该行/边界（修复前整带全 miss）：resh={resh}"
        );
        let ready_row = f2
            .rows
            .iter()
            .find(|r| r.extra.inline_maths.iter().any(|m| m.ready.is_some()));
        assert!(
            ready_row.is_some(),
            "行内公式就绪后帧应挂 Ready 位图（定向作废把旧 Loading 行换掉了）"
        );
        block_key
    });
    // `$$` 卡就绪：Pass2 blocked 不入缓存 → 无行可作废，但卡高必须按真位图重建。
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, ecx| {
            e.math_slots().borrow_mut().insert(
                block_key.as_str().into(),
                super::super::MathSlot::Ready(img.clone(), 30., 40.),
            );
            e.on_math_ready(&block_key, ecx);
        });
        let f3 = build_frame(ed.read(cx), window, px(600.), (0., 9999.));
        let card_h = f3
            .rows
            .iter()
            .find(|r| r.extra.card.is_some())
            .map(|r| f32::from(r.height))
            .unwrap_or(0.);
        let resh = ed.read(cx).take_reshapes();
        assert!(
            card_h > 42. + 1.,
            "$$ 卡就绪高应按真位图（>42px 回退）：{card_h}"
        );
        assert!(
            resh <= 3,
            "$$ 卡就绪不应整带重排（Pass2 blocked 行不缓存）：resh={resh}"
        );
    });
}

#[gpui::test]
fn inline_math_grown_row_caret_and_click_stay_aligned(cx: &mut TestAppContext) {
    // 用户实测：行内公式把行撑高后，点击该行「闪烁光标位置不对」——光标几何与
    // 点击映射必须都按撑高下移量（text_row_shift）定位，否则光标落在块顶空白处。
    use gpui::App;
    cx.update(crate::actions::bind_local_keymaps);
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
        let off = super::source_offset_at_local(ed.read(cx), &frame, point(px(x), px(y)));
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

