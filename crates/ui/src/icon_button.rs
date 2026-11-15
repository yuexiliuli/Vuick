//! 自绘图标按钮 `ui::icon_button`：单个「图标 + 可选 hover/press 背景」按钮的统一实现。
//!
//! 收敛 shell 标题栏窗口控制钮/侧栏开关等手搓样板（flex 居中 + keyed hover/press +
//! `hoverable` 弹簧背景），替换后保持**字节等价**的可观测行为：
//! - 元素 id + `debug_selector` 由 id 标签派生（`element_id_label`）；
//! - hover 背景走 [`crate::motion::hoverable`]（单一来源：keyed 态 + `hover_spring` 淡入）；
//! - 按压为**可选** keyed 态（`.pressed(Some(fill))`）：按下/抬起/移出三事件更新
//!   pressed + `cx.notify(view_id)`，与 shell/music 现状一致（等效 gpui CSS `active` 的
//!   「按下即换底、松开还原」模型）；图标色闭包收到 `(hovered, pressed)` 动态取色。
//!
//! # 构建时机
//! 本组件采用**急切构建**（`.build(window, cx)`），在调用方 render 期同步解析 keyed
//! hover/press 态并落盘——与旧手搓实现在调用方 `use_keyed_state` 同一时机，保证
//! 「测试探针（如 shell `last_title_ctrl_fg`）读到的是真实渲染路径解析出的颜色」。
//! （对比 `RenderOnce` 延迟构建：状态键会被组件类型名命名空间隔离，调用方无法以
//! 裸键读取/写入同一 keyed 态，探针会断。）
//!
//! # gpui 语义注意（on_mouse_down ↔ on_click）
//! 元素注册 `on_mouse_down(Left)`（如 `.pressed(...)` 内部压住态接线）后，gpui 的
//! `on_click` 鼠标点击合成会被压制（手搓 div 同证，非本组件缺陷）。因此 `.pressed()`
//! 与 `.on_click()` **不要在同一按钮上并用**——与旧调用点分工一致：标题钮 pressed 无
//! click（A/C），侧栏开关/圆角钮 click/down 无 pressed（B/I）。

use std::rc::Rc;

use gpui::{
    AbsoluteLength, AnyElement, App, ClickEvent, CursorStyle, ElementId, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
    transparent_black, white,
};

use crate::motion::{element_id_label, hoverable};
use crate::ui::icon::{Icon, IconName};

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type DownHandler = Rc<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;
type ColorFor = Box<dyn Fn(bool, bool) -> Hsla>;

/// 图标按钮构建器（`ui::icon_button(id, icon)` 起步，链式配置后 `.build(window, cx)`）。
pub struct IconButton {
    id: ElementId,
    icon: IconName,
    w: f32,
    h: f32,
    radius: Option<AbsoluteLength>,
    icon_size: f32,
    color: Option<Hsla>,
    icon_color_for: Option<ColorFor>,
    hover_fill: Option<Hsla>,
    hover_animated: bool,
    pressed: Option<Hsla>,
    bg: Option<Hsla>,
    native: Option<gpui::WindowControlArea>,
    cursor: bool,
    on_click: Option<ClickHandler>,
    on_mouse_down: Option<(MouseButton, DownHandler)>,
}

/// `ui::icon_button(id, icon)` 便捷入口。
pub fn icon_button(id: impl Into<ElementId>, icon: IconName) -> IconButton {
    IconButton::new(id, icon)
}

impl IconButton {
    fn new(id: impl Into<ElementId>, icon: IconName) -> Self {
        Self {
            id: id.into(),
            icon,
            w: 24.0,
            h: 24.0,
            radius: None,
            icon_size: 16.0,
            color: Some(white()),
            icon_color_for: None,
            hover_fill: None,
            hover_animated: true,
            pressed: None,
            bg: None,
            native: None,
            cursor: true,
            on_click: None,
            on_mouse_down: None,
        }
    }

    /// 正方形按钮（宽 = 高 = `size`）。
    pub fn square(mut self, size: f32) -> Self {
        self.w = size;
        self.h = size;
        self
    }

    /// 矩形按钮（宽 `w` × 高 `h`）。
    pub fn wh(mut self, w: f32, h: f32) -> Self {
        self.w = w;
        self.h = h;
        self
    }

    /// 圆角半径；**不设 = 无圆角**（原生标题钮）。可传 `gpui::rems(x)`/`gpui::px(x)`（`rems`
    /// 与 gpui `rounded_md()` 等 Tailwind 档位同构，可逐字还原）。
    pub fn radius<R: Into<AbsoluteLength>>(mut self, radius: R) -> Self {
        self.radius = Some(radius.into());
        self
    }

