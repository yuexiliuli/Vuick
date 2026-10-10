/// 回归：「快速来回拖滚动条，滚动块停在旧位、要晃几下才跟上」——一帧内的高频
/// `MouseMoveEvent` 突发必须**帧级合并、绝不丢最后位置**。旧代码按 `max_fps` 节流
/// 丢弃未到期事件：突发里只有第一单生效，随后停住不产生新事件 → `desired` 停在
/// 第一单（如 30%），滚到轨道底也到不了；「晃几下」才补。现改为事件记
/// `pending_target`（永远最新）、`prepaint` 每帧落地。
#[gpui::test]
fn scrollbar_fast_drag_never_drops_final_target(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let mut doc = String::new();
    for i in 0..80 {
        doc.push_str(&format!(
            "## 第{i}节\n\n正文段落{i}，内容内容内容内容。\n- 列表{i} 内容内容。\n\n`code {i}`\n\n"
        ));
    }
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_epub(true, Default::default(), cx);
            md.set_source(&doc, cx);
        });
    });
    cx.simulate_resize(size(px(300.), px(200.)));
    settle(cx);

    let ov = cx
        .debug_bounds("md-vscrollbar-overlay")
        .expect("纵向滚动条 overlay 应有布局 bounds");
    let n = cx.update(|_, cx| host.read(cx).md.read(cx).item_count());
    assert!(n > 0, "预览应有内容");
    let container = ov.size.height.as_f32();
    let track_top = ov.origin.y.as_f32();
    // 轨道右侧 16px；滑块初始在顶部：thumb_start=0、thumb_len=48（MIN 下限）。
    let track_right = ov.right() - px(4.5);
    let thumb_center = point(track_right, px(track_top + 4.0 + 24.0));
    let bottom_pt = point(track_right, px(ov.bottom().as_f32() - 1.0));

    // 按下（滑块顶部）。fast 突发：30% → 60% → 轨道底，**中间不停帧**。
    cx.simulate_mouse_down(thumb_center, MouseButton::Left, Modifiers::default());
    let mut y = track_top + container * 0.30;
    cx.simulate_mouse_move(
        point(track_right, px(y)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    y = track_top + container * 0.60;
    cx.simulate_mouse_move(
        point(track_right, px(y)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_move(
        bottom_pt,
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    // 停住（不再产生事件）：帧级落地应把「在底部」的最后一单补上。
    settle(cx);
    cx.simulate_mouse_up(bottom_pt, MouseButton::Left, Modifiers::default());
    settle(cx);

    let (desired, max_scroll) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let m = md.read(cx);
        let ls = m.test_list_state();
        let max_off = ls.max_offset_for_scrollbar().y.as_f32();
        (m.desired_scroll.get(), max_off)
    });
    assert!(
        desired >= max_scroll * 0.98,
        "突发拖到底后最终目标不得被丢弃（desired 应近 {max_scroll:.0}，实际 {desired:.0}）；旧节流会停在第一单 ~30%"
    );
}
/// 表格滚动容器 = 预览视口宽；内部固定宽表格超宽时块内可横向滚动、滚动条真实渲染，
/// 句柄跨帧稳定（set_offset 能把表格真正向左滚动）——对齐代码块方案。
#[gpui::test]
fn wide_table_scrolls_in_wrap_mode(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 5 列 × 超长内容：估算宽远超 300px 视口
    let long = "x".repeat(60);
    let md_src = format!(
        "| {long} | {long} | {long} | {long} | {long} |\n|---|---|---|---|---|\n| {long} | {long} | {long} | {long} | {long} |\n"
    );
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(&md_src, cx);
        });
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);

    let preview = cx
        .debug_bounds("md-preview")
        .expect("预览根 md-preview 应存在");
    let table = cx
        .debug_bounds("md-table")
        .expect("表格 md-table 应存在");
    assert!(
        table.size.width > preview.size.width,
        "超宽表格应宽于预览视口（从而可在块内横向滚动而非被裁剪），table={table:?} preview={preview:?}"
    );
    // 表格滚动容器应等于预览视口宽（自身不撑大，内容在内部横向滚动）
    let hscroll = cx
        .debug_bounds("md-table-hscroll")
        .expect("表格滚动容器 md-table-hscroll 应存在");
    assert!(
        hscroll.size.width <= preview.size.width + px(1.),
        "表格滚动容器应等于预览视口宽，实际 hscroll={hscroll:?} preview={preview:?}"
    );
    // 表格应完整放进滚动容器：容器严格高于表格（自然高度，不被裁剪）；
    // 底部预留滚动条轨道条（≥15px），横向滚动条不遮挡表格末行。
    assert!(
        hscroll.size.height > table.size.height,
        "滚动容器应严格高于表格（自然高度不被裁剪），table_h={:.0} hscroll_h={:?}",
        table.size.height.as_f32(),
        hscroll.size.height
    );
    assert!(
        hscroll.size.height.as_f32() - table.size.height.as_f32() >= 15.0,
        "底部应预留横向滚动条轨道条（≥15px 不遮挡内容），table_h={:.0} hscroll_h={:?}",
        table.size.height.as_f32(),
        hscroll.size.height
    );
    // 表格渲染高度至少覆盖 表头 + 分隔线 + 数据行（末行不被裁剪）
    let p = cx.update(|_w, cx| StyleParams::from_theme_with_font(cx, None));
    let row_h = p.para_size.as_f32() * 1.45 + 12.0;
    let content_h = row_h * 2.0 + 1.0;
    assert!(
        table.size.height.as_f32() >= content_h - 1.0,
        "表格高度应覆盖表头+数据行+分隔线（content_h={content_h:.1}），实际={:?}",
        table.size.height
    );
    // 滚动条叠层真实渲染（非零尺寸）
    let sbar = cx
        .debug_bounds("md-table-scrollbar")
        .expect("表格横向滚动条 md-table-scrollbar 应存在");
    assert!(
        sbar.size.width > px(0.) && sbar.size.height > px(0.),
        "横向滚动条不应塌缩成 0，实际={sbar:?}"
    );

    // 表格行在对应滚动句柄处注册；句柄 max_offset.x 应 > 0（可横向滚动）
    let rows = parse_to_rows(&md_src);
    let table_ix = rows
        .iter()
        .position(|r| r.kind == RowKind::Table)
        .expect("应有表格行");
    let (max_x,) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        let h = md.table_scroll.get(&table_ix).expect("表格应注册滚动句柄");
        (h.max_offset().x,)
    });
    assert!(max_x > px(0.), "超宽表格句柄 max_offset.x 应 > 0，实际={max_x:?}");

    // 关键：句柄跨帧稳定，set_offset 能真正向左滚动表格（滚动条拖动生效）
    let origin_x = table.origin.x.as_f32();
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let h = md
            .read(cx)
            .table_scroll
            .get(&table_ix)
            .expect("表格应注册滚动句柄")
            .clone();
        h.set_offset(gpui::point(px(-200.), px(0.)));
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);
    let table2 = cx
        .debug_bounds("md-table")
        .expect("滚动后 md-table 应仍存在");
    assert!(
        table2.origin.x.as_f32() < origin_x - 100.0,
        "设置表格滚动句柄后表格应向左滚动，origin.x: {origin_x:.0} -> {:?}",
        table2.origin.x
    );
}

