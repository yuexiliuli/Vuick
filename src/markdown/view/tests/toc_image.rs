/// EPUB 目录数据：标题行应进入 toc；scroll_to_row 定位不 panic（目录跳转由侧边栏提供）。
#[gpui::test]
fn toc_builds_and_scrolls(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let md_src = "# 第一章\n开头\n\n## 第一节\n内容\n\n# 第二章\n结尾\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(md_src, cx));
    });
    cx.simulate_resize(size(px(400.), px(600.)));
    settle(cx);

    let toc = cx.update(|_, cx| host.read(cx).md.read(cx).toc_entries());
    assert_eq!(toc.len(), 3, "应有 3 个标题进入目录，实际={toc:?}");
    let (row0, lvl0, _) = &toc[0];
    assert_eq!(*row0, 0usize, "首个目录项应是第 0 行（# 第一章）");
    assert_eq!(*lvl0, 1u8, "第一章是 h1");
    assert!(toc[0].2.contains("第一章"));

    // 跳转到一个中段标题（不应 panic），并验证目标行被滚动到视口顶部附近
    let target = toc[1].0;
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, _cx| md.scroll_to_row(target));
    });
    settle(cx);
    let target_bounds = cx.update(|_, cx| {
        host.read(cx).md.read(cx).list_state.bounds_for_item(target)
    });
    let tb = target_bounds.expect("跳转后目标行应有 bounds");
    assert!(
        tb.origin.y >= px(0.) && tb.origin.y < px(200.),
        "跳转到中段标题后目标行应滚动到视口顶部附近（origin.y={:?}）",
        tb.origin.y
    );
}

/// 设置了 `set_base_dir` 后，相对路径图片 src 应解析为绝对路径；
/// URL / Windows 盘符绝对路径保持原样；未设置时保持原样。
#[gpui::test]
fn base_dir_resolves_relative_image_srcs(cx: &mut TestAppContext) {
    use std::path::PathBuf;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;

    let manifest = env!("CARGO_MANIFEST_DIR").replace('/', "\\");
    let abs = format!(r"{manifest}\fixtures\img测试.png");
    // 相对名 + 盘符绝对路径 + URL：相对应基于 base_dir 拼接，绝对保持原样，URL 保持原样。
    let md_src = format!("![1](img测试.png)\n\n![1]({abs})\n\n![1](https://example.com/a.png)\n");
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_base_dir(Some(PathBuf::from(format!(r"{manifest}\fixtures"))));
            md.set_source(&md_src, cx);
        });
    });
    cx.simulate_resize(size(px(400.), px(600.)));
    settle(cx);

    let srcs = cx.update(|_, cx| {
        host.read(cx)
            .md
            .read(cx)
            .rows
            .iter()
            .filter(|r| r.docrow.kind == IrKind::Image)
            .map(|r| Some(r.image_src().to_string()))
            .collect::<Vec<_>>()
    });
    assert_eq!(srcs.len(), 3, "应解析出 3 张图片行，实际={srcs:?}");
    assert_eq!(
        srcs[0].as_deref(),
        Some(abs.as_str()),
        "相对路径应基于 base_dir 解析为绝对路径，实际={:?}",
        srcs[0]
    );
    assert_eq!(
        srcs[1].as_deref(),
        Some(abs.as_str()),
        "盘符绝对路径应保持原样，实际={:?}",
        srcs[1]
    );
    assert_eq!(
        srcs[2].as_deref(),
        Some("https://example.com/a.png"),
        "URL 应保持原样，实际={:?}",
        srcs[2]
    );

    // 本地文件应读取固有尺寸（fixtures/img测试.png = 72x72 JPEG，按内容 magic 探测）
    let dims = cx.update(|_, cx| {
        host.read(cx)
            .md
            .read(cx)
            .rows
            .iter()
            .filter(|r| r.docrow.kind == IrKind::Image)
            .map(|r| (r.img_w, r.img_h))
            .collect::<Vec<_>>()
    });
    assert_eq!(dims[0], (Some(580), Some(773)), "img测试.png 尺寸应 580x773，实际={:?}", dims[0]);
    assert_eq!(dims[1], (Some(580), Some(773)), "img测试.png 尺寸应 580x773，实际={:?}", dims[1]);
    assert_eq!(dims[2], (None, None), "URL 无本地尺寸，实际={:?}", dims[2]);
}

