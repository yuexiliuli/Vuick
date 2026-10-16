use crate::markdown::ir::IrKind;
use std::collections::HashMap;
use std::sync::Arc;

use super::{note_marker_hit_at, NoteMarkerHit};
use gpui::MouseUpEvent;

/// 构造仅含「PNG 签名 + IHDR 头」的伪 PNG 字节（`image_dimensions_bytes` 只嗅头部，
/// 故只需头 24 字节即可读出固有尺寸；不参与整图解码）。
fn fake_png(w: u32, h: u32) -> Vec<u8> {
    let mut d = Vec::with_capacity(24);
    d.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    d.extend_from_slice(&[0, 0, 0, 13]);
    d.extend_from_slice(b"IHDR");
    d.extend_from_slice(&w.to_be_bytes());
    d.extend_from_slice(&h.to_be_bytes());
    d
}

/// P4-1：EPUB `%%ALIGN:RIGHT%%` 指令 → 该文本行右对齐（逐带移量 = 可用宽 − 行宽）；
/// 无指令的普通正文不产生对齐移量（恒左，行为与改造前逐位一致）。
#[gpui::test]
fn epub_align_directive_right_offsets(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("%%ALIGN:RIGHT%%落款行\n\n普通正文段\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let (frame, width) = vcx.update(|window, cx| {
        // EPUB 只读阅读模式：量程/渲染与 WYSIWYG 同源，仅多 EPUB 语义。
        ed.update(cx, |e, _| e.set_editable(false));
        ed.update(cx, |e, _| e.set_epub(true));
        let width = f32::from(window.viewport_size().width).max(1.);
        let frame = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
        (std::rc::Rc::new(frame), width)
    });
    let sig = frame.rows.iter().find(|r| r.src_line == 0).expect("落款行");
    let para = frame.rows.iter().find(|r| r.src_line == 2).expect("普通正文段");
    assert_eq!(sig.kind, IrKind::Paragraph);
    assert!(!sig.align_offsets.is_empty(), "右对齐行应有逐带移量");
    assert!(sig.align_offsets[0] > px(0.), "落款行右对齐：首带移量应 > 0");
    assert!(para.align_offsets.is_empty(), "无指令的普通正文不应对齐");
    let avail = width - 2.0 * f32::from(frame.padding);
    let line_w = f32::from(sig.wrapped[0].width());
    let expect = (avail - line_w).max(0.);
    assert!(
        (f32::from(sig.align_offsets[0]) - expect).abs() < 0.6,
        "移量应 = 可用宽 − 行宽：expect {expect:.1}，实测 {:.1}",
        f32::from(sig.align_offsets[0])
    );
}

