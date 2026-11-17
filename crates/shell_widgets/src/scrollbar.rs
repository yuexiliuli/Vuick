//! 美化横向滚动条：跟随 [`ScrollHandle`] 的自绘滚动条。
//!
//! 用法：放在滚动容器旁边/下方（同宽高），例如：
//! ```ignore
//! div().flex().flex_row().flex_1()
//!     .child(scroll_container)      // .overflow_y_scroll().track_scroll(&handle)
//!     .child(horizontal_scrollbar(&state, "scrollbar", thumb))
//! ```
//! 内容不超高/超宽时不显示；仅在可滚动时画一条淡轨道 + 圆角半透明滑块。

use gpui::{
    point, px, relative, size, App, Bounds, CursorStyle, Element, ElementId, GlobalElementId,
    Hitbox, HitboxBehavior, IntoElement, InspectorElementId, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Rgba, ScrollHandle, Style, Window, fill,
};
use std::cell::RefCell;
use std::rc::Rc;

use config::layout::{HSCROLL_GAP_RIGHT, HSCROLL_THUMB_MIN};

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

/// 横向滚动条的自定义元素（原 div 版迁移）：跟随 [`ScrollHandle`] 自绘在横向滚动容器下方。
///
/// - 内容没超宽时不绘制；
/// - 点击/拖动均可滑到对应位置（拖动用 `state.drag` 跨帧记录抓取点）。
/// - 只画滑块、不画整条轨道：选项卡栏底边与编辑区交界处不要出现
///   “淡淡的灰色条状”（全宽半透明轨道），只用短滑块指示可滚/进度。
/// - 滑块底色由调用方从 `ui::theme::ChromeColors::scrollbar_thumb` 传入（亮暗同值）。
/// - 按下成功后 `window.capture_pointer`：指针逸出 3px 元素带拖拽仍持续跟手
///   （review H 红测修复——div 版 `on_mouse_move` 以 `hitbox.is_hovered` 门控，
///   未捕获时逸出即冻结）。
struct HScrollbar {
    handle: ScrollHandle,
    drag: Rc<RefCell<Option<Pixels>>>,
    visible: Rc<RefCell<bool>>,
    id: ElementId,
    thumb: Rgba,
}

/// `prepaint` 产出、`paint` 消费的状态：本帧滑块几何 + 整带 hitbox。
struct HScrollbarPrepaint {
    geom: Option<(Pixels, Pixels, Pixels, Pixels)>,
    hitbox: Hitbox,
}

