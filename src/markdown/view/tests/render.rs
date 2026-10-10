/// GFM 表格应渲染成有实际宽高的网格；表格行注册**块内**横向滚动句柄，且不撑出
/// 页面级横向滚动（页面恒换行，超宽表格在块内滚动）。
#[gpui::test]
fn renders_markdown_table_as_grid(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let md_src = "| 型号 | 尺寸 |\n|:---|---:|\n| A | 10 |\n| B | 20 |\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(md_src, cx));
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);

    let table = cx
        .debug_bounds("md-table")
        .expect("表格应渲染出 md-table 网格");
    assert!(table.size.width > px(0.), "表格应有宽度，实际={table:?}");
    assert!(table.size.height > px(0.), "表格应有高度（表头+数据行），实际={table:?}");

    let (has_table_scroll, page_max_x) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        (
            !md.table_scroll.is_empty(),
            md.h_handle.max_offset().x,
        )
    });
    assert!(has_table_scroll, "表格行应注册块内横向滚动句柄");
    assert_eq!(
        page_max_x, px(0.),
        "页面恒换行：表格不撑出页面级横向滚动，max_offset.x={page_max_x:?}"
    );
}

/// 每个标题下的文本行（段落/列表/引述）都应获得相同的加高行距（不只是第一个标题下）。
/// 复现：多个标题各带若干段，逐个用 `bounds_for_item` 检查文本 item 高度。
#[gpui::test]
fn paragraph_gap_applies_after_every_heading(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 两个标题各带文本：开头段落 → 标题一 → 段落 → 标题二 → 列表项 → 引述
    let md_src = "# 标题一\n开头段落\n\n## 标题二\n第二个段落\n- 列表项\n> 引述\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(md_src, cx));
    });
    cx.simulate_resize(size(px(400.), px(600.)));
    settle(cx);

    let p = cx.update(|_w, cx| StyleParams::from_theme_with_font(cx, None));
    let rows = parse_to_rows(md_src);
    let expected = p.para_line.as_f32() + p.para_gap.as_f32();
    // 恒换行下文本行单行高 = 包裹盒的最小高度（`para_line + para_gap + 6` 兜底 + py_1
    // 呼吸内衬），比旧非换行「单行裁剪」高约 6px；以首段为基准，逐行校验一致性——
    // 既保留「第二个标题下的段落间距同样生效」的原回归语义，又不硬编码具体像素。
    let ref_h: f32 = cx
        .update(|_, cx| {
            let m = host.read(cx).md.read(cx);
            let ref_ix = rows
                .iter()
                .position(|r| r.kind == RowKind::Paragraph)
                .expect("应有段落行");
            m.list_state
                .bounds_for_item(ref_ix)
                .expect("段落行应有 bounds")
                .size
                .height
                .as_f32()
        });
    assert!(ref_h >= expected, "单行段落高不应低于单行基线（ref_h={ref_h:.1} >= {expected:.1}）");

    // 找出所有文本行（段落/列表/引述）在 rows 中的下标
    let mut checked = 0usize;
    for (ix, r) in rows.iter().enumerate() {
        if !matches!(
            r.kind,
            RowKind::Paragraph | RowKind::Bullet | RowKind::Quote
        ) {
            continue;
        }
        let b = cx
            .update(|_, cx| host.read(cx).md.read(cx).list_state.bounds_for_item(ix))
            .expect("文本行 item 应有 bounds");
        // 引述带本身略矮（内衬少 2px 量级），放宽半行容差；段落/列表须与基准一致。
        let tol = if r.kind == RowKind::Quote { 4.0 } else { 1.0 };
        assert!(
            (b.size.height.as_f32() - ref_h).abs() < tol,
            "第 {ix} 行（{:?}）高度应一致（基准 {ref_h:.1}±{tol}），实际={:?}——段落间距在任一标题下都不应塌缩/丢失",
            r.kind,
            b.size.height
        );
        checked += 1;
    }
    assert_eq!(checked, 4, "应检查到 4 个文本行（2 段+1 列表+1 引述）");
}

