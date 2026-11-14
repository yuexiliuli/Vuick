
// ---------------------------------------------------------------------------
// 视图
// ---------------------------------------------------------------------------
struct Host {
    ed: Entity<CodeEditor>,
}
impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.ed.clone()
    }
}

fn open<'a>(cx: &'a mut TestAppContext, text: &str) -> (Entity<Host>, &'a mut VisualTestContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let ed = cx.new(|cx| CodeEditor::new(cx));
        ed.update(cx, |ed, _| ed.set_text(text));
        Host { ed }
    });
    let cx: &mut VisualTestContext = cx;
    settle(cx);
    (host, cx)
}

fn settle(cx: &mut VisualTestContext) {
    for _ in 0..4 {
        cx.run_until_parked();
        let _ = cx.refresh();
    }
    cx.run_until_parked();
}

fn focus(host: &Entity<Host>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let fh = ed.read(cx).focus_handle();
        window.focus(&fh, cx);
    });
    settle(cx);
}

fn editor_text(host: &Entity<Host>, cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| host.read(cx).ed.read(cx).text_string())
}

fn editor_selection(host: &Entity<Host>, cx: &mut VisualTestContext) -> Selection {
    cx.update(|_, cx| host.read(cx).ed.read(cx).selection())
}

fn window_handle(cx: &mut VisualTestContext) -> gpui::AnyWindowHandle {
    cx.windows().first().copied().expect("有窗口")
}

#[gpui::test]
fn markdown_editor_renders_wysiwyg_styles(cx: &mut TestAppContext) {
    use std::ops::Range;
    use gpui::HighlightStyle;

    // Phase 1：`.md` 编辑区行渲染走「所见即所得」滤镜——记号淡化、正文富样式，
    // 且底层纯文本完全不变（光标/选区/撤销/保存走原路径，无「视觉↔字节」映射）。
    let text = "# 标题\n\n**加粗** `code` [链接](https://x) 与 *斜体*\n- 列表项\n> 引用\n";
    let (host, cx) = open(cx, text);

    // 非 md 语言不受滤镜影响（回归护栏：syntect 路径保持可用）
    let _rust_row = cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.set_language(Some("rust")));
        ed.update(cx, |ed, _| ed.test_line_styles(window, 0))
    });

    // 切到 markdown → 启用滤镜
    let row0 = cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.set_language(Some("markdown")));
        ed.update(cx, |ed, _| ed.test_line_styles(window, 0))
    });
    // 标题行：`#` 记号淡化 + 正文加粗
    assert!(
        row0.iter().any(|(_, s)| s.fade_out.is_some()),
        "标题记号（#）应淡化: {row0:?}"
    );
    assert!(
        row0.iter().any(|(_, s)| s.font_weight == Some(gpui::FontWeight::BOLD)),
        "标题正文应加粗: {row0:?}"
    );

    // 第 3 行：**加粗** `code` [链接](https://x) 与 *斜体*
    let row2 = cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.test_line_styles(window, 2))
    });
    let has = |row: &[(Range<usize>, HighlightStyle)], pred: fn(&HighlightStyle) -> bool| {
        row.iter().any(|(_, s)| pred(s))
    };
    assert!(
        has(&row2, |s| s.font_weight == Some(gpui::FontWeight::BOLD)),
        "应渲染加粗: {row2:?}"
    );
    assert!(
        has(&row2, |s| s.font_style == Some(gpui::FontStyle::Italic)),
        "应渲染斜体: {row2:?}"
    );
    assert!(
        has(&row2, |s| s.background_color.is_some()),
        "行内代码应有底色: {row2:?}"
    );
    assert!(
        has(&row2, |s| s.fade_out.is_some()),
        "记号应淡化: {row2:?}"
    );
    assert!(
        has(&row2, |s| s.underline.is_some()),
        "链接应有下划线: {row2:?}"
    );

    // 底层文本保持不变（所见≠所得底稿仍是纯 Markdown）
    assert_eq!(editor_text(&host, cx), text);
}

/// 括号文本字符按嵌套层级着色（对齐 VS Code bracketPairColorization）：`build_rows`
/// 对含括号行把 `col..col+1` span 覆为 `bracket_guide_palette[depth % 3]`（与引导线
/// 同色池）；无括号行走零分配 Borrowed 分支、产物与纯语法一致。
///
/// 用无语法高亮语言（None）做精确断言：无基色 → 每行样式段恰为 bracket 覆色 span，
/// 可逐字符验深度取色与「邻近非括号字节零分配不变」（空）。
/// 字节列演算（col 皆字节下标）：
/// - 行 0 `fn f() {`：`(`4、`)`5、`{`7（均 d0 → palette[0]）；
/// - 行 1 `    if ok { foo(bar); }`：`{`10（d1）、`(`15/`)`19（d2）、`}`22（d1）
///   （"    if ok { foo(bar); }"：i4..9、{10、f12..15、`(`15、`)`19、`}`22）；
/// - 行 2 `    let n = 1;`：无括号 → 空。
#[gpui::test]
fn bracket_char_colorization_by_depth(cx: &mut TestAppContext) {
    use std::ops::Range;
    use gpui::HighlightStyle;

    let text = "fn f() {\n    if ok { foo(bar); }\n    let n = 1;\n}\n";
    let (host, cx) = open(cx, text);

    let palette = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bracket_guide_palette());
    let bracket_chars = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bracket_chars().to_vec());
    // 括号字符表已随 set_text 同步落地（打开/替换路径 `recompute_fold_brackets_full`），
    // 且按 (line,col) 升序——着色消费的数据源。
    assert!(!bracket_chars.is_empty(), "打开路径应同步落地括号字符表");

    let row = |cx: &mut VisualTestContext, l: usize| -> Vec<(Range<usize>, HighlightStyle)> {
        cx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |ed, _| ed.test_line_styles(window, l))
        })
    };

    // 行 0：单行配对 `(`/`)` 与开块 `{` 均 d0。
    let r0 = row(cx, 0);
    assert_eq!(
        r0,
        vec![
            (4..5, HighlightStyle { color: Some(palette[0]), ..Default::default() }),
            (5..6, HighlightStyle { color: Some(palette[0]), ..Default::default() }),
            (7..8, HighlightStyle { color: Some(palette[0]), ..Default::default() }),
        ],
        "行 0 括号字符应各按 d0 覆 palette[0]: {r0:?}"
    );

    // 行 1：嵌套两层——`{` d1 → palette[1]，`(`/`)` d2 → palette[2]，`}` d1。
    let r1 = row(cx, 1);
    assert_eq!(
        r1,
        vec![
            (10..11, HighlightStyle { color: Some(palette[1]), ..Default::default() }),
            (15..16, HighlightStyle { color: Some(palette[2]), ..Default::default() }),
            (19..20, HighlightStyle { color: Some(palette[2]), ..Default::default() }),
            (22..23, HighlightStyle { color: Some(palette[1]), ..Default::default() }),
        ],
        "行 1 嵌套括号应按 depth 覆 palette[d%3]、相邻非括号字节不变: {r1:?}"
    );

    // 无括号行：零分配 Borrowed 分支 → 空前基色下无任何样式段（与改动前逐位一致）。
    let r2 = row(cx, 2);
    assert!(r2.is_empty(), "无括号行应保持纯语法产物（空）: {r2:?}");
}

#[gpui::test]
fn large_rs_first_frame_colors_visible_area_sync(cx: &mut TestAppContext) {
    use std::ops::Range;
    use gpui::HighlightStyle;

    // 首屏同步上色回归（方案 A+B）：大 `.rs`（≥数百行、顶部含多行块注释 / 原始字符串
    // 跨行构造）打开**首帧**（空缓存、未预热、不 settle）走真实 `build_rows` 路径，可见区
    // **立即**有着色（不再先纯文本、后补色）。多行构造的作用域从文件头整段解析：闭界
    // 后的行不再被误染注释/字符串色（作用域跨行正确闭合，非逐行孤立上色）。第二段：
    // 首帧后下滚（真路径）仍连续上色。
    // 注：本引擎 `parse_line` 按行消费 syntect 的 scope ops，多行作用域的**内部行**（块
    // 注释中间行 / 原始字符串中间行）历来接不到 ops → 为空的既有行为，此处不作断言；
    // 断言的是「闭界后的解析正确」与「首帧可见区整体有色」。
    let mut lines: Vec<String> = vec![
        "// 顶部行注释".into(),
        "/* 块注释开始".into(),
        "   块注释中间行".into(),
        "   块注释结束 */".into(),
        "fn main() {".into(),
        "    // 函数内行注释".into(),
        "    let s = \"普通字符串\";".into(),
        "    let rs = r#\"原始字符串".into(),
        "        第一行".into(),
        "        第三行也是\"#;".into(),
        "    /* 第二个块".into(),
        "        继续 */".into(),
        "    let n: u64 = 1234;".into(),
        "    call();".into(),
        "}".into(),
    ];
    for i in 0..420 {
        lines.push(format!("fn pad{i}() {{ let v: u32 = {i}; // c{i} }}"));
    }
    let text = lines.join("\n");
    let (host, cx) = open(cx, &text);

    // 切到 Rust 语法后**立刻**（无中间渲染/预热/settle）走首帧 `build_rows`。
    let first = cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.set_language(Some("rust")));
        ed.update(cx, |ed, _| ed.test_build_rows_first_frame(window, 0, 20))
    });
    assert!(first.len() >= 21, "首帧应返回可见行（实际 {} 行）", first.len());
    let color_of = |row: &[(Range<usize>, HighlightStyle)]| -> Option<gpui::Hsla> {
        row.iter().find(|(_, s)| s.color.is_some()).and_then(|(_, s)| s.color)
    };
    // 首帧可见区**整体有色**：绝大多数行有着色段（多行作用域内部行除外），不再整帧空白。
    let colored_rows = first.iter().filter(|(_, st)| !st.is_empty()).count();
    assert!(
        colored_rows >= 10,
        "首帧可见区应整体有色（≥10 行，实际 {colored_rows}/{} 行）——首帧不再被延后成空样式",
        first.len()
    );
    // 具体 token 即有着色（真语法解析，非占位）：注释 / 字符串 / 数字各归其色。
    let c_comment = color_of(&first[0].1).expect("顶部 // 行应有注释色");
    let c_string = color_of(&first[6].1).expect("字符串行应有字符串色");
    let c_number = color_of(&first[12].1).expect("数字行应有数字色");
    assert_ne!(c_string, c_comment, "字符串色 ≠ 注释色");
    assert_ne!(c_number, c_comment, "数字色 ≠ 注释色");
    // 多行块注释/原始字符串**闭界后的解析正确**（作用域跨行连续）：闭界后行不得被
    // 误染注释/字符串色——块注释闭界后的 `fn main`、原始字符串闭界后的行均为各自色。
    assert_ne!(
        color_of(&first[4].1),
        Some(c_comment),
        "块注释闭界后（fn main）不得误染注释色"
    );
    assert_ne!(
        color_of(&first[12].1),
        Some(c_comment),
        "第二个块注释闭界后（数字行）不得误染注释色"
    );
    assert_ne!(
        color_of(&first[13].1),
        Some(c_string),
        "原始字符串闭界后（call();）不得误染字符串色"
    );
    // 多行构造起点即入作用域：原始字符串开始行与普通字符串同色。
    assert_eq!(
        color_of(&first[7].1),
        Some(c_string),
        "原始字符串开始行应为字符串色（作用域从开始行接入）"
    );

    // 首帧后下滚（真路径，预热保存点 → `extend_down` 连续续栈）仍每行有色。
    let deep = cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.test_build_rows_snapshot(window, 60, 100))
    });
    assert!(deep.len() >= 41, "下滚窗口应返回行（实际 {} 行）", deep.len());
    for (i, (text, st)) in deep.iter().enumerate() {
        assert!(
            !st.is_empty(),
            "下滚第 {i} 行（60+{i}，{text:?}）应有颜色（extend_down 续栈）"
        );
    }
}

#[gpui::test]
fn font_size_change_keeps_viewport_top_anchored(cx: &mut TestAppContext) {
    // 回归（2026-09-19 用户反馈）：编辑区 Ctrl+滚轮放大/缩小字号时窗口也跟着滚动。
    // 修复 = 行高变化时把滚动目标同比换算（同 PDF 的视口锚定）——视口顶部的内容点
    // 不随行高变化跳走。这里直接改字号覆盖（等价于 Ctrl+滚轮 / Ctrl+=/- 的落地路径），
    // 断言「滚动 px 按行高比缩放、顶部绝对视觉行不变」。
    let text = (1..=300).map(|i| format!("第{i}行")).collect::<Vec<_>>().join("\n");
    let (host, cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    // 普通滚轮（非 Ctrl）滚到中段 → 作为缩放前的视口锚点。
    let b = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    let pos = point(
        px(b.origin.x.as_f32() + b.size.width.as_f32() / 2.0),
        px(b.origin.y.as_f32() + b.size.height.as_f32() / 2.0),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-3000.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(cx);
    let (d1, lh1, f0) = cx.update(|_, cx| {
        let ed = ed.read(cx);
        (ed.test_desired_scroll_px(), ed.test_metrics().1, ed.font_size_px())
    });
    assert!(d1 > 0., "应先滚到中段（d1={d1}）");
    // 放大字号 → 行高变大 → 滚动目标应按行高比缩放（视口顶部内容点锚定）。
    ed.update(cx, |ed, cx| {
        ed.set_font_override(Some(f0 + 6.0));
        cx.notify();
    });
    settle(cx);
    let (d2, lh2) = cx.update(|_, cx| {
        let ed = ed.read(cx);
        (ed.test_desired_scroll_px(), ed.test_metrics().1)
    });
    assert!(lh2 > lh1, "放大后行高应变大：{lh1} → {lh2}");
    assert!(
        (d2 - d1 * (lh2 / lh1)).abs() < lh1 * 0.5,
        "滚动目标应按行高比缩放（锚定视口）：{d1} → {d2}（期望 {:.1}）",
        d1 * (lh2 / lh1)
    );
    let row1 = (d1 / lh1).floor();
    let row2 = (d2 / lh2).floor();
    assert!(
        (row2 - row1).abs() <= 2.0,
        "缩放后顶部所在绝对视觉行应不变：{row1} → {row2}"
    );
    // 缩小（回到略小于初始）→ 反向同样锚定。
    ed.update(cx, |ed, cx| {
        ed.set_font_override(Some(f0 - 2.0));
        cx.notify();
    });
    settle(cx);
    let (d3, lh3) = cx.update(|_, cx| {
        let ed = ed.read(cx);
        (ed.test_desired_scroll_px(), ed.test_metrics().1)
    });
    assert!(lh3 < lh2, "缩小后行高应变小：{lh2} → {lh3}");
    assert!(
        (d3 - d1 * (lh3 / lh1)).abs() < lh1 * 0.5,
        "缩小同样按行高比锚定：{d1} → {d3}（期望 {:.1}）",
        d1 * (lh3 / lh1)
    );
}

#[gpui::test]
fn refresh_metrics_skips_unchanged_font_compute(cx: &mut TestAppContext) {
    // 指纹门控：同样主题连续刷帧不再全量重算（跳过 resolve_font/ch_advance）；
    // 首帧必算一次；用户改字号（指纹变）后下一帧重算一次。
    let (host, cx) = open(cx, "foo\nbar");
    let c1 = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metric_compute_count());
    assert_eq!(c1, 1, "首帧后度量应只全量重算一次（{c1}）");
    // 同样主题连续刷帧：指纹命中，不再走昂贵字体度量重算。
    settle(cx);
    let c2 = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metric_compute_count());
    assert_eq!(c2, c1, "同样主题连续刷帧不应重算字体度量（{c1} → {c2}）");
    // 用户改字号：指纹变 → 下一帧重新全量重算。
    let f0 = cx.update(|_, cx| host.read(cx).ed.read(cx).font_size_px());
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    ed.update(cx, |ed, cx| {
        ed.set_font_override(Some(f0 + 4.0));
        cx.notify();
    });
    settle(cx);
    let c3 = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metric_compute_count());
    assert_eq!(c3, c2 + 1, "字号变化后应再全量重算一次（{c2} → {c3}）");
}

#[gpui::test]
fn editor_bottom_pad_quantized_to_line_height(cx: &mut TestAppContext) {
    // 回归（专家评审 P4）：底部预留（横向滚动条厚 16px + 额外行高）向上量化到
    // line_height 整数倍——否则滚到底视口停在「行间半格」上，最后一行加载/点击
    // 命中/光标出现次像素误差。断言 pad 是行高的整数倍且 ≥ 一行。
    let (host, cx) = open(cx, "row0\nrow1\nrow2");
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1).max(1.);
    let pad = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bottom_pad_px());
    let frac = pad / lh;
    assert!(
        (frac - frac.round()).abs() < 1e-3,
        "底部预留 {pad:.2}px 应为行高 {lh:.2}px 的整数倍，实际 {} 行",
        frac
    );
    assert!(pad >= lh, "底部预留应至少覆盖一行（{pad:.2} ≥ {lh:.2}）");
}

#[gpui::test]
fn editor_renders_list_and_rows(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "第一行\n第二行\n第三行");
    let wrap = cx
        .debug_bounds("ce-list-wrap")
        .expect("编辑器列表容器应渲染");
    assert!(wrap.size.height > px(0.));
    assert!(wrap.size.width > px(0.));
    // 回归（2026-09-15 实机验证）：`list()` 必须 `.size_full()` 撑满容器。
    // 否则 List 走 Infer 测量，高度=内容高，ListState 视口高=0，
    // 整块编辑区不绘制任何行（编辑区空白、看似“无法编辑”）。
    let viewport_h = cx.update(|_, cx| host.read(cx).ed.read(cx).list_viewport_height());
    assert!(
        viewport_h > 0.,
        "list 视口应被撑满（viewport_h={viewport_h}），否则可见行数为 0"
    );
    assert_eq!(editor_text(&host, cx), "第一行\n第二行\n第三行");
}

#[gpui::test]
fn typing_then_undo_redo(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "");
    focus(&host, cx);
    let w = window_handle(cx);
    cx.simulate_keystrokes("abc");
    settle(cx);
    assert_eq!(editor_text(&host, cx), "abc");

    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-z").unwrap());
    settle(cx);
    assert_eq!(editor_text(&host, cx), "");

    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-y").unwrap());
    settle(cx);
    assert_eq!(editor_text(&host, cx), "abc");
}

#[gpui::test]
fn caret_and_backspace(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "ab");
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    ed.update(cx, |ed, _| ed.set_caret_to(1));
    settle(cx);
    let w = window_handle(cx);
    cx.dispatch_keystroke(w, Keystroke::parse("backspace").unwrap());
    settle(cx);
    assert_eq!(editor_text(&host, cx), "b");
    // 回退后光标在 0
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.head(), 0);
}

#[gpui::test]
fn select_all_then_type_replaces(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "hello");
    focus(&host, cx);
    let w = window_handle(cx);
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-a").unwrap());
    settle(cx);
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.range(), 0..5);
    cx.simulate_keystrokes("x");
    settle(cx);
    assert_eq!(editor_text(&host, cx), "x");
}

#[gpui::test]
fn mouse_click_places_caret_on_second_row(cx: &mut TestAppContext) {
    let text = "row0\nrow1\nrow2";
    let (host, cx) = open(cx, text);
    // 取行高，点击第 2 行文本区
    let lh = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_metrics().1
    });
    cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let fh = ed.read(cx).focus_handle();
        window.focus(&fh, cx);
    });
    settle(cx);
    let y = px(lh * 1.5); // 第 2 行中部
    let x = px(4.); // gutter 内 → 列 0
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(cx);
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.head(), "row0\n".len(), "应落到第 2 行行首");
}

#[gpui::test]
fn fold_block_heuristic_detects_indent_blocks(cx: &mut TestAppContext) {
    let text = "fn foo() {\n    a();\n    b();\n}\nfn bar() {\n    c();\n}";
    let (host, cx) = open(cx, text);
    let ends = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        // 0 行块：缩进 0，直到同缩进 "}" → [0,3)；4 行块（单行体）→ [4,6)。
        (ed.test_fold_block_end(0), ed.test_fold_block_end(4))
    });
    assert_eq!(ends, (Some(3), Some(6)));
    // 文件末行无后续块 → 不可折叠。
    let none = cx.update(|_, cx| host.read(cx).ed.read(cx).test_fold_block_end(6));
    assert_eq!(none, None);
}

#[gpui::test]
fn fold_reduces_scroll_range_and_expand_restores(cx: &mut TestAppContext) {
    let text = (0..20).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let (host, cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(5, 10)));
    settle(cx);
    let folded = cx.update(|_, cx| host.read(cx).ed.read(cx).test_scroll_item_count());
    // 折叠 [5,10) 隐藏 4 行 → 量程条目数应相对全文减少。
    assert!(folded < 20 + 8, "折叠后量程条目数应缩小（folded={folded}）");
    // 再次 toggle（同为 start）→ 展开恢复。
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(5, 10)));
    settle(cx);
    let expanded = cx.update(|_, cx| host.read(cx).ed.read(cx).test_scroll_item_count());
    assert!(expanded > folded, "展开后量程条目数应恢复（{folded} → {expanded}）");
}

#[gpui::test]
fn click_maps_through_fold_to_logical_line(cx: &mut TestAppContext) {
    // 20 行折叠 [5,10)（隐藏 6..9）。视觉行 7 = 逻辑行 11：点击应命中 11 行行首。
    let text = (0..20).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let (host, cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(5, 10)));
    settle(cx);
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1).max(1.);
    let y = px(lh * 7.5); // 视觉行 7
    let x = px(4.); // 行号列内 → 列 0
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(cx);
    let head = editor_selection(&host, cx).head();
    let expected = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(11, 0));
    assert_eq!(head, expected, "点击视觉行 7 应命中折叠后的逻辑行 11（{head} vs {expected}）");
}

#[gpui::test]
fn gutter_click_toggles_active_fold(cx: &mut TestAppContext) {
    let text = (0..20).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let (host, cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(5, 10)));
    settle(cx);
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1).max(1.);
    // 点击折叠首行（视觉行 5）gutter → 展开。
    let y = px(lh * 5.5);
    let x = px(2.);
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(cx);
    assert!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().is_empty()),
        "点击折叠首行 gutter 应展开折叠"
    );
}

#[gpui::test]
fn typing_inside_folded_body_expands_fold(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "aaa\nbbb\nccc\nddd");
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(1, 3))); // 隐藏行 2
    settle(cx);
    // 把光标直接放进隐藏行 2 中间，键入 → 自动展开并写入。
    let off = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(2, 1));
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.set_caret_to(off)));
    focus(&host, cx);
    cx.simulate_keystrokes("X");
    settle(cx);
    assert!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().is_empty()),
        "编辑折叠体内应自动展开折叠"
    );
    // 行 0="aaa" 行 1="bbb" 行 2="ccc"（被折叠隐藏）行 3="ddd"：在行 2 第 1 列插 X。
    assert_eq!(editor_text(&host, cx), "aaa\nbbb\ncXcc\nddd");
}

#[gpui::test]
fn ctrl_shift_bracket_folds_and_unfolds(cx: &mut TestAppContext) {
    let text = "fn foo() {\n    a();\n    b();\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    let w = window_handle(cx);
    // 光标在 0 行：Ctrl+Shift+[ → 折叠 [0,3)。
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-shift-[").unwrap());
    settle(cx);
    let folds = cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().to_vec());
    assert_eq!(folds, vec![Fold { start: 0, end: 3 }]);
    // Ctrl+Shift+] → 展开。
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-shift-]").unwrap());
    settle(cx);
    assert!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().is_empty()),
        "Ctrl+Shift+] 应展开折叠"
    );
}

#[gpui::test]
fn ruler_x_computed_from_metric_and_moves_with_hscroll(cx: &mut TestAppContext) {
    // 长行文档让横向可视范围/滚动条可用。
    let text = "x".repeat(300);
    let (host, cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (cw, _, gutter) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    // 测试窗 1920 宽 > 80 列标尺位置 → 标尺在可视区。
    let avail_w = cx.update(|_, cx| host.read(cx).ed.read(cx).test_content_w_px());
    let x0 = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_ruler_x(80, avail_w))
        .expect("80 列标尺应在可视区");
    assert!(
        (x0 - (gutter + 80.0 * cw)).abs() < 1.0,
        "标尺 x 应 = gutter + col*char_width（x0={x0}）"
    );
    // 横向滚动 ≥5 格 → 标尺随内容左移约 300px。
    let ed_b = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + 200.),
        px(ed_b.origin.y.as_f32() + 50.),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(300.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(cx);
    let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(hscroll > 0., "应真正横向滚动（hscroll={hscroll}）");
    let x1 = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_ruler_x(80, avail_w))
        .expect("80 列标尺滚动后仍应在可视区");
    assert!(
        (x1 - (x0 - hscroll)).abs() < 1.0,
        "标尺应随内容左移 hscroll（x0={x0} x1={x1} hscroll={hscroll}）"
    );
}

#[gpui::test]
fn fold_candidates_detected_for_indented_blocks(cx: &mut TestAppContext) {
    // 打开即自动探测：内层 if 块 + 外层函数块（可嵌套）。
    let text = "fn a() {\n    if x {\n        y();\n    }\n    z();\n}";
    let (host, cx) = open(cx, text);
    let cands = cx.update(|_, cx| host.read(cx).ed.read(cx).test_fold_candidates().to_vec());
    // 落地点按 `start` 升序（`candidate_starting_at` 二分查询依赖排序）；集合不变。
    assert_eq!(cands, vec![Fold { start: 0, end: 5 }, Fold { start: 1, end: 3 }]);
    // gutter 指示器口径：候选起点行有标记。
    let marks = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        (
            ed.test_has_fold_or_candidate(0),
            ed.test_has_fold_or_candidate(1),
            ed.test_has_fold_or_candidate(2),
        )
    });
    assert_eq!(marks, (true, true, false));
}

#[gpui::test]
fn gutter_click_on_candidate_creates_fold(cx: &mut TestAppContext) {
    // 候选已自动探测但未激活：点击 candidate 起点行的 gutter → 创建激活折叠。
    let text = "fn a() {\n    if x {\n        y();\n    }\n    z();\n}";
    let (host, cx) = open(cx, text);
    // 确认尚未激活。
    assert!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().is_empty()),
        "打开时不应有激活折叠"
    );
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1).max(1.);
    // 视觉行 1 = 逻辑行 1（if 块候选）。
    let y = px(lh * 1.5);
    let x = px(2.);
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(cx);
    let folds = cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().to_vec());
    assert_eq!(folds, vec![Fold { start: 1, end: 3 }], "点击候选起点应创建激活折叠");
}

#[gpui::test]
fn fold_arrow_state_tracks_candidate_vs_folded(cx: &mut TestAppContext) {
    // 折叠箭头状态 = 渲染/测试共用的单一权威（`fold_arrow_state`）：
    // 候选起点 → Some(false)「▾」；点击折叠（test_toggle_fold_at 与 gutter 点击同一
    // 路径 `toggle_fold_at`）成激活折叠 → Some(true)「▸」；普通行 → None 不画箭头。
    let text = "fn a() {\n    if x {\n        y();\n    }\n    z();\n}";
    let (host, cx) = open(cx, text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).test_fold_arrow_state(0)),
        Some(false),
        "候选起点：下向「▾」"
    );
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).test_fold_arrow_state(1)),
        Some(false),
        "候选起点：下向「▾」"
    );
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).test_fold_arrow_state(2)),
        None,
        "普通行：不画箭头"
    );
    // 折叠 if 块候选 → 激活折叠 → Some(true)「▸」；外层 fn 仍是候选「▾」。
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_toggle_fold_at(1)));
    settle(cx);
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).test_fold_arrow_state(1)),
        Some(true),
        "已折叠首行：右向「▸」"
    );
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).test_fold_arrow_state(0)),
        Some(false),
        "外层候选不受激活折叠影响：仍「▾」"
    );
    // 再次 toggle（同为 start）→ 展开 → 回落为候选「▾」。
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_toggle_fold_at(1)));
    settle(cx);
    assert_eq!(
        cx.update(|_, cx| ed.read(cx).test_fold_arrow_state(1)),
        Some(false),
        "展开后回落为候选「▾」"
    );
}

