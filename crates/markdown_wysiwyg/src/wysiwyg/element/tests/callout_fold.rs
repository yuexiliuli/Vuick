#[gpui::test]
fn callout_fold_click_path_round_trips(cx: &mut TestAppContext) {
    use super::{Frame, RowGutter};
    use gpui::{Bounds, Modifiers, MouseButton, MouseDownEvent, size};
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "> [!NOTE]- 甲\n> 乙\n> 丙\n尾";
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
        ed.update(cx, |e, _| {
            *e.rd.borrow_mut().frame.borrow_mut() = Some(std::rc::Rc::new(frame1))
        });
        let f1 = ed
            .read(cx).rd
            .borrow()
            .frame
            .borrow()
            .as_ref()
            .map(std::rc::Rc::clone)
            .unwrap();
        let ev1 = MouseDownEvent {
            button: MouseButton::Left,
            position: chevron_point(&f1),
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        ed.update(cx, |e, cx| e.on_mouse_down(&ev1, window, cx));
        assert!(
            ed.read(cx).text().as_ref().starts_with("> [!NOTE]+"),
            "点击应展开"
        );
        // 展开态 → 再点折叠回。
        let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        ed.update(cx, |e, _| {
            *e.rd.borrow_mut().frame.borrow_mut() = Some(std::rc::Rc::new(frame2))
        });
        let f2 = ed
            .read(cx).rd
            .borrow()
            .frame
            .borrow()
            .as_ref()
            .map(std::rc::Rc::clone)
            .unwrap();
        let ev2 = MouseDownEvent {
            button: MouseButton::Left,
            position: chevron_point(&f2),
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        ed.update(cx, |e, cx| e.on_mouse_down(&ev2, window, cx));
        assert!(
            ed.read(cx).text().as_ref().starts_with("> [!NOTE]-"),
            "再点应折叠回"
        );
        assert_eq!(ed.read(cx).text().as_ref(), text, "往返后内容还原");
        // 关键断言：折叠后 body 行必须真正折叠（0 高）——光标在 marker 行也不得被 reveal 弹开。
        // 折叠 body 0 高不物化，高度经布局 `block_height` 断言。
        let frame3 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let body_ix = frame3
            .block_index_for_src_line(1)
            .expect("callout 主体块应存在");
        assert_eq!(
            frame3.block_height(body_ix),
            0.,
            "点击折叠后 body 应真正折叠（回归）"
        );
    });
}

