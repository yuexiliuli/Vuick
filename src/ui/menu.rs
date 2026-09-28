//! 自绘下拉菜单：`PopupMenu`（实体）/ [`PopupMenuItem`] / [`Dropdown`]（触发宿主）+ [`DropdownMenu`] trait，
//! 替代 `gpui_component::menu::{DropdownMenu, PopupMenu, PopupMenuItem}`。
//!
//! # 结构
//! - [`DropdownMenu`]：trait，给 [`Button`](crate::ui::button::Button) 挂下拉菜单：
//!   `.dropdown_menu(|menu, _, _| …)` 返回 [`Dropdown`]（取代 gpui-component 的
//!   `DropdownMenuPopover`）。
//! - [`Dropdown`]：触发宿主。元素级状态（`use_keyed_state`，按宿主 id 隔离）持有
//!   `open` 与打开的 `Entity<PopupMenu>`；点击触发按钮（宿主 `on_mouse_down` 且按下点不在
//!   已开菜单内）切换开合；打开时用 `deferred(anchored(Local::TopLeft))` 在按钮左下挂菜单，
//!   并与菜单实体订阅 `DismissEvent`（菜单内选择/ Esc / 点击外关闭都会收敛到「关闭」）。
//! - [`PopupMenu`]：`Entity` + `Render`，持焦点、选中索引与渲染元素；键盘 ↑↓/Enter/Esc
//!   直接 `on_key_down` 处理（不引 keymap/actions）；点击外 `on_mouse_down_out` 关闭并
//!   把焦点还给开菜单前的句柄。
//!
//! # 范围
//! 只实现本 App 用到的单层菜单能力（`ElementItem` 自定义元素 + `Separator`）；子菜单、
//! 滚动、kbd 绑定等 gpui-component 的重交互能力不迁移（App 未使用）。

use std::rc::Rc;

use gpui::{
    Anchor, AnyElement, App, AppContext as _, ClickEvent, Context, DismissEvent, Edges, ElementId,
    Entity, EventEmitter, FocusHandle, Focusable, Hsla, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Pixels, Render, RenderOnce,
    StatefulInteractiveElement, Styled, Window, accesskit, anchored, deferred, div,
    prelude::FluentBuilder as _, px,
};

use crate::ui::theme::Theme;

/// 菜单项点击处理器。
type ItemHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
/// 菜单项自定义内容构建器（每次渲染调用，保证读到最新应用状态）。
type ItemRenderer = Box<dyn Fn(&mut Window, &mut App) -> AnyElement + 'static>;

/// 单层下拉菜单项。
pub enum PopupMenuItem {
    /// 分隔线。
    Separator,
    /// 自定义元素 / 按钮式菜单项。
    ElementItem {
        render: ItemRenderer,
        disabled: bool,
        handler: Option<ItemHandler>,
    },
}

impl PopupMenuItem {
    /// 用一个自定义元素当菜单项内容（App 的 `menu_action_item` 用它排「标签 + 快捷键」）。
    pub fn element<F, E>(builder: F) -> Self
    where
        F: Fn(&mut Window, &mut App) -> E + 'static,
        E: IntoElement,
    {
        PopupMenuItem::ElementItem {
            render: Box::new(move |window, cx| builder(window, cx).into_any_element()),
            disabled: false,
            handler: None,
        }
    }

    /// 分隔线。
    pub fn separator() -> Self {
        PopupMenuItem::Separator
    }

    /// 注册点击回调（选中确认或指针点击时执行，随后关闭菜单）。
    pub fn on_click<F>(mut self, handler: F) -> Self
    where
        F: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    {
        match &mut self {
            PopupMenuItem::ElementItem { handler: h, .. } => {
                *h = Some(Rc::new(handler));
            }
            PopupMenuItem::Separator => {}
        }
        self
    }

