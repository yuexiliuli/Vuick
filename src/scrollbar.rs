//! 美化横向滚动条：跟随 [`ScrollHandle`] 的自绘滚动条。
//!
//! 用法：放在滚动容器旁边/下方（同宽高），例如：
//! ```ignore
//! div().flex().flex_row().flex_1()
//!     .child(scroll_container)      // .overflow_y_scroll().track_scroll(&handle)
//!     .child(horizontal_scrollbar(&state, "scrollbar"))
//! ```
//! 内容不超高/超宽时不显示；仅在可滚动时画一条淡轨道 + 圆角半透明滑块。

use gpui::{
    div, point, px, rgba, CursorStyle, ElementId, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, ScrollHandle, Styled,
};
use std::cell::RefCell;
use std::rc::Rc;

/// 滑块距边缘间距
const GAP_RIGHT: Pixels = px(3.);
/// 滑块最小尺寸
const THUMB_MIN: Pixels = px(26.);

/// 可拖拽分隔条（编辑/预览分割线、PDF 目录右缘等）在「悬浮/拖拽中」的强调色。
///
/// 全局唯一来源：所有可拖动条共享同一种强调色，改这一处即可统一全部。
pub const RESIZE_ACCENT: u32 = 0x0078d4;

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
fn hscroll_geometry(handle: &ScrollHandle) -> Option<(Pixels, Pixels, Pixels, Pixels)> {
    let bounds = handle.bounds();
    let viewport_w = bounds.size.width;
    if viewport_w <= px(0.) {
        return None;
    }
    let max_scroll = handle.max_offset().x;
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
        .on_mouse_down(MouseButton::Left, {
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
        })
        .on_mouse_move({
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
        })
        .on_mouse_up(MouseButton::Left, {
            let drag = drag.clone();
            move |_ev, _window, _cx| {
                *drag.borrow_mut() = None;
            }
        })
        .on_mouse_up_out(MouseButton::Left, {
            let drag = drag.clone();
            move |_ev, _window, _cx| {
                *drag.borrow_mut() = None;
            }
        });

    el
}
