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
        tall <= 19.0 * config::math::INLINE_MATH_BAND + 1e-3,
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
    cx.update(crate::test_support::bind_local_keymaps);
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
fn math_block_single_click_keeps_card_double_click_opens_edit(cx: &mut TestAppContext) {
    // A1/B2 交互语义（2026-10 改造）：单击数学块卡片**不改光标**（不 reveal 回源码
    // 字符串）；**双击**弹出**数学编辑卡**——预填公式正文（无 `$$` 定界符）、光标
    // **不进**块（不 reveal）、块保持卡片折叠；不再像旧实现那样原地 set_caret reveal。
    use super::BlockCard;
    use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
    cx.update(crate::test_support::bind_local_keymaps);
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
        // 双击：弹出数学编辑卡（预填正文、无定界符）；光标不进块；块仍卡片（未 reveal）。
        let ev2 = MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(x), px(y)),
            modifiers: Modifiers::default(),
            click_count: 2,
            first_mouse: false,
        };
        ed.update(cx, |e, cx| e.on_mouse_down(&ev2, window, cx));
        assert!(ed.read(cx).math_edit_open(), "双击应打开数学编辑卡");
        assert_eq!(ed.read(cx).math_edit_src_line(), Some(0), "卡锚定到块首源码行");
        let prefill = ed
            .read(cx)
            .math_edit_input()
            .map(|i| i.read(cx).text_string());
        assert_eq!(
            prefill.as_deref(),
            Some(r"\int_0^1 x^2 dx"),
            "输入框预填公式正文（不含 $$ 定界符）"
        );
        assert_eq!(
            ed.read(cx).caret(),
            text.find("尾").unwrap(),
            "双击打开编辑卡不得把光标移进块（否则 reveal）"
        );
        let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        assert!(
            f2.rows
                .iter()
                .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
            "打开编辑卡后块仍卡片折叠（未 reveal 成源码）"
        );
    });
}

#[gpui::test]
fn math_block_single_line_double_click_opens_edit(cx: &mut TestAppContext) {
    // 单行 `$$…$$` 数学块（`$$\sum…$$` 独占一行）：双击必须**直接**弹出数学编辑卡、
    // 预填两个 `$` 之间的内层 LaTeX（无定界符）；光标不进块、块仍卡片折叠（不再像旧的
    // 原地 reveal——旧语义是「直接还原成源码字符串，不得先变成行内小公式再双击一次」，
    // 新交互下由编辑卡承担编辑入口）。
    use super::BlockCard;
    use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
    cx.update(crate::test_support::bind_local_keymaps);
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
        // 双击一次：弹出编辑卡、输入框预填内层 LaTeX、光标不进块、卡片仍在。
        assert!(ed.read(cx).math_edit_open(), "单行块双击应打开编辑卡");
        let prefill = ed
            .read(cx)
            .math_edit_input()
            .map(|i| i.read(cx).text_string());
        assert_eq!(
            prefill.as_deref(),
            Some(r"\sum_{i=1}^{n} i = \frac{n(n+1)}{2}"),
            "单行块预填两个 `$` 之间的内层（无定界符）"
        );
        assert_eq!(
            ed.read(cx).caret(),
            text.find("尾").unwrap(),
            "双击不得把光标移进块"
        );
        assert!(
            ed.read(cx)
                .row_col(ed.read(cx).caret())
                .0
                > 0,
            "光标不在首行（块内）"
        );
        let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        assert!(
            f2.rows
                .iter()
                .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
            "单行块双击后仍卡片（未 reveal 成字符串）"
        );
    });
}

#[gpui::test]
fn math_edit_card_live_write_back_updates_doc_and_rerenders(cx: &mut TestAppContext) {
    // 编辑卡每键 `Edited` 即时写回（撤销栈入口 `apply_edit`）：对多行输入区域做**真实
    // CodeEditor 编辑**（全选替换 = `set_selection_range` + `insert_text`，走同一编辑主链
    // 并发出 `EditorEvent::Edited`）→ 文档正文被替换（保留 `$$` 定界符与行数）、区域
    // `source` 更新、重建帧仍以卡片渲染新公式，且**光标不进块**。多行 `$$\n…\n$$`
    // 与单行 `$$…$$` 两种形态都验；连续两次写入各自成独立撤销单元（整段替换意图 =
    // Atomic、永不合并）。
    // ⚠️ GPUI 的 `emit` 是**延迟副作用**（`App::update` 结束时才冲刷）→ 编辑与断言必须
    // 分属两个 `vcx.update` 块，否则订阅写回尚未发生。
    use super::BlockCard;
    use gpui::{App, Bounds, size};
    cx.update(crate::test_support::bind_local_keymaps);
    // 全选替换输入区域内容（CodeEditor 真实编辑入口）：清空选区起点 + 全选 + 键入替换，
    // 触发 `EditorEvent::Edited` → 订阅写回（与旧 `InputState::set_value` 等价语义）。
    let replace_all = |input: &gpui::Entity<editor::CodeEditor>,
                       cx: &mut App,
                       text: &str| {
        input.update(cx, |ed, cx| {
            let len = ed.text_string().len();
            ed.set_caret_to(0);
            ed.set_selection_range(0..len, cx);
            ed.insert_text(text, cx);
        });
    };
    // —— 多行 `$$\n…\n$$` ——
    let text = "$$\n\\int_0^1 x^2 dx\n$$\n尾行\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let tail = text.find("尾").unwrap();
    let ed = vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(600.)),
            )));
            e.set_caret(tail); // 光标在块外（末行）
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        ed.update(cx, |e, cx| e.open_math_edit(0, window, cx));
        assert!(ed.read(cx).math_edit_open(), "应打开编辑卡");
        let input = ed.read(cx).math_edit_input().expect("应有输入区");
        // 第 1 次写回：输入区改为新正文 → 订阅在 update 结束冲刷时生效。
        replace_all(&input, cx, "x + y");
        ed
    });
    vcx.update(|window, cx| {
        assert_eq!(
            ed.read(cx).content(),
            "$$\nx + y\n$$\n尾行\n",
            "应保留 `$$` 定界符与三行块结构"
        );
        assert_eq!(
            ed.read(cx).derived().borrow().math_regions[0].source,
            "x + y",
            "区域 source 应即时更新"
        );
        assert_eq!(ed.read(cx).caret(), tail, "写回不得把光标拖进块内");
        let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        assert!(
            f2.rows
                .iter()
                .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
            "写回后重建帧仍以卡片渲染新公式（未 reveal）"
        );
        // 第 2 次写回：上一单元已固化入栈（Atomic 不合并 → 每键一条 undo）。
        let input = ed.read(cx).math_edit_input().expect("应有输入区");
        replace_all(&input, cx, "x + y + z");
    });
    vcx.update(|_window, cx| {
        assert_eq!(
            ed.read(cx).content(),
            "$$\nx + y + z\n$$\n尾行\n",
            "第 2 次写回继续保留定界符与行数"
        );
        assert_eq!(
            ed.read(cx).undo_stack.len() + (ed.read(cx).open_unit.is_some() as usize),
            2,
            "两次写回应各自构成独立撤销单元"
        );
        assert_eq!(ed.read(cx).caret(), tail, "第 2 次写回仍不拖光标");
    });
    // —— 单行 `$$a+b$$` ——
    let text1 = "$$a+b$$\n尾行\n";
    let (host1, vcx1) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text1));
        WyHost { ed }
    });
    let vcx1: &mut VisualTestContext = vcx1;
    let tail1 = text1.find("尾").unwrap();
    let ed1 = vcx1.update(|window, cx| {
        let ed = host1.read(cx).ed.clone();
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(600.)),
            )));
            e.set_caret(tail1);
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        ed.update(cx, |e, cx| e.open_math_edit(0, window, cx));
        let input = ed.read(cx).math_edit_input().expect("应有输入区");
        replace_all(&input, cx, "c^2");
        ed
    });
    vcx1.update(|window, cx| {
        assert_eq!(
            ed1.read(cx).content(),
            "$$c^2$$\n尾行\n",
            "单行块只替换 `$` 之间的内层，定界符保留"
        );
        assert_eq!(
            ed1.read(cx).derived().borrow().math_regions[0].source,
            "c^2"
        );
        assert_eq!(ed1.read(cx).caret(), tail1, "单行块写回也不拖光标");
        let f3 = build_frame(ed1.read(cx), window, px(600.), (0., 999_999.));
        assert!(
            f3.rows
                .iter()
                .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
            "单行块写回后仍卡片渲染"
        );
    });
}

