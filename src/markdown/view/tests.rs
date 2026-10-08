//! 视图层测试：行级虚拟化 / 横向滚动 / 表格 / 目录 / 图片 / 字号缩放 的回归与冒烟测试。

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
    let handle = PreviewVerticalScrollHandle {
        list: ls.clone(),
        content_width: 300.0,
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
    let handle = PreviewVerticalScrollHandle {
        list: ls.clone(),
        content_width: 300.0,
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
    let handle = PreviewVerticalScrollHandle {
        list: ls.clone(),
        content_width: 300.0,
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
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 几行短内容（估算宽远小于 300 视口）→ 不发生横向溢出
    let short = "短文本\n第二行\n结束";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(short, cx));
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
            .position(|r| r.kind == RowKind::Code)
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
                md.win_rows[j].kind != RowKind::Code
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
            .filter(|&j| md.win_rows[j].kind == RowKind::Code)
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
                md.win_rows[j].kind == RowKind::Code
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
                md.win_rows[j].kind == RowKind::Code
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

/// GFM 表格应渲染成有实际宽高的网格，且其宽度计入横向滚动内容宽估算。
#[gpui::test]
fn renders_markdown_table_as_grid(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
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
    cx.update(crate::actions::bind_local_keymaps);
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

/// EPUB 阅读 / 宿主全局字号：缩放应更新正文字号覆盖（基数 = 主题默认）。
#[gpui::test]
fn font_zoom_updates_override(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
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

    // 模拟宿主推进全局字号（放大 1.5）
    cx.update(|window, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_font_size_anchored(default + 1.5, cx));
        window.refresh();
    });
    settle(cx);
    let after = cx.update(|_, cx| host.read(cx).md.read(cx).font_override);
    assert!(after.is_some(), "缩放后应产生字号覆盖");
    assert!(
        (after.unwrap() - (default + 1.5)).abs() < 0.001,
        "字号应增大 1.5，实际={after:?}"
    );
}

/// 标题字号应随正文字号等比缩放（#/##/### 一起变大变小，子级始终小于父级）。
/// 设计基准 `HEADING_DESIGN_BASE`(=14) 下 h1=20 h2=18 h3/4=16 h5/6=15；其余字号按
/// `body / 基准` 等比放大，最多只放大 2 倍。与运行期默认字号 `layout::DEFAULT_FONT_SIZE`
/// 无耦合（默认字号调大时基准不变）。
#[gpui::test]
fn heading_metrics_scale_with_body_font(cx: &mut TestAppContext) {
    let h = |body: f32| -> [f32; 7] {
        let mut out = [0.0f32; 7];
        for lv in 1..=6u8 {
            out[lv as usize] =
                crate::markdown::view::style::heading_metrics_for_size(body, lv).0.as_f32();
        }
        out
    };
    // 参考字号 14px：标题等于设计值。
    let ref14 = h(14.0);
    assert_eq!(
        &ref14[1..],
        &[20.0, 18.0, 16.0, 16.0, 15.0, 15.0],
        "14px 时标题应等于设计值：{ref14:?}"
    );
    // 等比放大：正文翻倍 → 各标题也翻倍（比例不随 DEFAULT_FONT_SIZE 变化）。
    let big = h(28.0);
    for lv in 1..=6u8 {
        assert!(
            (big[lv as usize] - ref14[lv as usize] * 2.0).abs() < 0.001,
            "h{lv} 应放大 2 倍：{} vs {}",
            big[lv as usize],
            ref14[lv as usize] * 2.0
        );
    }
    // 任意正文字号下：标题始终大于正文、子级小于父级（h1>h2>h3>h6）。
    for &body in &[14.0f32, 24.0, 28.0] {
        let hb = h(body);
        for lv in 1..=6u8 {
            assert!(
                hb[lv as usize] > body,
                "h{lv} 应大于正文字号 {body}：{}",
                hb[lv as usize]
            );
        }
        assert!(
            hb[1] > hb[2] && hb[2] > hb[3] && hb[3] > hb[5],
            "h1>h2>h3>h6 @ {body}：{hb:?}"
        );
    }
    // 绑定默认值：预览默认正文 = layout::DEFAULT_FONT_SIZE，标题随之缩放。
    cx.update(crate::actions::bind_local_keymaps);
    let p = cx.update(|cx| StyleParams::from_theme_with_font(cx, None));
    assert_eq!(
        p.para_size.as_f32(),
        crate::config::layout::DEFAULT_FONT_SIZE,
        "预览默认正文字号应与 layout 一致"
    );
    for lv in 1..=6u8 {
        let expect = crate::markdown::view::style::heading_metrics_for_size(
            crate::config::layout::DEFAULT_FONT_SIZE,
            lv,
        )
        .0;
        assert_eq!(p.heading_metrics(lv).0, expect, "h{lv} 默认字号下应随正文缩放");
    }
}

/// 字号缩放（EPUB 阅读 / 宿主 set_font_override）：缩放**不带动页面滚动**（同 PDF 视口锚定）。
///
/// 预览自身的 Ctrl 滚轮缩放覆盖层已移除——缩放入口统一在宿主（app::panes zoom_overlay →
/// change_font_size），这里验证底层原语 `set_font_size_anchored`（宿主推进全局字号时
/// 的绝对落点/外部调用方都经它落地）：
/// 视口顶部块锚定不动；普通滚轮（非 Ctrl）照常滚动、字号不变。
#[gpui::test]
fn font_zoom_keeps_viewport_top_anchored(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 大文档（块数 > WINDOWED_MIN_BLOCKS）→ 窗口级懒解析，纵向可滚。
    let mut src = String::new();
    for i in 0..1000 {
        src.push_str(&format!("# 第{i}章\n\n正文段落内容 alpha，用于缩放手感验证。\n- 列表项\n\n"));
    }
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(
                PreviewSrc::from(gpui::SharedString::from(src.clone())),
                std::rc::Rc::new(crate::markdown::scan_blocks(&src)),
                cx,
            );
        });
    });
    cx.simulate_resize(size(px(600.), px(700.)));
    settle(cx);
    let n = cx.update(|_, cx| host.read(cx).md.read(cx).item_count());
    assert!(
        n > crate::config::limits::WINDOWED_MIN_BLOCKS,
        "应启用窗口级懒解析，块数 {n}"
    );
    // 跳到文档中部（留出双向滚动余量）。
    let mid = n / 2;
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.scroll_to_row(mid);
            cx.notify();
        });
    });
    settle(cx);
    let def = cx.update(|_, cx| StyleParams::from_theme_with_font(cx, None).para_size.as_f32());
    let before = cx.update(|_, cx| host.read(cx).md.read(cx).test_abs_scroll_top().0);
    assert!(before > 100, "应已滚到中部（block={before}）");

    // 缩放字号（+1.5）：字号变大，且滚动位置（视口顶部块）不变（不滚动）。
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_font_size_anchored(def + 1.5, cx);
            cx.notify();
        });
    });
    settle(cx);
    let (f1, after_block) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        (md.font_override, md.test_abs_scroll_top().0)
    });
    assert!(f1.is_some() && f1.unwrap() > def, "缩放应放大字号：{f1:?}");
    assert!(
        (after_block as i64 - before as i64).abs() <= 2,
        "缩放不应滚动：视口顶部块 {before} → {after_block}"
    );

    // 普通滚轮（非 Ctrl）：照常滚动、字号不变。
    let f_before_plain = f1.unwrap();
    let pos = {
        let b = cx.debug_bounds("md-preview-root").expect("预览根应渲染");
        point(
            px(b.origin.x.as_f32() + b.size.width.as_f32() * 0.5),
            px(b.origin.y.as_f32() + b.size.height.as_f32() * 0.5),
        )
    };
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(0.), px(240.))),
        modifiers: Modifiers::default(),
        ..Default::default()
    });
    settle(cx);
    let (f2, plain_block) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        (md.font_override, md.test_abs_scroll_top().0)
    });
    assert_eq!(f2, Some(f_before_plain), "普通滚轮不应改变字号");
    assert!(
        plain_block != after_block,
        "普通滚轮应照常滚动（块 {after_block} → {plain_block}）"
    );
}

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
            .filter(|r| r.kind == RowKind::Image)
            .map(|r| r.image().map(|i| i.src.to_string()))
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
            .filter(|r| r.kind == RowKind::Image)
            .map(|r| {
                let i = r.image();
                (i.and_then(|i| i.img_w), i.and_then(|i| i.img_h))
            })
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
        let img_rows: Vec<_> = md.rows.iter().filter(|r| r.kind == RowKind::Image).collect();
        assert_eq!(img_rows.len(), 1, "应有 1 个图片行");
        let r = img_rows[0];
        let i = r.image();
        (
            i.is_some_and(|i| i.img_data.is_some()),
            i.is_some_and(|i| i.img_w.is_some() && i.img_h.is_some()),
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

/// 基准：代码块/表格句柄地图「每帧整张 HashMap 克隆 vs Rc 共享」的代价对比。
/// 旧代码在 `render()` 里逐帧 `code_scroll/table_scroll/code_hl.clone()`（O(N) 深拷贝），
/// 新逻辑克隆 `Rc`（O(1) 引用计数）。打印、不构成断言；`--ignored` 运行。
#[test]
#[ignore]
fn bench_block_extras_clone_vs_rc_share() {
    use std::time::Instant;
    use std::collections::HashMap;
    use std::rc::Rc;
    let mut m: HashMap<usize, u64> = HashMap::new();
    for i in 0..1000 {
        m.insert(i, i as u64);
    }
    let shared = Rc::new(m.clone());
    let runs = 2_000_000usize;
    let t0 = Instant::now();
    let mut sink = 0u64;
    for i in 0..runs {
        let c = m.clone();
        sink = sink.wrapping_add(c.get(&(i % 1000)).copied().unwrap_or(0));
    }
    let cloned = t0.elapsed();
    let t1 = Instant::now();
    let mut sink2 = 0u64;
    for i in 0..runs {
        let c = shared.clone();
        sink2 = sink2.wrapping_add(c.get(&(i % 1000)).copied().unwrap_or(0));
    }
    let rc_share = t1.elapsed();
    assert_eq!(sink, sink2, "两种取数方式结果必须一致");
    println!(
        "1000 项句柄地图共享（{runs} 次）：每帧整张克隆={}ms, Rc 共享={}ms（提升 {:.1}x）",
        cloned.as_millis(),
        rc_share.as_millis(),
        cloned.as_secs_f64() / rc_share.as_secs_f64().max(1e-9),
    );
}

/// 基准（--ignored）：窗口模式滚动时，代码块语法高亮 LRU 缓存 + 自适应过扫的效果。
///
/// 方法：合成「代码块散布整篇」的大文档（窗口级懒解析），在文档中**滚动 200 步**
/// （每步前进 6 块、可见带 20 块），驱动 `ensure_window`：
/// - **冷**（旧行为）：每次重建后清空 LRU → 每窗所有代码块都重跑 syntect；
/// - **热**（新）：保留 LRU → 重叠代码块复用 `Rc`，只有新进窗的块才解析；
/// - **过扫重建数**：固定余量 24 vs 大余量 100（模拟自适应高视口）各自的窗口重建次数。
/// 打印、不断言；`--ignored` 运行。
#[gpui::test]
#[ignore]
fn bench_windowed_scroll_highlight_cache(cx: &mut TestAppContext) {
    use std::time::Instant;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;

    let block = "```rust\nfn main() {\n    let s = \"str\";\n    // 注释行\n    let n = s.len();\n    println!(\"{n}\");\n}\n```\n\n正文段 `code` 与[链接](https://x)\n\n";
    let mut src = String::new();
    for _ in 0..1500 {
        src.push_str(block);
    }
    let index = std::rc::Rc::new(crate::markdown::scan_blocks(&src));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(PreviewSrc::from(SharedString::from(src)), index, cx)
        });
    });
    cx.simulate_resize(size(px(700.), px(700.)));
    settle(cx);
    let n_items = cx.update(|_, cx| host.read(cx).md.read(cx).item_count());
    assert!(n_items > crate::config::limits::WINDOWED_MIN_BLOCKS);

    let steps = 200usize;
    let step = 6usize;
    let vis = 20usize;

    // —— 冷（旧）：每窗全量重解析 ——
    let t0 = Instant::now();
    let cold_rebuilds = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, _| {
            md.overdraw_margin = 24;
            let mut rebuilds = 0usize;
            let mut last = md.win_lo;
            for s in 0..steps {
                let lo = s * step;
                md.ensure_window(lo, lo + vis);
                if md.win_lo != last {
                    rebuilds += 1;
                    last = md.win_lo;
                }
                md.code_hl_cache.clear(); // 旧行为：无缓存
            }
            rebuilds
        })
    });
    let cold_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // —— 热（新）：LRU 复用 ——
    let t0 = Instant::now();
    let warm_rebuilds = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, _| {
            md.overdraw_margin = 24;
            let mut rebuilds = 0usize;
            let mut last = md.win_lo;
            for s in 0..steps {
                let lo = s * step;
                md.ensure_window(lo, lo + vis);
                if md.win_lo != last {
                    rebuilds += 1;
                    last = md.win_lo;
                }
            }
            rebuilds
        })
    });
    let warm_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let cache_len = cx.update(|_, cx| host.read(cx).md.read(cx).code_hl_cache.len());

    // —— 过扫：固定 24 vs 大余量 100（自适应高视口），重建次数对比 ——
    let rebuilds_m24 = run_scroll_rebuild_count(cx, &host, 24, steps, step, vis);
    let rebuilds_m100 = run_scroll_rebuild_count(cx, &host, 100, steps, step, vis);
    let adaptive_for_200 = super::adaptive_overdraw(200);

    println!(
        "窗口滚动 {} 步（步进 {step} 块、可见 {vis} 块，{} 块文档）：高亮冷(旧,无缓存)={cold_ms:.1} ms（{cold_rebuilds} 次重建） vs 热(新,LRU)={warm_ms:.1} ms（{warm_rebuilds} 次重建） 提升 {:.2}x；LRU 驻留 {} 块；过扫重建数 固定24={rebuilds_m24} vs 余量100={rebuilds_m100}（自适应视口200块→{adaptive_for_200}）",
        steps,
        n_items,
        cold_ms / warm_ms.max(1e-9),
        cache_len,
    );
}

