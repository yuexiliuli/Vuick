//! 滚动容器助手：[`scroll_panel`] / [`ScrollPanelBuilder`] 一次性收敛「relative 包装层 +
//! `overflow_y_scroll` 滚动容器（`track_scroll` 跟踪）+ 叠层常显纵向滚动条
//! （`viewport_from_layout`）」逐字同构样板（sidebar 目录 / 搜索 / 文件树三处）。
//!
//! 行为与既有 sidebar 样板逐位一致：
//! - 包装层 `relative + flex + flex_col + flex_1 + min_w(0) + min_h(0)`——内容超出时在
//!   容器内滚动，滚动条作为 absolute 覆盖层压在右缘；
//! - 滚动容器 id = 调用方传入的 `id`（测试 / 调试锚，如 `sidebar-toc-scroll`）；
//!   包装层与滚动条覆盖层 id 默认由它派生（`-wrap` / `-vscrollbar`），[`ScrollPanelBuilder`]
//!   可显式覆盖（shell 三处各自实测 id 与派生形式不一致：如 `sidebar-toc-vscrollbar`、
//!   `sidebar-files-wrap`）；
//! - 三层（包装 / 滚动容器 / 滚动条覆盖层）都挂 `debug_selector`（各自 id label）——
//!   gpui 仅在显式 `debug_selector` 时记录 `debug_bounds`，shell 侧栏测试按这些 key 断言；
//! - 滚动条 `Scrollbar::vertical(Always)` + `viewport_from_layout()`：内容超出视口才显示
//!   thumb、轨道点击跳转、thumb 可拖（滚动条自身几何由 `scrollbar` 模块单测覆盖）。

