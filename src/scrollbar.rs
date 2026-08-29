//! 美化滚动条：跟随 [`ScrollHandle`] 的自绘垂直/横向滚动条。
//!
//! - [`Scrollbar`]：垂直（消息区右侧）。
//! - [`horizontal_scrollbar`]：横向（表格超宽时表格下方），支持**拖拽滑块**。
//!
//! 用法：放在滚动容器旁边/下方（同宽高），例如：
//! ```ignore
//! div().flex().flex_row().flex_1()
//!     .child(scroll_container)      // .overflow_y_scroll().track_scroll(&handle)
//!     .child(Scrollbar::new(handle))
//! ```
//! 内容不超高/超宽时不显示；仅在可滚动时画一条淡轨道 + 圆角半透明滑块。

use gpui::{
    div, point, px, rgba, size, App, Bounds, Corners, CursorStyle, Element, ElementId,
    GlobalElementId, InspectorElementId, InteractiveElement, IntoElement, LayoutId, MouseButton,
    ParentElement, Pixels, ScrollHandle, Style, Styled, Window,
};
use std::cell::RefCell;
use std::rc::Rc;

/// 滑块宽度 / 高度
const BAR_W: Pixels = px(6.);
/// 滑块圆角半径
const THUMB_RADIUS: Pixels = px(3.);
/// 滑块距边缘间距
const GAP_RIGHT: Pixels = px(3.);
/// 滑块最小尺寸（太小不好看/不好点）
const THUMB_MIN: Pixels = px(26.);

/// 横向滚动条的共享状态：滚动句柄 + 拖拽抓取偏移（跨帧保留，元素每帧重建也不丢）。
///
/// 由持有表格的调用方保存一个实例（每个表格一个），渲染时仅借用。
#[derive(Clone)]
pub struct TableScrollState {
    pub handle: ScrollHandle,
    /// `Some(px)` = 正在拖拽，值为"指针相对滑块中心"的水平偏移
    pub drag: Rc<RefCell<Option<Pixels>>>,
}

impl TableScrollState {
    pub fn new() -> Self {
        Self {
            handle: ScrollHandle::new(),
            drag: Rc::new(RefCell::new(None)),
        }
    }
}

impl Default for TableScrollState {
    fn default() -> Self {
        Self::new()
    }
}

/// 计算横向滚动条几何；内容没超宽时返回 `None`（无需绘制/可滚动）。
fn hscroll_geometry(
    handle: &ScrollHandle,
) -> Option<(Pixels, Pixels, Pixels, Pixels)> {
    let bounds = handle.bounds();
    let viewport_w = bounds.size.width;
    if viewport_w <= px(0.) {
        return None;
    }
    let max_scroll = handle.max_offset().width;
    let content_w = viewport_w + max_scroll;
    if content_w <= viewport_w + px(1.) {
        return None;
    }
    let bar_w = (viewport_w - GAP_RIGHT * 2.).max(px(0.));
    let frac = ((-handle.offset().x).max(px(0.)) / max_scroll).clamp(0., 1.);
    let thumb_w = (bar_w * (viewport_w / content_w)).max(THUMB_MIN).min(bar_w);
    let thumb_x = GAP_RIGHT + (bar_w - thumb_w) * frac;
    Some((bar_w, thumb_w, thumb_x, max_scroll))
}

/// 把"滑块中心相对轨道左端"的偏移换算成滚动位置并写回句柄。
fn set_thumb_from_center(
    px_from_bar_left: Pixels,
    bar_w: Pixels,
    thumb_w: Pixels,
    max_scroll: Pixels,
    handle: &ScrollHandle,
) {
    let travel = (bar_w - thumb_w).max(px(0.));
    if travel <= px(0.) || max_scroll <= px(0.) {
        return;
    }
    let frac = (px_from_bar_left / travel).clamp(0., 1.);
    handle.set_offset(point(-(max_scroll * frac), handle.offset().y));
}