/// EPUB 阅读 / 宿主全局字号：缩放应更新正文字号覆盖（基数 = 主题默认）。
#[gpui::test]
fn font_zoom_updates_override(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let md_src = "# 标题\n正文段落内容，用于字号缩放验证。\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(md_src, cx));
    });
    cx.simulate_resize(size(px(400.), px(600.)));
    settle(cx);

    let default = cx.update(|_, cx| {
        StyleParams::from_theme_with_font(cx, None).para_size.as_f32()
    });
    // 初始无覆盖
    let none = cx.update(|_, cx| host.read(cx).md.read(cx).font_override);
    assert!(none.is_none(), "初始应无字号覆盖");

    // 模拟宿主推进全局字号（放大 1.5）
    cx.update(|window, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_font_size_anchored(default + 1.5, cx));
        window.refresh();
    });
    settle(cx);
    let after = cx.update(|_, cx| host.read(cx).md.read(cx).font_override);
    assert!(after.is_some(), "缩放后应产生字号覆盖");
    assert!(
        (after.unwrap() - (default + 1.5)).abs() < 0.001,
        "字号应增大 1.5，实际={after:?}"
    );
}

/// 标题字号应随正文字号等比缩放（#/##/### 一起变大变小，子级始终小于父级）。
/// 设计基准 `HEADING_DESIGN_BASE`(=14) 下 h1=24 h2=20 h3=18 h4=16 h5/6=15；其余字号按
/// `body / 基准` 等比放大，最多只放大 2 倍。与运行期默认字号 `layout::DEFAULT_FONT_SIZE`
/// 无耦合（默认字号调大时基准不变）。
#[gpui::test]
fn heading_metrics_scale_with_body_font(cx: &mut TestAppContext) {
    let h = |body: f32| -> [f32; 7] {
        let mut out = [0.0f32; 7];
        for lv in 1..=6u8 {
            out[lv as usize] =
                crate::markdown::view::style::heading_metrics_for_size(body, lv).0.as_f32();
        }
        out
    };
    // 参考字号 14px：标题等于设计值。
    let ref14 = h(14.0);
    assert_eq!(
        &ref14[1..],
        &[24.0, 20.0, 18.0, 16.0, 15.0, 15.0],
        "14px 时标题应等于设计值：{ref14:?}"
    );
    // 等比放大：正文翻倍 → 各标题也翻倍（比例不随 DEFAULT_FONT_SIZE 变化）。
    let big = h(28.0);
    for lv in 1..=6u8 {
        assert!(
            (big[lv as usize] - ref14[lv as usize] * 2.0).abs() < 0.001,
            "h{lv} 应放大 2 倍：{} vs {}",
            big[lv as usize],
            ref14[lv as usize] * 2.0
        );
    }
    // 任意正文字号下：标题始终大于正文、子级小于父级（h1>h2>h3>h6）。
    for &body in &[14.0f32, 24.0, 28.0] {
        let hb = h(body);
        for lv in 1..=6u8 {
            assert!(
                hb[lv as usize] > body,
                "h{lv} 应大于正文字号 {body}：{}",
                hb[lv as usize]
            );
        }
        assert!(
            hb[1] > hb[2] && hb[2] > hb[3] && hb[3] > hb[5],
            "h1>h2>h3>h6 @ {body}：{hb:?}"
        );
    }
    // 绑定默认值：预览默认正文 = layout::DEFAULT_FONT_SIZE，标题随之缩放。
    cx.update(crate::actions::bind_local_keymaps);
    let p = cx.update(|cx| StyleParams::from_theme_with_font(cx, None));
    assert_eq!(
        p.para_size.as_f32(),
        crate::config::layout::DEFAULT_FONT_SIZE,
        "预览默认正文字号应与 layout 一致"
    );
    for lv in 1..=6u8 {
        let expect = crate::markdown::view::style::heading_metrics_for_size(
            crate::config::layout::DEFAULT_FONT_SIZE,
            lv,
        )
        .0;
        assert_eq!(p.heading_metrics(lv).0, expect, "h{lv} 默认字号下应随正文缩放");
    }
}

