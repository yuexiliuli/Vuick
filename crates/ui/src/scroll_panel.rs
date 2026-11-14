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
    App, ElementId, IntoElement, InteractiveElement as _, MouseButton, MouseDownEvent,
    ParentElement as _, ScrollHandle, StatefulInteractiveElement as _, Styled as _, Window, div,
    px,
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
/// 不一致），或给滚动容器追加样式（目录面板的 `py_1`，内容区上下留白随滚动量程），
/// 或经 [`Self::on_container_mouse_down`] 注入容器级鼠标事件（WYSIWYG 活区点击空白用）。
/// 三层（包装 / 滚动容器 / 滚动条覆盖层）都挂与各自 id label 同名的 `debug_selector`。
pub struct ScrollPanelBuilder {
    container_id: ElementId,
    handle: ScrollHandle,
    wrap_id: Option<ElementId>,
    bar_id: Option<ElementId>,
    container_style: Option<fn(gpui::Div) -> gpui::Div>,
    /// 滚动容器元素级 `on_mouse_down` 注入（可选项；默认 `None` 与既有行为逐位一致）。
    /// 容器已是 `Stateful` id 元素（`.id(container_id)`），直接挂元素级监听。
    on_container_mouse_down: Option<(
        MouseButton,
        Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App) + 'static>,
    )>,
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
            on_container_mouse_down: None,
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

    /// 给滚动容器挂**元素级** `on_mouse_down`（点击容器空白 / 命中 / 选择等需要
    /// 「容器容器级」鼠标事件的场景）。listener 形态与 [`InteractiveElement::on_mouse_down`]
    /// 一致，`Context::listener(...)` 产出可直接传入。默认不设（`None`）与既有行为
    /// 逐位一致，sidebar 三处调用点零改动。
    pub fn on_container_mouse_down(
        mut self,
        button: MouseButton,
        listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_container_mouse_down = Some((button, Box::new(listener)));
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
        let mut container = base
            .id(container_id)
            .debug_selector(move || container_label.clone())
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(handle);
        // 容器级 `on_mouse_down` 注入（可选项，默认 `None` 逐位保持既有行为）。
        if let Some((button, listener)) = self.on_container_mouse_down {
            container = container.on_mouse_down(button, listener);
        }
        let container = container.child(inner);

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
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use gpui::{
        App, Context, InteractiveElement, MouseDownEvent, ParentElement, Render, Styled,
        TestAppContext, VisualTestContext, Window, div, point, px, size,
    };

    struct ScrollPanelHarness {
        handle: ScrollHandle,
        rows: usize,
        /// 测试注入：容器级 `on_mouse_down` 触发计数（`with_hook` 时每次容器点击 +1）。
        container_hits: Rc<Cell<u32>>,
        with_hook: bool,
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
            let mut builder = ScrollPanelBuilder::new("panel-scroll", self.handle.clone());
            if self.with_hook {
                let hits = self.container_hits.clone();
                builder = builder.on_container_mouse_down(
                    MouseButton::Left,
                    move |_: &MouseDownEvent, _: &mut Window, _: &mut App| {
                        hits.set(hits.get() + 1);
                    },
                );
            }
            builder.build(inner)
        }
    }

    fn harness(
        cx: &mut TestAppContext,
        rows: usize,
        with_hook: bool,
    ) -> (&mut VisualTestContext, Rc<Cell<u32>>, ScrollHandle) {
        let handle = ScrollHandle::new();
        let hits = Rc::new(Cell::new(0u32));
        let (_, cx) = cx.add_window_view({
            let handle = handle.clone();
            let hits = hits.clone();
            move |_, _| ScrollPanelHarness {
                handle,
                rows,
                container_hits: hits.clone(),
                with_hook,
            }
        });
        cx.simulate_resize(size(px(400.), px(300.)));
        // 布局 / 滚动量程稳定（对齐 palette 既有 `run_until_parked` 驱动姿势）。
        for _ in 0..3 {
            cx.run_until_parked();
        }
        (cx, hits, handle)
    }

    /// 滚动可达 + 滚动条按 viewport 显示：内容（60×24=1440px）超出视口（300px）→
    /// 容器可滚（`max_offset > 0`）、滚动条量程 = 视口 + 可滚距离 > 视口 → 显示 thumb。
    #[gpui::test]
    fn tall_content_is_scrollable_and_bar_scales_with_viewport(cx: &mut TestAppContext) {
        let (_, _, handle) = harness(cx, 60, false);
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
        let (_, _, handle) = harness(cx, 3, false);
        assert_eq!(handle.max_offset().y, px(0.), "内容不超出时不可滚");
        assert!(
            handle.bounds().size.height + handle.max_offset().y <= handle.bounds().size.height,
            "内容 ≤ 视口：滚动条量程 = 视口（不显示）"
        );
    }

    /// 容器级 `on_mouse_down` 钩子：点击滚动容器**内容区**（非轨道）→ 回调触发。
    #[gpui::test]
    fn container_mouse_down_hook_fires_on_container_click(cx: &mut TestAppContext) {
        let (cx, hits, _) = harness(cx, 60, true);
        assert_eq!(hits.get(), 0, "前置：未点击前无触发");
        // 容器中部（x=200 远离右缘 16px 轨道）→ 命中滚动容器（内容/空白均在容器内）。
        cx.simulate_mouse_down(point(px(200.), px(150.)), MouseButton::Left, Default::default());
        cx.run_until_parked();
        assert_eq!(hits.get(), 1, "容器内点击应触发 on_container_mouse_down 钩子");
    }

    /// 钩子不得吞掉滚动条交互：点击右缘 16px 轨道（自绘滚动条消费并 `stop_propagation`）
    /// → 容器钩子**不**触发（滚动条拖拽/轨道跳转仍正常）。
    #[gpui::test]
    fn container_mouse_down_hook_does_not_eat_scrollbar_clicks(cx: &mut TestAppContext) {
        let (cx, hits, _) = harness(cx, 60, true);
        // x=392 ∈ 轨道 [384,400] → 滚动条消费，容器钩子不得触发。
        cx.simulate_mouse_down(point(px(392.), px(150.)), MouseButton::Left, Default::default());
        cx.run_until_parked();
        assert_eq!(
            hits.get(),
            0,
            "滚动条轨道点击应被自绘滚动条消费（stop_propagation），不落容器钩子"
        );
    }
}