    /// 禁用（置灰 + 不响应点击）。
    #[allow(dead_code)] // 公开 API：App 现未禁用菜单项，保留给后续调用点
    pub fn disabled(mut self, disabled: bool) -> Self {
        match &mut self {
            PopupMenuItem::ElementItem { disabled: d, .. } => {
                *d = disabled;
            }
            PopupMenuItem::Separator => {}
        }
        self
    }

    fn is_separator(&self) -> bool {
        matches!(self, PopupMenuItem::Separator)
    }
}

/// 菜单项几何：常规项高。
const ITEM_HEIGHT: Pixels = px(26.);
/// 菜单项内边距。
const ITEM_PADDING: Pixels = px(8.);
/// 菜单面板内边距 + 项间距。
const PANEL_PADDING: Pixels = px(4.);

/// 单层下拉菜单视图（`Entity` 承载焦点与键盘状态）。
pub struct PopupMenu {
    focus_handle: FocusHandle,
    /// 开菜单前的焦点句柄，关闭时归还。
    previous_focus: Option<FocusHandle>,
    menu_items: Vec<PopupMenuItem>,
    selected_index: Option<usize>,
    min_width: Option<Pixels>,
}

impl PopupMenu {
    fn new(cx: &mut App) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            previous_focus: None,
            menu_items: Vec::new(),
            selected_index: None,
            min_width: None,
        }
    }

    /// 用构建器新建一个菜单实体（在实体内跑构建闭包，菜单项可经 `window/cx` 读动态状态）。
    pub fn build(
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(Self, &mut Window, &mut Context<Self>) -> Self,
    ) -> Entity<Self> {
        cx.new(|cx| f(Self::new(cx), window, cx))
    }

    /// 追加一个菜单项。
    pub fn item(mut self, item: PopupMenuItem) -> Self {
        self.menu_items.push(item);
        self
    }

    /// 追加一条分隔线（空菜单或已在尾部时不加，尾部分隔线渲染时忽略）。
    pub fn separator(mut self) -> Self {
        if self.menu_items.is_empty() {
            return self;
        }
        if let Some(last) = self.menu_items.last()
            && last.is_separator()
        {
            return self;
        }
        self.menu_items.push(PopupMenuItem::separator());
        self
    }

    /// 菜单最小宽度（默认 160px，保证「标签……快捷键」两栏不挤）。
    #[allow(dead_code)] // 公开 API，App 现用默认宽度
    pub fn min_w(mut self, width: impl Into<Pixels>) -> Self {
        self.min_width = Some(width.into());
        self
    }

    /// 确认选中项：执行回调并关闭菜单（点击项 or Enter）。
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.prevent_default();
        let Some(item) = self.menu_items.get(ix) else {
            return;
        };
        match item {
            PopupMenuItem::ElementItem { disabled: true, .. } => {}
            PopupMenuItem::ElementItem { handler, .. } => {
                self.selected_index = Some(ix);
                if let Some(handler) = handler {
                    handler(&ClickEvent::default(), window, cx);
                }
                self.dismiss(window, cx);
            }
            PopupMenuItem::Separator => {}
        }
    }

    /// 关闭菜单：清选中、发 `DismissEvent`（宿主据此收起）、归还焦点（若点击没带走焦点）。
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_index = None;
        cx.emit(DismissEvent);
        let focus_still_inside = self.focus_handle.contains_focused(window, cx);
        if !focus_still_inside
            && let Some(handle) = self.previous_focus.take()
        {
            window.focus(&handle, cx);
        }
    }

    fn select_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let cur = self.selected_index.unwrap_or(0);
        let prev = self
            .menu_items
            .iter()
            .enumerate()
            .rev()
            .find(|(i, item)| *i < cur && !item.is_separator() && !item_disabled(item));
        match prev {
            Some((ix, _)) => self.set_selected(ix, window, cx),
            None => {
                let last = self
                    .menu_items
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, item)| !item.is_separator() && !item_disabled(item));
                if let Some((ix, _)) = last {
                    self.set_selected(ix, window, cx);
                }
            }
        }
    }

    fn select_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let cur = self.selected_index;
        let next = match cur {
            Some(ix) => self
                .menu_items
                .iter()
                .enumerate()
                .skip(ix + 1)
                .find(|(_, item)| !item.is_separator() && !item_disabled(item)),
            None => self
                .menu_items
                .iter()
                .enumerate()
                .find(|(_, item)| !item.is_separator() && !item_disabled(item)),
        };
        if let Some((ix, _)) = next {
            self.set_selected(ix, window, cx);
        }
    }

    fn set_selected(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_index != Some(ix) {
            self.selected_index = Some(ix);
            cx.notify();
        }
    }

    fn on_key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match e.keystroke.key.as_ref() {
            "up" => self.select_up(window, cx),
            "down" => self.select_down(window, cx),
            "enter" => {
                if let Some(ix) = self.selected_index {
                    self.activate(ix, window, cx);
                }
            }
            "escape" => {
                cx.stop_propagation();
                self.dismiss(window, cx);
            }
            _ => {}
        }
    }

    fn on_mouse_down_out(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // 单层菜单：任何「菜单外」按下都关闭（点中菜单内由项自身处理，不会走到这）。
        let _ = e;
        self.dismiss(window, cx);
    }
}