    /// 图标尺寸（px）。
    pub fn icon_size(mut self, size: f32) -> Self {
        self.icon_size = size;
        self
    }

    /// 静态图标色（B 侧栏开关 / 静态钮）。被 [`Self::icon_color_for`] 覆盖。
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    /// 动态图标色：按 `(hovered, pressed)` 取色（A/C 标题钮「注入感」）。
    /// 闭包在 `.build()`（调用方 render 期）同步调用——调用方可在闭包内落盘探针。
    pub fn icon_color_for(mut self, f: impl Fn(bool, bool) -> Hsla + 'static) -> Self {
        self.icon_color_for = Some(Box::new(f));
        self
    }

    /// 悬停背景；`Some` = 启用 keyed hover + 背景淡入（`hover_animated(true)` 走
    /// [`crate::motion::hoverable`] 弹簧，`false` 立即换底）。`None` = 无 hover 态/背景
    /// （纯静态钮）。
    pub fn hover_fill(mut self, fill: Option<Hsla>) -> Self {
        self.hover_fill = fill;
        self
    }

    /// hover 背景是否弹簧淡入（默认 true）。`false` = 悬停立即切到 `hover_fill`。
    pub fn hover_animated(mut self, animated: bool) -> Self {
        self.hover_animated = animated;
        self
    }

    /// 启用 keyed 按压态：`Some(fill)` = 按下时背景直接切到 `fill`（松开/移出还原），
    /// 并接线 `on_mouse_down/up/up_out` 更新 pressed + `cx.notify(view_id)`。
    /// `None` = 无按压态（不建 keyed press、不接线）。**压住不放时 pressed 背景优先于
    /// hover 弹簧**（等效 gpui CSS `active`；shell A 的 `.active(...)` 由此模型替代）。
    pub fn pressed(mut self, fill: Option<Hsla>) -> Self {
        self.pressed = fill;
        self
    }

    /// 静态背景（无 hover 路径时用，如圆角钮的 `bg_secondary`）。`hover_fill` 为
    /// `Some` 时 hover 路径接管背景，本值不作常态底。
    pub fn bg(mut self, bg: Option<Hsla>) -> Self {
        self.bg = bg;
        self
    }

    /// 原生窗口控制区（`window_control_area`，标题栏最小化/最大化/关闭钮）。参数化
    /// 调用方逐钮指定的命中区（`Min`/`Max`/`Close`，语义同旧手写 `.window_control_area`）。
    pub fn native(mut self, area: gpui::WindowControlArea) -> Self {
        self.native = Some(area);
        self
    }

    /// 光标：`true` = 手型（默认）；`false` = 保留平台默认箭头（music 标题钮）。
    pub fn cursor(mut self, on: bool) -> Self {
        self.cursor = on;
        self
    }