/// 实际渲染的图片行高度应按固有比例随可用宽度变化（aspect-ratio 驱动），
/// 缩小视口高度也随之缩小——不出现固定高度的大片留白。
#[gpui::test]
fn image_rows_height_follows_width(cx: &mut TestAppContext) {
    use std::path::PathBuf;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;

    let manifest = env!("CARGO_MANIFEST_DIR").replace('/', "\\");
    let wabsp = format!(r"{manifest}\fixtures\img测试.png");
    let md_src = format!("# 标题\n\n![w]({wabsp})\n\n![s]({wabsp})\n");
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_base_dir(Some(PathBuf::from(format!(r"{manifest}\fixtures"))));
            md.set_source(&md_src, cx);
        });
    });

    let measure = |cx: &mut VisualTestContext| -> (f32, f32) {
        let (wh, dh) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            let wi = md
                .rows
                .iter()
                .position(|r| r.docrow.kind == IrKind::Image && r.docrow.text == "w")
                .expect("应有 wuling 图片行");
            let di = md
                .rows
                .iter()
                .position(|r| r.docrow.kind == IrKind::Image && r.docrow.text == "s")
                .expect("应有 dify 图片行");
            let wb = md.list_state.bounds_for_item(wi).expect("wuling 行应有 bounds");
            let db = md.list_state.bounds_for_item(di).expect("dify 行应有 bounds");
            (wb.size.height.as_f32(), db.size.height.as_f32())
        });
        (wh, dh)
    };

    // 宽视口 500px：72x72 图按比例放大占满可用宽；行高随宽度变化。
    cx.simulate_resize(size(px(500.), px(800.)));
    settle(cx);
    let (w_wide, d_wide) = measure(cx);

    // 窄视口 260px：行高同比缩小（aspect-ratio 驱动）。
    cx.simulate_resize(size(px(260.), px(800.)));
    settle(cx);
    let (w_small, d_small) = measure(cx);

    assert!(
        w_small < w_wide - 50.0,
        "缩小视口后图片行高度应明显变小：wide={w_wide:.0} small={w_small:.0}"
    );
    assert!(
        d_small < d_wide - 50.0,
        "第二张图片行高同样应随宽度缩小：wide={d_wide:.0} small={d_small:.0}"
    );

    // 图片盒水平居中：md-img-box 左缘 ≈ (视口宽 - 盒宽)/2
    cx.simulate_resize(size(px(500.), px(800.)));
    settle(cx);
    let bo = cx
        .debug_bounds("md-img-box")
        .expect("图片盒 md-img-box 应存在");
    let prev = cx
        .debug_bounds("md-preview")
        .expect("预览根 md-preview 应存在");
    let view_w = prev.size.width.as_f32();
    let expect_x = (view_w - bo.size.width.as_f32()) / 2.0;
    assert!(
        (bo.origin.x.as_f32() - expect_x).abs() < 2.0,
        "图片盒应水平居中：盒宽={:.0}，期望左缘={expect_x:.0}，实际={:?}",
        bo.size.width.as_f32(),
        bo.origin
    );

    // 宽视口（预览 > 600）：盒宽应被 IMAGE_MAX_W 封顶并仍然居中
    cx.simulate_resize(size(px(900.), px(800.)));
    settle(cx);
    // 幂等再 resize 一次：debug_bounds 可能滞后一帧（前一次操作也是 resize 时更易踩中）。
    cx.simulate_resize(size(px(900.), px(800.)));
    settle(cx);
    let bo2 = cx
        .debug_bounds("md-img-box")
        .expect("图片盒 md-img-box 应存在(宽视口)");
    let prev2 = cx
        .debug_bounds("md-preview")
        .expect("预览根 md-preview 应存在(宽视口)");
    let view_w2 = prev2.size.width.as_f32();
    assert!(
        (bo2.size.width.as_f32() - IMAGE_MAX_W).abs() < 2.0,
        "宽视口下图片盒宽应封顶 {IMAGE_MAX_W}，实际={:.0}",
        bo2.size.width.as_f32()
    );
    let expect_x2 = (view_w2 - IMAGE_MAX_W) / 2.0;
    assert!(
        (bo2.origin.x.as_f32() - expect_x2).abs() < 2.0,
        "宽视口下图片盒仍应水平居中：期望左缘={expect_x2:.0}，实际={:?}",
        bo2.origin
    );
}

