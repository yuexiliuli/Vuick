    /// EPUB 阅读排版端到端：标题文本含注记、正文 `class=text` 首行缩进 2 全角空格、
    /// 落款行右对齐、`[n]` 注记悬停浮现注释卡片。
    #[gpui::test]
    fn epub_note_hover_shows_annotation_card(cx: &mut TestAppContext) {
        use crate::markdown::parse::RowAlign;
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let src = "# 1872年德文版序言[^1]\n\n　　共产主义者同盟[^2]成立于1847年。\n\n%%ALIGN:RIGHT%%卡尔·马克思　弗里德里希·恩格斯\n";
        let mut notes = std::collections::BTreeMap::new();
        notes.insert(1, "这是第一条注释内容。".to_string());
        notes.insert(2, "这是第二条注释内容。".to_string());
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_epub(true, notes, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(700.), px(500.)));
        settle(cx);

        // 行数据语义：标题含注记 [1]；正文首行缩进「　　」且含 [2]；落款右对齐。
        let (heading_ok, body_ok, sig_align) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            let h = md.rows.iter().find(|r| matches!(r.kind, RowKind::Heading(_))).expect("有标题");
            let body = md
                .rows
                .iter()
                .find(|r| r.text.contains("共产主义者同盟"))
                .expect("有正文段");
            let sig = md
                .rows
                .iter()
                .find(|r| r.text.contains("卡尔·马克思"))
                .expect("有落款行");
            (
                h.text.contains("[1]") && h.notes.iter().any(|(_, n)| *n == 1),
                body.text.starts_with("\u{3000}\u{3000}") && body.notes.iter().any(|(_, n)| *n == 2),
                sig.align,
            )
        });
        assert!(heading_ok, "标题应含注记 [1]");
        assert!(body_ok, "正文应首行缩进且含注记 [2]");
        assert_eq!(sig_align, Some(RowAlign::Right), "落款应右对齐");

        // 命中表：paint 后应有注记矩形（文内引用 + 末尾定义都会有命中）。
        let hits: Vec<(usize, bool, gpui::Bounds<gpui::Pixels>)> = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            md.note_ctx
                .hits
                .borrow()
                .iter()
                .map(|h| (h.n, h.is_def, h.bounds))
                .collect()
        });
        assert!(!hits.is_empty(), "应有注记命中, 实际 {hits:?}");
        let (n, _is_def, b) = hits[0];
        let c = b.center();

        // 鼠标悬停到注记 → 悬浮注记命中置位 + 卡片出现。
        cx.simulate_mouse_move(point(px(c.x.as_f32()), px(c.y.as_f32())), None, Modifiers::default());
        settle(cx);
        let hover = cx.update(|_, cx| host.read(cx).md.read(cx).hover_hit.map(|h| h.n));
        assert_eq!(hover, Some(n), "悬停在注记 {n} 上应置 hover_hit");
        assert!(cx.debug_bounds("md-note-card").is_some(), "悬浮注释卡片应出现");

        // 移开注记 → 卡片消失。
        cx.simulate_mouse_move(point(px(10.), px(10.)), None, Modifiers::default());
        settle(cx);
        let hover = cx.update(|_, cx| host.read(cx).md.read(cx).hover_hit.map(|h| h.n));
        assert_eq!(hover, None, "移开注记应清除 hover");
    }

    /// 引用标记**双向跳转**：点击文内 `[n]` → 定位末尾定义 `[!n]` 并加亮；再点定义 → 跳回文内引用。
    #[gpui::test]
    fn click_citation_marker_navigates_both_ways(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        // 全量小文档：标题带文内引用 [^1]，末尾是定义 [1]。
        let src = "# 标题[^1]\n\n正文引用[^1]处。\n\n[!1]　引用一详情。\n";
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(600.), px(400.)));
        settle(cx);

        // 行结构：0=标题([^1]),1=空行,2=正文([^1]),3=空行,4=定义[!1]。
        let (ref1_row, def1_row) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            let ref1 = md
                .rows
                .iter()
                .position(|r| r.notes.iter().any(|(_, n)| *n == 1))
                .expect("有文内引用");
            let def1 = md
                .rows
                .iter()
                .position(|r| r.note_defs.iter().any(|(_, n)| *n == 1))
                .expect("有定义");
            (ref1, def1)
        });
        assert_eq!((ref1_row, def1_row), (0, 4), "文内引用应在标题行、定义在第 4 行");

        // 点击文内引用 [1] → 定位到定义行并加亮定义标记（is_def=true）。
        let ref1_center = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            md.note_ctx
                .hits
                .borrow()
                .iter()
                .find(|h| h.n == 1 && !h.is_def)
                .copied()
                .expect("应有文内引用命中")
                .bounds
                .center()
        });
        cx.simulate_mouse_down(ref1_center, MouseButton::Left, Modifiers::default());
        settle(cx);
        let (hl, sel, def_hit) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            let def_hit = md
                .note_ctx
                .hits
                .borrow()
                .iter()
                .any(|h| h.n == 1 && h.is_def);
            (md.marker_hl, md.selection, def_hit)
        });
        assert_eq!(hl, Some((def1_row, 1, true)), "点文内引用应定位到定义并加亮定义标记");
        assert!(sel.is_none(), "点标记不应进入文本点选");
        assert!(def_hit, "定义行仍在视口内、定义命中仍可查");

        // 反向：点击定义 [1] → 跳回文内引用行并加亮引用标记（is_def=false）。
        let def1_center = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            md.note_ctx
                .hits
                .borrow()
                .iter()
                .find(|h| h.n == 1 && h.is_def)
                .copied()
                .expect("应有定义命中")
                .bounds
                .center()
        });
        cx.simulate_mouse_down(def1_center, MouseButton::Left, Modifiers::default());
        settle(cx);
        let hl = cx.update(|_, cx| host.read(cx).md.read(cx).marker_hl);
        assert_eq!(hl, Some((ref1_row, 1, false)), "点定义应跳回文内引用并加亮引用标记");
    }

    /// 非 EPUB 的普通 markdown：悬浮在引用标记 `[n]` 上也应浮现该引用的详情正文
    /// （取自定义 `[!n]` 行剥掉 `[n]` 标记后的文本，`cit.details`）。
    #[gpui::test]
    fn hover_citation_marker_shows_detail_text_without_epub(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let src = "# 引言[^1]\n\n[!1]　这是引用 1 的详情介绍。\n";
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(600.), px(400.)));
        settle(cx);

        // 悬浮文内引用 [1]（标题行）→ 详情卡片浮现。
        let c = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            md.note_ctx
                .hits
                .borrow()
                .iter()
                .find(|h| h.n == 1 && !h.is_def)
                .copied()
                .expect("引用命中")
                .bounds
                .center()
        });
        cx.simulate_mouse_move(c, None, Modifiers::default());
        settle(cx);
        let (hover, detail) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            (
                md.hover_hit.map(|h| h.n),
                md.cit.details.get(&1).map(|s| s.to_string()),
            )
        });
        assert_eq!(hover, Some(1), "悬停应在引用 1 上");
        assert_eq!(
            detail.as_deref(),
            Some("这是引用 1 的详情介绍。"),
            "非 EPUB 悬浮也应能取到引用详情正文"
        );
        assert!(
            cx.debug_bounds("md-note-card").is_some(),
            "非 EPUB 悬停引用也应出现详情卡片"
        );

        // 移开 → 卡片消失。
        cx.simulate_mouse_move(point(px(10.), px(10.)), None, Modifiers::default());
        settle(cx);
        let hover = cx.update(|_, cx| host.read(cx).md.read(cx).hover_hit.map(|h| h.n));
        assert_eq!(hover, None, "移开应清除悬浮");
        assert!(
            cx.debug_bounds("md-note-card").is_none(),
            "移开引用后详情卡片应消失"
        );
    }

    /// 相邻引述渲染行应**上下紧贴**（无外层纵向外衬）：整段引述呈一整片灰底，
    /// 无白色分割。单个引述文本内部折行由带内 `py_1` 提供行距。
    #[gpui::test]
    fn consecutive_quotes_render_as_one_band(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let src = "> 巴枯宁翻译的《共产党宣言》俄文第一版\n> 现在来看看俄国吧！\n> 卡·马克思　弗·恩格斯";
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(700.), px(500.)));
        settle(cx);

        let (kinds, tops, heights) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            let kinds: Vec<_> = md.rows.iter().map(|r| r.kind).collect();
            let mut tops = Vec::new();
            let mut heights = Vec::new();
            for i in 0..md.rows.len() {
                let b = md.list_state.bounds_for_item(i).expect("行应有 bounds");
                tops.push(b.origin.y.as_f32());
                heights.push(b.size.height.as_f32());
            }
            (kinds, tops, heights)
        });
        assert!(
            kinds.iter().all(|k| *k == RowKind::Quote),
            "三行都应是引述：{kinds:?}"
        );
        // 相邻引述行上下紧贴：row(i+1).top == row(i).top + row(i).height（±0.01 亚像素）。
        for i in 0..tops.len() - 1 {
            let gap = tops[i + 1] - (tops[i] + heights[i]);
            assert!(
                gap.abs() < 0.01,
                "引述行 {i} 与 {i}+1 应无白色分割（gap={gap:.3}px）"
            );
        }
    }

    /// 鼠标拖动选择 + Ctrl/Cmd+C 复制：跨两段正文拖选 → selection 覆盖两行，
    /// 复制文本含两段内容并以换行连接。
    #[gpui::test]
    fn mouse_drag_selects_text_and_copies(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let src = "# 标题\n\n第一段正文内容足够长以便折行测试鼠标选择。\n\n第二段正文内容也足够用来拖选。\n";
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(700.), px(500.)));
        settle(cx);

        // 找两个正文段的行框（0=标题,2=第一段,4=第二段），在其行框内按下并拖到第二段。
        let (b0, b2) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            (
                md.list_state.bounds_for_item(2).expect("第一段 bounds"),
                md.list_state.bounds_for_item(4).expect("第二段 bounds"),
            )
        });
        let p0 = point(px(b0.origin.x.as_f32() + 100.0), px(b0.center().y.as_f32()));
        let p2 = point(px(b2.origin.x.as_f32() + 100.0), px(b2.center().y.as_f32()));
        cx.simulate_mouse_down(p0, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(p2, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(p2, MouseButton::Left, Modifiers::default());
        settle(cx);

        let (sel, text) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            (md.selection, md.selected_text())
        });
        let sel = sel.expect("拖选后应有 selection");
        assert_eq!(sel.0 .0, 2, "选择应从第一段开始（实际 {sel:?}）");
        assert_eq!(sel.1 .0, 4, "选择应到第二段结束（实际 {sel:?}）");
        let text = text.expect("应有选中文本");
        assert!(text.contains("正文内容"), "选中文本应含两段公共子串：{text:?}");
        assert!(text.contains('\n'), "跨行选择应以换行连接：{text:?}");

        // 复制到剪贴板。
        cx.update(|window, cx| {
            md.update(cx, |md, cx| md.copy_selection(window, cx));
        });
        let cb = cx
            .cx
            .read_from_clipboard()
            .and_then(|i| i.text())
            .map(|t| t.to_string());
        let cb = cb.expect("复制后剪贴板应有内容");
        assert!(
            cb.contains("正文内容") && cb.contains('\n'),
            "剪贴板应含两段公共子串并以换行连接：{cb:?}"
        );

        // 点空白处清除选择。
        cx.simulate_mouse_down(point(px(690.), px(10.)), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(
            point(px(690.), px(10.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        settle(cx);
        let sel = cx.update(|_, cx| host.read(cx).md.read(cx).selection);
        assert_eq!(sel, None, "点空白应清除选择");
    }

    #[gpui::test]
    fn non_char_boundary_selection_over_cjk_does_not_panic(cx: &mut TestAppContext) {
        // 回归：选区字节偏移切进多字节字符（CJK `。`）时，旧代码 `push_bg` 直接
        // 用该区间喂 StyledText → shaping 时 run 边界非字符边界 → direct_write
        // panic（byte index not a char boundary）。修复：`char_align_range` 吸收。
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let src = "# 标题\n\n你说得对，好。行不行。\n";
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(700.), px(500.)));
        settle(cx);
        // 注入选区：终点 19 落在 `。`（字节 18..21）中间——旧代码渲染即崩。
        let mid = "你说得对，好。".find('。').unwrap() + 1;
        assert_eq!(mid, 19, "句号起于 18，切半偏移 19");
        cx.update(|window, cx| {
            let md2 = host.read(cx).md.clone();
            md2.update(cx, |md, cx| {
                md.selection = Some(((2, 0), (2, mid)));
                // 复制路径的 `&text[a..b]` 切片也必须防御非字符边界（旧代码在此崩）。
                md.copy_selection(window, cx);
                cx.notify();
            });
        });
        // 渲染带该选区高亮的行 + 复制：均不 panic 即修复生效。
        settle(cx);
        let text = cx.update(|_, cx| host.read(cx).md.read(cx).selected_text());
        assert_eq!(
            text.as_deref(),
            Some("你说得对，好。"),
            "非边界偏移向前对齐到完整字符，且不 panic"
        );
    }

    #[gpui::test]
    fn select_text_works_on_inline_code_row(cx: &mut TestAppContext) {
        // 回归：含行内代码（反引号）的行**可以**鼠标拖选字符。旧实现把 StyledText
        // 移进 RoundedInlineText（st=None），不写文本命中表 → hit_test 找不到该行，
        // 选区建立不了。现统一 TextTrack 始终持有 st，两类行一致可点选。
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let src = "# 标题\n\n我们看看`行内代码`能不能拖选出来。\n\n第二段正文足够用来拖选。\n";
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(700.), px(500.)));
        settle(cx);
        let (b1, b2) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            (
                md.list_state.bounds_for_item(2).expect("含代码段"),
                md.list_state.bounds_for_item(4).expect("第二段"),
            )
        });
        // 混入 ListState cells 引用。
        let p0 = point(px(b1.origin.x.as_f32() + 60.0), px(b1.center().y.as_f32()));
        let p2 = point(px(b2.origin.x.as_f32() + 60.0), px(b2.center().y.as_f32()));
        cx.simulate_mouse_down(p0, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(p2, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(p2, MouseButton::Left, Modifiers::default());
        settle(cx);
        let sel = cx.update(|_, cx| host.read(cx).md.read(cx).selection);
        let sel = sel.expect("含行内代码的行应可拖选");
        assert_eq!(sel.0 .0, 2, "选择应从含代码段开始（实际 {sel:?}）");
        let text = cx.update(|_, cx| host.read(cx).md.read(cx).selected_text());
        assert!(
            text.as_deref().is_some_and(|t| t.contains("代码")),
            "选中文本应含代码段内文字：{text:?}"
        );
    }