fn item_disabled(item: &PopupMenuItem) -> bool {
    match item {
        PopupMenuItem::ElementItem { disabled, .. } => *disabled,
        PopupMenuItem::Separator => true,
    }
}

impl EventEmitter<DismissEvent> for PopupMenu {}
impl Focusable for PopupMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PopupMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let (colors, is_dark) = (theme.colors, theme.is_dark());
        let fg: Hsla = colors.fg.into();
        let fg_muted: Hsla = colors.fg_muted.into();
        let accent: Hsla = colors.accent.into();
        let selected_bg = accent.opacity(if is_dark { 0.32 } else { 0.16 });
        let items_count = self.menu_items.len();
        let selected = self.selected_index;

        let items = self
            .menu_items
            .iter()
            .enumerate()
            // 忽略尾部分隔线（与 gpui-component 对齐）
            .filter(|(ix, item)| !(*ix + 1 == items_count && item.is_separator()))
            .map(|(ix, item)| match item {
                PopupMenuItem::Separator => {
                    let border: Hsla = colors.border.into();
                    div()
                        .flex()
                        .h(px(1.))
                        .mx_1()
                        .my_1()
                        .bg(border.opacity(0.6))
                        .into_any_element()
                }
                PopupMenuItem::ElementItem { render, disabled, .. } => {
                    let is_selected = selected == Some(ix);
                    let this = div()
                        .id(ElementId::from(format!("menu-item-{ix}")))
                        .flex()
                        .items_center()
                        .min_h(ITEM_HEIGHT)
                        .px(ITEM_PADDING)
                        .rounded_md()
                        .gap_2()
                        .text_color(if *disabled { fg_muted } else { fg })
                        .when(is_selected, |d| d.bg(selected_bg))
                        .when(!disabled, |d| {
                            d.cursor_pointer()
                                .on_hover(cx.listener(move |this, hovered, _, cx| {
                                    if *hovered {
                                        this.selected_index = Some(ix);
                                    } else if this.selected_index == Some(ix) {
                                        this.selected_index = None;
                                    }
                                    cx.notify();
                                }))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.activate(ix, window, cx);
                                }))
                        });
                    this.child((render)(window, cx)).into_any_element()
                }
            })
            .collect::<Vec<_>>();

        div()
            .id("popup-menu")
            .role(accesskit::Role::Menu)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down_out(cx.listener(Self::on_mouse_down_out))
            .bg(colors.surface)
            .border_1()
            .border_color(colors.border)
            .rounded_md()
            .text_color(fg)
            .occlude()
            .child(
                div()
                    .id("items")
                    .flex()
                    .flex_col()
                    .p(PANEL_PADDING)
                    .gap_y(PANEL_PADDING)
                    .min_w(self.min_width.unwrap_or(px(160.)))
                    .children(items),
            )
    }
}