#[gpui::test]
fn math_edit_card_click_outside_closes(cx: &mut TestAppContext) {
    // 点**卡片外**任意处 → 关闭数学编辑卡（写回已实时生效，无取消语义）。
    // 本测试沿直接 `on_mouse_down` 路径（点卡外空白 → 普通选区路径内关闭）；
    // 卡片浮层 `on_mouse_down_out` 在生产树里兜底同一语义。
    use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
    cx.update(crate::test_support::bind_local_keymaps);
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
            e.set_caret(text.find("尾").unwrap());
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        ed.update(cx, |e, cx| e.open_math_edit(0, window, cx));
        assert!(ed.read(cx).math_edit_open(), "应先打开编辑卡");
        assert!(
            ed.read(cx).math_edit_anchor().is_some(),
            "打开时应按当前帧算出窗口锚点"
        );
        // 卡片外的坐标：卡片**保守包围盒**右下之外（卡片加大后尾行可能被浮层覆盖，
        // 故取包围盒外侧一点）——数学卡在顶部，其下与右侧皆空。
        let box_b = ed.read(cx).math_edit_card_bounds().expect("卡包围盒");
        let (x, y) = (
            f32::from(box_b.left()) + 40.,
            f32::from(box_b.bottom()) + 40.,
        );
        let down = MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(x), px(y)),
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        ed.update(cx, |e, cx| e.on_mouse_down(&down, window, cx));
        assert!(
            !ed.read(cx).math_edit_open(),
            "点卡片外应关闭编辑卡"
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
    cx.update(crate::test_support::bind_local_keymaps);
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
    use config::math;
    use crate::markdown::parse::inline_style;
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
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
    cx.update(crate::test_support::bind_local_keymaps);
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
    cx.update(crate::test_support::bind_local_keymaps);
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
    cx.update(crate::test_support::bind_local_keymaps);
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

#[gpui::test]
fn math_block_ro_double_click_keeps_card(cx: &mut TestAppContext) {
    // 只读模式（预览/EPUB）是 **WYSIWYG 只读形态**：双击块级数学卡 = **不做任何事**——
    // 不弹编辑卡（写回依赖可编辑）、光标不进块、块不 reveal 回原始 LaTeX，公式始终
    // 保持渲染后的卡片；单击同样不动。多行 `$$\n…\n$$` 与单行 `$$…$$` 都验。
    // 可编辑口径见 `math_block_single_click_keeps_card_double_click_opens_edit`（弹卡、
    // 预填、光标不进块）。
    use super::BlockCard;
    use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
    cx.update(crate::test_support::bind_local_keymaps);
    // —— 多行 `$$\n…\n$$` ——
    let text = "$$\n\\int_0^1 x^2 dx\n$$\n尾行\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        ed.update(cx, |e, _| e.set_editable(false));
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
            e.set_caret(text.find("尾").unwrap());
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let x = f32::from(frame.padding) + 40.;
        let y = {
            let card = frame
                .rows
                .iter()
                .find(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
                .expect("只读帧应有数学卡片");
            f32::from(card.top) + f32::from(card.height) / 2.
        };
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        let caret_before = ed.read(cx).caret();
        let one = MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(x), px(y)),
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        ed.update(cx, |e, cx| e.on_mouse_down(&one, window, cx));
        assert_eq!(
            ed.read(cx).caret(),
            caret_before,
            "只读单击块卡不得移动光标"
        );
        let dbl = MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(x), px(y)),
            modifiers: Modifiers::default(),
            click_count: 2,
            first_mouse: false,
        };
        ed.update(cx, |e, cx| e.on_mouse_down(&dbl, window, cx));
        assert!(
            !ed.read(cx).math_edit_open(),
            "只读模式双击不得弹编辑卡（写回依赖可编辑）"
        );
        assert_eq!(
            ed.read(cx).caret(),
            caret_before,
            "只读双击光标不动（不进块）"
        );
        assert_ne!(
            ed.read(cx).row_col(ed.read(cx).caret()).0,
            0,
            "光标不在块首行（未 reveal 进块）"
        );
        let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        assert!(
            f2.rows
                .iter()
                .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
            "只读双击后块仍保持渲染卡（未 reveal 成源码）"
        );
    });
    // —— 单行 `$$…$$` ——
    let text1 = "$$\\sum_{i=1}^{n} i$$\n尾行\n";
    let (host1, vcx1) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text1));
        ed.update(cx, |e, _| e.set_editable(false));
        WyHost { ed }
    });
    let vcx1: &mut VisualTestContext = vcx1;
    vcx1.update(|window, cx| {
        let ed1 = host1.read(cx).ed.clone();
        ed1.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(600.)),
            )));
            e.set_caret(text1.find("尾").unwrap());
        });
        let frame = build_frame(ed1.read(cx), window, px(600.), (0., 999_999.));
        let x = f32::from(frame.padding) + 40.;
        let y = {
            let card = frame
                .rows
                .iter()
                .find(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
                .expect("单行只读帧应有数学卡片");
            f32::from(card.top) + f32::from(card.height) / 2.
        };
        ed1.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        let caret_before = ed1.read(cx).caret();
        let dbl = MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(x), px(y)),
            modifiers: Modifiers::default(),
            click_count: 2,
            first_mouse: false,
        };
        ed1.update(cx, |e, cx| e.on_mouse_down(&dbl, window, cx));
        assert!(
            !ed1.read(cx).math_edit_open(),
            "只读单行双击不得弹编辑卡"
        );
        assert_eq!(
            ed1.read(cx).caret(),
            caret_before,
            "只读单行双击光标不动（不进块）"
        );
        let f2 = build_frame(ed1.read(cx), window, px(600.), (0., 999_999.));
        assert!(
            f2.rows
                .iter()
                .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
            "只读单行双击后块仍保持渲染卡（未 reveal 成源码）"
        );
    });
}