#[gpui::test]
fn gutter_fold_arrow_renders_right_of_line_number(cx: &mut TestAppContext) {
    // 折叠/候选行渲染 `ce-fold-arrow`，且箭头**字形**在固定槽内（槽恒存在、位于
    // 行号右侧：槽左缘 = 数字格右缘 = 行号锚点）。行号锚点可测信号 = 槽左缘：
    //   = gutter 右缘 − 右内边距(6) − 槽宽(14)（FOLD_ARROW_SLOT_PX，见 view/mod.rs）。
    let text = "fn a() {\n    x();\n}";
    let (host, cx) = open(cx, text);
    // 打开即有候选起点（未折叠）→ 箭头在槽内右侧。
    let gutter = cx.debug_bounds("ce-gutter").expect("行号列应渲染");
    let slot = cx.debug_bounds("ce-gutter-arrow-slot").expect("箭头槽应恒存在");
    let arrow = cx.debug_bounds("ce-fold-arrow").expect("候选行应渲染折叠箭头");
    let gutter_right = gutter.origin.x.as_f32() + gutter.size.width.as_f32();
    let slot_left = slot.origin.x.as_f32();
    let slot_right = slot_left + slot.size.width.as_f32();
    assert!(
        (slot_left - (gutter_right - 6.0 - 14.0)).abs() <= 1.0,
        "槽左缘（行号锚点）应 = gutter 右缘 − 右内边距 − 槽宽（{slot_left} vs {}）",
        gutter_right - 6.0 - 14.0
    );
    // 箭头字形右对齐在槽内：左缘 ≥ 槽左缘、右缘 ≤ 槽右缘。
    assert!(
        arrow.origin.x.as_f32() + 0.5 >= slot_left,
        "箭头应位于行号右侧的槽内（箭头左缘 {} ≥ 槽左缘 {slot_left}）",
        arrow.origin.x.as_f32()
    );
    assert!(
        arrow.origin.x.as_f32() + arrow.size.width.as_f32() <= slot_right + 0.5,
        "箭头不应越出槽右缘"
    );
    assert!(
        slot_right <= gutter_right + 0.5,
        "箭头槽不应越出 gutter（宽度口径未含槽会被 overflow_hidden 裁掉）"
    );
    // 折叠候选 → 激活折叠：箭头仍在（方向「▸/▾」由 fold_arrow_state 保证）。
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_toggle_fold_at(0)));
    settle(cx);
    assert!(
        cx.debug_bounds("ce-fold-arrow").is_some(),
        "已折叠行应仍渲染折叠箭头"
    );
}

#[gpui::test]
fn gutter_fold_arrow_keeps_number_anchor(cx: &mut TestAppContext) {
    // 回归锁（用户真机：行号被箭头左推、带/不带箭头行错位）：折叠切换前后
    // 数字右缘锚点（= 箭头槽左缘）必须纹丝不动（±1px）。修复前 `justify_end`
    // 把「行号+箭头」整组右对齐 → 有箭头行数字被左推（实测 diff 10px）。
    let text = "fn a() {\n    x();\n}";
    let (host, cx) = open(cx, text);
    let anchor0 = cx
        .debug_bounds("ce-gutter-arrow-slot")
        .expect("箭头槽应恒存在")
        .origin
        .x
        .as_f32();
    let num0 = cx.debug_bounds("ce-gutter-number").expect("数字格应渲染");
    let num_right0 = num0.origin.x.as_f32() + num0.size.width.as_f32();
    assert!(
        (num_right0 - anchor0).abs() <= 1.0,
        "数字右缘应钉在槽左缘（{num_right0} vs {anchor0}）"
    );
    // 折叠候选 → 激活折叠（箭头仍在，方向从「▾」→「▸」）。
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_toggle_fold_at(0)));
    settle(cx);
    let anchor1 = cx
        .debug_bounds("ce-gutter-arrow-slot")
        .expect("箭头槽仍恒存在")
        .origin
        .x
        .as_f32();
    let num1 = cx.debug_bounds("ce-gutter-number").expect("数字格仍渲染");
    let num_right1 = num1.origin.x.as_f32() + num1.size.width.as_f32();
    assert!(
        (anchor1 - anchor0).abs() <= 1.0,
        "折叠切换前后行号锚点应不变（{anchor0} → {anchor1}）"
    );
    assert!(
        (num_right1 - num_right0).abs() <= 1.0,
        "折叠切换前后数字右缘应不变（{num_right0} → {num_right1}）"
    );
}

#[gpui::test]
fn gutter_fold_arrow_absent_on_plain_rows(cx: &mut TestAppContext) {
    // 无折叠候选、无激活折叠的普通文档：不渲染任何折叠箭头（槽仍占位但空着）。
    let (_host, cx) = open(cx, "aaa\nbbb\nccc");
    assert!(
        cx.debug_bounds("ce-fold-arrow").is_none(),
        "无可折叠行时不应渲染折叠箭头"
    );
}

#[gpui::test]
fn folded_row_highlight_bg_tracks_fold_state(cx: &mut TestAppContext) {
    // 已折叠行的整行背景 `ce-fold-row-bg`：折叠激活后出现在折叠首行；候选（未
    // 折叠）行与普通行都没有。用 test_toggle_fold_at 走「候选未折叠 → 折叠 →
    // 展开」三态，断言背景随折叠出现/消失。
    let text = "fn a() {\n    if x {\n        y();\n    }\n    z();\n}";
    let (host, cx) = open(cx, text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    // 候选未折叠：候选行（line 0/1）与普通行都不渲染折叠背景。
    assert!(
        cx.debug_bounds("ce-fold-row-bg").is_none(),
        "候选未折叠时不渲染折叠行背景"
    );
    // 折叠 if 块候选（line 1）→ 激活折叠 → 折叠首行出现背景，且铺满整行（高 = 行高）。
    // `test_toggle_fold_at` 不比真机 gutter 点击（聚焦自带 notify）——显式 notify 让
    // 下一帧按折叠态重建行元素（与 `fold_arrow_state` 渲染/测试同口径）。
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_toggle_fold_at(1)));
    cx.update(|_, cx| ed.update(cx, |_, cx| cx.notify()));
    settle(cx);
    let bg = cx
        .debug_bounds("ce-fold-row-bg")
        .expect("已折叠首行应渲染整行背景");
    let lh = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1)
        .max(1.);
    assert!(
        (bg.size.height.as_f32() - lh).abs() < 1.0,
        "背景应铺满整行（高 {} vs 行高 {lh}）",
        bg.size.height.as_f32()
    );
    // 再次 toggle（同 start）→ 展开 → 背景消失。
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_toggle_fold_at(1)));
    cx.update(|_, cx| ed.update(cx, |_, cx| cx.notify()));
    settle(cx);
    assert!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().is_empty()),
        "再次 toggle 后折叠应已展开"
    );
    assert!(
        cx.debug_bounds("ce-fold-row-bg").is_none(),
        "展开后折叠背景应消失"
    );
}

#[gpui::test]
fn fold_row_bg_fainter_than_selection_bg_both_themes(cx: &mut TestAppContext) {
    // 选区底 = **半透明补偿色**（rgba 钉死 + 叠编辑区底复合验证 = 目标取色值）；
    // 折叠/光标行底 = 不透明 token（暗 #172931/#242525、亮 #E6EFF8×2）。
    // 「折叠/光标行比选区更贴背景」的亮度/luminance 语义按**选区复合目标色**比较
    // （半透明 fill 自身的 luminance 不代表实际观感——像素观感是 alpha×fill 叠底）。
    let (host, cx) = open(cx, "a\nb");
    let read_bg = |host: &Entity<Host>, cx: &mut VisualTestContext| -> (gpui::Hsla, gpui::Hsla, gpui::Hsla) {
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_sel_and_fold_bg())
    };
    // WCAG 相对 luminance（线性化 sRGB 三通道加权）。
    let luminance = |c: &gpui::Hsla| -> f32 {
        let c: gpui::Rgba = (*c).into();
        let lin = |v: f32| {
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
    };
    // 同一 WCAG 公式，输入为 (r,g,b) 三元组（合成后颜色用）。
    let luminance_rgb = |(r, g, b): (f32, f32, f32)| -> f32 {
        let lin = |v: f32| {
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
    };
    // alpha×fill + (1-alpha)×bg 的 RGB 合成（0..1 线性）。
    let composite_rgb = |c: &gpui::Hsla, bg: &gpui::Hsla| -> (f32, f32, f32) {
        let c: gpui::Rgba = (*c).into();
        let bg: gpui::Rgba = (*bg).into();
        let a = c.a;
        (
            a * c.r + (1.0 - a) * bg.r,
            a * c.g + (1.0 - a) * bg.g,
            a * c.b + (1.0 - a) * bg.b,
        )
    };
    let near = |v: f32, t: f32| (v - t).abs() < 0.01;
    // 折叠/光标行 vs 选区**复合目标**：dev(行底) < dev(选区目标，相对内容底)。
    let assert_fainter_than_sel =
        |sel: &gpui::Hsla, c: &gpui::Hsla, bg: &gpui::Hsla, mode: &str| {
            let sel_dev = (luminance_rgb(composite_rgb(sel, bg)) - luminance(bg)).abs();
            let c_dev = (luminance(c) - luminance(bg)).abs();
            assert!(
                c_dev < sel_dev,
                "{mode}：该底应比选区复合目标更贴背景（dev {c_dev} < {sel_dev}）"
            );
        };
    // 未初始化主题默认 Light：精确色值 + 半透明 + 复合目标验证。
    let (l_sel, l_fold, l_active) = read_bg(&host, cx);
    assert_eq!(l_sel, gpui::rgba(0x619bd373).into(), "亮：选区底 = rgba(0x619BD373)");
    assert!(l_sel.a < 1.0, "亮：选区底应半透明（竖线穿选区透出）");
    assert_eq!(l_fold, gpui::rgb(0xe6eff8).into(), "亮：折叠底 = #E6EFF8");
    assert_eq!(
        l_active, l_fold,
        "亮：光标行底 = 折叠行底（#E6EFF8 同色）"
    );
    // 亮：rgba(0x619BD373) 叠 #FFFFFF 应复合出目标 #B8D2EB=(184,210,235)。
    let (lr, lg, lb) = composite_rgb(&l_sel, &gpui::rgb(0xffffff).into());
    assert!(
        near(lr, 184.0 / 255.0) && near(lg, 210.0 / 255.0) && near(lb, 235.0 / 255.0),
        "亮：半透明选区复合应=#B8D2EB（{lr}/{lg}/{lb}）"
    );
    assert_fainter_than_sel(&l_sel, &l_fold, &gpui::rgb(0xffffff).into(), "亮底折叠");
    // 切暗 → 重渲染后读回：精确色值 + 半透明 + 复合目标验证（暗色光标行 ≠ 折叠行）。
    cx.update(|_, cx| {
        ui::theme::Theme::set(ui::theme::ThemeMode::Dark, cx)
    });
    settle(cx);
    let (d_sel, d_fold, d_active) = read_bg(&host, cx);
    assert_eq!(d_sel, gpui::rgba(0x3ab5e973).into(), "暗：选区底 = rgba(0x3AB5E973)");
    assert!(d_sel.a < 1.0, "暗：选区底应半透明（竖线穿选区透出）");
    assert_eq!(d_fold, gpui::rgb(0x172931).into(), "暗：折叠底 = #172931");
    assert_eq!(d_active, gpui::rgb(0x242525).into(), "暗：光标行底 = #242525");
    assert_ne!(d_active, d_fold, "暗：光标行 ≠ 折叠行（两色已区分）");
    // 暗：rgba(0x3AB5E973) 叠 #121314 应复合出目标 #245C74=(36,92,116)。
    let (dr, dg, db) = composite_rgb(&d_sel, &gpui::rgb(0x121314).into());
    assert!(
        near(dr, 36.0 / 255.0) && near(dg, 92.0 / 255.0) && near(db, 116.0 / 255.0),
        "暗：半透明选区复合应=#245C74（{dr}/{dg}/{db}）"
    );
    assert_fainter_than_sel(&d_sel, &d_fold, &gpui::rgb(0x121314).into(), "暗底折叠");
    assert_fainter_than_sel(&d_sel, &d_active, &gpui::rgb(0x121314).into(), "暗底光标行");
}

#[gpui::test]
fn active_line_bg_highlights_cursor_row_when_focused(cx: &mut TestAppContext) {
    // 新增「光标所在行整行高亮」：编辑器持焦且 head 落行内 → 该行铺 `ce-active-line-bg`。
    // 未聚焦不渲染；聚焦后默认光标（head=0）在首行 → 首行渲染且高=行高；光标移到另一行
    // → 高亮转移到新行（y 随行移动）。active 按帧现算、不受光标闪烁影响。
    let (host, cx) = open(cx, "aaa\nbbb\nccc\nddd\neee");
    assert!(
        cx.debug_bounds("ce-active-line-bg").is_none(),
        "未聚焦时不应渲染光标行高亮"
    );
    let lh = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1)
        .max(1.);
    // 聚焦 → 默认光标在首行 → 首行出现高亮，铺满整行（高=行高）。
    focus(&host, cx);
    let bg = cx
        .debug_bounds("ce-active-line-bg")
        .expect("持焦时光标行应渲染整行高亮");
    assert!(
        (bg.size.height.as_f32() - lh).abs() < 1.0,
        "高亮应铺满整行（高 {} vs 行高 {lh}）",
        bg.size.height.as_f32()
    );
    assert!(
        (bg.origin.y.as_f32() - 0.0).abs() < 1.0,
        "默认光标在首行：高亮 y 应=首行顶（{}）",
        bg.origin.y.as_f32()
    );
    // 光标移到第 3 行（行索引 2，0 基）→ 高亮转移到该行（y ≈ 2×行高），首行不再高亮。
    let off2 = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(2, 0));
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, cx| ed.set_selection_range(off2..off2, cx));
    });
    settle(cx);
    let bg2 = cx
        .debug_bounds("ce-active-line-bg")
        .expect("光标移到第 3 行后该行应高亮");
    assert!(
        (bg2.origin.y.as_f32() - 2.0 * lh).abs() < 1.0,
        "高亮应转移到新光标行（y {} vs 期望 {}）",
        bg2.origin.y.as_f32(),
        2.0 * lh
    );
}

#[gpui::test]
fn active_line_bg_highlights_cursor_row_number_gutter(cx: &mut TestAppContext) {
    // 光标行整行高亮**延伸到行号列**：行号单元格随行高亮（复用既有行高亮底色，
    // 见 `gutter_items_for` 的 fold/active 两档判定）。未聚焦不渲染；聚焦后默认光标
    // 在首行 → 首行行号格高亮（y≈0、高≈行高）；光标移到第 3 行 → 高亮转移到该行
    // （y≈2×行高）；存在非坍缩选区（文字被选中）时整行高亮让位 → 行号格高亮消失。
    let (host, cx) = open(cx, "aaa\nbbb\nccc\nddd\neee");
    assert!(
        cx.debug_bounds("ce-gutter-active-bg").is_none(),
        "未聚焦时行号列不应渲染光标行高亮"
    );
    let (_, lh, gw) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    // 聚焦 → 默认光标在首行 → 首行行号格出现高亮，高=行高、y=首行顶、宽=整行号列宽。
    focus(&host, cx);
    let bg = cx
        .debug_bounds("ce-gutter-active-bg")
        .expect("持焦后首行行号格应高亮");
    assert!(
        (bg.size.height.as_f32() - lh).abs() < 1.0,
        "行号格高亮高应=行高（{} vs 行高 {lh}）",
        bg.size.height.as_f32()
    );
    assert!(
        (bg.origin.y.as_f32() - 0.0).abs() < 1.0,
        "默认光标在首行：行号格高亮 y 应=首行顶（{}）",
        bg.origin.y.as_f32()
    );
    assert!(
        (bg.size.width.as_f32() - gw).abs() < 1.0,
        "行号格高亮宽应=整行号列宽（{} vs 行号列宽 {gw}）——宽≈0 时背景不可见",
        bg.size.width.as_f32()
    );
    // 行号**数字本身**高亮（强调色）：数字格与单元格高亮同位置同尺寸（首行 y≈0）。
    let num = cx
        .debug_bounds("ce-gutter-number-active")
        .expect("持焦后活动行行号数字应挂高亮 selector");
    assert!(
        (num.origin.y.as_f32() - bg.origin.y.as_f32()).abs() < 1.0
            && (num.size.height.as_f32() - bg.size.height.as_f32()).abs() < 1.0,
        "活动行数字应与单元格高亮同 y/高（num y {} 高 {}；bg y {} 高 {}）",
        num.origin.y.as_f32(),
        num.size.height.as_f32(),
        bg.origin.y.as_f32(),
        bg.size.height.as_f32()
    );
    assert!(
        (num.origin.y.as_f32() - 0.0).abs() < 1.0,
        "默认光标在首行：活动行数字 y 应=首行顶（{}）",
        num.origin.y.as_f32()
    );
    // 光标移到第 3 行（行索引 2，0 基）→ 行号列高亮转移到该行（y ≈ 2×行高）。
    let off2 = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(2, 0));
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, cx| ed.set_selection_range(off2..off2, cx));
    });
    settle(cx);
    let bg2 = cx
        .debug_bounds("ce-gutter-active-bg")
        .expect("光标移到第 3 行后该行号格应高亮");
    assert!(
        (bg2.origin.y.as_f32() - 2.0 * lh).abs() < 1.0,
        "行号格高亮应转移到新光标行（y {} vs 期望 {}）",
        bg2.origin.y.as_f32(),
        2.0 * lh
    );
    let num2 = cx
        .debug_bounds("ce-gutter-number-active")
        .expect("光标移到第 3 行后活动行数字应跟随");
    assert!(
        (num2.origin.y.as_f32() - 2.0 * lh).abs() < 1.0,
        "活动行数字应转移到新光标行（y {} vs 期望 {}）",
        num2.origin.y.as_f32(),
        2.0 * lh
    );
    // 选中文字（非坍缩选区）→ 整行高亮让位给选中字符 → 行号格高亮消失。
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, cx| ed.set_selection_range(0..3, cx));
    });
    settle(cx);
    assert!(
        cx.debug_bounds("ce-gutter-active-bg").is_none(),
        "存在非坍缩选区时行号列不应渲染光标行高亮"
    );
    assert!(
        cx.debug_bounds("ce-gutter-number-active").is_none(),
        "存在非坍缩选区时活动行数字高亮应消失"
    );
}

#[gpui::test]
fn active_line_bg_requires_focus(cx: &mut TestAppContext) {
    // 光标行整行高亮**强依赖持焦**：未持焦时移动光标（notify + settle 重建行元素）
    // 不产生任何高亮；持焦瞬间该高亮才出现在光标所在行。这从状态层锁死「失焦 →
    // 背景不出现/消失」语义（`active = focused && head 落行内`，见 `build_rows`；
    // 真正的 `on_blur` 事件派发在 gpui 测试环境不触发，故用「未持焦 + 移动光标」
    // 直接断言 `focused=false` 分支——光标行判定与焦点态解耦，二者都各自可测）。
    let text = "aaa\nbbb\nccc";
    let (host, cx) = open(cx, text);
    // 未持焦：把光标移到第 2 行并重建 → 仍无高亮（focused=false 直接短路）。
    let off1 = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(1, 0));
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, cx| ed.set_selection_range(off1..off1, cx));
    });
    settle(cx);
    assert!(
        cx.debug_bounds("ce-active-line-bg").is_none(),
        "未持焦时光标所在行不得渲染高亮"
    );
    // 聚焦 → 光标（head）所在第 2 行立即出现高亮，且高度=行高、y 落在该行。
    let lh = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1)
        .max(1.);
    focus(&host, cx);
    let bg = cx
        .debug_bounds("ce-active-line-bg")
        .expect("持焦后光标所在行应渲染整行高亮");
    assert!(
        (bg.size.height.as_f32() - lh).abs() < 1.0,
        "高亮应铺满整行（高 {} vs 行高 {lh}）",
        bg.size.height.as_f32()
    );
    assert!(
        (bg.origin.y.as_f32() - lh).abs() < 1.0,
        "高亮应在光标所在第 2 行（y {} vs 期望 {lh}）",
        bg.origin.y.as_f32()
    );
}

#[gpui::test]
fn active_line_bg_yields_to_folded_row_bg(cx: &mut TestAppContext) {
    // 光标所在行恰为已折叠首行：`ce-fold-row-bg` 优先、`ce-active-line-bg` 不渲染
    // （折叠优先、互斥，见 `row_element` 的 `rd.folded.is_none()` 门槛）。
    let text = "fn a() {\n    if x {\n        y();\n    }\n    z();\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    // 默认光标在首行（未折叠行）→ active 高亮正常出现（基线正常态）。
    assert!(
        cx.debug_bounds("ce-active-line-bg").is_some(),
        "聚焦后未折叠行应先渲染光标行高亮"
    );
    // 光标移到 if 块首行（逻辑行 1）并折叠它 → 该行显示折叠底、不显示 active 底（互斥）。
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, cx| ed.set_selection_range(ed.offset_for_line_col(1, 0)..ed.offset_for_line_col(1, 0), cx));
    });
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.test_toggle_fold_at(1));
    });
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |_, cx| cx.notify());
    });
    settle(cx);
    assert!(
        cx.debug_bounds("ce-fold-row-bg").is_some(),
        "已折叠首行应渲染折叠整行底"
    );
    assert!(
        cx.debug_bounds("ce-active-line-bg").is_none(),
        "光标落在已折叠首行时不应渲染 active 高亮（折叠底优先）"
    );
}

#[gpui::test]
fn active_line_bg_gutter_yields_to_folded_row_bg(cx: &mut TestAppContext) {
    // 折叠优先互斥同样延伸到行号列：光标所在行恰为已折叠首行时，行号格格底显示
    // `ce-gutter-fold-bg`、不显示 `ce-gutter-active-bg`（镜像文本侧
    // `active_line_bg_yields_to_folded_row_bg` 的断言骨架）。
    let text = "fn a() {\n    if x {\n        y();\n    }\n    z();\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    // 默认光标在首行（未折叠行）→ 行号格 active 高亮正常出现（基线正常态）。
    assert!(
        cx.debug_bounds("ce-gutter-active-bg").is_some(),
        "聚焦后未折叠行应渲染行号格光标行高亮"
    );
    assert!(
        cx.debug_bounds("ce-gutter-fold-bg").is_none(),
        "未折叠行不应渲染折叠行号格底"
    );
    // 光标移到 if 块首行（逻辑行 1）并折叠它 → 该行号格显示折叠底、不显示 active 底。
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, cx| ed.set_selection_range(ed.offset_for_line_col(1, 0)..ed.offset_for_line_col(1, 0), cx));
    });
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.test_toggle_fold_at(1));
    });
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |_, cx| cx.notify());
    });
    settle(cx);
    assert!(
        cx.debug_bounds("ce-gutter-fold-bg").is_some(),
        "已折叠首行号格应渲染折叠底"
    );
    assert!(
        cx.debug_bounds("ce-gutter-active-bg").is_none(),
        "光标落在已折叠首行时行号格不应渲染 active 高亮（折叠底优先）"
    );
    assert!(
        cx.debug_bounds("ce-gutter-number-active").is_none(),
        "已折叠首行的行号数字应保持灰显（不挂强调 selector）"
    );
}

#[gpui::test]
fn active_line_bg_suppressed_while_text_selection(cx: &mut TestAppContext) {
    // 核心修复：存在**非坍缩选区**（文字被选中）时光标行整行高亮必须消失，只高亮
    // 选中的字符本身（`build_rows` 的 `active` 加 `selection.is_collapsed()` 守卫）；
    // 选区坍缩回单点光标 → 光标行整行高亮出现。用 `set_selection_range`（同文件
    // 既有手法）先后设非坍缩选区（跨 3 字符）与坍缩光标。
    let text = "aaaa\nbbbb\ncccc";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    let lh = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1)
        .max(1.);
    // 基线：坍缩光标（head=0 首行）→ 光标行整行高亮出现。
    assert!(
        cx.debug_bounds("ce-active-line-bg").is_some(),
        "基线：坍缩光标时光标行应整行高亮"
    );
    // 非坍缩选区：选中首行 0..3 字符（anchor=0 / head=3）→ 整行高亮消失。
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, cx| ed.set_selection_range(0..3, cx));
    });
    settle(cx);
    assert!(
        cx.debug_bounds("ce-active-line-bg").is_none(),
        "选中文字（非坍缩选区）时不应再渲染光标行整行高亮"
    );
    // 选区坍缩回单点光标（head=2，仍在首行）→ 光标行整行高亮重新出现。
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, cx| ed.set_selection_range(2..2, cx));
    });
    settle(cx);
    let bg = cx
        .debug_bounds("ce-active-line-bg")
        .expect("选区坍缩回光标后该行应重新整行高亮");
    assert!(
        (bg.size.height.as_f32() - lh).abs() < 1.0,
        "高亮应铺满整行（高 {} vs 行高 {lh}）",
        bg.size.height.as_f32()
    );
}

#[gpui::test]
fn selection_bg_semi_transparent_keeps_guides_visible(cx: &mut TestAppContext) {
    // 选区底 = **半透明补偿色**：存在非坍缩选区（文字被选中）时，选区像素叠编辑区
    // 底仍复合出目标取色值，但 alpha<1 → 引导竖线（叠层在文字/选区层之下）能透出，
    // 不再被不透明纯色盖死。缺省 Off（`BRACKET_GUIDES_MODE`，2026-11-14 用户真机口径）
    // 下括对彩线默认关、缩进引导线全量回显 → 可见竖线 = 缩进引导线（`ce-indent-guide`，
    // 同样位于文字/选区层之下），用同帧存在性佐证 z 序契约。
    let text = "fn a() {\n    x();\n}";
    let (host, cx) = open(cx, text);
    assert!(
        cx.debug_bounds("ce-indent-guides").is_some(),
        "前置：缩进引导线叠层应渲染"
    );
    let read_sel_alpha = |host: &Entity<Host>, cx: &mut VisualTestContext| -> f32 {
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_sel_and_fold_bg().0.a)
    };
    let set_noncollapsed = |host: &Entity<Host>, cx: &mut VisualTestContext| {
        // 非坍缩选区：选中首行 0..3 字符（选中文字，不等于光标单点）。
        cx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |ed, cx| ed.set_selection_range(0..3, cx));
        });
        settle(cx);
    };
    // 亮：半透明 + 竖线同帧仍在。
    set_noncollapsed(&host, cx);
    let l_a = read_sel_alpha(&host, cx);
    assert!(
        (0.0..1.0).contains(&l_a),
        "亮：选区底应半透明（a={l_a} ∈ (0,1)）"
    );
    assert!(
        cx.debug_bounds("ce-indent-guide").is_some(),
        "亮：选中文字帧缩进引导竖线仍存在（半透明透出契约）"
    );
    // 暗：同样。
    cx.update(|_, cx| ui::theme::Theme::set(ui::theme::ThemeMode::Dark, cx));
    settle(cx);
    set_noncollapsed(&host, cx);
    let d_a = read_sel_alpha(&host, cx);
    assert!(
        (0.0..1.0).contains(&d_a),
        "暗：选区底应半透明（a={d_a} ∈ (0,1)）"
    );
    assert!(
        cx.debug_bounds("ce-indent-guide").is_some(),
        "暗：选中文字帧缩进引导竖线仍存在（半透明透出契约）"
    );
}

#[gpui::test]
fn guides_stay_visible_over_fold_and_active_rows(cx: &mut TestAppContext) {
    // 行底色拆成独立叠层（ruler < row_bg < guides < harea）后：引导竖线在折叠行
    // /光标行的整行底色上仍清晰可见——同帧两者共存、且几何上有 y 重叠（竖线不再被
    // 不透明整行底色吞掉）。缺省 Off（2026-11-14 用户真机口径）下括对彩线默认关、
    // 缩进引导线全量回显 → 可见竖线 = 缩进引导线（`ce-indent-guide`，位于 row_bg 之上
    // 的 guides 层）；代理与重叠断言走 `test_indent_guide_segments_flagged`
    // （`debug_bounds` 不编码 z 序，故用「两者同帧都在 + 竖线段与底色带 y 区间重叠」锁定
    // 该回归）。
    let text = "fn a() {\n    if x {\n        y();\n    }\n    z();\n}";
    let (host, cx) = open(cx, text);
    let lh = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1)
        .max(1.);
    // 任一缩进引导线段的 y 区间与该行底色带 [band.0, band.1) 有交集。
    let guides_overlap =
        |host: &Entity<Host>, cx: &mut VisualTestContext, band: (f32, f32)| -> bool {
            cx.update(|_, cx| {
                let ed = host.read(cx).ed.clone();
                ed.read(cx)
                    .test_indent_guide_segments_flagged(0, 0.0, lh, 1.0, 1200.0, 1000.0, None)
                    .into_iter()
                    .any(|(_, y, h, _)| y < band.1 && y + h > band.0)
            })
        };
    // Phase A：折叠 if 块首行（line 1，未聚焦）→ 折叠首行铺整行底色；外层 fn a 的
    // 缩进引导线仍画 → 底色与竖线同帧共存且 y 重叠。
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.test_toggle_fold_at(1));
    });
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |_, cx| cx.notify());
    });
    settle(cx);
    let fold_bg = cx
        .debug_bounds("ce-fold-row-bg")
        .expect("已折叠首行应渲染整行底色");
    assert!(
        (fold_bg.size.height.as_f32() - lh).abs() < 1.0,
        "折叠底色高应=行高（{} vs {lh}）",
        fold_bg.size.height.as_f32()
    );
    assert!(
        cx.debug_bounds("ce-indent-guide").is_some(),
        "折叠帧同帧仍应渲染缩进引导竖线"
    );
    let fold_band = (
        fold_bg.origin.y.as_f32(),
        fold_bg.origin.y.as_f32() + fold_bg.size.height.as_f32(),
    );
    assert!(
        guides_overlap(&host, cx, fold_band),
        "缩进引导竖线段应跨过折叠行底色带 {fold_band:?}"
    );
    // Phase B：展开折叠、聚焦并放光标到 line 1（非折叠行）→ 光标行整行高亮与竖线
    // 共存（同样不被吞）。
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.test_toggle_fold_at(1));
    });
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |_, cx| cx.notify());
    });
    settle(cx);
    focus(&host, cx);
    let off1 = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(1, 0));
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, cx| ed.set_selection_range(off1..off1, cx));
    });
    settle(cx);
    let active = cx
        .debug_bounds("ce-active-line-bg")
        .expect("持焦光标行应渲染整行高亮");
    assert!(
        (active.size.height.as_f32() - lh).abs() < 1.0,
        "光标行高亮高应=行高（{} vs {lh}）",
        active.size.height.as_f32()
    );
    assert!(
        cx.debug_bounds("ce-indent-guide").is_some(),
        "光标行帧同帧仍应渲染缩进引导竖线"
    );
    let active_band = (
        active.origin.y.as_f32(),
        active.origin.y.as_f32() + active.size.height.as_f32(),
    );
    assert!(
        guides_overlap(&host, cx, active_band),
        "bracket 竖线段应跨过光标行高亮带 {active_band:?}"
    );
}

