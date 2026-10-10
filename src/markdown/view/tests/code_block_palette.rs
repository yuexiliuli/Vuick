/// 代码块上下各补一行空白带：背景色在首个代码行上方 / 末个代码行下方各多出一整行。
/// 验证 3 行代码的代码块渲染出 5 个相同高度的背景带，且首末两个空白带与代码行等高。
#[gpui::test]
fn code_block_has_blank_bands_above_and_below(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let md_src = "```rs\nlet a = 1;\nlet b = 2;\nlet c = 3;\n```\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(md_src, cx);
        });
    });
    cx.simulate_resize(size(px(400.), px(400.)));
    settle(cx);

    let blank_top = cx
        .debug_bounds("md-code-blank-top")
        .expect("代码块顶部空白带应存在");
    let blank_bottom = cx
        .debug_bounds("md-code-blank-bottom")
        .expect("代码块底部空白带应存在");
    // "md-code-band" 选择器被同名的三个代码带覆盖，debug_bounds 只保留最后一个（末行代码带）
    let last_band = cx
        .debug_bounds("md-code-band")
        .expect("代码背景带 md-code-band 应存在");
    let band_h = last_band.size.height.as_f32();

    // 顶/底空白带与代码带等高
    assert!(
        (blank_top.size.height.as_f32() - band_h).abs() < 0.5
            && (blank_bottom.size.height.as_f32() - band_h).abs() < 0.5,
        "空白带应与代码带等高：top={:?} bottom={:?} band={:?}",
        blank_top.size.height,
        blank_bottom.size.height,
        last_band.size.height
    );
    // 底部空白带紧贴最后一（3rd）个代码带下方，背景连续
    assert_eq!(
        blank_bottom.origin.y.as_f32(),
        last_band.origin.y.as_f32() + last_band.size.height.as_f32()
    );

    // 整体跨度：顶部空白带顶部 -> 底部空白带顶部 = 顶空白 + 3 行代码 = 4 * code_line
    let span = blank_bottom.origin.y.as_f32() - blank_top.origin.y.as_f32();
    assert!(
        (span - band_h * 4.0).abs() < 1.5,
        "顶空白带顶到 3 行代码块底部应跨 4*code_line，span={span:.1} band_h={band_h:.1}"
    );
    // 边界：顶部空白带从代码块渲染行顶端（y=0）开始，首行代码带在它下方
    assert_eq!(blank_top.origin.y.as_f32(), 0.0, "顶空白带应位于代码块渲染行顶端，实际={:?}", blank_top.origin.y);
}

/// 回归：代码块背景按主题定制——亮 #F5F5F5、暗 #262626（第 5 步谐波化的中性灰阶）。
/// 明暗切换走 `ui::theme::Theme::set`（第 3 波后 `StyleParams` 从 `ui::theme` 取色）。
#[gpui::test]
fn code_block_bg_matches_theme_palette(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    // 浅色主题
    cx.update(|cx| crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Light, cx));
    let light = cx.update(|cx| StyleParams::from_theme_with_font(cx, None).code_bg);
    assert_eq!(
        light,
        gpui::rgb(0xf5f5f5).into(),
        "亮主题代码块背景应为 #F5F5F5"
    );
    // 深色主题
    cx.update(|cx| crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Dark, cx));
    let dark = cx.update(|cx| StyleParams::from_theme_with_font(cx, None).code_bg);
    assert_eq!(
        dark,
        gpui::rgb(0x262626).into(),
        "暗主题代码块背景应为 #262626"
    );
    // 恢复浅色（主题为全局共享，避免影响后续测试）
    cx.update(|cx| crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Light, cx));
}