#[gpui::test]
fn math_inline_ro_double_click_no_reveal(cx: &mut TestAppContext) {
    // 只读模式（预览/EPUB）是 **WYSIWYG 只读形态**：双击行内 `$…$` 公式 spacer **不
    // reveal**——光标不进公式源区间（shape 按「caret 严格落在 `$…$` 内」reveal 回
    // 原始 LaTeX），重建帧该行公式仍以 spliced 渲染呈现。可编辑口径见
    // `inline_math_double_click_enters_edit`（双击光标进公式内、reveal 可编辑）。
    use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "前置文字 $x^2+\\alpha$ 后置文字\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        ed.update(cx, |e, _| e.set_editable(false));
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
            .expect("只读帧应有行内公式 splice");
        let im = para.extra.inline_maths[0].clone();
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
            !(caret > im.source.start && caret < im.source.end),
            "只读双击不得把光标放进公式源区间内（否则 reveal）：caret={caret}，公式={:?}",
            im.source
        );
        // reveal 判定与 shape 一致：重建帧该行公式仍 splice（未 reveal 成源码）。
        let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let para2 = frame2
            .rows
            .iter()
            .find(|r| r.src_lines.contains(&src_line))
            .unwrap();
        assert!(
            !para2.extra.inline_maths.is_empty(),
            "只读双击后行内公式应仍 splice（不 reveal 回原始 LaTeX）"
        );
    });
}

#[gpui::test]
fn math_edit_card_switch_block_closes_old_and_opens_new(cx: &mut TestAppContext) {
    // 双击**另一块**数学卡 → 先关旧卡（旧输入框/订阅 drop）再开新卡（新预填、锚行
    // 换成新块）；两个块各自独立。开卡期间单击另一块不管卡（走数学卡早退）。
    use super::BlockCard;
    use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "$$\na + b\n$$\n\n$$\nx^2\n$$\n尾行\n";
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
            e.set_caret(text.find("尾").unwrap());
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let cards: Vec<(usize, f32)> = frame
            .rows
            .iter()
            .filter(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. })))
            .map(|r| (r.src_line, f32::from(r.top) + f32::from(r.height) / 2.))
            .collect();
        assert_eq!(cards.len(), 2, "应有 2 块数学卡");
        let pad = f32::from(frame.padding);
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        let x = pad + 40.;
        let down = |line: usize, clicks: usize| MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(x), px(cards.iter().find(|c| c.0 == line).unwrap().1)),
            modifiers: Modifiers::default(),
            click_count: clicks,
            first_mouse: false,
        };
        // 双击第 1 块 → 开卡。
        ed.update(cx, |e, cx| e.on_mouse_down(&down(cards[0].0, 2), window, cx));
        assert!(ed.read(cx).math_edit_open(), "应先开第 1 块卡");
        assert_eq!(ed.read(cx).math_edit_src_line(), Some(cards[0].0));
        assert_eq!(
            ed.read(cx)
                .math_edit_input()
                .map(|i| i.read(cx).text_string())
                .as_deref(),
            Some("a + b"),
            "第 1 块预填 a + b"
        );
        // 双击第 2 块 → 先关旧卡、再开新卡（锚行换成第 2 块、预填换源）。
        ed.update(cx, |e, cx| e.on_mouse_down(&down(cards[1].0, 2), window, cx));
        assert!(ed.read(cx).math_edit_open(), "切换后仍应开着卡");
        assert_eq!(
            ed.read(cx).math_edit_src_line(),
            Some(cards[1].0),
            "卡应切换到位移后的第 2 块"
        );
        assert_eq!(
            ed.read(cx)
                .math_edit_input()
                .map(|i| i.read(cx).text_string())
                .as_deref(),
            Some("x^2"),
            "第 2 块预填 x^2"
        );
        // 重新双击第 1 块 → 再切回来。
        ed.update(cx, |e, cx| e.on_mouse_down(&down(cards[0].0, 2), window, cx));
        assert_eq!(ed.read(cx).math_edit_src_line(), Some(cards[0].0), "可再切回第 1 块");
        // 双击当前块（同一块）→ 关闭（切换语义）。
        ed.update(cx, |e, cx| e.on_mouse_down(&down(cards[0].0, 2), window, cx));
        assert!(!ed.read(cx).math_edit_open(), "双击同块应关闭卡");
    });
}