/// 居中阅读栏（wrap 模式）：内容列宽 = min(72ch, 视口) 且水平居中；窗口窄于 72ch 时
/// 退化为占满视口（不横向超界）。宽块（代码/表格）仍在列内各自横向滚动（另一组测试钉死）。
#[gpui::test]
fn reading_column_caps_and_centers_wrap_list(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let md_src = "# 标题\n正文段落内容，用于验证居中阅读栏。\n\n- 列表项\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source(md_src, cx);
            md.set_wrap(true, cx);
        });
    });
    // 宽窗口（视口 > 72ch）：列宽 = 阅读栏宽，list-wrap 在面板内居中。
    cx.simulate_resize(size(px(1400.), px(800.)));
    settle(cx);
    let wrap = cx.debug_bounds("md-list-wrap").expect("md-list-wrap 应渲染");
    let pane = cx.debug_bounds("md-preview").expect("md-preview 应渲染");
    let body_fs = cx.update(|_, cx| StyleParams::from_theme_with_font(cx, None).para_size.as_f32());
    let col = crate::config::markdown::reading_col_px(body_fs);
    let pane_w = pane.size.width.as_f32();
    assert!(
        (wrap.size.width.as_f32() - col).abs() < 3.0,
        "宽窗下列宽应 = 72ch（{col:.0}），实际 {:.0}",
        wrap.size.width.as_f32()
    );
    let expect_left = pane.origin.x.as_f32() + (pane_w - wrap.size.width.as_f32()) / 2.;
    assert!(
        (wrap.origin.x.as_f32() - expect_left).abs() < 3.0,
        "列应水平居中：期望左缘 {expect_left:.0}，实际 {:?}",
        wrap.origin
    );
    // 窄窗口（视口 < 72ch）：列宽退化为占满视口，不引入横向超界。
    cx.simulate_resize(size(px(300.), px(800.)));
    settle(cx);
    let wrap_n = cx.debug_bounds("md-list-wrap").expect("窄窗 md-list-wrap 应渲染");
    let pane_n = cx.debug_bounds("md-preview").expect("窄窗 md-preview 应渲染");
    assert!(
        (wrap_n.size.width.as_f32() - pane_n.size.width.as_f32()).abs() < 3.0,
        "窄于 72ch 应占满视口：wrap={:.0} pane={:.0}",
        wrap_n.size.width.as_f32(),
        pane_n.size.width.as_f32()
    );
    // 非换行语义已废弃：`set_wrap(false)` 后页面仍恒换行——列宽仍 = 居中阅读栏
    // 72ch（不为内容自然宽撑宽）、无页面级横向滚动。
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_wrap(false, cx));
    });
    cx.simulate_resize(size(px(1400.), px(800.)));
    settle(cx);
    // 幂等再 resize 一次 + 多 pump 帧：`set_wrap` 的通知与 resize 合帧时 debug_bounds
    // 可能停在上一尺寸（h_handle 已新、元素 bounds 滞后一帧），多泵一轮即收敛。
    cx.simulate_resize(size(px(1400.), px(800.)));
    settle(cx);
    let wrap_p = cx
        .debug_bounds("md-list-wrap")
        .expect("恒换行 md-list-wrap 应渲染");
    let (still_wrap, max_x) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        (md.is_wrap(), md.h_handle.max_offset().x)
    });
    assert!(still_wrap, "set_wrap(false) 后页面仍恒换行");
    assert!(
        (wrap_p.size.width.as_f32() - col).abs() < 3.0,
        "恒换行下列宽应仍 = 居中阅读栏 {col:.0}，实际 {:.0}（不再按内容自然宽撑宽）",
        wrap_p.size.width.as_f32()
    );
    assert_eq!(max_x, px(0.), "页面不应有页面级横向滚动，max_offset.x={max_x:?}");
}

/// 现代标题排版的配套常量（单一来源集中式断言）：
/// - 阅读栏列宽 = `READING_COL_CHARS × body × READING_COL_AVG_CHAR_EM`（正文 14px ≈ 806px）；
/// - 标题行高系数分级（h1 收紧 ~1.3 → h5/6 透气 ~1.5），大标题行高 < 小标题行高 < 正文 2×。
#[gpui::test]
fn reading_col_and_heading_line_ratio_constants(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let p = cx.update(|cx| StyleParams::from_theme_with_font(cx, None));
    let body = p.para_size.as_f32();
    let col = crate::config::markdown::reading_col_px(body);
    assert_eq!(
        col,
        crate::config::markdown::READING_COL_CHARS
            * body
            * crate::config::markdown::READING_COL_AVG_CHAR_EM,
        "阅读栏宽应 = 列数 × 正文字号 × 平均字宽系数"
    );
    // 正文 14px 参考：72ch ≈ 806px（典型阅读栏宽，符合「~800px 级」预期）。
    assert!((crate::config::markdown::reading_col_px(14.0) - 806.4).abs() < 0.01);
    assert!(
        crate::config::markdown::READING_COL_ENABLED,
        "阅读栏默认应开启"
    );
    // 行高系数分级：h1 < h2 < h3 < h4 < h5，且都 > 1、< 2（h1 收紧、小级透气）。
    let ratios = crate::config::markdown::HEADING_LINE_RATIOS;
    assert!(
        ratios[1] < ratios[2] && ratios[2] < ratios[3] && ratios[3] < ratios[4] && ratios[4] < ratios[5],
        "标题行高系数应由大到小分级：{ratios:?}"
    );
    for lv in 1..=6u8 {
        let (fs, lh) = p.heading_metrics(lv);
        let ratio = f32::from(lh) / f32::from(fs);
        assert!(
            (ratio - crate::config::markdown::HEADING_LINE_RATIOS[lv as usize]).abs() < 0.001,
            "h{lv} 行高系数应等于分级常量 {}：得 {ratio:.2}",
            crate::config::markdown::HEADING_LINE_RATIOS[lv as usize]
        );
    }
}

