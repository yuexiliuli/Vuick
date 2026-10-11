
use super::*;
use crate::config::markdown::IMAGE_MAX_W;
use gpui::{
    AppContext, Context, Entity, Modifiers, MouseButton, Render, TestAppContext,
    VisualTestContext, point, px, size,
};

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
    cx.update(crate::actions::bind_local_keymaps);
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

/// EPUB 轻量视口分页读数：非 EPUB → `None`；EPUB 小文档 → 页数 sane 且滚到底 =
/// 末页；窗口模式（> 窗口阈值块数）desired 口径滚到底 = 末页。
#[gpui::test]
fn epub_viewport_page_bounds(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    cx.simulate_resize(size(px(300.), px(200.)));
    settle(cx);

    // 非 EPUB → 无页码
    assert!(
        cx.update(|_, cx| host.read(cx).md.read(cx).viewport_page_bounds()).is_none(),
        "非 EPUB 模式不应有页码"
    );

    // EPUB 小文档：多段 → 多页；滚到底 = 末页。
    let mut doc = String::new();
    for i in 0..60 {
        doc.push_str(&format!("## 第{i}节\n\n正文段落{i}。\n\n"));
    }
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_epub(true, Default::default(), cx);
            md.set_source(&doc, cx);
        });
    });
    cx.simulate_resize(size(px(300.), px(200.)));
    settle(cx);
    let (p1, n) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.read(cx).viewport_page_bounds().expect("epub 应有页码")
    });
    assert_eq!(p1, 1, "初始应为第 1 页");
    assert!(n >= 2, "多段内容应分多页（总页数={n}）");
    // 量程健康：打开后（无任何拖拽/交互）滚动条量程应立即 ≈ 全文（不坍缩）——否则
    // 用户「无法一次拖到底」（需先拖一次 start_drag 才重铺）。小文档视口 200。
    let (maxoff, view, total_h) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let v = md.read(cx);
        (
            v.list_state.max_offset_for_scrollbar().y.as_f32(),
            v.list_state.viewport_bounds().size.height.as_f32(),
            v.total_h(),
        )
    });
    let sum_h = maxoff + view;
    assert!(
        sum_h >= total_h * 0.9,
        "打开即量程不坍缩（summary~={sum_h:.0} vs total={total_h:.0}）"
    );
    // 滚到底（公开跳转入口 scroll_to_row，小文档走 ListState reveal）→ 末页。
    let last = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.read(cx).item_count().saturating_sub(1)
    });
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, _| md.scroll_to_row(last));
    });
    for _ in 0..4 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }
    let (p2, n2) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.read(cx).viewport_page_bounds().expect("epub 应有页码")
    });
    assert!(
        p2 > p1 && p2 <= n2,
        "滚动后当前页应前进且合法（{p1} -> {p2}/{n2}）"
    );

    // 注：窗口模式（> WINDOWED_MIN_BLOCKS）的页码依赖宿主布局后的视口高/高度模型；
    // 单元环境的固定窗口 ListState 不测视口（viewport 高度为 0），此路径留真机/集成
    // 验证。小文档主路径（上图）已覆盖「初始页 / 滚动前进 / 页码合法」。
}

