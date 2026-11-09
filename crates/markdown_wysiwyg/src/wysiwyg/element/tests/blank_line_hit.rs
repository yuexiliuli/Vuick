use super::Frame; // 只有 Frame 是本模块还没 import 的名字（WyHost/build_frame 等已就位）

// WYSIWYG 空行点击命中回归（用户报告：点击空行不落光标）。
// 空行 = 源文档里字面空白的行（段落间 `\n\n`、文档头/尾空白行），shape 后是
// `wrapped` 为空、`editable=false` 的行（`shape_text_row`：空文本 → 无折行、
// byte_map 空 → `editable = !map2.is_empty() = false`）。老命中路径两处缺陷：
// ① `caret_geometry` 对 `editable=false` 恒回 `on=false` → 光标移到空行却画不出
// 竖条；② （`editable=true` 若遇空文本兜底）`wrapped.first()` 落空 → 返回
// `editor.caret()` 光标不动。此文件锁「点空行 → 光标落在该行源区间且画得出」的
// 真实渲染路径回归。
//
// ⚠️ 本模块是所有 `include!("tests/*.rs")` 的共享命名空间：`WyHost`/`build_frame`/
// `WyEditor`/`IrKind`/`point`/`px`/`size`/`Bounds`/`Modifiers`/`MouseButton`/
// `MouseDownEvent` 等已由 `byte_map.rs` 系/邻文件 import 进模块作用域，此处**不再
// 重复 `use`**（重复即 E0252）。


/// 按真实窗口内容宽建帧并注入、`last_bounds` 置原点。返回帧（含完整 rows）。
/// ⚠️ 须在 `set_caret` **之后**调用（`set_caret` 会清 `self.frame`，此处重建后注入，
/// 点击才读得到它）。
fn blank_harness(
    vcx: &mut VisualTestContext,
    ed: &Entity<WyEditor>,
) -> std::rc::Rc<Frame> {
    vcx.update(|window, cx| {
        let width = f32::from(window.viewport_size().width).max(1.);
        let frame = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
        let bounds_h = f32::from(frame.total).max(600.);
        ed.update(cx, |e, _| {
            e.set_last_bounds(Some(Bounds::new(
                point(px(0.), px(0.)),
                size(px(width), px(bounds_h)),
            )));
        });
        let rc = std::rc::Rc::new(frame);
        ed.update(cx, |e, _| *e.rd.borrow_mut().frame.borrow_mut() = Some(rc.clone()));
        rc
    })
}

/// 点/拖之后把同一帧重新注入（生产每输入一帧重建；测试手动回填，
/// `on_mouse_down`/`begin_selection_at`/`set_caret` 都会清 `self.frame`）。
fn reinject(vcx: &mut VisualTestContext, ed: &Entity<WyEditor>, frame: &std::rc::Rc<Frame>) {
    ed.update(vcx, |e, _| *e.rd.borrow_mut().frame.borrow_mut() = Some(frame.clone()));
}

fn down_at(x: f32, y: f32) -> MouseDownEvent {
    MouseDownEvent {
        button: MouseButton::Left,
        position: point(px(x), px(y)),
        modifiers: Modifiers::default(),
        click_count: 1,
        first_mouse: false,
    }
}

fn blank_rows(frame: &std::rc::Rc<Frame>) -> Vec<usize> {
    frame
        .rows
        .iter()
        .filter(|r| r.kind == IrKind::Blank)
        .map(|r| r.src_line)
        .collect()
}

fn click_and_caret(
    vcx: &mut VisualTestContext,
    ed: &Entity<WyEditor>,
    x: f32,
    y: f32,
) -> (usize, usize) {
    let down = down_at(x, y);
    vcx.update(|window, cx| {
        ed.update(cx, |e, cx| {
            e.on_mouse_down(&down, window, cx);
        });
        let e = ed.read(cx);
        let (row, _col) = e.row_col(e.caret());
        (e.caret(), row)
    })
}

