//! 视图层测试：行级虚拟化 / 横向滚动 / 表格 / 目录 / 图片 / 字号缩放 的回归与冒烟测试。

use super::*;
use super::dimensions::IMAGE_MAX_W;
use gpui::{AppContext, Context, Entity, Render, TestAppContext, VisualTestContext, size};

struct PreviewHost {
    md: Entity<MarkdownPreview>,
}
impl Render for PreviewHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.md.clone()
    }
}

/// 以 300x400 视口打开空预览，再把源码设为 500 个字符的超宽行。
/// 顺序：先 `set_source` 再 `resize` —— resize 触发一帧完整重绘，让布局/滚动条就绪。
fn setup_wide(cx: &mut TestAppContext) -> (Entity<PreviewHost>, &mut VisualTestContext) {
    cx.update(gpui_component::init);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let wide = "x".repeat(500);
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(&wide, cx));
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);
    (host, cx)
}

/// 让渲染收敛：`max_offset` / `bounds` 只在每帧布局时刷新，多送几帧确保
/// 滚动容器以最终视口尺寸完成布局。
fn settle(cx: &mut VisualTestContext) {
    for _ in 0..5 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }
    cx.run_until_parked();
}

/// 超宽内容应让横向滚动句柄产生正向 max_offset（可滚动范围正确）。
#[gpui::test]
fn wide_source_makes_harea_overflow(cx: &mut TestAppContext) {
    let (host, cx) = setup_wide(cx);

    let (max_x, view_w, content_w) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        (md.h_handle.max_offset().x, md.h_handle.bounds().size.width, md.content_w)
    });
    assert!(content_w > 300.0, "估算内容宽应超过视口，content_w={content_w:?}");
    assert!(
        max_x > px(0.),
        "横向应可滚动（max_offset.x={max_x:?}，视口宽={view_w:?}，内容宽={content_w:?}）"
    );

    // 设置横向偏移应生效（内容向左移，滚动条随之移动）
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.h_handle.set_offset(gpui::point(px(-120.), px(0.)));
            cx.notify();
        });
    });
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let off_x = cx.update(|_, cx| host.read(cx).md.read(cx).h_handle.offset().x);
    assert!(off_x < px(-100.), "横向偏移应生效，实际={off_x:?}");
}

/// 内容超宽时，横向滚动条叠层应铺满预览区（得到确定的布局视口），
/// 且 `content_w` 超过该视口 → gpui-component 的横条一定画出滑块
/// （组件内判据：content_size = max_offset + viewport > container_size）。
#[gpui::test]
fn hscrollbar_overlay_covers_preview_when_overflow(cx: &mut TestAppContext) {
    let (host, cx) = setup_wide(cx);

    let preview = cx
        .debug_bounds("md-preview")
        .expect("预览根 md-preview 应存在");
    let overlay = cx
        .debug_bounds("md-hscrollbar-overlay")
        .expect("横向滚动条叠层 md-hscrollbar-overlay 应存在");
    let harea = cx
        .debug_bounds("md-harea")
        .expect("横向滚动容器 md-harea 应存在");

    assert!(
        (overlay.size.width.as_f32() - preview.size.width.as_f32()).abs() < 1.0,
        "横向滚动条叠层应铺满预览区宽度，overlay={overlay:?} preview={preview:?}"
    );
    assert!(
        overlay.size.width > px(0.) && overlay.size.height > px(0.),
        "叠层不应塌缩成 0，overlay={overlay:?}"
    );
    assert_eq!(harea.size.width, overlay.size.width);

    let (content_w, max_x) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        (md.content_w, md.h_handle.max_offset().x)
    });
    assert!(
        content_w > overlay.size.width.as_f32(),
        "内容宽应超过滚动条视口宽，content_w={content_w:.0} overlay_w={}",
        overlay.size.width.as_f32()
    );
    assert!(max_x > px(0.), "横向应存在可滚动范围，max_offset.x={max_x:?}");
}

/// 内容不超宽时横向不可滚动（不产出多余横向滚动范围）。
#[gpui::test]
fn no_hscroll_when_content_fits(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 几行短内容（估算宽远小于 300 视口）→ 不发生横向溢出
    let short = "短文本\n第二行\n结束";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(&short, cx));
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);

    let (max_x, content_w) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        (md.h_handle.max_offset().x, md.content_w)
    });
    assert!(
        content_w < 300.0,
        "短内容估算宽应小于视口，content_w={content_w:.0}"
    );
    assert_eq!(max_x, px(0.), "短内容不应产生横向滚动范围，实际={max_x:?}");
}

