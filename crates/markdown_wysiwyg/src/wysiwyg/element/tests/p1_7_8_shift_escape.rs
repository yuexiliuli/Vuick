// P1-7 / P1-8 红测（2026-11-16 批次，与实现同 commit）：
// - P1-7：异步完成回调（代码高亮 / 行内公式）在**增量编辑行平移后**不得按当刻
//   `src_line → block` 反查作废——请求时刻的 `src_line` 经队列传到完成回调时任然是旧值，
//   平移后反查会作废同一位置的无关块、漏作废真正的内容块（编辑错位）。修复 = 请求/
//   claim 时捕获**块内容指纹**（`frame::block_content_id`，随行平移稳定），完成回调按
//   指纹作废。
// - P1-8：保存路径对 alt 含 `]`、src 含 `(`/`)` 产出 `\]`/`\(`/`\)` 转义，但解析器
//   （`inline_image_spans` / `parse_image_line`）不认转义 → 往返不自洽（保存后丢失图片
//   识别）。修复 = 解析器按**未转义**定界符扫描（`\]`/`\(`/`\)` 是字面字符），src 反解
//   字面值；图片卡片/加载消费字面值。

/// P1-7（代码块高亮，content_sync.rs:133 / on_code_parse_ready）：
/// 构造「请求解析 → 在其上方增量插行（行平移、块内容未变）→ 模拟后台完成回调」。
/// 修复前 `on_code_parse_ready` 把旧 `src_line` 映射到平移后同位置的无关块 → 真正内容块
/// 缓存残留（永远停在纯文本）、无关块被误作废。修复后按「请求时刻的块内容指纹」作废
/// → 内容块（新行号处）被精确作废、无关块保留。
#[gpui::test]
fn code_parse_ready_after_line_shift_invalidates_correct_block(cx: &mut TestAppContext) {
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let mut text = String::new();
    text.push_str("para0 some more words to wrap\n");
    text.push_str("para1 some more words to wrap\n");
    text.push_str("```rust\n");
    let mut code = String::new();
    for i in 0..12i32 {
        code.push_str(&format!("fn code_{i}(a: u32) -> u32 {{\n    a + {i}\n}}\n"));
    }
    text.push_str(&code);
    text.push_str("```\n");
    text.push_str("para2 some more words\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let fence_line = 2usize; // ```rust 旧源行
        assert_eq!(
            ed.read(cx).doc().line_str(fence_line),
            "```rust",
            "前置：第 {fence_line} 行应为代码围栏"
        );
        // 请求后台解析（shape 期 `request_code_parse` 的登记：键 + src_line + 内容指纹钉）。
        ed.update(cx, |e, _ecx| e.request_code_parse("rust", &code, fence_line));
        // 建帧（带盖住代码块）→ 代码块入行整形缓存（纯文本占位行）。
        let _f1 = build_frame(ed.read(cx), window, px(900.), (0., 1600.));
        // 增量编辑：文档最前插一行 → 代码块整体下移一行（行平移），其内容未变。
        ed.update(cx, |e, ecx| e.replace_range(0..0, "INSERTED line\n", ecx));
        let fence_new_block = ed.read(cx).doc().block_index_for_line(fence_line + 1);
        let neighbour = ed.read(cx).doc().block_index_for_line(fence_line);
        assert_ne!(
            fence_new_block, neighbour,
            "前置：平移后旧行与新块应落在不同块"
        );
        assert!(
            ed.read(cx).shape_rows_has(fence_new_block),
            "前置：行平移但内容未变的代码块缓存应保留（内容寻址）"
        );
        // 模拟后台高亮就绪回调（携带**请求时刻**的旧 src_line）。
        ed.update(cx, |e, ecx| e.on_code_parse_ready(&[fence_line], ecx));
        assert!(
            !ed.read(cx).shape_rows_has(fence_new_block),
            "P1-7：代码块（平移后）应被精确作废，不得残留纯文本缓存"
        );
        assert!(
            ed.read(cx).shape_rows_has(neighbour),
            "P1-7：旧 src_line 现指向的无关块不得被误作废"
        );
    });
}

/// P1-7（行内公式，content_sync.rs:301 / on_math_ready）：
/// 同一根因在行内公式路径：claim 时登记 `(src_line, 内容指纹)`，完成回调按指纹作废。
/// 构造「行内公式 claim → 上方插行（行平移、段落内容未变）→ on_math_ready」。
#[gpui::test]
fn inline_math_ready_after_line_shift_invalidates_correct_block(cx: &mut TestAppContext) {
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "para0 some words here\npara1 some words here\ninline $a+b$ tail\npara2 words\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    // 光标置文末（离开公式 → 行内公式被 splice 带出到 frame.rows）。
    let (frame, inline_line, inline_key) = vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let n = ed.read(cx).content().len();
        ed.update(cx, |e, _| e.set_caret(n));
        let f = build_frame(ed.read(cx), window, px(600.), (0., 9999.));
        let row = f
            .rows
            .iter()
            .find(|r| !r.extra.inline_maths.is_empty())
            .expect("夹具应含行内公式行");
        let sl = row.src_line;
        let key = row.extra.inline_maths[0].key.to_string();
        (f, sl, key)
    });
    assert_eq!(inline_line, 2, "前置：行内公式行应在旧源行 2");
    // sync_math：登记「行内公式→(行, 块内容指纹)」并占 Loading 槽。
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, ecx| e.sync_math(ecx, &frame));
    });
    // 增量编辑：文档最前插一行 → 公式所在行平移（段落内容未变）。
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, ecx| e.replace_range(0..0, "INSERTED line\n", ecx));
        let para_new_block = ed.read(cx).doc().block_index_for_line(inline_line + 1);
        let neighbour = ed.read(cx).doc().block_index_for_line(inline_line);
        assert_ne!(
            para_new_block, neighbour,
            "前置：平移后旧行与新块应落在不同块"
        );
        assert!(
            ed.read(cx).shape_rows_has(para_new_block),
            "前置：行平移但内容未变的行内公式行缓存应保留"
        );
        // 模拟行内公式就绪回调（携带 **claim 时刻**的旧 src_line / 指纹）。
        ed.update(cx, |e, ecx| e.on_math_ready(&inline_key, ecx));
        assert!(
            !ed.read(cx).shape_rows_has(para_new_block),
            "P1-7：行内公式所在块（平移后）应被精确作废"
        );
        assert!(
            ed.read(cx).shape_rows_has(neighbour),
            "P1-7：旧行无关块不得被误作废"
        );
    });
}

