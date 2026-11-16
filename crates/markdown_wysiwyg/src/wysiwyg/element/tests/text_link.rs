#[test]
fn math_to_unicode_translates_superscripts_and_symbols() {
    use super::math_to_unicode;
    assert!(math_to_unicode("E=mc^2").contains('²'), "上标 2 → ²");
    assert!(
        math_to_unicode("\\int_0^1 x^2 dx").contains('∫'),
        "积分符号"
    );
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
    assert!(
        g.nodes.iter().any(|n| n.id == "A" && n.label == "开始"),
        "A=开始"
    );
    assert!(
        g.nodes.iter().any(|n| n.id == "C" && n.label == "完成"),
        "C=完成"
    );
    // 回归：`B{判断}` 的定义不能被后续裸引用 `B -->` 覆盖成 “B”。
    assert!(
        g.nodes.iter().any(|n| n.id == "B" && n.label == "判断"),
        "B 的标签应保持为「判断」，却被后续裸引用覆盖：{:?}",
        g.nodes.iter().filter(|n| n.id == "B").collect::<Vec<_>>()
    );
    let b = g.nodes.iter().find(|n| n.id == "B").unwrap();
    assert_eq!(b.shape, MmShape::Diamond, "B 为判断（菱形）");
    let e = g
        .edges
        .iter()
        .find(|e| e.from == "B" && e.to == "C")
        .unwrap();
    assert_eq!(e.label.as_deref(), Some("是"), "边 B→C(是)");
}

#[gpui::test]
fn undo_redo_round_trip(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
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
    // C5 修复：WYSIWYG 链接不该用 `colors.accent`（浅色主题 = 中性灰），
    // 而是单一源 `StyleParams.link`（亮 #0b57d0 / 暗 #6fb1ff，与源码编辑器同蓝）。
    use super::runs_to_text_runs;
    use gpui::FontWeight;
    let base = gpui::font("Arial");
    let mono = gpui::font("Consolas");
    let text = "点 示例 你";
    // 渲染字节 [4..10) 是链接（“示例”，两个 3 字节汉字 → 6 字节）。用**已对齐**的
    // char 边界区间测正常路径；中切区间由 `runs_to_text_runs_clamps_*` 单独覆盖。
    let runs = vec![((4..10), crate::markdown::parse::RunKind::Link)];
    let accent = gpui::Hsla::default();
    let link: gpui::Hsla = gpui::rgb(0x0b57d0).into();
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
        .find(|r| r.color == link && r.len == 6)
        .expect("链接段应为蓝色 TextRun（len=6 汉字）");
    assert_eq!(link_run.color, link, "链接文字色应等于 StyleParams.link 蓝");
    assert!(link_run.underline.is_some(), "链接应有下划线");
    let ul = link_run.underline.as_ref().unwrap();
    assert_eq!(ul.color, Some(link), "下划线颜色与文字同蓝");
    // 非链接段保持预设底色（基色），不误伤。
    let _ = FontWeight::default();
    assert!(
        out.iter().any(|r| r.len == 4),
        "应有链接前导“点 ”（4 字节，对齐后的字符边界段）"
    );
}