/// 横向滚动条（div 版，可拖拽）：跟随 [`ScrollHandle`] 自绘在横向滚动容器下方。
///
/// - 内容没超宽时不绘制；
/// - 点击/拖动均可滑到对应位置（拖动用 `state.drag` 跨帧记录抓取点）。
pub fn horizontal_scrollbar(
    state: &TableScrollState,
    id: impl Into<ElementId>,
) -> impl IntoElement {
    let handle = state.handle.clone();
    let drag = state.drag.clone();

    let mut el = div()
        .id(id)
        .relative()
        // 约 3px 高的一条细线，半透明，贴着容器底部浮一层
        .h(px(3.))
        .w_full()
        .cursor(CursorStyle::PointingHand);

    if let Some((bar_w, thumb_w, thumb_x, _)) = hscroll_geometry(&handle) {
        el = el
            .child(
                div()
                    .absolute()
                    .left(GAP_RIGHT)
                    .top(px(0.))
                    .w(bar_w)
                    .h(px(2.))
                    .rounded(px(1.))
                    .bg(rgba(0x93a0aa14)),
            )
            .child(
                div()
                    .absolute()
                    .left(thumb_x)
                    .top(px(0.))
                    .w(thumb_w)
                    .h(px(2.))
                    .rounded(px(1.))
                    .bg(rgba(0x64748b4a)),
            );
    }

    el = el
        .on_mouse_down(
            MouseButton::Left,
            {
                let handle = handle.clone();
                let drag = drag.clone();
                move |ev, window, _cx| {
                    let Some((bar_w, thumb_w, _, max_scroll)) = hscroll_geometry(&handle) else {
                        return;
                    };
                    let bar_left = handle.bounds().left() + GAP_RIGHT;
                    let rel = ev.position.x - bar_left;
                    // 记录抓取点相对滑块中心的偏移，拖动时保持
                    *drag.borrow_mut() = Some(rel - thumb_w / 2.0);
                    set_thumb_from_center(rel, bar_w, thumb_w, max_scroll, &handle);
                    window.refresh();
                }
            },
        )
        .on_mouse_move(
            {
                let handle = handle.clone();
                let drag = drag.clone();
                move |ev, window, _cx| {
                    if drag.borrow().is_none() {
                        return;
                    }
                    let Some((bar_w, thumb_w, _, max_scroll)) = hscroll_geometry(&handle) else {
                        *drag.borrow_mut() = None;
                        return;
                    };
                    let grab = drag.borrow().unwrap();
                    let bar_left = handle.bounds().left() + GAP_RIGHT;
                    let rel = ev.position.x - bar_left - grab;
                    set_thumb_from_center(rel, bar_w, thumb_w, max_scroll, &handle);
                    window.refresh();
                }
            },
        )
        .on_mouse_up(
            MouseButton::Left,
            {
                let drag = drag.clone();
                move |_ev, _window, _cx| {
                    *drag.borrow_mut() = None;
                }
            },
        )
        .on_mouse_up_out(
            MouseButton::Left,
            {
                let drag = drag.clone();
                move |_ev, _window, _cx| {
                    *drag.borrow_mut() = None;
                }
            },
        );

    el
}

/// 滑块最小高度（太小不好看/不好点）
const THUMB_MIN_H: Pixels = px(26.);

pub struct Scrollbar {
    handle: ScrollHandle,
}

impl Scrollbar {
    pub fn new(handle: ScrollHandle) -> Self {
        Self { handle }
    }
}