#[gpui::test]
fn ruler_infra_renders_but_defaults_off(cx: &mut TestAppContext) {
    // margin guide 默认关闭（EDITOR_RULER_COLUMNS 为空）：叠层容器仍渲染、无竖线子元素；
    // 坐标换算（第 80 列在可视区内应给 Some(x)）保持精确，供用户配置恢复使用。
    let text = "x".repeat(300);
    let (host, cx) = open(cx, &text);
    let (cw, _, gutter) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let avail_w = cx.update(|_, cx| host.read(cx).ed.read(cx).test_content_w_px());
    let x = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_ruler_x(80, avail_w))
        .expect("第 80 列在可视区内应可通过单纯换算得到 x");
    assert!((x - (gutter + 80.0 * cw)).abs() < 1.0, "标尺换算 = gutter + col*char_width");
    assert!(
        cx.debug_bounds("ce-rulers").is_some(),
        "标尺叠层容器仍应在渲染树中"
    );
    // 默认列数为空 → 不渲染任何 ce-ruler-line（margin guide 默认隐藏）。
    assert!(
        cx.debug_bounds("ce-ruler-line").is_none(),
        "默认不渲染第 80 列竖线（用户要的是括号标尺）"
    );
}

#[gpui::test]
fn bracket_guides_detected_and_rendered(cx: &mut TestAppContext) {
    // 用户示例：三对跨行花括号 → 三条括号标尺竖线。配对集合（`bracket_pairs`）保留断言；
    // 「渲染存在性」按历史规范 docs/bracket-guide-redesign.md §8.4 改指**新的默认主源叠层**
    // `ce-indent-guides`（bracket 竖线已降级为默认关的可选叠加，其纯函数段测试在别处单独保留）。
    let text = "impl RowData {\n    fn empty() -> Self {\n        Self {\n            a: 1,\n        }\n    }\n}";
    let (host, cx) = open(cx, text);
    let pairs = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bracket_pairs().to_vec());
    // 落地点按 `open_line` 升序（标尺视口二分依赖排序）；配对集合不变；depth = 嵌套 DFS 深度。
    assert_eq!(
        pairs,
        vec![
            crate::editor::fold::BracketPair { open_line: 0, open_col: 13, close_line: 6, close_col: 0, guide_units: 0, guide_body_min_units: Some(4), depth: 0 },
            crate::editor::fold::BracketPair { open_line: 1, open_col: 23, close_line: 5, close_col: 4, guide_units: 4, guide_body_min_units: Some(8), depth: 1 },
            crate::editor::fold::BracketPair { open_line: 2, open_col: 13, close_line: 4, close_col: 8, guide_units: 8, guide_body_min_units: Some(12), depth: 2 },
        ]
    );
    // 缩进引导线叠层确实渲染进树（默认视觉主源）。
    assert!(
        cx.debug_bounds("ce-indent-guides").is_some(),
        "缩进引导线叠层应默认渲染"
    );
}

#[gpui::test]
fn bracket_guide_window_keeps_enclosing_pair_when_viewport_deep(cx: &mut TestAppContext) {
    // 标尺窗口化（Sprint 4a，`BRACKET_GUIDE_WINDOW_LOOKBACK_LINES`）回归：
    // 视口已滚到某外层块的**深处**（open_line 在视口顶上方、但仍在回看窗口内）时，
    // 该块的竖线仍须画出——竖线贯穿视口（高层包装块/长函数体）。若窗口下界误用
    // 视口顶（而非 视口顶−回看），深滚时外层竖线会整段漏画。
    use config::limits::BRACKET_GUIDE_WINDOW_LOOKBACK_LINES as LOOKBACK;
    let mut text = String::new();
    text.push_str("fn outer() {\n"); // line 0
    text.push_str("    println!(\"head\");\n"); // line 1..9
    for _ in 2..10 {
        text.push_str("    \n");
    }
    text.push_str("    {\n"); // line 10
    for _ in 11..40 {
        text.push_str("    \n");
    }
    text.push_str("    }\n"); // line 40
    text.push_str("}\n"); // line 41
    let (host, cx) = open(cx, &text);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    // 段几何行距 = 吸附行距 pitch（sf=1.0 时与生产同式 rhtz 正值变体），非裸 lh。
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    let top_vis = 20; // 视口在两层块中间（外层 open=0 上方远处、仍回看窗口内）。
    assert!(
        top_vis < LOOKBACK,
        "本用例的前置：20 < 回看 {LOOKBACK}"
    );
    // 视口内可见竖线段：外层 fn（x≈0）从 21 到底、内层 {（x≈4）从 21 到 39。
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(
            top_vis, 0.0, lh, 1.0, 1200.0, 1000.0, config::layout::BracketGuidesMode::All,
        )
    });
    let outer = segs
        .iter()
        .find(|(x, _, h)| (*x - 0.0 * cw).abs() < 1.0 && *h > (40 - top_vis - 1) as f32 * pitch)
        .expect("视口深处仍应有外层 fn 竖线（贯穿视口）");
    assert!(
        outer.1 < pitch * 2.,
        "外层竖线应贴视口顶（从 open 之后的深处横贯）：y={}",
        outer.1
    );
    let inner = segs
        .iter()
        .find(|(x, _, h)| (*x - 4.0 * cw).abs() < 1.0 && *h > 15.)
        .expect("应有内层 { 竖线");
    // 内层 { 首覆盖行恰在视口顶（ct=top_vis）→ 顶部恰接触该行上缘（y=0，head=0
    // 不越界），线在视口内可见（h>0）。
    assert!(
        inner.1 >= 0. && inner.2 > 0.,
        "内层竖线应在视口内可见：{inner:?}"
    );
}

#[gpui::test]
fn bracket_guide_segments_span_each_pair(cx: &mut TestAppContext) {
    use config::layout::{
        BRACKET_GUIDE_CLOSE_TAIL_LINES as TAIL, BRACKET_GUIDE_OPEN_HEAD_LINES as HEAD,
    };
    // 与用户样例同构。注意 spans：2026-11-14 第 4 片对齐 VSCode——竖线从**开行**含起，末行
    // 按「闭括号前有无文本」决定含/不含（本例三条闭行都是行首独占 `}` → 不含闭行）。
    let text = "impl RowData {\n    fn empty() -> Self {\n        Self {\n            a: 1,\n        }\n    }\n}";
    let (host, cx) = open(cx, text);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    // 段几何行距 = 吸附行距 pitch（sf=1.0 时与生产同式 rhtz 正值变体），非裸 lh。
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    let harea_w = 1200.0;
    let view_h = 1000.0;
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(
            0, 0.0, lh, 1.0, harea_w, view_h, config::layout::BracketGuidesMode::All,
        )
    });
    // 三条竖线：x = 块首行缩进列 0/4/8；跨度 [开行, 闭行)（闭行独占 `}` 不含闭行）。
    assert_eq!(segs.len(), 3, "三对跨行花括号 → 三条竖线");
    // 外层 impl 块：x≈0cw，y=(0+HEAD)pitch 起、高 (6−HEAD+TAIL)pitch（行 0..5 + 尾巴）。
    let impl_guide = segs
        .iter()
        .find(|(_, y, h)| {
            (*y - (0.0 + HEAD) * pitch).abs() < 0.5
                && (*h - (6.0 - HEAD + TAIL) * pitch).abs() < 0.5
        })
        .expect("应有 impl 块竖线");
    assert!(
        (impl_guide.0 - 0.0 * cw).abs() < 1.0,
        "外层竖线应在块首列 0×字宽，实际 {}",
        impl_guide.0
    );
    // Self 块：x≈8cw，y=(2+HEAD)pitch 起、高 (2−HEAD+TAIL)pitch（行 2..3 + 尾巴）。
    let self_guide = segs
        .iter()
        .find(|(_, y, h)| {
            (*y - (2.0 + HEAD) * pitch).abs() < 0.5
                && (*h - (2.0 - HEAD + TAIL) * pitch).abs() < 0.5
        })
        .expect("应有 Self 块竖线");
    assert!(
        (self_guide.0 - 8.0 * cw).abs() < 1.0,
        "内层竖线应在块首列 8×字宽，实际 {}",
        self_guide.0
    );
}

#[gpui::test]
fn bracket_guide_overlapping_same_column_merged(cx: &mut TestAppContext) {
    use config::layout::{
        BRACKET_GUIDE_CLOSE_TAIL_LINES as TAIL, BRACKET_GUIDE_OPEN_HEAD_LINES as HEAD,
    };
    // 同列（缩进 0）嵌套的两个块：外层 fn a 与内层 `{` 同列、**异 depth**——2026-11-14
    // 起仅同 depth 合并（VSCode 按 depth%3 取色，异 depth 各成段），故此处应画两根。
    let text = "fn a() {\n{\n    x();\n}\n}";
    let (host, cx) = open(cx, text);
    let (cw, lh, _) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    // 段几何行距 = 吸附行距 pitch（sf=1.0 时与生产同式 rhtz 正值变体），非裸 lh。
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(
            0, 0.0, lh, 1.0, 1200.0, 1000.0, config::layout::BracketGuidesMode::All,
        )
    });
    // 两根（fn a 的 [0,4) 与内层 `{` 的 [1,3)）同列（x=0）、异 depth → 各自成段；同列
    // 不合并的语义由本用例锁死（depth 合并见 colored/depth 用例）。
    assert_eq!(segs.len(), 2, "同列异 depth 竖线应各自成段（不合并）");
    // 外层 depth0：行 0..3（+ 端点半行微调；闭 `}` 独占 → 不含闭行）。
    let outer = segs
        .iter()
        .find(|(x, y, h)| {
            (x / cw).round() as i64 == 0
                && (*y - (0.0 + HEAD) * pitch).abs() < 0.5
                && (*h - (4.0 - HEAD + TAIL) * pitch).abs() < 0.5
        })
        .expect("应有外层 fn a 竖线（[0,4) + 端点半行）");
    assert!((outer.0 - 0.0 * cw).abs() < 1.0, "外层竖线应在列 0");
    // 内层 depth1：行 1..2（+ 端点半行微调；闭 `}` 独占 → 不含闭行）。
    let inner = segs
        .iter()
        .find(|(x, y, h)| {
            (x / cw).round() as i64 == 0
                && (*y - (1.0 + HEAD) * pitch).abs() < 0.5
                && (*h - (2.0 - HEAD + TAIL) * pitch).abs() < 0.5
        })
        .expect("应有内层 `{` 竖线（[1,3) + 端点半行）");
    assert!(
        (inner.0 - 0.0 * cw).abs() < 1.0,
        "内层竖线也应在列 0（同列异 depth 各自成段）"
    );
}

/// 复现 `target/guidefix.rs`（gitignored 夹具，故意搞乱括号）前三处用户反馈——**C4 定案后的
/// 预期断言**（2026-11-14 重写：原「现状快照」断言旧 LIFO 的 col8 伪标尺，按设计稿 §6 取反）：
/// mega 8 开不配对 -> col8 消失；ifx1 配对 -> col4 出现；z1 前是 col4 真标尺。夹具抄自该文件
/// 第 1-16 行（行内多括号 mega 行、错缩进、未配对全保留，行号对准）；第 17 行起的合法函数独立
/// 配对、不会再闭合本区域残留栈，故截断到 16 行与本文件整篇在本区域逐位一致。
#[gpui::test]
fn bracket_guide_repro_guidefix_fixture(cx: &mut TestAppContext) {
    // 夹具与 `guidefix_fixture_text()` 逐字节同（该 16 行内联 == 共享常量，直接用共享源）。
    let text = guidefix_fixture_text();
    let (host, cx) = open(cx, text);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    // 段几何行距 = 吸附行距 pitch（sf=1.0 时与生产同式 rhtz 正值变体），非裸 lh。
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    // 全部配对（17 行起的合法块独立配对，故视口内配对 open_line 只可能 ≤15）。
    let pairs = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bracket_pairs().to_vec());
    let near: Vec<_> = pairs.iter().filter(|p| p.open_line <= 15).copied().collect();
    eprintln!("REPRO pairs (open_line open_col close_line guide_units):");
    for p in &near {
        eprintln!("  {p:?}");
    }
    // 实际画出的竖线段（HEAD=TAIL=0、sf=1、hscroll=0 → col=round(x/cw)，行区间可反解）。
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(
            0, 0.0, lh, 1.0, 2000.0, 4000.0, config::layout::BracketGuidesMode::All,
        )
    });
    eprintln!("REPRO segments (col, rows0idx=[ct,cb), rows1idx=[ct+1,cb]):");
    let fmt = |x: f32, y: f32, h: f32| {
        let col = (x / cw).round() as i64;
        let ct = (y / pitch).round() as i64;
        let cb = ct + (h / pitch).round() as i64;
        format!("col={col} rows0idx=[{ct},{cb}) → 1idx=[{},{}]", ct + 1, cb)
    };
    for (x, y, h) in &segs {
        eprintln!("  {}", fmt(*x, *y, *h));
    }

    // C1：line4(mega) 与 line15(t2;) 之间「不该出现」的标尺（col8 合并段）**整体消失**——
    // C4 下 mega 8 开（缩进 8 层）永不配对。
    let spurious = segs.iter().any(|(x, _, _)| (x / cw).round() as i64 == 8);
    eprintln!("C1 spurious col8 1idx[5,15): present={spurious}");
    assert!(
        !spurious,
        "C4：line4 mega 行 8 个未闭 `{{` 永不配对 → col8 伪标尺整体消失"
    );

    // C2：line3 `if x1 == 1 {`（缩进 4）与 line13 缩进 4 的 `}` 配对 → col4 标尺出现，
    // 始于 1idx 行 4（行3..5 之间确有标尺，贯穿后续）。
    let has_ifx1_pair = near.iter().any(|p| p.open_line == 2 && p.guide_units == 4);
    let col4_in_gap = segs.iter().any(|(x, y, h)| {
        let col = (x / cw).round() as i64;
        let ct = (y / pitch).round() as i64;
        let cb = ct + (h / pitch).round() as i64;
        col == 4 && ct <= 3 && 3 < cb // 覆盖 0idx 行 3（=1idx 4，行3..5 之间）
    });
    eprintln!(
        "C2 if-x1 pair open_line=2: present={has_ifx1_pair}; col4 guide at 1idx 行4: present={col4_in_gap}"
    );
    assert!(has_ifx1_pair, "C4：if x1 的 `{{` 应与缩进 4 层 `}}` 配对");
    assert!(col4_in_gap, "C4：row3..5 之间应有 col4 竖线（始于行 4）");

    // C3：line13 z1 前面是 col4 真标尺，且无 col8 伪标尺。
    let col8_covers_13 = segs.iter().any(|(x, y, h)| {
        let col = (x / cw).round() as i64;
        let ct = (y / pitch).round() as i64;
        let cb = ct + (h / pitch).round() as i64;
        col == 8 && ct <= 12 && 12 < cb // 覆盖 0idx 行 12（=1idx 13）
    });
    let col4_covers_13 = segs.iter().any(|(x, y, h)| {
        let col = (x / cw).round() as i64;
        let ct = (y / pitch).round() as i64;
        let cb = ct + (h / pitch).round() as i64;
        col == 4 && ct <= 12 && 12 < cb
    });
    eprintln!("C3 row13 z1 前面：col8 伪标尺={col8_covers_13}, col4 真标尺={col4_covers_13}");
    assert!(!col8_covers_13, "C4：row13 z1 前不应再有 col8 伪标尺");
    assert!(col4_covers_13, "C4：row13 z1 前应有 col4 真标尺");
}

/// C4 新测试共用的 fixture 文本（`target/guidefix.rs` 前 16 行，1-16 行；mega 行/错缩进/
/// 未配对全保留，行号对齐）。
fn guidefix_fixture_text() -> &'static str {
    "\
fn a() {
    let x = 1;
    if x1 == 1 {
        t1();fn a() {y();fn a() {y();fn a() {y();fn a() {y();fn a() {y();fn a() {y();fn a() {y();fn a() {
    let x2 = 1;
    if x == 1 {
        y();
        z();
    }
    w();
}

        z1();
    }
    t2();
}
"
}

/// `target/guidefix2.rs` 的真实文本（braced vs chained 双态，含中文注释；与
/// `indent_guides_active_open_requires_block_opener` 同源，块体模型对拍/真值共用）。
fn guidefix2_fixture_text() -> &'static str {
    r#"impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 启动首帧墙钟：首次 render 时补记 firstframe 段并打印 [STARTUP] 汇总
        // （GPUI_EDITOR_STARTUP_DIAG=1 门控；未开启单次 var_os 检查即返回）。
        base::startup::summarize();
        let theme = Theme::global(cx);
        // 与原组件库 Root 一致：以正文缺省字号为 rem 基准（本项目视图全走 px，供保险）。
        window.set_rem_size(theme.typography.default_body_size);
        div()
            .id("ui-root")
            .debug_selector(|| "ui-root".into())
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_action_tab))
            .on_action(cx.listener(Self::on_action_tab_prev))
            .relative()
            .size_full()
            // 根层不铺不透明底色，最终底色由 EditorApp 的 Mica 半透明窗口底提供。
            .bg(gpui::transparent_black())
            .text_color(theme.colors.fg)
            .child(self.view.clone())
    }
}"#
}

/// C4 视图三 case 之一：mega 8 开不配对 → col8 伪标尺整体消失；ifx1 配对 → col4 段出现。
/// 段集 = col0 [1idx2,10) + col4 [1idx4,13)；行 15/16（1idx）无任何段。
#[gpui::test]
fn guidefix_col8_fake_gone_col4_present(cx: &mut TestAppContext) {
    let text = guidefix_fixture_text();
    let (host, cx) = open(cx, text);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(
            0, 0.0, lh, 1.0, 2000.0, 4000.0, config::layout::BracketGuidesMode::All,
        )
    });
    // 归一化 (列, 0idx 顶部行 ct, 0idx 开区间底 cb)。
    let norm: Vec<(i64, i64, i64)> = segs
        .iter()
        .map(|(x, y, h)| {
            (
                (x / cw).round() as i64,
                (y / pitch).round() as i64,
                ((y + h) / pitch).round() as i64,
            )
        })
        .collect();
    // ① col8 完全消失（mega 8 开永不配对）。
    assert!(!norm.iter().any(|(c, _, _)| *c == 8), "col8 伪标尺应整体消失: {norm:?}");
    // ② col0（fn a）＝0idx[0,10) ＝ 1idx[1,10)（2026-11-14 第 4 片：含开行，闭 `}` 独占
    // → 不含闭行）。
    assert!(
        norm.iter().any(|(c, ct, cb)| *c == 0 && *ct == 0 && *cb == 10),
        "应有 col0 段 1idx[1,10): {norm:?}"
    );
    // ③ col4（ifx1）＝0idx[2,13) ＝ 1idx[3,13)（含开行，闭 `}` 独占 → 不含闭行）。
    assert!(
        norm.iter().any(|(c, ct, cb)| *c == 4 && *ct == 2 && *cb == 13),
        "应有 col4 段 1idx[3,13): {norm:?}"
    );
    // 行 15/16（1idx，0idx 14/15）无任何段：两段 cb≤13 <14。
    assert!(
        !norm.iter().any(|(_, _, cb)| *cb > 13),
        "行 15/16 不应有段经过: {norm:?}"
    );
}

/// C4 列计算：体内「首非空白为 `} ) ]`」的行被 inner_min 排除——否则该 `}` 行（缩进 0）
/// 会把 inner_min 拖到 0、c 被拖到 1。断言 C 块竖线画在 4 列而非 1 列。
#[gpui::test]
fn column_min_inner_excludes_closer(cx: &mut TestAppContext) {
    // A=(0,4) 缩进0；B=(1,2) 缩进4；C=(3,6) 缩进4，体内行 4 是缩进 0 的孤立 `}`。
    let text = "\
fn a() {
    {
    }
    if x {
}
    y();
    }
}
";
    let (host, cx) = open(cx, text);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(
            0, 0.0, lh, 1.0, 1200.0, 1000.0, config::layout::BracketGuidesMode::All,
        )
    });
    let cols: Vec<i64> = segs.iter().map(|(x, _, _)| (x / cw).round() as i64).collect();
    assert!(cols.contains(&4), "C 块（体内含右花括号行）竖线应在 4 列: {cols:?}");
    assert!(!cols.contains(&1), "体内右花括号行不应把列拖到 1: {cols:?}");
}

#[gpui::test]
fn guide_click_highlights_pair_and_elsewhere_clears(cx: &mut TestAppContext) {
    // 语义（用户确认，块体模型 2026-11-14 用户实测）：点体内某行 / 开行 → active 高亮 =
    // 该行关联**最内层**括号块的**体**（由光标派生、随光标跟随）；点非块内行（如顶层）→
    // 无高亮。命中断言：行 3 ∈ Self 块体 [3,4)、行 2 = Self 开行 → 同为 Self 块体；
    // 行 7 = 顶层 fn 行 → None。
    // 列演算：Self 体 = [open+1, close) = [3,4)；c = min(开列 units=13, 闭列 units=8,
    // 体内 min 非闭缩进+1 = 12+1=13) = 8 → group = (8, 3, 4)。
    let text = "impl RowData {\n    fn empty() -> Self {\n        Self {\n            a: 1,\n        }\n    }\n}\nfn other() {}";
    let (host, cx) = open(cx, text);
    let (lh, gutter) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.read(cx).test_metrics();
        (ed.1.max(1.), ed.2)
    });
    let mods = Modifiers::default();
    // 点体内（行 3 中部，x 取 gutter+10px → 文本区）。
    let y = px(lh * 3.5);
    let x = px(gutter + 10.);
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    // 点亮 Self 块体。
    let lit = cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_guide_group());
    assert_eq!(lit, Some((8, 3, 4)), "点行 3 应点亮 Self 块体 (col8, [3,4))");
    // 点开行（行 2 = `Self {` 开行）→ 同样点亮 Self 块体（不是外层 fn）。
    let y2 = px(lh * 2.5);
    cx.simulate_mouse_move(point(x, y2), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y2), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y2), MouseButton::Left, mods);
    let lit = cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_guide_group());
    assert_eq!(lit, Some((8, 3, 4)), "点开行 2 应点亮 Self 块体 (col8, [3,4))，非外层 fn");
    // 点顶层行 7 → 无高亮。
    let y7 = px(lh * 7.5);
    cx.simulate_mouse_move(point(x, y7), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y7), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y7), MouseButton::Left, mods);
    let lit = cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_guide_group());
    assert_eq!(lit, None, "点顶层行 7 应无高亮");
}

// ---------------------------------------------------------------------------
// §8 缩进引导线（indentation guides）为视觉主源：主源算法 / 空行续线 / 折叠 /
// 横滚 / active 列 / 叠层默认关
// ---------------------------------------------------------------------------

/// §8.2 预期表（is=4，行号 1 基，c1=0-based 列0、c5=0-based 列4）逐行画列集：
/// 1..16 → 空 / c1 / c1 / c1+c5 / c1 / c1 / c1+c5 / c1+c5 / c1 / c1 / 空 /
/// 空行续 c1 / c1+c5(孤线) / c1 / c1 / 空。段化后按 (列=round(x/cw), 1idx 行区间)
/// 反解每 1idx 行的画列集，与预期表逐行对表。
#[gpui::test]
fn indent_guides_guidefix_table(cx: &mut TestAppContext) {
    let text = guidefix_fixture_text();
    let (host, cx) = open(cx, text);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_indent_guide_segments(0, 0.0, lh, 1.0, 2000.0, 4000.0)
    });
    // 1idx 行 → 画列（0-based col）：由段的 0idx 行区间 [ct, cb) 反解。
    let mut rows: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
    for (x, y, h) in &segs {
        let col = (x / cw).round() as i64;
        let ct = (y / pitch).round() as i64;
        let cb = ((y + h) / pitch).round() as i64;
        for r in ct..cb {
            let r1 = (r + 1) as usize;
            rows.entry(r1).or_default().push(col as usize);
        }
    }
    // 预期（1idx 行 → 0-based 画列集，升序）；`‑` 表示该行无任何引导线。
    let expected: &[(usize, &[usize])] = &[
        (1, &[]),
        (2, &[0]),
        (3, &[0]),
        (4, &[0, 4]),
        (5, &[0]),
        (6, &[0]),
        (7, &[0, 4]),
        (8, &[0, 4]),
        (9, &[0]),
        (10, &[0]),
        (11, &[]),
        (12, &[0]),
        (13, &[0, 4]),
        (14, &[0]),
        (15, &[0]),
        (16, &[]),
    ];
    for (r1, want) in expected {
        let got = rows.get(r1).cloned().unwrap_or_default();
        assert_eq!(got.as_slice(), *want, "1idx 行 {r1} 画列与 §8.2 预期表不符");
    }
    // 无列行（1/11/16）与 has-col 行的总集 = 全 16 行（无漏）；无列行不在 `rows` 内是
    // 正常（该行没有引导线），由上一循环缺省空集断言兜底。
    let covered: std::collections::BTreeSet<usize> = expected.iter().map(|(r, _)| *r).collect();
    for r1 in &covered {
        assert!(
            rows.contains_key(r1) || expected.iter().any(|(r, c)| r == r1 && c.is_empty()),
            "1idx 行 {r1} 应对表覆盖（有列或显式空）"
        );
    }
}

/// §8.2 空行续线：fixture 空行（0idx 11 = 1idx 12）按可视序 A=上一非空(0idx10 `}` indent0)、
/// B=下一非空(0idx12 z1 indent8)、A<B → 1+⌊0/4⌋=1 → 只续 **c1（col0）**；c5 不续。
/// 且该 col0 段把空行两侧贯通（行 11..16(0idx) 之间连续标尺）。
#[gpui::test]
fn indent_guides_blank_continuation(cx: &mut TestAppContext) {
    let text = guidefix_fixture_text();
    let (host, cx) = open(cx, text);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_indent_guide_segments(0, 0.0, lh, 1.0, 2000.0, 4000.0)
    });
    // 空行（1idx 12 = 0idx 11）被某段 col0 覆盖（跨空行续线）。
    assert!(
        segs.iter().any(|(x, y, h)| {
            let col = (x / cw).round() as i64;
            let ct = (y / pitch).round() as i64;
            let cb = ((y + h) / pitch).round() as i64;
            col == 0 && ct <= 11 && 11 < cb
        }),
        "空行 1idx 12 应被 col0 续线覆盖: {segs:?}"
    );
    // 空行不产 c5（A=0,B=8 → 级数 1 → 只 c1）：无 col4 段经过 0idx 11。
    assert!(
        !segs.iter().any(|(x, y, h)| {
            let col = (x / cw).round() as i64;
            let ct = (y / pitch).round() as i64;
            let cb = ((y + h) / pitch).round() as i64;
            col == 4 && ct <= 11 && 11 < cb
        }),
        "空行只续 c1，不应给 c5: {segs:?}"
    );
    // 贯通：col0 存在覆盖 0idx [10,15) 的段（空行把 11..16 前后连通）。
    assert!(
        segs.iter().any(|(x, y, h)| {
            let col = (x / cw).round() as i64;
            let ct = (y / pitch).round() as i64;
            let cb = ((y + h) / pitch).round() as i64;
            col == 0 && ct <= 11 && 15 <= cb
        }),
        "col0 应贯通空行两侧（0idx 11..16）: {segs:?}"
    );
}