/// 代码块的浅灰背景带与表格网格都应内缩（不贴左分隔栏边缘），与正文对齐。
/// 回归：背景必须画在内层元素上，外层仅用 px_3 留白（bg 会铺满含 padding 的整盒）。
#[gpui::test]
fn code_band_and_table_inset_from_left_edge(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 段落 → 代码栅栏 → 表格，保证三种元素都在同一份源码里
    let md_src = "开场段落\n\n```rust\nlet a = 1;\nlet b = 2;\n```\n\n| 型号 | 尺寸 |\n|---|---|\n| A | 10 |\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(md_src, cx));
    });
    cx.simulate_resize(size(px(400.), px(600.)));
    settle(cx);

    let band = cx
        .debug_bounds("md-code-band")
        .expect("代码背景带 md-code-band 应存在");
    let table = cx
        .debug_bounds("md-table")
        .expect("表格 md-table 应存在");
    // 与正文一致的 px_3 = 12px 内缩（容 1px 浮点误差）
    for (name, b) in [("代码背景带", band), ("表格", table)] {
        assert!(
            (b.origin.x - px(12.)).abs() < px(1.),
            "{name} 应从左边缘内缩约 12px，实际 origin.x={:?}",
            b.origin.x
        );
    }
    // 代码背景带应有宽高（不会塌缩成 0）
    assert!(
        band.size.width > px(0.) && band.size.height > px(0.),
        "代码背景带不应塌缩，实际={band:?}"
    );
}

/// GFM 表格应渲染成有实际宽高的网格，且其宽度计入横向滚动内容宽估算。
#[gpui::test]
fn renders_markdown_table_as_grid(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let md_src = "| 型号 | 尺寸 |\n|:---|---:|\n| A | 10 |\n| B | 20 |\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(md_src, cx));
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);

    let table = cx
        .debug_bounds("md-table")
        .expect("表格应渲染出 md-table 网格");
    assert!(table.size.width > px(0.), "表格应有宽度，实际={table:?}");
    assert!(table.size.height > px(0.), "表格应有高度（表头+数据行），实际={table:?}");

    let content_w = cx.update(|_, cx| host.read(cx).md.read(cx).content_w);
    assert!(
        content_w + 40.0 >= table.size.width.as_f32(),
        "表格宽度应计入横向滚动内容宽（content_w={content_w:.0}，表宽={:?}）",
        table.size.width
    );
}

/// 每个标题下的文本行（段落/列表/引述）都应获得相同的加高行距（不只是第一个标题下）。
/// 复现：多个标题各带若干段，逐个用 `bounds_for_item` 检查文本 item 高度。
#[gpui::test]
fn paragraph_gap_applies_after_every_heading(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 两个标题各带文本：开头段落 → 标题一 → 段落 → 标题二 → 列表项 → 引述
    let md_src = "# 标题一\n开头段落\n\n## 标题二\n第二个段落\n- 列表项\n> 引述\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(md_src, cx));
    });
    cx.simulate_resize(size(px(400.), px(600.)));
    settle(cx);

    let p = cx.update(|_w, cx| StyleParams::from_theme_with_font(cx, None));
    let rows = parse_to_rows(md_src);
    let expected = p.para_line.as_f32() + p.para_gap.as_f32();

    // 找出所有文本行（段落/列表/引述）在 rows 中的下标
    let mut checked = 0usize;
    for (ix, r) in rows.iter().enumerate() {
        if !matches!(
            r.kind,
            RowKind::Paragraph | RowKind::Bullet | RowKind::Quote
        ) {
            continue;
        }
        let b = cx
            .update(|_, cx| host.read(cx).md.read(cx).list_state.bounds_for_item(ix))
            .expect("文本行 item 应有 bounds");
        assert!(
            (b.size.height.as_f32() - expected).abs() < 1.0,
            "第 {ix} 行（{:?}）高度应为 {expected:.1}，实际={:?}",
            r.kind,
            b.size.height
        );
        checked += 1;
    }
    assert_eq!(checked, 4, "应检查到 4 个文本行（2 段+1 列表+1 引述）");
}