#[gpui::test]
fn math_edit_card_title_drag_moves_anchor_and_keeps_after_paint(cx: &mut TestAppContext) {
    // 标题栏（**拖拽把手**）真实渲染路径：绘制出卡片 → 标题栏按下（抓取点=标题栏内、
    // 基准锚点=当前缓存锚点）→ 窗口级 `on_window_mouse_move`(+Δx,+Δy) → 缓存锚点 =
    // 原锚点 + Δ → paint 期 `math_edit_anchor_at` 刷新后偏移仍保持（不被每帧 paint 冲掉）
    // → 松开结束，随后纯移动不再改锚点；期间卡片未关、文档光标未动。
    use gpui::{Bounds, Modifiers, MouseButton, size};
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "$$\n\\int_0^1 x^2 dx\n$$\n尾行\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let tail = text.find("尾").unwrap();
    vcx.update(|window, cx| {
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(600.)),
            )));
            e.set_caret(tail);
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        ed.update(cx, |e, cx| e.open_math_edit(0, window, cx));
        assert!(ed.read(cx).math_edit_open(), "应打开编辑卡");
        // 绘制一帧：元素树 + deferred 卡片 + 窗口级 mouse 监听（paint 期注册）就位。
        let _ = window.draw(cx);
    });
    let anchor0 = ed
        .read_with(vcx, |e, _| e.math_edit_anchor())
        .expect("打开即缓存锚点");
    // 标题栏中心（真实拖拽把手）按下。
    let title_b = vcx
        .debug_bounds("wy-math-edit-title")
        .expect("标题栏已绘制");
    let title = point(
        title_b.origin.x + title_b.size.width / 2.,
        title_b.origin.y + title_b.size.height / 2.,
    );
    vcx.simulate_mouse_down(title, MouseButton::Left, Modifiers::default());
    assert!(
        ed.read_with(vcx, |e, _| e.math_edit_dragging()),
        "按下标题栏应进入拖拽"
    );
    // 窗口级 mouse_move(+Δx,+Δy) → 锚点 = 原锚点 + Δ。
    let (dx, dy) = (40.0f32, 30.0f32);
    vcx.simulate_mouse_move(
        point(px(f32::from(title.x) + dx), px(f32::from(title.y) + dy)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    let anchor1 = ed
        .read_with(vcx, |e, _| e.math_edit_anchor())
        .expect("拖拽后锚点");
    assert!(
        (f32::from(anchor1.x) - (f32::from(anchor0.x) + dx)).abs() < 1.
            && (f32::from(anchor1.y) - (f32::from(anchor0.y) + dy)).abs() < 1.,
        "拖拽后锚点 = 原锚点 + Δ：({:.1},{:.1})→({:.1},{:.1})，期望 (+{dx},+{dy})",
        f32::from(anchor0.x),
        f32::from(anchor0.y),
        f32::from(anchor1.x),
        f32::from(anchor1.y)
    );
    assert!((f32::from(ed.read_with(vcx, |e, _| e.math_edit_delta().x)) - dx).abs() < 1.,
        "累计偏移随锚点同步更新");
    assert!(ed.read_with(vcx, |e, _| e.math_edit_open()), "拖拽期间卡片不关");
    assert_eq!(
        ed.read_with(vcx, |e, _| e.caret()),
        tail,
        "拖拽不得移动文档光标"
    );
    // paint 期刷新锚点：以**当前帧 + 元素原点**（与 paint 同代配对）重算 `anchor_at`（含
    // delta 叠加）→ 拖拽偏移仍保持，不被每帧 paint 冲掉。
    vcx.update(|_window, cx| {
        let origin = ed.read(cx).content_origin();
        let f = ed.read(cx).frame.borrow().clone();
        ed.update(cx, |e, _| {
            let base = f
                .as_ref()
                .and_then(|f| e.math_edit_anchor_at(origin, f))
                .expect("锚点");
            e.set_math_edit_anchor(Some(base));
        });
    });
    let anchor2 = ed
        .read_with(vcx, |e, _| e.math_edit_anchor())
        .expect("paint 后锚点");
    assert!(
        (f32::from(anchor2.x) - f32::from(anchor1.x)).abs() < 1.
            && (f32::from(anchor2.y) - f32::from(anchor1.y)).abs() < 1.,
        "paint 刷新后拖拽偏移仍保持：({:.1},{:.1})（应与拖拽终点 ({:.1},{:.1}) 一致）",
        f32::from(anchor2.x),
        f32::from(anchor2.y),
        f32::from(anchor1.x),
        f32::from(anchor1.y)
    );
    // 松开结束；随后纯移动不再改锚点；卡片仍开。
    vcx.simulate_mouse_up(
        point(px(f32::from(title.x) + dx), px(f32::from(title.y) + dy)),
        MouseButton::Left,
        Modifiers::default(),
    );
    assert!(
        !ed.read_with(vcx, |e, _| e.math_edit_dragging()),
        "松开结束拖拽"
    );
    let before_more = ed.read_with(vcx, |e, _| e.math_edit_anchor());
    vcx.simulate_mouse_move(
        point(px(f32::from(title.x) + dx + 20.), px(f32::from(title.y) + dy + 20.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    assert_eq!(
        ed.read_with(vcx, |e, _| e.math_edit_anchor()),
        before_more,
        "松开后移动不再改锚点"
    );
    assert!(
        ed.read_with(vcx, |e, _| e.math_edit_open()),
        "拖拽结束卡片仍开"
    );
    // 关闭后重新打开：拖拽偏移重置（delta=0，卡片回到文档几何位）。
    vcx.update(|window, cx| {
        ed.update(cx, |e, cx| e.close_math_edit(cx));
        ed.update(cx, |e, cx| e.open_math_edit(0, window, cx));
    });
    assert!(
        f32::from(ed.read_with(vcx, |e, _| e.math_edit_delta().x)).abs() < 0.5,
        "重开后拖拽偏移重置"
    );
    // 需求 ①：卡片加大（宽 ≥520、保守包围盒高 ≥200、编辑器区高 ≥160）。
    assert!(super::MATH_EDIT_CARD_W >= 520., "卡片加宽 ≥520");
    assert!(super::MATH_EDIT_CARD_H >= 200., "卡片保守包围盒高 ≥200");
    assert!(super::MATH_EDIT_EDITOR_H >= 160., "编辑器区固定高 ≥160");
}

#[gpui::test]
fn math_edit_multiline_writeback_grows_body_lines(cx: &mut TestAppContext) {
    // 多行块：编辑器内**真实插入 `\n`**（CodeEditor 编辑主链 → `Edited` → 订阅写回）
    // → 文档正文行数 +1、`$$` 围栏形态保留、derived region 平移后仍命中、块仍卡片折叠；
    // 每键写回 = 独立撤销单元（含 `\n` → Atomic 不合并）。
    use super::BlockCard;
    use gpui::{Bounds, size};
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "$$\na + b\n$$\n尾行\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let tail = text.find("尾").unwrap();
    let ed = vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(600.)),
            )));
            e.set_caret(tail);
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        ed.update(cx, |e, cx| e.open_math_edit(0, window, cx));
        let input = ed.read(cx).math_edit_input().expect("应有输入区");
        // 光标移到输入区末尾 → 真实插入 `\n`（编辑主链发 `Edited`）。
        input.update(cx, |ed, cx| {
            let at = ed.text_string().len();
            ed.set_caret_to(at);
            ed.insert_text("\n", cx);
        });
        ed
    });
    vcx.update(|_window, cx| {
        assert_eq!(
            ed.read(cx).content(),
            "$$\na + b\n\n$$\n尾行\n",
            "插入换行后正文行数 +1、围栏形态保留"
        );
        assert_eq!(
            ed.read(cx).derived().borrow().math_regions[0].source,
            "a + b\n",
            "derived region source 随插入更新（仍含换行）"
        );
        {
            let drc = ed.read(cx).derived();
            let dr = drc.borrow();
            assert_eq!(dr.math_regions.len(), 1, "region 平移后仍在");
            assert_eq!(dr.math_regions[0].range.start, 0, "块首行仍命中");
        }
        {
            let drc = ed.read(cx).derived();
            let dr = drc.borrow();
            let blk_range = dr.math_regions[0].range.clone();
            let caret_line = ed.read(cx).row_col(ed.read(cx).caret()).0;
            assert!(
                !(blk_range.start <= caret_line && caret_line < blk_range.end),
                "多行写回不得把光标拖进块（光标行 {caret_line} 在块 {blk_range:?} 内）"
            );
        }
        // 第 2 键：再插入一行字符 → 写回仍在同一块内、独立撤销单元。
        let input = ed.read(cx).math_edit_input().expect("应有输入区");
        input.update(cx, |ed, cx| {
            let at = ed.text_string().len();
            ed.set_caret_to(at);
            ed.insert_text("c", cx);
        });
    });
    vcx.update(|window, cx| {
        assert_eq!(
            ed.read(cx).content(),
            "$$\na + b\nc\n$$\n尾行\n",
            "第二键写回继续保留围栏与两行正文"
        );
        assert_eq!(
            ed.read(cx).derived().borrow().math_regions[0].source,
            "a + b\nc",
            "region source 再次更新"
        );
        assert_eq!(
            ed.read(cx).undo_stack.len() + (ed.read(cx).open_unit.is_some() as usize),
            2,
            "每次换行写回各自构成独立撤销单元"
        );
        let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        assert!(
            f2.rows
                .iter()
                .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
            "写回后重建帧仍以卡片渲染新公式（未 reveal）"
        );
    });
}