/// 以给定过扫余量跑一遍向前滚动，返回实际窗口重建次数（`win_lo` 变化次数）。
fn run_scroll_rebuild_count(
    cx: &mut VisualTestContext,
    host: &gpui::Entity<PreviewHost>,
    margin: usize,
    steps: usize,
    step: usize,
    vis: usize,
) -> usize {
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, _| {
            md.overdraw_margin = margin;
            let mut rebuilds = 0usize;
            let mut last = md.win_lo;
            for s in 0..steps {
                let lo = s * step;
                md.ensure_window(lo, lo + vis);
                if md.win_lo != last {
                    rebuilds += 1;
                    last = md.win_lo;
                }
            }
            rebuilds
        })
    })
}

/// 基准（--ignored）：#3 统一高度提示的 O(块数) SumTree 物化成本。仿真大文档量级
/// （16 万块）：ListState::new 与 with_uniform_item_height 各建一遍节点树，再模拟
/// 连续换宽（每次重建）10 次。打印、不断言。
#[gpui::test]
#[ignore]
fn bench_list_uniform_height_cost(cx: &mut TestAppContext) {
    use std::time::Instant;
    let (new_ms, hint_ms, x10_ms, remeasure_ms, remeasure_win_ms) = cx.update(|_cx| {
        use gpui::{ListAlignment, ListState, px};
        let n = 160_000usize;
        let t = Instant::now();
        let state = ListState::new(n, ListAlignment::Top, px(12.0));
        let new_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let state_hinted = state.clone().with_uniform_item_height(px(24.0));
        let hint_ms = t.elapsed().as_secs_f64() * 1000.0;
        // 模拟连续换宽：每次 with_uniform 全量重建
        let t = Instant::now();
        for w in 0..10 {
            let _ = state_hinted.clone().with_uniform_item_height(px(20.0 + w as f32));
        }
        let x10_ms = t.elapsed().as_secs_f64() * 1000.0;
        // 同块数换内容时的 remeasure（不重建树，仅置脏可见区）
        let t = Instant::now();
        for _ in 0..10 {
            state_hinted.remeasure();
        }
        let remeasure_ms = t.elapsed().as_secs_f64() * 1000.0;
        // 优化后：打字路径只 remeasure 可见窗口（~60 行）而非全量
        let t = Instant::now();
        for _ in 0..10 {
            state_hinted.remeasure_items(50..110);
        }
        let remeasure_win_ms = t.elapsed().as_secs_f64() * 1000.0;
        (new_ms, hint_ms, x10_ms, remeasure_ms, remeasure_win_ms)
    });
    println!(
        "16 万块 ListState：new={new_ms:.1} ms；with_uniform_item_height={hint_ms:.1} ms（+new ≈ 打开时两次数节点分配）；连续换宽 ×10（每次重建）={x10_ms:.1} ms；打字同块数全量 remeasure ×10={remeasure_ms:.1} ms vs 只重测可见窗口 ×10={remeasure_win_ms:.1} ms（{:.0}x）",
        remeasure_ms / remeasure_win_ms.max(1e-6),
    );
    let _ = cx;
}