/// EPUB/PDF 内存图片端到端：set_memory_images 注册虚拟 src 后，
/// set_source 应把字节挂到图片行（img_data）并解析出固有尺寸，渲染出图片盒。
#[gpui::test]
fn epub_in_memory_images_render(cx: &mut TestAppContext) {
    let doc = crate::epub::parse_epub_file(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/xy.epub"))
        .expect("解析 xy.epub 应成功");
    let vsrcs: Vec<String> = doc.images.keys().cloned().collect();
    assert!(!vsrcs.is_empty(), "xy.epub 应提取到内存图片");
    // 用读取到的真实图片字节构造虚拟 src（保证注册表有对应项）
    let first = doc
        .images
        .iter()
        .find(|(_, img)| img.format == gpui::ImageFormat::Jpeg)
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| "epubimg://1.jpeg".to_string());
    let md_src = format!("![图]({first})");

    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_memory_images(doc.images.clone());
            md.set_source(&md_src, cx);
        });
    });
    cx.simulate_resize(size(px(500.), px(700.)));
    settle(cx);

    // 图片行应挂上内存字节与尺寸
    let (has_data, dims_ok) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        let img_rows: Vec<_> = md.rows.iter().filter(|r| r.docrow.kind == IrKind::Image).collect();
        assert_eq!(img_rows.len(), 1, "应有 1 个图片行");
        let r = img_rows[0];
        (
            r.img_data.is_some(),
            r.img_w.is_some() && r.img_h.is_some(),
        )
    });
    assert!(has_data, "EPUB 内存图片应挂到图片行(img_data)");
    assert!(dims_ok, "内存图片应解析出固有尺寸");

    // 渲染出图片盒
    let box_ = cx
        .debug_bounds("md-img-box")
        .expect("内存图片应渲染出 md-img-box");
    assert!(box_.size.width > px(0.) && box_.size.height > px(0.), "图片盒应有尺寸，实际={box_:?}");
}

/// 回归：超宽代码块应能横向滚动、且整个代码块只有一个可拖动的滚动条，
/// 而不是被裁剪遮挡 / 每行一个滚动条。
/// 这在换行模式（wrap=true，默认）下尤其重要——整个围栏折叠成一个渲染行，
/// 内部一个横向滚动容器 + 底部单一 Always 滚动条，句柄跨帧稳定（set_offset 能真正滚动）。
#[gpui::test]
fn wide_code_line_makes_harea_overflow(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 一个多行长代码行组成的代码栅栏，行宽超过视口
    let long_line = "f".repeat(500);
    let md_src = format!("```rs\n{long_line}\n{long_line}\n{long_line}\n```\n");
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(&md_src, cx);
        });
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);

    let preview = cx
        .debug_bounds("md-preview")
        .expect("预览根 md-preview 应存在");
    let band = cx
        .debug_bounds("md-code-band")
        .expect("代码背景带 md-code-band 应存在");
    assert!(
        band.size.width > preview.size.width,
        "超宽代码背景带应宽于预览视口（从而可横向滚动而非被裁剪），band={band:?} preview={preview:?}"
    );
    // 代码块滚动容器应等于预览视口宽（自身不撑大，内容在内部横向滚动）
    let hscroll = cx
        .debug_bounds("md-code-hscroll")
        .expect("代码块滚动容器 md-code-hscroll 应存在");
    assert!(
        hscroll.size.width <= preview.size.width + px(1.),
        "代码滚动容器应等于预览视口宽，实际 hscroll={hscroll:?} preview={preview:?}"
    );
    // 滚动条叠层真实渲染（非零尺寸）
    let sbar = cx
        .debug_bounds("md-code-scrollbar")
        .expect("代码块横向滚动条 md-code-scrollbar 应存在");
    assert!(
        sbar.size.width > px(0.) && sbar.size.height > px(0.),
        "横向滚动条不应塌缩成 0，实际={sbar:?}"
    );

    // 整个围栏折叠成一个 Code 渲染行：item 高度 = 5 * code_line
    //（3 行代码 + 上下各 1 行空白带，多行紧贴相连成一大块）
    let p = cx.update(|_w, cx| StyleParams::from_theme_with_font(cx, None));
    let rows = parse_to_rows(&md_src);
    let code_ix: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind == RowKind::Code)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(code_ix.len(), 1, "整个围栏应折叠成一个 Code 渲染行");
    let b = cx
        .update(|_, cx| host.read(cx).md.read(cx).list_state.bounds_for_item(code_ix[0]))
        .expect("代码行 item 应有 bounds");
    assert!(
        (b.size.height.as_f32() - p.code_line.as_f32() * 5.0).abs() < 1.5,
        "代码块行高应等于 5*code_line（3 行代码 + 上下空白带），实际={:?}",
        b.size.height
    );

    // 关键：代码块滚动句柄跨帧稳定，set_offset 能真正移动代码带（即滚动条可拖动生效）
    let origin_x = band.origin.x.as_f32();
    // 代码可横向溢出时，句柄 max_offset.x 应 > 0 —— on_scroll_wheel 据此 stop_propagation，
    // 保证滚轮只横向滚代码、页面不跟着纵向滚动。
    let (code_max_x,) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        let h = md.code_scroll.get(&code_ix[0]).expect("代码块应注册滚动句柄");
        (h.max_offset().x,)
    });
    assert!(code_max_x > px(0.), "超宽代码块句柄 max_offset.x 应 > 0，实际={code_max_x:?}");
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let h = md
            .read(cx)
            .code_scroll
            .get(&code_ix[0])
            .expect("代码块应注册滚动句柄")
            .clone();
        h.set_offset(gpui::point(px(-200.), px(0.)));
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);
    let box2 = cx
        .debug_bounds("md-code-band")
        .expect("滚动后 md-code-band 应仍存在");
    assert!(
        box2.origin.x.as_f32() < origin_x - 100.0,
        "设置代码滚动句柄后代码带应向左滚动，origin.x: {origin_x:.0} -> {:?}",
        box2.origin.x
    );

    // 非换行语义已废弃：页面恒换行 → 页面级 `h_handle` 无量程，代码仍在**块内**横向滚动。
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_wrap(false, cx));
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);
    let (page_max_x, code_max_x) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        let h = md
            .code_scroll
            .get(&code_ix[0])
            .expect("代码块应注册滚动句柄");
        (md.h_handle.max_offset().x, h.max_offset().x)
    });
    assert_eq!(
        page_max_x, px(0.),
        "set_wrap(false) 后页面级横向滚动应始终为 0（块内滚动），实际={page_max_x:?}"
    );
    assert!(
        code_max_x > px(0.),
        "宽代码块在块内仍应可横向滚动，max_offset.x={code_max_x:?}"
    );
}