/// 回归：「EPUB 滚动条无法一次拖到底」的真机根因——真机字体/DPI 渲染后的列表实测
/// 总高可能**高于**启发式估算 `n×est_hint`（CJK 0.55×字号等近似偏乐观）。旧实现
/// 小文档的滚动条 `content_size` 用的是估算值 → thumb 拖到底只发 `-(n×hint−视口)`，
/// 被 `set_offset_from_scrollbar` 夹在 `n×hint−视口`，离真实底部差一段 → 拖不到底。
/// 修复：小文档量程跟随 ListState 自身（`max_offset + 视口`，与 gpui 内置同口径），
/// thumb 全程 == 可滚域，拖到底必到真实末尾。
#[gpui::test]
fn small_doc_scrollbar_follows_list_summary_when_taller_than_estimate(
    cx: &mut TestAppContext,
) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    cx.simulate_resize(size(px(300.), px(200.)));
    settle(cx);

    let mut doc = String::new();
    for i in 0..80 {
        doc.push_str(&format!("## 第{i}节\n\n正文段落{i}，内容内容内容。\n\n"));
    }
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_epub(true, Default::default(), cx);
            md.set_source(&doc, cx);
        });
    });
    cx.simulate_resize(size(px(300.), px(200.)));
    settle(cx);

    let (n, hint, ls, view) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let m = md.read(cx);
        (
            m.item_count(),
            m.est_hint,
            m.test_list_state(),
            m.list_state.viewport_bounds(),
        )
    });
    assert!(n > 0 && hint > 0.0, "小文档应有位齐全（n={n} hint={hint:.1}）");
    // 模拟真机：实测总高 = 1.5×估算（真实排版比启发式估算高）。
    let real_h = n as f32 * hint * 1.5;
    ls.reset_with_uniform_height(n, px(hint * 1.5));

    // 与 `vertical_scrollbar` 完全同构地构造句柄（content_height 仍传估算以复刻旧缺陷）。
    let handle = PreviewScrollHandle {
        list: ls.clone(),
        content_height: n as f32 * hint,
        est_hint: hint,
        n,
        windowed: false,
        desired: std::rc::Rc::new(std::cell::Cell::new(0.)),
    };
    // ① 量程必须跟随 ListState 实测（而非估算）——旧实现会断言失败。
    let content_h = handle.content_size().height.as_f32();
    let expect = view.size.height.as_f32() + ls.max_offset_for_scrollbar().y.as_f32();
    assert!(
        (content_h - expect).abs() < 1.0,
        "小文档量程应跟随 ListState 实测：content_h={content_h:.0} expect={expect:.0}（估算 n×hint={:.0}）",
        n as f32 * hint
    );
    // ② 拖到底（start_drag 冻结 → thumb 到底发 -(量程−视口)）必须到达真实末尾：
    // 视口顶部不再停在「估算末尾」附近，而是末段——最后一行须落进可见窗口。
    handle.start_drag();
    handle.set_offset(point(px(0.), px(-(content_h - view.size.height.as_f32()))));
    let bottom_ix = ls.logical_scroll_top().item_ix;
    let item_h = real_h / n.max(1) as f32;
    let visible = (view.size.height.as_f32() / item_h).ceil() as usize;
    assert!(
        bottom_ix + visible >= n - 1,
        "拖到底后末行应在视口内（top_ix={bottom_ix}，可见≈{visible}，共{n}，content_h={content_h:.0} real_h={real_h:.0}）；旧实现缩在估算末尾 ⇒ 末行不可达"
    );
    // 清理冻结值，避免影响测试后段。
    handle.end_drag();
}

/// 回归：「小文档预览拖滚动条不跟鼠标」——thumb 与拖拽目标的映射必须**逐 px 线性**，
/// 不得读 ListState 实测树（拖拽中标签逐帧重测量 → 同一逻辑位置映射的 px 逐帧漂移
/// → thumb 从抓点滑开）。现在 thumb 轴 = 共享 `desired`（px），`set_offset` 写 desired，
/// `offset()` 读 desired ⇒ 无论树如何变化始终 1:1。
#[gpui::test]
fn small_doc_scrollbar_thumb_tracks_drag_linearly(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    cx.simulate_resize(size(px(300.), px(200.)));
    settle(cx);

    // 混合行型（真实测量高度 ≠ 统一提示），多段 → 有量程。
    let mut doc = String::new();
    for i in 0..80 {
        doc.push_str(&format!(
            "## 第{i}节\n\n正文段落{i}，内容内容内容。\n- 列表{i}\n\n`code`\n\n"
        ));
    }
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_epub(true, Default::default(), cx);
            md.set_source(&doc, cx);
        });
    });
    cx.simulate_resize(size(px(300.), px(200.)));
    settle(cx);

    let (n, hint, ls, view) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let m = md.read(cx);
        (m.item_count(), m.est_hint, m.test_list_state(), m.list_state.viewport_bounds())
    });
    assert!(n > 0 && hint > 0.0, "小文档应就绪（n={n} hint={hint:.1}）");
    let handle = PreviewScrollHandle {
        list: ls.clone(),
        content_height: n as f32 * hint,
        est_hint: hint,
        n,
        windowed: false,
        desired: std::rc::Rc::new(std::cell::Cell::new(0.)),
    };
    handle.start_drag(); // 冻结 → 拖拽全程 content_size 稳定
    let vh = view.size.height.as_f32();
    let content_h = handle.content_size().height.as_f32();
    let max_scroll = (content_h - vh).max(0.);
    assert!(max_scroll > 1.0, "内容应有可拖量程（content_h={content_h:.0} vh={vh:.0}）");
    for pct in [0.1, 0.25, 0.5, 0.75, 1.0] {
        let target = max_scroll * pct;
        handle.set_offset(point(px(0.), px(-target)));
        let off = -handle.offset().y.as_f32();
        assert!(
            (off - target).abs() < 0.001,
            "thumb 与拖拽目标应逐 px 一致（pct={pct}：off={off:.2} target={target:.2}）"
        );
    }
    handle.end_drag();
}