/// 基准（--ignored）：预览纵向滚动条**拖拽每步成本**（真机「预览拖滚动条明显滞后，
/// 编辑器正常」的定位用）。两种模式各量：小文档（混合行型、真实测量高度）与窗口
/// 模式（fixtures/大文档测试2.md）。每步 = 快照句柄 set_offset 拖一个 pct + 一帧
/// 完整 render/layout/prepaint（cx.refresh），量 K 步均摊墙钟（release 为准）。
/// 打印、不断言。
#[gpui::test]
#[ignore]
fn bench_preview_scrollbar_drag_step_cost(cx: &mut TestAppContext) {
    use std::time::Instant;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, vcx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let vcx: &mut VisualTestContext = vcx;

    // —— 小文档：混合行型（行高差异大 → 真实测量 ≠ 统一提示）——
    let mut src = String::new();
    for i in 0..60 {
        src.push_str(&format!(
            "# 标题 {i} 内容内容内容内容内容\n\n普通段落行 {i} 内容内容内容内容内容内容\n\n"
        ));
        src.push_str(&format!("- 列表项 {i} 内容内容内容内容\n\n"));
        src.push_str(&format!("> 引述 {i} 内容内容内容内容内容内容内容\n\n"));
        if i % 6 == 0 {
            src.push_str(&format!(
                "```rs\nlet x = {i};\nfn f() {{ g() }}\n```\n\n"
            ));
        }
    }
    vcx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(&src, cx));
    });
    vcx.simulate_resize(size(px(980.), px(760.)));
    settle(vcx);

    let small = vcx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let m = md.read(cx);
        (m.item_count(), m.index.is_some())
    });
    eprintln!("[drag-bench] small: items={} windowed={}", small.0, small.1);
    let steps_n = 120usize;
    let mut t_set = 0.0f64;
    let mut t_pp = 0.0f64;
    let mut t_frame = 0.0f64;
    let p_small = vcx.update(|_w, cx| Rc::new(StyleParams::from_theme_with_font(cx, None)));
    for k in 1..=steps_n {
        let pct = k as f32 / (steps_n as f32 + 1.0);
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, _cx| {
                let list = md.list_state.clone();
                let view = list.viewport_bounds();
                let handle = PreviewVerticalScrollHandle {
                    list,
                    content_width: md.content_w,
                    content_height: md.total_h(),
                    est_hint: md.est_hint,
                    n: md.item_count(),
                    windowed: md.index.is_some(),
                    desired: md.desired_scroll.clone(),
                };
                let c = handle.content_size().height.as_f32();
                let target = -((c - view.size.height.as_f32()) * pct);
                let t = Instant::now();
                handle.set_offset(point(px(0.), px(target)));
                t_set += t.elapsed().as_secs_f64() * 1000.0;
                let t = Instant::now();
                md.prepare_render(&p_small, 980.0, _cx);
                t_pp += t.elapsed().as_secs_f64() * 1000.0;
                _cx.notify();
            });
        });
        let t = Instant::now();
        vcx.run_until_parked();
        let _ = vcx.cx.refresh();
        vcx.run_until_parked();
        t_frame += t.elapsed().as_secs_f64() * 1000.0;
    }
    eprintln!(
        "[drag-bench] small-doc: set_offset 均摊 {:.3} ms/步 | prepare_render 均摊 {:.3} ms/步 | 整帧(含布局) 均摊 {:.3} ms/步",
        t_set / steps_n as f64,
        t_pp / steps_n as f64,
        t_frame / steps_n as f64
    );

    // —— 窗口模式（真实大文档，存在才测）——
    let path = "fixtures/大文档测试2.md";
    if let Ok(text) = std::fs::read_to_string(path) {
        let rope0 = ropey::Rope::from_str(&text);
        let idx0 = std::rc::Rc::new(crate::markdown::scan_blocks_rope(&rope0));
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.set_source_with_index(
                    crate::markdown::PreviewSrc::Rope(rope0),
                    idx0.clone(),
                    cx,
                );
            });
        });
        vcx.simulate_resize(size(px(980.), px(760.)));
        for _ in 0..6 {
            vcx.run_until_parked();
            let _ = vcx.cx.refresh();
        }
        let win = vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            let m = md.read(cx);
            (m.item_count(), m.index.is_some())
        });
        eprintln!("[drag-bench] windowed: items={} windowed={}", win.0, win.1);
        let p_win = vcx.update(|_w, cx| Rc::new(StyleParams::from_theme_with_font(cx, None)));
        let mut t_set = 0.0f64;
        let mut t_pp = 0.0f64;
        let mut t_draw = 0.0f64;
        for k in 1..=steps_n {
            let pct = k as f32 / (steps_n as f32 + 1.0);
            vcx.update(|_, cx| {
                let md = host.read(cx).md.clone();
                md.update(cx, |md, _cx| {
                    let list = md.list_state.clone();
                    let view = list.viewport_bounds();
                    let handle = PreviewVerticalScrollHandle {
                        list,
                        content_width: md.content_w,
                        content_height: md.total_h(),
                        est_hint: md.est_hint,
                        n: md.item_count(),
                        windowed: md.index.is_some(),
                        desired: md.desired_scroll.clone(),
                    };
                    let c = handle.content_size().height.as_f32();
                    let target = -((c - view.size.height.as_f32()) * pct);
                    let t = Instant::now();
                    handle.set_offset(point(px(0.), px(target)));
                    t_set += t.elapsed().as_secs_f64() * 1000.0;
                    let t = Instant::now();
                    md.prepare_render(&p_win, 980.0, _cx);
                    t_pp += t.elapsed().as_secs_f64() * 1000.0;
                    _cx.notify();
                });
            });
            // 第一次 park 消费 notify → 走完整 render/layout/prepaint/paint（整帧）。
            let t = Instant::now();
            vcx.run_until_parked();
            t_draw += t.elapsed().as_secs_f64() * 1000.0;
            let _ = vcx.cx.refresh();
            vcx.run_until_parked();
        }
        eprintln!(
            "[drag-bench] windowed: set_offset 均摊 {:.3} ms/步 | prepare_render 均摊 {:.3} ms/步 | 帧(含布局/绘制，first park) 均摊 {:.3} ms/步",
            t_set / steps_n as f64,
            t_pp / steps_n as f64,
            t_draw / steps_n as f64
        );

        // —— 拆账：窗口模式拖动帧的成本 = apply_desired(重建 ListState) + ensure_window(解析窗口行)？
        let (mut t_app, mut t_win, app_cnt, win_cnt) = vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            let p = Rc::new(StyleParams::from_theme_with_font(cx, md.read(cx).font_override));
            let mut t_app = 0.0f64;
            let mut t_win = 0.0f64;
            let mut app_cnt = 0u32;
            let mut win_cnt = 0u32;
            md.update(cx, |md, _cx| {
                let total = md.total_h();
                let view = md.list_state.viewport_bounds().size.height.as_f32();
                let n = md.item_count();
                for k in 1..=steps_n {
                    let pct = k as f32 / (steps_n as f32 + 1.0);
                    let target = (total - view).max(0.0) * pct;
                    let t = Instant::now();
                    md.desired_scroll.set(target);
                    md.restore_zoom_anchor();
                    md.apply_desired(md.desired_scroll.get());
                    t_app += t.elapsed().as_secs_f64() * 1000.0;
                    app_cnt += 1;
                    // 与 prepare_render 同口径的窗口区间
                    let (lo_vis, _off) = md.abs_scroll_top();
                    let hint = md.est_hint.max(20.0);
                    let vis_base = ((view / hint).ceil() as usize).max(1);
                    let overdraw = adaptive_overdraw(vis_base);
                    let vis_n = vis_base + overdraw;
                    let last = n.saturating_sub(1);
                    let need_lo = lo_vis.saturating_sub(overdraw);
                    let need_hi = lo_vis.saturating_add(vis_n).min(last);
                    let t = Instant::now();
                    md.ensure_window(need_lo, need_hi);
                    md.bump_content_width_to_cover(&p);
                    t_win += t.elapsed().as_secs_f64() * 1000.0;
                    win_cnt += 1;
                }
            });
            (t_app, t_win, app_cnt, win_cnt)
        });
        eprintln!(
            "[drag-bench] windowed 拆账: apply_desired(重定位) 均摊 {:.3} ms/步 ×{app_cnt} | ensure_window+bump(解析窗口) 均摊 {:.3} ms/步 ×{win_cnt}",
            t_app / app_cnt as f64,
            t_win / win_cnt as f64
        );
        let _ = &mut t_app;
        let _ = &mut t_win;

        // —— 拆账 2：dispatch_pending_code_hl（延后补色）单步成本——
        let (t_disp, disp_cnt) = vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            let mut t_disp = 0.0f64;
            let mut disp_cnt = 0u32;
            md.update(cx, |md, cx| {
                for _k in 1..=steps_n {
                    let t = Instant::now();
                    md.dispatch_pending_code_hl(cx);
                    t_disp += t.elapsed().as_secs_f64() * 1000.0;
                    disp_cnt += 1;
                }
            });
            (t_disp, disp_cnt)
        });
        eprintln!(
            "[drag-bench] windowed dispatch_pending_code_hl 均摊 {:.3} ms/步 ×{disp_cnt}",
            t_disp / disp_cnt as f64
        );

        // —— 慢速拖拽（相邻窗口重叠 → 增量窗口生效）——
        let (t_slow, slow_cnt) = vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            let p = Rc::new(StyleParams::from_theme_with_font(cx, md.read(cx).font_override));
            let mut t_slow = 0.0f64;
            let mut slow_cnt = 0u32;
            md.update(cx, |md, cx| {
                let total = md.total_h();
                let view = md.list_state.viewport_bounds().size.height.as_f32();
                let hint = md.est_hint.max(20.0);
                // 每次前移约 30 块（贴近真实慢拖）：窗口高度 ~112 块，必然相邻重叠。
                let step_px = 30.0 * hint;
                let mut target = 0.0f32;
                md.desired_scroll.set(0.0);
                md.prepare_render(&p, 980.0, cx);
                while target < (total - view).max(0.0) {
                    target += step_px;
                    let t = Instant::now();
                    md.desired_scroll.set(target);
                    md.restore_zoom_anchor();
                    md.apply_desired(md.desired_scroll.get());
                    md.prepare_render(&p, 980.0, cx);
                    t_slow += t.elapsed().as_secs_f64() * 1000.0;
                    slow_cnt += 1;
                }
            });
            (t_slow, slow_cnt)
        });
        eprintln!(
            "[drag-bench] windowed 慢速拖拽(Δ≈30块): prepare_render 均摊 {:.3} ms/步 ×{slow_cnt}",
            t_slow / slow_cnt as f64
        );

        // —— 拆账 3：parse_block_window_rope 增量带（Δ40）vs 全窗（Δ150）——
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                let Some(src) = md.src_buf.clone() else { return };
                let Some(idx) = md.index.clone() else { return };
                let mut t_d40 = 0.0f64;
                let mut t_d150 = 0.0f64;
                let lo0 = md.win_lo.min(idx.blocks.len().saturating_sub(150));
                for k in 0..60usize {
                    let base = (idx.blocks.len() as f32 * (k as f32 / 60.0)) as usize;
                    let t = Instant::now();
                    let _ = match &src {
                        PreviewSrc::Str(s) => crate::markdown::parse::parse_block_window(s, &idx, base, base + 40),
                        PreviewSrc::Rope(r) => crate::markdown::parse::parse_block_window_rope(r, &idx, base, base + 40),
                    };
                    t_d40 += t.elapsed().as_secs_f64() * 1000.0;
                    let t = Instant::now();
                    let _ = match &src {
                        PreviewSrc::Str(s) => crate::markdown::parse::parse_block_window(s, &idx, base, base + 150),
                        PreviewSrc::Rope(r) => crate::markdown::parse::parse_block_window_rope(r, &idx, base, base + 150),
                    };
                    t_d150 += t.elapsed().as_secs_f64() * 1000.0;
                }
                let _ = lo0;
                eprintln!(
                    "[drag-bench] parse_block_window_rope 均摊：Δ40={:.3} ms | Δ150={:.3} ms",
                    t_d40 / 60.0,
                    t_d150 / 60.0
                );
                let _ = cx;
            });
        });
    } else {
        eprintln!("[drag-bench] skip windowed: 缺 {path}");
    }
}