/// 评审 NIT guard：长纯空白段（≥500 连续空行）的空行邻居取值在改为「上一游标 + 下一
/// 惰性预取（只前进不回头）」后，逐行画列须与「无界扫 `(0..v).rev()` / `(v+1)..vcount`」
/// 参考**逐位一致**——证明复杂度优化（O(V×N)→O(V)）不改行为。
#[gpui::test]
fn indent_guides_blank_continuation_long_blank_run_matches_reference(cx: &mut TestAppContext) {
    use crate::editor::fold::{blank_guide_level, indent_guide_level};
    use config::layout::INDENT_GUIDE_INDENT_SIZE as IS;
    // 0idx 0：`a();` 缩进 4；接着 500 个空行（0idx 1..=500）；0idx 501：`b();` 缩进 8。
    let mut text = String::from("    a();\n");
    for _ in 0..500 {
        text.push('\n');
    }
    text.push_str("        b();\n");
    let (host, cx) = open(cx, &text);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    // 视口足够大到覆盖全部 503 行（含尾部幻影空行）。
    let view_h = lh * 8000.0;
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_indent_guide_segments(0, 0.0, lh, 1.0, 2000.0, view_h)
    });
    let mut rows: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
    for (x, y, h) in &segs {
        let col = (x / cw).round() as i64;
        let ct = (y / pitch).round() as i64;
        let cb = ((y + h) / pitch).round() as i64;
        for r in ct..cb {
            rows.entry(r as usize).or_default().push(col as usize);
        }
    }
    // 参考（旧无界扫语义）：仅 0 与 501 非空，其余为空 → A=4、B=8、A<B → 级数 2。
    let indent_of = |logical: usize| -> Option<i32> {
        match logical {
            0 => Some(4),
            501 => Some(8),
            _ => None, // 500 空行 + 尾幻影行。
        }
    };
    let rows_total = 503usize;
    for v in 0..rows_total {
        let want: Vec<usize> = match indent_of(v) {
            Some(i) => (1..=indent_guide_level(i as usize, IS) as usize)
                .map(|l| (l - 1) * IS)
                .collect(),
            None => {
                let prev = (0..v).rev().find_map(|w| indent_of(w));
                let next = ((v + 1)..rows_total).find_map(|w| indent_of(w));
                let lvl = blank_guide_level(prev, next, IS as i32) as usize;
                (1..=lvl).map(|l| (l - 1) * IS).collect()
            }
        };
        let got = rows.get(&v).cloned().unwrap_or_default();
        assert_eq!(got, want, "0idx 行 {v}（长空行段）画列应与无界扫参考一致");
    }
    // 形状断言：500 空行段 c1+c5 贯通（A<B → 级数 2），非散列。
    assert!(
        rows.get(&250).map(|c| c.as_slice()) == Some(&[0, 4usize][..]),
        "空行中段应同画 c1+c5: {:?}",
        rows.get(&250)
    );
}

/// 折叠感知（历史规范 docs/bracket-guide-redesign.md §8.3；终稿 active 体口径见
/// editing.rs active_pair / active_guide_group doc）：折叠一块后——
/// ① 折叠体内行不出现在可视序 → 其缩进级竖线整体消失（本应画 c5 的体内行无任何线）；
/// ② 折叠**头行**照常渲染、照常画自家引导线（col0）；
/// ③ 空行恰在折叠边界处 → 上一/下一非空**可视**行（fold 展开态）取 A/B 续线；
/// ④ 展开折叠 → c5 恢复（折叠只是视觉压缩，不改缩进）。
#[gpui::test]
fn indent_guides_fold_suppresses_interior(cx: &mut TestAppContext) {
    // 0idx：0 fn a(缩进0)、1 if x(4)、2 y(8)、3 }(4)、4 空行、5 w(4)、6 }(0)。
    let text = "\
fn a() {
    if x {
        y();
    }

    w();
}
";
    let (host, cx) = open(cx, text);
    // 折叠 if 块（首行 1，隐藏体 `(1,end)`——行 2 隐藏；end=3 行可见）。
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.test_toggle_fold_at(1));
    });
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |_, cx| cx.notify());
    });
    settle(cx);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    // 折叠后可视序：v0=行0、v1=行1(头)、v2=行3(})、v3=行4(空行)、v4=行5(w)、v5=行6(})。
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_indent_guide_segments(0, 0.0, lh, 1.0, 2000.0, 4000.0)
    });
    // ① 折叠体内（行 2 y() 缩进 8 本应画 c5）不出现在可视序 → 无任何 col4 段。
    assert!(
        !segs.iter().any(|(x, _, _)| (x / cw).round() as i64 == 4),
        "折叠体内行不产引导线（col4 整体消失）: {segs:?}"
    );
    // ② 折叠头行（v1 = 行1 缩进 4）照常画 col0。
    assert!(
        segs.iter().any(|(x, y, h)| {
            let col = (x / cw).round() as i64;
            let ct = (y / pitch).round() as i64;
            let cb = ((y + h) / pitch).round() as i64;
            col == 0 && ct <= 1 && 1 < cb
        }),
        "折叠头行应画自家 col0: {segs:?}"
    );
    // ③ 空行（v3 = 0idx 4）在折叠边界处：可视邻居 A=v2(行3 `}` 缩进4)、B=v4(行5 缩进4)、
    //    A==B → ⌈4/4⌉=1 → 续 col0（经空行）。
    assert!(
        segs.iter().any(|(x, y, h)| {
            let col = (x / cw).round() as i64;
            let ct = (y / pitch).round() as i64;
            let cb = ((y + h) / pitch).round() as i64;
            col == 0 && ct <= 3 && 3 < cb
        }),
        "折叠边界空行按可视邻居续 col0: {segs:?}"
    );
    // ④ 展开 → col4 恢复。
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.test_toggle_fold_at(1));
    });
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |_, cx| cx.notify());
    });
    settle(cx);
    let segs2 = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_indent_guide_segments(0, 0.0, lh, 1.0, 2000.0, 4000.0)
    });
    assert!(
        segs2.iter().any(|(x, _, _)| (x / cw).round() as i64 == 4),
        "展开折叠后 col4 引导线应恢复: {segs2:?}"
    );
}

/// §8.1/§8.6 横滚与裁剪：x = g·cw − hscroll（同 bracket 段公式）。
/// ① 横滚 2 格 → c5 平移到 (4−2)·cw、c1(col0) 的 x<0 → 整列裁剪；② 深缩进行列
/// 超出可视宽 harea_w → 超宽列裁剪（不越界画）。
#[gpui::test]
fn indent_guides_hscroll_moves_and_clips(cx: &mut TestAppContext) {
    // 行0 缩进 4（式画 col0）、行1 缩进 12（画 col0+col4+col8）。
    let text = "    x();\n            y();";
    let (host, cx) = open(cx, text);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    // 基线（hscroll=0、宽视口）：col0/col4/col8 均画。
    let base = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_indent_guide_segments(0, 0.0, lh, 1.0, 2000.0, 4000.0)
    });
    let base_cols: std::collections::BTreeSet<i64> =
        base.iter().map(|(x, _, _)| (x / cw).round() as i64).collect();
    assert!(
        base_cols.contains(&0) && base_cols.contains(&4) && base_cols.contains(&8),
        "基线应画 col0/4/8: {base_cols:?}"
    );
    // 横滚 2 格 → 列 x = g·cw − 2cw（归一化 col=round(x/cw) 随之整体左移 2，故按 x 断言）。
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.test_set_hscroll(2));
    });
    let moved = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_indent_guide_segments(0, 0.0, lh, 1.0, 2000.0, 4000.0)
    });
    // g=0（c1）→ x=0−2cw=−2cw<0 → 整列裁剪。
    assert!(
        !moved.iter().any(|(x, _, _)| (x / cw).round() as i64 == 0),
        "横滚后 c1 x<0 应整列裁剪: {moved:?}"
    );
    // g=4（c5）→ x=4cw−2cw=2cw。
    assert!(
        moved.iter().any(|(x, _, _)| (*x - 2.0 * cw).abs() < 1.0),
        "c5 应平移到 (4−2)·cw=2cw: {moved:?}"
    );
    // g=8（c9）→ x=8cw−2cw=6cw。
    assert!(
        moved.iter().any(|(x, _, _)| (*x - 6.0 * cw).abs() < 1.0),
        "c9 应平移到 (8−2)·cw=6cw: {moved:?}"
    );
    // 窄视口 harea_w=5.1cw：g=8 x=6cw > 5.1cw → 超宽裁剪；g=4 x=2cw 仍画。
    let clipped = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx)
            .test_indent_guide_segments(0, 0.0, lh, 1.0, 5.1 * cw, 1000.0)
    });
    assert!(
        !clipped.iter().any(|(x, _, _)| (*x - 6.0 * cw).abs() < 1.0),
        "超可视宽的列应裁剪: {clipped:?}"
    );
    assert!(
        clipped.iter().any(|(x, _, _)| (*x - 2.0 * cw).abs() < 1.0),
        "可视宽内列应保留: {clipped:?}"
    );
}

/// active 视觉（**块体模型**，2026-11-14 用户实测唯一真值；替代旧「最内层块内容带」/
/// 纯缩进组）：点 0idx 行 7（guidefix `z();`）→ 关联括对 = 严格含 7 的最内块 `if x==1`
/// （open 5 / close 8，depth 最大）→ 体 `[visual(5)+1, visual(8))` = [6,8)、列 = 引导列 4
/// → group = (4, 6, 8)。不看光标行自身缩进（除以块体为准）。
#[gpui::test]
fn indent_guides_active_column_on_grid(cx: &mut TestAppContext) {
    let text = guidefix_fixture_text();
    let (host, cx) = open(cx, text);
    // 点 0idx 行 7（z();）中部（文本区，不碰 gutter）。
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1).max(1.);
    let gutter = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().2);
    let mods = Modifiers::default();
    let y = px(lh * 7.5);
    let x = px(gutter + 10.);
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(cx);
    let group = cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_guide_group());
    assert_eq!(
        group,
        Some((4, 6, 8)),
        "点 0idx 行 7 → 块体模型：if x==1 块体 [6,8)（z(); 所在最内块体）"
    );
}

/// 主源叠层（历史规范 docs/bracket-guide-redesign.md §8.5 item 7）= `ce-indent-guides`
/// 默认渲染；bracket 竖线叠层默认 **Off**
/// （`BRACKET_GUIDES_MODE = BracketGuidesMode::Off`，2026-11-14 用户真机口径：彩色
/// 「全部括对竖线」默认关掉，消除其盖住缩进 active 高亮造成的错乱）：光标进入括对体 →
/// `ce-bracket-guides` 竖线层不渲染、只留横向钩线层 `ce-bracket-hooks`
/// （bracketPairsHorizontal='active'），灰缩进线全量回显。结构化段数据 / Active/All 让位
/// 表 / 钩线断言见 `bracket_guides_active_mode_only_active_pair_renders` /
/// `bracket_guides_mode_all_toggle`。
#[gpui::test]
fn indentation_guides_detected_and_bracket_overlay_off_by_default(cx: &mut TestAppContext) {
    let text = "fn main() {\n    if b == 2 {\n        x1();\n        x2();\n    }\n    let c = 3;\n}";
    let (host, cx) = open(cx, text);
    assert!(
        cx.debug_bounds("ce-indent-guides").is_some(),
        "缩进引导线叠层应默认渲染"
    );
    assert!(
        cx.debug_bounds("ce-bracket-guides").is_none(),
        "缺省 Off：bracket 竖线叠层应不渲染（彩线默认关，错乱根因已消除）"
    );
    // 缺省 Off：光标进 if 体 → 竖线层仍不渲染；钩线层恒挂（if 开行钩线在）；
    // 灰缩进线全量回显。
    focus(&host, cx);
    set_caret_row_col(&host, cx, 2, 5); // 光标在 if 体 x1() 行 → active = if。
    assert!(
        cx.debug_bounds("ce-bracket-guides").is_none(),
        "Off：active 括对也不产彩色竖线（只留钩线）"
    );
    assert!(
        cx.debug_bounds("ce-bracket-hooks").is_some(),
        "Off：钩线层仍应渲染"
    );
    assert!(
        cx.debug_bounds("ce-bracket-hook-open").is_some(),
        "Off：active 括对开行钩线应渲染（bracketPairsHorizontal 保留）"
    );
    assert!(
        cx.debug_bounds("ce-indent-guide").is_some(),
        "灰缩进线全量回显（Off 不受任何括对抑制）"
    );
}

/// Active 模式（中间态/对比，`BRACKET_GUIDES_MODE` 翻转即启用）：仅活动括对产彩色竖线 +
/// 钩线、其它括对不产线；非 active 括对列灰缩进
/// 保留（VSCode `bracketPairs:'active'`）。夹具 `fn main(){ if b==2{ x1(); x2(); }
/// let c=3; }`：
/// - fn 括对（row0..row6，depth0，引导列 0）；
/// - if 括对（row1..row4，depth1，引导列 4；`}` 独占 → 竖线 [1,4) 不含闭行）。
/// 光标在 if 体（row2 x1()）→ active = if。断言**只一条** active 段（col4、depth1、
/// active=true、行 [1,4)）+ if 开行底边钩线 + 让位表只含 if 括对（fn 括对列 0 灰缩进保留）。
#[gpui::test]
fn bracket_guides_active_mode_only_active_pair_renders(cx: &mut TestAppContext) {
    let text = "fn main() {\n    if b == 2 {\n        x1();\n        x2();\n    }\n    let c = 3;\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    set_caret_row_col(&host, cx, 2, 5); // 光标在 if 体 → active = if（open1/close4）。
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let cw = cw.max(1.);
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    let (segs, suppress) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        (
            ed.test_bracket_guide_segments_flagged(
                0, 0.0, lh, 1.0, 1200.0, 1000.0, config::layout::BracketGuidesMode::Active,
            ),
            ed.test_bracket_guide_suppress(
                0, 1000.0, lh, config::layout::BracketGuidesMode::Active,
            ),
        )
    });
    // 只一条 active 段：col4、depth1、行 [1,4)（`}` 独占不含闭行）。fn 括对不产线。
    assert_eq!(segs.len(), 1, "Active 模式应只产 active 括对一条竖线：{segs:?}");
    let (x, y, h, depth, hl) = segs[0];
    assert!(
        (x / cw).round() as i64 == 4
            && (y / pitch).round() as i64 == 1
            && (h / pitch).round() as i64 == 3,
        "if 竖线应 col4、行[1,4)、高3行：({x},{y},{h})"
    );
    assert_eq!(depth, 1, "if 括对 depth=1");
    assert!(hl, "Active 模式产出的段应为 active=true");
    // 让位表只含 if 括对行 × 引导列（fn 括对列 0 不在表 → 其灰缩进线保留、与 active 共存）。
    assert_eq!(
        suppress,
        vec![(1usize, 4usize), (2, 4), (3, 4), (4, 4)],
        "Active 让位表只含 active 括对行 × 引导列：{suppress:?}"
    );
    // 钩线：开列（13）> 引导列（4）→ 开行底边横段；闭列（4）== 引导列（4）→ 无闭行横段。
    assert!(
        cx.debug_bounds("ce-bracket-hook-open").is_some(),
        "Active 模式（有 active 括对）应画开行钩线"
    );
    assert!(
        cx.debug_bounds("ce-bracket-hook-close").is_none(),
        "闭列 == 引导列时不画闭行钩线"
    );
}

/// All 模式（VSCode `bracketPairs:'always'`）：同夹具下 fn 括对（col0）与 if 括对（col4）
/// 全部产线（现行为）——与 Active 模式互补验证可切换。
#[gpui::test]
fn bracket_guides_mode_all_toggle(cx: &mut TestAppContext) {
    let text = "fn main() {\n    if b == 2 {\n        x1();\n        x2();\n    }\n    let c = 3;\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    set_caret_row_col(&host, cx, 2, 5); // 光标在 if 体 → active = if。
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let cw = cw.max(1.);
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments_flagged(
            0, 0.0, lh, 1.0, 1200.0, 1000.0, config::layout::BracketGuidesMode::All,
        )
    });
    // 两条竖线：fn 括对 col0 depth0 + if 括对 col4 depth1（全部产线 → All 语义）。
    let norm: Vec<(i64, usize)> = segs.iter().map(|(x, _, _, d, _)| ((x / cw).round() as i64, *d)).collect();
    assert_eq!(segs.len(), 2, "All 模式应全部括对产线：{norm:?}");
    assert!(norm.contains(&(0, 0)), "fn 括对段应在 col0/depth0：{norm:?}");
    assert!(norm.contains(&(4, 1)), "if 括对段应在 col4/depth1：{norm:?}");
}

/// B2 回归：拖选改「帧级合并」（`on_mouse_move` 只记最新拖点、每帧 `render` 开头
/// `flush_pending_drag` 落地）后，选区语义与旧「逐事件同步更新」逐位一致——
/// ① 逐帧拖到目标位的命中与**直接单击同点**一致；② 快速「连续移动 + 抬起」跨帧时，
/// mouseup 落地最后一次拖点（不得停在按下点）。锚点 = 按下点（拖选应收敛为选区）。
#[gpui::test]
async fn drag_select_frame_coalesced_and_flushes_on_up(cx: &mut TestAppContext) {
    let text = "line0\nline1\nline2\nline3\n";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    let (lh, gutter) = cx.update(|_, cx| {
        let m = host.read(cx).ed.read(cx).test_metrics();
        (m.1.max(1.), m.2)
    });
    let mods = Modifiers::default();
    let row1_start = "line0\n".len();
    let y = px(lh * 1.5);
    let caret = cx.update(|_, cx| host.read(cx).ed.clone());
    // 「真实字符边界 → 命中 offset」参考：直接单击同点，取光标落位（head）。
    // （不硬编码 col 映射：命中/边界语义由既有 `mouse_click_lands_on_ascii_char_*`
    // 提测，这里只断言「拖选结果 == 直接点击结果」。）
    let click_head = |cx: &mut VisualTestContext, col: usize| -> usize {
        let x = cx.update(|window, cx| {
            px(gutter + caret.read(cx).test_char_boundary_x(window, 1, col))
        });
        cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
        settle(cx);
        editor_selection(&host, cx).head()
    };
    let x_down = cx.update(|window, cx| {
        px(gutter + caret.read(cx).test_char_boundary_x(window, 1, 2))
    });
    let x_a = cx.update(|window, cx| {
        px(gutter + caret.read(cx).test_char_boundary_x(window, 1, 4))
    });
    let x_c = cx.update(|window, cx| {
        px(gutter + caret.read(cx).test_char_boundary_x(window, 1, 12.min(5)))
    });
    let ref_a = click_head(cx, 4);
    let ref_c = click_head(cx, 12.min(5));
    // 按下于第 1 行 col=2（行首偏 2 字），开始拖选。
    {
        let x = x_down;
        cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
        settle(cx);
    }
    // ① 逐帧拖到 x_a → 选区头随帧落下，等于单击同点的命中。
    cx.simulate_mouse_move(point(x_a, y), MouseButton::Left, mods);
    settle(cx);
    let mid = editor_selection(&host, cx);
    assert_eq!(mid.head(), ref_a, "拖到该点 → 选区头应等于单击同点命中");
    assert!(!mid.is_collapsed(), "拖选进行中应收敛为选区");
    assert_eq!(mid.anchor(), row1_start + 2, "锚点 = 按下点不应漂移");
    // ② 跨帧收尾：连续两次快速移动 + 抬起（中间不落帧）→ mouseup 落地最后一次。
    let x_b = cx.update(|window, cx| {
        px(gutter + caret.read(cx).test_char_boundary_x(window, 1, 8))
    });
    cx.simulate_mouse_move(point(x_b, y), MouseButton::Left, mods);
    cx.simulate_mouse_move(point(x_c, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x_c, y), MouseButton::Left, mods);
    settle(cx);
    let sel = editor_selection(&host, cx);
    assert_eq!(
        sel.head(),
        ref_c,
        "mouseup 应落地最后一次拖点（否则选区停在按下点 = 回归）"
    );
    assert!(!sel.is_collapsed(), "拖选应收敛为选区（anchor≠head）");
}

#[gpui::test]
fn mouse_click_lands_on_ascii_char_boundaries(cx: &mut TestAppContext) {
    // 回归护栏：等宽 ASCII 命中测试在接入真实行布局后仍应精确落在字符边界。
    let line = "abcdef";
    let (host, cx) = open(cx, line);
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let y = px(lh * 0.5);
    for (k, expected_head) in [(1usize, 1usize), (2, 2), (3, 3), (6, 6)] {
        let x = cx.update(|window, cx| {
            px(gutter + ed.read(cx).test_char_boundary_x(window, 0, expected_head))
        });
        let mods = Modifiers::default();
        cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
        settle(cx);
        assert_eq!(
            editor_selection(&host, cx).head(),
            expected_head,
            "点击第 {k} 个 ASCII 字符右边界应落在第 {k} 字符后"
        );
    }
}

#[gpui::test]
fn mouse_click_lands_on_cjk_char_boundaries(cx: &mut TestAppContext) {
    // 回归（2026-09-15 真机反馈）：中文行点击中间输入，实际偏移到更后面。
    // 根因：命中测试按「每字符 = 1×ASCII 字符宽」等宽近似，而 CJK 全角字实际
    // 宽 2× → 点击第 k 个字被算成第 2k 个字。现在用真实行布局
    // `closest_index_for_x` 做像素→字符边界映射，点击即落点。
    let line = "中华文字"; // 4 个全角字：每字 3 字节
    let (host, cx) = open(cx, line);
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let y = px(lh * 0.5);
    for (k, expected_head) in [(1usize, 3usize), (2, 6), (3, 9), (4, 12)] {
        let x = cx.update(|window, cx| {
            px(gutter + ed.read(cx).test_char_boundary_x(window, 0, expected_head))
        });
        let mods = Modifiers::default();
        cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
        settle(cx);
        let head = editor_selection(&host, cx).head();
        assert_eq!(head, expected_head, "点击第 {k} 个全角字右边界应落在第 {k} 字后");
    }
}

#[gpui::test]
fn mouse_drag_selects_across_rows_rust_vs_plain(cx: &mut TestAppContext) {
    // 复现调查：大文档测试.rs 无法鼠标选择、而 .txt/.md 正常。
    // 组件内鼠标逻辑与语言无关，此测试把「带 Rust 高亮」与「纯文本」并排验证，
    // 确认是否为语言/高亮路径引起。
    for lang in [Some("rust"), Some("markdown"), None] {
        let text = "fn main() {\n    let s = \"hello\";\n    // 注释\n}\n";
        let (host, cx) = open(&mut *cx, text);
        let ed = cx.update(|_, cx| host.read(cx).ed.clone());
        ed.update(cx, |ed, _| ed.set_language(lang));
        focus(&host, cx);
        settle(cx);
        let ed = cx.update(|_, cx| host.read(cx).ed.clone());
        let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
        let x1 = cx.update(|window, cx| {
            px(gutter + ed.read(cx).test_char_boundary_x(window, 0, 2))
        });
        let x2 = cx.update(|window, cx| {
            px(gutter + ed.read(cx).test_char_boundary_x(window, 2, 3))
        });
        let y1 = px(lh * 0.5);
        let y2 = px(lh * 2.5);
        let mods = Modifiers::default();
        cx.simulate_mouse_move(point(x1, y1), MouseButton::Left, mods);
        cx.simulate_mouse_down(point(x1, y1), MouseButton::Left, mods);
        cx.simulate_mouse_move(point(x2, y2), MouseButton::Left, mods);
        cx.simulate_mouse_up(point(x2, y2), MouseButton::Left, mods);
        settle(cx);
        let sel = editor_selection(&host, cx);
        assert!(
            !sel.is_collapsed(),
            "lang={lang:?}: 拖选应形成选区，sel={sel:?}"
        );
        assert_eq!(sel.range().start, 2, "lang={lang:?} 选区起点");
        assert_eq!(
            sel.range().end,
            "fn main() {\n    let s = \"hello\";\n".len() + 3,
            "lang={lang:?} 选区终点应在第 3 行第 4 字符后"
        );
    }
}

/// 回归（拖选生命周期终点之一，场景 c）：**拖到元素/窗口外松开**时元素级
/// `on_mouse_up`（受 `hitbox.is_hovered` 门控）不会派发——必须由 Capture 阶段的
/// `on_mouse_up_out` 复位 `dragging`，否则残留拖选态会让「后续仅移动鼠标」继续把
/// 选区从 anchor 一路拉长（与 P4 菜单项 `stop_propagation` 截断 MouseUp 同源）。
#[gpui::test]
fn mouse_drag_released_outside_does_not_extend_on_next_move(cx: &mut TestAppContext) {
    let text = "fn main() {\n    let s = \"hello\";\n    // 注释\n    let t = 1;\n}\n";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let x1 = cx.update(|window, cx| {
        px(gutter + ed.read(cx).test_char_boundary_x(window, 0, 2))
    });
    let x2 = cx.update(|window, cx| {
        px(gutter + ed.read(cx).test_char_boundary_x(window, 2, 3))
    });
    let y1 = px(lh * 0.5);
    let y2 = px(lh * 2.5);
    let mods = Modifiers::default();
    // 编辑区内按下 → 拖到第 3 行 → 形成跨行选区（此时 dragging=true）。
    cx.simulate_mouse_move(point(x1, y1), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x1, y1), MouseButton::Left, mods);
    cx.simulate_mouse_move(point(x2, y2), MouseButton::Left, mods);
    settle(cx);
    let extended = editor_selection(&host, cx);
    assert!(!extended.is_collapsed(), "拖选应已形成选区：{extended:?}");
    // 在远离编辑区/窗口的「窗口外」松开：元素级 on_mouse_up（hitbox 门控）不派发，
    // 走 on_mouse_up_out 复位拖选态。
    let far = point(px(-1000.), px(-1000.));
    cx.simulate_mouse_up(far, MouseButton::Left, mods);
    settle(cx);
    // 之后仅移动鼠标（不按任何键）到文档更深处：拖选态已复位 → 不得再把选区拉长。
    let far_in = point(x2, px(lh * 3.5));
    cx.simulate_mouse_move(far_in, MouseButton::Left, mods);
    settle(cx);
    let after = editor_selection(&host, cx);
    assert_eq!(
        (after.range().start, after.range().end),
        (extended.range().start, extended.range().end),
        "窗口外松开后仅移动鼠标不得继续扩选：{extended:?} → {after:?}"
    );
}

#[gpui::test]
fn caret_image_is_laid_out_when_focused(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "abc\nxyz");
    focus(&host, cx);
    settle(cx);
    // 回归（2026-09-15 实机）：光标此前画画的是 colors.accent（Light 主题 =#f5f5f5
    // 白底近白 → 看不见）；这里断言光标元素真实布局出来，且尺寸 = 2px×行高。
    let b = cx
        .debug_bounds("ce-caret")
        .expect("焦点状态下应渲染光标元素");
    assert!(
        b.size.width.as_f32() > 1.0 && b.size.height.as_f32() > 0.0
            && b.origin.y < px(40.),
        "光标应落在第 1 行、宽≥1px、高>0：{b:?}"
    );
}

#[gpui::test]
fn gutter_fixed_while_content_scrolls_horizontally(cx: &mut TestAppContext) {
    // 回归（2026-09-15 用户反馈）：编辑区内容横向滚动时，行号也跟着滚走。
    // 现在行号是编辑区左缘的固定列（`ce-gutter`，独立 List 与文本锁步）——
    // 横向滚动内容后行号列左缘必须纹丝不动，而文本内容必须真的被推走。
    // 测试窗 1920px 宽 → 最长行需远超可视区才有横向可滚量程。
    let long = "一".repeat(600);
    let (host, cx) = open(cx, &format!("{long}\n第二行"));
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let gutter0 = cx
        .debug_bounds("ce-gutter")
        .expect("行号固定列应渲染");
    assert!(
        gutter0.size.width.as_f32() > 0.,
        "行号列应有宽度：{gutter0:?}"
    );
    // 向编辑区中心派发横向滚轮（向右 = 内容左移，正 delta）。事件坐标与
    // `debug_bounds` 同空间（不能用 `window.bounds()`——那是平台坐标）。
    let ed_b = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + ed_b.size.width.as_f32() / 2.0),
        px(ed_b.origin.y.as_f32() + ed_b.size.height.as_f32() / 2.0),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(120.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(cx);
    let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(hscroll > 0., "横向滚动应真正发生（hscroll={hscroll}）");
    let gutter1 = cx.debug_bounds("ce-gutter").expect("行号列仍在");
    assert_eq!(
        gutter0.origin.x,
        gutter1.origin.x,
        "行号列左缘不应随内容横向滚动（{gutter0:?} → {gutter1:?}）"
    );
    assert!(
        gutter1.origin.x.as_f32().abs() < 1.0,
        "行号应贴在编辑区左缘（origin.x={}）",
        gutter1.origin.x.as_f32()
    );
}

#[gpui::test]
fn ime_composition_replaces_marked_text_and_commits(cx: &mut TestAppContext) {
    // 回归：IME 组合曾把每次 `replace_and_mark(None, …)` 追加到旧组合后面
    // （产生 'nn'...垃圾残留），提交后也不清 marked → 下次组合把已提交文本当
    // 旧 marked 再次替换。参照 gpui-component 的组合流：None 区间 = 替换当前
    // marked；普通 `replace_text_in_range` 提交后自动结束组合。
    let (host, cx) = open(cx, "");
    focus(&host, cx);
    cx.update(|window, app| {
        let ed = host.read(app).ed.clone();
        ed.update(app, |ed, cx| {
            ed.replace_text_in_range(None, "prefix ", window, cx);
            // 组合第 1 字：n → ni → 你（每次更新替换旧的 marked）
            ed.replace_and_mark_text_in_range(None, "n", None, window, cx);
            ed.replace_and_mark_text_in_range(None, "ni", None, window, cx);
            ed.replace_and_mark_text_in_range(None, "你", None, window, cx);
            // 提交第 1 字（结束本次组合，marked 清空）
            ed.replace_text_in_range(None, "你", window, cx);
            // 组合第 2 字并提交
            ed.replace_and_mark_text_in_range(None, "h", None, window, cx);
            ed.replace_and_mark_text_in_range(None, "ha", None, window, cx);
            ed.replace_and_mark_text_in_range(None, "好", None, window, cx);
            ed.replace_text_in_range(None, "好", window, cx);
        });
    });
    assert_eq!(editor_text(&host, cx), "prefix 你好");
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.head(), "prefix 你好".len());
    // 提交后再开一次组合，不应把已提交的 "prefix 你好" 当旧 marked 覆盖
    cx.update(|window, app| {
        let ed = host.read(app).ed.clone();
        ed.update(app, |ed, cx| {
            ed.replace_and_mark_text_in_range(None, "x", None, window, cx);
            ed.replace_text_in_range(None, "!", window, cx);
        });
    });
    assert_eq!(editor_text(&host, cx), "prefix 你好!");
}