/// 回归（统一口径）：小文档 `total_h()` 跟随 ListState 实测（而非估算），且页码用
/// 「底部到达页」语义——真机实测 > 估算时，拖到底必须「第 N/N 页」，滚轮到底也与
/// 滚动条同一「底」。旧实现：total_h 用 `行数×平均高` → 拖到底页码停在 < 总页数。
#[gpui::test]
fn small_doc_total_h_follows_list_and_page_reaches_end(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    cx.simulate_resize(size(px(300.), px(200.)));
    settle(cx);

    let mut doc = String::new();
    for i in 0..80 {
        doc.push_str(&format!("## 第{i}节\n\n正文段落{i}，内容内容内容。\n\n"));
    }
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_epub(true, Default::default(), cx);
            md.set_source(&doc, cx);
        });
    });
    cx.simulate_resize(size(px(300.), px(200.)));
    settle(cx);

    let (n, hint, ls, view) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let m = md.read(cx);
        (m.item_count(), m.est_hint, m.test_list_state(), m.list_state.viewport_bounds())
    });
    let estimate = n as f32 * hint;
    let vh = view.size.height.as_f32();
    // 模拟真机实测更高：summary 铺成 1.5×估算。
    ls.reset_with_uniform_height(n, px(hint * 1.5));

    // ① total_h() 必须跟随 ListState 实测（= vertical_total_px），而非估算。
    let (total, measured) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let m = md.read(cx);
        (m.total_h(), m.vertical_total_px())
    });
    assert!(
        (total - measured).abs() < 1.0 && total > estimate * 1.4,
        "total_h 应跟随实测（total={total:.0} 实测={measured:.0} 估算={estimate:.0}）"
    );

    // ② 拖到底（与滚动条同一[底]：start_drag 冻结 → thumb 到底）→ 页码 = 第 N/N 页。
    let handle = PreviewScrollHandle {
        list: ls.clone(),
        content_height: estimate,
        est_hint: hint,
        n,
        windowed: false,
        desired: std::rc::Rc::new(std::cell::Cell::new(0.)),
    };
    handle.start_drag();
    let content_h = handle.content_size().height.as_f32();
    handle.set_offset(point(px(0.), px(-(content_h - vh))));
    handle.end_drag();
    let (page, total_pages) = cx.update(|_, cx| {
        host.read(cx)
            .md
            .read(cx)
            .viewport_page_bounds()
            .expect("epub 应有页码读数")
    });
    assert_eq!(
        page, total_pages,
        "拖到底应到第 N/N 页：current={page}/{total_pages}，total_h={total:.0} 估={estimate:.0}"
    );
}

/// 页面恒换行：超长无空格行在窄视口下**折行**而非撑出页面级横向滚动。
/// 旧「非换行自然宽 + 页面级横向滚动」模式已废弃——`md-harea`（页面横向滚动容器）
/// 量程必须为 0，折行后最右字右缘 ≤ 视口右缘 − `PREVIEW_SIDE_GAP_PX`。
#[gpui::test]
fn wide_unbroken_line_wraps_no_page_hscroll(cx: &mut TestAppContext) {
    let (host, cx) = setup_wide(cx);
    let side_gap = crate::config::markdown::PREVIEW_SIDE_GAP_PX;
    let pane = cx
        .debug_bounds("md-preview")
        .expect("md-preview 应渲染");
    let pane_right = pane.right().as_f32();
    let (max_x, max_right) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        let max_right = md
            .text_ctx
            .hits
            .borrow()
            .iter()
            .map(|h| h.origin.x.as_f32() + h.layout.bounds().size.width.as_f32())
            .fold(0.0f32, f32::max);
        (md.h_handle.max_offset().x, max_right)
    });
    assert_eq!(max_x, px(0.), "页面不应再有页面级横向滚动，max_offset.x={max_x:?}");
    assert!(
        max_right <= pane_right - side_gap + 1.5,
        "超长行折行后最右字应留在右留白内：right={max_right:.1} pane_right={pane_right:.1} gap={side_gap:.0}"
    );
}