impl IntoElement for HScrollbar {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for HScrollbar {
    type RequestLayoutState = ();
    type PrepaintState = HScrollbarPrepaint;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
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
    ) -> (LayoutId, Self::RequestLayoutState) {
        // 同原 div `.w_full().h(px(3.))`：宽铺满父级、高 3px。
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = px(3.).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let geom = hscroll_geometry(&self.handle);
        // 整带 hitbox：本元素点击/悬停命中的唯一入口，同时被 `capture_pointer` 引用。
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        HScrollbarPrepaint { geom, hitbox }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        // 画滑块：对应原 div `absolute().left(thumb_x).top(0).w(thumb_w).h(2).rounded(1)`。
        if let Some((_bar_w, thumb_w, thumb_x, _)) = prepaint.geom {
            let thumb_bounds =
                Bounds::new(point(bounds.origin.x + thumb_x, bounds.origin.y), size(thumb_w, px(2.)));
            window.paint_quad(fill(thumb_bounds, self.thumb).corner_radii(px(1.)));
        }
        window.set_cursor_style(CursorStyle::PointingHand, &prepaint.hitbox);

        // 鼠标监听：gpui 只允许在 paint 阶段注册 `on_mouse_event`（对照 ui::scrollbar）。
        // 关闭状态引用进闭包后元素每帧重建也不丢（drag/visible 是跨帧共享的 Rc）。
        let hitbox_id = prepaint.hitbox.id;
        let hit_bounds = bounds;

        // 按下：进拖拽 + 捕获指针。捕获由 window.rs 在 mouse_up 自动释放；它的作用只是让本
        // hitbox 在拖拽期间 `is_hovered` 恒真（光标保持 PointingHand）。move 的送达并不依赖
        // 捕获——window 级 `on_mouse_event` 与指针位置无关，带外 move 同样到达（红测剔除法实证）。
        window.on_mouse_event({
            let handle = self.handle.clone();
            let drag = self.drag.clone();
            let visible = self.visible.clone();
            move |event: &MouseDownEvent, phase, window, _cx| {
                if !phase.bubble()
                    || event.button != MouseButton::Left
                    || !hit_bounds.contains(&event.position)
                {
                    return;
                }
                // 抓取滑块即视为“正在滚动”→ 保持滚动条可见（自动隐藏计时由调用方重启）。
                *visible.borrow_mut() = true;
                let Some((bar_w, thumb_w, _, max_scroll)) = hscroll_geometry(&handle) else {
                    return;
                };
                let bar_left = handle.bounds().left() + HSCROLL_GAP_RIGHT;
                let rel = event.position.x - bar_left;
                // 记录抓取点相对滑块中心的偏移，拖动时保持。
                *drag.borrow_mut() = Some(rel - thumb_w / 2.0);
                set_thumb_from_center(rel, bar_w, thumb_w, max_scroll, &handle);
                window.capture_pointer(hitbox_id);
                window.refresh();
            }
        });

        // 移动：拖拽中持续跟手。window 级监听无 hover/位置门控，带内/带外 move 都会到达；
        // 以 `drag.is_some()` 为唯一开关（mousedown 命中才开、mouseup 关闭）保证不越权。
        window.on_mouse_event({
            let handle = self.handle.clone();
            let drag = self.drag.clone();
            move |event: &MouseMoveEvent, phase, window, _cx| {
                if !phase.bubble() || drag.borrow().is_none() {
                    return;
                }
                let Some((bar_w, thumb_w, _, max_scroll)) = hscroll_geometry(&handle) else {
                    *drag.borrow_mut() = None;
                    return;
                };
                let grab = drag.borrow().unwrap();
                let bar_left = handle.bounds().left() + HSCROLL_GAP_RIGHT;
                let rel = event.position.x - bar_left - grab;
                set_thumb_from_center(rel, bar_w, thumb_w, max_scroll, &handle);
                window.refresh();
            }
        });

        // 抬起：清拖拽态。window 级监听不依赖悬停，带内/带外释放都到此处（覆盖原
        // `on_mouse_up` 与 `on_mouse_up_out` 两路）；指针捕获已由 gpui 在 mouse_up 自动
        // 释放，无需显式 release_pointer。
        window.on_mouse_event({
            let drag = self.drag.clone();
            move |event: &MouseUpEvent, phase, _window, _cx| {
                if !phase.bubble() || event.button != MouseButton::Left || drag.borrow().is_none() {
                    return;
                }
                *drag.borrow_mut() = None;
            }
        });
    }
}