#[test]
fn runs_to_text_runs_clamps_mid_char_ranges_to_char_boundaries() {
    // 回归：run 边界落在多字节字符（CJK/emoji）内部时不得 panic，产出的每个
    // gpui::TextRun 长度必须停在 `text` 的字符边界上并铺满全文——否则 GPUI
    // `Text`/`StyledText` 在非字符边界切 run 会 panic
    // （`elements/text.rs`：`assertion failed: self.text.is_char_boundary(run.start)`），
    // 对应预览侧 `markdown::view::row::char_align_range` 的既有防御。
    use super::runs_to_text_runs;
    use crate::markdown::parse::RunKind;
    let base = gpui::font("Arial");
    let mono = gpui::font("Consolas");
    let accent = gpui::Hsla::default();
    let link: gpui::Hsla = gpui::rgb(0x0b57d0).into();
    let code_inline_fg = gpui::Hsla::default();
    // "测🍎试❗"：测=3B，🍎=4B，试=3B，❗=3B → 13 字节。
    let text = "测🍎试❗";
    assert_eq!(text.len(), 13);
    let plain = gpui::Hsla::default();

    // 校验输出：run 无空隙铺满全文，且每个 run 边界都是字符边界。
    let verify_tiling = |text: &str, out: &[gpui::TextRun]| {
        let mut off = 0usize;
        for r in out {
            assert!(r.len > 0, "不应产出空 run");
            assert!(
                text.is_char_boundary(off),
                "run 起点 {off} 不是字符边界（len={}）",
                r.len
            );
            let end = off + r.len;
            assert!(end <= text.len(), "run 越界：{end} > {}", text.len());
            assert!(
                text.is_char_boundary(end),
                "run 终点 {end} 不是字符边界（len={}）",
                r.len
            );
            off = end;
        }
        assert_eq!(off, text.len(), "TextRun 必须无空隙铺满全文");
    };

    // —— 1) 起点切进『测』内部(b=1)、终点切进 🍎 内部(b=5) ——
    // 吸收后应是 [0..3) 正文 + [3..7) 加粗(🍎) + [7..13) 正文。
    let out = runs_to_text_runs(
        text,
        &[(1..5, RunKind::Bold)],
        &base,
        plain,
        &mono,
        accent,
        link,
        code_inline_fg,
    );
    verify_tiling(text, &out);
    let bold = out
        .iter()
        .filter(|r| r.font.weight == gpui::FontWeight::BOLD)
        .collect::<Vec<_>>();
    assert_eq!(bold.len(), 1, "应收敛出一个加粗 run: {out:?}");
    assert_eq!(bold[0].len, 4, "加粗 run 应恰好覆盖 🍎（4 字节）");

    // —— 2) 完全落在『测』内部的区间 [1..2)（两端都非边界）→ 坍缩为空、被丢弃 ——
    let out = runs_to_text_runs(
        text,
        &[(1..2, RunKind::Bold), (10..13, RunKind::Italic)],
        &base,
        plain,
        &mono,
        accent,
        link,
        code_inline_fg,
    );
    verify_tiling(text, &out);
    let italics = out
        .iter()
        .filter(|r| r.font.style == gpui::FontStyle::Italic)
        .collect::<Vec<_>>();
    assert_eq!(italics.len(), 1, "斜体 run 应保留一个（❗）: {out:?}");
    assert!(
        text.is_char_boundary(italics[0].len),
        "斜体 run 长度应为字符边界长"
    );

    // —— 3) 越界区间 + 反向区间 + 重叠区间：一律不 panic、输出仍然铺满 ——
    #[allow(clippy::reversed_empty_ranges)] // 故意喂「反向 → 空」区间，检验 tiling 兜底不 panic
    let tricky = vec![
        (20..30, RunKind::Bold), // 整体越界 → 吸收到行尾/空
        (9..2, RunKind::Italic), // 反向 → 空
        (1..13, RunKind::Link),  // 切进首字符 → 吸收成 [0..13) 整行链接
    ];
    let out = runs_to_text_runs(
        text,
        &tricky,
        &base,
        plain,
        &mono,
        accent,
        link,
        code_inline_fg,
    );
    verify_tiling(text, &out);
    assert!(
        out.iter().any(|r| r.underline.is_some()),
        "链接段应至少保留（整行）：{out:?}"
    );

    // —— 4) 行内代码 + 多字节：菱形/emoji 混排不被切半 ——
    let text2 = "代码`测🍎`尾";
    let out = runs_to_text_runs(
        text2,
        &[(3..8, RunKind::InlineCode)],
        &base,
        plain,
        &mono,
        accent,
        link,
        code_inline_fg,
    );
    verify_tiling(text2, &out);
    assert!(
        out.iter().any(|r| r.font == mono),
        "行内代码段应走等宽字体：{out:?}"
    );
}

#[gpui::test]
fn link_theme_single_source_equals_editor_blue(cx: &mut TestAppContext) {
    // 回检测试（坑 D 单一源）：WyTheme.link ← StyleParams.link ← 明/暗主题蓝，
    // 且与源码编辑器 `editor/markdown.rs` 的 LINK_LIGHT/LINK_DARK 同值。
    use crate::markdown::view::style::StyleParams;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("none"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_window, cx| {
        let ed = host.read(cx).ed.clone();
        let theme = ed.read(cx).rd.borrow().theme.clone();
        let p = StyleParams::from_theme_with_font(cx, Some(f32::from(theme.font_size)));
        assert_eq!(theme.link, p.link, "WyTheme.link 抄自 StyleParams.link");
        // 浅色主题（main.rs 强制 Light）：应编辑器 LINK 蓝 #0b57d0 逐字节一致。
        let cs = if p.is_dark {
            ui::theme::ColorScheme::dark()
        } else {
            ui::theme::ColorScheme::light()
        };
        let expect: gpui::Hsla = cs.link.into();
        assert_eq!(theme.link, expect, "链接蓝单一源 ColorScheme::link");
    });
}

