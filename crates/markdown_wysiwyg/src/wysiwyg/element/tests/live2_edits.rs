// live2「所见即所得实时编辑」三问题回归（S1/S2/S3，同一线并包）。
// 全部经真实事件/帧路径（`WyHost` + `dispatch_keystroke` + `build_frame`），headless。
// ⚠️ 共享命名空间：`WyHost`/`build_frame`/`caret_geometry`/`caret_bar_height`/
// `WyEditor`/`point`/`px`/`size`/`VisualTestContext`/`TestAppContext` 已由邻文件
// import 就位，此处**不再重复 use**（重复即 E0252）；`Keystroke`/`IrKind` 仅函数内
// 局部 use（同 edit_cache/math 姿势）。

/// S1 复现：Shift+方向键应建立选区（固定锚 + 活动端扩展），与编辑区同语义。
/// 复现文档 `"你好世界 hello\n"`（你@0好@3世@6界@9 空格@12 hello@13 换行@18）。
/// 修复前：`wy-editor` 键上下文**无** `shift-*` 绑定 → Shift+Right 无任何动作，
/// caret 停在 0、选区恒 None → 断言必败（RED）。
#[gpui::test]
fn shift_arrow_selects_text_like_editor(cx: &mut TestAppContext) {
    use gpui::Keystroke;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("你好世界 hello\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    // 聚焦编辑器 → 键上下文 `wy-editor` 生效（对齐 edit_cache 的 ctrl-a 姿势）。
    vcx.update(|window, cx| {
        let fh = ed.read(cx).focus_handle();
        window.focus(&fh, cx);
    });
    let wh = vcx.windows().first().copied().expect("窗口句柄");
    ed.update(vcx, |e, _| {
        e.set_caret(0);
        assert_eq!(e.selection(), None, "初始无选区");
    });

    // Shift+Right ×3 → caret 前移『你好世』（每击一字符 3 字节），锚钉在起点 0。
    for _ in 0..3 {
        vcx.dispatch_keystroke(wh, Keystroke::parse("shift-right").unwrap());
    }
    vcx.update(|_, cx| {
        let e = ed.read(cx);
        assert_eq!(
            e.selection(),
            Some(0..9),
            "Shift+Right×3 应选中『你好世』[0,9)，实际 {:?}",
            e.selection()
        );
        assert_eq!(e.sel_anchor, Some(0), "Shift 选区锚应固定钉在起点 0");
    });

    // Shift+Left 收缩：3→2→1→0 字符，锚保持起点。
    for _ in 0..2 {
        vcx.dispatch_keystroke(wh, Keystroke::parse("shift-left").unwrap());
    }
    vcx.update(|_, cx| {
        let e = ed.read(cx);
        assert_eq!(e.selection(), Some(0..3), "两次 Shift+Left 应收缩到『你』[0,3)");
        assert_eq!(e.sel_anchor, Some(0), "收缩后锚仍钉起点 0");
    });

    // 收缩到空后反向再扩展：锚仍固定 0，选区从另一侧生长（与编辑区一致）。
    vcx.dispatch_keystroke(wh, Keystroke::parse("shift-left").unwrap());
    vcx.update(|_, cx| assert_eq!(ed.read(cx).selection(), None, "缩到空 = 无选区"));
    vcx.dispatch_keystroke(wh, Keystroke::parse("shift-right").unwrap());
    vcx.update(|_, cx| {
        assert_eq!(
            ed.read(cx).selection(),
            Some(0..3),
            "空后 Shift+Right 应在固定锚 0 上重新生长"
        );
    });

    // 纯方向键 + 有选区（编辑区同语义）：`arrow(false, +1)` 的 `!select &&
    // !is_collapsed` 分支 → 先收起选区到**选区终点**，不继续前移（VS Code 同款）。
    vcx.dispatch_keystroke(wh, Keystroke::parse("right").unwrap());
    vcx.update(|_, cx| {
        let e = ed.read(cx);
        assert_eq!(e.sel_anchor, None, "纯 Right 应收起选区锚");
        assert_eq!(e.selection(), None, "纯 Right 后无选区");
        assert_eq!(e.caret, 3, "纯 Right 应把选区收起在终点 3（不继续前移）");
    });
    // 收起后纯 Right 才正常前移一字符。
    vcx.dispatch_keystroke(wh, Keystroke::parse("right").unwrap());
    vcx.update(|_, cx| assert_eq!(ed.read(cx).caret, 6, "无选区后 Right 前移一字符 → 6"));
}

/// S2 复现：标题行里在 `#`（~`######`）标记引导区连续敲空格 → 源文本只保留一个
/// 分隔空格（后续空格键入被吞掉），类型/所见/拷贝三者一致；一旦标记后出现非空格
/// 文本，空格输入照常。走真实键入路径 `replace_text_utf16`（平台直输）。
/// 修复前：每次空格都落盘 → `#` + 空格×N = `#    …`（断言必败，RED）。
#[gpui::test]
fn typing_extra_space_after_heading_marker_is_swallowed(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(""));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());

    // 键入 `#`（caret→1）。
    ed.update(vcx, |e, cx| e.replace_text_utf16(None, "#", cx));
    // 第 1 个分隔空格照常插入（标记后尚无空格 → 产生分隔空格）。
    ed.update(vcx, |e, cx| e.replace_text_utf16(None, " ", cx));
    ed.update(vcx, |e, _| {
        assert_eq!(e.text().as_ref(), "# ", "首个分隔空格应保留");
        assert_eq!(e.caret(), 2, "首个空格后光标停在标记后 (col=2)");
    });
    // 连续敲空格 ×4 → 全部吞掉，源文本仍 `# `（修复前 = `#      `，断言必败）。
    for _ in 0..4 {
        ed.update(vcx, |e, cx| e.replace_text_utf16(None, " ", cx));
    }
    ed.update(vcx, |e, _| {
        assert_eq!(e.text().as_ref(), "# ", "标记后连续空格应只保留一个分隔空格");
        assert_eq!(e.caret(), 2, "吞掉后光标仍停在标记后第 1 空格之后 (col=2)");
    });
    // 标记后出现非空格文本 → 空格输入照常。
    ed.update(vcx, |e, cx| e.replace_text_utf16(None, "你好", cx));
    ed.update(vcx, |e, cx| e.replace_text_utf16(None, " ", cx));
    ed.update(vcx, |e, _| {
        assert_eq!(e.text().as_ref(), "# 你好 ", "标记后出现非空格文本，空格照常插入");
    });

    // `######`（六级标题）同规则；`# ` 后再 `#`（标记后出现非空格）→ 空格照常。
    for (marker, first_caret) in [("##", 3usize), ("######", 7usize)] {
        ed.update(vcx, |e, cx| {
            while e.len() > 0 {
                let r = 0..e.len();
                e.replace_range(r, "", cx);
            }
            e.set_caret(0);
            e.replace_text_utf16(None, marker, cx);
            e.replace_text_utf16(None, " ", cx);
            // 再敲 3 个空格 → 全吞。
            for _ in 0..3 {
                e.replace_text_utf16(None, " ", cx);
            }
        });
        let expected = format!("{marker} ");
        ed.update(vcx, |e, _| {
            assert_eq!(
                e.text().as_ref(),
                expected.as_str(),
                "{marker} 标记后连续空格应只留一个分隔空格"
            );
            assert_eq!(e.caret(), first_caret, "{marker} 吞掉后光标停在第 1 空格之后");
        });
    }
}