/// 回归：「快速来回拖滚动条，滚动块停在旧位、要晃几下才跟上」——一帧内的高频
/// `MouseMoveEvent` 突发必须**帧级合并、绝不丢最后位置**。旧代码按 `max_fps` 节流
/// 丢弃未到期事件：突发里只有第一单生效，随后停住不产生新事件 → `desired` 停在
/// 第一单（如 30%），滚到轨道底也到不了；「晃几下」才补。现改为事件记
/// `pending_target`（永远最新）、`prepaint` 每帧落地。
#[gpui::test]
fn scrollbar_fast_drag_never_drops_final_target(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let mut doc = String::new();
    for i in 0..80 {
        doc.push_str(&format!(
            "## 第{i}节\n\n正文段落{i}，内容内容内容内容。\n- 列表{i} 内容内容。\n\n`code {i}`\n\n"
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

    let ov = cx
        .debug_bounds("md-vscrollbar-overlay")
        .expect("纵向滚动条 overlay 应有布局 bounds");
    let n = cx.update(|_, cx| host.read(cx).md.read(cx).item_count());
    assert!(n > 0, "预览应有内容");
    let container = ov.size.height.as_f32();
    let track_top = ov.origin.y.as_f32();
    // 轨道右侧 16px；滑块初始在顶部：thumb_start=0、thumb_len=48（MIN 下限）。
    let track_right = ov.right() - px(4.5);
    let thumb_center = point(track_right, px(track_top + 4.0 + 24.0));
    let bottom_pt = point(track_right, px(ov.bottom().as_f32() - 1.0));

    // 按下（滑块顶部）。fast 突发：30% → 60% → 轨道底，**中间不停帧**。
    cx.simulate_mouse_down(thumb_center, MouseButton::Left, Modifiers::default());
    let mut y = track_top + container * 0.30;
    cx.simulate_mouse_move(
        point(track_right, px(y)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    y = track_top + container * 0.60;
    cx.simulate_mouse_move(
        point(track_right, px(y)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_move(
        bottom_pt,
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    // 停住（不再产生事件）：帧级落地应把「在底部」的最后一单补上。
    settle(cx);
    cx.simulate_mouse_up(bottom_pt, MouseButton::Left, Modifiers::default());
    settle(cx);

    let (desired, max_scroll) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let m = md.read(cx);
        let ls = m.test_list_state();
        let max_off = ls.max_offset_for_scrollbar().y.as_f32();
        (m.desired_scroll.get(), max_off)
    });
    assert!(
        desired >= max_scroll * 0.98,
        "突发拖到底后最终目标不得被丢弃（desired 应近 {max_scroll:.0}，实际 {desired:.0}）；旧节流会停在第一单 ~30%"
    );
}
/// 表格滚动容器 = 预览视口宽；内部固定宽表格超宽时块内可横向滚动、滚动条真实渲染，
/// 句柄跨帧稳定（set_offset 能把表格真正向左滚动）——对齐代码块方案。
#[gpui::test]
fn wide_table_scrolls_in_wrap_mode(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 5 列 × 超长内容：估算宽远超 300px 视口
    let long = "x".repeat(60);
    let md_src = format!(
        "| {long} | {long} | {long} | {long} | {long} |\n|---|---|---|---|---|\n| {long} | {long} | {long} | {long} | {long} |\n"
    );
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
    let table = cx
        .debug_bounds("md-table")
        .expect("表格 md-table 应存在");
    assert!(
        table.size.width > preview.size.width,
        "超宽表格应宽于预览视口（从而可在块内横向滚动而非被裁剪），table={table:?} preview={preview:?}"
    );
    // 表格滚动容器应等于预览视口宽（自身不撑大，内容在内部横向滚动）
    let hscroll = cx
        .debug_bounds("md-table-hscroll")
        .expect("表格滚动容器 md-table-hscroll 应存在");
    assert!(
        hscroll.size.width <= preview.size.width + px(1.),
        "表格滚动容器应等于预览视口宽，实际 hscroll={hscroll:?} preview={preview:?}"
    );
    // 表格应完整放进滚动容器：容器严格高于表格（自然高度，不被裁剪）；
    // 底部预留滚动条轨道条（≥15px），横向滚动条不遮挡表格末行。
    assert!(
        hscroll.size.height > table.size.height,
        "滚动容器应严格高于表格（自然高度不被裁剪），table_h={:.0} hscroll_h={:?}",
        table.size.height.as_f32(),
        hscroll.size.height
    );
    assert!(
        hscroll.size.height.as_f32() - table.size.height.as_f32() >= 15.0,
        "底部应预留横向滚动条轨道条（≥15px 不遮挡内容），table_h={:.0} hscroll_h={:?}",
        table.size.height.as_f32(),
        hscroll.size.height
    );
    // 表格渲染高度至少覆盖 表头 + 分隔线 + 数据行（末行不被裁剪）
    let p = cx.update(|_w, cx| StyleParams::from_theme_with_font(cx, None));
    let row_h = p.para_size.as_f32() * 1.45 + 12.0;
    let content_h = row_h * 2.0 + 1.0;
    assert!(
        table.size.height.as_f32() >= content_h - 1.0,
        "表格高度应覆盖表头+数据行+分隔线（content_h={content_h:.1}），实际={:?}",
        table.size.height
    );
    // 滚动条叠层真实渲染（非零尺寸）
    let sbar = cx
        .debug_bounds("md-table-scrollbar")
        .expect("表格横向滚动条 md-table-scrollbar 应存在");
    assert!(
        sbar.size.width > px(0.) && sbar.size.height > px(0.),
        "横向滚动条不应塌缩成 0，实际={sbar:?}"
    );

    // 表格行在对应滚动句柄处注册；句柄 max_offset.x 应 > 0（可横向滚动）
    let rows = parse_to_rows(&md_src);
    let table_ix = rows
        .iter()
        .position(|r| r.kind == RowKind::Table)
        .expect("应有表格行");
    let (max_x,) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        let h = md.table_scroll.get(&table_ix).expect("表格应注册滚动句柄");
        (h.max_offset().x,)
    });
    assert!(max_x > px(0.), "超宽表格句柄 max_offset.x 应 > 0，实际={max_x:?}");

    // 关键：句柄跨帧稳定，set_offset 能真正向左滚动表格（滚动条拖动生效）
    let origin_x = table.origin.x.as_f32();
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let h = md
            .read(cx)
            .table_scroll
            .get(&table_ix)
            .expect("表格应注册滚动句柄")
            .clone();
        h.set_offset(gpui::point(px(-200.), px(0.)));
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);
    let table2 = cx
        .debug_bounds("md-table")
        .expect("滚动后 md-table 应仍存在");
    assert!(
        table2.origin.x.as_f32() < origin_x - 100.0,
        "设置表格滚动句柄后表格应向左滚动，origin.x: {origin_x:.0} -> {:?}",
        table2.origin.x
    );
}

/// 换行模式下，短代码块的背景应铺满预览视口宽（跟随窗口），而不是只到文本最右边缘，
/// 使各代码块视觉宽度统一；且不产生（无溢出的）横向滚动条。
#[gpui::test]
fn code_block_bg_spans_pane_width_in_wrap(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
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

/// 代码块上下各补一行空白带：背景色在首个代码行上方 / 末个代码行下方各多出一整行。
/// 验证 3 行代码的代码块渲染出 5 个相同高度的背景带，且首末两个空白带与代码行等高。
#[gpui::test]
fn code_block_has_blank_bands_above_and_below(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let md_src = "```rs\nlet a = 1;\nlet b = 2;\nlet c = 3;\n```\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(md_src, cx);
        });
    });
    cx.simulate_resize(size(px(400.), px(400.)));
    settle(cx);

    let blank_top = cx
        .debug_bounds("md-code-blank-top")
        .expect("代码块顶部空白带应存在");
    let blank_bottom = cx
        .debug_bounds("md-code-blank-bottom")
        .expect("代码块底部空白带应存在");
    // "md-code-band" 选择器被同名的三个代码带覆盖，debug_bounds 只保留最后一个（末行代码带）
    let last_band = cx
        .debug_bounds("md-code-band")
        .expect("代码背景带 md-code-band 应存在");
    let band_h = last_band.size.height.as_f32();

    // 顶/底空白带与代码带等高
    assert!(
        (blank_top.size.height.as_f32() - band_h).abs() < 0.5
            && (blank_bottom.size.height.as_f32() - band_h).abs() < 0.5,
        "空白带应与代码带等高：top={:?} bottom={:?} band={:?}",
        blank_top.size.height,
        blank_bottom.size.height,
        last_band.size.height
    );
    // 底部空白带紧贴最后一（3rd）个代码带下方，背景连续
    assert_eq!(
        blank_bottom.origin.y.as_f32(),
        last_band.origin.y.as_f32() + last_band.size.height.as_f32()
    );

    // 整体跨度：顶部空白带顶部 -> 底部空白带顶部 = 顶空白 + 3 行代码 = 4 * code_line
    let span = blank_bottom.origin.y.as_f32() - blank_top.origin.y.as_f32();
    assert!(
        (span - band_h * 4.0).abs() < 1.5,
        "顶空白带顶到 3 行代码块底部应跨 4*code_line，span={span:.1} band_h={band_h:.1}"
    );
    // 边界：顶部空白带从代码块渲染行顶端（y=0）开始，首行代码带在它下方
    assert_eq!(blank_top.origin.y.as_f32(), 0.0, "顶空白带应位于代码块渲染行顶端，实际={:?}", blank_top.origin.y);
}

/// 回归：代码块背景按主题定制——亮 #F5F5F5、暗 #262626（第 5 步谐波化的中性灰阶）。
/// 明暗切换走 `ui::theme::Theme::set`（第 3 波后 `StyleParams` 从 `ui::theme` 取色）。
#[gpui::test]
fn code_block_bg_matches_theme_palette(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    // 浅色主题
    cx.update(|cx| crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Light, cx));
    let light = cx.update(|cx| StyleParams::from_theme_with_font(cx, None).code_bg);
    assert_eq!(
        light,
        gpui::rgb(0xf5f5f5).into(),
        "亮主题代码块背景应为 #F5F5F5"
    );
    // 深色主题
    cx.update(|cx| crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Dark, cx));
    let dark = cx.update(|cx| StyleParams::from_theme_with_font(cx, None).code_bg);
    assert_eq!(
        dark,
        gpui::rgb(0x262626).into(),
        "暗主题代码块背景应为 #262626"
    );
    // 恢复浅色（主题为全局共享，避免影响后续测试）
    cx.update(|cx| crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Light, cx));
}

/// 行内代码（反引号）pill 配色单一源：亮 #F0F0F0/#202020、暗 #3A3A3A/#E8E8E8——
/// 预览与 WYSIWYG 都取 [`StyleParams::code_inline_bg`/`code_inline_fg`]（WYSIWYG 侧
/// 在 `theme_mirrors_style_params_single_source` 验证抄自同源）。
#[gpui::test]
fn inline_code_palette_single_source(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    cx.update(|cx| crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Light, cx));
    let light = cx.update(|cx| StyleParams::from_theme_with_font(cx, None));
    assert_eq!(light.code_inline_fg, gpui::rgb(0x202020).into(), "亮主题行内代码文字");
    assert_eq!(light.code_inline_bg, gpui::rgb(0xf0f0f0).into(), "亮主题行内代码底");
    cx.update(|cx| crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Dark, cx));
    let dark = cx.update(|cx| StyleParams::from_theme_with_font(cx, None));
    assert_eq!(dark.code_inline_fg, gpui::rgb(0xe8e8e8).into(), "暗主题行内代码文字");
    assert_eq!(dark.code_inline_bg, gpui::rgb(0x3a3a3a).into(), "暗主题行内代码底");
    cx.update(|cx| crate::ui::theme::Theme::set(crate::ui::theme::ThemeMode::Light, cx));
}

/// 源码行 → 渲染行映射（EPUB 搜索跳转）：标题/空行/段落各占一渲染行，
/// 代码围栏折叠成单个渲染行覆盖多源码行，命中的源码行应定位到对应渲染行。
#[gpui::test]
fn row_for_line_maps_source_lines(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 行 0 标题 / 1 空 / 2 正文一 / 3 正文二 / 4 空 / 5-8 代码围栏 / 9 空
    let src = "# 标题\n\n正文一\n正文二\n\n```\ncode1\ncode2\n```\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(src, cx));
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    settle(cx);

    let md = cx.read(|cx| host.read(cx).md.clone());
    let at = |line: usize| cx.read(|cx| md.read(cx).row_for_line(line));
    assert_eq!(at(2), Some(2), "源码第 2 行（正文一）应映射到渲染行 2");
    assert_eq!(at(3), Some(3), "源码第 3 行（正文二）应映射到渲染行 3");
    assert_eq!(at(6), Some(5), "代码围栏内第 6 行应定位到折叠的代码渲染行");
    assert_eq!(at(8), Some(5), "代码围栏末行仍属折叠代码行");
    assert_eq!(at(0), Some(0), "标题行映射到渲染行 0");
    // 空预览 → None
    let (host2, cx2) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let row = cx2.update(|_, _cx| host2.read(_cx).md.read(_cx).row_for_line(0));
    assert_eq!(row, None, "空预览无渲染行 → None");
}