#[gpui::test]
fn ime_new_selected_range_is_relative_to_marked_text(cx: &mut TestAppContext) {
    // 回归 1（崩溃）：`new_selected_range` 曾被当「文档绝对 UTF-16 偏移」→ 光标
    // 跳到文档开头附近，落点多字节字符中间时后续插入触发
    // `byte index is not on a char boundary` panic（buffer.rs apply_edit）。
    // 回归 2（跳开头）：同一根因让大文档任意位置输入中文时插入位置跑回文首。
    let (host, cx) = open(cx, "一二三四五");
    let ed = cx.update(|_, app| host.read(app).ed.clone());
    // 光标移到文末（第 5 字后，字节 15）
    ed.update(cx, |ed, _| ed.set_caret_to(15));
    // 文末开始组合 "ni"，IMS 光标停在组合内第 1 个 UTF-16 单元后（相对 1..1）
    cx.update(|window, app| {
        ed.update(app, |ed, cx| {
            ed.replace_and_mark_text_in_range(None, "ni", Some(1..1), window, cx);
        });
    });
    // 光标 = 文末 + 组合内偏移 1 单元（"n"=1 字节）→ 16，绝不能跳回文档开头
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.head(), 15 + 1, "光标应停在组合内相对位置而非文档开头");
    assert_eq!(editor_text(&host, cx), "一二三四五ni");
    // 再输一次组合，替换 marked、不崩溃、光标仍在组合内
    cx.update(|window, app| {
        ed.update(app, |ed, cx| {
            ed.replace_and_mark_text_in_range(None, "你i", Some(1..1), window, cx);
        });
    });
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.head(), 15 + "你".len(), "光标应停在组合内 '你' 之后");
    assert_eq!(editor_text(&host, cx), "一二三四五你i");
    // 提交
    cx.update(|window, app| {
        ed.update(app, |ed, cx| {
            ed.replace_text_in_range(None, "你", window, cx);
        });
    });
    assert_eq!(editor_text(&host, cx), "一二三四五你");
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.head(), 15 + "你".len());
}

#[gpui::test]
fn cjk_long_line_does_not_narrow_content_width(cx: &mut TestAppContext) {
    // 回归（用户实测）：纯中文长行曾因「字符数×等宽」低估字形宽 → 行宽小于真实
    // 文本宽 → StyledText 行内折行 → 行号错位、点下一行文本重叠。
    // 现在宽度按「宽度单位」计，内容宽必须 ≥ 真实最宽行的整形宽（即不会折行）。
    let cjk = "一".repeat(200); // 200 全角字 = 400 格 = 600 字节
    let (host, cx) = open(cx, &format!("{cjk}\n第二行"));
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (cw, _, _) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let content_w = cx.update(|_, cx| ed.read(cx).test_content_w_px());
    // 真实整形宽（行内末字边界 x，byte_col=600 = 行尾）
    let shaped = cx.update(|window, cx| {
        ed.read(cx)
            .test_char_boundary_x(window, 0, 200 * 3)
    });
    assert!(
        content_w >= shaped - 1.0 && content_w >= 400.0 * cw - 1.0,
        "内容宽应 ≥ 真实最宽行（content={content_w:.1} shaped宽={shaped:.1} 400×charw={:.1}）",
        400.0 * cw
    );
}

#[gpui::test]
fn gutter_scroll_locked_to_text_scroll(cx: &mut TestAppContext) {
    // 回归（用户实测）：滚轮滚动时行号「翻动/闪烁」——旧实现是每次 render 手工按
    // `logical_scroll_top` 重建行号叠层，与 List 行绘制不同步。现在行号列是独立的
    // 第二个 `List`（独立 ListState），由 `sync_gutter_scroll` 与文本区逐帧锁步。
    let text = (1..=100).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let (host, cx) = open(cx, &text);
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (t0, g0) = (
        cx.update(|_, cx| ed.read(cx).test_text_scroll_top()),
        cx.update(|_, cx| ed.read(cx).test_gutter_scroll_top()),
    );
    assert_eq!(t0, g0, "初始行号应与文本对齐（{t0:?} vs {g0:?}）");
    // 向下滚 6 格（每格 3 行 ≈ 3×行高）
    let ed_b = cx.debug_bounds("code-editor").expect("编辑器应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + 200.),
        px(ed_b.origin.y.as_f32() + 100.),
    );
    for _ in 0..6 {
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: pos,
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-30.))),
            modifiers: Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        settle(cx);
    }
    let (t1, g1) = (
        cx.update(|_, cx| ed.read(cx).test_text_scroll_top()),
        cx.update(|_, cx| ed.read(cx).test_gutter_scroll_top()),
    );
    assert!(t1.0 > t0.0, "应已向下滚动（{t0:?} → {t1:?}）");
    assert_eq!(t1, g1, "滚动后行号应与文本锁步一致（{t1:?} vs {g1:?}）");
}

#[gpui::test]
fn empty_line_typing_keeps_hscroll_zero(cx: &mut TestAppContext) {
    // 回归（用户实测）：大文档里「空行输入内容时，界面会突然往右弹，然后又弹回」。
    // 根因：`reveal_caret` 横向分支在 col < margin 时把 hscroll 设成 col（逐字左移
    // 内容），等到 col=4 越过 margin 后走 col-right_edge 又 saturating 回 0 →
    // 每 4 个字符一次「往右弹又弹回」。修复：左缘 reveal 目标 = col-margin（col<margin
    // 时为 0，不动），右缘逻辑不变。
    // 文档含超长行以撑起横向量程（content_w > 视口）——否则 hscroll 量程为 0，
    // 弹跳被量程夹掉不可见。
    let text = "x".repeat(600) + "\n\n短行";
    let (host, cx) = open(cx, &text);
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let max0 = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(max0 > 0., "超长行应撑起横向量程（max={max0}）");
    // 光标放到空行（行 1，0 基）行首
    let off = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(1, 0));
    ed.update(cx, |ed, _| ed.set_caret_to(off));
    settle(cx);
    let h0 = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert_eq!(h0, 0., "空行(未横向滚动)初始 hscroll 应为 0");
    // 连续输入超 margin 个字符（跨过 0..3 的左缘 margin 区）→ hscroll 保持 0
    for c in "abcde".chars() {
        ed.update(cx, |ed, cx| ed.insert_text(&c.to_string(), cx));
        settle(cx);
        let h = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
        assert_eq!(h, 0., "空行输入 {c} 后不应触发横向滚动（hscroll={h}）");
    }
}

#[gpui::test]
fn hscroll_max_shrinks_after_deleting_widest_content(cx: &mut TestAppContext) {
    // 回归（用户实测）：横向滚动条拉到最右 → 末尾追加超宽内容（量程变大）→
    // 删除后量程不缩回。方案 A+B：删除置脏后（量程可见 + 首次/节流）精确重算使
    // max_line_units 收敛，render 再把 h_handle 夹回新量程——不逐键 O(n)。
    let (host, cx) = open(cx, "start\nline2");
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let base_max = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    // 光标移到文末，追加 600 个 x 的超宽内容
    let len = cx.update(|_, cx| host.read(cx).ed.read(cx).text_string().len());
    ed.update(cx, |ed, _| ed.set_caret_to(len));
    let wide = "x".repeat(600);
    ed.update(cx, |ed, cx| ed.insert_text(&wide, cx));
    settle(cx);
    let wide_max = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        wide_max > base_max + 1.0,
        "追加超宽内容后量程应增大（{base_max} → {wide_max}）"
    );
    // 撤销 = 删除刚追加的超宽内容 → 量程应缩回
    let w = window_handle(cx);
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-z").unwrap());
    settle(cx);
    let shrunk_max = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        shrunk_max <= base_max + 1.0,
        "删除超宽内容后量程应缩回（wide={wide_max} → shrunk={shrunk_max}，base={base_max}）"
    );
}

#[gpui::test]
fn hscroll_range_is_exact_not_quantized_and_reaches_content_end(cx: &mut TestAppContext) {
    // 回归（P7 治本）：历史补丁把横向量程按 64px 向上量化（每 ~8 字符一档），量程
    // 最多比真实内容宽多 63px → 滚到“真底”后内容右缘停在可视区末端左侧一段空白，
    // 且最宽行末尾输入要依赖「64px 量化 + 120ms 冷却」量程才不坍缩。
    // 现状：量程 = 容器**实测** `h_handle.max_offset().x` =（Buffer O(1) 精确的最宽行
    // 宽 − 文本区可视宽），不量化、无浮余：滚到最右恰好露出内容右缘（不缺内容），
    // 量程随最宽行逐字符精确变化（无 64px 一档突跳、不闪烁）。
    let text = "x".repeat(400) + "\n短行";
    let (host, cx) = open(cx, &text);
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let cw = cx.update(|_, cx| ed.read(cx).test_metrics().0).max(1.0);
    // 用实际布局出的文本区可视宽（`ce-harea` = 编辑区宽 − 行号列宽）做基准，
    // 不依赖测试窗硬编码宽度。
    let harea_w = cx
        .debug_bounds("ce-harea")
        .expect("文本区容器应渲染")
        .size
        .width
        .as_f32();
    let expected_exact = (400.0 * cw).max(harea_w) - harea_w;
    assert!(expected_exact > 0., "超宽行应撑起横向量程（expected={expected_exact:.1}）");
    let max_x = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        (max_x - expected_exact).abs() < 2.0,
        "横向量程应等于真实内容宽−可视宽（无 64px 量化上浮）：max={max_x:.1} 精确={expected_exact:.1}"
    );
    // 滚到最右（真实轮齿距事件）→ 应停在量程末端：内容右缘恰好落在可视区右缘
    // （旧量化会留下一段空白：right=可视−63px）。
    let ed_b = cx.debug_bounds("code-editor").expect("编辑器应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + 200.),
        px(ed_b.origin.y.as_f32() + 30.),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(1_000_000.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(cx);
    let h = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(
        (h - max_x).abs() < 4.0,
        "滚到最右应停在量程末端（h={h:.1} max={max_x:.1}）"
    );
    let right = 400.0 * cw - h;
    assert!(
        (right - harea_w).abs() < 8.0,
        "滚到底后内容右缘应恰好落在可视区右缘（right={right:.1} harea_w={harea_w:.1}）；旧 64px 量化会留下大段空白"
    );
    // 最宽行末尾继续输入：量程应逐字符精确增长（= 10×格宽），不是 64px 一档突跳。
    ed.update(cx, |ed, _| ed.set_caret_to(400));
    for _ in 0..10 {
        ed.update(cx, |ed, cx| ed.insert_text("x", cx));
    }
    settle(cx);
    let max2 = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    let grown_exact = expected_exact + 10.0 * cw;
    assert!(
        (max2 - grown_exact).abs() < 3.0,
        "最宽行末尾打字量程应逐字符精确增长（max2={max2:.1} expected={grown_exact:.1}）"
    );
}

#[gpui::test]
fn click_near_line_start_keeps_horizontal_scroll(cx: &mut TestAppContext) {
    // 回归（用户实测）：点击行首/前几个字符曾把内容强行拉去最左（内容左右晃动）。
    // 根因：鼠标点击也走 `reveal_caret` 的横向 margin 规则（col<margin → hscroll=col）。
    // 现在点击/拖动只做竖向 reveal，横向滚动只能由键盘/输入/滚轮驱动。
    let long = "x".repeat(600);
    let (host, cx) = open(cx, &format!("short\n{long}"));
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    // 先横向滚到右侧
    let ed_b = cx.debug_bounds("code-editor").expect("编辑器应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + 200.),
        px(ed_b.origin.y.as_f32() + lh * 0.5),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(400.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(cx);
    let before = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(before > 0., "应先横向滚动（before={before}）");
    // 点击短行行首（空白处/首个字符）——不得把内容拖回最左
    let y = px(lh * 0.5);
    let x = px(gutter + 2.0);
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(cx);
    let after = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(
        after.abs() >= before - 0.1,
        "点击不应重置横向滚动（before={before} → after={after}）"
    );
    // 空白行同样不应重置
    let blank_y = px(lh * 2.5);
    cx.simulate_mouse_move(point(x, blank_y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, blank_y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, blank_y), MouseButton::Left, mods);
    settle(cx);
    let after2 = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(
        after2.abs() >= before - 0.1,
        "点击行首空白也不应重置横向滚动（{before} → {after2}）"
    );
}

#[gpui::test]
fn typing_long_cjk_keeps_caret_visible(cx: &mut TestAppContext) {
    // 回归（用户实测）：内容超出窗口后，新输入不可见（光标没跟随）。
    // 根因：横向 reveal 用「字符列」，中文每字占 2 格 → 少滚一半。改按「宽度单位」
    // 列后，中文长行光标会滚回可视区。
    let (host, cx) = open(cx, "");
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (cw, _, _gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let visible = cx.update(|_, cx| ed.read(cx).test_visible_chars());
    // 打远超可视格的中文（400 字 = 800 格）
    cx.simulate_keystrokes(&"你".repeat(400));
    settle(cx);
    let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(hscroll > 0., "超窗输入应自动横向滚到光标处（hscroll={hscroll}）");
    // 光标近似 x（单位列 800 格）× 格宽 − 已滚像素应落在可视区 [0, 可视格×格宽]
    let caret_x_approx = 800.0 * cw;
    let visible_px = (visible.saturating_sub(4)) as f32 * cw;
    assert!(
        caret_x_approx - hscroll <= visible_px + cw * 2.0
            && caret_x_approx - hscroll + cw >= 0.0,
        "光标应滚回可视区（caret_x={caret_x_approx:.0} hscroll={hscroll:.0} 可视≈{visible_px:.0}）"
    );
}

#[gpui::test]
fn hscroll_bar_max_tracks_content_growth(cx: &mut TestAppContext) {
    // 回归（用户实测）：底部横向滚动条不随内容变化。量程应跟随 content_w_px 增长。
    let (host, cx) = open(cx, "");
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let max0 = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(max0 == 0., "短内容量程应为 0（max0={max0}）");
    // 输入超过可视宽度 → content_w_px 增长 → 量程应 > 0
    cx.simulate_keystrokes(&"x".repeat(400));
    settle(cx);
    let max1 = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        max1 > 0.,
        "内容变宽后横向滚动条量程应 > 0（max1={max1}）"
    );
    // 追加更长 → 量程继续增长
    cx.simulate_keystrokes(&"x".repeat(400));
    settle(cx);
    let max2 = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        max2 > max1 + 1.0,
        "更宽内容应让量程继续增长（{max1} → {max2}）"
    );
}

#[gpui::test]
fn vertical_range_stays_healthy_while_typing_in_widest_line(cx: &mut TestAppContext) {
    // 回归（用户实测）：在最宽长行末尾持续输入时，纵向滚动条“突然变长又变短”。
    // 旧根因：内容宽逐键变化 → gpui List 布局宽度变化时清空全部 size_hint →
    // 依赖 `max_offset_for_scrollbar()` 的量程坍缩，`ensure_uniform_hint` 的 120ms
    // 冷却又会阻止持续打字时的自愈（量程一直缩成极小）；曾用「64px 量化」临时绕开。
    // 现状（P7 治本）：纵向量程 = 确定性像素模型（`list_max_offset` = 全文行数 ×
    // 行高 − 视口），与 gpui 换宽清 size_hint 无关；量程恒健康，**无需冷却、无需
    // 量化**——持续输入最宽行末尾，量程立即正确（也无需 sleep 等自愈）。
    let mut text = "x".repeat(10000);
    for i in 1..=500 {
        text.push_str(&format!("\nline{i}"));
    }
    let (host, cx) = open(cx, &text);
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let healthy = cx.update(|_, cx| host.read(cx).ed.read(cx).list_max_offset());
    assert!(healthy > 6000., "初始纵向量程应健康（{healthy}）");
    // 光标移到最宽行（第 1 行）末尾
    ed.update(cx, |ed, _| ed.set_caret_to(10000));
    // 持续输入 80 字符（跨大量 64px 粒度）：量程必须始终健康（无坍缩、无冷却期）。
    for _ in 0..80 {
        ed.update(cx, |ed, cx| ed.insert_text("x", cx));
        settle(cx);
    }
    // 立即断言（不等任何冷却）——确定性量程随内容即时正确。
    let after = cx.update(|_, cx| host.read(cx).ed.read(cx).list_max_offset());
    assert!(after > 6000., "持续输入后纵向量程应保持健康（{after}）");
}

#[gpui::test]
fn reveal_keeps_caret_visible_while_typing_in_long_line(cx: &mut TestAppContext) {
    // 回归（用户实测）：长行末尾持续输入时「横向滚动条算错，最新内容看不见」。
    // 根因：`reveal_caret` 把带 +8 估算余量的 visible_chars 当真实可视格数 → 光标被
    // 滚到真实右缘之外 ~4 格（实测 screen=1911 > 视口1883）。修复：先减掉 +8 余量。
    let mut text = "x".repeat(10000);
    for i in 1..=500 {
        text.push_str(&format!("\nline{i}"));
    }
    let (host, cx) = open(cx, &text);
    focus(&host, cx);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (cw, _, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let viewport_px = 1920.0 - gutter; // 测试窗 1920 宽
    ed.update(cx, |ed, _| ed.set_caret_to(10000));
    for _ in 0..60 {
        ed.update(cx, |ed, cx| ed.insert_text("x", cx));
        settle(cx);
        let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
        let caret_units = cx.update(|_, cx| host.read(cx).ed.read(cx).test_caret_units());
        let screen = caret_units as f32 * cw - hscroll;
        assert!(
            (0.0..=viewport_px + 0.5).contains(&screen),
            "光标应滚回可视区（screen={screen:.0} ≤ 视口{viewport_px:.0}）"
        );
    }
}

#[gpui::test]
fn scroll_range_survives_width_change(cx: &mut TestAppContext) {
    // 回归（2026-09-15 实机）：gpui 在宽度变化时清空全部 size_hint，未测条目高度按
    // 0 计 → 依赖 `max_offset_for_scrollbar()` 的量程坍缩、滚动条量程失真。
    // 现状（P7 治本）：纵向量程 = 确定性像素模型（`list_max_offset` = 全文行数 ×
    // 行高 − 视口），与 gpui 换宽清 size_hint **无关**；已删除 `ensure_uniform_hint`
    // 的 120ms 换宽冷却——换宽后量程**立即**健康，无需等冷却自愈。
    let text = (1..=300)
        .map(|i| format!("第{i}行"))
        .collect::<Vec<_>>()
        .join("\n");
    let (host, cx) = open(cx, &text);
    // 横向内容宽度跟随窗口宽（短文档 content=avail_w），换宽让 gpui List 清空全部
    // size_hint 也无所谓——量程是确定性 `reach_px`，resize 落定即正确（不等冷却）。
    fn reflow_and_assert(host: &Entity<Host>, cx: &mut VisualTestContext, w: f32) -> f32 {
        cx.simulate_resize(size(px(w), px(600.)));
        // 多 pump 几帧等 viewport_bounds 布局落定，但不 sleep：无冷却可等。
        settle(cx);
        let m = cx.update(|_, cx| host.read(cx).ed.read(cx).list_max_offset());
        assert!(m > 1000., "换宽后量程应立即健康（m={m:.1} w={w}）");
        m
    }
    let mut last_max = reflow_and_assert(&host, cx, 300.);
    assert!(
        last_max > 1000.,
        "首次渲染量程应接近 300 行（last_max={last_max:.1}）"
    );
    // 再换两次宽 → 量程保持健康（无冷却依赖）。
    for w in [440., 520.] {
        last_max = reflow_and_assert(&host, cx, w);
    }
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1);
    let pad = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bottom_pad_px());
    assert!(
        (last_max - (300.0 * lh + pad - 600.0)).abs() < lh * 4.,
        "量程应≈300×行高+底部预留−视口高（{last_max:.1} vs {:.1}）",
        300.0 * lh + pad - 600.0
    );
}

// ---------------------------------------------------------------------------
// 粘贴/列表窗口（回归：空标签粘贴只显示首行）
// ---------------------------------------------------------------------------

#[gpui::test]
fn empty_new_tab_paste_not_collapsed_to_first_row(cx: &mut TestAppContext) {
    // 回归（用户实测「新建空标签 → 粘贴多行 → 编辑器只显示第一行」）：
    // `CodeEditor::new` 建空标签时 ListState 若按 `row_count`(1) 初始化，则固定窗口
    // 虚拟化从未建立（`resync_list` 只在 set_text/开文件/撤销重做时重置为
    // `LIST_WINDOW_ROWS`）；`apply_desired_to_window` 的「窗口覆盖」判定又会误判
    // 无需补建 → 粘贴 300 行后列表仍只有 1 项 → `list()` 只渲染首行。
    // 修复：`new()` 直接以固定窗口 `LIST_WINDOW_ROWS` 初始化。
    use config::limits::EDITOR_LIST_PROVISIONAL_ROWS as WINDOW;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let ed = cx.new(|cx| CodeEditor::new(cx));
        Host { ed }
    });
    let cx: &mut VisualTestContext = cx;
    settle(cx);
    let win0 = cx.update(|_, cx| host.read(cx).ed.read(cx).test_list_window_len());
    assert_eq!(
        win0, WINDOW,
        "空标签首帧 ListState 就应为固定窗口 {WINDOW} 项（而不是 1 项）"
    );
    // 粘贴 300 行多行文本。
    let text = (0..300).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    ed.update(cx, |ed, cx| ed.paste_text(&text, cx));
    settle(cx);
    let (lc, vis, win, sic) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.read(cx);
        (
            ed.line_count(),
            ed.test_folds().visual_line_count(),
            ed.test_list_window_len(),
            ed.test_scroll_item_count(),
        )
    });
    assert_eq!(lc, 300, "数据行数应完整");
    assert_eq!(vis, 300, "视觉行数应完整");
    assert!(sic >= vis, "滚动量程应覆盖全部视觉行（sic={sic} vis={vis}）");
    assert_eq!(
        win, WINDOW,
        "粘贴后固定窗口条目数应保持 = {WINDOW}（实际 {win}）——否则只渲染首行"
    );
}

// ---------------------------------------------------------------------------
// M5-B2-3 行级缓存：无编辑滚动多帧后渲染不变 / 编辑后缓存失效
// ---------------------------------------------------------------------------

#[gpui::test]
fn row_cache_reuses_scrolled_rows_and_invalidates_on_edit(cx: &mut TestAppContext) {
    // M5-B2-3：`build_rows` 行级缓存——滚动帧只物化新进窗口的行。重连线序断言：
    // 1) 无编辑滚动多帧后，重叠行渲染（文本 + 样式段）逐位不变且命中缓存（浅复用）；
    // 2) 编辑（buffer.version()+1）后缓存键失效，同一窗口下行取数路径如实反映新文本，
    //    未编辑行不受影响。
    let text = (0..200)
        .map(|i| format!("line{i:03} fn f{i}() {{ let v = {i}; // {i} }}"))
        .collect::<Vec<_>>()
        .join("\n");
    let (host, cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());

    // 帧 1：窗口 A = 逻辑行 0..50（真实 build_rows 路径，与 render 一帧同源）。
    let a = cx.update(|window, cx| {
        ed.update(cx, |ed, _| ed.test_build_rows_snapshot(window, 0, 50))
    });
    assert_eq!(a.len(), 51, "窗口 A 应含 51 行");

    // 帧 2：无编辑滚动 +10 行 → 窗口 B = 10..60。
    let hits0 = cx.update(|_, cx| ed.read(cx).test_row_cache().0);
    let b = cx.update(|window, cx| {
        ed.update(cx, |ed, _| ed.test_build_rows_snapshot(window, 10, 60))
    });
    let hits1 = cx.update(|_, cx| ed.read(cx).test_row_cache().0);

    // 无编辑滚动多帧后渲染不变：B 与 A 重叠的行（10..49）逐位一致。
    for i in 10..50 {
        assert_eq!(b[i - 10].0, a[i].0, "滚动复用行文本应逐位一致（行 {i}）");
        assert_eq!(b[i - 10].1, a[i].1, "滚动复用行样式应逐位一致（行 {i}）");
    }
    // 滚动帧只物化新进窗口的行：41 行重叠应基本全命中（而非整窗重物化）。
    assert!(
        hits1 - hits0 >= 40,
        "无编辑滚动应大量复用缓存（hits {hits0} → {hits1}）"
    );

    // 帧 3：编辑窗口内的行 20 → version+1 → 缓存键整体失效 → 数据路径反映新文本。
    let off = cx.update(|_, cx| ed.read(cx).offset_for_line_col(20, 4));
    cx.update(|_, cx| {
        ed.update(cx, |ed, cp| {
            ed.set_caret_to(off);
            ed.insert_text("X", cp);
        })
    });
    let c = cx.update(|window, cx| {
        ed.update(cx, |ed, _| ed.test_build_rows_snapshot(window, 10, 60))
    });
    assert!(
        c[10].0.contains('X'),
        "编辑后窗口行 20 应反映新文本（缓存未吞编辑）: {:?}",
        c[10].0
    );
    // 未编辑行（行 11 / 22）文本不受编辑影响：缓存失效不引入失真。
    assert_eq!(c[0].0, b[0].0, "未编辑行文本不应漂移（行 10）");
    assert_eq!(c[12].0, b[12].0, "未编辑行文本不应漂移（行 22）");
}

/// 方案2 护栏：`apply_live_replay` 的「先干跑自检后落盘」契约——基线一致时按序重放成功、
/// 产物 == 期望全文；基线错位（越界 / 切中多字节字符 / 终态不符，如 CRLF/BOM/漏 op 的
/// 偏移漂移）时**不改动缓冲区**、返回 `false`，由调用方（`app::commit_live`）回退行级
/// diff。若护栏失败也把缓冲区改掉，fallback 的 diff 基线就会取到污损文本，内容必错。
#[gpui::test]
fn live_replay_guard_never_mutates_on_drift(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let ed = cx.new(|cx| CodeEditor::new(cx));
        // 基线一致（happy path）：1 条 op 按序重放 → true 且内容 == 期望。
        ed.update(cx, |ed, _| {
            ed.set_text("a\nb\n");
            let ops = vec![(0..0, gpui::SharedString::from("+"))];
            assert!(ed.apply_live_replay(&ops, "+a\nb\n"), "基线一致时应重放成功");
            assert_eq!(ed.text_string(), "+a\nb\n", "重放后内容应等于期望全文");
        });
        // 终态错位：op 重放到本缓冲区的产物 != 期望（模拟外部漂移基线）→ 不改动、false。
        ed.update(cx, |ed, _| {
            ed.set_text("a\nb\n");
            let before = ed.text_string();
            let ops = vec![(0..0, gpui::SharedString::from("+"))];
            assert!(
                !ed.apply_live_replay(&ops, "+X\na\nb\n"),
                "终态不符时应判定漂移并拒绝"
            );
            assert_eq!(ed.text_string(), before, "护栏失败不得改动缓冲区");
        });
        // 越界错位：op 区间超出当前文档尾 → 不改动、false。
        ed.update(cx, |ed, _| {
            ed.set_text("a\nb\n");
            let before = ed.text_string();
            let ops = vec![(99..120, gpui::SharedString::from("x"))];
            assert!(
                !ed.apply_live_replay(&ops, "x"),
                "越界 op 应被护栏拒绝"
            );
            assert_eq!(ed.text_string(), before, "越界时不得改动缓冲区");
        });
        // 切中多字节字符：op 区间切破 UTF-8 字符边界 → 不改动、false。
        ed.update(cx, |ed, _| {
            ed.set_text("中文内容");
            let before = ed.text_string();
            let ops = vec![(1..2, gpui::SharedString::from("X"))];
            assert!(
                !ed.apply_live_replay(&ops, "X中文内容"),
                "切中字符边界的 op 应被护栏拒绝"
            );
            assert_eq!(ed.text_string(), before, "切中字符时不得改动缓冲区");
        });
    });
}

// ---------------------------------------------------------------------------
// Wave2-S1：折叠候选 / 括号配对后台**增量**刷新（受影响段局部重扫 + 尾部平移）
// ---------------------------------------------------------------------------