use gpui::{
    ElementId, IntoElement, InteractiveElement as _, ParentElement as _, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

use crate::ui::motion::element_id_label;
use crate::ui::scrollbar::{Scrollbar, ScrollbarMode};

/// 由容器基 id 派生兄弟 id（包装层 / 滚动条覆盖层）。
fn sibling_id(id: &ElementId, suffix: &str) -> ElementId {
    ElementId::from(format!("{}-{suffix}", element_id_label(id)))
}

/// 滚动容器助手：`relative` 包装层 + `overflow_y_scroll` 滚动容器（`track_scroll`
/// 跟踪 `handle`）+ 叠一条常显（`Always`）纵向滚动条（`viewport_from_layout` 跟随容器布局）。
///
/// - `id`：滚动容器元素 id（测试 / 调试锚；包装层与滚动条覆盖层由它派生 `-wrap` /
///   `-vscrollbar`）；
/// - `handle`：跨帧稳定滚动句柄（`ScrollHandle::new()` 存入宿主状态，`max_offset()`
///   表示可滚距离）；
/// - `inner`：滚动容器内容（超出即滚动）。
///
/// 全默认 ＝ [`ScrollPanelBuilder::new`] 不改任何选项；需要覆盖派生 id / 追加容器样式
/// 时改用构建器（shell 三处迁移用）。
pub fn scroll_panel(
    id: impl Into<ElementId>,
    handle: ScrollHandle,
    inner: impl IntoElement,
) -> impl IntoElement {
    ScrollPanelBuilder::new(id, handle).build(inner)
}

/// 滚动容器助手构建器：默认包装结构 / 兄弟 id 派生 / 样式与 [`scroll_panel`] 逐位一致，
/// 可覆盖派生的包装层 / 滚动条覆盖层 id（shell 三处实测 id 与 `{id}-vscrollbar` 派生
/// 不一致），或给滚动容器追加样式（目录面板的 `py_1`，内容区上下留白随滚动量程）。
/// 三层（包装 / 滚动容器 / 滚动条覆盖层）都挂与各自 id label 同名的 `debug_selector`。
pub struct ScrollPanelBuilder {
    container_id: ElementId,
    handle: ScrollHandle,
    wrap_id: Option<ElementId>,
    bar_id: Option<ElementId>,
    container_style: Option<fn(gpui::Div) -> gpui::Div>,
}

impl ScrollPanelBuilder {
    /// `container_id`：滚动容器元素 id（测试 / 调试锚；`&'static str` / `SharedString` 均可）。
    pub fn new(id: impl Into<ElementId>, handle: ScrollHandle) -> Self {
        Self {
            container_id: id.into(),
            handle,
            wrap_id: None,
            bar_id: None,
            container_style: None,
        }
    }

    /// 覆盖包装层元素 id（默认 `{container_id}-wrap`）。
    pub fn wrap_id(mut self, id: impl Into<ElementId>) -> Self {
        self.wrap_id = Some(id.into());
        self
    }

    /// 覆盖滚动条覆盖层元素 id（默认 `{container_id}-vscrollbar`；shell 三处都用显式值）。
    pub fn bar_id(mut self, id: impl Into<ElementId>) -> Self {
        self.bar_id = Some(id.into());
        self
    }

    /// 给滚动容器（`overflow_y_scroll + track_scroll` 那层）追加样式：如目录面板的
    /// `py_1`（内容区上下留白，padding 计入滚动量程）。
    pub fn container_style(mut self, f: fn(gpui::Div) -> gpui::Div) -> Self {
        self.container_style = Some(f);
        self
    }

    /// 产出完整面板（`relative` 包装层 + 滚动容器 + 叠层滚动条）。
    pub fn build(self, inner: impl IntoElement) -> impl IntoElement {
        let container_id = self.container_id;
        let container_label = element_id_label(&container_id);
        let wrap_id = self
            .wrap_id
            .unwrap_or_else(|| sibling_id(&container_id, "wrap"));
        let bar_id = self
            .bar_id
            .unwrap_or_else(|| sibling_id(&container_id, "vscrollbar"));
        let wrap_label = element_id_label(&wrap_id);
        let bar_label = element_id_label(&bar_id);
        let handle = &self.handle;

        // `container_style` 在 `.id()` 之前作用到裸 Div（`py_1` 等纯样式，与状态的
        // 添加顺序无关）；`.id()` 之后容器转 `Stateful<Div>`，类型不可名于入参。
        let base = div();
        let base = match self.container_style {
            Some(f) => f(base),
            None => base,
        };
        let container = base
            .id(container_id)
            .debug_selector(move || container_label.clone())
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(handle)
            .child(inner);

        div()
            .id(wrap_id)
            .debug_selector(move || wrap_label.clone())
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .child(container)
            .child(
                div()
                    .id(bar_id)
                    .debug_selector(move || bar_label.clone())
                    .absolute()
                    .inset_0()
                    .child(
                        Scrollbar::vertical(handle)
                            .mode(ScrollbarMode::Always)
                            .viewport_from_layout(),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, InteractiveElement, ParentElement, Render, Styled, TestAppContext,
        VisualTestContext, Window, div, px, size,
    };

    struct ScrollPanelHarness {
        handle: ScrollHandle,
        rows: usize,
    }

    impl Render for ScrollPanelHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let inner = div()
                .flex()
                .flex_col()
                .children((0..self.rows).map(|i| {
                    div()
                        .id(ElementId::from(format!("row-{i}")))
                        .h(px(24.))
                        .child("行")
                }));
            scroll_panel("panel-scroll", self.handle.clone(), inner)
        }
    }

    fn harness(cx: &mut TestAppContext, rows: usize) -> (&mut VisualTestContext, ScrollHandle) {
        let handle = ScrollHandle::new();
        let (_, cx) = cx.add_window_view({
            let handle = handle.clone();
            move |_, _| ScrollPanelHarness { handle, rows }
        });
        cx.simulate_resize(size(px(400.), px(300.)));
        // 布局 / 滚动量程稳定（对齐 palette 既有 `run_until_parked` 驱动姿势）。
        for _ in 0..3 {
            cx.run_until_parked();
        }
        (cx, handle)
    }

    /// 滚动可达 + 滚动条按 viewport 显示：内容（60×24=1440px）超出视口（300px）→
    /// 容器可滚（`max_offset > 0`）、滚动条量程 = 视口 + 可滚距离 > 视口 → 显示 thumb。
    #[gpui::test]
    fn tall_content_is_scrollable_and_bar_scales_with_viewport(cx: &mut TestAppContext) {
        let (_, handle) = harness(cx, 60);
        let bounds = handle.bounds();
        assert_eq!(
            bounds.size.height,
            px(300.),
            "滚动容器视口高应等于容器高度，实际 {:?}",
            bounds
        );
        let max_off = handle.max_offset().y;
        assert!(max_off > px(0.), "内容超出视口应可滚动，max_offset={max_off:?}");
        assert!(
            bounds.size.height + max_off > bounds.size.height,
            "滚动条 content_size = 视口 + 可滚距离 > 视口 → 按 viewport 显示 thumb"
        );
    }

    /// 内容不超出视口 → 不可滚、滚动条量程 = 视口（thumb 不显示）。
    #[gpui::test]
    fn content_fitting_viewport_is_not_scrollable(cx: &mut TestAppContext) {
        let (_, handle) = harness(cx, 3);
        assert_eq!(handle.max_offset().y, px(0.), "内容不超出时不可滚");
        assert!(
            handle.bounds().size.height + handle.max_offset().y <= handle.bounds().size.height,
            "内容 ≤ 视口：滚动条量程 = 视口（不显示）"
        );
    }
}
