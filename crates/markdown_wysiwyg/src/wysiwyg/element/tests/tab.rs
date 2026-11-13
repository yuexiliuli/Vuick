// Tab 键在 WYSIWYG 实时编辑区（live2，`wy-editor` 键上下文）的插入语义（E3）。
//
// 期望：WY 持焦按 Tab → 在光标处插入**可见缩进**（4 空格，与源码编辑区 `tab_size=4`
// 同单位；不插 `\t`——真机 DirectWrite 把它排成零宽），且渲染帧显示得出来；绝不触发
// Root 层的 Tab 焦点循环。markdown 语义护栏：段落行首空白自 2026-11-13 起**回显**
// （`trim_paragraph` 保留行首 ASCII 空白，P1 所见即所得）但**不产生**代码块等块语义
// 变化；列表中标记后的 4 空格保留；代码块内容中的 4 空格保留。
//
// ⚠️ 共享命名空间（byte_map.rs 顶部的 import 已就位，禁止重复 use）：
// `WyHost`/`build_frame`/`WyEditor`/`point`/`px`/`size`/`VisualTestContext`/
// `TestAppContext`。`Keystroke`/`IrKind` 按邻文件惯例函数内局部 use。

/// 单编辑器（WyHost 内包装）的聚焦助手：`window.focus` + 返回窗口句柄。
fn focus_wy(host: &Entity<WyEditor>, vcx: &mut VisualTestContext) -> gpui::AnyWindowHandle {
    vcx.update(|window, cx| {
        let fh = host.read(cx).focus_handle();
        window.focus(&fh, cx);
    });
    vcx.windows().first().copied().expect("窗口句柄")
}

/// 该行（`frame.rows` 首个命中 kind 的行）的可见整形总宽（px）。
fn row_width(row: &super::ShapedRow) -> f32 {
    row.wrapped
        .iter()
        .map(|wl| wl.unwrapped_layout.width.as_f32())
        .sum()
}

/// t3-段落（真实窗口拓扑：WY 宿主在 `ui::root::Root` 内，Root 也登记了
/// `tab→focus_next`）：WY 持焦按 Tab **不被** Root 焦点循环劫走，光标处插入 4 空格
/// 缩进，渲染帧可见（渲染文本保留 4 空格、整形宽度变大）。需 Root 排版一并登记（同 main.rs）。
#[gpui::test]
fn tab_in_paragraph_inserts_visible_indent(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    use gpui::{AppContext, Keystroke};
    // 与 main.rs 一致：Input + wy-editor + Root 三套键上下文全登记。
    cx.update(|app| {
        base::actions::bind_input_keys(app);
        crate::wysiwyg::bind_keys(app);
        ui::root::bind_keys(app);
    });
    let (root, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("ab\n"));
        ui::root::Root::new(ed, window, cx)
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = root.read_with(vcx, |r, _| {
        r.view().clone().downcast::<WyEditor>().expect("Root 内应承载 WyEditor")
    });

    // 插入前基准帧（同一编辑器内部对比，免字体依赖）。
    let w0 = vcx.update(|window, cx| {
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Paragraph)
            .expect("应有段落行");
        row_width(row)
    });

    // 聚焦编辑器 → 光标落 "ab" 中间（源 offset 1）→ 按 Tab。
    vcx.update(|window, cx| {
        let fh = ed.read(cx).focus_handle();
        window.focus(&fh, cx);
    });
    let wh = vcx.windows().first().copied().expect("窗口句柄");
    ed.update(vcx, |e, _| e.set_caret(1));
    vcx.dispatch_keystroke(wh, Keystroke::parse("tab").unwrap());

    vcx.update(|window, cx| {
        let e = ed.read(cx);
        assert_eq!(
            e.text().as_ref(),
            "a    b\n",
            "段落中按 Tab 应在光标处插入 4 空格可见缩进（H2 修复：不插 `\\t`——真机 DirectWrite 零宽）"
        );
        assert_eq!(e.caret(), 5, "插入后光标应停在 4 空格之后（1 + 4）");
        let frame = build_frame(e, window, px(600.), (0., 999_999.));
        let row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Paragraph)
            .expect("插入后仍应为段落行（块语义不变）");
        let concat: String = row.wrapped.iter().map(|wl| wl.text.as_ref()).collect();
        assert!(concat.contains("    "), "渲染文本应保留 4 空格缩进（否则不可见）: {concat:?}");
        let w1 = row_width(row);
        assert!(
            w1 > w0,
            "含缩进的段落可见宽应大于插入前（{w1:.1} vs {w0:.1}）——缩进必须显示出来"
        );
    });
}