/// 对拍 oracle：对当前文本跑整篇扫描，返回按 `start` / `open_line` 排序的
/// fold 候选 / 括号配对（与视图落地口径一致）。
fn full_scan_oracle(text: &str) -> (Vec<Fold>, Vec<crate::editor::fold::BracketPair>) {
    let lines: Vec<&str> = text.split('\n').collect();
    let n = lines.len();
    let mut folds = crate::editor::fold::scan_fold_candidates(
        |i: usize| {
            lines
                .get(i)
                .map(|s| gpui::SharedString::from(*s))
                .unwrap_or_else(|| gpui::SharedString::new_static(""))
        },
        n,
    );
    folds.sort_by_key(|f| f.start);
    let mut pairs = crate::editor::fold::scan_bracket_pairs(
        |i: usize| {
            lines
                .get(i)
                .map(|s| gpui::SharedString::from(*s))
                .unwrap_or_else(|| gpui::SharedString::new_static(""))
        },
        n,
    );
    pairs.sort_by_key(|p| p.open_line);
    (folds, pairs)
}

/// 断言当前编辑器的 fold 候选 / 括号配对与整篇扫描逐位一致（对拍）。
fn assert_scan_parity(host: &Entity<Host>, cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.read(cx);
        let cands = ed.test_fold_candidates().to_vec();
        let pairs = ed.test_bracket_pairs().to_vec();
        let text = ed.text_string();
        let (ef, ep) = full_scan_oracle(&text);
        assert_eq!(cands, ef, "折叠候选与整篇扫描不一致（text={text:?}）");
        assert_eq!(pairs, ep, "括号配对与整篇扫描不一致（text={text:?}）");
    });
}

/// 等待后台折叠候选/括号配对扫描落地：先强制渲染一帧让 render 把 `scan_dirty_edit`
/// 挂起的扫描真正 spawn，再用时钟推进 + 毫秒级 sleep 多泵几轮（测试执行器的后台
/// 线程需真实时间，`run_until_parked` 恒零时间不够），在途任务（`scan_inflight`）
/// 结束即返回；无在途任务直接返回。
#[gpui::test]
fn structural_edits_update_fold_brackets_incrementally(cx: &mut TestAppContext) {
    // 结构编辑/删除后：折叠候选与括号配对经**增量**刷新（受影响段局部重扫 + 尾部
    // 平移）落地，结果与整篇扫描逐位一致；且确实走增量路径（非 O(全文) 整篇兜底）。
    // 刷新用测试**同步**落地入口 `test_scan_sync_land`（与后台刷新共用同一计算函数
    // `compute_fold_brackets`；后台调度的异步链路另由 `async_refresh_lands_incrementally`
    // 冒烟覆盖）。注意：`test_scan_sync_land` 必须与编辑同一 `ed.update` 闭包内调用，
    // 否则测试执行器会在语句间自动渲染、先行消费 `scan_dirty_edit`。
    let text = "fn a() {\n    let x = {\n        y,\n    };\n    z();\n}\nfn b() {\n    w();\n}";
    let (host, cx) = open(cx, text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    // 打开时同步整篇扫描即对拍基准。
    assert_scan_parity(&host, cx);

    // 编辑 1：`z();` 行首插入换行 + 更深缩进的新 if 块（净 +2 行、新 brace）。
    cx.update(|_, cx| {
        ed.update(cx, |ed, cx| {
            let off = ed.offset_for_line_col(4, 0);
            ed.set_caret_to(off);
            ed.insert_text("\n    if ok {\n        q();\n    }", cx);
            assert!(ed.test_scan_sync_land(), "结构插入应走增量路径");
        })
    });
    assert_scan_parity(&host, cx);

    // 编辑 2：删掉整个 if 块（多行删除，净 −3）——走真实编辑主链（apply_edit_synced）。
    cx.update(|_, cx| {
        ed.update(cx, |ed, _| {
            let (a, b) = (ed.offset_for_line_col(5, 0), ed.offset_for_line_col(8, 0));
            ed.apply_replaces(&[(a..b, String::new())]);
            assert!(ed.test_scan_sync_land(), "多行删除应走增量路径");
        })
    });
    assert_scan_parity(&host, cx);

    let (inc, full) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_scan_landings());
    assert!(
        inc >= 2 && full <= 1,
        "结构编辑应走增量路径而非整篇兜底（inc={inc} full={full}）"
    );
}

/// 泵空测试执行器里的后台异步任务（真实应用由 RAF/notify + OS 事件循环持续驱动；
/// 测试执行器对 render 内 `cx.spawn` 轮询不足 → 用时钟推进 + 毫秒级 sleep 兜底，
/// 与仓库其它异步模块（pdf/video/image）同款挠法）。
fn pump_async(cx: &mut VisualTestContext) {
    for _ in 0..40 {
        cx.executor().advance_clock(std::time::Duration::from_millis(16));
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(2));
        cx.run_until_parked();
    }
}

#[gpui::test]
fn undo_redo_keep_fold_brackets_parity_and_incremental(cx: &mut TestAppContext) {
    // undo/redo（文档行数变化路径）同样登记受影响行段：增量刷新落地后与整篇扫描
    // 一致；激活折叠在编辑后保持合理（FoldMap 平移/展开不越界）。
    let text = "fn a() {\n    let x = {\n        y,\n    };\n    z();\n}\nfn b() {\n    w();\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    let w = window_handle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    // 预置一个激活折叠 [1,5)（fn a 体）→ 编辑后折叠状态仍合理。
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(1, 5)));
    settle(cx);

    // 结构编辑：`z();` 行首插入换行 + 一行（同一闭包内同步增量落地）。
    cx.update(|_, cx| {
        ed.update(cx, |ed, cx| {
            let off = ed.offset_for_line_col(4, 0);
            ed.set_caret_to(off);
            ed.insert_text("\n    q();", cx);
            assert!(ed.test_scan_sync_land(), "结构编辑应走增量路径");
        })
    });
    assert_scan_parity(&host, cx);
    // 编辑进折叠体 → 该折叠被展开（FoldMap.sync_edit），不得残留越界区间。
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.read(cx);
        for f in ed.test_folds().folds() {
            assert!(
                f.start < f.end && f.end <= ed.text_string().split('\n').count(),
                "折叠区间越界：{f:?}"
            );
        }
    });

    // 撤销（键盘路径 → render 起后台异步增量刷新）→ 对拍仍一致。
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-z").unwrap());
    pump_async(cx);
    assert_scan_parity(&host, cx);

    // 重做 → 对拍仍一致。
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-y").unwrap());
    pump_async(cx);
    assert_scan_parity(&host, cx);

    let (inc, full) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_scan_landings());
    assert!(
        inc >= 3 && full <= 1,
        "编辑 + 撤销 + 重做都应走增量路径（inc={inc} full={full}）"
    );
}

/// 后台异步刷新链路冒烟：一次结构编辑后，`cx.spawn` + `background_executor` + 落地
/// 通知的全链路在真实渲染帧驱动下能增量落地（首帧即与整篇扫描一致）。测试执行器
/// 只可靠轮询**首个** render 内 spawn 的任务（后续轮的轮询属观测器材性，已由同步
/// 落地入口覆盖），真实应用由 OS 事件循环持续驱动、无此问题。
#[gpui::test]
fn async_refresh_lands_incrementally(cx: &mut TestAppContext) {
    let text = "fn a() {\n    let x = {\n        y,\n    };\n}\nfn b() {\n    w();\n}";
    let (host, cx) = open(cx, text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    // 结构编辑：`x();` 后插入换成 + 更深块。
    cx.update(|_, cx| {
        ed.update(cx, |ed, cx| {
            let off = ed.offset_for_line_col(2, 0);
            ed.set_caret_to(off);
            ed.insert_text("\n        q();\n    }\n    r();\n}\nfn c() {", cx);
        })
    });
    settle(cx);
    // 渲染驱动下增量落地已就绪（对拍整篇扫描）且确实走了增量路径。
    assert_scan_parity(&host, cx);
    let (inc, full) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_scan_landings());
    assert!(inc >= 1, "异步后台刷新应走增量路径（inc={inc} full={full}）");
}




#[gpui::test]
fn quick_scroll_progressive_fill_reaches_full_color(cx: &mut TestAppContext) {
    // 分片补色回归：大 `.rs` 远跳（>160 行缺口）后，延后帧不再整窗空样式，而是从
    // **可见区顶部**起每帧补 `HL_FILL_BUDGET_ROWS` 行真颜色。两段验证：
    // A) 确定性补色状态机（`test_fill_step` 直接驱动步进，不经渲染帧）：①首步后
    //    可见区顶部若干行已入缓存（非空颜色）、窗口底未补（不是全窗重锚，告别全空）；
    //    ②同窗口下一步有色前缀变长；③多步补满 → 整窗有色（done）→ 下帧命中缓存；
    //    ④窗口变（仍在拖）→ 重锚回新可见顶、顶部带重新有色。
    // B) 真实渲染集成（`build_rows` 经 draw 跑；延后帧 `request_animation_frame` 预约
    //    下一帧、flush 驱动到静止）：远跳后最终满窗有色（告别 1s 死白），fill 会话
    //    复位（自动静止、无空转帧）。
    let text: String = (0..120_000)
        .map(|i| match i % 4 {
            0 => format!("fn foo{i}() {{ // c{i}\n"),
            1 => format!("let x: u32 = {i} + 1;\n"),
            2 => format!("core::writeln!(\"fmt {i}\");\n"),
            _ => format!("struct S{i} {{ a: u32, }}\n"),
        })
        .collect();
    let (host, cx) = open(cx, &text);
    let lh = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1)
        .max(1.);
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.set_language(Some("rust")));
    });
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let row_runs = |ed: &Entity<CodeEditor>, cx: &mut VisualTestContext, logical: usize| {
        cx.update(|_, cx| ed.read(cx).test_hl_row_runs(logical))
    };
    let fill_step = |ed: &Entity<CodeEditor>, cx: &mut VisualTestContext,
                     lo_l: usize,
                     hi_l: usize,
                     vis_top_l: usize| {
        cx.update(|_, cx| {
            ed.update(cx, |ed, _| ed.test_fill_step(lo_l, hi_l, vis_top_l))
        })
    };

    // A1) 首步（远窗口 [71900, 72060]，可见顶 71960）：只补可见区顶部一截（BUDGET 行），
    //     顶部已有非空颜色、窗口底未补（不是全窗重锚）。
    let (range1, done1) = fill_step(&ed, cx, 71_900, 72_060, 71_960);
    assert_eq!(range1.0, 71_960, "补色起点=可见区顶部 vis_top（非窗口 lo）");
    assert_eq!(range1.1, 71_960 + 31, "首步恰好补 BUDGET 行前缀");
    assert!(!done1, "首步未整窗覆盖（分片推进中）");
    assert!(
        (0..8).all(|k| row_runs(&ed, cx, 71_960 + k) > 0),
        "首步后可见区顶部前几行应已有非空颜色（不是全空）"
    );
    assert!(
        row_runs(&ed, cx, 72_052) == 0,
        "同帧窗口底仍未补（分片：本帧只补可见区顶部前缀）"
    );

    // A2) 同窗口下一步：有色前缀变长（游标继续下推）。
    let (range2, done2) = fill_step(&ed, cx, 71_900, 72_060, 71_960);
    assert!(range2.1 > range1.1, "同窗口下一帧有色前缀应变长（{}→{}）", range1.1, range2.1);
    assert!(
        (0..33).all(|k| row_runs(&ed, cx, 71_960 + k) > 0),
        "第二次步进后此前缀内全部有色（含上一帧的部分）"
    );
    assert!(!done2, "两步后仍未整窗覆盖");

    // A3) 继续步进到补满：整窗有色（done=true，缓存覆盖整个请求窗 → 下帧命中快路径）。
    let (_range3, done3) = fill_step(&ed, cx, 71_900, 72_060, 71_960);
    let (_range4, done4) = fill_step(&ed, cx, 71_900, 72_060, 71_960);
    assert!(done3 || done4, "多步后应整窗覆盖（done）");
    assert!(
        (72_000..72_060).all(|k| row_runs(&ed, cx, k) > 0),
        "多步补满后窗口深部也应有色（非只顶部前缀）"
    );
    let (_r5, done5) = fill_step(&ed, cx, 71_900, 72_060, 71_960);
    assert!(done5, "补满后保持 done（无空转）");
    let (_r6, done6) = fill_step(&ed, cx, 71_900, 72_060, 71_960);
    assert!(done6, "补满后反复步进保持 done（不重锚、不空转）");
    assert!(
        (72_000..72_060).all(|k| row_runs(&ed, cx, k) > 0),
        "补满后整窗保持有色（后续帧缓存命中）"
    );

    // A4) 窗口变了（仍在拖）：重锚回新可见顶，顶部带重新有色、新窗口深部未补。
    let (range7, done7) = fill_step(&ed, cx, 72_040, 73_200, 72_100);
    assert_eq!(range7.0, 72_100, "拖走后重锚回新可见顶");
    assert!(!done7, "新窗口步进中仍未覆盖全窗");
    assert!(
        (0..8).all(|k| row_runs(&ed, cx, 72_100 + k) > 0),
        "拖走后新窗口顶部也应立即有色带"
    );
    assert!(
        row_runs(&ed, cx, 73_192) == 0,
        "新窗口底部未补（拖动中仍在推进）"
    );

    // B) 真实渲染集成：远跳新窗口并 flush 到静止 → 语义同 A，走真实 draw 路径
    //    （含延后帧里 `request_animation_frame` 的下一帧回调），最终满窗有色且会话复位。
    cx.update(|_, cx| {
        let ed2 = host.read(cx).ed.clone();
        ed2.update(cx, |ed, _| ed.test_scroll_to_px(73_960.0 * lh));
    });
    for _ in 0..6 {
        let _ = cx.refresh();
        cx.update(|_, _| {});
    }
    assert!(
        row_runs(&ed, cx, 73_960) > 0 && row_runs(&ed, cx, 74_040) > 0,
        "真实渲染远跳后可见顶与更深行都应有色（告别死白）"
    );
    cx.update(|_, cx| {
        let ed2 = host.read(cx).ed.clone();
        ed2.update(cx, |ed, _| ed.test_scroll_to_px(74_000.0 * lh));
    });
    for _ in 0..6 {
        let _ = cx.refresh();
        cx.update(|_, _| {});
    }
    assert_eq!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_hl_fill()),
        Some(((0, 0), 0, 0)),
        "渲染静止后分片会话应复位（自动静止、无空转帧）"
    );
    assert!(
        row_runs(&ed, cx, 74_000) > 0 && row_runs(&ed, cx, 74_080) > 0,
        "再次远跳静止后新窗口整窗有色（告别死白）"
    );
}

// ---------------------------------------------------------------------------
// 超长行横向窗口化（见 `view::render::styled_row_text` / `long_line_window`）
// ---------------------------------------------------------------------------

#[gpui::test]
fn long_line_windows_to_visible_span(cx: &mut TestAppContext) {
    // 效果证据（真实渲染路径，`test_row_window_slice` 走生产同一 `long_line_window`）：
    // 200k 单行走窗口化后产出的文本长度 ≈ 可视窗口（远小于整行 200k），且随横滚
    // 左偏移跟随 hscroll（窗口更新不依赖新增缓存层，横滚即重绘）。
    let text = "x".repeat(200_000);
    let (host, cx) = open(cx, &text);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (cw, _, _) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let visible = cx.update(|_, cx| ed.read(cx).test_visible_chars());

    // 未横滚：窗口从行首起、文本长 ≈ 可视窗（+过扫余量），远小于整行 200k。
    let w0 = cx
        .update(|window, cx| ed.read(cx).test_row_window_slice(window, 0))
        .expect("200k 行应触发窗口化");
    let len0 = w0.0.end - w0.0.start;
    assert_eq!(w0.0.start, 0, "未横滚时窗口应始于行首");
    assert_eq!(w0.1, 0., "未横滚时左偏移应为 0");
    assert!(
        (20..2000).contains(&len0),
        "窗口文本应 ≈ 可视窗且 ≪ 200k（len={len0}，可视≈{visible}）"
    );
    assert!(
        len0 as f32 >= (visible as f32) * 0.5,
        "窗口至少覆盖大半可视窗（len={len0} visible={visible}）"
    );

    // 横向滚 ~5000px → 窗口跟随，左偏移 ≈ 已滚像素（±可视窗/格），文本仍 ≈ 可视窗。
    let ed_b = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + 200.),
        px(ed_b.origin.y.as_f32() + 50.),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(5_000.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(cx);
    let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(hscroll > 1000., "应已横向滚动（hscroll={hscroll}）");
    let w1 = cx
        .update(|window, cx| ed.read(cx).test_row_window_slice(window, 0))
        .expect("横滚后仍应窗口化");
    let len1 = w1.0.end - w1.0.start;
    assert!(
        (20..2000).contains(&len1),
        "横滚后窗口仍 ≈ 可视窗、≪ 行（len={len1}）"
    );
    assert!(w1.0.start > 0, "横滚后窗口应整体右移（start={}", w1.0.start);
    assert!(
        (w1.1 - hscroll).abs() <= (visible as f32) * cw,
        "窗口左偏移应跟随 hscroll（offset={:.0} hscroll={hscroll:.0} 可视px≈{:.0}）",
        w1.1,
        (visible as f32) * cw
    );
}

#[gpui::test]
fn long_line_cjk_window_offset_matches_real_layout(cx: &mut TestAppContext) {
    // 行为等价守卫：CJK 全角字真实字形宽 ≠ 2×格宽（Consolas 回退字形），窗口化若按
    // 「格宽×列」估算左偏移会在深滚时产生累积漂移、光标与文字错位。断言窗口偏移与
    // 整行真实整形宽（`row_layout` x_for_index，光标/命中同一口径）一致 → 裁剪文本
    // 与光标坐标严格对齐；窗口字节区间两端落在字符边界（不劈开 UTF-8）。
    let cjk = "一".repeat(4000); // 4000 全角 = 8000 单位 = 12000 字节（远超可视窗）
    let (host, cx) = open(cx, &cjk);
    settle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    // 滚到行中部（非量程尾，避免「整行已在可视窗内 → 回落整行」的退化分支）。
    let ed_b = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + 200.),
        px(ed_b.origin.y.as_f32() + 50.),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(2_000.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(cx);
    let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(hscroll > 500., "应已滚入行内（hscroll={hscroll}）");
    let w = cx
        .update(|window, cx| ed.read(cx).test_row_window_slice(window, 0))
        .expect("中部滚动应触发窗口化");
    assert!(w.0.start > 0, "窗口应始于行内（start={}", w.0.start);
    let real_start_x = cx
        .update(|window, cx| ed.read(cx).test_char_boundary_x(window, 0, w.0.start));
    assert!(
        (w.1 - real_start_x).abs() < 0.6,
        "窗口左偏移应与整行真实整形宽一致（offset={:.2} real={real_start_x:.2}）",
        w.1
    );
    // 裁剪两端均在字符边界（不劈开 CJK）→ 窗口末字节也应在边界上。
    let line = cx.update(|_, cx| host.read(cx).ed.read(cx).text_string());
    assert!(line.is_char_boundary(w.0.start) && line.is_char_boundary(w.0.end));
}

    /// 轨 B 基准（ignored）：点击 / 拖选 → paint 末尾「落帧」墙钟分布（min/median/p95）。
    /// 事件路径 = 生产 `CodeEditor::on_mouse_down/on_mouse_move`（含本轨新增 WY_TRACE
    /// `op_start("ed_click"/"ed_drag")`），本帧编辑器 paint 末尾 `op_finish_after_paint`
    /// 同帧结算——每轮计时「模拟事件 → run_until_parked + refresh（真实 paint）→ 落定」。
    /// 可选项：设 `BENCH_WY_TRACE_OUT=<path>` 时另端到端验证探针在 paint 末尾产出
    /// `OP kind=ed_click status=done` 行（WY_TRACE 同框架、真机同数据路径）。
    /// 运行：`cargo test -p editor bench_click_to_paint_frame_wall -- --ignored --nocapture`
    #[gpui::test]
    #[ignore]
    async fn bench_click_to_paint_frame_wall(cx: &mut TestAppContext) {
        use std::time::Instant;
        let text: String = (0..120)
            .map(|i| format!("line{i} some code content"))
            .collect::<Vec<_>>()
            .join("\n");
        let (host, cx) = open(cx, &text);
        focus(&host, cx);
        let lh = cx
            .update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1)
            .max(1.);
        // 预热：编辑区首帧布局
        for _ in 0..3 {
            cx.run_until_parked();
            let _ = cx.refresh();
            cx.run_until_parked();
        }
        let mods = Modifiers::default();
        const N: usize = 30;
        let mut clicks: Vec<f64> = Vec::with_capacity(N);
        for i in 0..N {
            // 点击点：x 避开 gutter，y 循环各行（点击即放置光标 → 重绘一帧）
            let x = px(40.0 + (i as f32 * 13.0) % 300.0);
            let y = px(lh * ((i % 15) as f32 + 0.5));
            cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
            let t0 = Instant::now();
            cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
            cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
            cx.run_until_parked();
            let _ = cx.refresh();
            cx.run_until_parked();
            clicks.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        // 拖选：按下 → 移动到右侧 → 抬起（移动那步一帧，量「拖选 → 落帧」）
        let mut drags: Vec<f64> = Vec::with_capacity(N);
        for i in 0..N {
            let y = px(lh * ((i % 15) as f32 + 0.5));
            let x1 = px(40.0);
            let x2 = px(40.0 + 80.0 + (i as f32 * 5.0) % 100.0);
            cx.simulate_mouse_move(point(x1, y), MouseButton::Left, mods);
            cx.simulate_mouse_down(point(x1, y), MouseButton::Left, mods);
            let t0 = Instant::now();
            cx.simulate_mouse_move(point(x2, y), MouseButton::Left, mods);
            cx.run_until_parked();
            let _ = cx.refresh();
            cx.simulate_mouse_up(point(x2, y), MouseButton::Left, mods);
            cx.run_until_parked();
            let _ = cx.refresh();
            cx.run_until_parked();
            drags.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        // 可选项：端到端验证探针在 paint 末尾结算 OP 行（WY_TRACE 同框架）。
        if let Ok(out) = std::env::var("BENCH_WY_TRACE_OUT") {
            unsafe { std::env::set_var("WY_TRACE", &out) };
            let _ = std::fs::remove_file(&out);
            let x = px(60.);
            let y = px(lh * 1.5);
            cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
            cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
            cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
            cx.run_until_parked();
            let _ = cx.refresh();
            cx.run_until_parked();
            let body = std::fs::read_to_string(&out).unwrap_or_default();
            assert!(
                body.lines().any(|l| l.contains("kind=ed_click") && l.contains("status=done")),
                "WY_TRACE 应在 paint 末尾结算 ed_click 行:\n{body}"
            );
            unsafe { std::env::remove_var("WY_TRACE") };
        }
        let stat =
            |v: &mut Vec<f64>| -> (f64, f64, f64) {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            (
                v[0],
                v[v.len() / 2],
                v[(v.len() as f64 * 0.95) as usize - 1],
            )
        };
        let (c_min, c_med, c_p95) = stat(&mut clicks);
        let (d_min, d_med, d_p95) = stat(&mut drags);
        println!("==== 点击/拖选 → 落帧墙钟（debug，headless；真机以 WY_TRACE 的 OP total= 行为准） ====");
        println!("  单击(ed_click) N={N}: min={c_min:.3}ms median={c_med:.3}ms p95={c_p95:.3}ms");
        println!("  拖选(ed_drag)  N={N}: min={d_min:.3}ms median={d_med:.3}ms p95={d_p95:.3}ms");
    }


/// 回归：首帧绘制/指标准备**之前**的 `scroll_to_line`（阅读进度恢复在 apply_text_restore
/// 经 on_next_frame 触发，先于新鲜编辑区元素首帧 paint → `line_height` 仍 0）不得把目标
/// 行按回退行高 1.0 换算成 px 写进 `desired_scroll`——否则首个真实指标落地时
/// `apply_desired_to_window` 把 `N×1px` 重映射到 `N/真实行高` 行（真机「md 重开：编辑区
/// 被拉回顶端 47→2」根因，rp_diag：apply after=47 → 下一帧 check top_line=2；headless
/// 测不出因为它给帧前指标已就绪）。修复 = 未就绪只暂存**逻辑行**，指标就绪后按真实
/// 行高落地（`refresh_metrics` 消费 `pending_scroll_line`）。
#[gpui::test]
fn early_line_scroll_survives_first_metrics_resolution(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let text = (0..89)
        .map(|i| format!("第 {i} 行"))
        .collect::<Vec<_>>()
        .join("\n");
    // 无窗口创建编辑器：绝不触发首帧 render → 指标未就绪（line_height==0）保真。
    let ed = cx.update(|cx| {
        let ed = cx.new(|cx| {
            let mut e = CodeEditor::new(cx);
            e.set_text(&text);
            e
        });
        ed.update(cx, |e, _| e.scroll_to_line(47));
        assert_eq!(
            ed.read(cx).test_desired_scroll_px(),
            0.,
            "指标未就绪时 scroll_to_line 不得写 desired（防 px 错拍）"
        );
        ed
    });
    // 挂进窗口渲染：首帧 `refresh_metrics` 解析真实行高并按真实行高落地暂存逻辑行——
    // 视口顶必须停在源码行 47。
    let (host, cx) = cx.add_window_view(|_window, _cx| Host { ed: ed.clone() });
    for _ in 0..6 {
        cx.run_until_parked();
        let _ = cx.refresh();
        cx.run_until_parked();
    }
    let top = cx.update(|_, cx| host.read(cx).ed.read(cx).viewport_top_line());
    assert_eq!(
        top, 47,
        "首帧前滚到 47：指标准备后视口顶应仍=47（不被 px 重映射回拢）"
    );
}

#[gpui::test]
fn hscrollbar_overlay_container_flush_to_editor_bottom(cx: &mut TestAppContext) {
    // 结构护栏（非 TDD 复现——评审 stash 实证本量法在修复前后皆绿）：横向滚动条叠层
    // `#ce-hscrollbar-overlay`（`absolute inset_0`，render.rs hscrollbar_overlay）必须
    // 撑满并贴住 `#code-editor` 底边。若有人给编辑区/叠层加底部 padding、或父容器
    // 不给足高，此护栏即红。
    //
    // ⚠️ 本护栏**不**测横条自身的可见贴底（滑块/轨道胶囊交叉轴底缘）——叠层底 ==
    // 编辑区底只是该可见贴底的几何前提；横条贴底行为本身由 `ui::scrollbar` 单测
    // `geom` 锁（水平贴底精确定位、旧几何会红）。两层各司其职，勿把本护栏当复现。
    let text = "x".repeat(500);
    let (_host, cx) = open(cx, &text);

    // 横向溢出确证：500 个 x 远超一屏（>1920px）→ 滚动条轨道几何存在（几何存在才有绘制）。
    let wide = cx
        .debug_bounds("ce-hscrollbar-overlay")
        .expect("横向滚动条叠层应渲染");
    let editor = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    assert!(
        wide.size.width > px(0.) && wide.size.height > px(0.),
        "叠层应撑满编辑区（{wide:?}）"
    );

    // 结构不变式：叠层（其中水平 Scrollbar 的 viewport）底缘 == 编辑区底缘。
    let track_bottom = wide.bottom();
    let area_bottom = editor.bottom();
    let layout_gap = (area_bottom - track_bottom).max(px(0.));
    eprintln!(
        "[HSCROLL] editor.bottom={:?} overlay.bottom={:?} overlay.h={:?} editor.h={:?}",
        area_bottom,
        track_bottom,
        wide.size.height,
        editor.size.height
    );
    assert!(
        layout_gap < px(1.),
        "横向滚动条叠层底未贴编辑区底边：空隙 {layout_gap:?}"
    );
}

/// guidefix 1-16 是**故意错乱**夹具（K150/K152）：禁逐行期望对齐，只做不变式断言。
/// 块体模型不变式（2026-11-14 用户实测口径）：逐 0idx 行断言引擎
/// `test_active_guide_group` == 独立侧 `block_active_guide_port`（同算法；「任一行关联 P
/// 或 None」——P = 块体模型 1/2/3 关联括对，体区间 = `[open+1, close)` 半开；None 则两边
/// 同为 None）；Some 时体区间非空 `first < last` 且不越界（last ≤ vcount）。
#[gpui::test]
fn indent_guides_active_group_invariants_on_messy_fixture(cx: &mut TestAppContext) {
    let text = guidefix_fixture_text();
    let (host, cx) = open(cx, text);
    let vcount = cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().visual_line_count());
    assert!(vcount > 0, "guidefix 应有可视行");
    let nrows = (text.split('\n').count()).min(vcount);
    for caret_vis in 0..nrows {
        set_caret_row(&host, cx, caret_vis);
        let engine =
            cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_guide_group());
        let port = block_active_guide_port(text, caret_vis);
        assert_eq!(
            engine, port,
            "行 {caret_vis}: 引擎 ≠ 块体模型 port（乱夹具任一行同算法）"
        );
        if let Some((_col, f, l)) = engine {
            assert!(
                f < l && l <= vcount,
                "行 {caret_vis}: 体区间 [{f},{l}) 应非空且不越界（vcount={vcount}）"
            );
        }
    }
}