/// P4-1：EPUB 阅读模式下**标题**默认居中（对齐 A 侧 `set_epub` 语义：
/// `align.or(if epub { Center })`）；非 EPUB 模式标题不居中（保持左对齐）。
#[gpui::test]
fn epub_heading_centers_in_reading_mode(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("# 第一章\n\n正文\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let width = vcx.update(|window, _| f32::from(window.viewport_size().width).max(1.));
    // 非 EPUB：标题左对齐（无逐带移量）。
    let frame = vcx.update(|window, cx| {
        let f = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
        std::rc::Rc::new(f)
    });
    let h1 = frame.rows.iter().find(|r| r.src_line == 0).expect("标题行");
    assert!(h1.align_offsets.is_empty(), "非 EPUB 标题不应居中");
    // EPUB：标题居中（首带移量 ≈ (可用宽 − 行宽)/2）。
    vcx.update(|window, cx| {
        ed.update(cx, |e, _| e.set_epub(true));
        window.refresh();
    });
    let frame_e = vcx.update(|window, cx| {
        let f = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
        std::rc::Rc::new(f)
    });
    let h1e = frame_e.rows.iter().find(|r| r.src_line == 0).expect("标题行");
    assert!(!h1e.align_offsets.is_empty(), "EPUB 标题应居中");
    let line_w = f32::from(h1e.wrapped[0].width());
    // 对齐移量以内容盒为基准并已扣标题 gutter（x_off）：文本实际左缘应落在
    // 「内容盒中心 − 行宽/2」（≈ 视口中心 − 行宽/2，左右内衬对称）。
    let pad = f32::from(frame_e.padding);
    let text_left = pad + f32::from(h1e.x_off) + f32::from(h1e.align_offsets[0]);
    let expect_left = width / 2.0 - line_w / 2.0;
    assert!(
        (text_left - expect_left).abs() < 0.6,
        "EPUB 标题居中：text_left 应 ≈ 视口中心 − 行宽/2：expect {expect_left:.1}，实测 {text_left:.1}"
    );
}

/// P4-1：对齐来源为解析出的 `RowAlign`（`%%ALIGN:LEFT%%` 应为左 → 无移量）。
#[gpui::test]
fn epub_align_left_has_no_offset(cx: &mut TestAppContext) {
    assert_eq!(
        crate::markdown::parse::RowAlign::Left,
        crate::markdown::parse::RowAlign::Left
    );
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("%%ALIGN:LEFT%%左对齐行\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let width = vcx.update(|window, _| f32::from(window.viewport_size().width).max(1.));
    let frame = vcx.update(|window, cx| {
        ed.update(cx, |e, _| e.set_epub(true));
        let f = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
        std::rc::Rc::new(f)
    });
    let sig = frame.rows.iter().find(|r| r.src_line == 0).expect("左对齐行");
    assert!(sig.align_offsets.is_empty(), "LEFT 不产生移量（左对齐恒 0）");
}

/// P4-1：EPUB 只读门控——`set_editable(false)` 后不可编辑；内存图注册后
/// `lazy_ensure_image_size` 能从字节嗅出固有尺寸（`epubimg://` 虚拟 src 不落盘）。
#[gpui::test]
fn epub_readonly_and_memory_image_size(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text("正文\n![图](epubimg://1.png)\n"));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    ed.update(vcx, |e, _| e.set_editable(false));
    assert!(
        !ed.read_with(vcx, |e, _| e.is_editable()),
        "EPUB 宿主应只读"
    );
    let mut map: HashMap<String, Arc<gpui::Image>> = HashMap::new();
    map.insert(
        "epubimg://1.png".to_string(),
        Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            fake_png(40, 30),
        )),
    );
    ed.update(vcx, |e, _| e.set_memory_images(map));
    ed.update(vcx, |e, _| e.lazy_ensure_image_size("epubimg://1.png"));
    assert_eq!(
        ed.read_with(vcx, |e, _| e.image_size("epubimg://1.png")),
        Some(Some((40.0, 30.0))),
        "内存图固有尺寸应从字节嗅出"
    );
}

/// P4-1：真实 EPUB 流程（xy.epub 由 app 层喂源）在元素层也能渲染：含 `%%ALIGN%%`
/// 行右对齐 + `epubimg://` 独立图片行（图片行带 `image` 数据）。用内联 markdown 构造。
#[gpui::test]
fn epub_image_row_and_align_render_together(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        // 对齐落款 + 独立图片行（EPUB 转换器的典型产物）。
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text(
                "# 第一章\n\n%%ALIGN:RIGHT%%卡尔·马克思　弗里德里希·恩格斯\n\n![插图](epubimg://2.jpeg)\n",
            )
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    let mut map: HashMap<String, Arc<gpui::Image>> = HashMap::new();
    map.insert(
        "epubimg://2.jpeg".to_string(),
        Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Jpeg,
            {
                let mut b = vec![0xFF, 0xD8];
                b.extend_from_slice(&[0, 0, 0, 0]); // 无 SOF0：尺寸嗅探失败 → 占位高（不影响行存在断言）
                b
            },
        )),
    );
    ed.update(vcx, |e, _| e.set_memory_images(map));
    ed.update(vcx, |e, _| e.set_epub(true));
    let width = vcx.update(|window, _| f32::from(window.viewport_size().width).max(1.));
    let frame = vcx.update(|window, cx| {
        let f = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
        std::rc::Rc::new(f)
    });
    let sig = frame.rows.iter().find(|r| r.src_line == 2).expect("落款行");
    assert!(!sig.align_offsets.is_empty(), "%ALIGN:RIGHT%% 落款行右对齐");
    let img = frame
        .rows
        .iter()
        .find(|r| r.kind == IrKind::Image)
        .expect("应含独立图片行");
    assert_eq!(
        img.image.as_ref().map(|i| i.src.as_ref()),
        Some("epubimg://2.jpeg"),
        "图片行 src 应为 EPUB 内存虚拟 src"
    );
}

