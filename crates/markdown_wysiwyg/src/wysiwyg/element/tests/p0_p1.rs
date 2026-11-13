// P0（markdown 解析崩溃）+ P1（WY 行首空白输入可见）回归（2026-11-13，同一线并包）。
//
// P0 机制：引述/列表/标题行的 `byte_map` 是「渲染字节 = 源字节 + off」的非恒等映射，
// 含多字节（中文）+ 行内 `$…$` 时，`splice_math_text` 此前把**渲染**坐标 span 当
// **源**坐标传给 `splice_inline_math` → 显示端点错 off 字节、落进多字节字符中间 →
// `display[cursor..d0]` 切片崩（`inside '能'` / `inside '质'`）。修复：调用方传源坐标
// + splice 自身按字符边界折片（双保险）。下列「引述/列表/标题 + 中文 + 公式」建帧
// 即复现，修复前 build_frame（shape 期 splice）必 panic。
//
// P1 机制：段落行首空白被 `markdown::parse::rows::trim_paragraph` 剥掉 → 源有空格、
// 渲染不回显，用户看着「行首 Tab/空格没用」。修复：`trim_paragraph` 保留行首 ASCII
// 空白（所见即所得缩进；本引擎无「4 空格缩进代码块」，块语义由行首符号决定，不翻车）。

/// P0：引述行含中文 + 行内公式 → 建帧不得 panic，公式占位（spacer）仍产出、行仍为
/// 引述。修复前 `build_frame` 在 shape 期 splice 处直接崩溃。
#[gpui::test]
fn quote_with_chinese_inline_math_shapes_without_byte_panic(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx
            .new(|cx| WyEditor::new(window, cx).with_text("> 质能方程 $E=mc^2$\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let frame = vcx.update(|window, cx| build_frame(ed.read(cx), window, px(600.), (0., 999_999.)));
    let row = frame
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Quote)
        .expect("引述行应在场");
    let concat: String = row.wrapped.iter().map(|wl| wl.text.as_ref()).collect();
    assert!(
        concat.contains("E=mc^2") || concat.trim_matches(' ').contains("质能方程"),
        "引述渲染文本应含原内容（spacer/Unicode 兜底任一形态）: {concat:?}"
    );
}

/// P0 同一机制：列表项 + 标题也走「剥前导标记 → off>0 非恒等映射」路径，必须同样不崩。
#[gpui::test]
fn bullet_and_heading_with_chinese_inline_math_shape_without_byte_panic(
    cx: &mut TestAppContext,
) {
    use crate::markdown::ir::IrKind;
    let text = "- 质能方程 $E=mc^2$\n## 质能方程 $E=mc^2$\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let frame = vcx.update(|window, cx| build_frame(ed.read(cx), window, px(600.), (0., 999_999.)));
    assert!(frame.rows.iter().any(|r| r.kind == IrKind::Bullet), "列表行应在场");
    assert!(frame.rows.iter().any(|r| matches!(r.kind, IrKind::Heading(_))), "标题行应在场");
}

/// P0 守卫：含中文 + 行内公式的**段落**行，行首按 Tab 插入前导空白后建帧也不崩
/// （用户原始复现：行首能/一 分界字符 + 前导 4 空格）。
#[gpui::test]
fn paragraph_leading_tab_then_chinese_math_line_still_shapes(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    use gpui::Keystroke;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx
            .new(|cx| WyEditor::new(window, cx).with_text("质能方程 $E=mc^2$\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let wh = focus_wy(&ed, vcx);
    // 行首按 Tab（前导 4 空格）→ 再建帧：修复前该路径（非恒等映射 + 中文 + 公式）崩。
    ed.update(vcx, |e, _| e.set_caret(0));
    vcx.dispatch_keystroke(wh, Keystroke::parse("tab").unwrap());
    let frame = vcx.update(|window, cx| build_frame(ed.read(cx), window, px(600.), (0., 999_999.)));
    let row = frame
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Paragraph)
        .expect("段落行应在场");
    let concat: String = row.wrapped.iter().map(|wl| wl.text.as_ref()).collect();
    assert!(
        concat.starts_with("    "),
        "P1：行首 Tab 后渲染应回显 4 空格可见缩进: {concat:?}"
    );
}

/// P1：段落块首（光标在行首、尚无内容）按**空格** → 源出现 1 空格且渲染回显（可见）。
#[gpui::test]
fn space_at_paragraph_line_start_is_visible(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("abcd\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    ed.update(vcx, |e, cx| {
        e.set_caret(0);
        e.replace_text_utf16(None, " ", cx); // 行首按空格
    });
    ed.update(vcx, |e, _| {
        assert_eq!(e.text().as_ref(), " abcd\n", "行首空格应落进源文本");
    });
    let frame = vcx.update(|window, cx| build_frame(ed.read(cx), window, px(600.), (0., 999_999.)));
    let row = frame
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Paragraph)
        .expect("段落行应在场");
    let concat: String = row.wrapped.iter().map(|wl| wl.text.as_ref()).collect();
    assert!(
        concat.starts_with(' '),
        "P1：行首空格应渲染回显（可见）: {concat:?}"
    );
}

/// P1：整行选中 + Tab → 各源行行首 4 空格缩进且**逐行渲染回显**（可见缩进）。
#[gpui::test]
fn tab_whole_line_selection_indents_visibly_per_row(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    use gpui::Keystroke;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("aaa\nbbb\nccc\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let wh = focus_wy(&ed, vcx);
    // 整选三行（0..12 = "aaa\nbbb\nccc\n"）→ Tab → 三行各缩进 4 空格。
    ed.update(vcx, |e, _| {
        e.caret = 12;
        e.sel_anchor = Some(0);
    });
    vcx.dispatch_keystroke(wh, Keystroke::parse("tab").unwrap());
    ed.update(vcx, |e, _| {
        assert_eq!(
            e.text().as_ref(),
            "    aaa\n    bbb\n    ccc\n",
            "三行全选 Tab 应各缩进 4 空格"
        );
    });
    let frame = vcx.update(|window, cx| build_frame(ed.read(cx), window, px(600.), (0., 999_999.)));
    let paras: Vec<String> = frame
        .rows
        .iter()
        .filter(|r| r.kind == IrKind::Paragraph)
        .map(|r| r.wrapped.iter().map(|wl| wl.text.as_ref()).collect::<String>())
        .collect();
    assert_eq!(paras.len(), 3, "三行都应为段落");
    for (i, p) in paras.iter().enumerate() {
        assert!(
            p.starts_with("    "),
            "P1：整行选中 Tab 后第 {i} 行渲染应回显 4 空格缩进: {p:?}"
        );
    }
}