/// 菜单加载器：给定空菜单 + 宿主窗口，返回装配好的菜单。
type MenuBuilder = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// 触发宿主：把按钮 + 按下即开/关的菜单包成一个元素。
pub struct Dropdown {
    id: ElementId,
    trigger: crate::ui::button::Button,
    menu_builder: MenuBuilder,
}

/// 元素级跨帧状态（按宿主 id 隔离）。
#[derive(Default)]
struct DropdownState {
    open: bool,
    menu: Option<Entity<PopupMenu>>,
}

impl Dropdown {
    fn new(id: impl Into<ElementId>, trigger: crate::ui::button::Button) -> Self {
        Self {
            id: id.into(),
            trigger,
            menu_builder: Rc::new(|menu, _, _| menu),
        }
    }

    fn with_menu(
        mut self,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Self {
        self.menu_builder = Rc::new(f);
        self
    }
}

impl RenderOnce for Dropdown {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state =
            window.use_keyed_state((self.id.clone(), "dropdown"), cx, |_, _| DropdownState::default());
        let view_id = window.current_view();
        let open = state.read(cx).open;

        // 打开时确保已有菜单实体：惰性构建 + 聚焦 + 订阅 DismissEvent（关闭时收敛状态）。
        if open
            && let Some(menu) = {
                match state.read(cx).menu.clone() {
                    Some(menu) => Some(menu),
                    None => {
                        let builder = self.menu_builder.clone();
                        let menu = PopupMenu::build(window, cx, move |pm, window, cx| {
                            (builder)(pm, window, cx)
                        });
                        let previous_focus = window.focused(cx);
                        menu.update(cx, |m, _| m.previous_focus = previous_focus);
                        menu.read(cx).focus_handle(cx).focus(window, cx);

                        let sub_state = state.clone();
                        // 菜单被关闭（选择/Esc/点击外）→ 宿主同时收起。
                        window
                            .subscribe(&menu, cx, move |_, _: &DismissEvent, _window, cx| {
                                sub_state.update(cx, |s, _| {
                                    s.open = false;
                                    s.menu = None;
                                });
                                cx.notify(view_id);
                            })
                            .detach();
                        state.update(cx, |s, _| s.menu = Some(menu.clone()));
                        Some(menu)
                    }
                }
            }
        {
            let menu = menu;
            let _ = &menu; // 借住实体生命周期（已存入 state）
        }

        let menu_view = state.read(cx).menu.clone();

        let trigger = self.trigger;
        div()
            .id(self.id.clone())
            .relative()
            // 触发开关放在「包着按钮的包装层」上（与菜单是兄弟，菜单内按下不会冒泡到这，
            // 不误切换）；菜单自己处理选择/Esc/点击外关闭。
            .child(
                div()
                            .on_mouse_down(
                                MouseButton::Left,
                                move |_, _window, cx| {
                                    let was_open = state.read(cx).open;
                                    state.update(cx, |s, _| {
                                        s.open = !was_open;
                                        if was_open {
                                            s.menu = None;
                                        }
                                    });
                                    cx.notify(view_id);
                                },
                            )
                    .child(trigger),
            )
            .when_some(menu_view, |this, menu| {
                this.child(
                    deferred(
                        anchored()
                            .position_mode(gpui::AnchoredPositionMode::Local)
                            .anchor(Anchor::TopLeft)
                            .snap_to_window_with_margin(Edges::all(px(8.)))
                            .child(menu),
                    )
                    .with_priority(100),
                )
            })
    }
}

impl IntoElement for Dropdown {
    type Element = gpui::ViewElement<Self>;

    #[track_caller]
    fn into_element(self) -> Self::Element {
        gpui::ViewElement::new(self)
    }
}