/// P4-2 回归修复：静态 `[n]` 标号要有 accent 高亮（用户报告「标号如[1][2]的高亮没了」）。
/// 根因：注记迁入 WYSIWYG 后 marker 显示区间只是普通正文 run，paint 只画跳转后的瞬态
/// 加亮。修复：`extract_notes` 在剥记号的同时把 `[n]` 显示区间登记成 `RunKind::NoteMarker`
///（与既有 runs 无重叠 break-merge），`runs_to_text_runs` 染成 accent 色（对齐 A 侧预览
/// `link_color = p.accent`）、不下划线。锁两条：① parse 后 marker 区间带
/// `RunKind::NoteMarker`（真实渲染路径上游）；② 该 run 经 `runs_to_text_runs` 产出
/// accent 色 TextRun（真实渲染路径具体化）。
#[test]
fn note_marker_static_highlight_restored() {
    use crate::markdown::parse::{RunKind, parse_to_rows};
    use super::runs_to_text_runs;
    // ① 「正文[^1]带注段。」 → 显示「正文[1]带注段。」，`[1]` 显示区间 = 6..9 字节。
    let rows = parse_to_rows("正文[^1]带注段。");
    assert_eq!(rows[0].text.as_ref(), "正文[1]带注段。");
    let mk_run = rows[0]
        .runs
        .iter()
        .find(|(rg, k)| *k == RunKind::NoteMarker && rg.start == 6 && rg.end == 9)
        .expect("注记标记显示区间应带 RunKind::NoteMarker（静态高亮来源）");
    assert_eq!(mk_run.0.len(), 3, "标记区间 = `[1]` 3 字节");
    // ② runs_to_text_runs 把 NoteMarker 染成 accent 色、无下划线（A 侧 p.accent 同色）。
    let base = gpui::font("Arial");
    let mono = gpui::font("Consolas");
    let accent: gpui::Hsla = gpui::rgb(0x3b82f6).into();
    let text = rows[0].text.as_ref();
    let out = runs_to_text_runs(
        text,
        &rows[0].runs,
        &base,
        gpui::Hsla::default(),
        &mono,
        accent,
        gpui::Hsla::default(),
        gpui::Hsla::default(),
    );
    let marker_run = out
        .iter()
        .find(|r| r.color == accent && r.len == 3)
        .expect("`[1]` 标记应渲染成 accent 色 TextRun");
    assert!(marker_run.underline.is_none(), "标记不下划线（不冒充链接）");
}