/// 块体模型（guidefix 乱夹具）：点 0idx12（尾部 `z1();`）→ 关联括对 = 严格含 12 的最内块
/// `if x1`（open2/close13，depth 最大）→ 体 `[visual(2)+1, visual(13))` = [3,13)、列 = 引导列 4
/// → (4, 3, 13)（整块体点亮，不再「只亮孤立光标连续段」）。
#[gpui::test]
fn indent_guides_active_repro_deep_row_isolated_segment_only(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, guidefix_fixture_text());
    set_caret_row(&host, cx, 12);
    let group = cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_guide_group());
    assert_eq!(
        group,
        Some((4, 3, 13)),
        "点 0idx12 → 块体模型：if x1 块体 [3,13)（z1(); 所在最内块体）"
    );
    assert_ne!(group, Some((4, 6, 8)), "非 if x==1 块体（0idx6/7 那个块）");
    assert_ne!(group, Some((4, 12, 13)), "非「仅光标连续段」旧语义");
}

// ---------------------------------------------------------------------------
// 缩进引导线 active 视觉（块体模型 = 括号块体高亮，2026-11-14 用户 3 夹具实测唯一真值；
// 顶部 L 横线 active_line_marker_overlay 已删，仅剩 active 竖线灰系高亮，见 layout.rs 颜色口径）
// ---------------------------------------------------------------------------

/// 更深嵌套多行块（块体模型 TDD 期望表）：`fn main(){\n let a=1;\n if b==2 {\n x1();\n
/// x2();\n }\n let c=3;\n}`。配对 fn(0,7,d0)、if(2,5,d1)：
/// - r0（fn 开行）→ ① open==0 → fn 体 [1,7) col0；r1（let a）/ r6（let c）→ ② fn 严格含
///   → fn 体 [1,7) col0；
/// - r2（if 开行）→ ① open==2 → if 体 [3,5) col4；r3/r4（x1/x2）→ ② 最内 if；
/// - r5（if `}`）→ ② fn 严格含 5（if 不含闭行）→ fn 体 [1,7) col0；
/// - r7（fn `}`）→ ③ close==7 → fn 体 [1,7) col0。
#[gpui::test]
fn indent_guides_active_group_nested_clean_fixture(cx: &mut TestAppContext) {
    let text = "fn main() {\n    let a = 1;\n    if b == 2 {\n        x1();\n        x2();\n    }\n    let c = 3;\n}";
    let (host, cx) = open(cx, text);
    let cases: &[(usize, Option<(usize, usize, usize)>)] = &[
        (0, Some((0, 1, 7))), // ① fn 开行 → fn 体
        (1, Some((0, 1, 7))), // ② fn 严格含 → fn 体
        (2, Some((4, 3, 5))), // ① if 开行 → if 体
        (3, Some((4, 3, 5))), // ② 最内 if
        (4, Some((4, 3, 5))), // ② 最内 if
        (5, Some((0, 1, 7))), // if `}` → ② fn 严格含 → fn 体
        (6, Some((0, 1, 7))), // ② fn 严格含 → fn 体
        (7, Some((0, 1, 7))), // ③ fn `}` → fn 体
    ];
    for &(row, want) in cases {
        set_caret_row(&host, cx, row);
        let group = cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_guide_group());
        assert_eq!(
            group,
            want,
            "行 {row}（=1基{}）group 期望 {want:?}（块体模型）",
            row + 1
        );
    }
}

/// **块体模型端口**（测试锚，2026-11-14 用户 3 夹具实测唯一真值）：与引擎
/// `active_pair` / `active_guide_group` 同算法的独立侧实现（配对复用 `fold::scan_bracket_pairs`
/// = 与引擎同口径的缩进分层配对；模型 1/2/3 + 体区间 + 列独立推演）。
/// 输入整段文本 + 0idx 逻辑行 r，返回 `(列, 体首可视, 体末可视+1)`（无折叠时可视行==逻辑行）：
/// - 关联 P：① open_line == r ② open_line < r < close_line（严格含，最内块）③ close_line == r
///   ——按序取首个非空层，各层取 depth 最大者；无 → None。
/// - 列 = `guide_pair_column` 同式（min(开列 units, 闭列 units, 对内最小非闭缩进+1)）。
/// - 体 = `[open+1, close)` 半开（不含括号两行）；体空 → None。
fn block_active_guide_port(text: &str, r: usize) -> Option<(usize, usize, usize)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let n = lines.len();
    let pairs = crate::editor::fold::scan_bracket_pairs(
        |i: usize| {
            lines
                .get(i)
                .map(|s| gpui::SharedString::from(*s))
                .unwrap_or_else(|| gpui::SharedString::new_static(""))
        },
        n,
    );
    use crate::editor::fold::BracketPair;
    // 列（与引擎 `guide_pair_column_fields` 同式）。
    let units_upto = |line: usize, byte_col: usize| -> usize {
        let s = lines.get(line).copied().unwrap_or("");
        let upto = byte_col.min(s.len());
        s[..upto]
            .chars()
            .map(crate::editor::buffer::char_width_units)
            .sum()
    };
    let inner_min_noncloser = |open: usize, close: usize| -> Option<usize> {
        const CAP: usize = 1024;
        if close.saturating_sub(open).saturating_sub(1) > CAP {
            return None;
        }
        let mut min: Option<usize> = None;
        for rr in (open + 1)..close {
            let s = lines.get(rr).copied().unwrap_or("");
            let t = s.trim_start();
            if t.is_empty() {
                continue;
            }
            match t.as_bytes()[0] {
                b'}' | b')' | b']' => continue,
                _ => {}
            }
            let u = crate::editor::fold::leading_indent(s).units();
            min = Some(min.map_or(u, |m| m.min(u)));
        }
        min
    };
    let pair_col = |p: &BracketPair| -> usize {
        let o = units_upto(p.open_line, p.open_col);
        let c = units_upto(p.close_line, p.close_col);
        match inner_min_noncloser(p.open_line, p.close_line) {
            Some(m) => o.min(c).min(m + 1),
            None => o.min(c),
        }
    };
    // 块体模型 1/2/3：按序取首个非空层，各层取 depth 最大（`block_pick_max_pair`）。
    let step1: Vec<&BracketPair> = pairs.iter().filter(|p| p.open_line == r).collect();
    let chosen = if !step1.is_empty() {
        block_pick_max_pair(step1)
    } else {
        let step2: Vec<&BracketPair> = pairs
            .iter()
            .filter(|p| p.open_line < r && r < p.close_line)
            .collect();
        if !step2.is_empty() {
            block_pick_max_pair(step2)
        } else {
            let step3: Vec<&BracketPair> = pairs.iter().filter(|p| p.close_line == r).collect();
            block_pick_max_pair(step3)
        }
    }?;
    let first = chosen.open_line + 1;
    if first >= chosen.close_line {
        return None; // 体空
    }
    Some((pair_col(chosen), first, chosen.close_line))
}

/// `block_active_guide_port` 的共享取大：候选取 depth 最大（跳过过期对；等 depth 保留首个，
/// 与引擎 `active_pair` 的 `pick_max_depth` 同约定）。
fn block_pick_max_pair<'a>(
    cands: Vec<&'a crate::editor::fold::BracketPair>,
) -> Option<&'a crate::editor::fold::BracketPair> {
    let mut best: Option<&'a crate::editor::fold::BracketPair> = None;
    for p in cands {
        if p.open_line >= p.close_line {
            continue;
        }
        match best {
            None => best = Some(p),
            Some(b) if b.depth < p.depth => best = Some(p),
            _ => {}
        }
    }
    best
}

/// **zip 锚：引擎 `active_guide_group` == `block_active_guide_port`**（2026-11-14 用户 3 夹具
/// 实测唯一真值；块体模型 1/2/3 + 体区间 `[open+1, close)`），覆盖 干净/嵌套/乱夹具/
/// guidefix2/空白行/深缩进全部内容行。任何一行分叉 = 引擎与 block_port 之一有 bug（两边同
/// 算法、彼此对拍——「两者不符就是实现 bug」）。
#[gpui::test]
fn active_guide_group_matches_block_port(cx: &mut TestAppContext) {
    let fixtures: &[&str] = &[
        // 干净单/多层（真值表 focus）
        "fn main() {\n    if a==1 {\n        foo();\n        bar();\n    }\n    baz();\n}",
        "fn main() {\n    let a = 1;\n    if b == 2 {\n        x1();\n        x2();\n    }\n    let c = 3;\n}",
        "fn main() {\n    bar();\n    baz();\n}",
        // 空白行桥接
        "fn a() {\n    if x {\n        \n    }\n}",
        "fn a() {\n\n    x();\n    y();\n\n}",
        // 深缩进 & 多块同列
        "a() {\n        x();\n        y();\n}\n    z();\n    w();\n",
        // 乱夹具 1-16（不变式；逐行值以 port 为准）
        guidefix_fixture_text(),
        // guidefix2（真实 .rs 文本；块体关键修复：点 div()/链式续行 .id() → fn 体 [2,20)）
        guidefix2_fixture_text(),
    ];
    for text in fixtures {
        let (host, cx) = open(cx, text);
        let vcount = cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().visual_line_count());
        // 不含幻影尾行（vcount 含尾换行幻影；比较 0..vcount 真实内容行）。
        let content_rows = (text.split('\n').count().max(vcount)).min(vcount);
        for row in 0..content_rows {
            let want = block_active_guide_port(text, row);
            set_caret_row(&host, cx, row);
            let got = cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_guide_group());
            assert_eq!(
                got, want,
                "夹具 {text:?} 行 {row}: 引擎 {got:?} ≠ 块体模型 port {want:?}"
            );
        }
    }
}


/// headless 断言用的高级 getter `test_active_guide_group` 曾与渲染**分叉**（getter 已整带、
/// 渲染仍按「含光标可视行的最近段」）。本测试让两条路径
/// **走同一 active 派生**（`active_guide_group()` → flagged 段化），对干净夹具逐关键行断言：
/// flagged 的 hl==true 段换算回 `(列, 行首, 行末+1)` 与 getter group 区间**逐位一致**（整块
/// 体点亮：点 foo()/bar()（0idx2/3）→ if 体 col4 [2,4) 两行都亮、不只光标行；点 0idx4 `}` /
/// 0idx5 baz() → fn 体 col0 [1,6) 整块体亮）。
/// active 竖线灰系高亮（VSCode 默认色）、无顶部横线。
#[gpui::test]
fn indent_guides_render_path_matches_active_group(cx: &mut TestAppContext) {
    let text = "fn main() {\n    if a==1 {\n        foo();\n        bar();\n    }\n    baz();\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let cw = cw.max(1.);
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    // (光标 0idx 行, 期望整块体 hl 段 [(列, 行首, 行末半开)])——对照
    // zip 锚 `active_guide_group_matches_block_port`（块体模型真值表）：
    //   0idx0（fn 开行）/ 0idx4（if `}`，fn 严格含）/ 0idx5（baz）→ fn 体 col0 [1,6)；
    //   0idx1（if 开行）~ 0idx3 → if 体 col4 [2,4)（foo/bar 两行都亮，不只光标行）。
    // 关键覆盖：渲染路径须与 getter 同源同值（K153 封口），含 ① 开行（0idx0/1 → 该块体）
    // 与闭行归外/自身块体（0idx4 → fn）——封口防两路径再分叉。
    let cases: &[(usize, &[(usize, usize, usize)])] = &[
        (0, &[(0, 1, 6)]),
        (1, &[(4, 2, 4)]),
        (2, &[(4, 2, 4)]),
        (3, &[(4, 2, 4)]),
        (4, &[(0, 1, 6)]),
        (5, &[(0, 1, 6)]),
    ];
    for &(row, want) in cases {
        set_caret_row(&host, cx, row);
        let (flag_hl, group) = cx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            let ed = ed.read(cx);
            // 与渲染（`indentation_guide_overlay`）同式派生 active：group →
            // `(col, 组首, 组末+1, 光标可视行)`。干净夹具无折叠 → 光标可视行 == 0idx 行
            // （与 render 的 `visual_row(logical)` 换算一致）。
            let group = ed.test_active_guide_group();
            let active = group.map(|(col, f, l_excl)| (col, f, l_excl, row));
            let segs = ed.test_indent_guide_segments_flagged(
                0, 0.0, lh, 1.0, 2000.0, 4000.0, active,
            );
            // hl==true 段 → (列, 行首, 行末+1)（与 group 区间同半开口径）。
            let flag_hl: Vec<(usize, usize, usize)> = segs
                .into_iter()
                .filter(|(_, _, _, hl)| *hl)
                .map(|(x, y, h, _)| {
                    (
                        (x / cw).round() as usize,
                        (y / pitch).round() as usize,
                        ((y + h) / pitch).round() as usize,
                    )
                })
                .collect();
            (flag_hl, group)
        });
        assert_eq!(
            flag_hl.as_slice(),
            want,
            "行 {row}: 渲染路径 hl 段应整组 {want:?}（不只光标行）; 实际 {flag_hl:?}"
        );
        let want_group: Vec<(usize, usize, usize)> = group
            .map(|(c, f, l_excl)| vec![(c, f, l_excl)])
            .unwrap_or_default();
        assert_eq!(
            flag_hl.as_slice(),
            want_group.as_slice(),
            "行 {row}: 渲染路径 hl 段应 == getter group 区间（两条路径同规则）; \
             {flag_hl:?} vs {want_group:?}"
        );
    }
}

/// 旧运行时 `guide_inner_min_noncloser` 的**测试参照**（生产路径已删、仅测试区保留，
/// K153 单源——生产只走预存字段，旧算法留在测试作 parity 唯一参照，防复刻走样/双路径漂移）：
/// 体内（`开+1 .. 闭−1`）最小「非闭行缩进 units」，跳过空行与「首非空白为 `} ) ]`」的行
/// （closer，不参与列下吸）；跨度 > 1024（防退化）或无合格行 → `None`（= +∞）。
/// 返回 `usize` 口径与预存字段（`u32`）语义一致。
fn guide_inner_min_noncloser_ref(rope: &ropey::Rope, open: usize, close: usize) -> Option<usize> {
    const INNER_SCAN_CAP: usize = 1024;
    if close.saturating_sub(open).saturating_sub(1) > INNER_SCAN_CAP {
        return None;
    }
    let mut min: Option<usize> = None;
    for r in (open + 1)..close {
        let s = crate::editor::buffer::line_shared(rope, r);
        let s = s.as_ref();
        let t = s.trim_start();
        if t.is_empty() {
            continue;
        }
        let first = t.as_bytes()[0];
        if first == b'}' || first == b')' || first == b']' {
            continue;
        }
        let units = crate::editor::fold::leading_indent(s).units();
        min = Some(min.map_or(units, |m| m.min(units)));
    }
    min
}

/// **落地期预存字段 parity**（K153 单源 / K160）：对全部夹具/支线的每对 `BracketPair`，
/// 断言 `guide_body_min_units`（折叠表扫描/增量期算好、帧内 O(1) 只读）== 旧运行时算法的
/// 结果（[`guide_inner_min_noncloser_ref`] 参照，与生产旧实现逐字节同语义）。覆盖：干净多层、
/// 单行块、欠缩进体内、深嵌套、多空行块、巨型括对（600 内行有值 / >1024 内行 → cap None）、
/// 乱夹具（guidefix / guidefix2）。生产只留字段这一个真值源——旧算法生产路径已删。
#[gpui::test]
fn guide_body_min_units_matches_old_runtime_parity(cx: &mut TestAppContext) {
    // 巨型括对：600 内行（<1024 → 应算值）；另 1100 内行（>1024 → cap → None）。
    let giant = {
        let mut t = String::from("fn big() {\n");
        for _ in 0..600 {
            t.push_str("    x();\n");
        }
        t.push('}');
        t
    };
    let giant_capped = {
        let mut t = String::from("fn big() {\n");
        for _ in 0..1100 {
            t.push_str("    x();\n");
        }
        t.push('}');
        t
    };
    let texts: Vec<String> = vec![
        // 干净多层（块体模型共源夹具）
        "fn main() {\n    if a==1 {\n        foo();\n        bar();\n    }\n    baz();\n}".into(),
        // 单行块 + 欠缩进体内（下拉列）
        "fn a() {\n y();\n    z();\n}".into(),
        // 深嵌套 + 同列多块
        "a() {\n        x();\n        y();\n}\n    z();\n    w();\n".into(),
        // 空行桥接 / 空体
        "fn a() {\n\n    x();\n    y();\n\n}".into(),
        "fn a() {\n    if x {\n        \n    }\n}".into(),
        // 闭行首更深的体内行 + 注释/字符串行（旧实现不跳过注释——仅列首缩进，须逐位对齐）
        "fn a() {\n  // comment\n    x();\n}".into(),
        // 乱夹具 / 真实 .rs
        guidefix_fixture_text().into(),
        guidefix2_fixture_text().into(),
        giant,
        giant_capped,
    ];
    for text in &texts {
        let (host, cx) = open(cx, text);
        let rope = ropey::Rope::from_str(text);
        let pairs = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bracket_pairs().to_vec());
        assert!(
            !pairs.is_empty(),
            "夹具应至少有一对跨行括对（{text:?}; 前 {} 字）",
            text.chars().take(40).collect::<String>()
        );
        for p in &pairs {
            let want = guide_inner_min_noncloser_ref(&rope, p.open_line, p.close_line);
            let got = p.guide_body_min_units.map(|u| u as usize);
            assert_eq!(
                got, want,
                "夹具 {text:?} 配对 ({},{})：预存字段 {got:?} ≠ 旧运行时算法 {want:?}",
                p.open_line, p.close_line
            );
        }
    }
}

/// **性能旁证**（仅打印、不落断言；`#[ignore]` 手动跑）——F3 级巨型括对（600 内行）下：
/// ① 旧运行时算法一次「逐内行 line_shared」耗时（示意旧路径逐帧 ×3 消费方的历史成本量级）；
/// ② 新路径 `test_bracket_guide_segments` 整趟（视口窗口 + `guide_pair_column` O(1) 读字段 +
/// 段合并）多轮中位。跑法：`cargo test -p editor guide_body_min_units_render_probe -- --ignored --nocapture`。
#[gpui::test]
#[ignore]
fn guide_body_min_units_render_probe(cx: &mut TestAppContext) {
    use std::time::Instant;
    let mut text = String::from("fn f() {\n");
    for _ in 0..600 {
        text.push_str("    x();\n");
    }
    text.push_str("}");
    let (host, cx) = open(cx, &text);
    let (_, lh, _) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let pairs = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bracket_pairs().to_vec());
    assert_eq!(pairs.len(), 1, "巨型括对应只产 1 对: {pairs:?}");
    let p = pairs[0];
    // ① 旧运行时逐内行扫（测试参照 = 历史逐帧行为量级）。
    let rope = ropey::Rope::from_str(&text);
    let t0 = Instant::now();
    let _ref_min = guide_inner_min_noncloser_ref(&rope, p.open_line, p.close_line);
    let ref_elapsed = t0.elapsed();
    eprintln!(
        "[PROBE] 旧运行时逐内行扫一次 (pair {}-{}): {:?} —— 旧路径逐帧 ×3 消费方 ≈3× 此值",
        p.open_line, p.close_line, ref_elapsed
    );
    // ② 新路径整趟（O(1) 读字段）多轮中位。
    let mut samples = Vec::new();
    for _ in 0..30 {
        let t1 = Instant::now();
        let segs = cx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            ed.read(cx).test_bracket_guide_segments(
                0,
                0.0,
                lh,
                1.0,
                2000.0,
                4000.0,
                config::layout::BracketGuidesMode::All,
            )
        });
        samples.push(t1.elapsed());
        assert!(!segs.is_empty(), "巨型括对应产竖线");
    }
    samples.sort();
    eprintln!(
        "[PROBE] 新路径 test_bracket_guide_segments 整趟（30 次中位）: {:?}",
        samples[samples.len() / 2]
    );
}

/// 把光标移到 0idx 行 `row`（`set_selection_range` 同生产路径，方向键/点击同样派生）。
fn set_caret_row(host: &Entity<Host>, cx: &mut VisualTestContext, row: usize) {
    let off = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(row, 0));
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, cx| ed.set_selection_range(off..off, cx));
    });
    settle(cx);
}

/// 缩进引导线两色完全对齐 VSCode 默认主题（2026-11-14，用户口径）：
/// - 亮（Light+）非 active `guide_inactive` = #D3D3D3、active `guide_active` = #939393；
/// - 暗（Dark+）非 active = #404040、active = #707070。
/// 通过 `Theme::set(Dark)` + `settle` 切主题后重渲染读回（`refresh_metrics` 每帧无条件写颜色
/// 字段，主题切换下一帧生效）——同 `indent_guides_render_path_matches_active_group` 的 getter
/// 用法；渲染路径与 getter 同源（K153 封口：布局只走 `guide_inactive`/`guide_active`）。
#[gpui::test]
fn indent_guides_colors_match_vscode_default(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "fn a() {\n    x();\n}");
    focus(&host, cx);
    settle(cx);
    let read_colors = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_guide_colors())
    };
    // 默认亮主题：VSCode Light+ 灰。
    let (g, a) = read_colors(cx);
    let want_g: gpui::Hsla = gpui::rgb(0xd3d3d3).into();
    let want_a: gpui::Hsla = gpui::rgb(0x939393).into();
    assert_eq!(g, want_g, "亮非 active = VSCode Light+ #D3D3D3");
    assert_eq!(a, want_a, "亮 active = VSCode Light+ #939393");
    // 切暗主题 → 重渲染 → VSCode Dark+ 灰。
    cx.update(|_, cx| ui::theme::Theme::set(ui::theme::ThemeMode::Dark, cx));
    settle(cx);
    let (g, a) = read_colors(cx);
    let want_g: gpui::Hsla = gpui::rgb(0x404040).into();
    let want_a: gpui::Hsla = gpui::rgb(0x707070).into();
    assert_eq!(g, want_g, "暗非 active = VSCode Dark+ #404040");
    assert_eq!(a, want_a, "暗 active = VSCode Dark+ #707070");
}

/// 把光标移到 0idx 行 `row`、字符列 `col`（字节偏移；`offset_for_line_col` 同生产定位路径，
/// 方向键/点击同样派生，供 `active_pair` 严格内含用例）。
fn set_caret_row_col(host: &Entity<Host>, cx: &mut VisualTestContext, row: usize, col: usize) {
    let off = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(row, col));
    cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, cx| ed.set_selection_range(off..off, cx));
    });
    settle(cx);
}

/// 括号引导线按 `depth % 3` 循环池上色（第 3 片，VSCode bracket guides）——**异 depth 段
/// 不合并**、各自成段并各自取色。干净夹具两层，演算（guide_pair_column = min(开列, 闭列,
/// 对内最小非闭缩进+1)，HEAD=TAIL=0）：
///   `fn a{`(0,c4)  `    if x{`(1,c8)  `        y();`(2)  `    }`(3,c4)  `}`(4,c0)
///   - 外层 fn a（depth0）：min(4, 0)=0 → col0；
///   - 内层 if x（depth1）：min(8, 4)=4 → col4。
/// 断言每段的 depth/列与「渲染同源」的 `test_bracket_guide_color(depth, active)` 对上
/// （getter 与渲染同式：palette[depth%3] · alpha 0.3/1.0）。
#[gpui::test]
fn bracket_guides_colored_by_depth(cx: &mut TestAppContext) {
    let text = "fn a{\n    if x{\n        y();\n    }\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    settle(cx);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let (segs, colors) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        let segs = ed.test_bracket_guide_segments_flagged(
            0, 0.0, lh, 1.0, 1200.0, 1000.0, config::layout::BracketGuidesMode::All,
        );
        let colors = (0..3)
            .map(|d| (ed.test_bracket_guide_color(d, false), ed.test_bracket_guide_color(d, true)))
            .collect::<Vec<_>>();
        (segs, colors)
    });
    // 两个括对（异 depth）各自成段：col0 depth0 + col4 depth1。
    // 光标默认 (0,0) = fn 开行 → 块体模型 ① 关联 fn → fn 段 active（hl=true）、if 段 inactive。
    assert_eq!(segs.len(), 2, "两层括对 → 两根竖线（异 depth 不合并）");
    let norm: Vec<(i64, usize, bool)> = segs
        .iter()
        .map(|(x, _, _, d, hl)| ((x / cw).round() as i64, *d, *hl))
        .collect();
    assert!(
        norm.contains(&(0, 0, true)),
        "col0 段应为 depth0 且 active（光标在 fn 开行，块体模型 ①）: {norm:?}"
    );
    assert!(
        norm.contains(&(4, 1, false)),
        "col4 段应为 depth1 且 inactive: {norm:?}"
    );
    // 颜色：active 段 = test_bracket_guide_color(depth, true) alpha 1.0；inactive 段 =
    // test_bracket_guide_color(depth, false) alpha 0.3；深度间不同色。
    let col_of = |d: usize| colors[d].0;
    assert!(
        col_of(0) != col_of(1),
        "palette[depth%3] 按深度取色应互异（VSCode 3 色循环池）"
    );
    for &(_, d, hl) in &norm {
        if hl {
            let c_act = colors[d].1;
            assert_eq!(c_act.a, 1.0, "active 段 alpha = 1.0（depth {d}）");
        } else {
            let c_in = colors[d].0;
            assert_eq!(c_in.a, 0.3, "inactive 段 alpha = 0.3（depth {d}）");
        }
    }
    // active 仅抬 alpha、hue/sat/light 与 inactive 同源（块体活跃段 getter 公式覆盖）。
    let (c_in, c_act) = colors[0];
    assert_eq!(
        (c_act.h, c_act.s, c_act.l),
        (c_in.h, c_in.s, c_in.l),
        "active 只改 alpha，不改 hue/sat/light"
    );
    assert_eq!(c_act.a, 1.0, "active 段 alpha = 1.0");
}

/// E3 封口（K153，2026-11-14）：bracket 竖线取色 = `bracket_guide_color` 单源。渲染竖线
/// 段（`bracket_guide_overlay` 内现直调 `bracket_guide_color(palette, depth, active)`）、
/// 钩线（active）、active 缩进高亮（`active_guide_highlight_color` → helper active 色）、
/// 测试 getter（`test_bracket_guide_color`）四处与 helper 输出逐项断言一致，防取色公式
/// 再散写分叉（呼应 `indent_guides_render_path_matches_active_group` 的封口姿势：渲染
/// 路径与 getter 走同一公式）。
#[gpui::test]
fn bracket_guide_color_render_path_matches_helper(cx: &mut TestAppContext) {
    use crate::editor::view::bracket_guide_color;
    // 两层括对干净夹具：fn col0（depth0）+ if col4（depth1）。
    let text = "fn a{\n    if x{\n        y();\n    }\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    settle(cx);
    let (palette, seg_cols, getter_cols) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        let lh = ed.test_metrics().1.max(1.);
        let palette: [gpui::Hsla; 3] = ed.test_bracket_guide_palette();
        // 渲染竖线段 → 真实产出字段 (depth, active)：render.rs:376 竖线条即消费自本路径的
        // 这些字段计算颜色（`bracket_guide_color(&palette, depth, active)`）。
        let segs = ed.test_bracket_guide_segments_flagged(
            0,
            0.0,
            lh,
            1.0,
            1200.0,
            1000.0,
            config::layout::BracketGuidesMode::All,
        );
        let seg_cols: Vec<(usize, bool, gpui::Hsla)> = segs
            .iter()
            .map(|&(_, _, _, d, hl)| (d, hl, bracket_guide_color(&palette, d, hl)))
            .collect();
        // getter / 测试锚路径 = test_bracket_guide_color（现直调 helper；全 (depth, active)）。
        let getter_cols: Vec<(usize, bool, gpui::Hsla)> = (0..3)
            .flat_map(|d| {
                [
                    (d, false, ed.test_bracket_guide_color(d, false)),
                    (d, true, ed.test_bracket_guide_color(d, true)),
                ]
            })
            .collect();
        (palette, seg_cols, getter_cols)
    });
    // **独立真值锚**（不经过 helper/getter，逐字写预重构公式）：任何消费方若私下改成
    // 另一套公式（palette[d%2]、其他 alpha 等），对照真值必红——否决「helper 镜像 helper」
    // 空转（评审 NEEDS-CHANGES #1）。hue/sat/light 同源、active 只抬 alpha。
    let truth = |depth: usize, active: bool| -> gpui::Hsla {
        let c = palette[depth % 3];
        if active {
            c.opacity(1.0)
        } else {
            c.opacity(0.3)
        }
    };
    // 两层括对 → 两根竖线（异 depth 不合并）。渲染侧（真实段字段 → helper 输出）逐段
    // 对照独立真值；同时与 getter 交叉（两条路径各自 == 真值 → 必相互一致）。
    let getter_map: std::collections::HashMap<(usize, bool), gpui::Hsla> =
        getter_cols.iter().map(|&(d, hl, c)| ((d, hl), c)).collect();
    assert_eq!(seg_cols.len(), 2, "两层括对 → 两根竖线（异 depth 不合并）");
    for &(d, hl, c) in &seg_cols {
        assert_eq!(
            c,
            truth(d, hl),
            "渲染侧取色（真实段 depth {d}, active {hl} → helper）应 == 独立真值公式"
        );
        assert_eq!(
            c,
            getter_map[&(d, hl)],
            "渲染侧取色（真实段 depth {d}, active {hl}）应 == getter 输出（两路径同源封口）"
        );
    }
    // getter 每 (depth, active) 对照真值（getter 与渲染同式同源封口的基础锚）。
    for &(d, hl, c) in &getter_cols {
        assert_eq!(
            c,
            truth(d, hl),
            "getter 色（depth {d}, active {hl}）应 == 独立真值公式"
        );
    }
    // active 缩进高亮（光标行 0 = fn 开行 → 块体模型① fn depth0）== 真值(0, true)。
    let hl_color = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        ed.test_active_guide_highlight_color()
    });
    assert_eq!(
        hl_color,
        truth(0, true),
        "active 引导高亮（fn d0 active 色）应 == 独立真值公式"
    );
}