    /// 点击回调（指针点击）。
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }

    /// 鼠标按下回调（`MouseButton` 分键）。
    pub fn on_mouse_down(
        mut self,
        button: MouseButton,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_mouse_down = Some((button, Rc::new(handler)));
        self
    }

    /// 急切构建：在调用方 render 期同步解析 keyed hover/press、落盘背景/图标色，
    /// 返回就绪的 `AnyElement`。见模块文档「构建时机」。
    pub fn build(self, window: &mut Window, cx: &mut App) -> AnyElement {
        let id = self.id.clone();
        let label = element_id_label(&id);
        let hover_key = ElementId::from(format!("{label}-hover"));
        let press_key = ElementId::from(format!("{label}-press"));
        let view_id = window.current_view();
        let hovered_s = window.use_keyed_state(hover_key, cx, |_, _| false);
        let pressed_fill = self.pressed;
        let static_bg = self.bg;
        let pressed_s = pressed_fill.map(|_| window.use_keyed_state(press_key, cx, |_, _| false));
        let hovered = *hovered_s.read(cx);
        let pressed = pressed_s.as_ref().map(|s| *s.read(cx)).unwrap_or(false);

        // 图标色：动态闭包优先，其次静态色，最后回退 Icon 默认白。
        let icon_color = match &self.icon_color_for {
            Some(f) => f(hovered, pressed),
            None => self.color.unwrap_or(white()),
        };

        let icon = Icon::new(self.icon)
            .size(px(self.icon_size))
            .text_color(icon_color);

        let mut el = div()
            .id(id.clone())
            .debug_selector(move || label.clone())
            .w(px(self.w))
            .h(px(self.h))
            .flex()
            .items_center()
            .justify_center()
            .when_some(self.radius, |el, r| el.rounded(r))
            .when(self.cursor, |el| el.cursor(CursorStyle::PointingHand))
            .when_some(self.native, |el, area| el.window_control_area(area));

        // keyed 按压态接线（`.pressed(Some(..))` 配置时每帧都注册，按下态由事件驱动翻转）。
        if let Some(st) = &pressed_s {
            let st_down = st.clone();
            let st_up = st.clone();
            let st_out = st.clone();
            el = el
                .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                    st_down.update(cx, |p, _| *p = true);
                    cx.notify(view_id);
                })
                .on_mouse_up(MouseButton::Left, move |_ev, _window, cx| {
                    st_up.update(cx, |p, _| *p = false);
                    cx.notify(view_id);
                })
                .on_mouse_up_out(MouseButton::Left, move |_ev, _window, cx| {
                    st_out.update(cx, |p, _| *p = false);
                    cx.notify(view_id);
                });
        }
        if let Some((button, handler)) = self.on_mouse_down {
            el = el.on_mouse_down(button, move |event, window, cx| handler(event, window, cx));
        }
        if let Some(handler) = self.on_click {
            el = el.on_click(move |event, window, cx| handler(event, window, cx));
        }

        // 背景优先级：pressed（压住）> hover（弹簧淡入 / 立即）> bg（静态）> 透明。
        if pressed && pressed_s.is_some() {
            el.bg(pressed_fill.unwrap()).child(icon).into_any_element()
        } else if let Some(fill) = self.hover_fill {
            let base = match static_bg {
                Some(b) => el.bg(b),
                None => el,
            };
            if self.hover_animated {
                hoverable(base, id, fill, window, cx)
                    .child(icon)
                    .into_any_element()
            } else {
                let bg = if hovered {
                    fill
                } else {
                    static_bg.unwrap_or(transparent_black())
                };
                base.bg(bg).child(icon).into_any_element()
            }
        } else {
            let base = match static_bg {
                Some(b) => el.bg(b),
                None => el,
            };
            base.child(icon).into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Modifiers, Render, TestAppContext, VisualTestContext, Window, point, px};
    use std::{cell::Cell, rc::Rc};

    struct Harness {
        clicks: Rc<Cell<usize>>,
        render_count: Rc<Cell<usize>>,
        last_state: Rc<Cell<(bool, bool)>>,
        hover_fill: Option<Hsla>,
        pressed: Option<Hsla>,
        icon_color_for: bool,
    }

    impl Render for Harness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.render_count.set(self.render_count.get() + 1);
            let clicks = self.clicks.clone();
            let last = self.last_state.clone();
            let mut btn = icon_button("test-icon-btn", IconName::Bell)
                .wh(40., 32.)
                .icon_size(14.)
                .hover_fill(self.hover_fill)
                .pressed(self.pressed)
                .on_click(move |_, _, _| clicks.set(clicks.get() + 1));
            if self.icon_color_for {
                btn = btn.icon_color_for(move |hovered, pressed| {
                    last.set((hovered, pressed));
                    if hovered || pressed {
                        gpui::white()
                    } else {
                        gpui::hsla(0., 0., 0.6, 1.)
                    }
                });
            }
            // 按钮故意避开窗口原点（测试指针默认在 (0,0)），否则初始即悬停。
            div().size(px(400.)).child(
                div()
                    .absolute()
                    .left(px(80.))
                    .top_0()
                    .child(btn.build(window, cx)),
            )
        }
    }

    struct HarnessState {
        clicks: Rc<Cell<usize>>,
        render_count: Rc<Cell<usize>>,
        last_state: Rc<Cell<(bool, bool)>>,
    }

    fn harness(
        cx: &mut TestAppContext,
        pressed: Option<Hsla>,
    ) -> (&mut VisualTestContext, Rc<HarnessState>) {
        let clicks = Rc::new(Cell::new(0));
        let render_count = Rc::new(Cell::new(0));
        let last_state = Rc::new(Cell::new((false, false)));
        let state = Rc::new(HarnessState {
            clicks: clicks.clone(),
            render_count: render_count.clone(),
            last_state: last_state.clone(),
        });
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            let render_count = render_count.clone();
            let last_state = last_state.clone();
            move |_, _| Harness {
                clicks,
                render_count,
                last_state,
                hover_fill: Some(gpui::hsla(0., 0., 0.8, 0.3)),
                pressed,
                icon_color_for: true,
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        (cx, state)
    }

    #[gpui::test]
    fn renders_with_id_and_block_size(cx: &mut TestAppContext) {
        let (cx, _) = harness(cx, None);
        let bounds = cx
            .debug_bounds("test-icon-btn")
            .expect("应渲染出 test-icon-btn 命中盒");
        assert_eq!(bounds.size.width, px(40.), "宽 40");
        assert_eq!(bounds.size.height, px(32.), "高 32");
    }

    #[gpui::test]
    fn click_fires_on_click(cx: &mut TestAppContext) {
        // 注：gpui 语义——元素挂 `on_mouse_down(Left)` 后 `on_click` 合成被压制
        // （手搓 div 同证）。故 on_click 在**无 pressed** 路径上验证（对应 shell 侧栏开关 /
        // music 圆角钮形态）；pressed keyed 态另由 pressed 测试覆盖。真实调用点亦如此
        // 分工（A/C：pressed 无 click；B/I：click/down 无 pressed）。
        let (cx, state) = harness(cx, None);
        let btn = cx
            .debug_bounds("test-icon-btn")
            .expect("应渲染 test-icon-btn");
        cx.simulate_click(btn.center(), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(state.clicks.get(), 1, "点击应触发一次 on_click");
    }

    #[gpui::test]
    fn hover_toggles_state_and_notifies_host(cx: &mut TestAppContext) {
        let (cx, state) = harness(cx, Some(gpui::hsla(0., 0., 0.6, 0.5)));
        let base = state.render_count.get();
        assert_eq!(state.last_state.get(), (false, false), "初始空闲");

        // 移入目标 → hover=true 且宿主重绘（icon_color_for 收到 (true,false)）。
        let btn = cx
            .debug_bounds("test-icon-btn")
            .expect("应渲染 test-icon-btn");
        cx.simulate_mouse_move(btn.center(), None, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert!(
            state.render_count.get() > base,
            "悬停应触发宿主重绘 base={base} now={}",
            state.render_count.get()
        );
        assert_eq!(
            state.last_state.get(),
            (true, false),
            "悬停态应注入 (true,false)"
        );

        // 移出 → 回落空闲。
        cx.simulate_mouse_move(point(px(0.), px(0.)), None, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert_eq!(state.last_state.get(), (false, false), "移出后回落空闲");
    }

    #[gpui::test]
    fn pressed_keyed_state_reaches_icon_color_for(cx: &mut TestAppContext) {
        let (cx, state) = harness(cx, Some(gpui::hsla(0., 0., 0.6, 0.5)));
        let btn = cx
            .debug_bounds("test-icon-btn")
            .expect("应渲染 test-icon-btn");

        // 移入目标 → hover=true（icon_color_for 收到 (true,false)）。
        cx.simulate_mouse_move(btn.center(), None, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert_eq!(
            state.last_state.get(),
            (true, false),
            "悬停应注入 (true,false)"
        );

        // 按下 → pressed=true（光标仍在钮上）→ 动态色收到 (true,true)，按压底优先。
        cx.simulate_mouse_down(btn.center(), MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert_eq!(
            state.last_state.get(),
            (true, true),
            "按下应注入 (true,true)"
        );

        // 松开（仍悬停）→ pressed=false 回落 hover（true,false）；移出 → 全空闲。
        cx.simulate_mouse_up(btn.center(), MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert_eq!(state.last_state.get(), (true, false), "松开后回落 hover 态");
        cx.simulate_mouse_move(point(px(0.), px(0.)), None, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert_eq!(state.last_state.get(), (false, false), "移出后回落空闲");
    }

    #[gpui::test]
    fn static_color_path_without_hover_uses_bg(cx: &mut TestAppContext) {
        // 无 hover_fill / 无 pressed 的静态钮：`.color` + `.bg` 静态度（site I 形态）。
        struct StaticHarness {
            clicks: Rc<Cell<usize>>,
        }
        impl Render for StaticHarness {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let clicks = self.clicks.clone();
                let btn = icon_button("static-icon-btn", IconName::Settings)
                    .square(36.)
                    .icon_size(17.)
                    .color(gpui::hsla(0.6, 0.6, 0.6, 1.))
                    .bg(Some(gpui::hsla(0.6, 0.6, 0.2, 1.)))
                    .on_click(move |_, _, _| clicks.set(clicks.get() + 1))
                    .build(window, cx);
                div().child(btn)
            }
        }
        let clicks = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            move |_, _| StaticHarness { clicks }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        let btn = cx
            .debug_bounds("static-icon-btn")
            .expect("应渲染 static-icon-btn");
        assert_eq!(btn.size.width, px(36.), "36 方钮宽");
        assert_eq!(btn.size.height, px(36.), "36 方钮高");
        cx.simulate_click(btn.center(), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clicks.get(), 1, "静态钮点击仍触发 on_click");
    }
}