#[gpui::test]
fn math_edit_single_line_folds_newlines_to_one_line(cx: &mut TestAppContext) {
    // 单行 `$$body$$`：编辑器内贴含 `\n` 文本（全选替换 = 真实 CodeEditor 编辑）→ 写回后
    // 正文被折为一行（`\n`→空格），块仍是单行 `$$…$$` 形态（保持单行合法、不破坏围栏）。
    use super::BlockCard;
    use gpui::{Bounds, size};
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "$$a+b$$\n尾行\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let tail = text.find("尾").unwrap();
    let ed = vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(600.)),
            )));
            e.set_caret(tail);
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        ed.update(cx, |e, cx| e.open_math_edit(0, window, cx));
        let input = ed.read(cx).math_edit_input().expect("应有输入区");
        // 全选替换为含 `\n` 的文本（模拟粘贴多行后落笔）。
        input.update(cx, |ed, cx| {
            let len = ed.text_string().len();
            ed.set_caret_to(0);
            ed.set_selection_range(0..len, cx);
            ed.insert_text("x\ny", cx);
        });
        ed
    });
    vcx.update(|window, cx| {
        assert_eq!(
            ed.read(cx).content(),
            "$$x y$$\n尾行\n",
            "含换行写回折为一行（`\\n`→空格），定界符保留"
        );
        assert_eq!(
            ed.read(cx).derived().borrow().math_regions[0].source,
            "x y",
            "单行块 source 为折行后文本"
        );
        assert_eq!(ed.read(cx).caret(), tail, "单行折行写回不拖光标");
        let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        assert!(
            f2.rows
                .iter()
                .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
            "折行后仍单行卡片（行数不变）"
        );
    });
}

#[gpui::test]
fn math_edit_long_formula_editor_bounded_and_roundtrips(cx: &mut TestAppContext) {
    // 长公式：超长单行正文 → 打开编辑卡并**真实绘制一帧**（内嵌 CodeEditor 渲染）不
    // panic；编辑器区有界（不溢出卡片外，固定编辑器高）；预填/写回转读一致。
    use super::{BlockCard, MATH_EDIT_CARD_W, MATH_EDIT_EDITOR_H};
    use gpui::{Bounds, size};
    cx.update(crate::test_support::bind_local_keymaps);
    // 单行超长 LaTeX（长行触发 CodeEditor 横向滚动，不参与布局溢出）。
    let long = r"\frac{\partial f}{\partial x} + \int_{0}^{\infty} e^{-x^2} dx ".repeat(24);
    let text = format!("$$\n{long}\n$$\n尾行\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let tail = text.find("尾").unwrap();
    vcx.update(|window, cx| {
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(600.)),
            )));
            e.set_caret(tail);
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        ed.update(cx, |e, cx| e.open_math_edit(0, window, cx));
        assert!(ed.read(cx).math_edit_open(), "长公式应打开编辑卡");
        // 真实绘制一帧（内嵌 CodeEditor 渲染），不得 panic。
        let _ = window.draw(cx);
    });
    // 编辑器区有界（宽 ≤ 卡片宽；高 = 固定编辑器高）。
    let editor_b = vcx
        .debug_bounds("wy-math-edit-editor")
        .expect("编辑器区已绘制");
    let card_b = vcx
        .debug_bounds("wy-math-edit-card")
        .expect("卡片已绘制");
    assert!(
        f32::from(editor_b.size.width) <= f32::from(card_b.size.width) + 1.,
        "编辑器宽 {:.1} 不得溢出卡片宽 {:.1}",
        f32::from(editor_b.size.width),
        f32::from(card_b.size.width)
    );
    assert!(
        (f32::from(editor_b.size.height) - MATH_EDIT_EDITOR_H).abs() < 2.,
        "编辑器区固定高 {MATH_EDIT_EDITOR_H}：实际 {:.1}",
        f32::from(editor_b.size.height)
    );
    assert!(
        f32::from(editor_b.size.width) <= f32::from(MATH_EDIT_CARD_W) - 8.,
        "编辑器不溢出卡片内容宽"
    );
    // 预填回读一致。
    assert_eq!(
        ed.read_with(vcx, |e, _| e.math_edit_input())
            .expect("输入区")
            .read_with(vcx, |ed, _| ed.text_string()),
        long,
        "超长公式预填回读一致"
    );
    // 写回转读一致：追加一个真实编辑 → 文档正文随之更新、重建帧仍卡片。
    vcx.update(|_window, cx| {
        let input = ed.read(cx).math_edit_input().expect("输入区");
        input.update(cx, |ed, cx| {
            let at = ed.text_string().len();
            ed.set_caret_to(at);
            ed.insert_text(" + 1", cx);
        });
    });
    vcx.update(|window, cx| {
        assert_eq!(
            ed.read(cx).derived().borrow().math_regions[0].source,
            format!("{long} + 1"),
            "写回后 region source 更新（超长正文回读一致）"
        );
        let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        assert!(
            f2.rows
                .iter()
                .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
            "超长写回后重建帧仍卡片渲染"
        );
    });
}