#[gpui::test]
fn inline_image_hit_is_found_and_within_display_range(cx: &mut TestAppContext) {
    // B1 修复：`![alt](src)` 的 `RunKind::Image` 只覆盖 alt——byte_map[alt] 是
    // **alt 起点**（不是 `!`）。旧实现在 alt 起点处查 `![` 永远找不到（tail 没有
    // `!`），且显示区间按整段源跨度算、越出 shaped 行 → `position_for_index` 落空。
    // 现在：src 起点从 `!` 反查，显示区间收在 alt 内。
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
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
fn runs_gap_and_tail_carry_base_text_color() {
    // 回归（评审后真机报告：WYSIWYG 文字不可见）：`runs_to_text_runs` 的间隙/尾段
    // TextRun 必须显式带正文色——收敛到 `text_runs` 核心时曾漏设（`TextRun::default()`
    // 的 color 是黑色），暗色主题下正文间隙/尾段被画成黑字 → 不可见。gpui 对每 run
    // 颜色直接决定字形绘制色（`text_system::line.rs` 累积器初值 `black()`）。
    use super::runs_to_text_runs;
    use crate::markdown::parse::RunKind;
    let base = gpui::font("Arial");
    let mono = gpui::font("Consolas");
    let accent = gpui::Hsla::default();
    let link: gpui::Hsla = gpui::rgb(0x0b57d0).into();
    let code_inline_fg = gpui::Hsla::default();
    // 可辨识、非默认黑的正文色（旧实现间隙即用它）。
    let fg: gpui::Hsla = gpui::rgb(0xabcdef).into();
    // "ab**cd**ef"：Bold 占 [2..4)，前导间隙 [0..2)、尾段 [4..6)。
    let text = "abcdef";
    let out = runs_to_text_runs(
        text,
        &[(2..4, RunKind::Bold)],
        &base,
        fg,
        &mono,
        accent,
        link,
        code_inline_fg,
    );
    assert_eq!(out.len(), 3, "间隙 + 粗体 + 尾段三段");
    assert_eq!(out[0].len, 2, "前导间隙 'ab'");
    assert_eq!(out[0].color, fg, "前导间隙必须带正文色（不得回落默认黑）");
    assert_eq!(out[1].len, 2, "粗体 'cd'");
    assert_eq!(out[1].color, fg, "粗体 run 继承正文色");
    assert_eq!(out[2].len, 2, "尾段 'ef'");
    assert_eq!(out[2].color, fg, "尾段必须带正文色（不得回落默认黑）");
    // 无 runs（整段纯文本）也应收满且带正文色。
    let out = runs_to_text_runs(text, &[], &base, fg, &mono, accent, link, code_inline_fg);
    assert_eq!(out.len(), 1, "纯文本段落单 run 铺满");
    assert_eq!(out[0].len, text.len());
    assert_eq!(out[0].color, fg, "纯文本 run 必须带正文色");
}

#[gpui::test]
fn unsafe_url_click_discards_not_fail_open_to_open_page(cx: &mut TestAppContext) {
    use crate::markdown::parse::recognition::LinkHit;
    use std::cell::RefCell;
    use std::rc::Rc;
    use super::{Frame, link_hit_at, text_row_shift};
    // Bug A 红测（mouse.rs:344-349）：点「不安全」URL 链接（`is_safe_external_url=false`，
    // 如 `javascript:…`）现状 **fail-open** 落入 `LinkHit::Url(u) | LinkHit::Page(u)` 并列臂
    // → 把不安全 url 当页面广播 `WyEvent::OpenPage` 给宿主。修复方向 = Url 且不安全显式
    // 丢弃（不 open_url 也不 OpenPage），Page 才 OpenPage。本测试同时锁「不安全 Url 被
    // discard」与「Page 正常 OpenPage」两态，能区分二者。
    cx.update(crate::test_support::bind_local_keymaps);
    // `[[内页]]` → LinkHit::Page("内页")（正常应 OpenPage）；`[x](javascript:evil)`
    // → LinkHit::Url("javascript:evil")（is_safe_external_url=false，应丢弃）。
    let text = "wiki[[内页]] 危险 [x](javascript:evil) 尾\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let captured = Rc::new(RefCell::new(Vec::<String>::new()));
    // 订阅（`App::subscribe` 激活是 defer 延后；detach 保活句柄，监听保留到实体释放）。
    // 注意：`cx.emit` 入 pending_effects，在本次 `App::update` 收尾才 flush——断言必须放
    // 在 `vcx.update` **之外**（flush 之后），否则 closure 内永远读到空。
    let mut frame_rc = None;
    vcx.update(|window, cx| {
        let captured_c = captured.clone();
        window
            .subscribe::<WyEditor, crate::markdown::wysiwyg::WyEvent>(
                &ed,
                cx,
                move |_em: Entity<WyEditor>,
                      ev: &crate::markdown::wysiwyg::WyEvent,
                      _w: &mut Window,
                      _cx: &mut gpui::App| {
                    if let crate::markdown::wysiwyg::WyEvent::OpenPage(p) = ev {
                        captured_c.borrow_mut().push(p.clone());
                    }
                },
            )
            .detach();
        // 建帧注入 + last_bounds 置原点（窗口坐标 == 帧局部坐标，reading_col_shift=0）。
        let width = f32::from(window.viewport_size().width).max(1.);
        let frame = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
        let bounds_h = f32::from(frame.total).max(600.);
        let rc = Rc::new(frame);
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(width), px(bounds_h)),
            )));
            *e.rd.borrow_mut().frame.borrow_mut() = Some(rc.clone());
        });
        frame_rc = Some(rc);
    });
    let rc = frame_rc.expect("帧应已注入");
    // 命中点 helper：按 `row.extra.links` 目标找链接显示区几何中点——与
    // `element::link_hit_grips`（element/mod.rs ~646）同式（origin=0 时即窗口坐标）。
    let hit_at = |f: &Frame, pred: &dyn Fn(&LinkHit) -> bool| -> Option<(f32, f32)> {
        let r = f
            .rows
            .iter()
            .find(|r| r.extra.links.iter().any(|l| pred(&l.target)))?;
        let lk = r.extra.links.iter().find(|l| pred(&l.target))?;
        let w0 = r.wrapped.first()?;
        let p0 = w0.position_for_index(lk.disp.start, r.lh)?;
        let p1 = w0.position_for_index(lk.disp.end, r.lh)?;
        let ox = f32::from(f.padding)
            + f32::from(r.x_off)
            + f32::from(r.align_offsets.first().copied().unwrap_or(px(0.)));
        let oy = f32::from(r.top) + text_row_shift(r);
        let cx_ = ox + f32::from(p0.x) + (f32::from(p1.x) - f32::from(p0.x)) / 2.;
        let cy = oy + f32::from(p0.y) + f32::from(r.lh) / 2.;
        Some((cx_, cy))
    };
    let (wxi, wyi) = hit_at(&rc, &|t| matches!(t, LinkHit::Page(p) if p == "内页"))
        .expect("文档应含 wiki 链接 [[内页]]");
    let (ux, uy) = hit_at(&rc, &|t| matches!(t, LinkHit::Url(u) if u == "javascript:evil"))
        .expect("应命中不安全 url 链接");
    // 点击（真实 handler 同步路径：`on_mouse_down` C5 分支 → emit OpenPage，收尾 flush）。
    vcx.update(|window, cx| {
        // ① 正向对照：wiki 页链接命中点准确（几何预检）。
        let probe_w = link_hit_at(ed.read(cx), &rc, point(px(wxi), px(wyi)));
        assert!(
            matches!(probe_w, Some(LinkHit::Page(ref p)) if p == "内页"),
            "wiki 点击点应命中（几何预检）：{probe_w:?}"
        );
        let down = MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(wxi), px(wyi)),
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        ed.update(cx, |e, cx| e.on_mouse_down(&down, window, cx));
        // ② 不安全 url 命中点准确（几何预检）。
        let probe = link_hit_at(ed.read(cx), &rc, point(px(ux), px(uy)));
        let probe_hits_unsafe =
            matches!(probe, Some(LinkHit::Url(ref u)) if u == "javascript:evil");
        assert!(
            probe_hits_unsafe,
            "点击点应命中不安全链接（几何预检）：{probe:?}"
        );
        let down2 = MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(ux), px(uy)),
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        ed.update(cx, |e, cx| e.on_mouse_down(&down2, window, cx));
    });
    // —— flush 完成后才在进程外断言 ——
    // ① wiki 页链接 → OpenPage("内页")（Page 语义必须保留，验证事件管线确能收到）。
    assert!(
        captured.borrow().contains(&"内页".to_string()),
        "wiki 页链接 [[内页]] 应正常 OpenPage（锚定事件管线可用）：{:?}",
        captured.borrow()
    );
    // ② 断言：不安全 URL 点击不得再产生 OpenPage 广播（当前 fail-open → 收到
    // "javascript:evil" → RED；修复后 discard → 不再追加 → GREEN）。
    assert!(
        !captured.borrow().iter().any(|p| p == "javascript:evil"),
        "点击不安全 URL 不应广播 OpenPage（fail-open），实际收到：{:?}",
        captured.borrow()
    );
}