/// S2×E3 交互：标题标记引导区（`# ` 之后、首个内容字符之前）内按 Tab 视同按空格——
/// 一旦已有 ≥1 个分隔空格（引导区成立），Tab 的 4 空格**整体吞掉**（源不变、光标不动），
/// 与空格键同一语义；若 Tab 的 4 空格绕过 S2 落进引导区（`#     ` 脏前缀，修复前），
/// 此断言必败（RED）。引导区尚未成立（`#` 后直接 Tab）则照常插入 4 空格（=扩大分隔）。
#[gpui::test]
fn tab_in_heading_guide_zone_is_swallowed_like_space(cx: &mut TestAppContext) {
    use gpui::Keystroke;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(""));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let wh = focus_wy(&ed, vcx);

    // 键入 `# `（标记 + 1 分隔空格）→ 引导区成立。
    ed.update(vcx, |e, cx| e.replace_text_utf16(None, "#", cx));
    ed.update(vcx, |e, cx| e.replace_text_utf16(None, " ", cx));
    // 引导区内按 Tab ×2：4 空格整体吞掉（源仍 `# `）。
    for _ in 0..2 {
        vcx.dispatch_keystroke(wh, Keystroke::parse("tab").unwrap());
    }
    ed.update(vcx, |e, _| {
        assert_eq!(e.text().as_ref(), "# ", "引导区 Tab 应整体吞掉（与空格同语义）");
        assert_eq!(e.caret(), 2, "吞掉后光标仍停在分隔空格之后");
    });

    // 引导区外（首个内容字符出现后）Tab 照常插入 4 空格可见缩进。
    ed.update(vcx, |e, cx| e.replace_text_utf16(None, "x", cx));
    vcx.dispatch_keystroke(wh, Keystroke::parse("tab").unwrap());
    ed.update(vcx, |e, _| {
        assert_eq!(e.text().as_ref(), "# x    ", "内容后 Tab 照常插 4 空格缩进");
    });
}