/// 行内代码（反引号）pill 配色单一源：亮 #F0F0F0/#202020、暗 #3A3A3A/#E8E8E8——
/// 预览与 WYSIWYG 都取 [`StyleParams::code_inline_bg`/`code_inline_fg`]（WYSIWYG 侧
/// 在 `theme_mirrors_style_params_single_source` 验证抄自同源）。
#[gpui::test]
fn inline_code_palette_single_source(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    cx.update(|cx| crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Light, cx));
    let light = cx.update(|cx| StyleParams::from_theme_with_font(cx, None));
    assert_eq!(light.code_inline_fg, gpui::rgb(0x202020).into(), "亮主题行内代码文字");
    assert_eq!(light.code_inline_bg, gpui::rgb(0xf0f0f0).into(), "亮主题行内代码底");
    cx.update(|cx| crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Dark, cx));
    let dark = cx.update(|cx| StyleParams::from_theme_with_font(cx, None));
    assert_eq!(dark.code_inline_fg, gpui::rgb(0xe8e8e8).into(), "暗主题行内代码文字");
    assert_eq!(dark.code_inline_bg, gpui::rgb(0x3a3a3a).into(), "暗主题行内代码底");
    cx.update(|cx| crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Light, cx));
}

/// 源码行 → 渲染行映射（EPUB 搜索跳转）：标题/空行/段落各占一渲染行，
/// 代码围栏折叠成单个渲染行覆盖多源码行，命中的源码行应定位到对应渲染行。
#[gpui::test]
fn row_for_line_maps_source_lines(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 行 0 标题 / 1 空 / 2 正文一 / 3 正文二 / 4 空 / 5-8 代码围栏 / 9 空
    let src = "# 标题\n\n正文一\n正文二\n\n```\ncode1\ncode2\n```\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(src, cx));
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);

    let md = cx.read(|cx| host.read(cx).md.clone());
    let at = |line: usize| cx.read(|cx| md.read(cx).row_for_line(line));
    assert_eq!(at(2), Some(2), "源码第 2 行（正文一）应映射到渲染行 2");
    assert_eq!(at(3), Some(3), "源码第 3 行（正文二）应映射到渲染行 3");
    assert_eq!(at(6), Some(5), "代码围栏内第 6 行应定位到折叠的代码渲染行");
    assert_eq!(at(8), Some(5), "代码围栏末行仍属折叠代码行");
    assert_eq!(at(0), Some(0), "标题行映射到渲染行 0");
    // 空预览 → None
    let (host2, cx2) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let row = cx2.update(|_, _cx| host2.read(_cx).md.read(_cx).row_for_line(0));
    assert_eq!(row, None, "空预览无渲染行 → None");
}



/// 搜索跳转后：只给命中**文本**临时高亮（不吃整行），
/// 时钟推进超过 [`HIGHLIGHT_MS`] 后自动消失。
#[gpui::test]
fn search_jump_highlight_appears_and_clears(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 行 0 标题 / 1 空 / 2 正文一 / 3 空 / 4 正文二（含关键字）/ 5 空
    let src = "# 标题\n\n正文一\n\n正文二，包含关键字\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(src, cx));
    });
    cx.simulate_resize(size(px(400.), px(400.)));
    settle(cx);

    // 高亮源码第 4 行（正文二）内的搜索词，只覆盖命中文本、不整行
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.highlight_search(4, "关键字", cx);
            cx.notify();
        });
    });
    for _ in 0..5 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }

    let (row_ix, rng) = cx
        .read(|cx| host.read(cx).md.read(cx).search_highlight_range())
        .expect("跳转后应定位到命中文本");
    assert_eq!(row_ix, 4, "正文二应映射到渲染行 4");
    let text = "正文二，包含关键字";
    assert_eq!(&text[rng.clone()], "关键字", "高亮区间应只覆盖搜索词");
    assert!(rng.len() < text.len(), "不应高亮整行（区间小于整行长度）");

    // 时钟推进超过高亮停留时长后，高亮应自动消失
    cx.executor().advance_clock(std::time::Duration::from_millis(HIGHLIGHT_MS + 500));
    for _ in 0..5 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }
    assert!(
        cx.read(|cx| host.read(cx).md.read(cx).search_highlight_range())
            .is_none(),
        "超时后高亮应自动消失"
    );
}

