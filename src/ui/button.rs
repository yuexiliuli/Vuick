//! 自绘按钮：`Button` + [`ButtonVariant`] + [`ButtonVariants`]，替代
//! `gpui_component::button::{Button, ButtonVariants}`。
//!
//! # 用法
//! 与 gpui-component 对齐：`Button::new(id).label("文件").ghost().w(px(56.)).h(MENU_BAR_H)`，
//! 样式走 gpui 的 [`Styled`] 链（`flex/items_center/px_2/rounded_md/text_size/text_color`）。
//! 交互：点击经 [`Button::on_click`]；禁用经 [`Button::disabled`]（`opacity` 淡化 + 鼠标
//! 激活阻断）；菜单弹层打开时由 popover 置 `.selected(true)` 呈现按压态。
//!
//! # 变体配色（单一来源）
//! 全部取 `theme::ColorScheme` 语义字段，禁止硬编码 `rgba()`：Primary=accent、Link=link、
//! 其余=fg（悬浮/按压背底用 fg 的低透明着染，深色下加深）。目前实现 App 实际用到的
//! 变体：默认 / Primary / Ghost / Link / Text。
//!
//! # 下拉菜单过渡
//! `#7 menu` 落地前，`.dropdown_menu(...)` 复用 `gpui_component::menu::DropdownMenu` trait
//! （要求 `Styled + Selectable + InteractiveElement + IntoElement`）；本模块对 `Button`
//! 实现了 `gpui_component::Selectable` 兼容垫片（弹出层用它设选中态），#7 完成后随
//! `gpui_component::menu` 一起删除。

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, CursorStyle, ElementId, FocusHandle, Hsla, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Refineable, RenderOnce, SharedString, Stateful,
    StatefulInteractiveElement, StyleRefinement, Styled, ViewElement, Window, div,
    prelude::FluentBuilder as _, relative, transparent_black, white,
};

use crate::ui::theme::{ColorScheme, Theme};

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// 按钮视觉变体。
///
/// 只实现本 App 实际用到的变体（见 `docs/ui-migration.md` #6）；需要更多变体时
/// 先在 `ColorScheme` 提供对应语义色再扩展，勿直接写死色值。
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum ButtonVariant {
    /// 描边透明按钮（默认）。
    #[default]
    Default,
    /// 强调色实底按钮（accent + 白字）。
    #[allow(dead_code)] // 公开变体，App 现仅消费 Ghost；测试覆盖，后续调用点接入
    Primary,
    /// 无边框透明按钮，悬浮/按压轻微着染（App 菜单栏按钮）。
    Ghost,
    /// 链接样按钮（link 色、透明、悬浮加深）。
    #[allow(dead_code)] // 同上：公开变体待后续调用点
    Link,
    /// 纯文本按钮（透明、无边框，悬浮轻微着染）。
    #[allow(dead_code)] // 同上：公开变体待后续调用点
    Text,
}

impl ButtonVariant {
    /// 常态前景/背景/边框。边框仅 `Default` 提供（其余变体无边框）。
    fn rest(&self, colors: &ColorScheme) -> (Hsla, Hsla, Option<Hsla>) {
        let fg: Hsla = colors.fg.into();
        match self {
            ButtonVariant::Default => (transparent_black(), fg, Some(colors.border.into())),
            ButtonVariant::Primary => (colors.accent.into(), white(), None),
            ButtonVariant::Ghost => (transparent_black(), fg, None),
            ButtonVariant::Link => (transparent_black(), colors.link.into(), None),
            ButtonVariant::Text => (transparent_black(), fg, None),
        }
    }

    /// 悬浮背底：基于语义前景色的低透明着染（深色下加深保对比），Primary 用 accent 微降。
    fn hover_bg(&self, colors: &ColorScheme, is_dark: bool) -> Hsla {
        let fg: Hsla = colors.fg.into();
        let accent: Hsla = colors.accent.into();
        match self {
            ButtonVariant::Primary => accent.opacity(0.9),
            ButtonVariant::Link => transparent_black(),
            _ => {
                let alpha = if is_dark { 0.14 } else { 0.08 };
                fg.opacity(alpha)
            }
        }
    }