/// 搜索跳转后：只给命中**文本**临时高亮（不吃整行），
/// 时钟推进超过 [`HIGHLIGHT_MS`] 后自动消失。
#[gpui::test]
fn search_jump_highlight_appears_and_clears(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 行 0 标题 / 1 空 / 2 正文一 / 3 空 / 4 正文二（含关键字）/ 5 空
    let src = "# 标题\n\n正文一\n\n正文二，包含关键字\n";
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(src, cx));
    });
    cx.simulate_resize(size(px(400.), px(400.)));
    settle(cx);

    // 高亮源码第 4 行（正文二）内的搜索词，只覆盖命中文本、不整行
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.highlight_search(4, "关键字", cx);
            cx.notify();
        });
    });
    for _ in 0..5 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }

    let (row_ix, rng) = cx
        .read(|cx| host.read(cx).md.read(cx).search_highlight_range())
        .expect("跳转后应定位到命中文本");
    assert_eq!(row_ix, 4, "正文二应映射到渲染行 4");
    let text = "正文二，包含关键字";
    assert_eq!(&text[rng.clone()], "关键字", "高亮区间应只覆盖搜索词");
    assert!(rng.len() < text.len(), "不应高亮整行（区间小于整行长度）");

    // 时钟推进超过高亮停留时长后，高亮应自动消失
    cx.executor().advance_clock(std::time::Duration::from_millis(HIGHLIGHT_MS + 500));
    for _ in 0..5 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }
    assert!(
        cx.read(|cx| host.read(cx).md.read(cx).search_highlight_range())
            .is_none(),
        "超时后高亮应自动消失"
    );
}

/// 回归：竖滚动条量程在首次渲染后由「平均行高 × 行数」铺设到接近真实总高，
/// 而不是只算可见行（否则滚动条一开始拖动不了全文）。这是确定性断言：
/// `update_height_hint` 在无布局（summary=0）时会把统一高度提示填满整列表。
#[gpui::test]
fn uniform_hint_seeds_full_scroll_range(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let mut md = cx.update(MarkdownPreview::new);
    let mut src = String::new();
    // 多种行型混合 + 一个大代码块：若平均行高漏算代码块，量程会严重偏小
    src.push_str("```rs\n");
    for i in 0..200 {
        src.push_str(&format!("let x = {i};\n"));
    }
    src.push_str("```\n\n");
    for i in 0..3000 {
        src.push_str(&match i % 5 {
            0 => format!("## 标题 {i} 内容内容内容内容内容内容内容内容内容内容内容内容\n"),
            1 => format!("- 列表项 {i} 内容内容内容内容内容内容内容内容内容内容内容内容\n"),
            2 => format!("普通段落行 {i} 内容内容内容内容内容内容内容内容内容内容内容内容\n"),
            3 => format!("> 引述内容 {i} 内容内容内容内容内容内容内容内容内容内容\n"),
            _ => format!("正文 {i}\n"),
        });
    }
    cx.update(|cx| md.set_source(&src, cx));
    let (n, hint, max_off) = cx.update(|cx| {
        let changed = md.update_height_hint(cx, 490.0);
        assert!(changed, "首次无布局（summary=0）时应触发一次重填");
        (
            md.rows.len(),
            md.est_hint,
            md.list_state.max_offset_for_scrollbar().y.as_f32(),
        )
    });
    assert!(n > 0, "应有渲染行");
    assert!(hint > 20.0, "平均行高应超过单行正文高，实际={hint:.1}");
    // 量程 = 行数 × 平均行高（尚未布局，无视口扣除）；应接近估算总高
    let expected = n as f32 * hint;
    assert!(
        (max_off - expected).abs() < expected * 0.05,
        "滚动条量程应接近 行数×平均行高：max_off={max_off:.0} expected={expected:.0} hint={hint:.1}"
    );
}

/// 回归：#3 换宽节流——宽度连续变化（拖分屏）时，O(块数) 的统一高度重建至多每
/// [`HINT_REAPPLY_INTERVAL_MS`] 一次，拖动期间不会每帧卡一下。首次建立不节流。
#[gpui::test]
fn hint_reapply_throttle_on_rapid_width_change(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let mut md = cx.update(MarkdownPreview::new);
    // 换行（wrap）文档：不同视口宽 → 折行数不同 → 平均行高不同 → 每次换宽都要重建提示
    let mut src = String::new();
    for i in 0..1200 {
        src.push_str(&format!("# 标题 {i} 内容内容内容内容内容内容内容内容内容内容\n正文段落 {i}\n\n"));
    }
    cx.update(|cx| md.set_source(&src, cx));

    let first = cx.update(|cx| md.update_height_hint(cx, 480.0));
    assert!(first, "首次（无提示记录）应立即建立，不节流");
    let mut applied = 0u8;
    for w in [481.0, 483.0, 486.0, 490.0, 495.0] {
        applied += cx.update(|cx| md.update_height_hint(cx, w)) as u8;
    }
    assert!(
        applied <= 1,
        "120ms 内连续换宽至多重建 1 次（其余被节流），实际 {applied}"
    );
}

/// 回归：#3 打字路径——源更新（块数不变）时只重测**可见窗口内**的行高，不再全量
/// remeasure 整棵 O(块数) SumTree。断言：块数不变时 ListState 的 item_count 不变、
/// 统一高度提示未被破坏（量程仍在），且窗口外未物化行保持占位。
#[gpui::test]
fn source_reapply_same_count_only_remeasures_window(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let block = "```rust\nfn f() {}\n```\n\n正文 `code`\n\n";
    let mut src = String::new();
    for _ in 0..1200 {
        src.push_str(block);
    }
    let index = std::rc::Rc::new(crate::markdown::scan_blocks(&src));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(PreviewSrc::from(SharedString::from(src.clone())), index, cx)
        });
    });
    cx.simulate_resize(size(px(600.), px(600.)));
    settle(cx);
    let n0 = cx.update(|_, cx| host.read(cx).md.read(cx).item_count());

    // 块数不变的小编辑（改一个字符再同步）
    let mut src2 = src.clone();
    src2 = src2.replacen("正文 `code`", "正文！`code`", 1);
    let index2 = std::rc::Rc::new(crate::markdown::scan_blocks(&src2));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(PreviewSrc::from(SharedString::from(src2)), index2, cx);
            cx.notify();
        });
    });
    for _ in 0..4 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }

    let (n1, win_len) = cx.update(|_, cx| {
        let md = host.read(cx).md.read(cx);
        (md.item_count(), md.win_rows.len())
    });
    assert_eq!(n1, n0, "块数不变时列表项数应保持不变（不再整棵重建）");
    assert!(
        win_len > 0 && win_len <= 200,
        "窗口行数应保持有界（win_len={win_len}）"
    );
    cx.simulate_resize(size(px(600.), px(600.)));
    settle(cx);
}

/// 大文档（块数 > WINDOWED_MIN_BLOCKS）必须走「窗口级懒解析」：
/// - 不物化整篇（win_rows 远小于块数、memory_estimate 有界）
/// - 渲染不 panic、可见窗口内有真实行
/// - 目录齐全、row_for_line 基于块索引可查、滚动到深处会移动窗口
#[gpui::test]
fn large_doc_uses_windowed_lazy_parse(cx: &mut TestAppContext) {
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    // 每轮迭代 8 块：Heading/Blank/Paragraph/Blank/Bullet/Blank/Code/Blank
    let iters = WINDOWED_MIN_BLOCKS / 2 + 400;
    let mut src = String::new();
    for i in 0..iters {
        src.push_str(&format!("# 标题{i}\n\n正文段落 {i}\n\n- item\n\n```rs\nlet a = {i};\n```\n\n"));
    }
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| md.set_source(&src, cx));
    });
    cx.simulate_resize(size(px(400.), px(600.)));
    settle(cx);

    let (n_items, win_lo, win_len, mem, mid_row, toc_n) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        (
            md.item_count(),
            md.win_lo,
            md.win_rows.len(),
            md.memory_estimate(),
            md.row_for_line(src.lines().count() / 2),
            md.toc.len(),
        )
    });
    assert!(
        n_items > WINDOWED_MIN_BLOCKS,
        "应启用窗口模式（块数={n_items} 应 > 阈值）"
    );
    assert!(win_len > 0, "渲染后应已物化窗口行");
    assert!(
        win_len < 200,
        "窗口行数应远小于全文（win_len={win_len}，全文 {n_items}）"
    );
    assert!(
        win_lo < 30,
        "初始窗口应从顶部附近开始，win_lo={win_lo}"
    );
    assert!(
        mem.1 < 1_000_000,
        "窗口模式驻留应该有界（{mem:?} 应 < 1MB）"
    );
    assert!(mid_row.is_some(), "row_for_line 应基于块索引可查（中部）");
    assert_eq!(toc_n, iters, "目录应由块索引全量提供");

    // 用像素滚动（scroll_vertical＝滚轮同款路径）连续下滚：render 每帧调度
    // ensure_window，窗口应随可见范围移动（固定窗口虚拟化下 `scroll_vertical`
    // 直接驱动窗口重定位，无「到达已布局窗口」的旧限制）。
    for _ in 0..30 {
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, view_cx| {
                md.scroll_vertical(-300.);
                view_cx.notify();
            });
        });
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }
    for _ in 0..3 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }
    let (scroll_top, win_lo2, win_len2) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        (
            md.abs_scroll_top().0,
            md.win_lo,
            md.win_rows.len(),
        )
    });
    assert!(
        scroll_top > 100,
        "像素滚动应将绝对可见块下移，abs_top_block={scroll_top}"
    );
    assert!(
        win_lo2 > 50,
        "窗口应随滚动下移，win_lo={win_lo2}（abs_top={scroll_top}）"
    );
    assert!(
        win_lo2 <= scroll_top && scroll_top < win_lo2 + win_len2 + 64,
        "窗口应覆盖可见范围，win_lo={win_lo2} len={win_len2} abs_top={scroll_top}"
    );
    assert!(win_len2 < 200, "任一时刻窗口应保持有界，win_len={win_len2}");
}