#[gpui::test]
fn two_callouts_fold_independently_with_caret_on_marker(cx: &mut TestAppContext) {
    // 回归：两个 callout 相邻（中间空行）。各自折叠/展开应互不影响；
    // 尤其「光标落在 marker 行（点击折叠后的常态）」时，该 callout 仍要真正折叠。
    cx.update(crate::test_support::bind_local_keymaps);
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
        assert_eq!(
            frame.block_height(note_ix),
            0.,
            "光标在 marker 行时 NOTE body 也应折叠"
        );
        assert_eq!(frame.block_height(warn_ix), 0., "WARNING body 也应折叠");
        // 展开 NOTE → 再折叠（光标仍在 NOTE marker 行）。
        ed.update(cx, |e, cx| e.toggle_alert_fold_at_line(0, cx));
        assert!(
            ed.read(cx).text().as_ref().starts_with("> [!NOTE]+"),
            "先展开"
        );
        ed.update(cx, |e, cx| e.toggle_alert_fold_at_line(0, cx));
        assert!(
            ed.read(cx).text().as_ref().starts_with("> [!NOTE]-"),
            "再折叠回"
        );
        let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let note2_ix = frame2.block_index_for_src_line(1).expect("NOTE 主体块");
        assert_eq!(
            frame2.block_height(note2_ix),
            0.,
            "折叠后 NOTE body 应真正折叠（不是 reveal）"
        );
        // 互不影响：折叠 WARNING 不应改动 NOTE。
        ed.update(cx, |e, cx| e.toggle_alert_fold_at_line(4, cx));
        assert!(
            ed.read(cx).text().as_ref().starts_with("> [!NOTE]-"),
            "NOTE 折叠态保持"
        );
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
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "## 甲\nbody 一\nbody 二\n## 乙\n尾";
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
        ed.update(cx, |e, _| {
            *e.rd.borrow_mut().frame.borrow_mut() = Some(std::rc::Rc::new(frame1))
        });
        let f1 = ed
            .read(cx).rd
            .borrow()
            .frame
            .borrow()
            .as_ref()
            .map(std::rc::Rc::clone)
            .unwrap();
        let ev1 = MouseDownEvent {
            button: MouseButton::Left,
            position: chevron_point(&f1),
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        ed.update(cx, |e, cx| e.on_mouse_down(&ev1, window, cx));
        assert!(
            ed.read(cx).rd.borrow().folded_headings.borrow().contains("## 甲"),
            "点击标题箭头应折叠"
        );
        let frame2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let body_ix = frame2.block_index_for_src_line(1).expect("body 块");
        assert_eq!(frame2.block_height(body_ix), 0., "折叠后 body 应真正折叠");
        let h2 = frame2
            .rows
            .iter()
            .find(|r| r.src_lines.contains(&0))
            .unwrap();
        assert!(f32::from(h2.height) > 0., "标题行本身保留");
        // 展开回：body 高度恢复，文本不变（视图本地，不写源码）。
        ed.update(cx, |e, _| {
            *e.rd.borrow_mut().frame.borrow_mut() = Some(std::rc::Rc::new(frame2))
        });
        let f2 = ed
            .read(cx).rd
            .borrow()
            .frame
            .borrow()
            .as_ref()
            .map(std::rc::Rc::clone)
            .unwrap();
        let ev2 = MouseDownEvent {
            button: MouseButton::Left,
            position: chevron_point(&f2),
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        ed.update(cx, |e, cx| e.on_mouse_down(&ev2, window, cx));
        assert!(
            !ed.read(cx).rd.borrow().folded_headings.borrow().contains("## 甲"),
            "再点应展开"
        );
        assert_eq!(ed.read(cx).text().as_ref(), text, "折叠/展开不写源码");
        let frame3 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let body3 = frame3
            .rows
            .iter()
            .find(|r| r.src_lines.contains(&1))
            .unwrap();
        assert!(f32::from(body3.height) > 0., "展开后 body 恢复高度");
    });
}

#[gpui::test]
fn heading_fold_stays_folded_with_caret_on_heading_line(cx: &mut TestAppContext) {
    // 回归（坑 P 同款）：点击折叠后光标停在标题行，该 section 不得被 reveal 弹开；
    // 光标严格落在 body 行内才 reveal。
    cx.update(crate::test_support::bind_local_keymaps);
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
        let body2 = frame2
            .rows
            .iter()
            .find(|r| r.src_lines.contains(&1))
            .unwrap();
        assert!(f32::from(body2.height) > 0., "光标在 body 行内应 reveal");
    });
}

#[gpui::test]
fn toggle_heading_fold_moves_caret_out_when_folding_from_body(cx: &mut TestAppContext) {
    // 折叠时若光标在 section body 内，先把光标移到标题行末（否则 body 被 reveal，
    // 折叠无效）——与 live 的 `move_to(end)` 同款。
    cx.update(crate::test_support::bind_local_keymaps);
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
            ed.read(cx).rd.borrow().folded_headings.borrow().contains("## Goals"),
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
    cx.update(crate::test_support::bind_local_keymaps);
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
            ed.read(cx).rd.borrow().folded_headings.borrow().contains("# 公式区"),
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
    use crate::markdown::ir::IrKind;
    use crate::markdown::parse::inline_style;
    use crate::markdown::parse::recognition::AlertKind;
    cx.update(crate::test_support::bind_local_keymaps);
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
    cx.update(crate::test_support::bind_local_keymaps);
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
            frame
                .rows
                .iter()
                .any(|r| matches!(r.extra.card, Some(BlockCard::Mermaid))),
            "应渲染出 mermaid 卡片"
        );
        ed.update(cx, |e, _| {
            *e.rd.borrow_mut().frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        // 触发一次真实绘制（measure→prepaint→paint），catch 任何渲染 panic。
        let _ = window.draw(cx);
    });
}