/// EPUB 阅读：Ctrl+滚轮缩放应更新正文字号覆盖（基数 = 主题默认）。
#[gpui::test]
fn font_zoom_updates_override(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let md_src = "# 标题\n正文段落内容，用于字号缩放验证。\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(md_src, cx));
    });
    cx.simulate_resize(size(px(400.), px(600.)));
    settle(cx);

    let default = cx.update(|_, cx| {
        StyleParams::from_theme_with_font(cx, None).para_size.as_f32()
    });
    // 初始无覆盖
    let none = cx.update(|_, cx| host.read(cx).md.read(cx).font_override);
    assert!(none.is_none(), "初始应无字号覆盖");

    // 模拟 Ctrl+滚轮向上（放大 1 档）
    cx.update(|window, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.bump_font_size(1.5, cx, default));
        window.refresh();
    });
    settle(cx);
    let after = cx.update(|_, cx| host.read(cx).md.read(cx).font_override);
    assert!(after.is_some(), "滚动后应产生字号覆盖");
    assert!(
        (after.unwrap() - (default + 1.5)).abs() < 0.001,
        "字号应增大 1.5，实际={after:?}"
    );
}

/// EPUB 目录：标题行应进入 toc；toggle_toc 可展开/收起；scroll_to_row 定位不 panic。
#[gpui::test]
fn toc_builds_and_scrolls(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
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

    let toc = cx.update(|_, cx| host.read(cx).md.read(cx).toc.clone());
    assert_eq!(toc.len(), 3, "应有 3 个标题进入目录，实际={toc:?}");
    let (row0, lvl0, _) = &toc[0];
    assert_eq!(*row0, 0usize, "首个目录项应是第 0 行（# 第一章）");
    assert_eq!(*lvl0, 1u8, "第一章是 h1");
    assert!(toc[0].2.contains("第一章"));

    // 目录组件渲染冒烟：右上角触发按钮应存在
    let btn = cx.debug_bounds("md-toc-toggle-wrap");
    assert!(btn.is_some(), "目录引导按钮应渲染（md-toc-toggle-wrap）");

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
    cx.update(gpui_component::init);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;

    let md_src = "![1](wuling.png)\n\n![1](C:\\Users\\c22500540\\Pictures\\dify.jpg)\n\n![1](https://example.com/a.png)\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_base_dir(Some(PathBuf::from(r"C:\Users\c22500540\Pictures")));
            md.set_source(md_src, cx);
        });
    });
    cx.simulate_resize(size(px(400.), px(600.)));
    settle(cx);

    let rows = cx.update(|_, cx| {
        host.read(cx)
            .md
            .read(cx)
            .rows
            .iter()
            .map(|r| r.kind.clone() == RowKind::Image)
            .collect::<Vec<_>>()
    });
    drop(rows); // 仅确保读取不 panic；src 从 parsed 断言

    // 直接从 parse 通路验证：resolve 逻辑在 set_source 内完成了 base 拼接
    let srcs = cx.update(|_, cx| {
        host.read(cx)
            .md
            .read(cx)
            .rows
            .iter()
            .filter(|r| r.kind == RowKind::Image)
            .map(|r| r.src.clone().map(|s| s.to_string()))
            .collect::<Vec<_>>()
    });
    assert_eq!(srcs.len(), 3, "应解析出 3 张图片行，实际={srcs:?}");
    assert_eq!(
        srcs[0].as_deref(),
        Some(r"C:\Users\c22500540\Pictures\wuling.png"),
        "相对路径应基于 base_dir 解析为绝对路径，实际={:?}",
        srcs[0]
    );
    assert_eq!(
        srcs[1].as_deref(),
        Some(r"C:\Users\c22500540\Pictures\dify.jpg"),
        "Windows 盘符绝对路径应保持原样，实际={:?}",
        srcs[1]
    );
    assert_eq!(
        srcs[2].as_deref(),
        Some("https://example.com/a.png"),
        "URL 应保持原样，实际={:?}",
        srcs[2]
    );

    // 本地文件应读取固有尺寸（wuling.png=565x724，dify.jpg=1632x492）
    let dims = cx.update(|_, cx| {
        host.read(cx)
            .md
            .read(cx)
            .rows
            .iter()
            .filter(|r| r.kind == RowKind::Image)
            .map(|r| (r.img_w, r.img_h))
            .collect::<Vec<_>>()
    });
    assert_eq!(dims[0], (Some(565), Some(724)), "wuling.png 尺寸期望 565x724，实际={:?}", dims[0]);
    assert_eq!(dims[1], (Some(1632), Some(492)), "dify.jpg 尺寸期望 1632x492，实际={:?}", dims[1]);
    assert_eq!(dims[2], (None, None), "URL 无本地尺寸，实际={:?}", dims[2]);
}