/// 字号缩放（EPUB 阅读 / 宿主 set_font_override）：缩放**不带动页面滚动**（同 PDF 视口锚定）。
///
/// 预览自身的 Ctrl 滚轮缩放覆盖层已移除——缩放入口统一在宿主（app::panes zoom_overlay →
/// change_font_size），这里验证底层原语 `set_font_size_anchored`（宿主推进全局字号时
/// 的绝对落点/外部调用方都经它落地）：
/// 视口顶部块锚定不动；普通滚轮（非 Ctrl）照常滚动、字号不变。
#[gpui::test]
fn font_zoom_keeps_viewport_top_anchored(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 大文档（块数 > WINDOWED_MIN_BLOCKS）→ 窗口级懒解析，纵向可滚。
    let mut src = String::new();
    for i in 0..1000 {
        src.push_str(&format!("# 第{i}章\n\n正文段落内容 alpha，用于缩放手感验证。\n- 列表项\n\n"));
    }
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(
                PreviewSrc::from(gpui::SharedString::from(src.clone())),
                std::rc::Rc::new(crate::markdown::scan_blocks(&src)),
                cx,
            );
        });
    });
    cx.simulate_resize(size(px(600.), px(700.)));
    settle(cx);
    let n = cx.update(|_, cx| host.read(cx).md.read(cx).item_count());
    assert!(
        n > crate::config::limits::WINDOWED_MIN_BLOCKS,
        "应启用窗口级懒解析，块数 {n}"
    );
    // 跳到文档中部（留出双向滚动余量）。
    let mid = n / 2;
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.scroll_to_row(mid);
            cx.notify();
        });
    });
    settle(cx);
    let def = cx.update(|_, cx| StyleParams::from_theme_with_font(cx, None).para_size.as_f32());
    let before = cx.update(|_, cx| host.read(cx).md.read(cx).test_abs_scroll_top().0);
    assert!(before > 100, "应已滚到中部（block={before}）");

    // 缩放字号（+1.5）：字号变大，且滚动位置（视口顶部块）不变（不滚动）。
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_font_size_anchored(def + 1.5, cx);
            cx.notify();
        });
    });
    settle(cx);
    let (f1, after_block) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        (md.font_override, md.test_abs_scroll_top().0)
    });
    assert!(f1.is_some() && f1.unwrap() > def, "缩放应放大字号：{f1:?}");
    assert!(
        (after_block as i64 - before as i64).abs() <= 2,
        "缩放不应滚动：视口顶部块 {before} → {after_block}"
    );

    // 普通滚轮（非 Ctrl）：照常滚动、字号不变。
    let f_before_plain = f1.unwrap();
    let pos = {
        let b = cx.debug_bounds("md-preview-root").expect("预览根应渲染");
        point(
            px(b.origin.x.as_f32() + b.size.width.as_f32() * 0.5),
            px(b.origin.y.as_f32() + b.size.height.as_f32() * 0.5),
        )
    };
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(0.), px(240.))),
        modifiers: Modifiers::default(),
        ..Default::default()
    });
    settle(cx);
    let (f2, plain_block) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        (md.font_override, md.test_abs_scroll_top().0)
    });
    assert_eq!(f2, Some(f_before_plain), "普通滚轮不应改变字号");
    assert!(
        plain_block != after_block,
        "普通滚轮应照常滚动（块 {after_block} → {plain_block}）"
    );
}