#[gpui::test]
fn math_edit_long_formula_tail_reachable_by_hscroll(cx: &mut TestAppContext) {
    // 复现：超长单行公式正文在编辑卡内被裁掉尾部，看不到、也无法选中/点中末尾字符。
    // 根因（几何证据）：内嵌 CodeEditor 首帧 `avail_w` 读不到 `element_bounds`（浮层
    // `deferred` prepaint 在 render **之后**才回写）→ 回退到**窗口宽** → `content_w_px`、
    // `visible_chars` 全按窗口宽算（修复前实测 content_w=窗口宽、visible≈249），横向量程
    // 虚高、reveal/点按映射错乱，滚到底尾部被甩出可视区左侧，肉眼看不到也无法点中。
    // 修复：`open_math_edit` 首帧前 `set_element_bounds` 给出卡片内编辑器区的确定可视宽
    // （`MATH_EDIT_EDITOR_W`）→ 首帧 content_w=真实行宽、visible≈卡片宽，横向量程正确，
    // 滚到底尾部落在可视区内；再叠加**尾部 gap**（`MATH_EDIT_TAIL_GAP_PX`，只加成
    // content_w）→ 末尾字符右侧留出可见空隙、不再贴裁切缘（可看清/可点中）。
    use gpui::{
        Bounds, Modifiers, MouseButton, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px, size,
    };
    cx.update(crate::test_support::bind_local_keymaps);
    // 单行超长正文：160 个 ASCII 宽单位 ≈ 1248px —— 落在「卡片内编辑器可视宽(~447)」与
    // 「窗口宽(≈1930)」之间：修复前 max() 取到窗口宽（量程/可见字符错乱），修复后取到
    // 真实行宽（尾部可达）。
    let long = "x+".repeat(80);
    let units = long.len() as f32; // 全 ASCII → 每字符 1 宽度单位
    let text = format!("$$\n{long}\n$$\n尾行\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let tail = text.find("尾").unwrap();
    vcx.update(|window, cx| {
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(600.)),
            )));
            e.set_caret(tail);
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        ed.update(cx, |e, cx| e.open_math_edit(0, window, cx));
        assert!(ed.read(cx).math_edit_open(), "长公式应打开编辑卡");
        assert!(
            ed.read(cx).math_edit_anchor().is_some(),
            "打开即缓存锚点（浮层就位）"
        );
        // 真实绘制一帧（内嵌 CodeEditor 渲染 + deferred 卡片浮层布局）。
        let _ = window.draw(cx);
    });
    let editor_b = vcx
        .debug_bounds("wy-math-edit-editor")
        .expect("编辑器区已绘制");
    let input = ed.read_with(vcx, |e, _| e.math_edit_input()).expect("输入区");
    let (max_scroll, content_w, visible, (cw, _lh, gutter), eb) =
        input.read_with(vcx, |ed, _| {
            (
                ed.test_hscroll_max_px(),
                ed.test_content_w_px(),
                ed.test_visible_chars(),
                ed.test_metrics(),
                ed.test_element_bounds(),
            )
        });
    let editor_w = f32::from(eb.size.width);
    let harea = (editor_w - gutter).max(0.);
    let raw_w = units * cw;
    assert!(
        max_scroll > 8.,
        "长公式编辑卡应存在横向量程：content_w={content_w:.1} harea={harea:.1} max_scroll={max_scroll:.1}"
    );
    // 内容宽 = 真实文本行宽 + 尾部显示 gap（`MATH_EDIT_TAIL_GAP_PX`；修复前 max(行宽,
    // 窗口宽) → 虚高到窗口宽 FAIL；修复后不再回退窗口宽，只在真实行宽上追加尾部留白）。
    assert!(
        (content_w - (raw_w + super::MATH_EDIT_TAIL_GAP_PX)).abs() < 3.,
        "内容宽应 = 真实行宽 + 尾部 gap（不是窗口宽）：content_w={content_w:.1} raw+gap={:.1}",
        raw_w + super::MATH_EDIT_TAIL_GAP_PX
    );
    // 可视字符数 = 卡片内编辑器宽（修复前 = 窗口宽 → 虚高到 ~249，FAIL；修复后 = 卡片
    // 宽推导值，`visible_chars_from_width` 有 `.max(80)` 下限 → ≤ 80）。
    assert!(
        visible < 120,
        "可视字符数应按卡片内编辑器宽算（不是窗口宽）：visible={visible} editor_w={editor_w:.1} gutter={gutter:.1}"
    );
    // 内嵌编辑器**可点中**：首次鼠标按下（绘制后第一个鼠标事件，deferred 浮层命中路径
    // 已就绪）在文本区点一下 → 光标落在真实文本列内（>0 且 < 行宽；修复前若首帧几何按
    // 窗口宽，仍能点但坐标系/可视范围错乱，这里是可点性兜底）。
    let first_click = point(
        editor_b.origin.x + px(160.),
        editor_b.origin.y + editor_b.size.height / 2.,
    );
    vcx.simulate_mouse_down(first_click, MouseButton::Left, Modifiers::default());
    let caret0 = input.read_with(vcx, |ed, _| ed.selection().head());
    assert!(
        caret0 > 0 && caret0 < units as usize,
        "点文本区应把光标放进真实文本列：caret0={caret0}（行宽 {units:.0}）"
    );
    // 横向滚轮滚到最右（量程上限）→ 末尾字符应落在可视区右缘内（修复前量程按窗口宽
    // 虚高 → 滚过头、末尾字符被甩出可视区左侧，看不到也无法点中，FAIL）。
    // ⚠️ 用**点击后最新一帧**的编辑器几何（卡片/卡行在重绘间会移动，防止滚到旧位置）。
    let editor_b = vcx
        .debug_bounds("wy-math-edit-editor")
        .expect("点击后编辑器区仍在");
    let wheel_center = point(
        editor_b.origin.x + editor_b.size.width / 2.,
        editor_b.origin.y + editor_b.size.height / 2.,
    );
    vcx.simulate_event(ScrollWheelEvent {
        position: wheel_center,
        delta: ScrollDelta::Pixels(point(px(50_000.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    });
    let (hscroll, max2) = input.read_with(vcx, |ed, _| {
        (ed.test_hscroll_px(), ed.test_hscroll_max_px())
    });
    assert!(
        hscroll >= 8.,
        "横向滚轮应让内嵌编辑器真正横滚（尾部可达）：hscroll={hscroll:.1} max={max2:.1}"
    );
    let tail_on_screen = raw_w - hscroll;
    assert!(
        tail_on_screen >= -2. && tail_on_screen <= harea + 2.,
        "滚到底末尾字符应在可视区内（否则被 overflow_hidden 裁掉、看不到也点不中）：\
         tail_on_screen={tail_on_screen:.1}（可视 [0, {harea:.1}]）hscroll={hscroll:.1}",
    );
    // 尾部 gap（本次 workaround 的验收点）：滚到底后末尾字符应停在可视右缘**左侧**
    // ≈`MATH_EDIT_TAIL_GAP_PX` 处——末尾字符右侧留出可辨空隙，不再贴裁切缘（可看清、
    // 可点中；若 gap 未加成，tail_to_edge≈0 → FAIL）。
    let tail_to_edge = harea - tail_on_screen;
    assert!(
        tail_to_edge > 20. && tail_to_edge <= super::MATH_EDIT_TAIL_GAP_PX + 12.,
        "滚到底末尾字符右侧应留出尾部 gap（不贴缘）：tail_to_edge={tail_to_edge:.1}，目标 {}",
        super::MATH_EDIT_TAIL_GAP_PX
    );
}

#[gpui::test]
fn math_edit_long_formula_typing_follows_hscroll(cx: &mut TestAppContext) {
    // 复现（用户实测）：长公式末尾继续输入时，横向滚动条不跟随（停在原位），内容变长后
    // 刚输入的新内容在可视区外看不到。与主编辑器
    // `reveal_keeps_caret_visible_while_typing_in_long_line` 同语义：末尾敲字应让 hscroll
    // 持续推进、末字符落进可视区（真实渲染路径 + 连续键入）。
    use gpui::{Bounds, point, px, size};
    cx.update(crate::test_support::bind_local_keymaps);
    let long = "x+".repeat(80);
    let text = format!("$$\n{long}\n$$\n尾行\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let tail = text.find("尾").unwrap();
    vcx.update(|window, cx| {
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(600.)),
            )));
            e.set_caret(tail);
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        ed.update(cx, |e, cx| e.open_math_edit(0, window, cx));
        assert!(ed.read(cx).math_edit_open(), "应打开编辑卡");
        let _ = window.draw(cx);
    });
    let input = ed.read_with(vcx, |e, _| e.math_edit_input()).expect("输入区");
    // 首帧基线。
    let (cw0, visible0, h0, gutter0, eb_w0) = input.read_with(vcx, |ed, _| {
        let m = ed.test_metrics();
        (
            m.0,
            ed.test_visible_chars(),
            ed.test_hscroll_px(),
            m.2,
            ed.test_element_bounds().size.width.as_f32(),
        )
    });
    assert!(cw0 > 0., "首帧后 char_width 应已就绪：cw0={cw0}");
    assert!(
        visible0 < 120,
        "首帧 visible 应按卡片宽算（不是窗口宽）：visible0={visible0} eb_w0={eb_w0:.1}"
    );
    // 光标放正文末尾 → 连续键入 → 每轮统计 hscroll 推进与末字符屏幕位置。
    let harea = (eb_w0 - gutter0).max(0.);
    assert!(
        visible0 < 120 && harea > 0.,
        "首帧几何应就绪（visible0={visible0} harea={harea:.1}）"
    );
    // 光标放正文末尾 → 连续键入 → 每轮统计 hscroll 推进与末字符屏幕位置。
    let mut last_h = h0;
    let mut mono_ok = true;
    let mut worst_screen_right = f32::MIN;
    let mut log = String::new();
    let mut step = 0usize;
    for _ in 0..40 {
        step += 1;
        input.update(vcx, |ed, cx| {
            ed.set_caret_to(ed.text_string().len());
            ed.insert_text("y", cx);
        });
        // 键入即 reveal（同调用栈）；再真实绘制一帧（sync_render_state 回读 h_handle、
        // 更新 visible_chars、ScrollableElement 落 max_offset）。
        let h_after_edit = vcx.update(|_, cx| input.read(cx).test_hscroll_px());
        vcx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let (hscroll, max, cw, vis, cols) = vcx.update(|_, cx| {
            let ed = input.read(cx);
            (
                ed.test_hscroll_px(),
                ed.test_hscroll_max_px(),
                ed.test_metrics().0,
                ed.test_visible_chars(),
                ed.test_caret_units(),
            )
        });
        // 末字符（=光标前一格）左缘 = 列×字宽 − hscroll。
        let screen = (cols as f32 - 1.) * cw - hscroll;
        worst_screen_right = worst_screen_right.max(screen + cw);
        if hscroll <= last_h {
            mono_ok = false;
            log.push_str(&format!(
                "  [{step}] hscroll={hscroll:.1} <= last={last_h:.1}（max={max:.1} cw={cw:.2} \
                 vis={vis} cols={cols} h_after_edit={h_after_edit:.1}）\n",
            ));
        }
        last_h = hscroll;
    }
    assert!(
        mono_ok,
        "末尾连续键入时横向滚动应持续推进（停在原位则新内容溢出可视区）：h0={h0:.1}，\n{}",
        log
    );
    // 末字符右缘落进可视文本区（修复前 visible 被 80 格下限虚高 → reveal 右缘偏大 → 末字符
    // 在真实右缘之外、被 overflow_hidden 裁掉看不到，worst 可达 530px > harea 447）。
    assert!(
        worst_screen_right <= harea + 2.,
        "末尾键后的末字符右缘应落进可视文本区：worst_screen_right={worst_screen_right:.1} \
         harea={harea:.1}（h0={h0:.1}，最后一轮 hscroll={last_h:.1}）"
    );
}

