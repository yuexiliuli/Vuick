//! 列表项行（`ListItemRow`）：命令面板 / 下拉菜单等「内容自组装 + 选中态 + 键盘导航」
//! 列表单行的统一行壳。
//!
//! 行壳固定对齐 `palette` 条目行真实形状：`flex + items_center + gap_2 + px_3 + mx_1 +
//! min_h(30) + rounded_md`；选中/悬停背景经 `with_spring(format!("{id}-bg"), hover_spring),
//! bg(accent.opacity(v)))` 淡入（曲线全站统一，`reduce_motion` 自动尊重）。**内容
//! （leading/title/trailing 内层 flex）由调用方组装传入**，本组件不烘文本样式。
//!
//! 交互分两个正交槽：`on_hover_select(hovered)` —— 悬停进入/移出**上抛给宿主**管理
//! `selected`（组件侧不自行写选中、不 notify；选中转而改变 `is_selected` → 弹簧背景
//! target）；`on_activate` —— 点击执行（Enter 由宿主键盘路径触发同一处理器）。返回
//! `ListItemRow` 型组件恒为手型 cursor（对齐 palette 条目行：恒可交互）。

use std::rc::Rc;

use gpui::{
    AnimationExt, AnyElement, App, ClickEvent, CursorStyle, ElementId, Hsla, InteractiveElement,
    IntoElement, ParentElement, RenderOnce, StatefulInteractiveElement, Styled, ViewElement,
    Window, div, prelude::FluentBuilder as _, px,
};

use crate::ui::motion::{element_id_label, hover_spring};

/// 列表项行：`content` 内层 flex 由调用方组装；选中态驱动弹簧背景 target；
/// 悬停经 `on_hover_select` 上抛、点击经 `on_activate` 上抛。
pub struct ListItemRow {
    id: ElementId,
    content: AnyElement,
    is_selected: bool,
    /// 选中/悬停背景 target 色（调用方从 theme 取；透明度由弹簧 0..1 调制）。
    accent: Hsla,
    on_hover_select: Option<Rc<dyn Fn(bool, &mut Window, &mut App)>>,
    on_activate: Option<Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>>,
}