/// 文档 `"上\n\n下\n"`：段落间空行。点空行 y 中心 → 光标落在该空行源区间
///（`line_start..=line_end` 内），非保持原值。
#[gpui::test]
fn blank_line_between_paragraphs_moves_caret(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("上\n\n下\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    // 先落一个确定的光标（行 0 首字节），再建帧注入——确保空行命中真实「移动」。
    ed.update(vcx, |e, _| e.set_caret(0));
    let frame = blank_harness(vcx, &ed);
    let blanks = blank_rows(&frame);
    assert!(
        !blanks.is_empty(),
        "文档 `上\n\n下\n` 应含空行块，实际行：{:?}",
        frame
            .rows
            .iter()
            .map(|r| (r.src_line, r.kind, r.editable, r.top, r.height))
            .collect::<Vec<_>>()
    );
    for line in blanks {
        let r = frame.rows.iter().find(|r| r.src_line == line).unwrap();
        let (top, h) = (f32::from(r.top), f32::from(r.height));
        let x = f32::from(frame.padding) + 4.;
        let y = top + h / 2.;
        let (caret_after, row_after) = click_and_caret(vcx, &ed, x, y);
        let ls = ed.read_with(vcx, |e, _| e.line_start(line));
        let le = ed.read_with(vcx, |e, _| e.line_end(line));
        assert!(
            caret_after >= ls && caret_after <= le,
            "点空行 src_line={line}（y={y} top={top} h={h}）光标应落在行源区间 \
             [{ls},{le}]，实际 caret={caret_after}"
        );
        assert_eq!(row_after, line, "点空行后 caret 应在该空行 src_line={line}");
        // —— 用户可见核心：光标画得出（caret_geometry.on=true）且落点在空行带内。
        // 空行 `editable=false` → 老 `caret_geometry` 走「只读不画」分支返回 on=false，
        // 光标移到该行却无竖条（本 bug 的直接用户症状）。
        let f2 = blank_harness(vcx, &ed);
        let (caret_on, _, carety) = vcx.update(|_, cx| {
            let e = ed.read(cx);
            let g = caret_geometry(e, &f2);
            (g.0, g.1, g.2)
        });
        let t2 = f32::from(carety);
        assert!(
            caret_on && t2 >= top && t2 < top + h,
            "点空行 src_line={line} 光标应可见（caret_geometry.on=true）且 top 落于本行带 \
             [{top},{})，实际 on={caret_on} top={t2}",
            top + h
        );
    }
}

/// 空行在**文档开头**（`"\n上\n"`）→ 点开头空行光标落在该行。
#[gpui::test]
fn blank_line_at_doc_start_moves_caret(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("\n上\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    ed.update(vcx, |e, _| e.set_caret(usize::MAX - 1));
    let frame = blank_harness(vcx, &ed);
    let blanks = blank_rows(&frame);
    assert!(!blanks.is_empty(), "`\\n上\\n` 应含开头空行");
    let line = blanks[0];
    let r = frame.rows.iter().find(|r| r.src_line == line).unwrap();
    let x = f32::from(frame.padding) + 4.;
    let y = f32::from(r.top) + f32::from(r.height) / 2.;
    let (caret_after, row_after) = click_and_caret(vcx, &ed, x, y);
    assert_eq!(
        row_after, line,
        "点开头空行 src_line={line} 光标应落在该空行，实际 caret={caret_after} (row {row_after})"
    );
}

/// 空行在**文档结尾**（`"上\n\n"`）→ 点结尾空行光标落在尾部。
#[gpui::test]
fn blank_line_at_doc_end_moves_caret(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("上\n\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    ed.update(vcx, |e, _| e.set_caret(0));
    let frame = blank_harness(vcx, &ed);
    let blanks = blank_rows(&frame);
    assert!(!blanks.is_empty(), "`上\\n\\n` 应含结尾空行");
    let line = *blanks.last().unwrap();
    let r = frame.rows.iter().find(|r| r.src_line == line).unwrap();
    let x = f32::from(frame.padding) + 4.;
    let y = f32::from(r.top) + f32::from(r.height) / 2.;
    let (caret_after, row_after) = click_and_caret(vcx, &ed, x, y);
    assert_eq!(
        row_after, line,
        "点结尾空行 src_line={line} 光标应落在该空行，实际 caret={caret_after} (row {row_after})"
    );
}

/// 空行后**再点文本行** → 光标回到文本行（空行命中不污染后续文本点击）。
#[gpui::test]
fn click_blank_then_text_still_works(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("上\n\n下\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let frame = blank_harness(vcx, &ed);
    let text_rows: Vec<usize> = frame
        .rows
        .iter()
        .filter(|r| r.kind == IrKind::Paragraph && !r.wrapped.is_empty())
        .map(|r| r.src_line)
        .collect();
    assert!(text_rows.len() >= 2, "应有至少两个段落行：{:?}", text_rows);
    let blank = blank_rows(&frame)[0];
    // 点空行 → 光标在空行。
    let r = frame.rows.iter().find(|r| r.src_line == blank).unwrap();
    let (_, row_after) = click_and_caret(
        vcx,
        &ed,
        f32::from(frame.padding) + 4.,
        f32::from(r.top) + f32::from(r.height) / 2.,
    );
    assert_eq!(row_after, blank, "点空行后光标应在空行");
    // 再点文本行「下」中部 → 光标落回文本行。
    reinject(vcx, &ed, &frame);
    let tr = frame.rows.iter().find(|r| r.src_line == text_rows[1]).unwrap();
    let (caret, row_after) = click_and_caret(
        vcx,
        &ed,
        f32::from(frame.padding) + 40.,
        f32::from(tr.top) + f32::from(tr.height) / 2.,
    );
    assert_eq!(
        row_after, text_rows[1],
        "点完空行再点文本行，光标应落在文本行 src_line={}，实际 caret={caret} row={row_after}",
        text_rows[1]
    );
}

/// 列表/引述后紧跟空行：点空行仍落该行（非首例位置：列表正文之后）。
#[gpui::test]
fn blank_after_list_blockquote_moves_caret(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("- 项\n\n> 引\n\n尾文\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    ed.update(vcx, |e, _| e.set_caret(0));
    let frame = blank_harness(vcx, &ed);
    let blanks = blank_rows(&frame);
    assert!(
        blanks.len() >= 2,
        "列表/引述后的空行应存在：{:?}（rows {:?}）",
        blanks,
        frame
            .rows
            .iter()
            .map(|r| (r.src_line, r.kind))
            .collect::<Vec<_>>()
    );
    for line in blanks {
        let r = frame.rows.iter().find(|r| r.src_line == line).unwrap();
        let (_, row_after) = click_and_caret(
            vcx,
            &ed,
            f32::from(frame.padding) + 4.,
            f32::from(r.top) + f32::from(r.height) / 2.,
        );
        assert_eq!(
            row_after, line,
            "列表/引述后点空行 src_line={line} 光标应落在该空行"
        );
        reinject(vcx, &ed, &frame);
    }
}

/// 拖选跨越空行：按下在空行前文字、拖到空行后文字 → 选区包含空行源字节。
#[gpui::test]
fn drag_selection_across_blank_line(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("甲\n\n乙\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let frame = blank_harness(vcx, &ed);
    let para: Vec<usize> = frame
        .rows
        .iter()
        .filter(|r| r.kind == IrKind::Paragraph && !r.wrapped.is_empty())
        .map(|r| r.src_line)
        .collect();
    assert!(para.len() >= 2, "应有段落行：{:?}", para);
    let blank = blank_rows(&frame)[0];
    let r0 = frame.rows.iter().find(|r| r.src_line == para[0]).unwrap();
    let rb = frame.rows.iter().find(|r| r.src_line == blank).unwrap();
    let r1 = frame.rows.iter().find(|r| r.src_line == para[1]).unwrap();
    // 按下在第一段文字上。
    let down = down_at(
        f32::from(frame.padding) + 4.,
        f32::from(r0.top) + f32::from(r0.height) / 2.,
    );
    vcx.update(|window, cx| {
        ed.update(cx, |e, cx| {
            e.on_mouse_down(&down, window, cx);
        });
    });
    // 在每个 move 之前把同一帧重新注入（生产每输入重建一帧；`on_mouse_down` 已清帧）。
    ed.update(vcx, |e, _| *e.rd.borrow_mut().frame.borrow_mut() = Some(frame.clone()));
    let mv = gpui::MouseMoveEvent {
        position: point(
            px(f32::from(frame.padding) + 40.),
            px(f32::from(rb.top) + f32::from(rb.height) / 2.),
        ),
        pressed_button: Some(MouseButton::Left),
        modifiers: Modifiers::default(),
    };
    vcx.update(|_window, cx| {
        ed.update(cx, |e, cx| {
            e.on_mouse_move(&mv, cx);
        });
    });
    ed.update(vcx, |e, _| *e.rd.borrow_mut().frame.borrow_mut() = Some(frame.clone()));
    let mv2 = gpui::MouseMoveEvent {
        position: point(
            px(f32::from(frame.padding) + 40.),
            px(f32::from(r1.top) + f32::from(r1.height) / 2.),
        ),
        pressed_button: Some(MouseButton::Left),
        modifiers: Modifiers::default(),
    };
    let (sel_start, sel_end) = vcx.update(|_window, cx| {
        ed.update(cx, |e, cx| {
            e.on_mouse_move(&mv2, cx);
        });
        let e = ed.read(cx);
        let sel = e.selection().expect("拖选应产生非空选区");
        (sel.start, sel.end)
    });
    let blank_ls = ed.read_with(vcx, |e, _| e.line_start(blank));
    let blank_le = ed.read_with(vcx, |e, _| e.line_end(blank));
    let ls0 = ed.read_with(vcx, |e, _| e.line_start(para[0]));
    let le1 = ed.read_with(vcx, |e, _| e.line_end(para[1]));
    assert!(
        sel_start < blank_ls && sel_end > blank_le && sel_start <= le1 && sel_end >= ls0,
        "拖选过空行应跨越空行源字节：sel=[{sel_start},{sel_end}] blank=[{blank_ls},{blank_le}] \
         首段=[{ls0},..] 二段=[..,{le1}]"
    );
}

/// 空文档（0 块，`frame` 占位为单个 `IrKind::Blank` 行，见 `empty_frame`）：点文档 →
/// 光标落行 0 首字节且 `caret_geometry.on=true`（空文档也画得出光标）。
#[gpui::test]
fn blank_empty_doc_click_shows_caret(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(""));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    ed.update(vcx, |e, _| e.set_caret(usize::MAX - 1));
    let frame = blank_harness(vcx, &ed);
    let r = frame
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Blank)
        .expect("空文档占位帧应有 Blank 行");
    let (top, h) = (f32::from(r.top), f32::from(r.height));
    let (caret, row) = click_and_caret(
        vcx,
        &ed,
        f32::from(frame.padding) + 4.,
        top + h / 2.,
    );
    assert_eq!(row, 0, "点空文档光标应落行 0（占位行 src_line={}）", r.src_line);
    let f2 = blank_harness(vcx, &ed);
    let (on, _, carety) = vcx.update(|_, cx| {
        let e = ed.read(cx);
        let g = caret_geometry(e, &f2);
        (g.0, g.1, g.2)
    });
    let t2 = f32::from(carety);
    assert!(
        on && t2 >= top && t2 < top + h,
        "点空文档光标应可见（on={on}）且落占位行带 [{top},{})，实际 top={t2}",
        top + h
    );
    assert_eq!(caret, 0, "空文档点按后光标应落行 0 首字节");
}