/// S3 复现（含对齐）：标题行（大字）光标竖条高度与**垂直位置**都应跟该行**文字字形**
/// ——高度 = 该行 shaped line 的真实 `ascent+descent`（非整行带 `r.lh`），且竖条顶 =
/// 字形顶（`带顶 + (r.lh − 字形高)/2`，gpui paint 定位基线的同源公式），竖条精确覆盖
/// 字形框、不再悬在行带下方；
/// 正文/B lank → 现状不变（高 == `f.line_h`、顶 == 行带顶）。
/// 修复前（RED）：高度 = `r.lh`、顶 = 带顶 + `(r.lh − f.line_h)/2` → 竖条底越出行带
/// 下沿、不贴字形 → 本断言必败。
#[gpui::test]
fn caret_bar_height_follows_row_line_height(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("# 标题\n正文\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let frame = blank_harness(vcx, &ed);
    let heading = frame
        .rows
        .iter()
        .find(|r| matches!(r.kind, IrKind::Heading(_)))
        .unwrap_or_else(|| panic!("帧应含标题行"));
    let body = frame
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Paragraph)
        .unwrap_or_else(|| panic!("帧应含正文行"));
    // 标题字号更大 → 该行 `wrapped` 带真实字形度量（ascent+descent，gpui paint 定位
    // 基线的同源数据，`vendor/zed/crates/gpui/src/text_system/line.rs`）。
    let w0 = heading
        .wrapped
        .first()
        .expect("有字标题行应有 shaped line");
    let glyph =
        f32::from(w0.unwrapped_layout.ascent) + f32::from(w0.unwrapped_layout.descent);
    let h_top = f32::from(heading.top);
    let lh = f32::from(heading.lh);
    let line_h = f32::from(frame.line_h);
    assert!(
        lh > line_h,
        "前提：标题行高 {} > 正文行高 {}",
        lh,
        line_h
    );
    // 光标落标题行（line 0 任意位置）→ 高 = 字形高、顶 = 字形顶（带内居中于字形框）。
    ed.update(vcx, |e, _| e.set_caret(1));
    let hh = vcx.update(|_, cx| caret_bar_height(ed.read(cx), &frame));
    let (onh, _, carety) = vcx.update(|_, cx| caret_geometry(ed.read(cx), &frame));
    assert!(onh, "标题行应可放光标");
    assert_eq!(
        f32::from(hh),
        glyph,
        "标题行光标高度应 = 该行字形高 ascent+descent（{glyph:.2}），实际 {}",
        f32::from(hh)
    );
    let want_top = h_top + ((lh - glyph) / 2.);
    assert!(
        (f32::from(carety) - want_top).abs() < 0.5,
        "标题行光标顶应 = 字形顶 {want_top:.2}，实际 {:.2}",
        f32::from(carety)
    );
    // 竖条精确覆盖字形框 + 不越出行带（旧「偏下/悬在行带下方」回归锁）。
    assert!(
        f32::from(carety) + f32::from(hh) <= h_top + lh + 0.5,
        "标题行竖条底不越出行带（{:.2} ≤ {}）",
        f32::from(carety) + f32::from(hh),
        h_top + lh
    );
    assert!(
        (f32::from(carety) - h_top).abs() >= 0.5 * (lh - glyph) / 2.,
        "标题行光标顶应离开带顶（若是旧锚带顶则必败）"
    );
    // 光标落正文行（line 1）→ 高 = 正文行高、顶 = 行带顶（现状不变）。
    let body_start = ed.read_with(vcx, |e, _| e.line_start(1));
    ed.update(vcx, |e, _| e.set_caret(body_start));
    let bh = vcx.update(|_, cx| caret_bar_height(ed.read(cx), &frame));
    let (onb, _, caretyb) = vcx.update(|_, cx| caret_geometry(ed.read(cx), &frame));
    assert!(onb, "正文行应可放光标");
    assert_eq!(
        f32::from(bh),
        f32::from(body.lh),
        "正文行光标高度应 = 该行行高（{}），实际 {}",
        f32::from(body.lh),
        f32::from(bh)
    );
    assert_eq!(
        f32::from(bh),
        line_h,
        "正文行光标高度 = 正文 line_h（现状不变）"
    );
    assert!(
        (f32::from(caretyb) - f32::from(body.top)).abs() < 0.5,
        "正文行光标顶应 = 行带顶（现状不变），实际 {}",
        f32::from(caretyb)
    );
}