impl ListItemRow {
    /// 以调用方指定的 id 构建（debug_selector / 弹簧键随 id 派生，供定位与跨帧隔离）。
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            content: div().into_any_element(),
            is_selected: false,
            accent: gpui::transparent_black(),
            on_hover_select: None,
            on_activate: None,
        }
    }

    /// 行内容（调用方组装的内层 flex：leading/title/trailing 均可）。
    pub fn content(mut self, content: impl IntoElement) -> Self {
        self.content = content.into_any_element();
        self
    }

    /// 选中态（true → 弹簧背景收敛到 `accent` 全显）。
    pub fn selected(mut self, is_selected: bool) -> Self {
        self.is_selected = is_selected;
        self
    }

    /// 选中/悬停背景 target 色（theme 语义色；透明度由 spring 调制）。
    pub fn accent(mut self, accent: Hsla) -> Self {
        self.accent = accent;
        self
    }

    /// 悬停进入/移出上抛（宿主管理 `selected` 并自行 notify；组件不写选中态）。
    pub fn on_hover_select(
        mut self,
        handler: impl Fn(bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_hover_select = Some(Rc::new(handler));
        self
    }

    /// 点击执行（Enter 由宿主键盘路径触发同一处理器）。
    pub fn on_activate(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_activate = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for ListItemRow {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let id = self.id;
        let label = element_id_label(&id);
        let spring_key = format!("{label}-bg");
        let accent = self.accent;
        let is_selected = self.is_selected;
        let on_hover_select = self.on_hover_select;
        let on_activate = self.on_activate;
        let mut root = div()
            .id(id)
            .debug_selector(move || label.clone())
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .mx_1()
            .min_h(px(30.))
            .rounded_md()
            .cursor(CursorStyle::PointingHand);
        // 悬停上抛（组件只转发；宿主做 selected + notify，不在此处 notify）。
        root = root.when_some(on_hover_select, |this, cb| {
            this.on_hover(move |h, window, cx| cb(*h, window, cx))
        });
        // 点击执行上抛。
        root = root.when_some(on_activate, |this, cb| {
            this.on_click(move |event, window, cx| cb(event, window, cx))
        });
        // 选中/悬停背景弹簧：`is_selected` 决定收敛 target，透明度经 `hover_spring` 单调。
        root.with_spring(
            spring_key,
            hover_spring(if is_selected { 1.0 } else { 0.0 }),
            move |this, v| this.bg(accent.opacity(v)),
        )
        .child(self.content)
    }
}

impl IntoElement for ListItemRow {
    type Element = ViewElement<Self>;

    #[track_caller]
    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, Modifiers, Render, TestAppContext, VisualTestContext, px,
    };
    use std::{cell::Cell, rc::Rc};

    /// 测试宿主：同时渲染选中 (`li-row-sel`) 与未选中 (`li-row`) 两行对照，共享
    /// `on_activate` / `on_hover_select` 回调计数。
    struct Harness {
        activations: Rc<Cell<usize>>,
        hover_selects: Rc<Cell<usize>>,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let activations = self.activations.clone();
            let hover_selects = self.hover_selects.clone();
            div()
                .id("wrap")
                .w(px(400.))
                .h(px(200.))
                .child(
                    ListItemRow::new("li-row")
                        .accent(gpui::hsla(0.60, 0.60, 0.60, 1.0))
                        .content(div().text_size(px(12.)).child("Item A"))
                        .on_hover_select(move |_, _, _| {
                            hover_selects.set(hover_selects.get() + 1)
                        })
                        .on_activate(move |_, _, _| {
                            activations.set(activations.get() + 1)
                        }),
                )
                .child(
                    ListItemRow::new("li-row-sel")
                        .selected(true)
                        .accent(gpui::hsla(0.60, 0.60, 0.60, 1.0))
                        .content(div().text_size(px(12.)).child("Item B")),
                )
        }
    }

    fn harness(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<Cell<usize>>, Rc<Cell<usize>>) {
        let activations = Rc::new(Cell::new(0));
        let hover_selects = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let activations = activations.clone();
            let hover_selects = hover_selects.clone();
            move |_, _| Harness {
                activations,
                hover_selects,
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        (cx, activations, hover_selects)
    }

    /// 渲染：选中与未选中两行都在场、内容文本参与布局（宽>0）；整链路无 panic
    /// （headless 读不到弹簧背景色，故只断 bounds——选中差异由两级弹簧 rely 真机）。
    #[gpui::test]
    fn selected_and_plain_render(cx: &mut TestAppContext) {
        let (cx, _, _) = harness(cx);
        for id in ["li-row", "li-row-sel"] {
            let row = cx.debug_bounds(id).expect("行应渲染");
            assert!(
                row.size.width.as_f32() > 0.,
                "content 文本应参与布局（宽>0）：{id}"
            );
        }
    }

    /// 点击命中区 → `on_activate` 触发一次。
    #[gpui::test]
    fn click_fires_on_activate(cx: &mut TestAppContext) {
        let (cx, activations, _) = harness(cx);
        let row = cx.debug_bounds("li-row").expect("行应在场");
        cx.simulate_click(row.center(), Modifiers::default());
        assert_eq!(activations.get(), 1, "点击列表项应触发 on_activate");
    }

    /// 悬停进入 → `on_hover_select` 上抛（计数 ≥1）；移出 → 再次上抛（计数增长）。
    #[gpui::test]
    fn hover_invokes_on_hover_select(cx: &mut TestAppContext) {
        let (cx, _, hover_selects) = harness(cx);
        let row = cx.debug_bounds("li-row").expect("行应在场");

        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        let after_enter = hover_selects.get();
        assert!(
            after_enter >= 1,
            "悬停进入应调用 on_hover_select，got={after_enter}"
        );

        cx.simulate_mouse_move(
            gpui::point(px(200.), px(150.)),
            None,
            Modifiers::default(),
        );
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert!(
            hover_selects.get() > after_enter,
            "悬停移出应再次调用 on_hover_select，after_enter={after_enter} now={}",
            hover_selects.get()
        );
    }
}