impl gpui::IntoElement for Scrollbar {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Scrollbar {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        // 宽度固定，高度交给 flex（所在行默认 stretch → 与滚动容器同高）
        let mut style = Style::default();
        style.size.width = (BAR_W + GAP_RIGHT * 2.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) -> () {}

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) {
        let handle = &self.handle;
        let viewport = handle.bounds();
        let viewport_h = viewport.size.height;
        if viewport_h <= px(0.) {
            return;
        }
        let content_h = viewport_h + handle.max_offset().height;
        if content_h <= viewport_h + px(1.) {
            return; // 内容不超高，无需滚动条
        }

        // 滚动进度（0..1）
        let scroll_down = (-handle.offset().y).max(px(0.));
        let max_scroll = (content_h - viewport_h).max(px(0.));
        let frac = if max_scroll > px(0.) {
            (scroll_down / max_scroll).clamp(0., 1.)
        } else {
            0.
        };

        // 轨道：左移出 GAP_RIGHT，一条非常淡的圆角底
        let track_x = bounds.left() + GAP_RIGHT;
        let track = Bounds::new(point(track_x, bounds.top()), size(BAR_W, bounds.size.height));
        window.paint_quad(
            gpui::fill(track, rgba(0x93a0aa1f)).corner_radii(Corners::all(THUMB_RADIUS)),
        );

        // 滑块（圆角、半透明）
        let thumb_h = (bounds.size.height * (viewport_h / content_h)).max(THUMB_MIN_H);
        let travel = (bounds.size.height - thumb_h).max(px(0.));
        let thumb_y = bounds.top() + travel * frac;
        let thumb = Bounds::new(point(track_x, thumb_y), size(BAR_W, thumb_h));
        window.paint_quad(
            gpui::fill(thumb, rgba(0x64748b96)).corner_radii(Corners::all(THUMB_RADIUS)),
        );
    }
}

/// 美化**横向**滚动条：跟随 [`ScrollHandle`] 的自绘滚动条（放在横向滚动容器下方）。
///
/// 用法：横向滚动容器 `.overflow_x_scroll().track_scroll(&handle)` 的子级/兄弟级
/// 均可；本组件占一行高度，内容没横向溢出时不显示。
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        div, px, rgb, size, SharedString, Styled, IntoElement, InteractiveElement, ParentElement,
        Context, Render, StatefulInteractiveElement, Window,
    };

    /// 复刻 `app.rs::render_messages` 的结构（行包装 + 可滚动消息列 + 右侧滚动条），
    /// 验证：消息内容超高时 `max_offset > 0`、可滚动到底（滚动条会随之出现）。
    struct MessagesHost {
        handle: ScrollHandle,
    }

    impl Render for MessagesHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let handle = self.handle.clone();
            let mut scroll = div()
                .id("messages")
                .flex_1()
                .overflow_y_scroll()
                .track_scroll(&handle)
                .px_4()
                .py_3()
                .min_w(px(0.));
            for i in 0..40 {
                scroll = scroll.child(
                    div()
                        .h(px(40.))
                        .mb_2()
                        .bg(rgb(0xe5e7eb))
                        .child(SharedString::from(format!("消息 {i}"))),
                );
            }
            // 复刻 app 的消息区：外层行给固定高度（app 中由 flex_col+flex_1 撑出的剩余高度），
            // 内部滚动列超高时溢出并可滚动
            div()
                .h(px(300.))
                .w(px(500.))
                .flex()
                .flex_row()
                .child(scroll)
                .child(Scrollbar::new(handle))
        }
    }

    #[gpui::test]
    async fn messages_scrollbar_engages_on_overflow(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_window, _cx| MessagesHost {
            handle: ScrollHandle::new(),
        });
        cx.simulate_resize(size(px(500.), px(300.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (max_h, _) = cx.update(|_window, cx| {
            let host = view.read(cx);
            (host.handle.max_offset().height, host.handle.bounds())
        });
        assert!(
            max_h > px(0.),
            "消息内容超高时应可滚动（滚动条出现）：max_offset={max_h:?}"
        );
        // 滚动到底后偏移应从 0 变化为负的 max_offset
        let h = cx.update(|_window, cx| view.read(cx).handle.clone());
        h.scroll_to_bottom();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        let bottom = cx.update(|_window, cx| view.read(cx).handle.offset().y);
        assert!(bottom < px(-1.), "应可滚到底部：offset={bottom:?}");
    }
}