/// t3-列表：列表项内（标记后）按 Tab → 源出现 4 空格、渲染保留、宽度变大。
#[gpui::test]
fn tab_in_list_item_after_marker_inserts_visible_indent(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    use gpui::Keystroke;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("- ab\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());

    let w0 = vcx.update(|window, cx| {
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Bullet)
            .expect("应有列表项行");
        row_width(row)
    });

    let wh = focus_wy(&ed, vcx);
    // 光标落在列表标记 `- ` 之后（源 offset 2）。
    ed.update(vcx, |e, _| e.set_caret(2));
    vcx.dispatch_keystroke(wh, Keystroke::parse("tab").unwrap());

    vcx.update(|window, cx| {
        let e = ed.read(cx);
        assert_eq!(e.text().as_ref(), "-     ab\n", "列表项 Tab 应在标记后插入 4 空格缩进");
        let frame = build_frame(e, window, px(600.), (0., 999_999.));
        let row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Bullet)
            .expect("插入后仍应为列表项行");
        let concat: String = row.wrapped.iter().map(|wl| wl.text.as_ref()).collect();
        assert!(concat.contains("    "), "列表渲染文本应保留 4 空格缩进: {concat:?}");
        assert!(
            row_width(row) > w0,
            "列表行含 4 空格缩进后可见宽应变大"
        );
    });
}

/// t3-代码块：代码首内容行按 Tab → 行首插入 4 空格缩进、渲染可见（等宽行宽变大）、
/// 行仍为 Code（不改变块语义）。
#[gpui::test]
fn tab_in_code_block_line_start_inserts_visible_indent(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    use gpui::Keystroke;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "```\ncd\n```\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());

    let w0 = vcx.update(|window, cx| {
        let frame = build_frame(ed.read(cx), window, px(600.), (0., 999_999.));
        let row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Code)
            .expect("应有代码块行");
        row_width(row)
    });

    let wh = focus_wy(&ed, vcx);
    // 光标落在代码首内容行行首（offset = "```\n".len()）。
    ed.update(vcx, |e, _| e.set_caret(4));
    vcx.dispatch_keystroke(wh, Keystroke::parse("tab").unwrap());

    vcx.update(|window, cx| {
        let e = ed.read(cx);
        assert_eq!(e.text().as_ref(), "```\n    cd\n```\n", "代码行行首应插入 4 空格缩进");
        let frame = build_frame(e, window, px(600.), (0., 999_999.));
        let row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Code)
            .expect("插入后仍应为代码块行（块语义不变）");
        let concat: String = row.wrapped.iter().map(|wl| wl.text.as_ref()).collect();
        assert!(concat.contains("    "), "代码渲染文本应保留 4 空格缩进: {concat:?}");
        assert!(row_width(row) > w0, "代码行含 4 空格缩进后可见宽应变大（等宽缩进可见）");
    });
}