/// 构建横向滚动条（返回自定义元素）：点击/拖动滑到对应位置，拖动逸出 3px 带仍跟手。
pub fn horizontal_scrollbar(
    state: &TableScrollState,
    id: impl Into<ElementId>,
    thumb: Rgba,
) -> impl IntoElement {
    HScrollbar {
        handle: state.handle.clone(),
        drag: state.drag.clone(),
        visible: state.visible.clone(),
        id: id.into(),
        thumb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, InteractiveElement, Modifiers, MouseButton, ParentElement, Render,
        StatefulInteractiveElement, Styled, TestAppContext, Window, div, point, px, rgba,
    };

    /// 红测宿主：真实的横向可滚动容器（`track_scroll` 使 `ScrollHandle` 获得 bounds/max_offset）
    /// 下方叠放 `horizontal_scrollbar`，用真实 `div` 布局 + 事件模拟驱动（对照 ui::slider 姿势）。
    struct HScrollHost {
        state: TableScrollState,
    }

    impl Render for HScrollHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("hscroll-host-root")
                .flex()
                .flex_col()
                .w(px(200.))
                .child(
                    // 200 宽视口 + 1000 宽内容 → max_offset.x=800，可横向滚动。
                    // 需先 `.id()` 升级成 Stateful（overflow/track_scroll 在 Stateful 的 Interactivity 上）。
                    div()
                        .id("hscroll-scroll-viewport")
                        .w(px(200.))
                        .h(px(30.))
                        .overflow_x_scroll()
                        .track_scroll(&self.state.handle)
                        .child(div().w(px(1000.)).h(px(30.))),
                )
                .child(
                    div()
                        .debug_selector(|| "hscroll-bar-wrap".into())
                        .child(horizontal_scrollbar(
                            &self.state,
                            "hscroll-red-test",
                            rgba(0x6060604a),
                        )),
                )
        }
    }

    /// 复现线（review H，scrollbar.rs on_mouse_down 无 capture_pointer）：
    /// 自绘滑块 mousedown 后，指针垂直漂出 3px 元素带仍按住 → gpui `on_mouse_move`
    /// （div.rs:309）以 `hitbox.is_hovered` 门控，逸出后不再派发 → 滑块「冻结」不再跟手。
    /// 期望语义：mousedown 后捕获指针（`window.capture_pointer`），逸出元素边界仍持续跟手。
    ///
    /// 几何演算（容器 200×30、内容 1000、gap=3、thumb_min=26）：
    ///   bar_w=200−6=194；thumb_w=max(194×0.2,26)=38.8；travel=194−38.8=155.2
    ///   滚动条 div y∈[30,33]（3px 带），bar 左缘=handle.bounds().left()+3=3。
    ///   mousedown x=40：rel=40−3=37、grab=37−19.4=17.6 → offset.x=−800×37/155.2≈−190.7
    ///   带内 move x=120：rel=120−3−17.6=99.4 → offset.x≈−800×99.4/155.2≈−512.4（跟手）
    ///   带外 move x=170、y=43（越 3px 带）：rel=170−3−17.6=149.4 → 期望 offset.x≈−770.1（更负）
    ///   缺陷下 mousedown 后未捕获指针 → 带外 move 不派发 → offset.x 停在 −512.4（不更负）= 红。
    #[gpui::test]
    fn drag_follows_pointer_outside_vertical_band(cx: &mut TestAppContext) {
        let state = TableScrollState::new();
        let host_state = state.clone();
        let (_, cx) = cx.add_window_view(move |_, _| HScrollHost { state: host_state });
        cx.update(|window, cx| window.draw(cx).clear(cx));

        // 前置自检：滚动容器已把 max_offset 写入句柄（≥0 说明几何成立、滑块在场）。
        let max_scroll = state.handle.max_offset().x;
        assert!(
            max_scroll > px(0.),
            "布局后 max_offset.x 应>0（可滚动才画滑块），实际 {max_scroll:?}"
        );
        let bar = cx
            .debug_bounds("hscroll-bar-wrap")
            .expect("滚动条应已布局");

        // 带内 mousedown：点按滑块（元素命中区内，y 取 3px 带中心）。
        let p_down = point(px(40.), bar.center().y);
        cx.simulate_mouse_move(p_down, None, Modifiers::default());
        cx.simulate_mouse_down(p_down, MouseButton::Left, Modifiers::default());
        assert!(
            state.drag.borrow().is_some(),
            "mousedown 滑块后应进入拖拽态"
        );
        let o_down = state.handle.offset().x;

        // 控制路径：仍在 3px 带内右移 → 滚动跟随（证明带内拖拽机制正常，把失败隔离到"带外"）。
        let p_band = point(px(120.), bar.center().y);
        cx.simulate_mouse_move(p_band, Some(MouseButton::Left), Modifiers::default());
        let o_band = state.handle.offset().x;
        assert!(
            o_band < o_down,
            "带内拖动应持续跟手（右移→offset 更负），实际 {o_band:?} 未较 {o_down:?} 前进"
        );

        // ★ 红测点：垂直逸出 3px 带（y=bottom+10，仍按住左键）再右移 → 期望 offset 继续更负。
        //   缺陷：`on_mouse_down` 未 `capture_pointer` → 逸出后 `on_mouse_move` 停发 → offset 冻结。
        let p_out = point(px(170.), bar.bottom() + px(10.));
        cx.simulate_mouse_move(p_out, Some(MouseButton::Left), Modifiers::default());
        let o_out = state.handle.offset().x;
        assert!(
            o_out < o_band,
            "拖拽逸出元素带后仍应按指针跟手（期望 offset.x < {o_band:?}，实际 {o_out:?}）——\
             自绘滑块 mousedown 后未 capture_pointer 导致滑块冻结（红测）"
        );

        // 释放：带外 mouseup 走 `on_mouse_up_out` 清拖拽态，不残留。
        cx.simulate_mouse_up(p_out, MouseButton::Left, Modifiers::default());
        assert!(
            state.drag.borrow().is_none(),
            "释放后拖拽态应清空"
        );
    }
}