    /// 按压/选中背底：比悬浮更深一档。
    fn active_bg(&self, colors: &ColorScheme, is_dark: bool) -> Hsla {
        let fg: Hsla = colors.fg.into();
        let accent: Hsla = colors.accent.into();
        match self {
            ButtonVariant::Primary => accent.opacity(0.75),
            ButtonVariant::Link => transparent_black(),
            _ => {
                let alpha = if is_dark { 0.24 } else { 0.14 };
                fg.opacity(alpha)
            }
        }
    }
}

/// 变体选择器：给按钮声明视觉变体（`ghost()` / `primary()` / `link()` / `text()`）。
pub trait ButtonVariants: Sized {
    fn with_variant(self, variant: ButtonVariant) -> Self;

    #[allow(dead_code)] // 首批仅 App 消费 ghost()；其余变体待后续调用点
    fn primary(self) -> Self {
        self.with_variant(ButtonVariant::Primary)
    }

    fn ghost(self) -> Self {
        self.with_variant(ButtonVariant::Ghost)
    }

    #[allow(dead_code)]
    fn link(self) -> Self {
        self.with_variant(ButtonVariant::Link)
    }

    #[allow(dead_code)]
    fn text(self) -> Self {
        self.with_variant(ButtonVariant::Text)
    }
}

/// 自绘按钮：状态/焦点/点击/键盘激活齐全的 `Stateful<Div>` 组合。
pub struct Button {
    id: ElementId,
    base: Stateful<gpui::Div>,
    /// 调用方经 [`Styled`] 链累计的样式（render 时叠加到默认样式之上，调用方可覆盖）。
    style: StyleRefinement,
    label: Option<SharedString>,
    variant: ButtonVariant,
    children: Vec<AnyElement>,
    disabled: bool,
    selected: bool,
    on_click: Option<ClickHandler>,
    tab_index: isize,
    tab_stop: bool,
}

impl Button {
    pub fn new(id: impl Into<ElementId>) -> Self {
        let id = id.into();
        Self {
            base: div().id(id.clone()),
            id,
            style: StyleRefinement::default(),
            label: None,
            variant: ButtonVariant::default(),
            children: Vec::new(),
            disabled: false,
            selected: false,
            on_click: None,
            tab_index: 0,
            tab_stop: true,
        }
    }