/// 横向滚动条叠层铺满预览区（得到确定的布局视口），但页面恒换行下 `h_handle`
/// 无页面级横向量程（超宽内容只在块内滚动）——`max_offset.x` 必须为 0。
#[gpui::test]
fn hscrollbar_overlay_covers_preview_without_page_range(cx: &mut TestAppContext) {
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
    let max_x = cx.update(|_, cx| host.read(cx).md.read(cx).h_handle.max_offset().x);
    assert_eq!(
        max_x, px(0.),
        "页面恒换行：宽内容也无页面级横向滚动量程，实际={max_x:?}"
    );
}

/// 内容不超宽时页面横向不可滚动（不产出多余页面级横向滚动范围）。
#[gpui::test]
fn no_hscroll_when_content_fits(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 几行短内容 → 不发生页面横向溢出
    let short = "短文本\n第二行\n结束";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(short, cx));
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);

    let max_x = cx.update(|_, cx| host.read(cx).md.read(cx).h_handle.max_offset().x);
    assert_eq!(max_x, px(0.), "短内容不应产生页面横向滚动范围，实际={max_x:?}");
}

/// 代码块的浅灰背景带与表格网格都应内缩（不贴左分隔栏边缘），与正文对齐。
/// 回归：背景必须画在内层元素上，外层仅用 px_3 留白（bg 会铺满含 padding 的整盒）。
#[gpui::test]
fn code_band_and_table_inset_from_left_edge(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
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
    // 与正文一致的内缩 = 内容列对称留白 `PREVIEW_SIDE_GAP_PX` + 行内内衬 `CONTENT_EDGE_PAD_X_PX`
    // （单一来源，容 1px 浮点误差）
    let expect_inset =
        crate::config::markdown::PREVIEW_SIDE_GAP_PX + crate::config::markdown::CONTENT_EDGE_PAD_X_PX;
    for (name, b) in [("代码背景带", band), ("表格", table)] {
        assert!(
            (b.origin.x - px(expect_inset)).abs() < px(1.),
            "{name} 应从左边缘内缩约 {expect_inset:.0}px，实际 origin.x={:?}",
            b.origin.x
        );
    }
    // 代码背景带应有宽高（不会塌缩成 0）
    assert!(
        band.size.width > px(0.) && band.size.height > px(0.),
        "代码背景带不应塌缩，实际={band:?}"
    );
}

/// 代码块的语法高亮缓存：` ```rust ` 围栏按语言整块解析（有 keyword/string/comment run），
/// 无语言 ` ``` ` 围栏不产出高亮。这是「预览代码块着色」的管线回归——
/// set_source → 建缓存 → 渲染时按主题解析颜色的完整链路。
#[gpui::test]
fn code_block_syntax_highlight_cache(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;

    let rust_src = "```rust\nfn main() {\n    let s = \"str\";\n    // 注释\n}\n```\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(rust_src, cx));
    });
    cx.simulate_resize(size(px(400.), px(600.)));
    settle(cx);

    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        let code_ix = md
            .rows
            .iter()
            .position(|r| r.docrow.kind == IrKind::Code)
            .expect("应有代码渲染行");
        let runs = md
            .code_hl
            .get(&code_ix)
            .expect("rust 代码行应有语法高亮缓存");
        assert!(!runs.is_empty(), "高亮缓存应非空");
        let names: Vec<&str> = runs
            .iter()
            .flat_map(|l| l.iter().map(|(_, n)| *n))
            .collect();
        assert!(names.contains(&"keyword"), "应识别关键字: {names:?}");
        assert!(names.contains(&"string"), "应识别字符串: {names:?}");
        assert!(names.contains(&"comment"), "应识别注释: {names:?}");
    });

    // 无语言栅栏 → 不建立高亮缓存（按纯文本渲染）
    let plain_src = "```\nlet x = 1;\n```\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(plain_src, cx));
    });
    cx.simulate_resize(size(px(400.), px(600.)));
    settle(cx);
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        assert!(md.code_hl.is_empty(), "无语言代码块不应有高亮缓存");
    });
}