/// M5-B3-1 回归（1c）：**折叠了一个标题后，在别处（折叠体之外）按 Enter**——
/// 折叠 section 必须保持折叠（body 仍 0 高）、`fold_plan` 的折叠体行区间仍正确、
/// 新文本正确。钉住「folded_headings 非空 → 全篇 text_shared 物化/重扫」航线的落点。
#[gpui::test]
fn heading_fold_survives_structural_enter_elsewhere(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    // `# Tail`（level 1 ≤ `## App` 的 level 2）截断 section → 折叠体 = 行 0..2。
    let text = "## App\napp body\n# Tail\noutside line\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 光标默认在行首（标题行）→ 折叠保持。
        ed.update(cx, |e, cx| e.toggle_heading_fold_at_line(0, cx));
        assert!(ed.read(cx).rd.borrow().folded_headings.borrow().contains("## App"));
        let frame0 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let body0 = frame0.block_index_for_src_line(1).expect("body 块");
        assert_eq!(frame0.block_height(body0), 0., "折叠后 body 应 0 高");
        // 在折叠体之外（`# Tail` 之下的段落）按 Enter。
        let at = ed.read(cx).content().find("outside").unwrap() + 4;
        ed.update(cx, |e, ecx| e.replace_range(at..at, "\n", ecx));
        assert!(
            ed.read(cx).text().as_ref().contains("outs\nide"),
            "Enter 文本应正确"
        );
        let frame1 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let body1 = frame1.block_index_for_src_line(1).expect("body 块");
        assert_eq!(
            frame1.block_height(body1),
            0.,
            "别处 Enter 后 body 应仍 0 高"
        );
        // fold_plan 仍含该折叠体、行区间未漂移。
        let plan = ed.read(cx).fold_plan_rc().expect("折叠计划");
        assert_eq!(plan.sets.fold_bodies.len(), 1, "应只有 1 个折叠体");
        assert_eq!(
            plan.sets.fold_bodies[0], 0usize..2,
            "折叠体行区间仍为 0..2"
        );
        assert_eq!(plan.sets.fold_split, 0, "无 alert 折叠体 → split 应为 0");
    });
}

/// M5-B3-1 子项 4：折叠标题后**在别处（折叠体之外）按 Enter**，全篇 `text_shared()`
/// 不应再被触达——标题折叠体已移入 `derived` 行号平移，`fold_plan` 的 sets 重建不再
/// 物化全文 + `heading_fold_regions` 全扫（旧实现每键 text_shared 一次 O(n) 副本）。
#[gpui::test]
fn heading_fold_enter_elsewhere_no_full_text_materialization(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "## App\napp body\n# Tail\noutside line\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 折叠 `## App`（body 0..2）→ 首次建帧（真正物化折叠计划的时刻）。
        ed.update(cx, |e, cx| e.toggle_heading_fold_at_line(0, cx));
        let _f0 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        // 先算好编辑点（`content()` 会物化全文 → 必须先于计数基线），再记录此刻
        // `text_shared` 调用计数（含此前折叠动作本身的物化）。
        let at = ed.read(cx).content().find("outside").unwrap() + 4;
        let c0 = ed.read(cx).doc().text_shared_calls();
        // 折叠体之外 Enter + 重建帧：此步任何一处都不得再全篇物化/全扫标题。
        ed.update(cx, |e, ecx| e.replace_range(at..at, "\n", ecx));
        let _f1 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let c1 = ed.read(cx).doc().text_shared_calls();
        assert_eq!(
            c1, c0,
            "折叠标题后别处 Enter 不应再全篇 text_shared（c0={c0} c1={c1}）"
        );
    });
}

