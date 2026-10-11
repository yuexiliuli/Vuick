use crate::markdown::ir::IrKind;
use std::collections::HashMap;
use std::sync::Arc;

use super::{note_marker_hit_at, NoteMarkerHit};

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
    cx.update(crate::actions::bind_local_keymaps);
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
    cx.update(crate::actions::bind_local_keymaps);
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
    cx.update(crate::actions::bind_local_keymaps);
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
    cx.update(crate::actions::bind_local_keymaps);
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
    cx.update(crate::actions::bind_local_keymaps);
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

/// P4-2：注记标记 `[^n]`/`[!n]` 迁入 WYSIWYG——shape 后 `extra.note_markers` 携带
/// 显示区间（`[1]` 3 字节），`note_marker_hit_at` 窗口坐标命中标记、行外不命中。
#[gpui::test]
fn wy_note_markers_populate_shape_and_hit(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
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
    cx.update(crate::actions::bind_local_keymaps);
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
