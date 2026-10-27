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
    Anchor, AnyElement, AnimationExt, App, AppContext as _, ClickEvent, Context, DismissEvent,
    Edges, ElementId, Entity, EventEmitter, FocusHandle, Focusable, Hsla, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Pixels, Render,
    RenderOnce, StatefulInteractiveElement, Styled, Subscription, Window, accesskit, anchored,
    deferred, div, prelude::FluentBuilder as _, px,
};

use crate::ui::theme::{ChromeColors, Theme};

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
        /// 选中/点击后**不关闭菜单**（保留焦点同实体换内容用，如「打开最近」More/返回）；
        /// 默认 false：保持既有「执行回调后关闭」语义。opt-in。
        keep_open: bool,
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
            keep_open: false,
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

    /// 点击/选中后**不关闭菜单**（opt-in）：`PopupMenu::activate` 据此跳过 dismiss，
    /// 焦点保留（「打开最近…」用它保持文件菜单展开）；其余项不调用 = 既有行为不变。
    pub fn keep_open(mut self) -> Self {
        match &mut self {
            PopupMenuItem::ElementItem { keep_open, .. } => {
                *keep_open = true;
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
}

impl PopupMenu {
    fn new(cx: &mut App) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            previous_focus: None,
            menu_items: Vec::new(),
            selected_index: None,
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

    /// 记下打开菜单前的焦点句柄（关闭时归还）。`Dropdown` 在元素内直接写字段；宿主在
    /// 窗口直接挂右键菜单时用它显式设置。
    pub fn set_previous_focus(&mut self, handle: Option<FocusHandle>) {
        self.previous_focus = handle;
    }

    /// 测试/诊断：返回 `(总条目数, 分割线数)`——总条目**含**分割线（项数 = 减分割线数），
    /// 用于断言菜单结构与分组（shell 文件树右键菜单按组用分隔线隔开）。`ElementItem`
    /// 的标签在渲染闭包内无法枚举，这里只数条目与分割线。
    #[allow(dead_code)] // shell 测试断言用
    pub fn layout_probe(&self) -> (usize, usize) {
        let separators = self
            .menu_items
            .iter()
            .filter(|i| i.is_separator())
            .count();
        (self.menu_items.len(), separators)
    }

    /// 测试/诊断：第 `index` 项是否被禁用（含分割线下标；越界/分割线 → `false`）。
    /// shell PDF 右键「唯一页时删除页面禁用」断言用。
    #[allow(dead_code)] // shell 测试断言用
    pub fn is_item_disabled(&self, index: usize) -> bool {
        match self.menu_items.get(index) {
            Some(PopupMenuItem::ElementItem { disabled, .. }) => *disabled,
            _ => false,
        }
    }

    /// 确认选中项：执行回调并关闭菜单（点击项 or Enter）。
    /// `keep_open` 项只执行回调、**不关闭**（如「打开最近…」保持文件菜单展开）。
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.prevent_default();
        let Some(item) = self.menu_items.get(ix) else {
            return;
        };
        match item {
            PopupMenuItem::ElementItem { disabled: true, .. } => {}
            PopupMenuItem::ElementItem {
                handler,
                keep_open,
                ..
            } => {
                self.selected_index = Some(ix);
                if let Some(handler) = handler {
                    handler(&ClickEvent::default(), window, cx);
                }
                if !keep_open {
                    self.dismiss(window, cx);
                }
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
        let surface: Hsla = colors.surface.into();
        let fg: Hsla = colors.fg.into();
        // 禁用项文字走独立语义 token（比 fg_muted 更弱一档，见 P1-7）。
        let fg_disabled: Hsla = colors.fg_disabled.into();
        // Win11 中性菜单高亮：灰填充（非强调蓝 tint）。
        let selected_bg: Hsla = ChromeColors::for_mode(is_dark).menu_highlight.into();
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
                        .rounded_sm()
                        .gap_2()
                        .text_color(if *disabled { fg_disabled } else { fg })
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
                        })
                        // P2-10：选中/悬停高亮淡入（键盘↑↓ 与鼠标同一曲线）。
                        .with_spring(
                            format!("menu-item-bg-{ix}"),
                            crate::ui::motion::hover_spring(if is_selected { 1.0 } else { 0.0 }),
                            move |d, v| d.bg(selected_bg.opacity(v)),
                        );
                    this.child((render)(window, cx)).into_any_element()
                }
            })
            .collect::<Vec<_>>();

        let items_column = div()
            .id("items")
            .flex()
            .flex_col()
            .p(PANEL_PADDING)
            .gap_y(PANEL_PADDING)
            .min_w(px(160.))
            .children(items);

        div()
            .id("popup-menu")
            .role(accesskit::Role::Menu)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down_out(cx.listener(Self::on_mouse_down_out))
            .bg(surface.opacity(if is_dark {
                crate::ui::theme::FLOAT_ACRYLIC_ALPHA_DARK
            } else {
                crate::ui::theme::FLOAT_ACRYLIC_ALPHA_LIGHT
            }))
            .border_1()
            .border_color(colors.border)
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(8.), gpui::hsla(0., 0., 0., 0.20))
                    .blur_radius(px(14.)),
                gpui::BoxShadow::new(px(0.), px(2.), gpui::hsla(0., 0., 0., 0.14))
                    .blur_radius(px(4.)),
            ])
            .rounded_lg()
            .text_color(fg)
            .child(items_column)
    }
}

