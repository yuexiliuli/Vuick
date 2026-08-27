//! 美化滚动条：跟随 [`ScrollHandle`] 的自绘垂直滚动条。
//!
//! 用法：放在滚动容器旁边（同高、相邻），例如：
//! ```ignore
//! div().flex().flex_row().flex_1()
//!     .child(scroll_container)      // .overflow_y_scroll().track_scroll(&handle)
//!     .child(Scrollbar::new(handle))
//! ```
//! 内容不超高时不显示；仅在可滚动时画一条淡轨道 + 圆角半透明滑块。

use gpui::{
    point, px, rgba, size, App, Bounds, Corners, Element, ElementId, GlobalElementId,
    InspectorElementId, LayoutId, Pixels, ScrollHandle, Style, Window,
};

/// 滑块宽度
const BAR_W: Pixels = px(6.);
/// 滑块圆角半径
const THUMB_RADIUS: Pixels = px(3.);
/// 滑块距右缘间距
const GAP_RIGHT: Pixels = px(3.);
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