/// 换行模式下，短代码块的背景应铺满预览视口宽（跟随窗口），而不是只到文本最右边缘，
/// 使各代码块视觉宽度统一；且不产生（无溢出的）横向滚动条。
#[gpui::test]
fn code_block_bg_spans_pane_width_in_wrap(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 很短的多行代码（远窄于视口）
    let md_src = "```rs\nlet a = 1;\nlet b = 2;\n```\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(md_src, cx);
        });
    });
    cx.simulate_resize(size(px(500.), px(400.)));
    settle(cx);

    let preview = cx
        .debug_bounds("md-preview")
        .expect("预览根 md-preview 应存在");
    let band = cx
        .debug_bounds("md-code-band")
        .expect("代码背景带 md-code-band 应存在");
    // 代码块背景铺满预览内容宽 = 视口宽 − 内容列对称留白 ×2 − 行内内衬 ×2
    // （单一来源 `PREVIEW_SIDE_GAP_PX` + `CONTENT_EDGE_PAD_X_PX`），各代码块宽度统一并跟随窗口。
    let expect_w = preview.size.width.as_f32()
        - 2.0 * crate::config::markdown::PREVIEW_SIDE_GAP_PX
        - 2.0 * crate::config::markdown::CONTENT_EDGE_PAD_X_PX;
    assert!(
        (band.size.width.as_f32() - expect_w).abs() < 2.0,
        "短代码块背景应铺满预览内容宽({expect_w:.0})，band={band:?} preview={preview:?}"
    );
}