#[gpui::test]
fn math_edit_empty_block_writeback_keeps_fences(cx: &mut TestAppContext) {
    // 空块 `$$\n$$`（无正文行）：双击弹卡预填空正文 → 键入正文 → 写回必须让正文独占
    // 一行、闭围栏独立成行（`$$\n<body>\n$$`）——若只插正文不换行会生成 `<body>$$`，
    // `$$` 不再是整行围栏，派生重扫会把闭围栏及以下整篇吞进块内（block 区间 0..4、
    // source 含尾行）。断言围栏形态保留、region 只含正文、光标仍在块外。
    use super::BlockCard;
    use gpui::{Bounds, size};
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "$$\n$$\n尾行\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let tail = text.find("尾").unwrap();
    let ed = vcx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(600.)),
            )));
            e.set_caret(tail);
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        ed.update(cx, |e, cx| e.open_math_edit(0, window, cx));
        assert!(ed.read(cx).math_edit_open(), "空块应打开编辑卡");
        let input = ed.read(cx).math_edit_input().expect("应有输入区");
        assert_eq!(input.read(cx).text_string(), "", "空块预填空正文");
        input.update(cx, |ed, cx| {
            ed.set_caret_to(0);
            ed.insert_text("a+b", cx);
        });
        ed
    });
    vcx.update(|window, cx| {
        assert_eq!(
            ed.read(cx).content(),
            "$$\na+b\n$$\n尾行\n",
            "空块写回：正文独占一行、闭围栏独立成行"
        );
        {
            let drc = ed.read(cx).derived();
            let d = drc.borrow();
            assert_eq!(d.math_regions.len(), 1, "仍只有 1 个数学块");
            assert_eq!(d.math_regions[0].range, 0usize..3, "区间只含开/闭围栏+正文");
            assert_eq!(d.math_regions[0].source, "a+b", "region 只含正文，不吞尾行");
        }
        {
            let blk = 0usize..3;
            let caret_line = ed.read(cx).row_col(ed.read(cx).caret()).0;
            assert!(
                !(blk.start <= caret_line && caret_line < blk.end),
                "空块写回不得把光标拖进块内（光标行 {caret_line} 在块 {blk:?} 内）"
            );
            assert_eq!(
                caret_line, 3,
                "光标应停在闭围栏后一行（尾行行首，随插入平移）"
            );
        }
        let f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        assert!(
            f2.rows
                .iter()
                .any(|r| matches!(&r.extra.card, Some(BlockCard::Math { .. }))),
            "空块写回后重建帧仍卡片渲染"
        );
    });
}

