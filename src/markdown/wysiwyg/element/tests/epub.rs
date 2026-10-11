use crate::markdown::ir::IrKind;
use std::collections::HashMap;
use std::sync::Arc;

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