/// P4-2 回归修复：双击注记标记 `[n]` **不**双向跳转（用户报告「某些段落双击 → 闪烁
/// 变动/文字掉到其他行」，EPUB 尾部 90 条注记尤其严重）。根因：P4-2 在 `on_mouse_down`
/// 无条件 `navigate_marker`，双击首击即 `set_scroll_target` 跳对端 + 瞬态加亮。修复：
/// 按下只登记「单击确认」待跳转（[`NOTE_NAV_CONFIRM_MS`] 窗口），第二击作废 → 双击
/// 全程不跳转、不闪；单击（确认窗口到点）仍正常跳转对端。
#[gpui::test]
fn wy_note_double_click_does_not_navigate(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text(
                "正文引用[^1]继续\n\n[!1] 这是尾部注释正文的第一行文字，长到足以验证双击行为不跳转\n",
            )
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    ed.update(vcx, |e, _| e.set_editable(false));
    ed.update(vcx, |e, _| e.set_epub(true));
    let width = vcx.update(|window, _| f32::from(window.viewport_size().width).max(1.));
    let frame = vcx.update(|window, cx| {
        let f = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
        std::rc::Rc::new(f)
    });
    // 定义行 `[!1]`（src_line 2）的 `[1]` 标记字形几何中心 = 单击/双击命中点。
    let def_row = frame
        .rows
        .iter()
        .find(|r| r.src_line == 2)
        .expect("定义行");
    let dmk = def_row
        .extra
        .note_markers
        .iter()
        .find(|m| m.n == 1 && m.is_def)
        .expect("定义标记 `[1]`");
    let w0 = &def_row.wrapped[0];
    let p0 = w0.position_for_index(dmk.disp.start, def_row.lh).unwrap();
    let p1 = w0.position_for_index(dmk.disp.end, def_row.lh).unwrap();
    assert_eq!(f32::from(p0.y), f32::from(p1.y), "行首 `[1]` 不应折行");
    let ox = f32::from(frame.padding) + f32::from(def_row.x_off);
    let center = point(
        px(ox + (f32::from(p0.x) + f32::from(p1.x)) / 2.0),
        px(f32::from(def_row.top) + f32::from(p0.y) + f32::from(def_row.lh) / 2.0),
    );
    let down = |click_count: usize| MouseDownEvent {
        button: MouseButton::Left,
        position: center,
        modifiers: Modifiers::default(),
        click_count,
        first_mouse: false,
    };
    let up = MouseUpEvent {
        button: MouseButton::Left,
        position: center,
        modifiers: Modifiers::default(),
        click_count: 1,
    };
    // 双击序列 down(1) up(1) down(2) up(2)：全程不得设置跳转目标/加亮。
    vcx.update(|window, cx| {
        ed.update(cx, |e, cx| {
            *e.frame.borrow_mut() = Some(frame.clone());
            e.on_mouse_down(&down(1), window, cx);
            e.on_mouse_up(&up, cx);
            e.on_mouse_down(&down(2), window, cx);
            e.on_mouse_up(&up, cx);
        });
    });
    assert_eq!(
        ed.read_with(vcx, |e, _| e.scroll_target_line()),
        None,
        "双击不得设置滚动目标（否则整页位移）"
    );
    assert_eq!(
        ed.read_with(vcx, |e, _| e.marker_hl_test()),
        None,
        "双击不得设置跳转加亮（否则闪烁）"
    );
    // 推进确认窗口 + 排空后台：双击首击登记的那枚「单击确认」定时器必须已被第二击作废，
    // 不得在窗口到点后补发跳转（否则仍会整页位移）。
    let drain = |cx: &TestAppContext| {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(
                config::timing::NOTE_NAV_CONFIRM_MS + 50,
            ));
        cx.run_until_parked();
    };
    let _ = vcx;
    drain(cx);
    {
        let win = cx.windows()[0];
        let mut vcx2 = VisualTestContext::from_window(win, cx);
        assert_eq!(
            ed.update(&mut vcx2, |e, _| e.marker_hl_test()),
            None,
            "双击全程不得触发跳转加亮（定时器须被第二击作废）"
        );
        assert_eq!(
            ed.update(&mut vcx2, |e, _| e.scroll_target_line()),
            None,
            "双击全程不得设置滚动目标"
        );
    }
    // 单击（唯一一次按下、确认窗口到点后）仍应正常跳转到文内引用行 0，并加亮文内标记。
    let mut vcx3 = VisualTestContext::from_window(cx.windows()[0], cx);
    vcx3.update(|window, cx| {
        ed.update(cx, |e, cx| {
            *e.frame.borrow_mut() = Some(frame.clone());
            e.on_mouse_down(&down(1), window, cx);
            e.on_mouse_up(&up, cx);
        });
    });
    assert_eq!(
        ed.read_with(&vcx3, |e, _| e.marker_hl_test()),
        None,
        "单击按下瞬间不得跳转（确认窗口内等待第二击判定）"
    );
    let _ = vcx3;
    drain(cx);
    let mhl = {
        let win = cx.windows()[0];
        let mut vcx4 = VisualTestContext::from_window(win, cx);
        ed.update(&mut vcx4, |e, _| e.marker_hl_test())
    };
    assert_eq!(mhl, Some((0, 1, false)), "单击确认窗口到点后应跳转并加亮文内引用标记");
    // （scroll_target 由宿主 prepaint 消费：本测试未注入 host_scroll，`advance_pending_scroll`
    //  会因无宿主把目标清掉——跳转是否发生以持续到 2.2s 的 `marker_hl` 为准。）
}