/// 优化回归：`code_scroll`/`code_hl`/`table_scroll` 必须按 [`Rc`] 跨帧**共享**——
/// 反复渲染后 self 里的 Rc 指针不变（渲染帧只克隆 Rc 本身、不整张克隆 HashMap），
/// 且强引用计数不随渲染增长。指针变化说明渲染路径在重建/深拷贝地图，即为回归。
#[gpui::test]
fn md_preview_block_extras_are_shared_rc_across_renders(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let src = "```rust\nlet a = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(src, cx));
    });
    cx.simulate_resize(size(px(500.), px(500.)));
    settle(cx);
    let (ptr0, count0, code_blocks) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        (
            std::rc::Rc::as_ptr(&md.code_scroll) as usize,
            std::rc::Rc::strong_count(&md.code_scroll),
            md.code_scroll.len(),
        )
    });
    assert_eq!(code_blocks, 1, "应注册 1 个代码块滚动句柄");
    assert!(count0 >= 1, "滚动句柄应已建成共享块");

    // 反复渲染多帧：指针必须不变、强引用计数不得增长
    for _ in 0..10 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }
    cx.run_until_parked();
    let (ptr1, count1) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        (
            std::rc::Rc::as_ptr(&md.code_scroll) as usize,
            std::rc::Rc::strong_count(&md.code_scroll),
        )
    });
    assert_eq!(ptr0, ptr1, "反复渲染后 code_scroll 应为同一 Rc（未整张重建/深拷贝）");
    assert_eq!(count0, count1, "反复渲染后强引用计数不应增长（无每帧克隆泄漏）");
}

/// 回归：编辑大文档触发预览重同步（`set_source_with_index` **窗口级懒解析**路径）
/// 重建 ListState 时，必须恢复到更新前的滚动位置——否则预览每次编辑都跳回开头。
#[gpui::test]
fn large_preview_keeps_scroll_across_source_replacement(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 构造超过 WINDOWED_MIN_BLOCKS 的文档 → 窗口级懒解析
    let block = "# 章节标题\n\n正文段落内容 alpha\n- 列表项\n\n```rust\nlet x = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
    let mut src = String::new();
    for _ in 0..700 {
        src.push_str(block);
    }
    let index = std::rc::Rc::new(crate::markdown::scan_blocks(&src));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(PreviewSrc::from(SharedString::from(src.clone())), index, cx)
        });
    });
    cx.simulate_resize(size(px(500.), px(500.)));
    settle(cx);
    let n = cx.update(|_, cx| host.read(cx).md.read(cx).item_count());
    assert!(
        n > crate::config::limits::WINDOWED_MIN_BLOCKS,
        "应启用窗口级懒解析，块数 {n}"
    );
    // 先滚动到中部
    let mid = n / 2;
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.scroll_to_row(mid);
            cx.notify();
        });
    });
    for _ in 0..8 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }
    cx.run_until_parked();
    let before =
        cx.update(|_, cx| host.read(cx).md.read(cx).list_state.logical_scroll_top().item_ix);
    assert!(before > 0, "滚动应生效（before={before}，目标 mid={mid}）");

    // 编辑后重设源（内容多一行 → 块数变化 → 走窗口模式 ListState 重建路径）
    let src2 = format!("新增内容行\n{src}");
    let index2 = std::rc::Rc::new(crate::markdown::scan_blocks(&src2));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(PreviewSrc::from(SharedString::from(src2)), index2, cx)
        });
    });
    cx.simulate_resize(size(px(500.), px(500.)));
    settle(cx);
    let after =
        cx.update(|_, cx| host.read(cx).md.read(cx).list_state.logical_scroll_top().item_ix);
    assert!(
        after >= before,
        "源更新后预览不应跳回开头：before={before} after={after}"
    );
}