/// 给按钮挂下拉菜单：`button.dropdown_menu(|menu, _, _| …)` → [`Dropdown`]。
pub trait DropdownMenu: Sized {
    fn dropdown_menu(
        self,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Dropdown;
}

impl DropdownMenu for crate::ui::button::Button {
    fn dropdown_menu(
        self,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Dropdown {
        Dropdown::new(self.element_id(), self).with_menu(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::button::{Button, ButtonVariants as _};
    use gpui::{Context, Modifiers, Render, TestAppContext, VisualTestContext, point, px};
    use std::{cell::Cell, rc::Rc};

    struct Harness {
        clicks: Rc<Cell<usize>>,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let clicks = self.clicks.clone();
            div()
                .id("menu-host")
                .w(px(400.))
                .h(px(300.))
                .child(
                    Button::new("menu-trigger")
                        .label("菜单")
                        .ghost()
                        .w(px(80.))
                        .h(px(30.))
                        .dropdown_menu(move |menu, _, _| {
                            let clicks = clicks.clone();
                            menu.item(
                                PopupMenuItem::element(|_, _| div().child("第一条"))
                                    .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
                            )
                            .separator()
                            .item(PopupMenuItem::element(|_, _| div().child("第二条")))
                        }),
                )
        }
    }

    fn harness(cx: &mut TestAppContext) -> (&mut VisualTestContext, Rc<Cell<usize>>) {
        let clicks = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            move |_, _| Harness { clicks }
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        (cx, clicks)
    }

    /// 触发按钮中心（宿主左上，菜单经绝对定位挂在按钮正下方，不重叠）。
    const TRIGGER: (f32, f32) = (40., 15.);
    /// 首项可见区内的点（菜单顶端约在按钮底 y≈30，首项 y≈34..60）。
    const ITEM: (f32, f32) = (40., 48.);
    /// 菜单与触发区之外的空白点。
    const OUTSIDE: (f32, f32) = (300., 200.);

    #[gpui::test]
    fn click_trigger_opens_and_item_activates(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (cx, clicks) = harness(cx);

        // 关闭态点触发按钮 → 打开菜单。
        cx.simulate_click(point(px(TRIGGER.0), px(TRIGGER.1)), Modifiers::default());
        cx.run_until_parked();
        // 点首项 → 触发回调且只触发一次。
        cx.simulate_click(point(px(ITEM.0), px(ITEM.1)), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clicks.get(), 1, "点菜单首项应触发一次回调");

        // 再点触发按钮（点项后已自动关闭）→ 重新打开；再点首项 → 又触发一次。
        cx.simulate_click(point(px(TRIGGER.0), px(TRIGGER.1)), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_click(point(px(ITEM.0), px(ITEM.1)), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clicks.get(), 2, "重新打开后点首项仍只触发一次");
    }

    #[gpui::test]
    fn keyboard_select_and_enter_activates(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (cx, clicks) = harness(cx);

        cx.simulate_click(point(px(TRIGGER.0), px(TRIGGER.1)), Modifiers::default());
        cx.run_until_parked();
        // 菜单聚焦后：↓ 选中首项 → Enter 确认执行并关闭。
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(clicks.get(), 1, "↓+Enter 应激活首项回调");
    }

    #[gpui::test]
    fn escape_and_click_outside_dismiss(cx: &mut TestAppContext) {
        cx.update(crate::actions::bind_local_keymaps);
        let (cx, clicks) = harness(cx);

        // 打开 → Esc 关闭。关闭后点首项位置回到触发按钮（打开），不再触发回调。
        cx.simulate_click(point(px(TRIGGER.0), px(TRIGGER.1)), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.simulate_click(point(px(ITEM.0), px(ITEM.1)), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clicks.get(), 0, "Esc 关闭后点击不应触发回调");

        // 打开 → 点空白处关闭。
        cx.simulate_click(point(px(TRIGGER.0), px(TRIGGER.1)), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_click(point(px(OUTSIDE.0), px(OUTSIDE.1)), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_click(point(px(ITEM.0), px(ITEM.1)), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clicks.get(), 0, "点空白关闭后点击不应触发回调");
    }
}
