//! 「编辑区⇄预览分割条 + 侧栏右缘调宽手柄」域（splitter，§8.3）：
//! 断言与 lib 单测 `src/app/tests/splitter.rs` 逐字一致（P1 测试域化收尾）；
//! `SidebarView` 改经 `common::*` 重导出的 `shell::app::SidebarView` 面访问；
//! 分割条/调宽私有字段（`split_ratio`/`split_drag`/`splitter_hovered`、
//! `sidebar.resize`/`sidebar.resize_hovered`）已按 §8.6 外提为 `#[doc(hidden)] pub`。

mod common;

use common::*;

/// 拖拽手柄落点（真实渲染路径）：
/// - 编辑↔预览分割条 → `splitter`（居中 2px 细线，**无**三点手柄）；
/// - 侧栏右缘调宽手柄（面板展开）→ `sidebar-resize`（内嵌细线命中区）；
/// - 未开预览 / 面板收起时对应手柄不出现。
#[gpui::test]
async fn splitter_and_sidebar_resize_handles_render(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    // 默认面板收起、预览未开：无分割条、无调宽手柄。
    assert!(
        cx.debug_bounds("splitter").is_none(),
        "未开预览不应渲染分割条"
    );
    assert!(
        cx.debug_bounds("splitter-grip-rest").is_none(),
        "分割条已无三点手柄"
    );
    // 打开目录面板 → 侧栏右缘调宽手柄出现（细线命中区）。
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| app.toggle_sidebar_view(SidebarView::Toc, cx));
    });
    for _ in 0..3 {
        cx.run_until_parked();
    }
    assert!(
        cx.debug_bounds("sidebar-resize").is_some(),
        "面板展开应有调宽手柄 sidebar-resize"
    );
    assert!(
        cx.debug_bounds("sidebar-resize-grip-rest").is_none(),
        "侧栏右缘手柄不再有三点圆点层"
    );
    // 打开 .md（自动开预览）→ 分割条出现；三点手柄恒不出现。
    let md = std::env::temp_dir().join("gpui-card-split.md");
    std::fs::write(&md, "# 标题\n正文\n").unwrap();
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
    });
    for _ in 0..8 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
    }
    assert!(
        cx.debug_bounds("splitter").is_some(),
        "md 预览分屏应渲染分割条"
    );
    assert!(
        cx.debug_bounds("splitter-grip-rest").is_none()
            && cx.debug_bounds("splitter-grip-accent").is_none(),
        "分割条不再有三点手柄（三点分隔已移除）"
    );
    let _ = std::fs::remove_file(&md);
}

/// 三点手柄不破坏分割条交互（真实事件路径）：悬停 → `splitter_hovered`（高亮态）；
/// 按下 → `split_drag`；右拖 → `split_ratio` 按窗口坐标位移增大；松开 → 结束。
#[gpui::test]
async fn splitter_grip_drag_still_resizes(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let md = std::env::temp_dir().join("gpui-card-split-drag.md");
    std::fs::write(&md, "# 标题\n正文\n".repeat(20)).unwrap();
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(md.to_str().unwrap(), window, cx));
    });
    for _ in 0..8 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
    }
    let sp = cx
        .debug_bounds("splitter")
        .expect("分割条应渲染（md 预览分屏）");
    let at = sp.center();
    // 悬停 → 高亮态。
    cx.simulate_mouse_move(at, None, Default::default());
    for _ in 0..3 {
        cx.run_until_parked();
    }
    assert!(
        cx.update(|_w, cx| app.read(cx).panes.splitter_hovered),
        "悬停分割条应打开高亮态 splitter_hovered"
    );
    // 按下 → 拖拽开始；右移 100px → 编辑区比例增大。
    let r0 = cx.update(|_w, cx| app.read(cx).panes.split_ratio);
    cx.simulate_mouse_down(at, MouseButton::Left, Default::default());
    for _ in 0..2 {
        cx.run_until_parked();
    }
    assert!(
        cx.update(|_w, cx| app.read(cx).panes.split_drag.is_some()),
        "按下分割条应开始拖拽"
    );
    let target = gpui::point(at.x + px(100.), at.y);
    cx.simulate_mouse_move(target, MouseButton::Left, Default::default());
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let r1 = cx.update(|_w, cx| app.read(cx).panes.split_ratio);
    assert!(
        r1 > r0,
        "右拖 100px 应加大编辑区比例：{r0:.3} → {r1:.3}"
    );
    cx.simulate_mouse_up(target, MouseButton::Left, Default::default());
    for _ in 0..3 {
        cx.run_until_parked();
    }
    assert!(
        cx.update(|_w, cx| app.read(cx).panes.split_drag.is_none()),
        "松开应结束拖拽"
    );
    let _ = std::fs::remove_file(&md);
}

/// 右缘调宽手柄不破坏侧栏调宽（真实事件路径）：悬停手柄 → `resize_hovered`；
/// 按下 → `resize`；右拖 60px → 侧栏宽 +60 且关闭自适应；松开 → 结束。
#[gpui::test]
async fn sidebar_resize_handle_still_resizes(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| app.toggle_sidebar_view(SidebarView::Toc, cx));
    });
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let sp = cx
        .debug_bounds("sidebar-resize")
        .expect("调宽手柄应渲染（面板展开）");
    let at = sp.center();
    let w0 = cx.update(|_w, cx| app.read(cx).sidebar.width);
    // 悬停 → hover 态。
    cx.simulate_mouse_move(at, None, Default::default());
    for _ in 0..3 {
        cx.run_until_parked();
    }
    assert!(
        cx.update(|_w, cx| app.read(cx).sidebar.resize_hovered),
        "悬停调宽手柄应打开调宽 hover 态"
    );
    // 按下 → 拖拽开始；右移 60px → 宽度 +60、关闭自适应；松开 → 结束。
    cx.simulate_mouse_down(at, MouseButton::Left, Default::default());
    for _ in 0..2 {
        cx.run_until_parked();
    }
    assert!(
        cx.update(|_w, cx| app.read(cx).sidebar.resize.is_some()),
        "按下调宽手柄应开始调宽"
    );
    let target = gpui::point(at.x + px(60.), at.y);
    cx.simulate_mouse_move(target, MouseButton::Left, Default::default());
    for _ in 0..3 {
        cx.run_until_parked();
    }
    cx.simulate_mouse_up(target, MouseButton::Left, Default::default());
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let (w1, auto, dragging) = cx.update(|_w, cx| {
        let a = app.read(cx);
        (a.sidebar.width, a.sidebar.width_auto, a.sidebar.resize.is_some())
    });
    assert!(
        (w1 - w0 - 60.0).abs() < 3.0,
        "右拖 60px 应加宽 60：{w0:.1} → {w1:.1}"
    );
    assert!(!auto, "手动拖拽应关闭宽度自适应");
    assert!(!dragging, "松开应结束调宽");
}