/// P1-8：图片保存转义后往返自洽——alt 含 `]`、src 含 `(`/`)` 的图片，
/// 保存（`\]`/`\(`/`\)` 转义）→ 重解析 → 图片识别仍在（`parse_image_line` /
/// `inline_image_spans` 按**未转义**定界符扫描），且重开卡片预填**字面值**（`a]b` /
/// `a(b)c.png`，再保存不二次转义）。覆盖块图（`apply_image_line_edit`）与行内图
/// （`apply_inline_image_edit`）。
#[gpui::test]
fn image_edit_escape_roundtrip_preserves_recognition(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::ImageEditTarget;
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    // ── 块图 ──
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("![旧](old.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let out = vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |e, ecx| {
            e.apply_image_line_edit(0, "a]b".into(), "a(b)c.png".into(), ecx)
        });
        let text = ed.read(cx).content();
        // 重开编辑卡（经 `image_line_alt_src` 反解）→ 预填字面值。
        ed.update(cx, |e, c| {
            e.open_image_edit(ImageEditTarget::Block { src_line: 0 }, window, c)
        });
        let link = ed.read(cx).image_edit_link_input().expect("链接输入框应打开");
        let text_inp = ed.read(cx).image_edit_text_input().expect("文本输入框应打开");
        let link_v = link.read(cx).value().to_string();
        let text_v = text_inp.read(cx).value().to_string();
        (text, link_v, text_v)
    });
    assert_eq!(
        out.0, "![a\\]b](a\\(b\\)c.png)\n",
        "保存应转义 `]`/`(`/`)` 为 `\\]`/`\\(`/`\\)`"
    );
    assert_eq!(out.1, "a(b)c.png", "P1-8：重开卡片 src 预填应反解字面值");
    assert_eq!(out.2, "a]b", "P1-8：重开卡片 alt 预填应反解字面值");
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let raw = ed.read(cx).doc().line_str(0).trim().to_string();
        // 重解析后仍是独立图片行（块识别不丢失）。
        let rows = ed.read(cx).doc().rows_in_block_window(0, 0);
        assert_eq!(
            rows[0].kind,
            crate::markdown::ir::IrKind::Image,
            "P1-8：重解析后第 0 行仍应为独立图片行（块识别不丢失）"
        );
        // 解析器按未转义定界符识别；src 反解字面值（加载/再保存不二次转义）。
        let (_, src) =
            crate::markdown::parse::parse_image_line(&raw).expect("仍应识别图片");
        assert_eq!(
            src, "a(b)c.png",
            "P1-8：`parse_image_line` src 应反解字面值（`\\(`/`\\)` → `(`/`)`）"
        );
    });
    // ── 行内图 ──
    let (host2, vcx2) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("text ![旧](old.png) tail\n")
        });
        WyHost { ed }
    });
    let vcx2: &mut VisualTestContext = vcx2;
    let out2 = vcx2.update(|window, cx: &mut App| {
        let ed = host2.read(cx).ed.clone();
        let refd = crate::markdown::wysiwyg::InlineImageRef {
            src_line: 0,
            display_off: 0,
            start: 5, // `!` 在 "text " 之后
        };
        ed.update(cx, |e, ecx| {
            e.apply_inline_image_edit(refd, "a]b".into(), "a(b)c.png".into(), ecx)
        });
        let text = ed.read(cx).content();
        // 重开编辑卡（经 `image_target_alt_src` 反解）→ 预填字面值。
        ed.update(cx, |e, c| {
            e.open_image_edit(ImageEditTarget::Inline(refd), window, c)
        });
        let link = ed.read(cx).image_edit_link_input().expect("链接输入框应打开");
        let text_inp = ed.read(cx).image_edit_text_input().expect("文本输入框应打开");
        let link_v = link.read(cx).value().to_string();
        let text_v = text_inp.read(cx).value().to_string();
        (text, link_v, text_v)
    });
    assert_eq!(
        out2.0, "text ![a\\]b](a\\(b\\)c.png) tail\n",
        "保存应只改行内图段并转义 `]`/`(`/`)`"
    );
    assert_eq!(out2.1, "a(b)c.png", "P1-8：行内重开卡片 src 预填应反解字面值");
    assert_eq!(out2.2, "a]b", "P1-8：行内重开卡片 alt 预填应反解字面值");
    vcx2.update(|_window, cx: &mut App| {
        let ed = host2.read(cx).ed.clone();
        let raw = ed.read(cx).doc().line_str(0).to_string();
        let tail = raw[5..].to_string(); // 自 `!` 起（行内图片段源区间）
        let spans = crate::markdown::parse::inline_style::inline_image_spans(&tail);
        assert_eq!(
            spans.len(),
            1,
            "P1-8：行内图片段重解析后仍应被识别（`inline_image_spans` 认 `\\]`/`\\(`/`\\)`）"
        );
    });
}