/// t3-段落行首（markdown 语义护栏 + P1 可见缩进）：源文本出现 4 空格（字节插入成立），
/// 渲染**回显**这 4 空格（P1：行首 Tab 不再被 `trim_paragraph` 吞掉，所见即所得）——
/// 且**不产生**代码块等意外块语义（行仍是 Paragraph）。精确断言「插入什么 / 渲染成什么」。
#[gpui::test]
fn tab_at_paragraph_line_start_lands_in_source_without_block_flip(cx: &mut TestAppContext) {
    use crate::markdown::ir::IrKind;
    use gpui::Keystroke;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("cd\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let wh = focus_wy(&ed, vcx);
    // 光标在段落行首（offset 0）→ Tab。
    ed.update(vcx, |e, _| e.set_caret(0));
    vcx.dispatch_keystroke(wh, Keystroke::parse("tab").unwrap());
    vcx.update(|window, cx| {
        let e = ed.read(cx);
        assert_eq!(e.text().as_ref(), "    cd\n", "行首 Tab 应落进源文本（字节级成立）");
        let frame = build_frame(e, window, px(600.), (0., 999_999.));
        // 行首空白**回显**（P1 修复：不再被 trim 裁掉），块语义不能翻车成代码块。
        assert!(
            frame.rows.iter().any(|r| r.kind == IrKind::Paragraph),
            "行首 Tab 不得把段落改写成代码块/其它块"
        );
        let row = frame
            .rows
            .iter()
            .find(|r| r.kind == IrKind::Paragraph)
            .expect("段落行应在场");
        let concat: String = row.wrapped.iter().map(|wl| wl.text.as_ref()).collect();
        assert_eq!(concat, "    cd", "行首空白应回显为可见缩进（P1），渲染为 `    cd`");
    });
}

/// 只读（`set_editable(false)`）时按 Tab 不得被 `wy-tab` 吞掉：无插入、且按键落回
/// Root 的 `tab→focus_next`（焦点从只读预览 WY 移到同宿主内下一个可聚焦控件）。
/// E3 只读门控前：`wy-tab` 绑定仍解析（`wy-editor` 上下文无条件挂载）且 on_action
/// 无条件登记 → 动作冒泡默认停传 → Root 焦点循环永不触发（回归）。
struct RootReadOnlyHost {
    wy: Entity<WyEditor>,
    other: gpui::FocusHandle,
}
impl Render for RootReadOnlyHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        use gpui::{InteractiveElement as _, ParentElement as _};
        gpui::div()
            .id("ro-host")
            .child(self.wy.clone())
            .child(gpui::div().id("other").track_focus(&self.other).child("B"))
    }
}

#[gpui::test]
fn read_only_tab_passes_through_to_root_focus_next(cx: &mut TestAppContext) {
    use gpui::Keystroke;
    // 与 main.rs 一致：wy-editor + Root 两套键上下文全登记。
    cx.update(|app| {
        crate::wysiwyg::bind_keys(app);
        ui::root::bind_keys(app);
    });
    let (root, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("ab\n"));
        ed.update(cx, |e, _| e.set_editable(false)); // A1：只读预览门控
        let other = cx.focus_handle().tab_stop(true);
        let view = cx.new(move |_| RootReadOnlyHost { wy: ed, other });
        ui::root::Root::new(view, window, cx)
    });
    let vcx: &mut VisualTestContext = vcx;
    // 渲染若干帧，使 dispatch tree 含 WY 与 `other` 两个可聚焦节点。
    for _ in 0..4 {
        vcx.run_until_parked();
        let _ = vcx.refresh();
    }
    vcx.run_until_parked();
    let (ed, hb) = vcx.update(|_, cx| {
        let host = root
            .read(cx)
            .view()
            .clone()
            .downcast::<RootReadOnlyHost>()
            .expect("Root 内应承载 RootReadOnlyHost");
        (host.read(cx).wy.clone(), host.read(cx).other.clone())
    });
    let wh = vcx.windows().first().copied().expect("窗口句柄");
    // 聚焦只读 WY → 按 Tab。
    vcx.update(|window, cx| {
        let fh = ed.read(cx).focus_handle();
        window.focus(&fh, cx);
    });
    vcx.dispatch_keystroke(wh, Keystroke::parse("tab").unwrap());
    vcx.run_until_parked();
    let (text, focused) = vcx.update(|window, cx| {
        let e = ed.read(cx);
        (e.text().to_string(), window.focused(cx))
    });
    assert_eq!(text, "ab\n", "只读预览按 Tab 不得插入任何字符");
    assert_eq!(
        focused,
        Some(hb),
        "只读预览按 Tab 应落回 Root 焦点循环（焦点移到下一个可聚焦控件），不被 wy-tab 吞掉"
    );
}