/// 实际渲染的图片行高度应按固有比例随可用宽度变化（aspect-ratio 驱动），
/// 缩小视口高度也随之缩小——不出现固定高度的大片留白。
#[gpui::test]
fn image_rows_height_follows_width(cx: &mut TestAppContext) {
    use std::path::PathBuf;
    cx.update(gpui_component::init);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;

    let wabsp = r"C:\Users\c22500540\Pictures\wuling.png";
    let md_src = format!("# 标题\n\n![w]({wabsp})\n\n![s](C:\\Users\\c22500540\\Pictures\\dify.jpg)\n");
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_base_dir(Some(PathBuf::from(r"C:\Users\c22500540\Pictures")));
            md.set_source(&md_src, cx);
        });
    });

    let measure = |cx: &mut VisualTestContext| -> (f32, f32) {
        let (wh, dh) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            let wi = md
                .rows
                .iter()
                .position(|r| r.kind == RowKind::Image && r.text == "w")
                .expect("应有 wuling 图片行");
            let di = md
                .rows
                .iter()
                .position(|r| r.kind == RowKind::Image && r.text == "s")
                .expect("应有 dify 图片行");
            let wb = md.list_state.bounds_for_item(wi).expect("wuling 行应有 bounds");
            let db = md.list_state.bounds_for_item(di).expect("dify 行应有 bounds");
            (wb.size.height.as_f32(), db.size.height.as_f32())
        });
        (wh, dh)
    };

    // 宽视口 500px：wuling 宽=min(500-24, 600)=476 → 高=476*724/565≈610，限高后按比例
    cx.simulate_resize(size(px(500.), px(800.)));
    settle(cx);
    let (w_wide, d_wide) = measure(cx);

    // 窄视口 260px：wuling 宽=260-24=236 → 高=236*724/565≈302（成比例缩小）
    cx.simulate_resize(size(px(260.), px(800.)));
    settle(cx);
    let (w_small, d_small) = measure(cx);

    assert!(
        w_small < w_wide - 50.0,
        "缩小视口后图片行高度应明显变小：wide={w_wide:.0} small={w_small:.0}"
    );
    assert!(
        d_small < d_wide - 50.0,
        "dify 行高同样应随宽度缩小：wide={d_wide:.0} small={d_small:.0}"
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
    let doc = crate::epub::parse_epub_file(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/xy.epub"))
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

    cx.update(gpui_component::init);
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
        let img_rows: Vec<_> = md.rows.iter().filter(|r| r.kind == RowKind::Image).collect();
        assert_eq!(img_rows.len(), 1, "应有 1 个图片行");
        let r = img_rows[0];
        (r.img_data.is_some(), r.img_w.is_some() && r.img_h.is_some())
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
    cx.update(gpui_component::init);
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

    // 整个围栏折叠成一个 Code 渲染行：item 高度 = 3 * code_line（多行紧贴相连成一大块）
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
        (b.size.height.as_f32() - p.code_line.as_f32() * 3.0).abs() < 1.5,
        "代码块行高应等于 3*code_line（连续块），实际={:?}",
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

    // 非换行模式：折叠成单行后宽度估算仍计入最宽代码行 → 产生横向滚动范围
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_wrap(false, cx));
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);
    let (max_x,) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        (md.read(cx).h_handle.max_offset().x,)
    });
    assert!(max_x > px(0.), "非换行模式也应横向可滚动，max_offset.x={max_x:?}");
}

/// 换行模式下，短代码块的背景应铺满预览视口宽（跟随窗口），而不是只到文本最右边缘，
/// 使各代码块视觉宽度统一；且不产生（无溢出的）横向滚动条。
#[gpui::test]
fn code_block_bg_spans_pane_width_in_wrap(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 很短的多行代码（远窄于视口）
    let md_src = "```rs\nlet a = 1;\nlet b = 2;\n```\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(md_src, cx);
        });
    });
    cx.simulate_resize(size(px(500.), px(400.)));
    settle(cx);

    let preview = cx
        .debug_bounds("md-preview")
        .expect("预览根 md-preview 应存在");
    let band = cx
        .debug_bounds("md-code-band")
        .expect("代码背景带 md-code-band 应存在");
    // 代码块背景铺满预览内容宽 = 视口宽 - 左右 px_3(24px)，各代码块宽度统一并跟随窗口
    let expect_w = preview.size.width.as_f32() - 24.0;
    assert!(
        (band.size.width.as_f32() - expect_w).abs() < 2.0,
        "短代码块背景应铺满预览内容宽({expect_w:.0})，band={band:?} preview={preview:?}"
    );
}