/// 回归：窗口模式 **增量窗口扩展**（拖滚动条/慢滚只解析新进入的带头/尾带、拼接复用
/// 既有行）必须与「整窗重解析」逐行一致——防拼接错位/丢行/顺序颠倒。用与
/// `prepare_render` 同步的目标序列驱动（apply_desired + ensure_window 交替前进），
/// 几次相邻重叠的重建后，把最终窗口文本与一次性整窗重解析对拍。
#[gpui::test]
fn windowed_incremental_window_matches_full_reparse(cx: &mut TestAppContext) {
    use crate::markdown::parse::parse_block_window_rope;
    use ropey::Rope;
    cx.update(crate::actions::bind_local_keymaps);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let md = cx.new(|cx| MarkdownPreview::new(cx));
        PreviewHost { md }
    });
    let cx: &mut VisualTestContext = cx;
    let iters = crate::config::limits::WINDOWED_MIN_BLOCKS + 500;
    let mut src = String::new();
    for i in 0..iters {
        src.push_str(&format!(
            "# 标题{i}\n\n正文段落 {i}，含一些文字。\n\n- 列表 {i}\n\n```rs\nlet a = {i};\n```\n\n"
        ));
    }
    let rope0 = Rope::from_str(&src);
    let idx0 = std::rc::Rc::new(crate::markdown::scan_blocks_rope(&rope0));
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, cx| {
            md.set_source_with_index(crate::markdown::PreviewSrc::Rope(rope0.clone()), idx0.clone(), cx)
        });
    });
    cx.simulate_resize(size(px(980.), px(760.)));
    settle(cx);
    assert!(
        cx.update(|_, cx| host.read(cx).md.read(cx).index.is_some()),
        "应启用窗口模式"
    );

    // 用一家家重叠的小步前进驱动多次增量重建（模拟慢速拖滚动条），覆盖
    // 「仅尾带追加」「仅首带前插」「首尾双伸」三种拼接分支。
    cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        md.update(cx, |md, _cx| {
            let view = md.list_state.viewport_bounds().size.height.as_f32();
            let hint = md.est_hint.max(20.0);
            let step = 18.0 * hint;
            let total = md.total_h();
            let mut target = 0.0f32;
            let n = md.item_count();
            let last = n.saturating_sub(1);
            // 依次推进，每次按 prepare_render 同口径建房（apply + ensure 交替，
            // 覆盖「仅尾带」「仅首带」「首尾双伸」三种增量拼接分支）。
            while target < (total - view).max(0.0) {
                target += step;
                md.desired_scroll.set(target);
                md.restore_zoom_anchor();
                md.apply_desired(md.desired_scroll.get());
                let (cur_lo_vis, _off) = md.abs_scroll_top();
                let h = md.est_hint.max(20.0);
                let vis_base = ((view / h).ceil() as usize).max(1);
                md.overdraw_margin = adaptive_overdraw(vis_base);
                let vis_n = vis_base.saturating_add(md.overdraw_margin);
                let need_lo = cur_lo_vis.saturating_sub(md.overdraw_margin);
                let need_hi = cur_lo_vis.saturating_add(vis_n).min(last);
                md.ensure_window(need_lo, need_hi);
            }
        });
    });
    // 对拍：最终窗口 [win_lo, +len) 与整窗重解析逐行文本一致。
    let (win_lo, win_rows, ref_rows) = cx.update(|_, cx| {
        let md = host.read(cx).md.clone();
        let md = md.read(cx);
        let lo = md.win_lo;
        let hi = lo.saturating_add(md.win_rows.len()).saturating_sub(1);
        let ref_rows = parse_block_window_rope(&rope0, &idx0, lo, hi);
        (lo, md.win_rows.clone(), ref_rows)
    });
    let join = |rs: &[crate::markdown::parse::Row]| -> String {
        rs.iter().map(|r| r.text.as_ref()).collect::<Vec<_>>().join("\n")
    };
    assert!(!win_rows.is_empty(), "应有窗口行");
    assert_eq!(
        join(&win_rows),
        join(&ref_rows),
        "增量拼接窗口（win_lo={win_lo}）应与整窗重解析逐行一致"
    );
}

    /// 预览与编辑器共享源 Rope（第二步）：
    /// - 窗口模式 `set_source_with_index(PreviewSrc::Rope(editor_rope))` 后，内部
    ///   `src_buf` 与传入 rope **共享同一棵 Arc 树**（ropey 2.0 `Rope` 是 Arc 树、
    ///   `Clone` O(1)）——大 md 预览不常驻「+1× 源文本」（那是共享句柄，不是副本）；
    /// - COW 语义：此后编辑原 rope（用户打字），预览持有的快照内容不受影响
    ///   （ropey 沿编辑路径 `Arc::make_mut`，未编辑子树仍共享）。
    #[gpui::test]
    fn windowed_preview_shares_editor_rope(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let cx: &mut VisualTestContext = cx;
        // 窗口模式的源文本（块数 > WINDOWED_MIN_BLOCKS）。
        use ropey::Rope;
        let iters = crate::config::limits::WINDOWED_MIN_BLOCKS + 600;
        let mut src = String::new();
        for i in 0..iters {
            src.push_str(&format!("# 标题 {i}\n\n正文段落 {i} *强调*\n\n```rs\nlet a = {i};\n```\n\n"));
        }
        let mut editor_rope = Rope::from(src.as_str());
        let snapshot = editor_rope.clone();
        let index = std::rc::Rc::new(crate::markdown::scan_blocks_rope(&snapshot));
        assert!(
            index.blocks.len() > crate::config::limits::WINDOWED_MIN_BLOCKS,
            "测试应走窗口模式"
        );
        cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.set_source_with_index(PreviewSrc::Rope(snapshot.clone()), index, cx);
            });
        });
        cx.simulate_resize(size(px(400.), px(600.)));
        settle(cx);

        let shared = cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.read(cx).test_src_rope_shared_with(&editor_rope)
        });
        assert!(
            shared,
            "预览源码应与编辑器共享同一棵 Rope（O(1) 克隆；false = 被物化成副本）"
        );

        // COW：编辑编辑器 rope（模拟打字），预览持有的快照内容仍等于原文。
        editor_rope.insert(editor_rope.len(), "\n末尾追加");
        assert_ne!(editor_rope.to_string(), src, "编辑器 rope 应已分叉");
        let src_text = cx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.read(cx).test_src_text()
        });
        assert_eq!(
            src_text.as_deref(),
            Some(src.as_str()),
            "预览快照在编辑器编辑后应保持原内容（COW，不受影响）"
        );
        // 摆动：预编译器不 panic；其余不深究（本测试只负责共享语义）。
        cx.run_until_parked();
        let _ = cx.cx.refresh();
    }

    /// EPUB 阅读排版端到端：标题文本含注记、正文 `class=text` 首行缩进 2 全角空格、
    /// 落款行右对齐、`[n]` 注记悬停浮现注释卡片。
    #[gpui::test]
    fn epub_note_hover_shows_annotation_card(cx: &mut TestAppContext) {
        use crate::markdown::parse::RowAlign;
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let src = "# 1872年德文版序言[^1]\n\n　　共产主义者同盟[^2]成立于1847年。\n\n%%ALIGN:RIGHT%%卡尔·马克思　弗里德里希·恩格斯\n";
        let mut notes = std::collections::BTreeMap::new();
        notes.insert(1, "这是第一条注释内容。".to_string());
        notes.insert(2, "这是第二条注释内容。".to_string());
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_epub(true, notes, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(700.), px(500.)));
        settle(cx);

        // 行数据语义：标题含注记 [1]；正文首行缩进「　　」且含 [2]；落款右对齐。
        let (heading_ok, body_ok, sig_align) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            let h = md.rows.iter().find(|r| matches!(r.kind, RowKind::Heading(_))).expect("有标题");
            let body = md
                .rows
                .iter()
                .find(|r| r.text.contains("共产主义者同盟"))
                .expect("有正文段");
            let sig = md
                .rows
                .iter()
                .find(|r| r.text.contains("卡尔·马克思"))
                .expect("有落款行");
            (
                h.text.contains("[1]") && h.notes.iter().any(|(_, n)| *n == 1),
                body.text.starts_with("\u{3000}\u{3000}") && body.notes.iter().any(|(_, n)| *n == 2),
                sig.align,
            )
        });
        assert!(heading_ok, "标题应含注记 [1]");
        assert!(body_ok, "正文应首行缩进且含注记 [2]");
        assert_eq!(sig_align, Some(RowAlign::Right), "落款应右对齐");

        // 命中表：paint 后应有注记矩形（文内引用 + 末尾定义都会有命中）。
        let hits: Vec<(usize, bool, gpui::Bounds<gpui::Pixels>)> = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            md.note_ctx
                .hits
                .borrow()
                .iter()
                .map(|h| (h.n, h.is_def, h.bounds))
                .collect()
        });
        assert!(!hits.is_empty(), "应有注记命中, 实际 {hits:?}");
        let (n, _is_def, b) = hits[0];
        let c = b.center();

        // 鼠标悬停到注记 → 悬浮注记命中置位 + 卡片出现。
        cx.simulate_mouse_move(point(px(c.x.as_f32()), px(c.y.as_f32())), None, Modifiers::default());
        settle(cx);
        let hover = cx.update(|_, cx| host.read(cx).md.read(cx).hover_hit.map(|h| h.n));
        assert_eq!(hover, Some(n), "悬停在注记 {n} 上应置 hover_hit");
        assert!(cx.debug_bounds("md-note-card").is_some(), "悬浮注释卡片应出现");

        // 移开注记 → 卡片消失。
        cx.simulate_mouse_move(point(px(10.), px(10.)), None, Modifiers::default());
        settle(cx);
        let hover = cx.update(|_, cx| host.read(cx).md.read(cx).hover_hit.map(|h| h.n));
        assert_eq!(hover, None, "移开注记应清除 hover");
    }

    /// 引用标记**双向跳转**：点击文内 `[n]` → 定位末尾定义 `[!n]` 并加亮；再点定义 → 跳回文内引用。
    #[gpui::test]
    fn click_citation_marker_navigates_both_ways(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        // 全量小文档：标题带文内引用 [^1]，末尾是定义 [1]。
        let src = "# 标题[^1]\n\n正文引用[^1]处。\n\n[!1]　引用一详情。\n";
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(600.), px(400.)));
        settle(cx);

        // 行结构：0=标题([^1]),1=空行,2=正文([^1]),3=空行,4=定义[!1]。
        let (ref1_row, def1_row) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            let ref1 = md
                .rows
                .iter()
                .position(|r| r.notes.iter().any(|(_, n)| *n == 1))
                .expect("有文内引用");
            let def1 = md
                .rows
                .iter()
                .position(|r| r.note_defs.iter().any(|(_, n)| *n == 1))
                .expect("有定义");
            (ref1, def1)
        });
        assert_eq!((ref1_row, def1_row), (0, 4), "文内引用应在标题行、定义在第 4 行");

        // 点击文内引用 [1] → 定位到定义行并加亮定义标记（is_def=true）。
        let ref1_center = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            md.note_ctx
                .hits
                .borrow()
                .iter()
                .find(|h| h.n == 1 && !h.is_def)
                .copied()
                .expect("应有文内引用命中")
                .bounds
                .center()
        });
        cx.simulate_mouse_down(ref1_center, MouseButton::Left, Modifiers::default());
        settle(cx);
        let (hl, sel, def_hit) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            let def_hit = md
                .note_ctx
                .hits
                .borrow()
                .iter()
                .any(|h| h.n == 1 && h.is_def);
            (md.marker_hl, md.selection, def_hit)
        });
        assert_eq!(hl, Some((def1_row, 1, true)), "点文内引用应定位到定义并加亮定义标记");
        assert!(!sel.is_some(), "点标记不应进入文本点选");
        assert!(def_hit, "定义行仍在视口内、定义命中仍可查");

        // 反向：点击定义 [1] → 跳回文内引用行并加亮引用标记（is_def=false）。
        let def1_center = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            md.note_ctx
                .hits
                .borrow()
                .iter()
                .find(|h| h.n == 1 && h.is_def)
                .copied()
                .expect("应有定义命中")
                .bounds
                .center()
        });
        cx.simulate_mouse_down(def1_center, MouseButton::Left, Modifiers::default());
        settle(cx);
        let hl = cx.update(|_, cx| host.read(cx).md.read(cx).marker_hl);
        assert_eq!(hl, Some((ref1_row, 1, false)), "点定义应跳回文内引用并加亮引用标记");
    }

    /// 非 EPUB 的普通 markdown：悬浮在引用标记 `[n]` 上也应浮现该引用的详情正文
    /// （取自定义 `[!n]` 行剥掉 `[n]` 标记后的文本，`cit.details`）。
    #[gpui::test]
    fn hover_citation_marker_shows_detail_text_without_epub(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let src = "# 引言[^1]\n\n[!1]　这是引用 1 的详情介绍。\n";
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(600.), px(400.)));
        settle(cx);

        // 悬浮文内引用 [1]（标题行）→ 详情卡片浮现。
        let c = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            md.note_ctx
                .hits
                .borrow()
                .iter()
                .find(|h| h.n == 1 && !h.is_def)
                .copied()
                .expect("引用命中")
                .bounds
                .center()
        });
        cx.simulate_mouse_move(c, None, Modifiers::default());
        settle(cx);
        let (hover, detail) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            (
                md.hover_hit.map(|h| h.n),
                md.cit.details.get(&1).map(|s| s.to_string()),
            )
        });
        assert_eq!(hover, Some(1), "悬停应在引用 1 上");
        assert_eq!(
            detail.as_deref(),
            Some("这是引用 1 的详情介绍。"),
            "非 EPUB 悬浮也应能取到引用详情正文"
        );
        assert!(
            cx.debug_bounds("md-note-card").is_some(),
            "非 EPUB 悬停引用也应出现详情卡片"
        );

        // 移开 → 卡片消失。
        cx.simulate_mouse_move(point(px(10.), px(10.)), None, Modifiers::default());
        settle(cx);
        let hover = cx.update(|_, cx| host.read(cx).md.read(cx).hover_hit.map(|h| h.n));
        assert_eq!(hover, None, "移开应清除悬浮");
        assert!(
            cx.debug_bounds("md-note-card").is_none(),
            "移开引用后详情卡片应消失"
        );
    }

    /// 相邻引述渲染行应**上下紧贴**（无外层纵向外衬）：整段引述呈一整片灰底，
    /// 无白色分割。单个引述文本内部折行由带内 `py_1` 提供行距。
    #[gpui::test]
    fn consecutive_quotes_render_as_one_band(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let src = "> 巴枯宁翻译的《共产党宣言》俄文第一版\n> 现在来看看俄国吧！\n> 卡·马克思　弗·恩格斯";
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(700.), px(500.)));
        settle(cx);

        let (kinds, tops, heights) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            let kinds: Vec<_> = md.rows.iter().map(|r| r.kind).collect();
            let mut tops = Vec::new();
            let mut heights = Vec::new();
            for i in 0..md.rows.len() {
                let b = md.list_state.bounds_for_item(i).expect("行应有 bounds");
                tops.push(b.origin.y.as_f32());
                heights.push(b.size.height.as_f32());
            }
            (kinds, tops, heights)
        });
        assert!(
            kinds.iter().all(|k| *k == RowKind::Quote),
            "三行都应是引述：{kinds:?}"
        );
        // 相邻引述行上下紧贴：row(i+1).top == row(i).top + row(i).height（±0.01 亚像素）。
        for i in 0..tops.len() - 1 {
            let gap = tops[i + 1] - (tops[i] + heights[i]);
            assert!(
                gap.abs() < 0.01,
                "引述行 {i} 与 {i}+1 应无白色分割（gap={gap:.3}px）"
            );
        }
    }

    /// 鼠标拖动选择 + Ctrl/Cmd+C 复制：跨两段正文拖选 → selection 覆盖两行，
    /// 复制文本含两段内容并以换行连接。
    #[gpui::test]
    fn mouse_drag_selects_text_and_copies(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let src = "# 标题\n\n第一段正文内容足够长以便折行测试鼠标选择。\n\n第二段正文内容也足够用来拖选。\n";
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(700.), px(500.)));
        settle(cx);

        // 找两个正文段的行框（0=标题,2=第一段,4=第二段），在其行框内按下并拖到第二段。
        let (b0, b2) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            (
                md.list_state.bounds_for_item(2).expect("第一段 bounds"),
                md.list_state.bounds_for_item(4).expect("第二段 bounds"),
            )
        });
        let p0 = point(px(b0.origin.x.as_f32() + 100.0), px(b0.center().y.as_f32()));
        let p2 = point(px(b2.origin.x.as_f32() + 100.0), px(b2.center().y.as_f32()));
        cx.simulate_mouse_down(p0, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(p2, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(p2, MouseButton::Left, Modifiers::default());
        settle(cx);

        let (sel, text) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            (md.selection, md.selected_text())
        });
        let sel = sel.expect("拖选后应有 selection");
        assert_eq!(sel.0 .0, 2, "选择应从第一段开始（实际 {sel:?}）");
        assert_eq!(sel.1 .0, 4, "选择应到第二段结束（实际 {sel:?}）");
        let text = text.expect("应有选中文本");
        assert!(text.contains("正文内容"), "选中文本应含两段公共子串：{text:?}");
        assert!(text.contains('\n'), "跨行选择应以换行连接：{text:?}");

        // 复制到剪贴板。
        cx.update(|window, cx| {
            md.update(cx, |md, cx| md.copy_selection(window, cx));
        });
        let cb = cx
            .cx
            .read_from_clipboard()
            .and_then(|i| i.text())
            .map(|t| t.to_string());
        let cb = cb.expect("复制后剪贴板应有内容");
        assert!(
            cb.contains("正文内容") && cb.contains('\n'),
            "剪贴板应含两段公共子串并以换行连接：{cb:?}"
        );

        // 点空白处清除选择。
        cx.simulate_mouse_down(point(px(690.), px(10.)), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(
            point(px(690.), px(10.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        settle(cx);
        let sel = cx.update(|_, cx| host.read(cx).md.read(cx).selection);
        assert_eq!(sel, None, "点空白应清除选择");
    }

    #[gpui::test]
    fn non_char_boundary_selection_over_cjk_does_not_panic(cx: &mut TestAppContext) {
        // 回归：选区字节偏移切进多字节字符（CJK `。`）时，旧代码 `push_bg` 直接
        // 用该区间喂 StyledText → shaping 时 run 边界非字符边界 → direct_write
        // panic（byte index not a char boundary）。修复：`char_align_range` 吸收。
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let src = "# 标题\n\n你说得对，好。行不行。\n";
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(700.), px(500.)));
        settle(cx);
        // 注入选区：终点 19 落在 `。`（字节 18..21）中间——旧代码渲染即崩。
        let mid = "你说得对，好。".find('。').unwrap() + 1;
        assert_eq!(mid, 19, "句号起于 18，切半偏移 19");
        cx.update(|window, cx| {
            let md2 = host.read(cx).md.clone();
            md2.update(cx, |md, cx| {
                md.selection = Some(((2, 0), (2, mid)));
                // 复制路径的 `&text[a..b]` 切片也必须防御非字符边界（旧代码在此崩）。
                md.copy_selection(window, cx);
                cx.notify();
            });
        });
        // 渲染带该选区高亮的行 + 复制：均不 panic 即修复生效。
        settle(cx);
        let text = cx.update(|_, cx| host.read(cx).md.read(cx).selected_text());
        assert_eq!(
            text.as_deref(),
            Some("你说得对，好。"),
            "非边界偏移向前对齐到完整字符，且不 panic"
        );
    }

    #[gpui::test]
    fn select_text_works_on_inline_code_row(cx: &mut TestAppContext) {
        // 回归：含行内代码（反引号）的行**可以**鼠标拖选字符。旧实现把 StyledText
        // 移进 RoundedInlineText（st=None），不写文本命中表 → hit_test 找不到该行，
        // 选区建立不了。现统一 TextTrack 始终持有 st，两类行一致可点选。
        cx.update(crate::actions::bind_local_keymaps);
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let src = "# 标题\n\n我们看看`行内代码`能不能拖选出来。\n\n第二段正文足够用来拖选。\n";
        let md = cx.update(|_, cx| host.read(cx).md.clone());
        md.update(cx, |md, cx| {
            md.set_wrap(true, cx);
            md.set_source(src, cx);
        });
        cx.simulate_resize(size(px(700.), px(500.)));
        settle(cx);
        let (b1, b2) = cx.update(|_, cx| {
            let md = host.read(cx).md.read(cx);
            (
                md.list_state.bounds_for_item(2).expect("含代码段"),
                md.list_state.bounds_for_item(4).expect("第二段"),
            )
        });
        // 混入 ListState cells 引用。
        let p0 = point(px(b1.origin.x.as_f32() + 60.0), px(b1.center().y.as_f32()));
        let p2 = point(px(b2.origin.x.as_f32() + 60.0), px(b2.center().y.as_f32()));
        cx.simulate_mouse_down(p0, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(p2, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(p2, MouseButton::Left, Modifiers::default());
        settle(cx);
        let sel = cx.update(|_, cx| host.read(cx).md.read(cx).selection);
        let sel = sel.expect("含行内代码的行应可拖选");
        assert_eq!(sel.0 .0, 2, "选择应从含代码段开始（实际 {sel:?}）");
        let text = cx.update(|_, cx| host.read(cx).md.read(cx).selected_text());
        assert!(
            text.as_deref().is_some_and(|t| t.contains("代码")),
            "选中文本应含代码段内文字：{text:?}"
        );
    }

    /// 基准（--ignored）：源码编辑区每键 → 预览同步（source+预览链）的确定性拆账。
    /// 真实 `fixtures/大文档测试2.md`（窗口模式），在一个 `md.update` 闭包内循环
    /// N 轮「rope 插字符 + 重扫 + set_source_with_index + prepare_render（窗口路径）」，
    /// 杜绝 gpui harness 每轮 update 开销污染；各段耗时由 `prof::` 标签累计、dump 输出
    /// 每轮均摊：md.set_source / md.ss.toc / md.ss.reset / md_hint / md.hm.rebuild /
    /// md_ensure_window。打印、不断言；`--ignored` 运行。
    #[gpui::test]
    #[ignore]
    fn bench_preview_edit_sync_stage_cost(cx: &mut TestAppContext) {
        use std::time::Instant;
        crate::prof::enable();
        cx.update(crate::actions::bind_local_keymaps);
        let path = "fixtures/大文档测试2.md";
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[bench] skip realfile: {e}");
                return;
            }
        };
        let (host, vcx) = cx.add_window_view(|_window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            PreviewHost { md }
        });
        let vcx: &mut VisualTestContext = vcx;
        let rope0 = ropey::Rope::from_str(&text);
        let idx0 = std::rc::Rc::new(crate::markdown::scan_blocks_rope(&rope0));
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |md, cx| {
                md.set_source_with_index(crate::markdown::PreviewSrc::Rope(rope0), idx0.clone(), cx);
            });
        });
        vcx.simulate_resize(size(px(980.), px(760.)));
        settle(vcx);
        let n_items = vcx.update(|_, cx| host.read(cx).md.read(cx).item_count());
        assert!(
            n_items > crate::config::limits::WINDOWED_MIN_BLOCKS,
            "需窗口模式（块数 {n_items}）"
        );

        let p = vcx.update(|_window, cx| Rc::new(StyleParams::from_theme_with_font(cx, None)));
        let n = 120usize;
        let t0 = Instant::now();
        let mut rope = ropey::Rope::from_str(&text);
        // 增量基底（镜像 app 的 `preview_prev`）：保留上一份 rope 快照 + 块索引，
        // 每键用 `scan_blocks_incremental_rope`（与 app/mod.rs 后台路径同式）——
        // 尾部分段共享 ⇒ 换源 drop 上一份索引只是 O(1) 减引用，不被 3.2MB 释放伪影污染。
        let mut prev_rope = rope.clone();
        let mut prev_idx = idx0.clone();
        let at = n_items / 2;
        let mut scans_ms = 0.0f64;
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |m, cx| {
                for i in 0..n {
                    // 把「单字符输入」建模在文档中部附近几个点轮转插入（近似逐键但避免
                    // 让同一段无限增长）。扫描是 app 后台职责，这里仅计时作参照。
                    let len = rope.len_chars();
                    let off_chars = ((at + (i % 12) * 2) % len.max(1)).min(len) as usize;
                    let off = rope.char_to_byte_idx(off_chars.min(rope.len_chars()));
                    let ts = Instant::now();
                    rope.insert(off, "x");
                    let idx = std::rc::Rc::new(crate::markdown::scan_blocks_incremental_rope(
                        &prev_rope,
                        &prev_idx,
                        &rope,
                        Some(off..off + 1),
                    ));
                    scans_ms += ts.elapsed().as_secs_f64() * 1000.0;
                    prev_rope = rope.clone();
                    prev_idx = idx.clone();
                    let src = crate::markdown::PreviewSrc::Rope(rope.clone());
                    m.set_source_with_index(src, idx, cx);
                    m.prepare_render(&p, 980.0, cx);
                }
            });
        });
        let wall = t0.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "[bench] 预览每键同步（窗口模式，{n} 轮，真实大文档，增量扫描+保留预览基底）：总 {}ms（{:.2}ms/轮）；其中消费端扫描（不含在预览内）{:.2}ms/轮；预览侧（set_source+prepare_render）{:.2}ms/轮",
            wall,
            wall / n as f64,
            scans_ms / n as f64,
            (wall - scans_ms) / n as f64,
        );
        // prof::dump 重新计算每标签总耗时（不含扫描），给出每轮均摊的精确拆账。
        crate::prof::dump("preview.edit.sync");
    }

    /// 基准（--ignored）：**合并决策的跨管线对比**——同一个大文档（真实 6.4MB，
    /// 窗口模式）上，`MarkdownPreview`（元素树 + ListState）vs `WyEditor` 只读等价
    /// 渲染（自绘 build_frame 带内整形）的「首帧 / 滚动重定位」成本。
    /// 目的（#4 统一文档层）：证明把预览迁到 WYSIWYG 自绘管线（`editable` 门控只读态）
    /// 后，只读查看的打开/滚动不劣于旧预览元素树；给合并的每一步提供 before/after 锚点。
    /// `scan_blocks_rope` 只作索引构建参照。打印、不断言；`--ignored` 运行。
    #[gpui::test]
    #[ignore]
    fn bench_preview_vs_wy_readonly(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::{WyEditor, build_frame};
        use std::time::Instant;
        crate::prof::enable();
        cx.update(crate::actions::bind_local_keymaps);
        let path = "fixtures/大文档测试2.md";
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[bench] skip realfile: {e}");
                return;
            }
        };
        let rope0 = ropey::Rope::from_str(&text);
        let t_scan = Instant::now();
        let idx0 = std::rc::Rc::new(crate::markdown::scan_blocks_rope(&rope0));
        let scan_ms = t_scan.elapsed().as_secs_f64() * 1000.0;

        struct DualHost {
            md: Entity<MarkdownPreview>,
            wy: Entity<WyEditor>,
        }
        impl Render for DualHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.md.clone()
            }
        }
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let md = cx.new(|cx| MarkdownPreview::new(cx));
            // WYSIWYG 用同步装配（小文档/测试路径）：本基准只关心 build_frame 的
            // 整形/滚动成本，索引已由 `set_source_text` 现建，口径与预览同源。
            let wy = cx.new(|cx| WyEditor::new(window, cx).with_text(&text));
            DualHost { md, wy }
        });
        let vcx: &mut VisualTestContext = vcx;
        let p = vcx.update(|_w, cx| Rc::new(StyleParams::from_theme_with_font(cx, None)));
        let wy = vcx.update(|_, cx| host.read(cx).wy.clone());
        // —— Preview（元素树）首窗：紧接 set_source（未 resize/未 settle），冷物化
        //    首个窗口 + 高度模型——否则 settle 期间真实渲染已建成缓存，测不到冷首帧。
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |m, cx| {
                m.set_source_with_index(crate::markdown::PreviewSrc::Rope(rope0), idx0, cx)
            });
        });
        let preview_first_ms = vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |m, cx| {
                let t0 = Instant::now();
                m.prepare_render(&p, 980.0, cx);
                t0.elapsed().as_secs_f64() * 1000.0
            })
        });
        vcx.simulate_resize(size(px(980.), px(760.)));
        settle(vcx);
        let n_items = vcx.update(|_, cx| host.read(cx).md.read(cx).item_count());
        assert!(
            n_items > crate::config::limits::WINDOWED_MIN_BLOCKS,
            "需窗口模式（块数 {n_items}）"
        );

        // —— Preview（元素树）滚动重定位——
        let n = 24usize;
        let t0 = Instant::now();
        vcx.update(|_, cx| {
            let md = host.read(cx).md.clone();
            md.update(cx, |m, cx| {
                let total = m.total_h();
                for i in 0..n {
                    let target = total * (i as f32 + 1.0) / (n as f32 + 1.0);
                    m.desired_scroll.set(target);
                    m.prepare_render(&p, 980.0, cx);
                }
            });
        });
        let preview_scroll_ms = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;

        // —— WyEditor（自绘 build_frame）——
        let wy_first_ms = vcx.update(|window, cx| {
            // 首屏可视带（≈ 预览的初始窗口），不算全篇。
            let t0 = Instant::now();
            let _f = build_frame(wy.read(cx), window, px(980.), (0., 2048.));
            t0.elapsed().as_secs_f64() * 1000.0
        });
        let wy_full_ms = vcx.update(|window, cx| {
            // 全篇一次性整形（对照参考；预览不这样做）。
            let t0 = Instant::now();
            let f = build_frame(wy.read(cx), window, px(980.), (0., 999_999.));
            let total = f32::from(f.total);
            let _ = total;
            t0.elapsed().as_secs_f64() * 1000.0
        });
        let wy_scroll_ms = vcx.update(|window, cx| {
            let total = f32::from(build_frame(wy.read(cx), window, px(980.), (0., 999_999.)).total);
            let t0 = Instant::now();
            for i in 0..n {
                let lo = total * (i as f32 + 1.0) / (n as f32 + 2.0);
                let _f = build_frame(wy.read(cx), window, px(980.), (lo, lo + 1024.));
            }
            t0.elapsed().as_secs_f64() * 1000.0 / n as f64
        });

        eprintln!(
            "[bench] 只读查看对比（{} 块 / 窗口模式，真实大文档，{n} 步滚动）：\n  \
             索引构建(scan_blocks_rope) 参照 {scan_ms:.2}ms\n  \
             Preview(元素树): 首帧 {preview_first_ms:.2}ms、滚动重定位 {preview_scroll_ms:.2}ms/步\n  \
             WyEditor(自绘):  首屏带 {wy_first_ms:.2}ms、全篇整形 {wy_full_ms:.2}ms、滚动带 {wy_scroll_ms:.2}ms/步",
            n_items
        );
        crate::prof::dump("preview.vs.wy.readonly");
    }

    /// 基准（--ignored）：**新搬入功能（搜索跳转高亮）的逐帧代价 A/B**——同一注入帧，
    /// 无高亮 vs 有高亮各 `window.draw` N 帧，取差 = `paint_search_highlight` 的每帧
    /// 开销（仅命中时存在）。量化「合并 + 搬运」相对改动前多出的真实成本。`--ignored` 运行。
    #[gpui::test]
    #[ignore]
    fn bench_wy_search_highlight_paint_cost(cx: &mut TestAppContext) {
        use crate::markdown::wysiwyg::WyEditor;
        use std::time::Instant;
        cx.update(crate::actions::bind_local_keymaps);
        let text = "alpha beta 中文命中 beta again\nsecond line here\n";
        struct WyOnlyHost {
            ed: Entity<WyEditor>,
        }
        impl Render for WyOnlyHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.ed.clone()
            }
        }
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
            WyOnlyHost { ed }
        });
        let vcx: &mut VisualTestContext = vcx;
        // 不注入帧：让元素在绘制时自建（小文档很快），A/B 两侧完全同口径，
        // 差值只含 `paint_search_highlight`（命中分支）的增量。
        let n = 16usize;
        let t_no = vcx.update(|window, cx| {
            let t0 = Instant::now();
            for _ in 0..n {
                let _ = window.draw(cx);
            }
            t0.elapsed().as_secs_f64() * 1000.0 / n as f64
        });
        let t_hl = vcx.update(|window, cx| {
            let ed = host.read(cx).ed.clone();
            ed.update(cx, |e, cx| e.highlight_search(0, "beta", cx));
            let t0 = Instant::now();
            for _ in 0..n {
                let _ = window.draw(cx);
            }
            t0.elapsed().as_secs_f64() * 1000.0 / n as f64
        });
        eprintln!(
            "[bench] WyEditor 每帧绘制（{n} 帧均摊）：无高亮 {t_no:.4}ms、搜索高亮命中 {t_hl:.4}ms、增量 Δ{:.4}ms/帧",
            (t_hl - t_no).max(0.0)
        );
    }