/// 活动括对由**块体模型**（2026-11-14 用户实测唯一真值）按光标**行号**派生（不再看字符列）：
/// ① open_line == r ② 严格含（open < r < close）最内 ③ close_line == r，各取 depth 最大。
/// 同 4 空格两层夹具 `fn a{\n    if x{\n        y();\n    }\n}`（P0 fn a(0,4,d0)、
/// P1 if x(1,3,d1)）：
/// - r0（fn 开行）→ ① P0；r1（if 开行）→ ① P1；r2（y(); 体）→ ② 最内 P1；
/// - r3（if `}`）→ ② 仅 fn 严格含 → P0（if 闭行归外层）；r4（fn `}` 顶层）→ ③ P0。
#[gpui::test]
fn active_pair_strict_contains(cx: &mut TestAppContext) {
    let text = "fn a{\n    if x{\n        y();\n    }\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    settle(cx);
    let read = |host: &Entity<Host>, cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            ed.read(cx).test_active_pair()
        })
    };
    // r1（if 开行）→ ① open_line==1 → P1（open1/close3/depth1）。
    set_caret_row_col(&host, cx, 1, 5);
    let a = read(&host, cx).expect("if 开行应有活动括对 P1");
    assert_eq!(
        (a.open_line, a.close_line, a.depth),
        (1, 3, 1),
        "if 开行 → ① P1"
    );
    // r2（体内）→ ② 严格含 → 最内 P1。
    set_caret_row_col(&host, cx, 2, 9);
    let a = read(&host, cx).expect("体内应有活动括对");
    assert_eq!(
        (a.open_line, a.close_line, a.depth),
        (1, 3, 1),
        "体内中间应取最内层 P1"
    );
    // r3（if `}`）→ ② 仅 fn 严格含 → P0（闭行归外层）。
    set_caret_row_col(&host, cx, 3, 4);
    let a = read(&host, cx).expect("if 闭行应归外层 P0");
    assert_eq!(
        (a.open_line, a.close_line, a.depth),
        (0, 4, 0),
        "if 闭行 → ② fn 严格含 → P0"
    );
    // r0（fn 开行）→ ① open_line==0 → P0。
    set_caret_row_col(&host, cx, 0, 4);
    let a = read(&host, cx).expect("fn 开行应有活动括对 P0");
    assert_eq!(
        (a.open_line, a.close_line, a.depth),
        (0, 4, 0),
        "fn 开行 → ① P0"
    );
    // r4（fn `}` 顶层）→ ③ close_line==4 → P0。
    set_caret_row_col(&host, cx, 4, 0);
    let a = read(&host, cx).expect("fn 顶层闭行应有活动括对 P0");
    assert_eq!(
        (a.open_line, a.close_line, a.depth),
        (0, 4, 0),
        "fn `}}` → ③ P0"
    );
}

/// 第 4 片：横向钩线—开行底边（active 括对三分支几何之一）。夹具与 brief 同构
/// `fn a(){\n if x{\n y();\n }\n}`，光标在 if 体内 → active 括对 = if 块（open1/close3）。
/// 演算（sf=Render 同 2.0，HEAD=TAIL=0）：
///   - if 引导列 = min(开列5, 闭列1, 对内最小非闭缩进 1+1) = 1；
///   - 开列 5 > 引导列 1 → 开行**底边**横段：左=1·cw、右=5·cw、y=开行下沿（2·pitch）− 半像素；
///   - 闭列 1 == 引导列 1 → 无闭行横段（VSCode endVisibleColumn > guide 才画）；
///   - active 括对唯一 → 只画 if 的钩线（fn a 非 active 不画）。
/// 同时断言竖线段跨度含开行（2026-11-14 第 4 片 A：`[open .. close)`）。
#[gpui::test]
fn bracket_hook_open_bottom_edge(cx: &mut TestAppContext) {
    let text = "fn a(){\n if x{\n y();\n }\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    settle(cx);
    // 光标在 if 体内（0idx 行 2 → inside if 块 [open1, close3)）。
    set_caret_row_col(&host, cx, 2, 1);
    let (cw, lh, gutter) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let cw = cw.max(1.);
    // 渲染用的吸附行距（sf=2.0 测试平台默认，与 render 的 pitch 同式）。
    let pitch = ((lh * 2.0 - 0.5).ceil()).max(1.0) / 2.0;
    let hairline = 1.0 / 2.0;
    let ap = cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_pair());
    assert_eq!(
        (ap.map(|p| p.open_line), ap.map(|p| p.close_line)),
        (Some(1), Some(3)),
        "active 括对应为 if 块"
    );
    let open_hook = cx
        .debug_bounds("ce-bracket-hook-open")
        .expect("开行底边横段应渲染");
    // 左缘 = 引导列（1·cw）+ gutter；右缘 ≈ 开列（5·cw）+ gutter。
    let x0 = gutter + 1.0 * cw;
    let x1 = gutter + 5.0 * cw;
    assert!(
        (open_hook.origin.x.as_f32() - x0).abs() < 1.0
            && (open_hook.origin.x.as_f32() + open_hook.size.width.as_f32() - x1).abs() < 1.0,
        "开行钩线左=引导列、右=开列：{open_hook:?}（期望 x0={x0} x1={x1}）"
    );
    assert!(
        (open_hook.size.width.as_f32() - 4.0 * cw).abs() < 1.5,
        "开行钩线宽 = 开列−引导列 = 4·cw：{open_hook:?}"
    );
    // y = 开行下沿 − 半像素（1 物理 px 厚，sf=2 → hairline=0.5 逻辑 px）。
    assert!(
        (open_hook.origin.y.as_f32() - (2.0 * pitch - hairline)).abs() < 1.5,
        "开行钩线应在开行下沿：{open_hook:?}（期望 {}）",
        2.0 * pitch - hairline
    );
    assert!(
        (open_hook.size.height.as_f32() - hairline).abs() < 0.5,
        "钩线厚 1 物理 px：{open_hook:?}"
    );
    // 闭行 `}` 独占、闭列 == 引导列 → 无闭行横段。
    assert!(
        cx.debug_bounds("ce-bracket-hook-close").is_none(),
        "闭列==引导列时不应画闭行横段"
    );
    // 竖线段跨度含开行、不含闭行（`}` 独占 → [open1, close3)）。
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(
            0, 0.0, lh, 1.0, 1200.0, 1000.0, config::layout::BracketGuidesMode::All,
        )
    });
    assert!(
        segs.iter().any(|(x, y, h)| {
            (x / cw).round() as i64 == 1
                && (y / pitch).round() as i64 == 1
                && (h / pitch).round() as i64 == 2
        }),
        "if 竖线段应含开行、不含闭行 [1,3)：{segs:?}"
    );
}

/// 第 4 片：横向钩线—闭行独占 `}` → **顶边**横段 + 竖线段不含闭行。闭列（4）深于引导列
/// （2）：体内行 ` y();` 缩进 1（欠缩进）把 `guide_pair_column` 下拉到 min(开9,闭4,2)=2，
/// 闭列 > 引导列 → 触发闭行横段（VSCode endVisibleColumn > guide）；闭行 `    }` 行首独占
/// `}` → hasTextBeforeClosingBracket=false → 顶边 + 竖线不含闭行。
#[gpui::test]
fn bracket_hook_close_alone_top_edge(cx: &mut TestAppContext) {
    let text = "fn a() {\n    if x {\n y();\n    }\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    settle(cx);
    set_caret_row_col(&host, cx, 2, 1); // 光标在 if 体内 → active = if（open1/close3）。
    let (cw, lh, gutter) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let cw = cw.max(1.);
    let pitch = ((lh * 2.0 - 0.5).ceil()).max(1.0) / 2.0;
    let hairline = 1.0 / 2.0;

    let close_hook = cx
        .debug_bounds("ce-bracket-hook-close")
        .expect("闭行独占 `}` 应画顶边横段");
    // 左缘 = 引导列（2）+ gutter；右缘 = 闭列（4）+ gutter。
    let x0 = gutter + 2.0 * cw;
    let x1 = gutter + 4.0 * cw;
    assert!(
        (close_hook.origin.x.as_f32() - x0).abs() < 1.0
            && (close_hook.origin.x.as_f32() + close_hook.size.width.as_f32() - x1).abs() < 1.0,
        "闭行顶边钩线左=引导列、右=闭列：{close_hook:?}"
    );
    assert!(
        (close_hook.origin.y.as_f32() - 3.0 * pitch).abs() < 1.0,
        "闭行独占 `}}` 钩线画在闭行**上沿**：{close_hook:?}"
    );
    assert!(
        (close_hook.size.height.as_f32() - hairline).abs() < 0.5,
        "钩线厚 1 物理 px：{close_hook:?}"
    );
    // 竖线段 `}` 独占 → 不含闭行：[open1, close3)。
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(
            0, 0.0, lh, 1.0, 1200.0, 1000.0, config::layout::BracketGuidesMode::All,
        )
    });
    assert!(
        segs.iter().any(|(x, y, h)| {
            (x / cw).round() as i64 == 2
                && (y / pitch).round() as i64 == 1
                && (h / pitch).round() as i64 == 2
        }),
        "闭行独占 `}}` → 竖线段不含闭行 [1,3)：{segs:?}"
    );
}

/// 第 4 片：横向钩线—闭行前有文本（`z(); }`）→ **底边**横段 + 竖线段**含闭行**
/// （VSCode `hasTextBeforeClosingBracket`=true → offset=1）。闭列（9）深于引导列（2）同 F2。
#[gpui::test]
fn bracket_hook_close_text_bottom_edge(cx: &mut TestAppContext) {
    let text = "fn a() {\n    if x {\n y();\n    z(); }\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    settle(cx);
    set_caret_row_col(&host, cx, 2, 1); // 光标在 if 体内 → active = if（open1/close3）。
    let (cw, lh, gutter) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let cw = cw.max(1.);
    let pitch = ((lh * 2.0 - 0.5).ceil()).max(1.0) / 2.0;
    let hairline = 1.0 / 2.0;

    let close_hook = cx
        .debug_bounds("ce-bracket-hook-close")
        .expect("闭行前有文本应画底边横段");
    let x0 = gutter + 2.0 * cw;
    let x1 = gutter + 9.0 * cw; // `    z(); }`：} 在字节列 9。
    assert!(
        (close_hook.origin.x.as_f32() - x0).abs() < 1.0
            && (close_hook.origin.x.as_f32() + close_hook.size.width.as_f32() - x1).abs() < 1.5,
        "闭行底边钩线左=引导列、右=闭列：{close_hook:?}"
    );
    assert!(
        (close_hook.origin.y.as_f32() - (4.0 * pitch - hairline)).abs() < 1.5,
        "闭行前有文本 → 钩线画在闭行**下沿**：{close_hook:?}"
    );
    // 竖线段闭行前有文本 → 含闭行：[open1, close3+1)。
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(
            0, 0.0, lh, 1.0, 1200.0, 1000.0, config::layout::BracketGuidesMode::All,
        )
    });
    assert!(
        segs.iter().any(|(x, y, h)| {
            (x / cw).round() as i64 == 2
                && (y / pitch).round() as i64 == 1
                && (h / pitch).round() as i64 == 3
        }),
        "闭行前有文本 → 竖线段含闭行 [1,4)：{segs:?}"
    );
}

/// 第 4 片（C3 让位）：`suppress` 表把「同 (行,列) 的缩进灰线」压掉；空表 → 行为与旧实现
/// 逐位一致（对照断言）。夹具 `fn a() {\n    x();\n}`：唯一缩进灰线在 (1, 0)。
#[gpui::test]
fn bracket_suppress_same_col_gray(cx: &mut TestAppContext) {
    let text = "fn a() {\n    x();\n}";
    let (host, cx) = open(cx, text);
    let (_, lh, _) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    let mut segs_of = |suppress: &[(usize, usize)]| {
        cx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            ed.read(cx).test_indent_guide_segments_flagged_suppressed(
                0, 0.0, lh, 1.0, 1200.0, 1000.0, None, suppress,
            )
        })
    };
    // 控制：无让位 → 缩进灰线段 (col0, 行1, 1 行, 非 active)。
    let plain = segs_of(&[]);
    assert_eq!(
        plain,
        vec![(0.0, 1.0 * pitch, 1.0 * pitch, false)],
        "空表 = 旧实现逐位一致：{plain:?}"
    );
    // 让位 (1,0) 恰好是 fn a 引导列 → 该灰线消失（bracket 竖线接管）。
    let suppressed = segs_of(&[(0, 0), (1, 0), (2, 0)]);
    assert!(
        suppressed.is_empty(),
        "括对引导列 (1,0) 灰线应被压掉：{suppressed:?}"
    );
}

/// 第 4 片（C3 让位）：`suppress_active_rows` 行保留灰线但**不点亮** active。夹具
/// `fn a() {\n    x();\n    y();\n}`（行 1/2 缩进 4 → 同一 col0 连续组）；active 组
/// [1,3) 整组点亮；让位表含行 2（`(2,4)` 非 col0 灰线位）→ 行 2 灰线保留但熄高亮。
#[gpui::test]
fn bracket_suppress_active_row_keeps_gray(cx: &mut TestAppContext) {
    let text = "fn a() {\n    x();\n    y();\n}";
    let (host, cx) = open(cx, text);
    let (_, lh, _) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    let active = Some((0usize, 1usize, 3usize, 1usize)); // col0、组 [1,3)、光标行 1。
    let mut segs_of = |suppress: &[(usize, usize)]| {
        cx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            ed.read(cx).test_indent_guide_segments_flagged_suppressed(
                0, 0.0, lh, 1.0, 1200.0, 1000.0, active, suppress,
            )
        })
    };
    let plain = segs_of(&[]);
    assert_eq!(
        plain,
        vec![(0.0, 1.0 * pitch, 2.0 * pitch, true)],
        "无让位：col0 组 [1,3) 整段点亮：{plain:?}"
    );
    let segs = segs_of(&[(2, 4)]);
    assert_eq!(
        segs,
        vec![
            (0.0, 1.0 * pitch, 1.0 * pitch, true),
            (0.0, 2.0 * pitch, 1.0 * pitch, false),
        ],
        "行 2 在让位表 → 保留灰线但不点亮：{segs:?}"
    );
}

/// 第 4 片（C3 让位表）：`bracket_guide_suppress` 覆盖括对行 × 引导列（每对 open..=close 视
/// 行、列 = `guide_pair_column`）。夹具两层：fn a→col0 覆 0..=4、if b→col4 覆 1..=3。
#[gpui::test]
fn bracket_suppress_table_covers_pair_rows(cx: &mut TestAppContext) {
    let text = "fn a() {\n    if b {\n        x();\n    }\n}";
    let (host, cx) = open(cx, text);
    let (_, lh, _) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let tbl = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_suppress(0, 1000.0, lh, config::layout::BracketGuidesMode::All)
    });
    for want in &[(0usize, 0usize), (1, 0), (1, 4), (2, 4), (3, 0), (4, 0)] {
        assert!(tbl.contains(want), "让位表应含 {want:?}: {tbl:?}");
    }
    assert!(!tbl.contains(&(5, 0)), "超出配对行范围不应有让位项: {tbl:?}");
}

/// 第 4 片（C3 让位 · 渲染路径封口）：overlay 的让位表压掉括对行的 **active 高亮**；对照组
/// 无让位时 active 组照常点亮。断言 `test_active_guide_group` 仍返回组（派生不受让位影响）。
#[gpui::test]
fn bracket_suppress_overlay_kills_active_hl(cx: &mut TestAppContext) {
    let text = "fn a() {\n    if x {\n        y();\n    }\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    settle(cx);
    set_caret_row(&host, cx, 2);
    let (_, lh, _) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let (group, suppress) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        (ed.test_active_guide_group(), ed.test_bracket_guide_suppress(
            0, 1000.0, lh, config::layout::BracketGuidesMode::All,
        ))
    });
    assert!(group.is_some(), "括对行上 active 组仍应派生（不受让位影响）");
    // 与 render `indentation_guide_overlay` 同式映射 active。
    let active = group.map(|(col, f, l_excl)| (col, f, l_excl, 2usize));
    let mut segs_of = |suppress: &[(usize, usize)]| {
        cx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            ed.read(cx).test_indent_guide_segments_flagged_suppressed(
                0, 0.0, lh, 1.0, 2000.0, 4000.0, active, suppress,
            )
        })
    };
    // 对照组：无让位 → 组内整段点亮。
    let with_supp = segs_of(&suppress);
    assert!(
        with_supp.iter().all(|(_, _, _, hl)| !*hl),
        "让位表压掉括对行 active 高亮：{with_supp:?}"
    );
    let no_supp = segs_of(&[]);
    assert!(
        no_supp.iter().any(|(_, _, _, hl)| *hl),
        "无让位对照：active 组应点亮：{no_supp:?}"
    );
}

/// **用户真机对照表**（2026-11-14，zip 已验证 = `active_guide_group` / 块体模型）：夹具 7
/// 行，逐行放光标断言渲染 hl 段行集（0 基半开 `[体首, 体末+1)`）与块体真值逐行一致：
///   点 1（fn 开行）/ 5（if `}`，fn 严格含）/ 6（baz）/ 7（fn `}`）→ 2-6 = [1,6)（fn 体）；
///   点 2（if 开行）/ 3（foo）/ 4（bar）→ 3-4 = [2,4)（if 体）。
/// 走 `indentation_guide_segments_flagged`（与渲染 `indentation_guide_overlay` 同式派生
/// active：group → (col, 体首, 体末+1, 光标可视行)），hl 段换算回 `(行首, 行末+1)` 对照。
#[gpui::test]
fn active_highlight_matches_user_table(cx: &mut TestAppContext) {
    let text = "fn main() {\n    if a == 1 {\n        foo();\n        bar();\n    }\n    baz();\n}";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    let (_, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let pitch = ((lh - 0.5).ceil()).max(1.0);
    // (光标 0idx 行, 期望 hl 段行集 [体首, 体末+1) 半开)：
    let cases: &[(usize, (usize, usize))] = &[
        (0, (1, 6)), // 点 fn 开行 → ① fn 体
        (1, (2, 4)), // 点 if 开行 → ① if 体
        (2, (2, 4)), // 点 foo → ② if 体
        (3, (2, 4)), // 点 bar → ② if 体
        (4, (1, 6)), // 点 if `}` → ② fn 严格含 → fn 体
        (5, (1, 6)), // 点 baz → ② fn 体
        (6, (1, 6)), // 点 fn `}` → ③ fn 体
    ];
    for &(row, want) in cases {
        set_caret_row(&host, cx, row);
        let (flag_hl, group) = cx.update(|_, cx| {
            let ed = host.read(cx).ed.clone();
            let ed = ed.read(cx);
            let group = ed.test_active_guide_group();
            let active = group.map(|(col, f, l_excl)| (col, f, l_excl, row));
            let segs = ed.test_indent_guide_segments_flagged(
                0, 0.0, lh, 1.0, 2000.0, 4000.0, active,
            );
            let flag_hl: Vec<(usize, usize)> = segs
                .into_iter()
                .filter(|(_, _, _, hl)| *hl)
                .map(|(_, y, h, _)| {
                    ((y / pitch).round() as usize, ((y + h) / pitch).round() as usize)
                })
                .collect();
            (flag_hl, group)
        });
        assert_eq!(
            flag_hl.as_slice(),
            &[want],
            "行 {row}: 渲染 hl 段应 == 用户表 {want:?}（0 基半开）；实际 {flag_hl:?}"
        );
        let want_group: Vec<(usize, usize)> = group
            .map(|(_, f, l_excl)| vec![(f, l_excl)])
            .unwrap_or_default();
        assert_eq!(
            flag_hl, want_group,
            "行 {row}: 渲染 hl 段应 == active_guide_group 区间（两路径同规则）"
        );
    }
}

/// **块体模型 · guidefix2 真值**（2026-11-14 用户 3 夹具实测唯一真值）：`target/guidefix2.rs`
/// 双态夹具（real file），配对 impl(0,21,d0)、fn-render(1,20,d1)。关键修复：点 `div()`
/// （0idx8）与链式续行 `.id(...)`（0idx9，旧"缩进级"口径误作 lv3 子块）→ 均关联 fn-render
/// 体 [2,20) col4 = (4,2,20)；fn-render 开行（0idx1）/ lv2 注释行（0idx2）同归 fn-render 体；
/// impl 开行（0idx0）→ ① open==0 → impl 体 [1,21) col0 = (0,1,21)。
/// 闭行（1 基 21 fn-render `}`、22 impl `}`，均 0idx 20/21）：fn-render `}` 无 open==20、
/// fn-render 不严格含自己闭行 → ② impl 严格含 20 → impl 体 (0,1,21)；impl `}` ③ close==21
/// → impl 体 (0,1,21)——两条都归外层 impl 体。
#[gpui::test]
fn indent_guides_active_open_requires_block_opener(cx: &mut TestAppContext) {
    let text = guidefix2_fixture_text();
    let (host, cx) = open(cx, text);
    let cases: &[(usize, Option<(usize, usize, usize)>)] = &[
        (8, Some((4, 2, 20))),  // div() → ② 最内 fn-render 体 [2,20)
        (9, Some((4, 2, 20))),  // .id() 链式续行 → ② 最内 fn-render 体（关键修复）
        (2, Some((4, 2, 20))),  // lv2 注释行 → ② fn-render 体
        (1, Some((4, 2, 20))),  // fn render { → ① open==1 → fn-render 体
        (0, Some((0, 1, 21))),  // impl { → ① open==0 → impl 体 [1,21)
        (20, Some((0, 1, 21))), // fn render `}`（1基21）→ ② impl 严格含 20 → impl 体
        (21, Some((0, 1, 21))), // impl `}`（1基22，顶级）→ ③ close==21 → impl 体
    ];
    for &(row, want) in cases {
        set_caret_row(&host, cx, row);
        let group = cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_guide_group());
        assert_eq!(
            group,
            want,
            "行 {row}（=1基{}）group 期望 {want:?}（块体模型：关联括对体）",
            row + 1
        );
    }
}

/// active 高亮**颜色**随「块体模型关联括对」（2026-11-14 用户实测唯一真值）：光标行关联括对
/// （`active_pair`：① 开行 ② 严格含 ③ 闭行，取 depth 最大）→ 该括对色 `palette[depth % 3]`；
/// 无关联括对 → 灰 `active_guide_color`。夹具 8 行（fn(open0,close6,d0) / if(open1,close4,
/// d1) + 尾行顶层代码）：
/// - r3（foo/bar 体）→ ② if → palette[1]；
/// - r5（baz，fn 内 if 外）→ ② fn → palette[0]；
/// - r0（fn 开行）→ ① fn → palette[0]；r6（fn `}` 顶层）→ ③ fn → palette[0]；
/// - r7（顶层 `let top = 1;`，无关联块）→ 灰 active_guide_color。
#[gpui::test]
fn active_highlight_color_follows_containing_pair(cx: &mut TestAppContext) {
    let text = "fn main() {\n    if a == 1 {\n        foo();\n        bar();\n    }\n    baz();\n}\nlet top = 1;\n";
    let (host, cx) = open(cx, text);
    focus(&host, cx);
    let (want_if, want_fn, gray) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        (
            ed.test_bracket_guide_color(1, true), // palette[1%3] 全不透明 = if 括对 active 色
            ed.test_bracket_guide_color(0, true), // palette[0%3] 全不透明 = fn 括对 active 色
            ed.test_guide_colors().1,             // active_guide_color（VSCode 灰）
        )
    });
    // 光标在 if 体内 → ② 最内 if（depth1）→ if 色。
    set_caret_row(&host, cx, 3);
    let (ap, color) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        (ed.test_active_pair().map(|p| p.depth), ed.test_active_guide_highlight_color())
    });
    assert_eq!(ap, Some(1), "光标在 if 体 → 关联括对 = if（depth1）");
    assert_eq!(color, want_if, "if 体内 hl 色应随 if 括对色 palette[1]");
    // 光标在 baz（0idx5）：if 外但 fn 内 → fn 括对色。
    set_caret_row(&host, cx, 5);
    let (ap, color) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        (ed.test_active_pair().map(|p| p.depth), ed.test_active_guide_highlight_color())
    });
    assert_eq!(ap, Some(0), "光标在 baz → 关联括对 = fn（depth0，if 不含 baz）");
    assert_eq!(color, want_fn, "baz 行 hl 色随 fn 括对色 palette[0]");
    // fn 开行（r0）/ fn 闭行（r6，顶层 `}`）→ ①/③ fn → fn 色（不再灰）。
    set_caret_row(&host, cx, 0);
    let (ap, color) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        (ed.test_active_pair().map(|p| p.depth), ed.test_active_guide_highlight_color())
    });
    assert_eq!(ap, Some(0), "fn 开行 → ① fn");
    assert_eq!(color, want_fn, "fn 开行 hl 色随 fn 括对色 palette[0]");
    set_caret_row(&host, cx, 6);
    let (ap, color) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        (ed.test_active_pair().map(|p| p.depth), ed.test_active_guide_highlight_color())
    });
    assert_eq!(ap, Some(0), "fn `}}` 顶层 → ③ fn");
    assert_eq!(color, want_fn, "fn `}}` hl 色随 fn 括对色 palette[0]");
    // 无关联块（顶层代码行）→ 灰 active_guide_color。
    set_caret_row(&host, cx, 7);
    let (ap, color) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        (ed.test_active_pair(), ed.test_active_guide_highlight_color())
    });
    assert_eq!(ap, None, "顶层 let top 行 → 无关联括对");
    assert_eq!(color, gray, "无关联块 → hl 色应为灰 active_guide_color");
}

/// 缺省 Off（2026-11-14 用户真机口径）：括对彩色竖线层 `ce-bracket-guides` **不挂载**；
/// 钩线层 `ce-bracket-hooks` 恒挂（bracketPairsHorizontal='active'，仅严格含光标的
/// active 括对）——光标进 if 体 → if 开行底边钩线 1 根（闭列==引导列 → 无闭行钩线）。
/// All 模式（对比用，`BRACKET_GUIDES_MODE` 编译期常量翻转）：竖线层挂载与否仅由
/// `BRACKET_GUIDES_MODE != Off` 守卫——缺省 Off 的 None 已直接证明守卫在 Off 分支失效，
/// 故 All 的竖线层渲染存在性 = 守卫取真 + 段数据非空（段数据在此按 All 显式断言非空、
/// fn col0 + if col4 两根）；不必也无法在常量默认 Off 的测试二进制里直接 render 断言。
#[gpui::test]
fn bracket_rails_off_by_default_and_hooks_active_pair(cx: &mut TestAppContext) {
    let text = "fn main() {\n    if a == 1 {\n        foo();\n        bar();\n    }\n    baz();\n}";
    let (host, cx) = open(cx, text);
    assert!(
        cx.debug_bounds("ce-bracket-guides").is_none(),
        "缺省 Off：ce-bracket-guides 竖线层不应渲染"
    );
    assert!(
        cx.debug_bounds("ce-bracket-hooks").is_some(),
        "钩线层恒挂（Off 也保留，容器在渲染树中）"
    );
    focus(&host, cx);
    set_caret_row_col(&host, cx, 3, 2); // 光标在 if 体 foo() → active = if（open1/close4）。
    settle(cx);
    assert!(
        cx.debug_bounds("ce-bracket-guides").is_none(),
        "光标进 if 体，竖线层仍不渲染（Off 只留钩线）"
    );
    assert!(
        cx.debug_bounds("ce-bracket-hook-open").is_some(),
        "if 开行钩线应渲染（bracketPairsHorizontal='active' 保留）"
    );
    assert!(
        cx.debug_bounds("ce-bracket-hook-close").is_none(),
        "if 闭列==引导列（col4）→ 无闭行钩线（对照 bracket_hook_open_bottom_edge）"
    );
    // All 模式（对比）：段数据全括对产线——fn col0/depth0 + if col4/depth1 两根（见上注释）。
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let cw = cw.max(1.);
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments_flagged(
            0, 0.0, lh, 1.0, 1200.0, 1000.0, config::layout::BracketGuidesMode::All,
        )
    });
    assert_eq!(segs.len(), 2, "All 模式应全部括对产竖线：{segs:?}");
    let norm: Vec<(i64, usize)> = segs.iter().map(|(x, _, _, d, _)| ((x / cw).round() as i64, *d)).collect();
    assert!(norm.contains(&(0, 0)), "All：fn 括对段应在 col0/depth0：{norm:?}");
    assert!(norm.contains(&(4, 1)), "All：if 括对段应在 col4/depth1：{norm:?}");
}