    /// 设置文本标签；不设则仅渲染子元素/图标。
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// 注册点击回调（指针 + 聚焦后的 Enter/Space）。
    #[allow(dead_code)] // 公开 API：测试使用；App 菜单栏按钮现经下拉菜单触发，后续调用点接入
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }

    /// 禁用：淡化显示并阻断指针/键盘激活（事件不外传父级）。
    #[allow(dead_code)]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// 是否为按压态（弹出层打开时由 popover `.selected(true)` 驱动，呈现激活背底）。
    #[allow(dead_code)]
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    #[allow(dead_code)]
    pub fn tab_index(mut self, index: isize) -> Self {
        self.tab_index = index;
        self
    }

    #[allow(dead_code)]
    pub fn tab_stop(mut self, tab_stop: bool) -> Self {
        self.tab_stop = tab_stop;
        self
    }

    fn focus_handle(&self, window: &mut Window, cx: &mut App) -> FocusHandle {
        window
            .use_keyed_state(self.id.clone(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone()
    }
}

impl Styled for Button {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl ParentElement for Button {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl InteractiveElement for Button {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.base.interactivity()
    }
}

impl StatefulInteractiveElement for Button {}

impl ButtonVariants for Button {
    fn with_variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }
}

// 过渡垫片：#7 menu 落地后随 `gpui_component::menu` 一起删除（见模块文档）。
impl gpui_component::Selectable for Button {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

/// 让自绘按钮可挂 gpui_component 的下拉菜单（`DropdownMenu::dropdown_menu` 有默认实现，
/// 只要求 `Styled + Selectable + InteractiveElement + IntoElement + 'static`）。#7 落地后删除。
impl gpui_component::menu::DropdownMenu for Button {}

impl RenderOnce for Button {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let focus_handle = self.focus_handle(window, cx);
        let theme = Theme::global(cx);
        let (colors, is_dark) = (theme.colors, theme.is_dark());
        let variant = self.variant;
        let disabled = self.disabled;
        let caller_style = self.style.clone();
        let on_click = self.on_click;
        let label = self.label;

        let (bg, fg, border) = variant.rest(&colors);

        let mut root = self
            .base
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .line_height(relative(1.))
            .cursor(CursorStyle::PointingHand)
            .bg(bg)
            .text_color(fg)
            .when_some(border, |this, b| this.border_1().border_color(b))
            .when(self.selected && !disabled, |this| {
                this.bg(variant.active_bg(&colors, is_dark))
            })
            .when(!self.selected && !disabled, |this| {
                this.hover(|s| s.bg(variant.hover_bg(&colors, is_dark)))
                    .active(|s| s.bg(variant.active_bg(&colors, is_dark)))
            })
            .when(disabled, |this| {
                this.opacity(0.45).on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
            })
            .when_some(on_click, |this, on_click| {
                this.on_click(move |event, window, cx| {
                    if !disabled {
                        on_click(event, window, cx);
                    }
                })
            })
            .track_focus(&focus_handle.tab_index(self.tab_index).tab_stop(self.tab_stop));
        // 调用方样式最后叠加：默认具名的左右/居中/指针与调用方同值会互相覆盖为同值，
        // 其余（w/h/px/rounded/text_size/text_color…）由调用方决定。
        root.style().refine(&caller_style);

        root.child(
            div()
                .whitespace_nowrap()
                .overflow_hidden()
                .when_some(label, |this, l| this.child(l)),
        )
        .children(self.children)
    }
}

impl IntoElement for Button {
    type Element = ViewElement<Self>;

    #[track_caller]
    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Modifiers, Render, TestAppContext, VisualTestContext, point, px};
    use std::{cell::Cell, rc::Rc};

    struct Harness {
        clicks: Rc<Cell<usize>>,
        parent_clicks: Rc<Cell<usize>>,
        disabled: bool,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let clicks = self.clicks.clone();
            let parent_clicks = self.parent_clicks.clone();
            div()
                .id("h-parent")
                .w(px(200.))
                .h(px(80.))
                .on_click(move |_, _, _| parent_clicks.set(parent_clicks.get() + 1))
                .child(
                    Button::new("test-btn")
                        .label("测试")
                        .ghost()
                        .w(px(100.))
                        .h(px(40.))
                        .disabled(self.disabled)
                        .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
                )
        }
    }

    fn harness(
        cx: &mut TestAppContext,
        disabled: bool,
    ) -> (&mut VisualTestContext, Rc<Cell<usize>>, Rc<Cell<usize>>) {
        let clicks = Rc::new(Cell::new(0));
        let parent_clicks = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            let parent_clicks = parent_clicks.clone();
            move |_, _| Harness {
                clicks,
                parent_clicks,
                disabled,
            }
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        (cx, clicks, parent_clicks)
    }

    #[test]
    fn builder_sets_variant_label_and_state() {
        let b = Button::new("b")
            .label("文件")
            .ghost()
            .disabled(true)
            .selected(true)
            .tab_index(2)
            .tab_stop(false);
        assert_eq!(b.variant, ButtonVariant::Ghost);
        assert_eq!(b.label.as_deref(), Some("文件"));
        assert!(b.disabled);
        assert!(b.selected);
        assert_eq!(b.tab_index, 2);
        assert!(!b.tab_stop);

        let primary = Button::new("p").primary();
        assert_eq!(primary.variant, ButtonVariant::Primary);
    }

    #[gpui::test]
    fn pointer_click_fires_once(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (cx, clicks, _) = harness(cx, false);
        cx.simulate_click(point(px(50.), px(20.)), Modifiers::default());
        assert_eq!(clicks.get(), 1, "点击按钮应触发一次 on_click");
        // 启用态点击按 gpui 默认向上冒泡（与 gpui-component 一致：仅禁用阻断，见下测）。
    }

    #[gpui::test]
    fn click_outside_hit_area_does_not_fire(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (cx, clicks, _) = harness(cx, false);
        // 按钮只在 100x40 命中区内可点：点父级空白（右上角）不应触发按钮。
        cx.simulate_click(point(px(180.), px(10.)), Modifiers::default());
        assert_eq!(clicks.get(), 0);
    }

    #[gpui::test]
    fn disabled_button_is_inert_and_blocks_parent(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (cx, clicks, parent_clicks) = harness(cx, true);
        cx.simulate_click(point(px(50.), px(20.)), Modifiers::default());
        assert_eq!(clicks.get(), 0, "禁用按钮不应触发 on_click");
        assert_eq!(parent_clicks.get(), 0, "禁用按钮应阻断事件外传父级");
    }
}
