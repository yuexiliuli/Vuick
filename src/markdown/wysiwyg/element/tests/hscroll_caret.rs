#[gpui::test]
fn table_overflow_shows_hscroll_and_track_drag(cx: &mut TestAppContext) {
    // 用户报「WYSIWYG 表格没有横向滚动条」→ 对齐 preview `markdown/view` 的表格
    // 溢出横向滚动：超宽表格底部出滚动条，轨道点击跳转、滑块可拖（拖动不写源码）。
    use crate::markdown::ir::IrKind;
    use gpui::{
        Bounds, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, size,
    };
    cx.update(crate::actions::bind_local_keymaps);
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
                super::table_hscroll_geo(ed.read(cx), &frame, tbl).expect("宽表格应显示横向滚动条");
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
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        let band_top = ctop + ctop_h - crate::config::layout::CODE_HSCROLL_BAND;
        let band_y = band_top + crate::config::layout::CODE_HSCROLL_BAND / 2.;
        // 轨道命中（远离左端滑块）→ 跳转（不进拖动态）。
        let jump_x = bar_x + bar_w - 4.0;
        let (blk_hit, on_thumb, _) = super::table_hscrollbar_hit(
            ed.read(cx),
            ed.read(cx).frame.borrow().as_ref().map(|f| &**f).unwrap(),
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
            f2.rows.iter().find(|r| r.src_line == blk).unwrap(),
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
    cx.update(crate::actions::bind_local_keymaps);
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
            let off0 = super::source_offset_at_local(ed.read(cx), &frame, p);
            let (l, c) = ed.read(cx).row_col(off0);
            (l, c)
        };
        // 滚到最右 → 同一可视 x 应命中更后列的单元格。
        ed.update(cx, |e, _| e.set_table_h_scroll(blk, max_scroll));
        let f2 = build_frame(ed.read(cx), window, px(300.), (0., 999_999.));
        let off1 = super::source_offset_at_local(ed.read(cx), &f2, p);
        let (line1, col1) = ed.read(cx).row_col(off1);
        assert_eq!(line0, line1, "横向滚动不应改变命中行");
        assert!(
            col1 > col0,
            "同一 x 滚动后应命中更后列：col0={col0} col1={col1}"
        );
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
    cx.update(crate::actions::bind_local_keymaps);
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
    cx.update(crate::actions::bind_local_keymaps);
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
    use gpui::{
        Bounds, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, size,
    };
    cx.update(crate::actions::bind_local_keymaps);
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
        ed.update(cx, |e, _| {
            *e.frame.borrow_mut() = Some(std::rc::Rc::new(frame))
        });
        let band_top = ctop + ctop_h - crate::config::layout::CODE_HSCROLL_BAND;
        let band_y = band_top + crate::config::layout::CODE_HSCROLL_BAND / 2.;
        // 点击轨道**末端**（远离初始左端滑块，必不在滑块上）→ 直接跳转（不进拖动态）。
        let jump_x = bar_x + bar_w - 4.0;
        let jump_y = band_y;
        // 断言**点击前**命中的是轨道（非滑块）。
        let (blk_pre, on_thumb_pre, _) = super::code_hscrollbar_hit(
            ed.read(cx),
            ed.read(cx).frame.borrow().as_ref().map(|f| &**f).unwrap(),
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
            ed.read(cx).frame.borrow().as_ref().map(|f| &**f).unwrap(),
            ed.read(cx)
                .frame
                .borrow()
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .find(|r| r.src_line == blk)
                .unwrap(),
        )
        .unwrap();
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
    cx.update(crate::actions::bind_local_keymaps);
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
    cx.update(crate::actions::bind_local_keymaps);
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
    cx.update(crate::actions::bind_local_keymaps);
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
    cx.update(crate::actions::bind_local_keymaps);
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
            assert!(
                l == 2 && col == 0,
                "前段行尾向右应到首内容行行首，得 ({l},{col})"
            );
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
            assert!(
                l == 5 && col == 0,
                "末内容行行尾向右应到后段行首，得 ({l},{col})"
            );
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
    cx.update(crate::actions::bind_local_keymaps);
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
    cx.update(crate::actions::bind_local_keymaps);
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
        assert!(
            (off_x - 12.).abs() < 0.1,
            "代码行文本内衬 = 面板 pl_3（12px），得 {off_x}"
        );
        let lh = f32::from(code.lh);
        let text_start = pad + off_x; // 24：代码文本起始 x（面板背景在 pad=12，与正文对齐）
        // 1) 点击文本起始处 → 光标 x == 文本起始 x（不右偏、不盖字）。
        let off0 = source_offset_at_local(ed.read(cx), &frame, point(px(text_start), px(lh * 1.5)));
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
        let off1 = source_offset_at_local(ed.read(cx), &frame, point(px(pad + 6.), px(lh * 1.5)));
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
        let off2 = source_offset_at_local(ed.read(cx), &frame, point(px(cx_), px(lh * 1.5)));
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