#[gpui::test]
fn math_edit_click_visible_row_does_not_jump_to_top(cx: &mut TestAppContext) {
    // 复现（用户实测）：编辑卡内换行/多行后，点击**第二个可视行** → 该行直接跳成第一行
    // （内容整体滚下一行）。根因疑点：`reveal_row` 的 `rows_visible` =
    // `floor((视口高 − 底部预留 pad)/行高)`，而数学编辑卡视口仅 `MATH_EDIT_EDITOR_H`(200)px、
    // 底部预留 pad ≈ 180px（16px 横向滚动条 + 8 行呼吸区量化）→ `rows_visible`=1，
    // 点击任意非首行都触发 `scroll_to_px` 把该行顶到顶部 = 「点哪行跳哪行到第一行」。
    use gpui::{Bounds, Modifiers, MouseButton, point, px, size};
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "$$\na + b\nc + d\n$$\n尾行\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let tail = text.find("尾").unwrap();
    vcx.update(|window, cx| {
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(600.)),
            )));
            e.set_caret(tail);
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        ed.update(cx, |e, cx| e.open_math_edit(0, window, cx));
        assert!(ed.read(cx).math_edit_open(), "应打开编辑卡");
        // 真实绘制一帧（deferred 浮层布局落定 → ListState 视口高回写）。
        let _ = window.draw(cx);
    });
    let input = ed.read_with(vcx, |e, _| e.math_edit_input()).expect("应有输入区");
    let (view_h, lh, pad) = input.read_with(vcx, |ed, _| {
        (
            ed.list_viewport_height(),
            ed.test_metrics().1,
            ed.test_bottom_pad_px(),
        )
    });
    assert!(view_h > 100., "卡片编辑器视口高应落定（≈200）：view_h={view_h}");
    let editor_b = vcx
        .debug_bounds("wy-math-edit-editor")
        .expect("编辑器区已绘制");
    let scroll0 = input.read_with(vcx, |ed, _| ed.test_text_scroll_top());
    // 点**第二个可视行**中部（y = 1.5×行高）→ 点击后滚动位置必须保持不变。
    let click = point(
        editor_b.origin.x + px(40.),
        editor_b.origin.y + px(lh * 1.5),
    );
    vcx.simulate_mouse_down(click, MouseButton::Left, Modifiers::default());
    let scroll1 = input.read_with(vcx, |ed, _| ed.test_text_scroll_top());
    assert_eq!(
        scroll1, scroll0,
        "点击第二可视行不得把该行滚到顶：前={scroll0:?} 后={scroll1:?} \
         （view_h={view_h:.1} lh={lh:.1} pad={pad:.1}，修复目标 rows_visible>1）"
    );
}

#[gpui::test]
fn math_edit_copy_paste_inside_card(cx: &mut TestAppContext) {
    // 复现（用户实测）：卡片内嵌编辑器的复制/粘贴无效。逐环节验证：程序化选中 →
    // Ctrl+C（剪贴板应有选中文本）→ 光标置首 + 写剪贴板 → Ctrl+V（内容插入光标处）。
    // 键位：`bind_input_keys` 在 "Input" 上下文绑 ctrl-c（CodeEditor 核心元素），
    // ctrl-v 由 `wysiwyg::bind_keys` 的 wy-editor 上下文兜底 → action 派发到聚焦的
    // 内嵌 CodeEditor（最深 handler + stop_propagation）。
    use gpui::{App, Bounds, Keystroke, point, px, size};
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "$$\na + b\n$$\n尾行\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let tail = text.find("尾").unwrap();
    vcx.update(|window, cx| {
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(600.), px(600.)),
            )));
            e.set_caret(tail);
        });
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        ed.update(cx, |e, cx| e.open_math_edit(0, window, cx));
        assert!(ed.read(cx).math_edit_open(), "应打开编辑卡");
        // 显式聚焦内嵌编辑器（真实用户点击输入区后的焦点态）。
        let input = ed.read(cx).math_edit_input().expect("应有输入区");
        let fh = input.read(cx).focus_handle();
        window.focus(&fh, cx);
        let _ = window.draw(cx);
    });
    let input = ed.read_with(vcx, |e, _| e.math_edit_input()).expect("应有输入区");
    let wh = vcx.windows().first().copied().expect("窗口句柄");
    // ① 选中正文 → Ctrl+C → 剪贴板应有选中文本。
    vcx.update(|_w, cx| {
        input.update(cx, |e, cx| {
            let len = e.text_string().len();
            e.set_selection_range(0..len, cx);
        });
    });
    vcx.dispatch_keystroke(wh, Keystroke::parse("ctrl-c").unwrap());
    let clip: Option<String> = vcx.update(|_w, cx: &mut App| {
        cx.read_from_clipboard()
            .map(|i| i.text().unwrap_or_default())
    });
    assert_eq!(
        clip.as_deref(),
        Some("a + b"),
        "Ctrl+C 应把选中文本写入剪贴板（卡片内嵌编辑器）"
    );
    // ② 光标放开头 → 写剪贴板 → Ctrl+V → 内容插入光标处。
    vcx.update(|_w, cx| {
        input.update(cx, |e, _| e.set_caret_to(0));
    });
    vcx.write_to_clipboard(gpui::ClipboardItem::new_string("XY".to_string()));
    vcx.dispatch_keystroke(wh, Keystroke::parse("ctrl-v").unwrap());
    let (after, doc) = vcx.update(|_w, cx: &mut App| {
        (input.read(cx).text_string(), ed.read(cx).content())
    });
    assert_eq!(
        after.as_str(),
        "XYa + b",
        "Ctrl+V 应把剪贴板内容粘贴进卡片内嵌编辑器（不能粘进文档正文）：after={after:?} doc={doc:?}"
    );
}