/// P4-2：注记标记 `[^n]`/`[!n]` 迁入 WYSIWYG——shape 后 `extra.note_markers` 携带
/// 显示区间（`[1]` 3 字节），`note_marker_hit_at` 窗口坐标命中标记、行外不命中。
#[gpui::test]
fn wy_note_markers_populate_shape_and_hit(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("正文引用[^1]继续\n\n[!1] 注记定义\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    ed.update(vcx, |e, _| e.set_editable(false));
    ed.update(vcx, |e, _| e.set_epub(true));
    let width = vcx.update(|window, _| f32::from(window.viewport_size().width).max(1.));
    let frame = vcx.update(|window, cx| {
        let f = build_frame(ed.read(cx), window, px(width), (0., 999_999.));
        std::rc::Rc::new(f)
    });
    let ref_row = frame.rows.iter().find(|r| r.src_line == 0).expect("文内引用行");
    let mk = ref_row
        .extra
        .note_markers
        .iter()
        .find(|m| m.n == 1 && !m.is_def)
        .expect("文内引用 `[^1]` 应入 extra.note_markers");
    assert_eq!(mk.disp.len(), 3, "标记显示区间长度应 = `[1]` 的 3 字节");
    let def_row = frame.rows.iter().find(|r| r.src_line == 2).expect("定义行");
    let dmk = def_row
        .extra
        .note_markers
        .iter()
        .find(|m| m.n == 1 && m.is_def)
        .expect("定义 `[!1]` 应入 extra.note_markers");
    assert_eq!(dmk.disp.len(), 3, "定义标记显示区间 = `[1]` 3 字节");
    // 命中：取标记字形几何中心（内容列局部 = 窗口坐标，last_bounds 未布局时原点 0）。
    let w0 = &ref_row.wrapped[0];
    let p0 = w0.position_for_index(mk.disp.start, ref_row.lh).unwrap();
    let p1 = w0.position_for_index(mk.disp.end, ref_row.lh).unwrap();
    assert_eq!(f32::from(p0.y), f32::from(p1.y), "单行标记不应折行");
    let ox = f32::from(frame.padding) + f32::from(ref_row.x_off);
    let center = point(
        px(ox + (f32::from(p0.x) + f32::from(p1.x)) / 2.0),
        px(f32::from(ref_row.top) + f32::from(p0.y) + f32::from(ref_row.lh) / 2.0),
    );
    let hit = vcx.update(|_w, cx| note_marker_hit_at(ed.read(cx), &frame, center));
    let hit = hit.expect("中心点应命中文内引用标记");
    assert_eq!((hit.src_line, hit.n, hit.is_def), (0, 1, false));
    assert_ne!(f32::from(hit.bounds.size.width), 0., "命中矩形应有宽度");
    // 行首左上角（不落在标记上）不应命中。
    let miss = vcx.update(|_w, cx| {
        note_marker_hit_at(ed.read(cx), &frame, point(px(f32::from(frame.padding)), px(f32::from(ref_row.top) + 2.0)))
    });
    assert!(miss.is_none(), "标记外点不应命中");
}

/// P4-2：引文跳转索引（源码行语义）+ 悬浮卡正文路由（EPUB 注记表优先 / details 回退）+
/// `navigate_marker` 双向跳转（引用 ↺ 定义）设置对端 `set_scroll_target` 与加亮态。
#[gpui::test]
fn wy_notes_citation_index_and_navigation(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| {
            WyEditor::new(window, cx).with_text("正文引用[^1]继续\n\n[!1] 注记定义\n")
        });
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    ed.update(vcx, |e, _| e.set_editable(false));
    ed.update(vcx, |e, _| e.set_epub(true));
    // 引文索引：refs[1] = 文内引用行 0；defs[1] = 定义行 2；details[1] = 定义正文。
    let cit = ed.read_with(vcx, |e, _| e.citation_index());
    assert_eq!(cit.refs.get(&1).map(|(l, _)| *l), Some(0));
    assert_eq!(cit.defs.get(&1).map(|(l, _)| *l), Some(2));
    let det = cit.details.get(&1).cloned().unwrap_or_default();
    assert!(det.as_ref().contains("注记定义"));
    // 未注入 EPUB 注记表时，正文回退 details。
    let t = ed.read_with(vcx, |e, _| e.note_text_for(1));
    assert_eq!(t.as_deref(), Some("注记定义"), "缺注记表时应回退 details");
    // 注入 EPUB 注记表 → 正文优先。
    let mut notes = std::collections::BTreeMap::new();
    notes.insert(1usize, "这是注释正文内容".to_string());
    ed.update(vcx, |e, _| e.set_notes(notes));
    let t2 = ed.read_with(vcx, |e, _| e.note_text_for(1));
    assert_eq!(t2.as_deref(), Some("这是注释正文内容"), "注记表优先");
    // 文内引用 `[^1]` 点击 → 跳到定义行 2 并加亮定义标记（scroll_target 在下一帧
    // prepaint 被消费，故在同一 update 内读取）。
    let hit = NoteMarkerHit {
        src_line: 0,
        n: 1,
        is_def: false,
        bounds: Bounds::new(point(px(0.), px(0.)), size(px(10.), px(10.))),
    };
    let (stl, mhl) = ed.update(vcx, |e, cx| {
        e.navigate_marker(&hit, cx);
        (e.scroll_target_line(), e.marker_hl_test())
    });
    assert_eq!(stl, Some(2), "引用 → 定义：滚动目标应为定义行");
    assert_eq!(mhl, Some((2, 1, true)), "引用 → 定义：应加亮定义标记 (行2, n=1, is_def)");

    // 定义 `[!1]` 点击 → 跳回文内引用行 0 并加亮引用标记。
    let hit2 = NoteMarkerHit {
        src_line: 2,
        n: 1,
        is_def: true,
        bounds: Bounds::new(point(px(0.), px(0.)), size(px(10.), px(10.))),
    };
    let (stl2, mhl2) = ed.update(vcx, |e, cx| {
        e.navigate_marker(&hit2, cx);
        (e.scroll_target_line(), e.marker_hl_test())
    });
    assert_eq!(stl2, Some(0), "定义 → 引用：滚动目标应为文内引用行");
    assert_eq!(mhl2, Some((0, 1, false)), "定义 → 引用：应加亮引用标记 (行0, n=1, is_def=false)");
}