/// E2-delta：跨多行选区 + Tab → 各源行行首多一层 4 空格缩进、原字符逐字节保留、选区平移
/// 后仍覆盖原行（与编辑区 `indent_selected_lines` 同语义）。修复前：整套选区被替换
/// 成单个 `\t` → 两行内容消失（RED）。
#[gpui::test]
fn tab_multiline_selection_indents_each_source_line(cx: &mut TestAppContext) {
    use gpui::Keystroke;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("aaa\nbbb\nccc\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let wh = focus_wy(&ed, vcx);
    // 整选第 0、1 两行（0..8 = "aaa\nbbb\n"）。
    ed.update(vcx, |e, _| {
        e.caret = 8;
        e.sel_anchor = Some(0);
    });
    vcx.dispatch_keystroke(wh, Keystroke::parse("tab").unwrap());
    vcx.update(|_, cx| {
        let e = ed.read(cx);
        assert_eq!(
            e.text().as_ref(),
            "    aaa\n    bbb\nccc\n",
            "跨两行选区 Tab 应两行行首各插 4 空格缩进、原文字逐字节保留（修复前整段被替换成单个 Tab）"
        );
        assert_eq!(e.selection(), Some(4..16), "缩进后选区应平移覆盖原两行");
    });
}

/// E2-delta：单行**部分**选中 + Tab → 整行缩进（VS Code 惯例），选中的字符不丢、
/// 选区平移仍覆盖原字符。
#[gpui::test]
fn tab_partial_line_selection_indents_whole_line(cx: &mut TestAppContext) {
    use gpui::Keystroke;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("aaa\nbbb\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let wh = focus_wy(&ed, vcx);
    // 只选中第 0 行中间两个字符（1..3 = "aa"）：Tab 应整行缩进（行首前缀，非替换）。
    ed.update(vcx, |e, _| {
        e.caret = 3;
        e.sel_anchor = Some(1);
    });
    vcx.dispatch_keystroke(wh, Keystroke::parse("tab").unwrap());
    vcx.update(|_, cx| {
        let e = ed.read(cx);
        assert_eq!(
            e.text().as_ref(),
            "    aaa\nbbb\n",
            "部分选中一行 Tab 应整行缩进，选中的字符 + 行首前缀都不丢"
        );
        assert_eq!(e.selection(), Some(5..7), "选区平移仍覆盖原 'aa'（每行缩进 4 字节）");
    });
}

/// E2-delta：跨多行缩进是一笔原子编辑 → 单次撤销整组回退（含选区态不残留）。
#[gpui::test]
fn tab_multiline_indent_undo_restores(cx: &mut TestAppContext) {
    use gpui::Keystroke;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("aaa\nbbb\nccc\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let wh = focus_wy(&ed, vcx);
    ed.update(vcx, |e, _| {
        e.caret = 8;
        e.sel_anchor = Some(0);
    });
    vcx.dispatch_keystroke(wh, Keystroke::parse("tab").unwrap());
    vcx.update(|_, cx| {
        assert_eq!(ed.read(cx).text().as_ref(), "    aaa\n    bbb\nccc\n");
    });
    vcx.dispatch_keystroke(wh, Keystroke::parse("ctrl-z").unwrap());
    vcx.update(|_, cx| {
        assert_eq!(
            ed.read(cx).text().as_ref(),
            "aaa\nbbb\nccc\n",
            "单次撤销应把整组 Tab 缩进回退"
        );
    });
}

/// E2-delta：列表两行全选 + Tab → 每行行首 4 空格缩进（列表行随之嵌套 = 用户接受的
/// 语义，不必特判，按行首前缀即可），源字符合部保留。
#[gpui::test]
fn tab_multiline_list_selection_nests_lines(cx: &mut TestAppContext) {
    use gpui::Keystroke;
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("- a\n- b\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let wh = focus_wy(&ed, vcx);
    ed.update(vcx, |e, _| {
        e.caret = "- a\n- b\n".len();
        e.sel_anchor = Some(0);
    });
    vcx.dispatch_keystroke(wh, Keystroke::parse("tab").unwrap());
    vcx.update(|_, cx| {
        assert_eq!(
            ed.read(cx).text().as_ref(),
            "    - a\n    - b\n",
            "列表行行首 4 空格缩进 = 嵌套一层，源字符合部保留"
        );
    });
}