/// 纯函数：自适应过扫余量随可见块数增长、并有 [24, MAX] 上下限、且单调不减。
#[test]
fn adaptive_overdraw_scales_with_visibility() {
    use crate::config::limits::{WINDOWED_OVERDRAW_BLOCKS, WINDOWED_OVERDRAW_MAX};
    assert_eq!(
        adaptive_overdraw(1),
        WINDOWED_OVERDRAW_BLOCKS,
        "小视口保持基准余量"
    );
    assert_eq!(adaptive_overdraw(20), WINDOWED_OVERDRAW_BLOCKS);
    let big = adaptive_overdraw(200);
    assert!(big > WINDOWED_OVERDRAW_BLOCKS, "大视口余量应超过基准: {big}");
    assert!(big <= WINDOWED_OVERDRAW_MAX);
    assert_eq!(
        adaptive_overdraw(10_000),
        WINDOWED_OVERDRAW_MAX,
        "超大视口应封顶"
    );
    let mut prev = 0usize;
    for v in 1..600 {
        let m = adaptive_overdraw(v);
        assert!(m >= prev, "余量应随可见块数单调不减: v={v} m={m} prev={prev}");
        prev = m;
    }
}

/// `CodeHlCache` 纯单元测试：LRU 命中复用同一 `Rc`（不重解析）、容量满逐出最久未用、
/// 清空后从零开始。
#[test]
fn code_hl_cache_lru_semantics() {
    use std::rc::Rc;
    let mut cache = super::CodeHlCache::new();
    // 命中 → 同一 Rc（复用，不重跑解析）
    let a1 = Rc::new(vec![vec![(0..3, "keyword")]]);
    cache.insert(7, a1.clone());
    let got = cache.get(7).expect("应命中");
    assert!(Rc::ptr_eq(&a1, &got), "命中应返回同一 Rc");
    // 更新已有键 → 仍同一键
    let a2 = Rc::new(vec![]);
    cache.insert(7, a2.clone());
    assert!(Rc::ptr_eq(&a2, &cache.get(7).unwrap()), "更新后返回新 Rc");
    // 容量：用小容量验证逐出（0 访问过 → 最久未用是 1）
    let mut small = super::CodeHlCache {
        cap: 3,
        map: std::collections::HashMap::new(),
        lru: std::collections::VecDeque::new(),
    };
    let mk = |x: usize| Rc::new(vec![vec![(x..x, "x")]]);
    for k in 0..3 {
        small.insert(k, mk(k));
    }
    small.get(0); // 0 变为最近使用
    small.insert(3, mk(3)); // 逐出最久未用（=1）
    assert!(small.get(0).is_some(), "访问过的 0 应存活");
    assert!(small.get(1).is_none(), "最久未用 1 应被逐出");
    assert!(small.get(3).is_some(), "新插入 3 应存在");
    small.clear();
    assert_eq!(small.len(), 0, "clear 后清空");
}

/// 回归 + LRU 复用：窗口模式的代码块高亮键必须是**全局块下标**（旧代码误用窗口内
/// 局部下标，win_lo>0 后渲染永远查不到 → 滚动后代码块丢色）。把列表滚到中部让
/// render 在那里建房：当前窗口每个代码块在 `code_hl` 里按全局键都能查到。
#[gpui::test]
fn windowed_code_highlight_uses_global_block_index(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;

    // 代码块散布整篇的大文档（> WINDOWED_MIN_BLOCKS → 窗口级懒解析）
    let block = "```rust\nfn f() { let s = \"x\"; }\n```\n\n正文段 `code`\n\n";
    let mut src = String::new();
    for _ in 0..1200 {
        src.push_str(block);
    }
    let index = std::rc::Rc::new(crate::markdown::scan_blocks(&src));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(PreviewSrc::from(SharedString::from(src)), index, cx)
        });
    });
    cx.simulate_resize(size(px(600.), px(600.)));
    settle(cx);

    // 像素滚动（与滚轮同款路径）连续下滚：render 每帧调度 ensure_window，
    // 窗口应随可见范围移动（固定窗口虚拟化的 `scroll_vertical` 会按需重定位窗口）。
    // 滚到中部让窗口建在 win_lo>0 处。
    for _ in 0..100 {
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.scroll_vertical(-500.);
                cx.notify();
            });
        });
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }
    // 滚动结束：大跳转的新进代码块走延后分片补色，泵渲染直到窗口内带语言代码块全部
    // 上色（有界；延后是本设计语义，不是丢色——见 `windowed_code_highlight_defers_*`）。
    let mut guard = 0;
    loop {
        let done = cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            let md = md.read(cx);
            (0..md.win_rows.len()).all(|j| {
                md.win_rows[j].docrow.kind != IrKind::Code
                    || md.win_rows[j].code_language().is_none()
                    || md.code_hl.get(&(md.win_lo + j)).is_some()
            })
        });
        if done || guard > 32 {
            break;
        }
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        guard += 1;
    }
    assert!(guard <= 32, "延后补色 32 帧内未收敛");

    let win_lo = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        assert!(md.win_lo > 0, "窗口应已移离顶部 win_lo={}", md.win_lo);
        let hl = md.code_hl.clone();
        assert!(!hl.is_empty(), "窗口内应有代码高亮");
        let code_keys: Vec<usize> = (0..md.win_rows.len())
            .filter(|&j| md.win_rows[j].docrow.kind == IrKind::Code)
            .map(|j| md.win_lo + j)
            .collect();
        let found = code_keys.iter().filter(|k| hl.get(k).is_some()).count();
        assert_eq!(
            found,
            code_keys.len(),
            "当前窗口的代码块应全部有高亮（全局键），漏 {}/{}\nwin_lo={} code_keys={code_keys:?}",
            code_keys.len() - found,
            code_keys.len(),
            md.win_lo,
        );
        md.win_lo
    });
    assert!(win_lo > 0);
}

