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

use crate::config::layout::{HSCROLL_GAP_RIGHT, HSCROLL_THUMB_MIN};

/// 横向滚动条的共享状态：滚动句柄 + 拖拽抓取偏移（跨帧保留，元素每帧重建也不丢）。
///
/// 由持有表格的调用方保存一个实例（每个表格一个），渲染时仅借用。
#[derive(Clone)]
pub struct TableScrollState {
    pub handle: ScrollHandle,
    /// `Some(px)` = 正在拖拽，值为"指针相对滑块中心"的水平偏移
    pub drag: Rc<RefCell<Option<Pixels>>>,
    /// 滚动条是否应显示（自动隐藏：滚动/拖拽时置真，停止滚动后由调用方定时隐藏）。
    pub visible: Rc<RefCell<bool>>,
}

impl TableScrollState {
    pub fn new() -> Self {
        Self {
            handle: ScrollHandle::new(),
            drag: Rc::new(RefCell::new(None)),
            visible: Rc::new(RefCell::new(false)),
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
    let bar_w = (viewport_w - HSCROLL_GAP_RIGHT * 2.).max(px(0.));
    let frac = ((-handle.offset().x).max(px(0.)) / max_scroll).clamp(0., 1.);
    let thumb_w = (bar_w * (viewport_w / content_w)).max(HSCROLL_THUMB_MIN).min(bar_w);
    let thumb_x = HSCROLL_GAP_RIGHT + (bar_w - thumb_w) * frac;
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
/// - 只画滑块、不画整条轨道：选项卡栏底边与编辑区交界处不要出现
///   “淡淡的灰色条状”（全宽半透明轨道），只用短滑块指示可滚/进度。
pub fn horizontal_scrollbar(
    state: &TableScrollState,
    id: impl Into<ElementId>,
) -> impl IntoElement {
    let handle = state.handle.clone();
    let drag = state.drag.clone();
    let visible = state.visible.clone();

    let mut el = div()
        .id(id)
        .relative()
        // 约 3px 高的一条细线，半透明，贴着容器底部浮一层
        .h(px(3.))
        .w_full()
        .cursor(CursorStyle::PointingHand);

    if let Some((_bar_w, thumb_w, thumb_x, _)) = hscroll_geometry(&handle) {
        el = el.child(
            div()
                .absolute()
                .left(thumb_x)
                .top(px(0.))
                .w(thumb_w)
                .h(px(2.))
                .rounded(px(1.))
                .bg(rgba(0x6060604a)),
        );
    }

    el = el
        .on_mouse_down(MouseButton::Left, {
            let handle = handle.clone();
            let drag = drag.clone();
            let visible = visible.clone();
            move |ev, window, _cx| {
                // 抓取滑块即视为“正在滚动”→ 保持滚动条可见（自动隐藏计时由调用方重启）
                *visible.borrow_mut() = true;
                let Some((bar_w, thumb_w, _, max_scroll)) = hscroll_geometry(&handle) else {
                    return;
                };
                let bar_left = handle.bounds().left() + HSCROLL_GAP_RIGHT;
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
                let bar_left = handle.bounds().left() + HSCROLL_GAP_RIGHT;
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