/// Cut-1b：纯文本（无 `$`、无 `[!`、无折叠标题）**跨行合并**（结构路径）与随后的
/// 强制整建都不得触达全篇 `text_shared()` 物化——`derived()` 三路预筛全空时直接返回
/// 空三表，不再把 6MB rope 物化成 String。配套 `bench_rw_delete_plain` 跨行合并
/// 24.9ms/次里 `wy.bf.setup` 的 ~16ms 主项（`text_shared` 全拷）就从这条路径消失。
#[gpui::test]
fn merge_lines_plain_skips_full_text_materialization(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "alpha beta gamma\n\ndelta epsilon zeta\n\neta theta iota\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        // 首次建帧：derived 冷启动（物化计数进基线）。
        let _f0 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        // 编辑点须在计数基线前算好（`content()` 会物化全文，不能计入）。
        let at = ed.read(cx).content().find("delta").unwrap();
        let c0 = ed.read(cx).doc().text_shared_calls();
        // 跨行合并：删掉 `delta` 前一行结尾的 `\n` → `cross_newline` → structural。
        ed.update(cx, |e, ecx| e.replace_range(at - 1..at, "", ecx));
        let _f1 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        // 强制整建（regions_ver++ 模拟结构性重建入口）：三路预筛全空 → 依然不得物化。
        ed.update(cx, |e, _| { let rd = e.rd.borrow_mut(); rd.regions_ver.set(rd.regions_ver.get().wrapping_add(1)); });
        let _f2 = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let c1 = ed.read(cx).doc().text_shared_calls();
        assert_eq!(
            c1, c0,
            "纯文本跨行合并/整建不得再全篇 text_shared（c0={c0} c1={c1}）"
        );
        // derived 语义与整建一致（此处两者都应是全空三表）。
        assert!(
            ed.read(cx)
                .derived()
                .borrow()
                .math_regions
                .is_empty()
                && ed.read(cx).derived().borrow().alert_fold_regions.is_empty()
                && ed.read(cx).derived().borrow().heading_fold_bodies.is_empty(),
            "纯文本跨行合并后 derived 应为全空三表"
        );
    });
}

/// M5-B3-1 子项 4 保真：折叠标题后**在别处 Enter**，增量路径（集合重建 + deltas 平移，
/// 移除了旧的「有折叠态便禁用 deltas 平移」守卫）产出的折叠计划必须与强制整建逐字段
/// 一致（`sets` + `deltas` + `delta_sum`）。
#[gpui::test]
fn heading_fold_enter_elsewhere_deltas_match_full_rebuild(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::WyEditor;
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    // `## App`(行 1) 折叠体 [1,4)（被 `# Tail` 截断）；math 块 [6,9)；编辑点在 `outside`。
    let text = "intro\n## App\napp body\napp body 2\n# Tail\noutside\n$$\nx^2\n$$\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, cx| e.toggle_heading_fold_at_line(1, cx));
        // 先整建一次（热缓存/基线）。
        let _f0 = build_frame(ed.read(cx), window, px(900.), (0., 999_999.));
        // 折叠体之外 Enter（`outside` 中段）。
        let at = ed.read(cx).content().find("outside").unwrap() + 4;
        ed.update(cx, |e, ecx| e.replace_range(at..at, "\n", ecx));
        let _f1 = build_frame(ed.read(cx), window, px(900.), (0., 999_999.));
        let inc = ed.read(cx).fold_plan_rc().expect("增量路径折叠计划");
        // 强制整建：清缓存 + 清待消费标志（增量条件不满足 → 走整建）。
        ed.update(cx, |e, _| {
            *e.rd.borrow_mut().fold_plan.borrow_mut() = None;
            e.rd.borrow_mut().struct_edit_pending.set(false);
        });
        let _f2 = build_frame(ed.read(cx), window, px(900.), (0., 999_999.));
        let full = ed.read(cx).fold_plan_rc().expect("整建折叠计划");
        assert_eq!(
            inc.sets.math_start, full.sets.math_start,
            "math_start 不一致"
        );
        assert_eq!(inc.sets.math_body, full.sets.math_body, "math_body 不一致");
        assert_eq!(
            inc.sets.fold_lines, full.sets.fold_lines,
            "fold_lines 不一致"
        );
        assert_eq!(
            inc.sets.fold_bodies, full.sets.fold_bodies,
            "fold_bodies 不一致"
        );
        assert_eq!(
            inc.sets.fold_split, full.sets.fold_split,
            "fold_split 不一致"
        );
        assert_eq!(&*inc.deltas, &*full.deltas, "deltas 不一致");
        assert!(
            (inc.delta_sum - full.delta_sum).abs() < 1e-3,
            "delta_sum 不一致 {} vs {}",
            inc.delta_sum,
            full.delta_sum
        );
    });
}