/// 回归：大跳转重建时新进代码块（文本超 [`CODE_HL_DEFER_BYTES`]）走「延后分片补色」
/// ——建房当帧该块暂不入 `code_hl`（大跳转/滚动帧不因 syntect 停驻，廉价帧），随后
/// 帧由 `dispatch_pending_code_hl` 按字节预算并入；最终窗口内所有代码块都有高亮。
#[gpui::test]
fn windowed_code_highlight_defers_big_jump_then_converges(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;

    // 窗口级大文档：前部小代码块（首次建房同步），尾部一个远超延后阈值的大代码块。
    let mut src = String::new();
    for _ in 0..2000 {
        src.push_str("```rust\nfn f() { let s = \"x\"; }\n```\n\n正文段\n\n");
    }
    src.push_str("```rust\n");
    for _ in 0..1200 {
        src.push_str("let v = 0123456789abcdef;\n");
    }
    src.push_str("```\n\n");
    let index = std::rc::Rc::new(crate::markdown::scan_blocks(&src));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(
                PreviewSrc::from(SharedString::from(src)),
                index,
                cx,
            )
        });
    });
    cx.simulate_resize(size(px(600.), px(600.)));
    settle(cx);

    // 一步大跳 → 窗口重建到覆盖大代码块的区域（> DEFER_BYTES → 延后）。
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.scroll_vertical(-2_000_000.);
            cx.notify();
        });
    });
    cx.run_until_parked();
    let _ = cx.cx.refresh();

    // 大跳转后的第一帧：窗口内应有带语言代码块命中延后队列（不在 code_hl、pending 挂起）。
    let (deferred_missing, pending) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        let missing = (0..md.win_rows.len())
            .filter(|&j| {
                md.win_rows[j].docrow.kind == IrKind::Code
                    && md.win_rows[j].code_language().is_some()
                    && md.code_hl.get(&(md.win_lo + j)).is_none()
            })
            .count();
        (missing, md.pending_code_hl.is_some())
    });
    assert!(
        deferred_missing > 0 && pending,
        "大跳转后第一帧应有延后未上色的代码块（missing={deferred_missing}, pending={pending}）"
    );

    // 后续帧分片补色：泵渲染直到延后队列清空（有界），再断言窗口内所有带语言代码块
    // 都有高亮。分片按帧推进，具体帧数取决于字节/块数预算，故不能写死帧数。
    let mut guard = 0;
    loop {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        let still_pending = cx
            .update(|_, cx| host.read(cx).md.read(cx).pending_code_hl.is_some());
        guard += 1;
        if !still_pending || guard > 32 {
            break;
        }
    }
    assert!(guard <= 32, "延后队列 32 帧内未收敛（怀疑死循环）");
    let (missing, pending) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        let missing = (0..md.win_rows.len())
            .filter(|&j| {
                md.win_rows[j].docrow.kind == IrKind::Code
                    && md.win_rows[j].code_language().is_some()
                    && md.code_hl.get(&(md.win_lo + j)).is_none()
            })
            .count();
        (missing, md.pending_code_hl.as_ref().map(|q| q.1.len()))
    });
    assert!(
        missing == 0 && pending.is_none(),
        "延后补色应收敛：仍缺 {missing} 个带语言块、队列挂起 {pending:?}"
    );
}