/// 注记跳转（远距）的重钉收敛回归：注入宿主滚动句柄后，`set_scroll_target` → 首帧
/// `scroll_to_line_impl` 写远端偏移；**复用帧**应走「收敛/修正」路径而非被误判成
/// 「用户接管」提前放弃——直接锁死 P4-2 注记跳转「复用态收敛判据」的同号比较
/// （`cur` 是正的宿主内容偏移，`target` 是负的句柄偏移；旧实现 `cur - target` 对远距
/// 跳转是「同向加倍」、|diff| 恒超接管阈值 → 每帧都当用户滚走而弃钉）。
#[gpui::test]
fn wy_far_note_jump_repin_converges_not_takeover(cx: &mut TestAppContext) {
    cx.update(crate::test_support::bind_local_keymaps);
    // 大文档：顶部正文多条 + 尾部一条注记定义（目标块在远端，模拟 EPUB 尾部注释章）。
    let mut text = String::new();
    for i in 0..800 {
        text.push_str(&format!(
            "正文第 {i} 段 alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau\n\n"
        ));
    }
    text.push_str("[!1] 尾部注记定义正文。\n");
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    ed.update(vcx, |e, _| {
        e.set_editable(false);
        e.set_epub(true);
    });
    let vhand = gpui::ScrollHandle::new();
    ed.update(vcx, |e, _| e.set_host_scroll(Some(vhand.clone()), 0.0));
    // 目标源码行 = `[!1]` 定义所在行（尾部）。
    let def_src_line = {
        let t = ed.read_with(vcx, |e, _| e.text());
        let at = t.as_ref().find("[!1]").expect("尾部应有定义标记");
        t.as_ref()[..at].bytes().filter(|&b| b == b'\n').count()
    };
    assert!(def_src_line >= 700, "定义应在远端：{def_src_line}");
    // —— 首帧（远端重建 + 写宿主偏移）——
    ed.update(vcx, |e, _| e.set_scroll_target(def_src_line));
    vcx.update(|window, cx: &mut gpui::App| {
        ed.update(cx, |e, _| {
            e.advance_pending_scroll(window);
        });
    });
    let off_write = vhand.offset().y.as_f32();
    assert!(off_write < -8_000., "远距跳转首帧应把宿主偏移写到远端：{off_write:.0}");
    let target_line = ed.read_with(vcx, |e, _| e.scroll_target_line());
    assert_eq!(target_line, Some(def_src_line), "首帧仍在重钉（目标未清）");
    // —— 复用帧 1：宿主持有目标但又漂移 3px（< 接管阈值 8px）——
    // 真实容器 set_offset 后 offset() 即返回所写值；这里模拟「已持有 + 3px 微小漂移」。
    vhand.set_offset(gpui::point(gpui::px(0.), gpui::px(off_write + 3.0)));
    vcx.update(|window, cx: &mut gpui::App| {
        ed.update(cx, |e, _| {
            e.advance_pending_scroll(window);
        });
    });
    let off_fix = vhand.offset().y.as_f32();
    let still_active = ed.read_with(vcx, |e, _| e.scroll_target_line()).is_some();
    assert!(
        still_active,
        "复用帧 3px 漂移（在接管阈值内）应保持重钉目标而非误判用户接管——旧实现 `cur - target` \
         同向加倍把远端跳转一律判成接管、提前放弃"
    );
    assert!(
        (off_fix - off_write).abs() < 0.75,
        "3px 漂移应被重写回目标：写 {off_write:.2}，实测 {off_fix:.2}"
    );
    // —— 复用帧 2：无漂移 → 按 `<1.5` 收敛清除目标（真正走收敛分支）——
    vcx.update(|window, cx: &mut gpui::App| {
        ed.update(cx, |e, _| {
            e.advance_pending_scroll(window);
        });
    });
    let final_off = vhand.offset().y.as_f32();
    assert!(
        ed.read_with(vcx, |e, _| e.scroll_target_line()).is_none(),
        "无漂移复用帧应按收敛清目标"
    );
    assert!(
        (final_off - off_write).abs() < 0.75,
        "收敛后宿主偏移应仍停在目标：{final_off:.2} vs {off_write:.2}"
    );
}