/// 右键上下文菜单助手：一处收敛「建 `PopupMenu` + `set_previous_focus` + `focus` +
/// `subscribe(DismissEvent)` + 宿主写好状态」近 30 行×2 同构样板（shell tab 栏右键 /
/// 媒体锁定右键）。
///
/// 封装闭环：
/// 1. `PopupMenu::build(window, cx, items)`——`items` 在菜单实体内运行（可 push 条目 /
///    读动态状态）；
/// 2. **焦点管理**：记下开菜单前的焦点（`PopupMenu` 关闭时归还）+ 聚焦菜单；
/// 3. **dismiss 订阅**：`DismissEvent`（选择 / Esc / 点击外）→ 调 `on_dismiss`
///    （宿主在里面对照 emitter 做**身份校验**后清自己的槽位 + `cx.notify()`），
///    杜绝旧菜单的关闭误清新菜单。
///
/// 返回 `(menu, sub)` 句柄：宿主把 `menu` 与 `position` 存入自己的槽位（渲染时
/// `deferred(anchored(Window)).position(pos)` 定位），`sub` 持住订阅；槽位字段存闭仍是
/// 宿主职责（`Context<T>` 拿不到 `&mut T`，见 §2.6 派生状态副作用纪律）。
pub fn spawn_context_menu<T>(
    window: &mut Window,
    cx: &mut Context<T>,
    items: impl FnOnce(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    on_dismiss: impl Fn(&mut T, &Entity<PopupMenu>, &mut Context<T>) + 'static,
) -> (Entity<PopupMenu>, Subscription)
where
    T: 'static,
{
    let menu = PopupMenu::build(window, cx, items);

    // 记下开菜单前的焦点（关闭归还）+ 聚焦菜单。
    let previous_focus = window.focused(cx);
    let focus_handle = menu.read(cx).focus_handle(cx);
    menu.update(cx, |m, _| m.set_previous_focus(previous_focus));
    window.focus(&focus_handle, cx);

    // 关闭（选择/Esc/点击外）→ 宿主按 emitter 身份校验清理自己的菜单槽位。
    let sub = cx.subscribe(&menu, move |this, emitter, _: &DismissEvent, cx| {
        on_dismiss(this, &emitter, cx);
    });

    (menu, sub)
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
    use gpui::{Context, Modifiers, Point, Render, TestAppContext, VisualTestContext, point, px};
    use std::{cell::Cell, rc::Rc};

    struct Harness {
        clicks: Rc<Cell<usize>>,
    }

    /// 直接持有并渲染 `PopupMenu` 实体的宿主（`Dropdown` 也是把它挂进元素树，
    /// 这里省去触发按钮，聚焦测试的滚动容器/滚动条结构）。
    struct MenuHost {
        menu: Entity<PopupMenu>,
    }

    impl Render for MenuHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("menu-host")
                .debug_selector(|| "menu-host".into())
                .w(px(400.))
                .h(px(400.))
                .child(self.menu.clone())
        }
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
        cx.update(crate::test_support::bind_local_keymaps);
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
        cx.update(crate::test_support::bind_local_keymaps);
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
        cx.update(crate::test_support::bind_local_keymaps);
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

    /// `keep_open` 项点击后菜单**不关闭**（DismissEvent 不发、焦点保留）；普通项
    /// 点击仍关闭（既有语义）。
    #[gpui::test]
    fn keep_open_item_does_not_dismiss(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let dismisses = Rc::new(Cell::new(0usize));
        let (_, cx) = cx.add_window_view({
            let dismisses = dismisses.clone();
            move |window, cx| {
                let menu = PopupMenu::build(window, cx, |menu, _w, _cx| {
                    let keep = PopupMenuItem::element(|_, _| div().child("More"))
                        .keep_open()
                        .on_click(|_, _, _| {});
                    let close = PopupMenuItem::element(|_, _| div().child("关闭项"))
                        .on_click(|_, _, _| {});
                    menu.item(keep).item(close)
                });
                // 订阅 DismissEvent 计数：keep_open 项不得触发、普通项应触发。
                let d = dismisses.clone();
                window
                    .subscribe(&menu, cx, move |_, _: &DismissEvent, _, _| {
                        d.set(d.get() + 1);
                    })
                    .detach();
                MenuHost { menu }
            }
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        // 首项（"More"，keep_open）：点击后菜单不关闭（dismiss 计数仍 0）。
        cx.simulate_click(point(px(40.), px(17.)), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            dismisses.get(),
            0,
            "keep_open 项点击不应触发菜单关闭（dismiss）"
        );
        // 第二项（普通项）：点击应关闭菜单（dismiss 计数 1）。
        cx.simulate_click(point(px(40.), px(47.)), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(dismisses.get(), 1, "普通项点击应关闭菜单（dismiss=1）");
    }

    // ---------------------------------------------------------------------
    // `spawn_context_menu`：右键菜单生命周期闭环（焦点管理 + dismiss 身份校验清槽位）。
    // ---------------------------------------------------------------------

    /// 右键菜单宿主：放 mimic shell 的「entity / pos / sub」三槽位，作
    /// `spawn_context_menu` 生命周期载体。
    struct ContextMenuHost {
        menu_entity: Option<Entity<PopupMenu>>,
        menu_pos: Option<Point<Pixels>>,
        menu_sub: Option<Subscription>,
    }

    impl Render for ContextMenuHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let content = match &self.menu_entity {
                Some(menu) => div()
                    .id("ctx-menu-host")
                    .size_full()
                    .child(menu.clone())
                    .into_any_element(),
                None => div().id("ctx-menu-host").size_full().into_any_element(),
            };
            content
        }
    }

    /// 打开：`spawn_context_menu` 建菜单 + 记/归还焦点 + 订阅；Esc 关闭 → dismiss
    /// 订阅按 emitter 身份校验命中当前槽位 → 清 entity/pos 槽位（宿主重绘）。
    #[gpui::test]
    fn context_menu_spawn_focuses_and_dismiss_cleans_slot(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let mut host = ContextMenuHost {
                menu_entity: None,
                menu_pos: None,
                menu_sub: None,
            };
            let (menu, sub) = spawn_context_menu(
                window,
                cx,
                |menu, _, _| {
                    menu.item(
                        PopupMenuItem::element(|_, _| div().child("右键菜单项"))
                            .on_click(|_, _, _| {}),
                    )
                },
                // 宿主侧：emitter 身份校验命中当前槽位才清理（旧菜单关闭不会误清新菜单）。
                |this: &mut ContextMenuHost,
                 emitter: &Entity<PopupMenu>,
                 cx: &mut Context<ContextMenuHost>| {
                    if this.menu_entity.as_ref() == Some(&emitter) {
                        this.menu_entity = None;
                        this.menu_pos = None;
                    }
                    cx.notify();
                },
            );
            host.menu_pos = Some(point(px(10.), px(10.)));
            host.menu_entity = Some(menu);
            host.menu_sub = Some(sub);
            host
        });
        // 打开态：菜单已聚焦且槽位写满。
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let (open_entity, open_pos) = cx.update(|_, cx| {
            (view.read(cx).menu_entity.is_some(), view.read(cx).menu_pos.is_some())
        });
        assert!(open_entity, "打开后实体槽位应有菜单");
        assert!(open_pos, "打开后位置槽位应有坐标");
        // Esc 关闭菜单 → DismissEvent → 订阅按身份校验清理槽位。
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        let (gone_entity, gone_pos) = cx.update(|_, cx| {
            (view.read(cx).menu_entity.is_none(), view.read(cx).menu_pos.is_none())
        });
        assert!(gone_entity, "dismiss 后实体槽位应被清理");
        assert!(gone_pos, "dismiss 后位置槽位应被清理");
    }
}
